//! Walker tests against the real file system and against slow in-memory
//! listers.
//!
//! * `walk_matches_independent_computation`: builds a fixture under
//!   `%TEMP%\ringscan-dev` (or `$TMPDIR/ringscan-dev`) with thousands of
//!   files, nested and unicode names, an empty dir, a link loop (unix:
//!   symlinks; Windows: a junction, which needs no privileges) and an
//!   unreadable directory (unix: chmod 000, only effective when not root;
//!   Windows: an icacls deny ACE), walks it with `StdLister` and compares
//!   every directory with an independent recursive `std::fs` computation.
//! * `concurrent_walk_with_layout_readers`: 8 walker threads over a slow
//!   synthetic lister while another thread keeps taking read locks, laying
//!   out the tree and checking it is consistent at every moment.
//! * `cancel_is_prompt`: an endless slow walk is cancelled; `walk` must
//!   return within 100 ms.
//!
//! Every fixture is created by the test itself and removed on drop
//! (permissions / ACLs are restored first).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ringscan_core::flags::*;
use ringscan_core::layout::{self, Params};
use ringscan_core::parking_lot::RwLock;
use ringscan_core::synth::Rng;
use ringscan_core::walk::{DirLister, Progress, RawEntry, StdLister, walk};
use ringscan_core::{SizeMode, Tree};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct TempDir {
    path: PathBuf,
    locked: Vec<PathBuf>,
}

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let path = std::env::temp_dir()
            .join("ringscan-dev")
            .join(format!("core-{tag}-{}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir_all(&path).unwrap();
        TempDir {
            path,
            locked: Vec::new(),
        }
    }

    /// Makes `dir` unlistable for the current user; returns false if that
    /// is not possible here (root on unix ignores permissions).
    fn lock(&mut self, dir: &Path) -> bool {
        self.locked.push(dir.to_path_buf());
        lock_dir(dir);
        fs::read_dir(dir).is_err()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        for d in &self.locked {
            unlock_dir(d);
        }
        if let Err(e) = fs::remove_dir_all(&self.path) {
            eprintln!("warning: could not remove {}: {e}", self.path.display());
        }
    }
}

#[cfg(unix)]
fn lock_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
}

#[cfg(unix)]
fn unlock_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
}

#[cfg(windows)]
fn current_user() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
        _ => user,
    }
}

