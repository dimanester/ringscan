//! Scan jobs: [`ScanJob`] scans a drive, a folder or all fixed drives on
//! background threads and fills a shared `Arc<RwLock<Tree>>` that the UI can
//! lay out at any moment.
//!
//! # Life of a job
//!
//! [`ScanJob::start`] creates the tree (root `C:\`, the folder path, or a
//! virtual "This PC"), spawns a coordinator thread and returns at once, so
//! even the drive enumeration (up to 1.5 s for a dead network drive) never
//! blocks the caller. The coordinator splits the target into *units* (one
//! per drive, or the folder) and runs them:
//!
//! * **Parallel per physical disk.** Units on different physical disks
//!   ([`ringscan_win::DriveInfo::disk_number`]) run concurrently, partitions
//!   of the same disk one after another (two walkers on one disk mostly
//!   fight over its queue); drives whose disk is unknown get their own lane.
//! * **Method per unit.** [`Method::Auto`] reads the MFT when the unit is a
//!   whole drive and the drive is NTFS; otherwise it walks the directories.
//!   It tries the MFT even without elevation: opening the raw volume then
//!   usually fails at once (access denied, well under a millisecond) and the
//!   unit walks, but some volumes grant raw read access to ordinary users
//!   (the development machine's data drive does), and those are then
//!   scanned in seconds and completely. If reading the MFT fails, the unit
//!   falls back to the walk and records why ([`UnitProgress::note`]); a
//!   cancelled MFT read does not fall back.
//! * **MFT units** read the MFT and build a *private* tree (no lock held,
//!   the UI sees the progress in MFT records), then install it with one
//!   write lock: a fresh single-drive scan simply swaps the tree in (the old
//!   tree is just the root, so no id the UI holds changes meaning; the
//!   generation is bumped past the old one), anything else is
//!   `clear_children` + `graft` below the drive node.
//! * **Walk units** run core's parallel walker directly on the shared tree
//!   (one short write lock per directory), so the chart grows live.
//!   Listing is [`crate::lister::WinLister`] on Windows (core's
//!   `StdLister` elsewhere, which keeps the engine testable on Linux),
//!   wrapped in a [`HardLinkFilter`] so each hard-linked file counts once.
//! * **Drive extras.** After each whole-drive unit the drive's `<free
//!   space>` and `<unaccounted>` nodes are added (see [`crate::drive`]),
//!   using free space queried at that moment.
//! * **Sorting.** When everything is done the children of every directory
//!   are sorted by allocated size ([`ScanOptions::sort`]): the directories
//!   are collected under a read lock and sorted a few thousand per write
//!   lock, so a reader (the UI, every frame) never waits more than a few
//!   milliseconds. One write lock for the whole tree (~150–250 ms for a
//!   system drive) froze the GUI for one long frame at the end of each scan.
//!
//! Progress ([`ScanJob::progress`]) is read from per-unit atomics without
//! touching the tree lock. [`ScanJob::cancel`] stops walkers within
//! milliseconds and the MFT reader at its next chunk; dropping a job
//! cancels it too (it does not wait).
//!
//! # After the scan
//!
//! The job stays the handle for everything that changes the tree:
//! [`ScanJob::rescan`] re-walks a folder (or re-scans a drive) into the
//! same tree and returns a new job for its progress; [`ScanJob::delete`]
//! recycles or deletes items through the shell, removes them from the tree
//! and updates the drive's free / unaccounted space;
//! [`ScanJob::empty_recycle_bin`] does the same for the Recycle Bin.
//!
//! Hard links stay counted once across rescans: every walk (and every MFT
//! read) leaves a per-volume [`LinkMap`] of the multi-linked files and the
//! directory whose link carries each file's size (a few MB for a system
//! drive; the full file-id table of the walk itself is dropped when the
//! walk ends). A folder rescan seeds its hard-link filter with the files
//! counted outside the folder and recounts those counted inside it.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ringscan_core::parking_lot::{Mutex, RwLock};
use ringscan_core::walk::{self, DirLister, Progress};
use ringscan_core::{NodeId, SizeMode, Tree, flags};
use ringscan_ntfs::{MftOptions, MftProgress};
use ringscan_win::DriveInfo;

use crate::drive::{self, DriveSpace};
use crate::lister::{
    HardLinkFilter, LinkMap, LossyPaths, RECYCLE_BIN_DIR, dir_hash, drive_root_letter,
};
use crate::mft::{self, MftBuildStats};

/// Walker threads per unit when [`ScanOptions::threads`] is 0. Measured on
/// `C:\Windows` (453K entries, see the crate README): 1 thread 33–36 s,
/// 4 threads 21–24 s, 8–32 threads 21–27 s. Walking is bound by the cost of
/// opening each directory (kernel and filter drivers) and flattens out from
/// 4 threads on; 8 leaves headroom for drives with more independent
/// subtrees without wasting threads.
pub const DEFAULT_WALK_THREADS: usize = 8;

/// Name of the virtual root of an [`Target::AllFixedDrives`] scan.
pub const THIS_PC_NAME: &str = "This PC";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// What to scan.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// A whole drive, by letter.
    Drive(char),
    /// A folder (a drive root such as `C:\` is treated as [`Target::Drive`]).
    Path(PathBuf),
    /// Every fixed drive (see [`crate::drive::fixed_drives`]) below a virtual
    /// "This PC" root.
    AllFixedDrives,
}

impl Target {
    /// Parses a command-line argument: `all` (any case), a drive (`C`, `C:`,
    /// `C:\`) or a path. Pass Explorer arguments through
    /// `ringscan_win::context_menu::path_from_arg` first.
    pub fn parse(arg: &str) -> Target {
        let a = arg.trim();
        if a.eq_ignore_ascii_case("all") {
            return Target::AllFixedDrives;
        }
        if a.len() == 1 && a.as_bytes()[0].is_ascii_alphabetic() {
            return Target::Drive(a.as_bytes()[0].to_ascii_uppercase() as char);
        }
        Target::Path(PathBuf::from(a)).normalized()
    }

    /// Upper-cases drive letters and turns a drive-root path into
    /// [`Target::Drive`].
    pub fn normalized(&self) -> Target {
        match self {
            Target::Drive(c) => Target::Drive(c.to_ascii_uppercase()),
            Target::Path(p) => match p.to_str().and_then(drive_root_letter) {
                Some(l) => Target::Drive(l),
                None => Target::Path(p.clone()),
            },
            Target::AllFixedDrives => Target::AllFixedDrives,
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Drive(c) => write!(f, "{c}:\\"),
            Target::Path(p) => write!(f, "{}", p.display()),
            Target::AllFixedDrives => f.write_str(THIS_PC_NAME),
        }
    }
}

