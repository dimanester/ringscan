//! Parallel directory walker.
//!
//! [`walk`] fills a directory node of a shared `RwLock<Tree>` from the file
//! system with N threads while the UI keeps reading the tree.
//!
//! # Design
//!
//! * **Work distribution.** One shared LIFO stack of directories still to be
//!   listed (`Mutex<Vec<Job>>` + `Condvar`) instead of per-thread
//!   work-stealing deques (crossbeam-deque). Listing one directory costs tens
//!   of microseconds to milliseconds of system calls while a queue operation
//!   costs ~100 ns, so a single queue is never the bottleneck, and
//!   termination (a pending counter: queued + in progress) and cancellation
//!   stay trivial to get right. LIFO (depth-first) keeps the queue short
//!   (memory ~ depth x fan-out instead of the widest level of the tree) and
//!   lists neighbouring directories close together in time.
//! * **Locking.** A directory is listed without any lock held, then inserted
//!   with one [`Tree::add_batch`] under a short write lock (directories with
//!   more than [`BATCH_MAX`] entries are split into slices so no write lock
//!   is held for more than ~1-2 ms). The queue lock and the tree lock are
//!   never held at the same time, so there is no lock-order deadlock;
//!   `parking_lot::RwLock` is task-fair (a waiting writer blocks new readers
//!   and vice versa), so neither the UI's short read locks at ~10 Hz nor the
//!   writers starve.
//! * **Reparse points** (entries with REPARSE) are inserted but never
//!   descended into, so junction / symlink loops cannot recurse.
//! * **Errors.** A failed or partial listing still inserts whatever was
//!   listed, sets ERROR on the directory node and increments
//!   `progress.errors`; entries the lister flagged ERROR count as errors too.
//! * **Cancellation.** Workers check `cancel` before each directory, while
//!   waiting for work (10 ms slices) and, through
//!   [`DirLister::list_cancellable`], every few hundred entries of a listing;
//!   a listing interrupted by cancel is discarded. `walk` returns once every
//!   thread has stopped. Directories that were queued but not listed stay in
//!   the tree as empty directories (without ERROR).
//! * **Races with deletes.** If the UI removes a node whose listing is in
//!   flight, `add_batch` sees a tombstone parent and drops the batch.
//! * **Progress.** Counters are updated once per inserted batch; `current` is
//!   refreshed by each thread at most every 50 ms with `try_lock`, so it
//!   never blocks a worker.
//! * **Hard links** are not detected by the walker (RawEntry has no link
//!   count): in walk mode every link of a file is counted. MFT mode (engine)
//!   counts them once.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, RwLock};

use crate::NodeId;
use crate::flags;
use crate::tree::Tree;

/// Largest number of entries inserted under one write lock.
pub const BATCH_MAX: usize = 16 * 1024;
/// How long an idle worker sleeps before re-checking `cancel`.
const WAIT_SLICE: Duration = Duration::from_millis(10);
/// Minimum interval between two `progress.current` updates of one thread.
const CURRENT_EVERY: Duration = Duration::from_millis(50);
/// StdLister checks `cancel` every this many entries.
const CANCEL_CHECK_EVERY: usize = 256;

/// One directory entry as produced by a [`DirLister`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawEntry {
    /// File name (no path).
    pub name: String,
    /// Directory (authoritative; DIR in `flags` is ignored).
    pub is_dir: bool,
    /// Logical size; for directories their own size (usually 0).
    pub logical: u64,
    /// Allocated size; for directories their own size (usually 0).
    pub alloc: u64,
    /// [`crate::flags`] bits (REPARSE, HIDDEN, CLOUD, ERROR, ...).
    pub flags: u16,
    /// Last modification, unix seconds; 0 = unknown.
    pub mtime: i64,
    /// File id / inode (0 = unknown); not stored in the tree.
    pub file_id: u64,
}

/// Lists one directory. Implemented by [`StdLister`] here and by the engine
/// over `ringscan-win` (FileIdBothDirectoryInfo) on Windows.
pub trait DirLister: Send + Sync {
    /// Appends the entries of `dir` (without "." and "..") to `out`, which
    /// the caller has cleared. On error, entries pushed so far are kept and
    /// inserted as a partial listing.
    fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()>;