#[cfg(windows)]
fn lock_dir(dir: &Path) {
    // Deny "list folder / read data" to ourselves on this test directory.
    let out = std::process::Command::new("icacls")
        .arg(dir)
        .arg("/deny")
        .arg(format!("{}:(RD)", current_user()))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "icacls deny failed: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[cfg(windows)]
fn unlock_dir(dir: &Path) {
    let _ = std::process::Command::new("icacls")
        .arg(dir)
        .arg("/remove:d")
        .arg(current_user())
        .output();
}

#[cfg(not(any(unix, windows)))]
fn lock_dir(_: &Path) {}
#[cfg(not(any(unix, windows)))]
fn unlock_dir(_: &Path) {}

fn write_file(path: &Path, len: usize) {
    static DATA: [u8; 1 << 16] = [b'x'; 1 << 16];
    let mut left = len;
    let mut buf = Vec::with_capacity(len);
    while left > 0 {
        let n = left.min(DATA.len());
        buf.extend_from_slice(&DATA[..n]);
        left -= n;
    }
    fs::write(path, buf).unwrap();
}

fn random_len(rng: &mut Rng) -> usize {
    match rng.below(10) {
        0 => 0,
        1 => 100_000 + rng.below(400_000) as usize,
        _ => rng.below(6000) as usize,
    }
}

/// Builds the fixture; returns (number of files written, whether the locked
/// directory is really unreadable).
fn build_fixture(t: &mut TempDir) -> (usize, bool) {
    let root = t.path.clone();
    let mut rng = Rng::new(1234);
    let mut files = 0;
    // Nested levels: 3 + 9 + 27 + 81 directories.
    let mut level = vec![root.join("nested")];
    fs::create_dir(&level[0]).unwrap();
    for depth in 0..4 {
        let mut next = Vec::new();
        for dir in &level {
            for i in 0..3 {
                let d = dir.join(format!("level{depth}_{i}"));
                fs::create_dir(&d).unwrap();
                for f in 0..(4 + rng.below(12)) {
                    write_file(&d.join(format!("file{f}.dat")), random_len(&mut rng));
                    files += 1;
                }
                next.push(d);
            }
        }
        level = next;
    }
    // One big flat directory.
    let big = root.join("big flat dir");
    fs::create_dir(&big).unwrap();
    for i in 0..2500 {
        write_file(
            &big.join(format!("item {i:04}.{}", ["txt", "LOG", "bin"][i % 3])),
            (i * 7) % 3000,
        );
        files += 1;
    }
    // Unicode, dots, an empty directory.
    let uni = root.join("Папка ґїє");
    fs::create_dir(&uni).unwrap();
    write_file(&uni.join("Звіт.Final.PDF"), 12_345);
    write_file(&uni.join(".hidden-ish"), 10);
    files += 2;
    fs::create_dir(root.join("empty")).unwrap();
    // Link loop.
    let looped = root.join("loop");
    fs::create_dir(&looped).unwrap();
    write_file(&looped.join("inside.txt"), 5);
    files += 1;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("..", looped.join("back")).unwrap();
        std::os::unix::fs::symlink("inside.txt", looped.join("file-link")).unwrap();
        std::os::unix::fs::symlink("does-not-exist", looped.join("dangling")).unwrap();
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(looped.join("back"))
            .arg(&root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "mklink /J failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = std::process::Command::new("attrib")
            .arg("+h")
            .arg(looped.join("inside.txt"))
            .output()
            .unwrap();
        assert!(out.status.success());
    }
    // Unreadable directory (with content created before locking).
    let locked = root.join("locked");
    fs::create_dir(&locked).unwrap();
    write_file(&locked.join("secret.txt"), 777);
    files += 1;
    let enforced = t.lock(&locked);
    (files, enforced)
}

// ---------------------------------------------------------------------------
// Independent computation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Totals {
    files: u64,
    dirs: u64,
    logical: u64,
    alloc: u64,
    errors: u64,
}

#[cfg(unix)]
fn is_dir_and_sizes(md: &fs::Metadata) -> (bool, u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (md.file_type().is_dir(), md.len(), md.blocks() * 512)
}

#[cfg(windows)]
fn is_dir_and_sizes(md: &fs::Metadata) -> (bool, u64, u64) {
    use std::os::windows::fs::MetadataExt;
    let is_dir = md.file_attributes() & 0x10 != 0;
    let len = if is_dir { 0 } else { md.len() };
    (is_dir, len, len.div_ceil(4096) * 4096)
}

#[cfg(not(any(unix, windows)))]
fn is_dir_and_sizes(md: &fs::Metadata) -> (bool, u64, u64) {
    let len = if md.is_dir() { 0 } else { md.len() };
    (md.is_dir(), len, len.div_ceil(4096) * 4096)
}

/// Recursive std::fs walk with the same counting rules as the tree: sizes
/// of everything below `dir` (directory entries' own sizes included), links
/// counted but not followed. Records the totals of every directory.
fn naive(dir: &Path, per_dir: &mut Vec<(PathBuf, Totals)>) -> Totals {
    let mut t = Totals::default();
    match fs::read_dir(dir) {
        Err(_) => t.errors += 1,
        Ok(rd) => {
            for item in rd {
                let Ok(item) = item else {
                    t.errors += 1;
                    break;
                };
                let path = item.path();
                let md = fs::symlink_metadata(&path).unwrap();
                let (is_dir, logical, alloc) = is_dir_and_sizes(&md);
                t.logical += logical;
                t.alloc += alloc;
                if is_dir {
                    t.dirs += 1;
                    if !md.file_type().is_symlink() {
                        let sub = naive(&path, per_dir);
                        t.files += sub.files;
                        t.dirs += sub.dirs;
                        t.logical += sub.logical;
                        t.alloc += sub.alloc;
                        t.errors += sub.errors;
                    }
                } else {
                    t.files += 1;
                }
            }
        }
    }
    per_dir.push((dir.to_path_buf(), t));
    t
}

