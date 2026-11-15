//! `ScanJob` end to end on a small fixture folder: Linux (core's
//! `StdLister`) and the Windows host (`WinLister`).
//!
//! The fixture is created by the test, under the cargo target dir on Linux
//! and under `%TEMP%\ringscan-dev\engine-tests-<pid>` on Windows, and
//! removed afterwards; only files inside it are modified or deleted.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ringscan_core::{NodeId, SizeMode, Tree, flags};
use ringscan_engine::{DeleteMode, Method, Phase, ScanJob, ScanOptions, Target};

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        #[cfg(windows)]
        let base = std::env::temp_dir().join("ringscan-dev");
        #[cfg(not(windows))]
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let dir = base.join(format!("engine-tests-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Fixture { dir }
    }

    fn file(&self, rel: &str, len: usize) -> PathBuf {
        let p = self.dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![b'x'; len]).unwrap();
        p
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Remove a junction as a link first so its target is never touched.
        #[cfg(windows)]
        let _ = fs::remove_dir(self.dir.join("link"));
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Directory link to `target` named `link`: a symlink on unix, a junction
/// on Windows (no privilege needed).
fn make_dir_link(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let out = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(out.status.success(), "mklink /J failed: {out:?}");
    }
}

fn find(t: &Tree, rel: &str) -> NodeId {
    let mut cur = t.root();
    for part in rel.split('/') {
        cur = t
            .find_child(cur, part)
            .unwrap_or_else(|| panic!("{rel} not in tree"));
    }
    cur
}

/// Sum of a mode over non-directory nodes below `id`.
fn file_bytes(t: &Tree, id: NodeId, mode: SizeMode) -> u64 {
    t.descendants(id)
        .filter(|&d| !t.node(d).is_dir())
        .map(|d| t.size(d, mode))
        .sum()
}

fn walk_opts() -> ScanOptions {
    ScanOptions {
        method: Method::Walk,
        threads: 4,
        ..ScanOptions::default()
    }
}

/// a/f1 (1000), a/f2 (5000), a/b/f3 (10000), c/ with a hard link to a/f1,
/// link -> a (symlink / junction), an empty dir e/.
fn standard_fixture(tag: &str) -> Fixture {
    let fx = Fixture::new(tag);
    let f1 = fx.file("a/f1.txt", 1000);
    fx.file("a/f2.bin", 5000);
    fx.file("a/b/f3.dat", 10_000);
    fs::create_dir_all(fx.dir.join("c")).unwrap();
    fs::create_dir_all(fx.dir.join("e")).unwrap();
    fs::hard_link(&f1, fx.dir.join("c/f1-link.txt")).unwrap();
    make_dir_link(&fx.dir.join("a"), &fx.dir.join("link"));
    fx
}

#[test]
fn walk_scan_counts_links_once_and_does_not_follow_links() {
    let fx = standard_fixture("walk");
    let job = ScanJob::start(Target::Path(fx.dir.clone()), walk_opts());
    let p = job.wait();
    assert_eq!(p.phase, Phase::Done);
    assert_eq!(p.method, Some(Method::Walk));
    assert!(!p.cancelled);
    assert_eq!(p.errors, 0, "{p:?}");
    assert_eq!(p.units.len(), 1);
    assert!(p.units[0].note.is_none(), "{:?}", p.units[0].note);

    let t = job.tree().read();
    let root = t.root();
    // Hard link: exactly one of the two names carries the size.
    let (f1, l1) = (find(&t, "a/f1.txt"), find(&t, "c/f1-link.txt"));
    let linked: Vec<bool> = [f1, l1]
        .iter()
        .map(|&id| t.node(id).has(flags::HARDLINK))
        .collect();
    assert_eq!(linked.iter().filter(|&&x| x).count(), 1, "{linked:?}");
    for id in [f1, l1] {
        let n = t.node(id);
        assert_eq!(n.logical() == 0, n.has(flags::HARDLINK));
    }
    // The link is listed, flagged and not descended.
    let link = find(&t, "link");
    assert!(t.node(link).has(flags::REPARSE));
    assert!(!t.has_children(link));
    // Sizes: every file once (plus the unix symlink's own length).
    let link_len = if cfg!(unix) {
        t.node(link).logical()
    } else {
        0
    };
    assert_eq!(file_bytes(&t, root, SizeMode::Logical), 16_000 + link_len);
    // Counts: f1, f2, f3, f1-link (+ the unix symlink, a non-directory).
    let expected_files = if cfg!(unix) { 5 } else { 4 };
    assert_eq!(t.files(root), expected_files);
    assert_eq!(p.files, expected_files);
    // Sorted by allocated size when done.
    let kids: Vec<u64> = t
        .children(root)
        .map(|c| t.size(c, SizeMode::Allocated))
        .collect();
    assert!(kids.windows(2).all(|w| w[0] >= w[1]), "{kids:?}");
    // Paths resolve to the real items.
    assert_eq!(job.node_path(f1), Some(fx.dir.join("a").join("f1.txt")));
    assert!(job.drives().is_empty(), "a folder scan has no drive extras");
}

