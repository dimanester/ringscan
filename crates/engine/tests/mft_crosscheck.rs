//! Cross-validates MFT mode against a directory walk of the same volume.
//!
//! For every NTFS test image (`fixtures/*.img`, made by
//! `scripts/make-ntfs-fixtures.sh`):
//!
//! 1. MFT side: `ringscan_ntfs::scan_mft` on the image file, then the
//!    engine's [`build_tree`].
//! 2. Walk side: the image is mounted read-only with ntfs-3g (inside the
//!    project, `fixtures/.mnt-engine-<pid>-<image>`, always unmounted again)
//!    and walked with core's walker + `StdLister`, wrapped in the engine's
//!    [`HardLinkFilter`].
//! 3. Both trees are compared path by path: the set of paths, directory or
//!    not, flags, own logical and allocated sizes, and every hard-link set.
//!    Every difference must fall into a category with a known cause (how
//!    ntfs-3g presents reparse points, resident data, directory sizes and
//!    named streams); anything else fails the test. The MFT total including
//!    metafiles is also compared with the used space ntfs-3g reports
//!    (statvfs).
//!
//! Needs root, FUSE and ntfs-3g; without them, or without fixtures, the test
//! prints why and passes. `RINGSCAN_CROSSCHECK=rich-c4k-s512,...` limits the
//! images. Run with `--nocapture` to see the report.
#![cfg(target_os = "linux")]

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CString, c_char, c_void};
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use ringscan_core::parking_lot::{Mutex, RwLock};
use ringscan_core::walk::{self, DirLister, Progress, RawEntry, StdLister};
use ringscan_core::{NodeId, SizeMode, Tree, flags};
use ringscan_engine::mft::orphaned_node;
use ringscan_engine::{HardLinkFilter, build_tree};
use ringscan_ntfs::{MftOptions, MftProgress, scan_mft};