fn mtime_secs(path: &Path) -> i64 {
    let m = fs::symlink_metadata(path).unwrap().modified().unwrap();
    m.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn walk_matches_independent_computation() {
    let mut tmp = TempDir::new("walk");
    let t0 = Instant::now();
    let (written, locked_enforced) = build_fixture(&mut tmp);
    let build_time = t0.elapsed();
    let root_str = tmp.path.to_str().unwrap().to_string();

    let tree = RwLock::new(Tree::new(&root_str, 0));
    let progress = Progress::new();
    let t0 = Instant::now();
    walk(
        &tree,
        0,
        &tmp.path,
        &StdLister,
        8,
        &progress,
        &AtomicBool::new(false),
    );
    let walk_time = t0.elapsed();

    let mut per_dir = Vec::new();
    let want = naive(&tmp.path, &mut per_dir);
    let tree = tree.into_inner();
    let p = progress.snapshot();
    eprintln!(
        "fixture: {written} files written in {build_time:?}; walk {walk_time:?}: {} files, {} dirs, {} bytes, {} errors; locked dir enforced: {locked_enforced}",
        p.files, p.dirs, p.bytes, p.errors
    );
    assert!(p.done);
    let root = tree.root();
    assert_eq!(
        Totals {
            files: tree.files(root),
            dirs: tree.dirs(root),
            logical: tree.size(root, SizeMode::Logical),
            alloc: tree.size(root, SizeMode::Allocated),
            errors: p.errors,
        },
        want
    );
    assert_eq!(
        (p.files, p.dirs, p.bytes),
        (want.files, want.dirs, want.alloc)
    );
    // Unix: the three symlinks are files; Windows: the junction is a dir.
    let link_files = if cfg!(unix) { 3 } else { 0 };
    assert_eq!(
        want.files,
        written as u64 + link_files - u64::from(locked_enforced)
    );

    // Every directory matches individually. A directory node also carries
    // its own size (from its entry in the parent's listing); the walk root
    // was created by Tree::new and has none.
    for (dir, t) in &per_dir {
        let id = tree
            .find_path(dir.to_str().unwrap())
            .unwrap_or_else(|| panic!("{} not found", dir.display()));
        let (own_l, own_a) = if id == root {
            (0, 0)
        } else {
            let (_, l, a) = is_dir_and_sizes(&fs::symlink_metadata(dir).unwrap());
            (l, a)
        };
        let got = (
            tree.files(id),
            tree.dirs(id),
            tree.size(id, SizeMode::Logical),
            tree.size(id, SizeMode::Allocated),
        );
        assert_eq!(
            got,
            (t.files, t.dirs, t.logical + own_l, t.alloc + own_a),
            "{}",
            dir.display()
        );
        assert_eq!(
            tree.node(id).has(ERROR),
            t.errors > 0 && dir.ends_with("locked")
        );
    }

    // Links are listed but never followed.
    let back = tree.find_path(&format!("{root_str}/loop/back")).unwrap();
    assert!(tree.node(back).has(REPARSE));
    assert_eq!(tree.children(back).count(), 0);
    assert_eq!(tree.node(back).is_dir(), cfg!(windows));
    let looped = tree.find_path(&format!("{root_str}\\LOOP")).unwrap();
    assert_eq!(tree.child_count(looped), if cfg!(unix) { 4 } else { 2 });
    if cfg!(windows) {
        let inside = tree.find_child(looped, "inside.txt").unwrap();
        assert!(tree.node(inside).has(HIDDEN), "attrib +h is reported");
    }

    // Locked directory.
    let locked = tree.find_child(root, "locked").unwrap();
    assert_eq!(tree.node(locked).has(ERROR), locked_enforced);
    assert_eq!(tree.files(locked), if locked_enforced { 0 } else { 1 });
    if !locked_enforced {
        eprintln!(
            "note: running as root, chmod 000 is not enforced; ERROR paths are covered by the in-memory lister tests"
        );
    }

    // Names, extensions, mtimes, paths.
    let uni = tree.find_path(&format!("{root_str}/папка ҐЇЄ")).unwrap();
    let report = tree.find_child(uni, "Звіт.Final.PDF").unwrap();
    assert_eq!(tree.ext_name(tree.node(report).ext()), "pdf");
    assert_eq!(tree.size(report, SizeMode::Logical), 12_345);
    let report_path = tmp.path.join("Папка ґїє").join("Звіт.Final.PDF");
    assert_eq!(tree.node(report).mtime(), mtime_secs(&report_path));
    let expected_path = format!(
        "{root_str}{}Папка ґїє\\Звіт.Final.PDF",
        if root_str.ends_with(['/', '\\']) {
            ""
        } else {
            "\\"
        }
    );
    assert_eq!(tree.path(report), expected_path);
    let big = tree.find_child(root, "big flat dir").unwrap();
    assert_eq!(tree.child_count(big), 2500);
    assert_eq!(
        tree.find_child(root, "empty").map(|e| tree.child_count(e)),
        Some(0)
    );

    // Layout over the scanned tree.
    let l = layout::compute(&tree, root, &Params::default());
    assert_eq!(l.total, want.alloc);
    assert!(l.depth >= 4);
}

/// Path-driven synthetic directory tree, with an optional delay per listing.
struct SlowLister {
    fanout: usize,
    files: usize,
    depth: usize,
    delay: Duration,
    calls: AtomicU64,
}

impl DirLister for SlowLister {
    fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        let level = dir.components().count() - 1;
        if level < self.depth {
            for i in 0..self.fanout {
                out.push(RawEntry {
                    name: format!("d{i}"),
                    is_dir: true,
                    ..Default::default()
                });
            }
        }
        for i in 0..self.files {
            out.push(RawEntry {
                name: format!("f{i}.bin"),
                logical: 1000 + i as u64,
                alloc: 4096,
                ..Default::default()
            });
        }
        Ok(())
    }
}