#[test]
fn all_links_option_counts_every_link() {
    let fx = standard_fixture("alllinks");
    let opts = ScanOptions {
        dedupe_hardlinks: false,
        ..walk_opts()
    };
    let job = ScanJob::start(Target::Path(fx.dir.clone()), opts);
    job.wait();
    let t = job.tree().read();
    assert!(t.live_nodes().all(|(_, n)| !n.has(flags::HARDLINK)));
    let link_len = if cfg!(unix) {
        t.node(find(&t, "link")).logical()
    } else {
        0
    };
    assert_eq!(
        file_bytes(&t, t.root(), SizeMode::Logical),
        17_000 + link_len
    );
}

#[test]
fn rescan_picks_up_changes_in_place() {
    let fx = standard_fixture("rescan");
    let job = ScanJob::start(Target::Path(fx.dir.clone()), walk_opts());
    job.wait();
    let (a, before) = {
        let t = job.tree().read();
        let a = find(&t, "a");
        (a, t.size(t.root(), SizeMode::Logical))
    };
    fx.file("a/new.bin", 2000);
    fs::remove_file(fx.dir.join("a/f2.bin")).unwrap();
    let re = job.rescan(a).unwrap();
    let p = re.wait();
    assert_eq!(p.phase, Phase::Done);
    assert_eq!(p.units.len(), 1);
    assert_eq!(p.units[0].node, a);
    let t = job.tree().read();
    assert!(t.is_live(a), "the rescanned folder keeps its id");
    assert!(t.find_child(a, "new.bin").is_some());
    assert!(t.find_child(a, "f2.bin").is_none());
    assert_eq!(
        t.size(t.root(), SizeMode::Logical),
        before + 2000 - 5000,
        "totals updated"
    );
    // A file rescans its folder; dead ids are refused.
    let f3 = find(&t, "a/b/f3.dat");
    drop(t);
    let re = job.rescan(f3).unwrap();
    assert_eq!(re.wait().units[0].node, job.tree().read().node(f3).parent());
    let gone = {
        let t = job.tree().read();
        find(&t, "a/b")
    };
    let re = job.rescan(a).unwrap();
    re.wait();
    assert!(!job.tree().read().is_live(gone));
    assert!(job.rescan(gone).is_err());
}