    /// Like [`DirLister::list`], but may stop early once `cancel` is set
    /// (the walker then discards the result). The default ignores `cancel`;
    /// override it if a single listing can take long (huge directories,
    /// slow network shares).
    fn list_cancellable(
        &self,
        dir: &Path,
        out: &mut Vec<RawEntry>,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        let _ = cancel;
        self.list(dir, out)
    }
}

/// [`DirLister`] over `std::fs` (no extra dependencies; used on Linux in
/// tests and as a fallback). Metadata is read without following symlinks
/// (`DirEntry::metadata`, i.e. `symlink_metadata`; free on Windows, one
/// `lstat` on unix).
///
/// * unix: `alloc = st_blocks * 512`, symlinks get REPARSE, directories
///   report their own size (like `du`), `file_id` = inode.
/// * Windows: `alloc` = logical rounded up to 4096 (std exposes no
///   allocation size, so compression / sparseness are not reflected),
///   directories report 0, flags from the file attributes (HIDDEN, SYSTEM,
///   COMPRESSED, SPARSE, CLOUD); REPARSE only for name surrogates (symlinks,
///   junctions), so e.g. OneDrive folders are still descended; online-only
///   cloud files get `alloc` 0.
/// * other platforms: like Windows without attributes.
///
/// Names that are not valid Unicode are converted lossily. An entry whose
/// metadata cannot be read is listed with ERROR and zero size; entries that
/// vanished between listing and `lstat` are skipped.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdLister;

impl DirLister for StdLister {
    fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()> {
        list_std(dir, out, None)
    }

    fn list_cancellable(
        &self,
        dir: &Path,
        out: &mut Vec<RawEntry>,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        list_std(dir, out, Some(cancel))
    }
}

fn list_std(dir: &Path, out: &mut Vec<RawEntry>, cancel: Option<&AtomicBool>) -> io::Result<()> {
    for (i, item) in fs::read_dir(dir)?.enumerate() {
        if i % CANCEL_CHECK_EVERY == CANCEL_CHECK_EVERY - 1
            && cancel.is_some_and(|c| c.load(Ordering::Relaxed))
        {
            return Ok(());
        }
        let entry = item?;
        let name = entry.file_name().to_string_lossy().into_owned();
        match entry.metadata() {
            Ok(md) => out.push(entry_from_metadata(name, &md)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => out.push(RawEntry {
                name,
                is_dir: entry.file_type().is_ok_and(|t| t.is_dir()),
                flags: flags::ERROR,
                ..RawEntry::default()
            }),
        }
    }
    Ok(())
}

/// Converts `symlink_metadata`-style metadata to a [`RawEntry`]. Public so
/// tests and tools can compute the exact values [`StdLister`] reports.
#[cfg(unix)]
pub fn entry_from_metadata(name: String, md: &fs::Metadata) -> RawEntry {
    use std::os::unix::fs::MetadataExt;
    let ft = md.file_type();
    RawEntry {
        name,
        is_dir: ft.is_dir(),
        logical: md.len(),
        alloc: md.blocks() * 512,
        flags: if ft.is_symlink() { flags::REPARSE } else { 0 },
        mtime: md.mtime(),
        file_id: md.ino(),
    }
}

/// Converts `symlink_metadata`-style metadata to a [`RawEntry`]. Public so
/// tests and tools can compute the exact values [`StdLister`] reports.
#[cfg(windows)]
pub fn entry_from_metadata(name: String, md: &fs::Metadata) -> RawEntry {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN: u32 = 0x2;
    const SYSTEM: u32 = 0x4;
    const DIRECTORY: u32 = 0x10;
    const SPARSE: u32 = 0x200;
    const COMPRESSED: u32 = 0x800;
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    let attrs = md.file_attributes();
    let is_dir = attrs & DIRECTORY != 0;
    let mut f = 0;
    for (attr, flag) in [
        (HIDDEN, flags::HIDDEN),
        (SYSTEM, flags::SYSTEM),
        (SPARSE, flags::SPARSE),
        (COMPRESSED, flags::COMPRESSED),
        (
            OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS,
            flags::CLOUD,
        ),
    ] {
        if attrs & attr != 0 {
            f |= flag;
        }
    }
    // Name surrogates only (symlinks, junctions); std's is_symlink() checks
    // the reparse tag for exactly that.
    if md.file_type().is_symlink() {
        f |= flags::REPARSE;
    }
    let logical = if is_dir { 0 } else { md.file_size() };
    let not_local = attrs & (OFFLINE | RECALL_ON_DATA_ACCESS) != 0;
    let alloc = if not_local {
        0
    } else {
        logical.div_ceil(4096) * 4096
    };
    let ft = md.last_write_time();
    let mtime = if ft == 0 {
        0
    } else {
        (ft / 10_000_000) as i64 - 11_644_473_600
    };
    RawEntry {
        name,
        is_dir,
        logical,
        alloc,
        flags: f,
        mtime,
        file_id: 0,
    }
}

/// Converts metadata to a [`RawEntry`] (no allocation size available).
#[cfg(not(any(unix, windows)))]
pub fn entry_from_metadata(name: String, md: &fs::Metadata) -> RawEntry {
    let ft = md.file_type();
    let logical = if ft.is_dir() { 0 } else { md.len() };
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    RawEntry {
        name,
        is_dir: ft.is_dir(),
        logical,
        alloc: logical.div_ceil(4096) * 4096,
        flags: if ft.is_symlink() { flags::REPARSE } else { 0 },
        mtime,
        file_id: 0,
    }
}

/// Live counters of a running walk (shared with the UI).
#[derive(Debug, Default)]
pub struct Progress {
    /// Files inserted (non-directory entries).
    pub files: AtomicU64,
    /// Directories inserted (directory entries, the start node excluded).
    pub dirs: AtomicU64,
    /// Sum of the allocated sizes of inserted entries (hard links excluded).
    pub bytes: AtomicU64,
    /// Failed listings + entries flagged ERROR.
    pub errors: AtomicU64,
    /// A directory recently being listed.
    pub current: Mutex<String>,
    /// Set by [`walk`] when it returns (finished or cancelled). Never reset.
    pub done: AtomicBool,
}

/// Plain copy of [`Progress`] at one moment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
    pub current: String,
    pub done: bool,
}