#[test]
fn concurrent_walk_with_layout_readers() {
    let lister = SlowLister {
        fanout: 5,
        files: 20,
        depth: 6,
        delay: Duration::from_micros(100),
        calls: AtomicU64::new(0),
    };
    let dirs: u64 = (1..=6).map(|d| 5u64.pow(d)).sum(); // 19 530
    let tree = RwLock::new(Tree::new("S", 0));
    let progress = Progress::new();
    let cancel = AtomicBool::new(false);
    let start = Instant::now();
    let (iterations, max_wait, max_layout, saw_current) = std::thread::scope(|s| {
        let reader = s.spawn(|| {
            let mut iterations = 0u32;
            let mut max_wait = Duration::ZERO;
            let mut max_layout = Duration::ZERO;
            let mut saw_current = false;
            while !progress.done.load(Ordering::Acquire) {
                let t0 = Instant::now();
                let t = tree.read();
                max_wait = max_wait.max(t0.elapsed());
                let t1 = Instant::now();
                let l = layout::compute(&t, t.root(), &Params::default());
                max_layout = max_layout.max(t1.elapsed());
                // Consistent at every moment: root = sum of its children.
                let root = t.root();
                let (mut size, mut files, mut dirs) = (0, 0, 0);
                for c in t.children(root) {
                    size += t.size(c, SizeMode::Allocated);
                    files += t.files(c) + u64::from(t.node(c).is_file());
                    dirs += t.dirs(c) + u64::from(t.node(c).is_dir());
                }
                assert_eq!(
                    (size, files, dirs),
                    (
                        t.size(root, SizeMode::Allocated),
                        t.files(root),
                        t.dirs(root)
                    )
                );
                assert_eq!(l.total, t.size(root, SizeMode::Allocated));
                drop(t);
                saw_current |= !progress.current.lock().is_empty();
                iterations += 1;
                std::thread::sleep(Duration::from_millis(2));
            }
            (iterations, max_wait, max_layout, saw_current)
        });
        walk(&tree, 0, Path::new("S"), &lister, 8, &progress, &cancel);
        reader.join().unwrap()
    });
    let elapsed = start.elapsed();
    let t = tree.read();
    eprintln!(
        "concurrent walk: {} nodes in {elapsed:?}; reader: {iterations} layouts, max read-lock wait {max_wait:?}, max layout {max_layout:?}",
        t.len()
    );
    assert_eq!(t.dirs(0), dirs);
    assert_eq!(t.files(0), (dirs + 1) * 20);
    assert_eq!(lister.calls.load(Ordering::Relaxed), dirs + 1);
    assert!(iterations >= 3, "reader starved: {iterations} iterations");
    assert!(
        max_wait < Duration::from_millis(250),
        "read lock waited {max_wait:?}"
    );
    assert!(saw_current);
}