/// How to scan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Method {
    /// MFT for a whole NTFS drive (falling back to the walk when the raw
    /// volume cannot be read, e.g. without elevation), else walk.
    #[default]
    Auto,
    /// Read the MFT (falls back to the walk, with a note, if impossible).
    Mft,
    /// Walk the directories.
    Walk,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Method::Auto => "auto",
            Method::Mft => "mft",
            Method::Walk => "walk",
        }
    }

    /// `auto`, `mft` or `walk` (any case).
    pub fn parse(s: &str) -> Option<Method> {
        [Method::Auto, Method::Mft, Method::Walk]
            .into_iter()
            .find(|m| m.name().eq_ignore_ascii_case(s.trim()))
    }

    fn to_u8(m: Option<Method>) -> u8 {
        match m {
            None => 0,
            Some(Method::Auto) => 1,
            Some(Method::Mft) => 2,
            Some(Method::Walk) => 3,
        }
    }

    fn from_u8(v: u8) -> Option<Method> {
        match v {
            1 => Some(Method::Auto),
            2 => Some(Method::Mft),
            3 => Some(Method::Walk),
            _ => None,
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Options of a scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanOptions {
    pub method: Method,
    /// Walker threads per unit, and MFT parse threads; 0 = defaults
    /// ([`DEFAULT_WALK_THREADS`] for walks, all cores for MFT parsing).
    pub threads: usize,
    /// Walk mode: count each hard-linked file once (default true). MFT mode
    /// always counts hard links once.
    pub dedupe_hardlinks: bool,
    /// Sort all children by allocated size when done (default true).
    pub sort: bool,
    /// Add `<free space>` / `<unaccounted>` to scanned drives (default true).
    pub drive_extras: bool,
}

impl Default for ScanOptions {
    fn default() -> ScanOptions {
        ScanOptions {
            method: Method::Auto,
            threads: 0,
            dedupe_hardlinks: true,
            sort: true,
            drive_extras: true,
        }
    }
}

impl ScanOptions {
    /// Walker threads actually used.
    pub fn walk_threads(&self) -> usize {
        if self.threads == 0 {
            DEFAULT_WALK_THREADS
        } else {
            self.threads
        }
    }
}

/// Where a scan (or one unit of it) is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Phase {
    /// Preparing (e.g. enumerating drives) or waiting for its turn.
    Starting = 0,
    /// Reading the MFT (see the `mft_*` record counters).
    ReadingMft = 1,
    /// Building the tree from the MFT listing.
    BuildingTree = 2,
    /// Walking the directories.
    Walking = 3,
    /// Adding drive extras / sorting.
    Finishing = 4,
    Done = 5,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::ReadingMft => "reading-mft",
            Phase::BuildingTree => "building-tree",
            Phase::Walking => "walking",
            Phase::Finishing => "finishing",
            Phase::Done => "done",
        }
    }

    fn from_u8(v: u8) -> Phase {
        match v {
            1 => Phase::ReadingMft,
            2 => Phase::BuildingTree,
            3 => Phase::Walking,
            4 => Phase::Finishing,
            5 => Phase::Done,
            _ => Phase::Starting,
        }
    }
}

/// MFT facts of a unit scanned in MFT mode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MftSummary {
    /// MFT record slots scanned.
    pub records: u64,
    /// Record slots whose sectors could not be read.
    pub unreadable_records: u64,
    /// Reading + parsing the MFT.
    pub read_time: Duration,
    /// Building the tree from the listing.
    pub build_time: Duration,
    pub build: MftBuildStats,
}

/// Progress of one unit (a drive or the scanned folder).
#[derive(Clone, Debug)]
pub struct UnitProgress {
    /// `C:\` or the folder path.
    pub name: String,
    /// The tree node this unit fills.
    pub node: NodeId,
    /// Drive letter of the volume, if known.
    pub drive: Option<char>,
    pub phase: Phase,
    /// [`Method::Mft`] or [`Method::Walk`] once decided.
    pub method: Option<Method>,
    /// Why the MFT was not used or failed, if it was considered.
    pub note: Option<String>,
    pub files: u64,
    pub dirs: u64,
    /// Allocated bytes found so far.
    pub bytes: u64,
    /// Folders that could not be (fully) listed, plus entries with errors.
    pub errors: u64,
    /// A folder being listed recently (walk), or "" .
    pub current: String,
    pub mft_records_done: u64,
    pub mft_records_total: u64,
    pub mft: Option<MftSummary>,
    pub elapsed: Duration,
}

/// Progress of a whole job (sums over its units).
#[derive(Clone, Debug)]
pub struct ScanProgress {
    pub phase: Phase,
    /// The method every unit used; `None` while undecided or if units
    /// differ (see `units`).
    pub method: Option<Method>,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
    pub current: String,
    pub mft_records_done: u64,
    pub mft_records_total: u64,
    pub elapsed: Duration,
    pub cancelled: bool,
    pub units: Vec<UnitProgress>,
}

impl ScanProgress {
    /// Files (plus directories) per second so far.
    pub fn entries_per_sec(&self) -> f64 {
        let s = self.elapsed.as_secs_f64();
        if s > 0.0 {
            (self.files + self.dirs) as f64 / s
        } else {
            0.0
        }
    }

    /// Files per second so far.
    pub fn files_per_sec(&self) -> f64 {
        let s = self.elapsed.as_secs_f64();
        if s > 0.0 { self.files as f64 / s } else { 0.0 }
    }
}

/// One scanned drive with its space accounting.
#[derive(Clone, Debug)]
pub struct DriveSummary {
    pub info: DriveInfo,
    /// The drive's node in the tree.
    pub node: NodeId,
    pub space: DriveSpace,
    pub method: Option<Method>,
    pub note: Option<String>,
}

/// How [`ScanJob::delete`] removes items.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeleteMode {
    /// Move to the Recycle Bin; never deletes permanently (an item that
    /// cannot be recycled fails with an error for which
    /// `ringscan_win::shell::is_not_recyclable` is true).
    Recycle,
    /// Delete permanently (the caller confirms first).
    Permanent,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct DriveSlot {
    info: DriveInfo,
    node: NodeId,
}

/// Shared by a job and the rescans started from it.
struct Context {
    target: Target,
    opts: ScanOptions,
    /// File-system path of a non-virtual root.
    root_path: Option<PathBuf>,
    drives: Mutex<Vec<DriveSlot>>,
    lossy: Arc<LossyPaths>,
    /// Multi-linked files per volume (drive letter; None = unknown volume).
    links: Mutex<HashMap<Option<char>, LinkMap>>,
}

impl Context {
    fn slots(&self) -> Vec<DriveSlot> {
        self.drives.lock().clone()
    }