impl Progress {
    pub fn new() -> Progress {
        Progress::default()
    }

    /// Reads all counters (each atomically; not a consistent cut).
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            files: self.files.load(Ordering::Relaxed),
            dirs: self.dirs.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            current: self.current.lock().clone(),
            done: self.done.load(Ordering::Acquire),
        }
    }
}

struct Job {
    id: NodeId,
    path: PathBuf,
}

struct QueueState {
    jobs: Vec<Job>,
    /// Jobs queued or being processed; the walk is over when it reaches 0.
    pending: usize,
}

struct Queue {
    state: Mutex<QueueState>,
    cv: Condvar,
}

impl Queue {
    /// Next directory to list, or None when the walk is complete or
    /// cancelled.
    fn pop(&self, cancel: &AtomicBool) -> Option<Job> {
        let mut st = self.state.lock();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(job) = st.jobs.pop() {
                return Some(job);
            }
            if st.pending == 0 {
                return None;
            }
            self.cv.wait_for(&mut st, WAIT_SLICE);
        }
    }

    /// Marks one job done and queues the subdirectories it found.
    fn finish(&self, new_jobs: &mut Vec<Job>) {
        let added = new_jobs.len();
        let mut st = self.state.lock();
        st.jobs.append(new_jobs);
        st.pending = st.pending + added - 1;
        let done = st.pending == 0;
        drop(st);
        // With one new job the current worker takes it itself.
        if done || added > 1 {
            self.cv.notify_all();
        }
    }
}

/// Walks `path` and inserts everything below it under node `at` (which must
/// be a live directory, usually empty: freshly created or after
/// [`Tree::clear_children`]). Uses `threads` threads (the calling thread is
/// one of them), one write lock per listed directory, and returns when the
/// walk is complete or `cancel` was set. Sets `progress.done` on return.
pub fn walk(
    tree: &RwLock<Tree>,
    at: NodeId,
    path: &Path,
    lister: &dyn DirLister,
    threads: usize,
    progress: &Progress,
    cancel: &AtomicBool,
) {
    let queue = Queue {
        state: Mutex::new(QueueState {
            jobs: vec![Job {
                id: at,
                path: path.to_path_buf(),
            }],
            pending: 1,
        }),
        cv: Condvar::new(),
    };
    std::thread::scope(|s| {
        for i in 1..threads.max(1) {
            let queue = &queue;
            std::thread::Builder::new()
                .name(format!("ringscan-walk-{i}"))
                .spawn_scoped(s, move || worker(queue, tree, lister, progress, cancel))
                .expect("failed to spawn walker thread");
        }
        worker(&queue, tree, lister, progress, cancel);
    });
    progress.done.store(true, Ordering::Release);
}