#[test]
fn cancel_is_prompt() {
    // Endless tree, 2 ms per listing.
    let lister = SlowLister {
        fanout: 4,
        files: 10,
        depth: usize::MAX,
        delay: Duration::from_millis(2),
        calls: AtomicU64::new(0),
    };
    let tree = RwLock::new(Tree::new("E", 0));
    let progress = Progress::new();
    let cancel = AtomicBool::new(false);
    let latency = std::thread::scope(|s| {
        let walker = s.spawn(|| walk(&tree, 0, Path::new("E"), &lister, 8, &progress, &cancel));
        std::thread::sleep(Duration::from_millis(300));
        let t0 = Instant::now();
        cancel.store(true, Ordering::Relaxed);
        walker.join().unwrap();
        t0.elapsed()
    });
    let t = tree.read();
    eprintln!(
        "cancel latency {latency:?} after {} listings, {} nodes",
        lister.calls.load(Ordering::Relaxed),
        t.len()
    );
    assert!(
        latency < Duration::from_millis(100),
        "cancel took {latency:?}"
    );
    assert!(progress.done.load(Ordering::Acquire));
    assert!(t.len() > 100);
    // Still consistent.
    let sum: u64 = t.children(0).map(|c| t.size(c, SizeMode::Logical)).sum();
    assert_eq!(sum, t.size(0, SizeMode::Logical));
}

#[test]
fn std_lister_honours_cancel_inside_a_listing() {
    let tmp = TempDir::new("cancel-list");
    for i in 0..2000 {
        fs::write(tmp.path.join(format!("f{i}")), b"").unwrap();
    }
    let mut out = Vec::new();
    StdLister
        .list_cancellable(&tmp.path, &mut out, &AtomicBool::new(true))
        .unwrap();
    assert!(
        out.len() < 2000,
        "listed {} entries despite cancel",
        out.len()
    );
    out.clear();
    StdLister.list(&tmp.path, &mut out).unwrap();
    assert_eq!(out.len(), 2000);
}

/// A real permission error from the OS on Windows, where the session is not
/// elevated: listing "System Volume Information" is denied. Read-only.
#[cfg(windows)]
#[test]
fn walk_reports_access_denied_on_system_dir() {
    let dir = Path::new(r"C:\System Volume Information");
    if !dir.exists() || fs::read_dir(dir).is_ok() {
        eprintln!("skipped: {} missing or readable (elevated?)", dir.display());
        return;
    }
    let tree = RwLock::new(Tree::new(dir.to_str().unwrap(), 0));
    let progress = Progress::new();
    walk(
        &tree,
        0,
        dir,
        &StdLister,
        2,
        &progress,
        &AtomicBool::new(false),
    );
    assert!(tree.read().node(0).has(ERROR));
    assert_eq!(progress.snapshot().errors, 1);
}