    fn update_slot_info(&self, node: NodeId, info: DriveInfo) {
        if let Some(s) = self.drives.lock().iter_mut().find(|s| s.node == node) {
            s.info = info;
        }
    }

    /// The drive whose node is `id` or an ancestor of it.
    fn slot_containing(&self, t: &Tree, id: NodeId) -> Option<DriveSlot> {
        let slots = self.slots();
        if !t.is_live(id) {
            return None;
        }
        std::iter::once(id)
            .chain(t.ancestors(id))
            .find_map(|a| slots.iter().find(|s| s.node == a).cloned())
    }

    /// Real file-system path of `id` (None for virtual nodes, nodes inside
    /// a virtual container such as `<orphaned>`, and a virtual root).
    fn node_path(&self, t: &Tree, id: NodeId) -> Option<PathBuf> {
        let raw = self.node_path_raw(t, id)?;
        Some(self.lossy.translate(&raw).into_owned())
    }

    /// The path of `id` as the walker spells it (names with unpaired
    /// surrogates in their lossy form; see [`LossyPaths`]).
    fn node_path_raw(&self, t: &Tree, id: NodeId) -> Option<PathBuf> {
        if !t.is_live(id) {
            return None;
        }
        let root = t.root();
        let mut chain = vec![id];
        chain.extend(t.ancestors(id));
        chain.pop(); // the root
        chain.reverse();
        let mut path = if t.node(root).is_virtual() {
            let (&first, rest) = chain.split_first()?;
            if t.node(first).is_virtual() {
                return None;
            }
            let p = PathBuf::from(t.name(first));
            chain = rest.to_vec();
            p
        } else {
            self.root_path.clone()?
        };
        for c in chain {
            if t.node(c).is_virtual() {
                return None;
            }
            path.push(t.name(c));
        }
        Some(path)
    }
}

/// One drive or folder of a job.
struct Unit {
    name: String,
    node: NodeId,
    path: PathBuf,
    drive: Option<DriveInfo>,
    /// `node` is a drive root: MFT possible, extras added.
    whole_drive: bool,
    /// Clear `node` before filling it.
    rescan: bool,
    walk: Progress,
    mft: MftProgress,
    phase: AtomicU8,
    method: AtomicU8,
    note: Mutex<Option<String>>,
    mft_summary: Mutex<Option<MftSummary>>,
    times: Mutex<(Option<Instant>, Option<Duration>)>,
}

impl Unit {
    fn new(
        name: String,
        node: NodeId,
        path: PathBuf,
        drive: Option<DriveInfo>,
        whole_drive: bool,
        rescan: bool,
    ) -> Arc<Unit> {
        Arc::new(Unit {
            name,
            node,
            path,
            drive,
            whole_drive,
            rescan,
            walk: Progress::new(),
            mft: MftProgress::new(),
            phase: AtomicU8::new(Phase::Starting as u8),
            method: AtomicU8::new(0),
            note: Mutex::new(None),
            mft_summary: Mutex::new(None),
            times: Mutex::new((None, None)),
        })
    }

    /// Key of the unit's volume in [`Context::links`].
    fn volume(&self) -> Option<char> {
        self.drive
            .as_ref()
            .map(|d| d.letter)
            .or_else(|| self.path.to_str().and_then(path_drive_letter))
    }

    fn set_phase(&self, p: Phase) {
        self.phase.store(p as u8, Ordering::Release);
    }

    fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Acquire))
    }

    fn set_method(&self, m: Method) {
        self.method.store(Method::to_u8(Some(m)), Ordering::Release);
    }

    fn method(&self) -> Option<Method> {
        Method::from_u8(self.method.load(Ordering::Acquire))
    }

    fn set_note(&self, note: String) {
        *self.note.lock() = Some(note);
    }

    fn begin(&self) {
        self.times.lock().0 = Some(Instant::now());
    }

    fn end(&self) {
        let mut t = self.times.lock();
        t.1 = Some(t.0.map_or(Duration::ZERO, |s| s.elapsed()));
    }

    fn snapshot(&self) -> UnitProgress {
        let w = self.walk.snapshot();
        let phase = self.phase();
        let (start, end) = *self.times.lock();
        let current = match phase {
            Phase::Walking => w.current,
            Phase::ReadingMft => format!("{} (MFT)", self.name),
            _ => String::new(),
        };
        UnitProgress {
            name: self.name.clone(),
            node: self.node,
            drive: self.drive.as_ref().map(|d| d.letter),
            phase,
            method: self.method(),
            note: self.note.lock().clone(),
            files: w.files,
            dirs: w.dirs,
            bytes: w.bytes,
            errors: w.errors,
            current,
            mft_records_done: self.mft.done.load(Ordering::Relaxed),
            mft_records_total: self.mft.total.load(Ordering::Relaxed),
            mft: self.mft_summary.lock().clone(),
            elapsed: end.unwrap_or_else(|| start.map_or(Duration::ZERO, |s| s.elapsed())),
        }
    }
}

struct JobState {
    cancel: AtomicBool,
    done: AtomicBool,
    finishing: AtomicBool,
    started: Instant,
    elapsed: Mutex<Option<Duration>>,
    units: Mutex<Vec<Arc<Unit>>>,
}

enum Plan {
    /// A new scan of the context's target.
    Fresh,
    /// Prepared units; afterwards sort the subtrees of `sort_roots`.
    Rescan {
        units: Vec<Arc<Unit>>,
        sort_roots: Vec<NodeId>,
    },
}

// ---------------------------------------------------------------------------
// ScanJob
// ---------------------------------------------------------------------------