// ------------------------------------------------------------------ setup

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixtures_dir() -> PathBuf {
    let dir = std::env::var_os("RINGSCAN_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace().join("fixtures"));
    // Canonical, so mount points compare equal to /proc/self/mountinfo.
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

fn images() -> Vec<PathBuf> {
    let filter: Option<Vec<String>> = std::env::var("RINGSCAN_CROSSCHECK")
        .ok()
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect());
    let mut out: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "img"))
                .filter(|p| {
                    filter.as_ref().is_none_or(|f| {
                        let stem = p.file_stem().unwrap().to_string_lossy().to_string();
                        f.contains(&stem)
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn can_mount() -> Result<(), String> {
    // SAFETY: plain libc call.
    if unsafe { geteuid() } != 0 {
        return Err("not running as root".into());
    }
    if Command::new("ntfs-3g").arg("--version").output().is_err() {
        return Err("ntfs-3g not installed".into());
    }
    if !std::fs::read_to_string("/proc/filesystems").is_ok_and(|s| s.contains("fuse")) {
        return Err("no FUSE support".into());
    }
    Ok(())
}

// ------------------------------------------------------------------ libc

unsafe extern "C" {
    fn geteuid() -> u32;
    fn llistxattr(path: *const c_char, list: *mut c_char, size: usize) -> isize;
    fn lgetxattr(
        path: *const c_char,
        name: *const c_char,
        value: *mut c_void,
        size: usize,
    ) -> isize;
}

/// Named data streams of `path` as ntfs-3g exposes them with
/// `streams_interface=xattr` (`user.<stream>`): (name, logical size).
fn streams(path: &Path) -> Vec<(String, u64)> {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: valid C string; a null buffer asks for the size.
    let n = unsafe { llistxattr(c.as_ptr(), std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; n as usize];
    // SAFETY: the buffer is writable for its length.
    let n = unsafe { llistxattr(c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 {
        return Vec::new();
    }
    buf.truncate(n as usize);
    buf.split(|&b| b == 0)
        .filter_map(|name| name.strip_prefix(b"user."))
        .map(|stream| {
            let full = CString::new([b"user.".as_slice(), stream].concat()).unwrap();
            // SAFETY: valid C strings; a null buffer asks for the size.
            let size = unsafe { lgetxattr(c.as_ptr(), full.as_ptr(), std::ptr::null_mut(), 0) };
            (
                String::from_utf8_lossy(stream).into_owned(),
                size.max(0) as u64,
            )
        })
        .collect()
}

// ------------------------------------------------------------------ mount

fn mountinfo_points() -> Vec<String> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|p| p.replace("\\040", " "))
        .collect()
}

fn is_mounted(dir: &Path) -> bool {
    let d = dir.to_string_lossy();
    mountinfo_points().iter().any(|p| *p == d)
}

fn unmount(dir: &Path) {
    if is_mounted(dir) {
        let ok = Command::new("fusermount")
            .arg("-u")
            .arg(dir)
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            let _ = Command::new("umount").arg("-l").arg(dir).status();
        }
    }
}

/// A read-only ntfs-3g mount that is always undone on drop.
struct Mount {
    dir: PathBuf,
    child: Option<Child>,
}

impl Mount {
    fn new(image: &Path, dir: PathBuf) -> Result<Mount, String> {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let dir = std::fs::canonicalize(&dir).map_err(|e| e.to_string())?;
        unmount(&dir);
        let child = Command::new("ntfs-3g")
            .arg("-o")
            .arg("ro,no_detach,streams_interface=xattr")
            .arg(image)
            .arg(&dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let mut m = Mount {
            dir,
            child: Some(child),
        };
        let t0 = Instant::now();
        while !is_mounted(&m.dir) {
            if t0.elapsed() > Duration::from_secs(20) {
                return Err("mount did not appear".into());
            }
            if let Some(Ok(Some(status))) = m.child.as_mut().map(|c| c.try_wait()) {
                m.child = None;
                return Err(format!("ntfs-3g exited: {status}"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(m)
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        unmount(&self.dir);
        if let Some(mut c) = self.child.take() {
            if matches!(c.try_wait(), Ok(None)) {
                // Still serving: ask FUSE to let go even if mountinfo did
                // not show the mount (yet).
                let _ = Command::new("fusermount")
                    .arg("-u")
                    .arg(&self.dir)
                    .stderr(Stdio::null())
                    .status();
            }
            let t0 = Instant::now();
            while !matches!(c.try_wait(), Ok(Some(_))) {
                if t0.elapsed() > Duration::from_secs(10) {
                    let _ = c.kill();
                    let _ = c.wait();
                    // A killed daemon leaves a dead mount behind.
                    let _ = Command::new("umount").arg("-l").arg(&self.dir).status();
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Unmounts leftovers of killed runs.
fn clean_stale_mounts() {
    for p in mountinfo_points() {
        if p.contains("/fixtures/.mnt-engine-") {
            unmount(Path::new(&p));
            let _ = std::fs::remove_dir(&p);
        }
    }
}

// ------------------------------------------------------------------ walk

/// Records the file id of every listed entry (path relative to the mount).
struct Recorder<'a, L> {
    inner: L,
    base: &'a Path,
    ids: Mutex<HashMap<String, u64>>,
}

impl<L: DirLister> DirLister for Recorder<'_, L> {
    fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> std::io::Result<()> {
        let start = out.len();
        let r = self.inner.list(dir, out);
        let rel = dir.strip_prefix(self.base).unwrap();
        let mut ids = self.ids.lock();
        for e in &out[start..] {
            let p = rel.join(&e.name).to_string_lossy().into_owned();
            ids.insert(p, e.file_id);
        }
        r
    }
}

// --------------------------------------------------------------- compare

#[derive(Clone, Copy, Debug)]
struct Info {
    is_dir: bool,
    flags: u16,
    logical: u64,
    alloc: u64,
}

/// Relative path -> own sizes for every node below the root, skipping
/// subtrees for which `skip` is true.
fn path_map(t: &Tree, skip: impl Fn(NodeId) -> bool) -> BTreeMap<String, Info> {
    let mut out = BTreeMap::new();
    let mut stack: Vec<(NodeId, String)> = vec![(t.root(), String::new())];
    while let Some((id, path)) = stack.pop() {
        for c in t.children(id) {
            if skip(c) {
                continue;
            }
            let p = if path.is_empty() {
                t.name(c).to_string()
            } else {
                format!("{path}/{}", t.name(c))
            };
            let n = t.node(c);
            let (mut logical, mut alloc) = (n.logical(), n.alloc());
            if n.is_dir() {
                for k in t.children(c) {
                    let kn = t.node(k);
                    if !kn.has(flags::FREE) {
                        logical -= kn.logical();
                        alloc -= kn.alloc();
                    }
                }
                stack.push((c, p.clone()));
            }
            out.insert(
                p,
                Info {
                    is_dir: n.is_dir(),
                    flags: n.flags(),
                    logical,
                    alloc,
                },
            );
        }
    }
    out
}

/// Explained differences: category -> (count, Δlogical, Δalloc) where Δ is
/// MFT minus walk, plus up to 3 example paths.
#[derive(Default)]
struct Tally {
    cats: BTreeMap<&'static str, (u64, i64, i64, Vec<String>)>,
    unexplained: Vec<String>,
}

impl Tally {
    fn add(&mut self, cat: &'static str, path: &str, dl: i64, da: i64) {
        let e = self.cats.entry(cat).or_default();
        e.0 += 1;
        e.1 += dl;
        e.2 += da;
        if e.3.len() < 3 {
            e.3.push(path.to_string());
        }
    }
    fn bad(&mut self, msg: String) {
        self.unexplained.push(msg);
    }
}

fn d(a: u64, b: u64) -> i64 {
    a as i64 - b as i64
}

/// Compares the sizes of one file (MFT `m` vs walk `w`); `real` is the
/// path in the mount, for stream lookups.
fn compare_file(tally: &mut Tally, path: &str, m: &Info, w: &Info, real: &Path) {
    if m.logical == w.logical && m.alloc == w.alloc {
        return;
    }
    let ads = streams(real);
    let ads_logical: u64 = ads.iter().map(|s| s.1).sum();
    if !ads.is_empty() {
        // Named streams are part of the MFT sizes; a walk sees only the
        // unnamed stream, and ntfs-3g does not expose stream allocations.
        if m.logical == w.logical + ads_logical && m.alloc >= w.alloc.min(m.alloc) {
            tally.add(
                "file with named streams: MFT counts them, a walk sees only the unnamed data",
                path,
                d(m.logical, w.logical),
                d(m.alloc, w.alloc),
            );
            return;
        }
        tally.bad(format!(
            "{path}: streams {ads:?} do not explain logical {} vs {}",
            m.logical, w.logical
        ));
        return;
    }
    if m.logical != w.logical {
        tally.bad(format!(
            "{path}: logical MFT {} vs walk {}",
            m.logical, w.logical
        ));
        return;
    }
    if m.alloc == 0 && w.alloc == w.logical.div_ceil(512) * 512 {
        tally.add(
            "MFT-resident data: no clusters (0), ntfs-3g st_blocks = length rounded to 512",
            path,
            0,
            d(m.alloc, w.alloc),
        );
        return;
    }
    tally.bad(format!(
        "{path}: alloc MFT {} vs walk {} (logical {})",
        m.alloc, w.alloc, m.logical
    ));
}

struct ImageReport {
    text: String,
    unexplained: Vec<String>,
}

fn crosscheck(image: &Path) -> Result<ImageReport, String> {
    let name = image.file_stem().unwrap().to_string_lossy().into_owned();
    let mut text = String::new();

    // MFT side.
    let t0 = Instant::now();
    let file = std::fs::File::open(image).map_err(|e| e.to_string())?;
    let scan = scan_mft(
        &file,
        &MftOptions::default(),
        &MftProgress::new(),
        &AtomicBool::new(false),
    )
    .map_err(|e| format!("scan_mft: {e}"))?;
    let t_read = t0.elapsed();
    let t1 = Instant::now();
    let built = build_tree(&scan.files, "X:\\");
    let (mft_tree, build) = (built.tree, built.stats);
    let t_build = t1.elapsed();
    let root_own_alloc = scan
        .files
        .iter()
        .find(|f| f.record == 5)
        .map_or(0, |f| f.alloc);
    let cluster = u64::from(scan.volume.bytes_per_cluster);
    let volume_size = scan.volume.volume_size();
    drop(scan);

    // Walk side.
    let dir = fixtures_dir().join(format!(".mnt-engine-{}-{name}", std::process::id()));
    let mount = Mount::new(image, dir)?;
    let base = mount.dir.clone();
    let recorder = Recorder {
        inner: HardLinkFilter::new(StdLister),
        base: &base,
        ids: Mutex::new(HashMap::new()),
    };
    let walk_tree = RwLock::new(Tree::new(&base.to_string_lossy(), 0));
    let progress = Progress::new();
    let t2 = Instant::now();
    walk::walk(
        &walk_tree,
        0,
        &base,
        &recorder,
        2,
        &progress,
        &AtomicBool::new(false),
    );
    let t_walk = t2.elapsed();
    let walk_tree = walk_tree.into_inner();
    let ids = recorder.ids.into_inner();
    let df = Command::new("stat")
        .args(["-f", "--format=%S %b %f"])
        .arg(&base)
        .output()
        .map_err(|e| e.to_string())?;
    let df: Vec<u64> = String::from_utf8_lossy(&df.stdout)
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    let used = (df[1] - df[2]) * df[0];

    // Path maps (metafiles and <orphaned> are invisible through ntfs-3g).
    let orphaned = orphaned_node(&mft_tree);
    let mft_map = path_map(&mft_tree, |id| {
        mft_tree.node(id).has(flags::METAFILE) || Some(id) == orphaned
    });
    let walk_map = path_map(&walk_tree, |_| false);

    let mut tally = Tally::default();
    for p in mft_map.keys().filter(|p| !walk_map.contains_key(*p)) {
        tally.bad(format!("only in MFT: {p}"));
    }
    for p in walk_map.keys().filter(|p| !mft_map.contains_key(*p)) {
        tally.bad(format!("only in walk: {p}"));
    }

    // Hard-link sets: walk paths grouped by inode (= MFT record).
    let mut by_ino: HashMap<u64, Vec<String>> = HashMap::new();
    for (p, &ino) in &ids {
        if walk_map.get(p).is_some_and(|w| !w.is_dir) {
            by_ino.entry(ino).or_default().push(p.clone());
        }
    }
    by_ino.retain(|_, v| v.len() > 1);
    let in_group: HashMap<&str, u64> = by_ino
        .iter()
        .flat_map(|(&ino, v)| v.iter().map(move |p| (p.as_str(), ino)))
        .collect();
    let mut link_entries = 0;
    for paths in by_ino.values() {
        link_entries += paths.len() - 1;
        let mut counted = |map: &BTreeMap<String, Info>, side: &str| -> Option<String> {
            let primaries: Vec<&String> = paths
                .iter()
                .filter(|p| map.get(*p).is_some_and(|i| !i.has_link()))
                .collect();
            let zero_links = paths.iter().all(|p| {
                map.get(p)
                    .is_some_and(|i| !i.has_link() || (i.logical == 0 && i.alloc == 0))
            });
            if primaries.len() != 1 || !zero_links {
                tally.bad(format!(
                    "{side}: hard-link set {paths:?} has {} counted links",
                    primaries.len()
                ));
                return None;
            }
            Some(primaries[0].clone())
        };
        let (Some(mp), Some(wp)) = (counted(&mft_map, "MFT"), counted(&walk_map, "walk")) else {
            continue;
        };
        let (m, w) = (mft_map[&mp], walk_map[&wp]);
        // Compare the counted sizes (same file, possibly different names).
        let mut sub = Tally::default();
        compare_file(&mut sub, &wp, &m, &w, &base.join(&wp));
        tally.unexplained.append(&mut sub.unexplained);
        for (cat, (_, dl, da, _)) in sub.cats {
            tally.add(cat, &wp, dl, da);
        }
        tally.add("hard-link set: sizes counted once, on one link", &wp, 0, 0);
    }
    for (p, m) in &mft_map {
        if m.has_link() && !in_group.contains_key(p.as_str()) {
            tally.bad(format!("MFT hard link outside any walk link set: {p}"));
        }
    }

    // Path by path.
    let (mut files, mut dirs) = (0u64, 0u64);
    for (p, m) in &mft_map {
        let Some(w) = walk_map.get(p) else { continue };
        let real = base.join(p);
        if w.flags & flags::REPARSE != 0 || m.flags & (flags::REPARSE | flags::CLOUD) != 0 {
            let both = w.flags & flags::REPARSE != 0;
            match (m.flags & (flags::REPARSE | flags::CLOUD) != 0, both) {
                (true, true) => tally.add(
                    "reparse point (junction/symlink/cloud): ntfs-3g shows a symlink",
                    p,
                    d(m.logical, w.logical),
                    d(m.alloc, w.alloc),
                ),
                (false, true) if !m.is_dir => tally.add(
                    "Interix symlink: a plain file to Windows, a symlink to ntfs-3g",
                    p,
                    d(m.logical, w.logical),
                    d(m.alloc, w.alloc),
                ),
                _ => tally.bad(format!(
                    "{p}: flags MFT {:#x} vs walk {:#x}",
                    m.flags, w.flags
                )),
            }
            continue;
        }
        if m.is_dir != w.is_dir {
            tally.bad(format!("{p}: dir MFT {} vs walk {}", m.is_dir, w.is_dir));
            continue;
        }
        if m.is_dir {
            dirs += 1;
            let ads: u64 = streams(&real).iter().map(|s| s.1).sum();
            // ntfs-3g reports the data size of a directory's index as
            // st_size (its allocation, rounded to clusters, as st_blocks);
            // NTFS (and the MFT reader) give directories logical size 0
            // plus their named streams.
            if m.logical != ads || w.logical > w.alloc {
                tally.bad(format!(
                    "{p}: dir logical MFT {} (streams {ads}) vs walk {} / {}",
                    m.logical, w.logical, w.alloc
                ));
            } else if m.logical != w.logical {
                tally.add(
                    "directory: ntfs-3g st_size = index data size, NTFS logical = 0 (+streams)",
                    p,
                    d(m.logical, w.logical),
                    0,
                );
            }
            if m.alloc != w.alloc {
                if ads > 0 && m.alloc > w.alloc {
                    tally.add(
                        "directory with named streams: their allocation is invisible to a walk",
                        p,
                        0,
                        d(m.alloc, w.alloc),
                    );
                } else {
                    tally.bad(format!(
                        "{p}: dir alloc MFT {} vs walk {}",
                        m.alloc, w.alloc
                    ));
                }
            }
            continue;
        }
        files += 1;
        if in_group.contains_key(p.as_str()) {
            continue; // compared per link set above
        }
        compare_file(&mut tally, p, m, w, &real);
    }

    // Totals: explained deltas must add up to the root difference.
    let hidden = |mode: SizeMode| -> u64 {
        let meta: u64 = mft_tree
            .children(0)
            .filter(|&c| mft_tree.node(c).has(flags::METAFILE))
            .map(|c| mft_tree.size(c, mode))
            .sum();
        meta + orphaned.map_or(0, |o| mft_tree.size(o, mode))
    };
    let ml = mft_tree.size(0, SizeMode::Logical) - hidden(SizeMode::Logical);
    let ma = mft_tree.size(0, SizeMode::Allocated) - hidden(SizeMode::Allocated);
    let wl = walk_tree.size(0, SizeMode::Logical);
    let wa = walk_tree.size(0, SizeMode::Allocated);
    let sum_dl: i64 = tally.cats.values().map(|c| c.1).sum();
    let sum_da: i64 = tally.cats.values().map(|c| c.2).sum();
    if d(ml, wl) != sum_dl || d(ma, wa) != sum_da {
        tally.bad(format!(
            "totals: MFT-walk = {} logical / {} alloc, explained {sum_dl} / {sum_da}",
            d(ml, wl),
            d(ma, wa)
        ));
    }

    let mft_all = mft_tree.size(0, SizeMode::Allocated);
    let _ = writeln!(
        text,
        "== {name}: volume {} MiB, ntfs-3g used {} bytes",
        volume_size >> 20,
        used
    );
    let _ = writeln!(
        text,
        "   MFT: read {:.0} ms, tree {:.0} ms: {} nodes ({} files, {} dirs, {} hard links, {} metafiles, {} orphans)",
        t_read.as_secs_f64() * 1e3,
        t_build.as_secs_f64() * 1e3,
        build.nodes,
        build.files,
        build.dirs,
        build.hardlinks,
        build.metafiles,
        build.orphans
    );
    let _ = writeln!(
        text,
        "   walk of the ntfs-3g mount: {:.0} ms, {} entries; compared {} paths ({files} files, {dirs} dirs, {} link sets, {link_entries} extra links)",
        t_walk.as_secs_f64() * 1e3,
        walk_map.len(),
        mft_map.len(),
        by_ino.len()
    );
    let _ = writeln!(
        text,
        "   totals without metafiles: MFT {ml} logical / {ma} alloc, walk {wl} / {wa}"
    );
    let _ = writeln!(
        text,
        "   MFT alloc incl. metafiles {mft_all} + root index {root_own_alloc} = {} vs used {used}: {} bytes ({} clusters) not in $DATA/$INDEX_ALLOCATION",
        mft_all + root_own_alloc,
        d(used, mft_all + root_own_alloc),
        d(used, mft_all + root_own_alloc) / cluster as i64
    );
    // Used clusters not covered by $DATA / $INDEX_ALLOCATION: other
    // non-resident attributes ($MFT:$BITMAP, a large $SECURITY_DESCRIPTOR
    // or $ATTRIBUTE_LIST), which the MFT reader does not count. A few
    // clusters; never negative (that would mean double counting).
    let gap = used as i64 - (mft_all + root_own_alloc) as i64;
    if !(0..=32 * cluster as i64).contains(&gap) {
        tally.bad(format!(
            "used {used} - MFT total {} = {gap} bytes, outside 0..=32 clusters",
            mft_all + root_own_alloc
        ));
    }
    for (cat, (n, dl, da, ex)) in &tally.cats {
        let _ = writeln!(
            text,
            "   {n:>6} x {cat}: Δlogical {dl}, Δalloc {da}  e.g. {ex:?}"
        );
    }
    drop(mount);
    Ok(ImageReport {
        text,
        unexplained: tally.unexplained,
    })
}

trait LinkFlag {
    fn has_link(&self) -> bool;
}

impl LinkFlag for Info {
    fn has_link(&self) -> bool {
        self.flags & flags::HARDLINK != 0
    }
}

#[test]
fn mft_tree_matches_ntfs3g_walk() {
    let imgs = images();
    if imgs.is_empty() {
        eprintln!(
            "NOTE: no NTFS fixtures in {} - run `sudo scripts/make-ntfs-fixtures.sh`; cross-check skipped",
            fixtures_dir().display()
        );
        return;
    }
    if let Err(why) = can_mount() {
        eprintln!("NOTE: cannot mount NTFS images ({why}); cross-check skipped");
        return;
    }
    clean_stale_mounts();
    let mut failures = Vec::new();
    for img in &imgs {
        match crosscheck(img) {
            Ok(r) => {
                println!("{}", r.text);
                for u in r.unexplained.iter().take(20) {
                    println!("   UNEXPLAINED: {u}");
                }
                if !r.unexplained.is_empty() {
                    failures.push(format!(
                        "{}: {} unexplained differences",
                        img.display(),
                        r.unexplained.len()
                    ));
                }
            }
            Err(e) => failures.push(format!("{}: {e}", img.display())),
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