fn worker(
    queue: &Queue,
    tree: &RwLock<Tree>,
    lister: &dyn DirLister,
    progress: &Progress,
    cancel: &AtomicBool,
) {
    let mut entries: Vec<RawEntry> = Vec::new();
    let mut subdirs: Vec<Job> = Vec::new();
    let mut last_current: Option<Instant> = None;
    while let Some(job) = queue.pop(cancel) {
        if last_current.is_none_or(|t| t.elapsed() >= CURRENT_EVERY) {
            if let Some(mut cur) = progress.current.try_lock() {
                cur.clear();
                cur.push_str(&job.path.to_string_lossy());
            }
            last_current = Some(Instant::now());
        }
        entries.clear();
        let result = lister.list_cancellable(&job.path, &mut entries, cancel);
        // An empty, successful listing changes nothing: skip the write lock.
        if !cancel.load(Ordering::Relaxed) && (result.is_err() || !entries.is_empty()) {
            insert_listing(
                tree,
                &job,
                &entries,
                result.is_err(),
                progress,
                &mut subdirs,
            );
        }
        queue.finish(&mut subdirs);
    }
}

/// Inserts one listing (in slices of at most BATCH_MAX entries per write
/// lock) and collects the subdirectories to descend into.
fn insert_listing(
    tree: &RwLock<Tree>,
    job: &Job,
    entries: &[RawEntry],
    failed: bool,
    progress: &Progress,
    subdirs: &mut Vec<Job>,
) {
    let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
    let mut errors = 0u64;
    let mut start = 0;
    loop {
        let end = (start + BATCH_MAX).min(entries.len());
        let slice = &entries[start..end];
        let new_dirs = {
            let mut t = tree.write();
            if !t.is_live(job.id) {
                break; // removed while we were listing: drop the rest
            }
            if start == 0 && failed {
                t.set_flags(job.id, flags::ERROR, 0);
                errors += 1;
            }
            t.add_batch(job.id, slice)
        };
        for e in slice {
            if e.is_dir {
                dirs += 1;
            } else {
                files += 1;
            }
            if e.flags & flags::HARDLINK == 0 {
                bytes += e.alloc;
            }
            if e.flags & flags::ERROR != 0 {
                errors += 1;
            }
        }
        for (id, i) in new_dirs {
            let e = &slice[i];
            if e.flags & flags::REPARSE == 0 {
                subdirs.push(Job {
                    id,
                    path: job.path.join(&e.name),
                });
            }
        }
        start = end;
        if start >= entries.len() {
            break;
        }
    }
    progress.files.fetch_add(files, Ordering::Relaxed);
    progress.dirs.fetch_add(dirs, Ordering::Relaxed);
    progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    progress.errors.fetch_add(errors, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SizeMode;

    /// In-memory lister: directory "d<depth>_<i>" has `fanout` subdirs and
    /// `files` files of 100 bytes, down to `depth` levels.
    struct FakeLister {
        fanout: usize,
        files: usize,
        depth: usize,
        fail: &'static str,
    }

    impl DirLister for FakeLister {
        fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()> {
            let level = dir.components().count() - 1;
            if dir.file_name().is_some_and(|n| n == self.fail) {
                out.push(RawEntry {
                    name: "partial".into(),
                    logical: 1,
                    alloc: 1,
                    ..Default::default()
                });
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
            }
            if level < self.depth {
                for i in 0..self.fanout {
                    out.push(RawEntry {
                        name: format!("d{level}_{i}"),
                        is_dir: true,
                        ..Default::default()
                    });
                }
            }
            for i in 0..self.files {
                out.push(RawEntry {
                    name: format!("f{i}.dat"),
                    logical: 100,
                    alloc: 4096,
                    ..Default::default()
                });
            }
            out.push(RawEntry {
                name: "link".into(),
                is_dir: true,
                flags: flags::REPARSE,
                ..Default::default()
            });
            Ok(())
        }
    }

    #[test]
    fn walk_fake_tree_counts_everything() {
        let lister = FakeLister {
            fanout: 3,
            files: 4,
            depth: 4,
            fail: "",
        };
        let tree = RwLock::new(Tree::new("root", 0));
        let progress = Progress::new();
        let cancel = AtomicBool::new(false);
        walk(&tree, 0, Path::new("root"), &lister, 4, &progress, &cancel);
        let t = tree.read();
        // dirs: 3 + 9 + 27 + 81 = 120 real + one REPARSE "link" per listed dir.
        let listed = 1 + 120;
        assert_eq!(t.dirs(0), 120 + listed);
        assert_eq!(t.files(0), 4 * listed);
        assert_eq!(t.size(0, SizeMode::Logical), 400 * listed);
        let s = progress.snapshot();
        assert!(s.done);
        assert_eq!(
            (s.files, s.dirs, s.bytes, s.errors),
            (4 * listed, 120 + listed, 4 * 4096 * listed, 0)
        );
        for (id, n) in t.live_nodes() {
            if n.has(flags::REPARSE) {
                assert_eq!(t.children(id).count(), 0);
            }
        }
    }

    #[test]
    fn walk_records_errors_and_partial_listings() {
        let lister = FakeLister {
            fanout: 2,
            files: 1,
            depth: 2,
            fail: "d1_1",
        };
        let tree = RwLock::new(Tree::new("root", 0));
        let progress = Progress::new();
        walk(
            &tree,
            0,
            Path::new("root"),
            &lister,
            3,
            &progress,
            &AtomicBool::new(false),
        );
        let t = tree.read();
        let failed: Vec<_> = t
            .live_nodes()
            .filter(|(_, n)| n.has(flags::ERROR))
            .map(|(id, _)| id)
            .collect();
        assert_eq!(failed.len(), 2, "two directories named d1_1");
        for id in failed {
            assert_eq!(t.name(id), "d1_1");
            assert_eq!(t.files(id), 1, "partial entry kept");
        }
        assert_eq!(progress.snapshot().errors, 2);
    }

    #[test]
    fn walk_missing_root_sets_error() {
        let tree = RwLock::new(Tree::new("x", 0));
        let progress = Progress::new();
        let missing = std::env::temp_dir()
            .join("ringscan-dev")
            .join("definitely-missing-dir-1b9f");
        walk(
            &tree,
            0,
            &missing,
            &StdLister,
            2,
            &progress,
            &AtomicBool::new(false),
        );
        assert!(tree.read().node(0).has(flags::ERROR));
        assert_eq!(progress.snapshot().errors, 1);
        assert!(progress.snapshot().done);
    }

    #[test]
    fn walk_into_removed_node_is_dropped() {
        let mut t = Tree::new("root", 0);
        let d = t.add_dir(0, "d", 0, 0);
        t.remove(d);
        let tree = RwLock::new(t);
        let lister = FakeLister {
            fanout: 2,
            files: 2,
            depth: 3,
            fail: "",
        };
        walk(
            &tree,
            d,
            Path::new("root/d"),
            &lister,
            2,
            &Progress::new(),
            &AtomicBool::new(false),
        );
        let t = tree.read();
        assert_eq!(t.live_len(), 1);
        assert_eq!(t.size(0, SizeMode::Logical), 0);
    }

    #[test]
    fn huge_directory_is_split_into_slices() {
        struct Big;
        impl DirLister for Big {
            fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()> {
                if dir == Path::new("r") {
                    for i in 0..(BATCH_MAX * 2 + 5) {
                        out.push(RawEntry {
                            name: format!("{i}"),
                            is_dir: i % 1000 == 0,
                            logical: 1,
                            ..Default::default()
                        });
                    }
                }
                Ok(())
            }
        }
        let tree = RwLock::new(Tree::new("r", 0));
        let progress = Progress::new();
        walk(
            &tree,
            0,
            Path::new("r"),
            &Big,
            2,
            &progress,
            &AtomicBool::new(false),
        );
        let t = tree.read();
        let n = (BATCH_MAX * 2 + 5) as u64;
        let dirs = n.div_ceil(1000);
        assert_eq!((t.files(0), t.dirs(0)), (n - dirs, dirs));
        assert_eq!(progress.snapshot().dirs, dirs);
        let names: Vec<_> = t.children(0).map(|c| t.name(c).to_string()).collect();
        assert_eq!(names.len() as u64, n);
        assert_eq!(
            names[BATCH_MAX],
            BATCH_MAX.to_string(),
            "order kept across slices"
        );
    }
}