/// Rescanning either folder of a hard-linked file (the one whose link is
/// counted, or the other) keeps the file counted exactly once.
#[test]
fn rescans_keep_hard_links_counted_once() {
    let fx = standard_fixture("relink");
    let job = ScanJob::start(Target::Path(fx.dir.clone()), walk_opts());
    job.wait();
    let total = |job: &ScanJob| {
        let t = job.tree().read();
        t.size(t.root(), SizeMode::Logical)
    };
    let expected = total(&job);
    // (counted link's folder, other link's folder)
    let folders = |job: &ScanJob| {
        let t = job.tree().read();
        let links = [find(&t, "a/f1.txt"), find(&t, "c/f1-link.txt")];
        let counted: Vec<NodeId> = links
            .iter()
            .copied()
            .filter(|&l| !t.node(l).has(flags::HARDLINK))
            .collect();
        assert_eq!(counted.len(), 1, "exactly one link carries the size");
        let other = links.into_iter().find(|&l| l != counted[0]).unwrap();
        (t.node(counted[0]).parent(), t.node(other).parent())
    };
    for _ in 0..2 {
        let (inside, outside) = folders(&job);
        job.rescan(inside).unwrap().wait();
        assert_eq!(total(&job), expected, "counted link inside the rescan");
        let (_, outside2) = folders(&job);
        assert_eq!(outside2, outside);
        job.rescan(outside).unwrap().wait();
        assert_eq!(total(&job), expected, "counted link outside the rescan");
    }
    let root = job.tree().read().root();
    job.rescan(root).unwrap().wait();
    assert_eq!(total(&job), expected, "whole folder");
    folders(&job);
}

#[test]
fn missing_target_reports_error() {
    let fx = Fixture::new("missing");
    let job = ScanJob::start(Target::Path(fx.dir.join("does-not-exist")), walk_opts());
    let p = job.wait();
    assert_eq!(p.errors, 1);
    let t = job.tree().read();
    assert!(t.node(t.root()).has(flags::ERROR));
    assert!(!t.has_children(t.root()));
}

#[test]
fn cancel_stops_quickly_and_drop_cancels() {
    let fx = Fixture::new("cancel");
    for i in 0..60 {
        for j in 0..40 {
            fx.file(&format!("d{i}/s{j}/f.txt"), 10);
        }
    }
    let job = ScanJob::start(Target::Path(fx.dir.clone()), walk_opts());
    job.cancel();
    let t0 = Instant::now();
    let p = job.wait();
    assert!(p.cancelled);
    assert!(t0.elapsed() < Duration::from_secs(5));
    // Dropping a running job cancels it: the tree stops changing soon and
    // stays usable.
    let job = ScanJob::start(Target::Path(fx.dir.clone()), walk_opts());
    let tree = job.tree().clone();
    drop(job);
    std::thread::sleep(Duration::from_millis(300));
    let generation = tree.read().generation();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(tree.read().generation(), generation);
    assert!(tree.read().live_len() >= 1);
}

#[test]
fn delete_refuses_root_virtual_and_unknown_nodes() {
    let fx = standard_fixture("refuse");
    let job = ScanJob::start(Target::Path(fx.dir.clone()), walk_opts());
    job.wait();
    let root = job.tree().read().root();
    for mode in [DeleteMode::Recycle, DeleteMode::Permanent] {
        let e = job.delete(&[root], mode).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        let e = job.delete(&[u32::MAX - 1], mode).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
    }
    assert!(job.delete(&[], DeleteMode::Permanent).is_ok());
    assert!(fx.dir.join("a/f1.txt").exists());
}

#[cfg(windows)]
#[test]
fn delete_permanently_removes_items_from_disk_and_tree() {
    let fx = standard_fixture("delete");
    let job = ScanJob::start(Target::Path(fx.dir.clone()), walk_opts());
    job.wait();
    let (b, f2, total) = {
        let t = job.tree().read();
        (
            find(&t, "a/b"),
            find(&t, "a/f2.bin"),
            t.size(t.root(), SizeMode::Logical),
        )
    };
    job.delete(&[b, f2], DeleteMode::Permanent).unwrap();
    assert!(!fx.dir.join("a/b").exists());
    assert!(!fx.dir.join("a/f2.bin").exists());
    assert!(fx.dir.join("a/f1.txt").exists());
    let t = job.tree().read();
    assert!(!t.is_live(b) && !t.is_live(f2));
    assert_eq!(t.size(t.root(), SizeMode::Logical), total - 15_000);
    // The junction was not followed when scanning, and deleting inside the
    // tree never touched its target.
    assert!(fx.dir.join("link").exists());
}