/// A running or finished scan and the handle to its tree. See the
/// [module docs](self).
pub struct ScanJob {
    tree: Arc<RwLock<Tree>>,
    ctx: Arc<Context>,
    state: Arc<JobState>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl ScanJob {
    /// Starts scanning `target` on background threads and returns at once.
    pub fn start(target: Target, opts: ScanOptions) -> ScanJob {
        let target = target.normalized();
        let (root_name, root_flags, root_path) = match &target {
            Target::Drive(c) => {
                let root = format!("{c}:\\");
                (root.clone(), 0, Some(PathBuf::from(root)))
            }
            Target::Path(p) => {
                let (name, path) = normalize_path(p);
                (name, 0, Some(path))
            }
            Target::AllFixedDrives => (THIS_PC_NAME.to_string(), flags::VIRTUAL, None),
        };
        let tree = Arc::new(RwLock::new(Tree::new(&root_name, root_flags)));
        let ctx = Arc::new(Context {
            target,
            opts,
            root_path,
            drives: Mutex::new(Vec::new()),
            lossy: Arc::new(LossyPaths::new()),
            links: Mutex::new(HashMap::new()),
        });
        ScanJob::spawn(tree, ctx, Plan::Fresh)
    }

    fn spawn(tree: Arc<RwLock<Tree>>, ctx: Arc<Context>, plan: Plan) -> ScanJob {
        let state = Arc::new(JobState {
            cancel: AtomicBool::new(false),
            done: AtomicBool::new(false),
            finishing: AtomicBool::new(false),
            started: Instant::now(),
            elapsed: Mutex::new(None),
            units: Mutex::new(Vec::new()),
        });
        let (t, c, s) = (Arc::clone(&tree), Arc::clone(&ctx), Arc::clone(&state));
        let worker = std::thread::Builder::new()
            .name("ringscan-scan".into())
            .spawn(move || run(&t, &c, &s, plan))
            .expect("failed to spawn the scan thread");
        ScanJob {
            tree,
            ctx,
            state,
            worker: Mutex::new(Some(worker)),
        }
    }

    /// The shared tree (read it with `tree().read()`; keep the lock short).
    pub fn tree(&self) -> &Arc<RwLock<Tree>> {
        &self.tree
    }

    /// The (normalized) target of the scan this job belongs to.
    pub fn target(&self) -> &Target {
        &self.ctx.target
    }

    pub fn options(&self) -> &ScanOptions {
        &self.ctx.opts
    }

    /// Current progress (cheap: atomics only, no tree lock).
    pub fn progress(&self) -> ScanProgress {
        let units: Vec<Arc<Unit>> = self.state.units.lock().clone();
        let snaps: Vec<UnitProgress> = units.iter().map(|u| u.snapshot()).collect();
        let done = self.state.done.load(Ordering::Acquire);
        let phase = if done {
            Phase::Done
        } else if self.state.finishing.load(Ordering::Acquire) {
            Phase::Finishing
        } else {
            [
                Phase::ReadingMft,
                Phase::BuildingTree,
                Phase::Walking,
                Phase::Finishing,
            ]
            .into_iter()
            .find(|p| snaps.iter().any(|u| u.phase == *p))
            .unwrap_or(Phase::Starting)
        };
        let method = match snaps.first().and_then(|u| u.method) {
            Some(m) if snaps.iter().all(|u| u.method == Some(m)) => Some(m),
            _ => None,
        };
        let current = snaps
            .iter()
            .find(|u| !u.current.is_empty())
            .map(|u| u.current.clone())
            .unwrap_or_default();
        ScanProgress {
            phase,
            method,
            files: snaps.iter().map(|u| u.files).sum(),
            dirs: snaps.iter().map(|u| u.dirs).sum(),
            bytes: snaps.iter().map(|u| u.bytes).sum(),
            errors: snaps.iter().map(|u| u.errors).sum(),
            current,
            mft_records_done: snaps.iter().map(|u| u.mft_records_done).sum(),
            mft_records_total: snaps.iter().map(|u| u.mft_records_total).sum(),
            elapsed: self
                .state
                .elapsed
                .lock()
                .unwrap_or_else(|| self.state.started.elapsed()),
            cancelled: self.state.cancel.load(Ordering::Relaxed),
            units: snaps,
        }
    }

    /// Asks the scan to stop (walkers stop within milliseconds, the MFT
    /// reader after its current chunk). What was scanned stays in the tree.
    pub fn cancel(&self) {
        self.state.cancel.store(true, Ordering::Relaxed);
    }

    /// The scan has finished (completed or cancelled) and the tree is final.
    pub fn is_done(&self) -> bool {
        self.state.done.load(Ordering::Acquire)
    }

    /// Blocks until the scan has finished; returns the final progress.
    pub fn wait(&self) -> ScanProgress {
        let handle = self.worker.lock().take();
        if let Some(h) = handle {
            let _ = h.join();
        }
        while !self.is_done() {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.progress()
    }

    /// Real file-system path of a node (None for virtual nodes and nodes
    /// inside `<orphaned>`). Takes a read lock; use [`ScanJob::node_path_in`]
    /// when you already hold one.
    pub fn node_path(&self, node: NodeId) -> Option<PathBuf> {
        self.ctx.node_path(&self.tree.read(), node)
    }

    /// [`ScanJob::node_path`] with a tree the caller has already locked
    /// (`t` must be this job's tree).
    pub fn node_path_in(&self, t: &Tree, node: NodeId) -> Option<PathBuf> {
        self.ctx.node_path(t, node)
    }

    /// The scanned drives with their space accounting.
    pub fn drives(&self) -> Vec<DriveSummary> {
        let slots = self.ctx.slots();
        let units: Vec<Arc<Unit>> = self.state.units.lock().clone();
        let t = self.tree.read();
        slots
            .into_iter()
            .map(|s| {
                let unit = units.iter().find(|u| u.node == s.node);
                DriveSummary {
                    space: drive::drive_space(&t, s.node, s.info.total),
                    method: unit.and_then(|u| u.method()),
                    note: unit.and_then(|u| u.note.lock().clone()),
                    node: s.node,
                    info: s.info,
                }
            })
            .collect()
    }

    /// Re-scans `node` into the same tree and returns a job for its
    /// progress. A file rescans its folder; the virtual "This PC" root
    /// rescans every drive; a drive node rescans the whole drive (MFT
    /// possible); a node inside a virtual container (`<orphaned>`) rescans
    /// its drive. The node's children are cleared when the rescan starts.
    pub fn rescan(&self, node: NodeId) -> io::Result<ScanJob> {
        let slots = self.ctx.slots();
        let (units, sort_roots) = {
            let t = self.tree.read();
            if !t.is_live(node) {
                return Err(invalid("the item no longer exists"));
            }
            let root = t.root();
            let mut target = node;
            if !t.node(target).is_dir() {
                target = t.node(target).parent();
            }
            let in_virtual = std::iter::once(target)
                .chain(t.ancestors(target))
                .any(|a| a != root && t.node(a).is_virtual());
            if in_virtual {
                target = self
                    .ctx
                    .slot_containing(&t, target)
                    .map_or(root, |s| s.node);
            }
            let drive_unit = |s: &DriveSlot| {
                let name = format!("{}:\\", s.info.letter);
                Unit::new(
                    name.clone(),
                    s.node,
                    PathBuf::from(name),
                    Some(s.info.clone()),
                    true,
                    true,
                )
            };
            if target == root && t.node(root).is_virtual() {
                let units: Vec<_> = slots.iter().map(drive_unit).collect();
                let roots = units.iter().map(|u| u.node).collect();
                (units, roots)
            } else if let Some(s) = slots.iter().find(|s| s.node == target) {
                (vec![drive_unit(s)], vec![target])
            } else {
                let path = self
                    .ctx
                    .node_path_raw(&t, target)
                    .ok_or_else(|| invalid("the item has no file-system path"))?;
                let drive = self.ctx.slot_containing(&t, target).map(|s| s.info);
                let name = path.display().to_string();
                let unit = Unit::new(name, target, path, drive, false, true);
                (vec![unit], vec![target])
            }
        };
        Ok(ScanJob::spawn(
            Arc::clone(&self.tree),
            Arc::clone(&self.ctx),
            Plan::Rescan { units, sort_roots },
        ))
    }

    /// Recycles or permanently deletes the items of `nodes` (one shell
    /// operation), removes the ones that are gone from the tree and updates
    /// the free / unaccounted space of their drives (after a recycle, the
    /// drive's Recycle Bin node is refreshed first). On error, items that
    /// were removed before the error are still taken out of the tree and
    /// the error is returned. Refuses roots, drives, virtual nodes, NTFS
    /// metafiles and items inside `<orphaned>` (`InvalidInput`).
    ///
    /// Blocks for the shell operation and the Recycle Bin refresh (both can
    /// take seconds for big folders), so call it from a worker thread;
    /// `ScanJob` is `Send + Sync`.
    pub fn delete(&self, nodes: &[NodeId], mode: DeleteMode) -> io::Result<()> {
        let mut items: Vec<(NodeId, PathBuf)> = Vec::new();
        {
            let t = self.tree.read();
            for &id in nodes {
                let Some(n) = t.get(id) else {
                    return Err(invalid("unknown item"));
                };
                if !n.is_live() {
                    continue; // already removed
                }
                if id == t.root() || n.is_virtual() || n.has(flags::METAFILE) {
                    return Err(invalid("this item cannot be deleted"));
                }
                let path = self
                    .ctx
                    .node_path(&t, id)
                    .ok_or_else(|| invalid("the item has no file-system path"))?;
                if path.to_str().and_then(drive_root_letter).is_some() {
                    return Err(invalid("a whole drive cannot be deleted"));
                }
                items.push((id, path));
            }
        }
        if items.is_empty() {
            return Ok(());
        }
        let paths: Vec<PathBuf> = items.iter().map(|(_, p)| p.clone()).collect();
        let result = match mode {
            DeleteMode::Recycle => ringscan_win::shell::recycle(&paths),
            DeleteMode::Permanent => ringscan_win::shell::delete_permanently(&paths),
        };
        let gone: Vec<NodeId> = items
            .iter()
            .filter(|(_, p)| result.is_ok() || std::fs::symlink_metadata(p).is_err())
            .map(|(id, _)| *id)
            .collect();
        if !gone.is_empty() {
            let mut slots: Vec<DriveSlot> = Vec::new();
            {
                let t = self.tree.read();
                for &id in &gone {
                    if let Some(s) = self.ctx.slot_containing(&t, id)
                        && !slots.iter().any(|x| x.node == s.node)
                    {
                        slots.push(s);
                    }
                }
            }
            {
                let mut t = self.tree.write();
                for &id in &gone {
                    t.remove(id);
                }
            }
            for s in &slots {
                if mode == DeleteMode::Recycle {
                    self.refresh_bin(s);
                }
                refresh_extras(&self.tree, &self.ctx, s);
            }
        }
        result
    }

    /// Empties the Recycle Bin (`None` = all drives) through the shell, then
    /// refreshes the bin nodes and the space accounting of the scanned
    /// drives.
    pub fn empty_recycle_bin(&self, drive: Option<char>) -> io::Result<()> {
        ringscan_win::shell::empty_recycle_bin(drive)?;
        let drive = drive.map(|c| c.to_ascii_uppercase());
        for s in self.ctx.slots() {
            if drive.is_none_or(|d| d == s.info.letter) {
                self.refresh_bin(&s);
                refresh_extras(&self.tree, &self.ctx, &s);
            }
        }
        Ok(())
    }

    /// Re-reads the free space of the scanned drives and updates their
    /// `<free space>` / `<unaccounted>` nodes (e.g. after the user changed
    /// files outside RingScan).
    pub fn refresh_drive_extras(&self) {
        if !self.ctx.opts.drive_extras {
            return;
        }
        for s in self.ctx.slots() {
            refresh_extras(&self.tree, &self.ctx, &s);
        }
    }

    /// Re-lists the per-user folders of a drive's `$Recycle.Bin` node.
    /// Folders that cannot be listed (other users' bins when not elevated)
    /// keep their previous contents.
    fn refresh_bin(&self, slot: &DriveSlot) {
        let bin = {
            let t = self.tree.read();
            if !t.is_live(slot.node) {
                return;
            }
            t.children(slot.node)
                .find(|&c| t.node(c).has(flags::RECYCLE) && t.node(c).is_dir())
        };
        let Some(bin) = bin else { return };
        let bin_path = PathBuf::from(format!("{}:\\{RECYCLE_BIN_DIR}", slot.info.letter));
        let lister = make_lister(&self.ctx, slot.info.cluster);
        refresh_bin_in(&self.tree, bin, &bin_path, &*lister);
    }
}

/// Re-lists the per-user folders of the Recycle Bin node `bin` (at
/// `bin_path`): each folder that can be listed completely replaces the
/// node's old contents (a new folder is added); a folder that cannot be
/// listed keeps its old contents. Each folder is walked into a private tree
/// first, so the shared tree is only locked for the swap.
fn refresh_bin_in(tree: &RwLock<Tree>, bin: NodeId, bin_path: &Path, lister: &dyn DirLister) {
    let mut entries = Vec::new();
    if lister.list(bin_path, &mut entries).is_err() {
        return;
    }
    let never = AtomicBool::new(false);
    for e in entries
        .iter()
        .filter(|e| e.is_dir && e.flags & flags::REPARSE == 0)
    {
        let private = RwLock::new(Tree::new(&e.name, 0));
        let progress = Progress::new();
        walk::walk(
            &private,
            0,
            &bin_path.join(&e.name),
            lister,
            2,
            &progress,
            &never,
        );
        let private = private.into_inner();
        if private.node(0).has(flags::ERROR) {
            continue;
        }
        let mut t = tree.write();
        if !t.is_live(bin) {
            return;
        }
        let dir = match t.find_child(bin, &e.name) {
            Some(c) => {
                t.clear_children(c);
                c
            }
            None => t.add_node(bin, &e.name, 0, 0, e.flags | flags::DIR, e.mtime),
        };
        t.graft(dir, &private);
    }
}

impl Drop for ScanJob {
    /// Dropping a job cancels its scan (without waiting for it).
    fn drop(&mut self) {
        self.cancel();
    }
}

// ---------------------------------------------------------------------------
// The coordinator and units
// ---------------------------------------------------------------------------

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

/// Display name and walk path for a folder target: absolute, without a
/// trailing separator, and in plain form (`\\?\C:\x` → `C:\x`) because the
/// shell does not accept verbatim paths.
fn normalize_path(p: &Path) -> (String, PathBuf) {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let Some(s) = abs.to_str() else {
        // Not representable as UTF-8: keep the exact path for walking.
        return (abs.to_string_lossy().into_owned(), abs);
    };
    let mut s = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\")
        && rest.as_bytes().get(1) == Some(&b':')
    {
        rest.to_string()
    } else {
        s.to_string()
    };
    while s.len() > 1 && s.ends_with(['\\', '/']) && drive_root_letter(&s).is_none() {
        s.pop();
    }
    let path = PathBuf::from(&s);
    (s, path)
}

/// Drive letter of an absolute Windows path (`C:\...`, `\\?\C:\...`).
fn path_drive_letter(s: &str) -> Option<char> {
    let s = s.strip_prefix(r"\\?\").unwrap_or(s);
    let b = s.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        .then(|| b[0].to_ascii_uppercase() as char)
}

/// Marks the job finished when dropped, so `wait()` returns even if the
/// coordinator panicked (only possible with unwinding, i.e. in tests).
struct Finish<'a>(&'a JobState);

impl Drop for Finish<'_> {
    fn drop(&mut self) {
        *self.0.elapsed.lock() = Some(self.0.started.elapsed());
        self.0.done.store(true, Ordering::Release);
    }
}

fn run(tree: &RwLock<Tree>, ctx: &Context, state: &JobState, plan: Plan) {
    let _finish = Finish(state);
    let (units, sort_roots) = match plan {
        Plan::Fresh => (fresh_units(tree, ctx), None),
        Plan::Rescan { units, sort_roots } => (units, Some(sort_roots)),
    };
    *state.units.lock() = units.clone();
    let groups = drive::group_by_disk(units, |u| u.drive.as_ref().and_then(|d| d.disk_number));
    std::thread::scope(|s| {
        for (i, group) in groups.into_iter().enumerate() {
            std::thread::Builder::new()
                .name(format!("ringscan-disk-{i}"))
                .spawn_scoped(s, move || {
                    for u in group {
                        scan_unit(tree, ctx, state, &u);
                    }
                })
                .expect("failed to spawn a scan thread");
        }
    });
    if ctx.opts.sort && !state.cancel.load(Ordering::Relaxed) {
        state.finishing.store(true, Ordering::Release);
        let roots = sort_roots.unwrap_or_else(|| vec![tree.read().root()]);
        for r in roots {
            sort_subtree(tree, r);
        }
    }
}

/// Units of a new scan; creates the drive nodes of "This PC".
fn fresh_units(tree: &RwLock<Tree>, ctx: &Context) -> Vec<Arc<Unit>> {
    let root = tree.read().root();
    match &ctx.target {
        Target::Drive(c) => {
            let name = format!("{c}:\\");
            let info = drive::drive_info(*c);
            if let Some(i) = &info {
                ctx.drives.lock().push(DriveSlot {
                    info: i.clone(),
                    node: root,
                });
            }
            vec![Unit::new(
                name.clone(),
                root,
                PathBuf::from(name),
                info,
                true,
                false,
            )]
        }
        Target::Path(_) => {
            let path = ctx.root_path.clone().unwrap_or_default();
            let info = path
                .to_str()
                .and_then(path_drive_letter)
                .and_then(drive::drive_info);
            let name = tree.read().name(root).to_string();
            vec![Unit::new(name, root, path, info, false, false)]
        }
        Target::AllFixedDrives => {
            let drives = drive::fixed_drives();
            let nodes: Vec<NodeId> = {
                let mut t = tree.write();
                drives
                    .iter()
                    .map(|d| t.add_dir(root, &format!("{}:\\", d.letter), 0, 0))
                    .collect()
            };
            let mut units = Vec::new();
            let mut slots = ctx.drives.lock();
            for (d, node) in drives.into_iter().zip(nodes) {
                let name = format!("{}:\\", d.letter);
                slots.push(DriveSlot {
                    info: d.clone(),
                    node,
                });
                units.push(Unit::new(
                    name.clone(),
                    node,
                    PathBuf::from(name),
                    Some(d),
                    true,
                    false,
                ));
            }
            units
        }
    }
}

/// Whether to read the MFT for `u`: `Ok(letter)` to try it, `Err(reason)`
/// to walk (`None` = walking is simply what was asked for).
fn mft_decision(method: Method, u: &Unit) -> Result<char, Option<String>> {
    if method == Method::Walk {
        return Err(None);
    }
    if !u.whole_drive {
        return Err((method == Method::Mft)
            .then(|| "MFT mode reads whole drives only; walked the folder".to_string()));
    }
    let Some(d) = &u.drive else {
        return Err(Some(
            "drive information unavailable; walked the drive".into(),
        ));
    };
    if !d.fs.eq_ignore_ascii_case("NTFS") {
        return Err(Some(format!(
            "{}: is {}, not NTFS; walked the drive",
            d.letter,
            if d.fs.is_empty() { "unknown" } else { &d.fs }
        )));
    }
    Ok(d.letter)
}

fn scan_unit(tree: &RwLock<Tree>, ctx: &Context, state: &JobState, u: &Unit) {
    let cancel = &state.cancel;
    u.begin();
    if cancel.load(Ordering::Relaxed) {
        u.set_phase(Phase::Done);
        u.end();
        return;
    }
    let mut need_walk = true;
    match mft_decision(ctx.opts.method, u) {
        Ok(letter) => {
            u.set_method(Method::Mft);
            u.set_phase(Phase::ReadingMft);
            match scan_mft_into(tree, ctx, u, letter, cancel) {
                Ok(()) => need_walk = false,
                Err(_) if cancel.load(Ordering::Relaxed) => need_walk = false,
                Err(e)
                    if e.kind() == io::ErrorKind::PermissionDenied
                        && !ringscan_win::is_elevated() =>
                {
                    u.set_note(
                        "not elevated: raw volume access denied, so the MFT could not be \
                         read; walked the drive"
                            .into(),
                    )
                }
                Err(e) => u.set_note(format!(
                    "MFT read failed ({e}); used the directory walk instead"
                )),
            }
        }
        Err(Some(reason)) => u.set_note(reason),
        Err(None) => {}
    }
    if need_walk {
        u.set_method(Method::Walk);
        u.set_phase(Phase::Walking);
        walk_unit(tree, ctx, u, cancel);
    }
    if ctx.opts.drive_extras {
        u.set_phase(Phase::Finishing);
        let slot = ctx.slot_containing(&tree.read(), u.node);
        if let Some(slot) = slot {
            refresh_extras(tree, ctx, &slot);
        }
    }
    u.set_phase(Phase::Done);
    u.end();
}

/// Walk path of a unit, with hard-link bookkeeping (see the module docs).
fn walk_unit(tree: &RwLock<Tree>, ctx: &Context, u: &Unit, cancel: &AtomicBool) {
    let cluster = u.drive.as_ref().map_or(0, |d| d.cluster);
    let threads = ctx.opts.walk_threads();
    if !ctx.opts.dedupe_hardlinks {
        if u.rescan {
            tree.write().clear_children(u.node);
        }
        let lister = platform_lister(ctx, cluster);
        walk::walk(tree, u.node, &u.path, &lister, threads, &u.walk, cancel);
        return;
    }
    let key = u.volume();
    let partial = u.rescan && !u.whole_drive;
    // A folder rescan: links counted outside the folder stay counted there
    // (seeds); links counted inside it are recounted (released).
    let (released, seeds): (Vec<u64>, Vec<(u64, u64)>) = if partial {
        let inside = subtree_dir_hashes(&tree.read(), u.node, &u.path);
        match ctx.links.lock().get(&key) {
            Some(map) => {
                let (inn, out): (Vec<_>, Vec<_>) = map
                    .iter()
                    .map(|(&id, &dir)| (id, dir))
                    .partition(|(_, dir)| inside.contains(dir));
                (inn.into_iter().map(|(id, _)| id).collect(), out)
            }
            None => (Vec::new(), Vec::new()),
        }
    } else {
        (Vec::new(), Vec::new())
    };
    if u.rescan {
        tree.write().clear_children(u.node);
    }
    let filter = HardLinkFilter::with_counted(platform_lister(ctx, cluster), seeds);
    walk::walk(tree, u.node, &u.path, &filter, threads, &u.walk, cancel);
    let released: HashSet<u64> = released.into_iter().collect();
    let found = filter.into_links(|id| released.contains(&id));
    let mut links = ctx.links.lock();
    let map = links.entry(key).or_default();
    if partial {
        map.retain(|id, _| !released.contains(id));
        map.extend(found);
    } else {
        *map = found;
    }
}

/// [`dir_hash`] of `node` (at file-system path `path`) and of every
/// directory below it, spelled as the walker spells them.
fn subtree_dir_hashes(t: &Tree, node: NodeId, path: &Path) -> HashSet<u64> {
    let mut out = HashSet::new();
    let mut stack = vec![(node, path.to_path_buf())];
    while let Some((id, p)) = stack.pop() {
        out.insert(dir_hash(&p));
        for c in t.children(id) {
            let n = t.node(c);
            if n.is_dir() && !n.is_virtual() {
                stack.push((c, p.join(t.name(c))));
            }
        }
    }
    out
}

/// MFT path of a unit: read, build a private tree, install it.
fn scan_mft_into(
    tree: &RwLock<Tree>,
    ctx: &Context,
    u: &Unit,
    letter: char,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let opts = MftOptions {
        threads: ctx.opts.threads,
        ..MftOptions::default()
    };
    let scan = {
        // The volume handle is closed before the (long) tree build.
        let vol = mft::VolumeReader::open(letter)?;
        ringscan_ntfs::scan_mft(&vol, &opts, &u.mft, cancel)?
    };
    u.set_phase(Phase::BuildingTree);
    let root_name = tree.read().name(u.node).to_string();
    let t0 = Instant::now();
    let mft::MftBuild {
        tree: private,
        stats: build,
        links,
    } = mft::build_tree(&scan.files, &root_name);
    let summary = MftSummary {
        records: scan.stats.records,
        unreadable_records: scan.stats.unreadable_records,
        read_time: scan.stats.total_time,
        build_time: t0.elapsed(),
        build,
    };
    drop(scan); // ~150 bytes per file; free it before the graft
    // Where each multi-linked file is counted, for later folder rescans.
    let mut dirs: HashMap<NodeId, u64> = HashMap::new();
    let map: LinkMap = links
        .into_iter()
        .map(|(id, dir)| {
            let h = *dirs
                .entry(dir)
                .or_insert_with(|| dir_hash(&fs_path_in(&private, dir, &u.path)));
            (id, h)
        })
        .collect();
    ctx.links.lock().insert(u.volume(), map);
    let root = private.root();
    u.walk.files.store(private.files(root), Ordering::Relaxed);
    u.walk.dirs.store(private.dirs(root), Ordering::Relaxed);
    u.walk
        .bytes
        .store(private.size(root, SizeMode::Allocated), Ordering::Relaxed);
    u.walk
        .errors
        .store(summary.unreadable_records, Ordering::Relaxed);
    *u.mft_summary.lock() = Some(summary);
    install(tree, u.node, private);
    Ok(())
}

/// Puts a privately built tree in place of `node`'s contents.
fn install(tree: &RwLock<Tree>, node: NodeId, mut private: Tree) {
    let mut t = tree.write();
    if node == t.root() && t.len() == 1 && !t.node(node).is_virtual() {
        // Fresh single-drive scan: the shared tree is just its root, so a
        // swap keeps every id the UI can hold (0) valid. Keep the
        // generation increasing so "generation changed" stays reliable.
        let old = t.generation();
        while private.generation() <= old {
            private.set_flags(private.root(), 0, 0);
        }
        *t = private;
    } else {
        t.clear_children(node);
        t.graft(node, &private);
        drop(t);
        drop(private); // outside the lock
    }
}

/// Path of `id` in `t` below the file-system path `base` of its root.
fn fs_path_in(t: &Tree, id: NodeId, base: &Path) -> PathBuf {
    let mut chain: Vec<NodeId> = std::iter::once(id).chain(t.ancestors(id)).collect();
    chain.pop(); // the root
    let mut p = base.to_path_buf();
    for &c in chain.iter().rev() {
        p.push(t.name(c));
    }
    p
}

fn make_lister(ctx: &Context, cluster: u32) -> Box<dyn DirLister> {
    let base = platform_lister(ctx, cluster);
    if ctx.opts.dedupe_hardlinks {
        Box::new(HardLinkFilter::new(base))
    } else {
        Box::new(base)
    }
}

#[cfg(windows)]
fn platform_lister(ctx: &Context, cluster: u32) -> crate::lister::WinLister {
    crate::lister::WinLister::new(cluster, Arc::clone(&ctx.lossy))
}

#[cfg(not(windows))]
fn platform_lister(_ctx: &Context, _cluster: u32) -> ringscan_core::StdLister {
    ringscan_core::StdLister
}

/// Re-queries the drive and applies its extras.
fn refresh_extras(tree: &RwLock<Tree>, ctx: &Context, slot: &DriveSlot) {
    let info = drive::drive_info(slot.info.letter).unwrap_or_else(|| slot.info.clone());
    drive::apply_drive_extras(&mut tree.write(), slot.node, info.total, info.free);
    ctx.update_slot_info(slot.node, info);
}

/// Directories sorted per write lock by [`sort_subtree`] (~5 ms).
const SORT_CHUNK: usize = 8192;

/// Sorts the children of `node` and of every directory below it by
/// allocated size, [`SORT_CHUNK`] directories per write lock (see the
/// module docs).
fn sort_subtree(tree: &RwLock<Tree>, node: NodeId) {
    let dirs: Vec<NodeId> = {
        let t = tree.read();
        if !t.is_live(node) {
            return;
        }
        std::iter::once(node)
            .chain(t.descendants(node).filter(|&d| t.node(d).is_dir()))
            .collect()
    };
    for chunk in dirs.chunks(SORT_CHUNK) {
        let mut t = tree.write();
        for &d in chunk {
            // A delete may have removed it since the list was made.
            if t.is_live(d) {
                t.sort_children(d, SizeMode::Allocated);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake Recycle Bin: per-user folders "A" (readable, new contents),
    /// "B" (unreadable) and "C" (new).
    struct FakeBin;

    impl DirLister for FakeBin {
        fn list(&self, dir: &Path, out: &mut Vec<ringscan_core::RawEntry>) -> io::Result<()> {
            let d = |name: &str| ringscan_core::RawEntry {
                name: name.into(),
                is_dir: true,
                ..Default::default()
            };
            let f = |name: &str, size: u64| ringscan_core::RawEntry {
                name: name.into(),
                logical: size,
                alloc: size,
                ..Default::default()
            };
            match dir.file_name().and_then(|n| n.to_str()) {
                Some("bin") => out.extend([d("A"), d("B"), d("C"), f("desktop.ini", 1)]),
                Some("A") => out.extend([f("$Rnew.txt", 500), d("$Rdir")]),
                Some("$Rdir") => out.push(f("inner", 70)),
                Some("B") => return Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                Some("C") => out.push(f("$Rc.bin", 9)),
                _ => {}
            }
            Ok(())
        }
    }

    #[test]
    fn recycle_bin_refresh_replaces_readable_user_folders_only() {
        let mut t = Tree::new("X:\\", 0);
        let bin = t.add_dir(0, "bin", flags::RECYCLE, 0);
        let a = t.add_dir(bin, "A", 0, 0);
        t.add_file(a, "$Rold.txt", 1000, 1000, 0, 0);
        let b = t.add_dir(bin, "B", 0, 0);
        t.add_file(b, "$Rother", 4000, 4000, 0, 0);
        t.add_file(bin, "desktop.ini", 1, 1, 0, 0);
        let tree = RwLock::new(t);
        refresh_bin_in(&tree, bin, Path::new("X").join("bin").as_path(), &FakeBin);
        let t = tree.read();
        assert!(t.is_live(a), "folder kept its id");
        let names = |id| {
            let mut v: Vec<String> = t.children(id).map(|c| t.name(c).to_string()).collect();
            v.sort();
            v
        };
        assert_eq!(names(a), ["$Rdir", "$Rnew.txt"]);
        assert_eq!(t.size(a, SizeMode::Logical), 570);
        assert_eq!(names(b), ["$Rother"], "unreadable folder keeps its data");
        let c = t.find_child(bin, "C").expect("new user folder added");
        assert_eq!(t.size(c, SizeMode::Logical), 9);
        assert_eq!(t.size(bin, SizeMode::Logical), 570 + 4000 + 9 + 1);
        assert_eq!(t.size(0, SizeMode::Logical), 570 + 4000 + 9 + 1);
    }

    #[test]
    fn job_handle_is_shareable() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ScanJob>();
        assert_send_sync::<ScanProgress>();
    }

    #[test]
    fn target_parsing() {
        assert_eq!(Target::parse("all"), Target::AllFixedDrives);
        assert_eq!(Target::parse("ALL"), Target::AllFixedDrives);
        assert_eq!(Target::parse("c"), Target::Drive('C'));
        assert_eq!(Target::parse("d:"), Target::Drive('D'));
        assert_eq!(Target::parse(r"E:\"), Target::Drive('E'));
        assert_eq!(Target::parse(r"\\?\C:\"), Target::Drive('C'));
        assert_eq!(
            Target::parse(r"C:\Windows"),
            Target::Path(PathBuf::from(r"C:\Windows"))
        );
        assert_eq!(Target::Drive('C').to_string(), r"C:\");
        assert_eq!(Method::parse("MFT"), Some(Method::Mft));
        assert_eq!(Method::parse("x"), None);
    }

    #[test]
    fn path_helpers() {
        assert_eq!(path_drive_letter(r"c:\x"), Some('C'));
        assert_eq!(path_drive_letter(r"\\?\D:\x"), Some('D'));
        assert_eq!(path_drive_letter(r"\\srv\share"), None);
        assert_eq!(path_drive_letter("/tmp"), None);
        #[cfg(unix)]
        {
            assert_eq!(normalize_path(Path::new("/tmp/x/")).0, "/tmp/x");
            assert_eq!(normalize_path(Path::new("/")).0, "/");
        }
        #[cfg(windows)]
        {
            assert_eq!(normalize_path(Path::new(r"C:\Windows\")).0, r"C:\Windows");
            assert_eq!(
                normalize_path(Path::new(r"\\?\C:\Windows")).0,
                r"C:\Windows"
            );
            assert_eq!(
                normalize_path(Path::new("c:/windows/system32")).0,
                r"c:\windows\system32"
            );
        }
    }

    #[test]
    fn node_paths_skip_virtual_root_and_reject_virtual_nodes() {
        let mut t = Tree::new(THIS_PC_NAME, flags::VIRTUAL);
        let c = t.add_dir(0, "C:\\", 0, 0);
        let w = t.add_dir(c, "Windows", 0, 0);
        let f = t.add_virtual(c, drive::FREE_NAME, flags::FREE, 5);
        let o = t.add_virtual(c, mft::ORPHANED_NAME, flags::DIR, 0);
        let lost = t.add_file(o, "lost", 1, 1, 0, 0);
        let ctx = Context {
            target: Target::AllFixedDrives,
            opts: ScanOptions::default(),
            root_path: None,
            drives: Mutex::new(Vec::new()),
            lossy: Arc::new(LossyPaths::new()),
            links: Mutex::new(HashMap::new()),
        };
        assert_eq!(ctx.node_path(&t, 0), None);
        assert_eq!(ctx.node_path(&t, c), Some(PathBuf::from("C:\\")));
        assert_eq!(
            ctx.node_path(&t, w),
            Some(PathBuf::from("C:\\").join("Windows"))
        );
        assert_eq!(ctx.node_path(&t, f), None);
        assert_eq!(ctx.node_path(&t, lost), None);
    }
}