#[cfg(windows)]
#[test]
fn win_lister_flags_on_ntfs() {
    use ringscan_core::walk::{DirLister, RawEntry};
    use ringscan_engine::lister::{HardLinkFilter, LossyPaths, WinLister};
    use std::sync::Arc;

    let fx = standard_fixture("lister");
    fx.file("tiny.txt", 1);
    fx.file("hidden.txt", 3000);
    let st = std::process::Command::new("attrib")
        .arg("+h")
        .arg(fx.dir.join("hidden.txt"))
        .status()
        .unwrap();
    assert!(st.success());
    let lister = HardLinkFilter::new(WinLister::new(4096, Arc::new(LossyPaths::new())));
    let mut root: Vec<RawEntry> = Vec::new();
    lister.list(&fx.dir, &mut root).unwrap();
    let get = |v: &[RawEntry], n: &str| v.iter().find(|e| e.name == n).cloned().unwrap();
    let link = get(&root, "link");
    assert!(link.is_dir && link.flags & flags::REPARSE != 0, "{link:?}");
    let tiny = get(&root, "tiny.txt");
    assert_eq!(
        (tiny.logical, tiny.alloc),
        (1, 0),
        "MFT-resident: no clusters"
    );
    let hidden = get(&root, "hidden.txt");
    assert_eq!(hidden.flags & flags::HIDDEN, flags::HIDDEN);
    assert_eq!(hidden.alloc, 4096);
    assert!(hidden.file_id != 0, "NTFS gives file ids");
    // Hard links: the second name seen is flagged.
    let mut a = Vec::new();
    lister.list(&fx.dir.join("a"), &mut a).unwrap();
    let mut c = Vec::new();
    lister.list(&fx.dir.join("c"), &mut c).unwrap();
    assert_eq!(get(&a, "f1.txt").flags & flags::HARDLINK, 0);
    assert_eq!(
        get(&c, "f1-link.txt").flags & flags::HARDLINK,
        flags::HARDLINK
    );
    assert_eq!(get(&a, "f1.txt").file_id, get(&c, "f1-link.txt").file_id);
}

/// A UNC folder has no Recycle Bin: recycling must fail without deleting,
/// and the tree must keep the item. Set `RINGSCAN_TEST_UNC_DIR` to a
/// writable UNC folder (see crates/win/README.md) to run it.
#[cfg(windows)]
#[test]
fn recycle_on_share_without_bin_keeps_item() {
    let Some(dir) = std::env::var_os("RINGSCAN_TEST_UNC_DIR") else {
        eprintln!("RINGSCAN_TEST_UNC_DIR not set; skipped");
        return;
    };
    let dir = PathBuf::from(dir).join(format!("engine-recycle-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("keep.txt"), b"data").unwrap();
    let job = ScanJob::start(Target::Path(dir.clone()), walk_opts());
    job.wait();
    let id = find(&job.tree().read(), "keep.txt");
    let e = job.delete(&[id], DeleteMode::Recycle).unwrap_err();
    assert!(ringscan_win::shell::is_not_recyclable(&e), "{e}");
    assert!(dir.join("keep.txt").exists());
    assert!(job.tree().read().is_live(id));
    fs::remove_dir_all(&dir).unwrap();
}

/// The CLI binary end to end (Linux; on Windows the binary is exercised
/// manually, see the README).
#[cfg(unix)]
#[test]
fn cli_json_report() {
    let fx = standard_fixture("cli");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ringscan-cli"))
        .arg("--cli")
        .arg(&fx.dir)
        .args(["--json", "--depth", "2", "--top", "2", "--method", "walk"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["method"], "walk");
    assert_eq!(v["files"], 5);
    assert_eq!(v["errors"], 0);
    assert_eq!(v["largest_files"].as_array().unwrap().len(), 2);
    let tree = &v["tree"];
    assert_eq!(tree["files"], 5);
    let a = tree["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "a")
        .unwrap();
    assert!(
        a["children"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "b")
    );
    // Text mode and a usage error.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ringscan-cli"))
        .arg(&fx.dir)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Largest files"));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ringscan-cli"))
        .args(["--depth"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ringscan-cli"))
        .arg(fx.dir.join("nope"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}

/// Whole-drive scan and rescan through the MFT on a real volume (read-only).
/// Set `RINGSCAN_TEST_MFT_DRIVE` to a drive letter whose raw volume is
/// readable (any drive when elevated); skipped when the MFT path is not
/// taken. Run with `--ignored`.
#[cfg(windows)]
#[test]
#[ignore = "scans a real drive"]
fn real_drive_mft_scan_and_rescan() {
    let letter = std::env::var("RINGSCAN_TEST_MFT_DRIVE")
        .ok()
        .and_then(|s| s.chars().next())
        .unwrap_or('D');
    let job = ScanJob::start(Target::Drive(letter), ScanOptions::default());
    let p = job.wait();
    if p.method != Some(Method::Mft) {
        eprintln!(
            "{letter}: not read through the MFT ({:?}); skipped",
            p.units[0].note
        );
        return;
    }
    let d = job.drives().remove(0);
    eprintln!(
        "{letter}: {} files in {:?}, scanned {} + unaccounted {} + free {} = total {}",
        p.files, p.elapsed, d.space.scanned, d.space.unaccounted, d.space.free, d.space.total
    );
    assert_eq!(
        d.space.scanned + d.space.unaccounted + d.space.free,
        d.space.total + d.space.overcount
    );
    let (root, kids, generation) = {
        let t = job.tree().read();
        let kids: Vec<NodeId> = t.children(t.root()).collect();
        (t.root(), kids, t.generation())
    };
    // Whole-drive rescan: cleared and grafted below the same root.
    let re = job.rescan(root).unwrap().wait();
    assert_eq!(re.method, Some(Method::Mft));
    let t = job.tree().read();
    assert!(t.generation() > generation);
    assert!(
        kids.iter().all(|&k| !t.is_live(k)),
        "old children are tombstones"
    );
    let (free, unaccounted) = ringscan_engine::drive::extras_of(&t, t.root());
    assert!(free.is_some(), "free space re-added");
    assert!(unaccounted.is_some() || d.space.unaccounted == 0);
    assert!(
        t.files(t.root()).abs_diff(p.files) < 1000,
        "{} vs {}",
        t.files(t.root()),
        p.files
    );
}

/// Permanent delete on a scanned drive updates `<free space>`. Deletes only
/// the 64 MiB file it creates under %TEMP%\ringscan-dev; reads all of C:
/// (~30 s). Run with `--ignored`.
#[cfg(windows)]
#[test]
#[ignore = "walks the whole system drive"]
fn delete_on_scanned_drive_updates_free_space() {
    let fx = Fixture::new("drive-delete");
    let big = fx.file("big.bin", 64 << 20);
    let job = ScanJob::start(Target::Drive('C'), walk_opts());
    job.wait();
    let (node, free_before, unacc_before) = {
        let t = job.tree().read();
        let node = t
            .find_path(&big.to_string_lossy())
            .expect("test file in the tree");
        let (f, u) = ringscan_engine::drive::extras_of(&t, t.root());
        (
            node,
            t.node(f.unwrap()).alloc(),
            u.map_or(0, |u| t.node(u).alloc()),
        )
    };
    job.delete(&[node], DeleteMode::Permanent).unwrap();
    assert!(!big.exists());
    let t = job.tree().read();
    assert!(!t.is_live(node));
    let (f, u) = ringscan_engine::drive::extras_of(&t, t.root());
    let free_after = t.node(f.unwrap()).alloc();
    let unacc_after = u.map_or(0, |u| t.node(u).alloc());
    let grew = free_after as i64 - free_before as i64;
    eprintln!(
        "free {free_before} -> {free_after} (+{grew}), unaccounted {unacc_before} -> {unacc_after}"
    );
    // Other programs write to C: meanwhile: allow 48 MiB of noise.
    assert!((grew - (64 << 20)).abs() < 48 << 20, "free grew by {grew}");
    assert!((unacc_after as i64 - unacc_before as i64).abs() < 48 << 20);
}
