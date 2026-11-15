//! Scanning seam: how the UI gets a [`Tree`] that fills up while it watches.
//!
//! Real scans are `ringscan_engine::ScanJob`s: [`Scan::path`] (a folder or
//! a drive), [`Scan::this_pc`] (every fixed drive under a virtual "This PC"
//! root, physical disks in parallel) and [`Scan::drive`]. The engine picks
//! the method per drive (MFT for a whole NTFS drive when the raw volume can
//! be read, usually only when elevated; otherwise the parallel directory
//! walk), counts hard links once, and adds `<free space>` /
//! `<unaccounted>` to scanned drives. Demo mode builds a deterministic
//! synthetic tree instead and never touches the disk.
//!
//! Design decisions:
//!
//! * **The job owns the tree.** [`Scan::tree`] is the job's
//!   `Arc<RwLock<Tree>>`; the UI only takes short `read_recursive` locks.
//!   The main job is kept in an `Arc` so a worker thread can run a blocking
//!   [`ScanJob::delete`] / `empty_recycle_bin` on it while the UI keeps
//!   drawing. Dropping a [`Scan`] (a new scan replaces it) cancels its jobs.
//! * **One rescan at a time.** [`Scan::rescan`] starts an engine rescan
//!   (clear + re-walk in place, or a whole drive again) as a second job on
//!   the same tree and refuses while anything is running.
//! * **Status.** While a job runs, [`Scan::status`] reports that job's
//!   progress (counters are atomics, no tree lock). Once everything is idle
//!   it reports the totals of the tree itself (files, folders, scanned bytes
//!   without `<unaccounted>`, folders with errors), cached per tree
//!   generation, so the status bar stays right after rescans and deletes.
//! * **Method.** [`Method`] summarizes the per-drive methods for the status
//!   bar; [`ScanStatus::units`] keeps each drive's method and the engine's
//!   note on why the MFT was not used.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ringscan_core::parking_lot::RwLock;
use ringscan_core::{NodeId, SizeMode, Tree, flags, synth};
use ringscan_engine::{self as engine, Phase, ScanJob, ScanOptions, ScanProgress, Target};

/// How the data of a scan is obtained (for the status bar).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// The engine has not decided yet (enumerating drives).
    Pending,
    /// Parallel directory walk with this many threads per drive.
    Walk { threads: usize },
    /// Read from the NTFS master file table.
    Mft,
    /// MFT for some drives, directory walk for the others.
    Mixed { threads: usize },
    /// Synthetic demo tree.
    Demo,
}

/// Method and engine note of one drive (or the folder) of the main scan.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UnitStatus {
    /// `C:\` or the folder path.
    pub name: String,
    pub method: Option<engine::Method>,
    /// Why the MFT was not used, if it was considered.
    pub note: Option<String>,
}

/// Progress of the running job, or the totals of the finished tree.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScanStatus {
    pub files: u64,
    pub dirs: u64,
    /// Allocated bytes found (without `<unaccounted>`).
    pub bytes: u64,
    /// Folders that could not be (fully) listed.
    pub errors: u64,
    /// A folder being listed right now (empty when idle).
    pub current: String,
    /// A job is still running.
    pub running: bool,
    /// The user stopped the scan.
    pub cancelled: bool,
    /// Duration of the running (or the last finished) job.
    pub elapsed: Duration,
    /// `(records read, total)` while an MFT is being read.
    pub mft: Option<(u64, u64)>,
    /// Drives (or the folder) of the main scan.
    pub units: Vec<UnitStatus>,
}

/// Why [`Scan::rescan`] did not start.
#[derive(Debug)]
pub enum RescanError {
    /// A scan or rescan is still running.
    Busy,
    /// Synthetic data cannot be rescanned.
    Demo,
    /// The engine refused (virtual node, vanished item, ...).
    Failed(io::Error),
}

/// One scan session: the shared tree plus the jobs filling it.
pub struct Scan {
    pub tree: Arc<RwLock<Tree>>,
    pub method: Method,
    job: Option<Arc<ScanJob>>,
    rescan: Option<ScanJob>,
    /// Status of a synthetic tree (no job).
    fixed: ScanStatus,
    /// Idle totals and the tree generation they were computed for.
    idle: Option<(u64, ScanStatus)>,
}

/// The engine target for a path argument (Explorer's mangled `C:"` is
/// repaired; drive roots become whole-drive targets).
pub fn target_for(path: &Path) -> Target {
    let p = ringscan_win::context_menu::path_from_arg(path.as_os_str());
    match p.to_str() {
        Some(s) => Target::parse(s),
        None => Target::Path(p).normalized(),
    }
}

/// Command-line arguments that start a scan of `target` again (for
/// "Restart as administrator").
pub fn target_args(target: &Target) -> Vec<String> {
    match target {
        Target::AllFixedDrives => Vec::new(),
        Target::Drive(c) => vec![format!("{c}:\\")],
        Target::Path(p) => vec![p.to_string_lossy().into_owned()],
    }
}

impl Scan {
    fn with_job(job: ScanJob) -> Scan {
        Scan {
            tree: Arc::clone(job.tree()),
            method: Method::Pending,
            job: Some(Arc::new(job)),
            rescan: None,
            fixed: ScanStatus::default(),
            idle: None,
        }
    }

    fn with_tree(tree: Tree) -> Scan {
        let root = tree.root();
        let fixed = ScanStatus {
            files: tree.files(root),
            dirs: tree.dirs(root),
            bytes: tree.size(root, SizeMode::Allocated),
            ..ScanStatus::default()
        };
        Scan {
            tree: Arc::new(RwLock::new(tree)),
            method: Method::Demo,
            job: None,
            rescan: None,
            fixed,
            idle: None,
        }
    }

    /// Starts an engine scan of `target` (returns at once).
    pub fn start(target: Target) -> Scan {
        Scan::with_job(ScanJob::start(target, ScanOptions::default()))
    }

    /// Scans a folder or a drive; the tree root is the folder itself.
    pub fn path(path: &Path) -> Scan {
        Scan::start(target_for(path))
    }

    /// Scans one whole drive.
    pub fn drive(letter: char) -> Scan {
        Scan::start(Target::Drive(letter))
    }

    /// Scans every fixed drive under a virtual "This PC" root.
    pub fn this_pc() -> Scan {
        Scan::start(Target::AllFixedDrives)
    }

    /// Deterministic synthetic "This PC" with `nodes` nodes (no disk access).
    pub fn demo(seed: u64, nodes: usize) -> Scan {
        Scan::with_tree(synth::this_pc(seed, nodes))
    }

    /// A balanced synthetic tree for performance tests: every folder down
    /// to depth `levels` has `fanout` subfolders and two files of similar
    /// size, so tens of thousands of arcs are visible at once.
    pub fn demo_uniform(levels: u32, fanout: u32) -> Scan {
        let mut rng = synth::Rng::new(11);
        let mut tree = Tree::new("C:\\", 0);
        let mut frontier = vec![(0 as NodeId, 0u32)];
        let mut batch = Vec::new();
        while let Some((dir, depth)) = frontier.pop() {
            batch.clear();
            if depth < levels {
                for i in 0..fanout {
                    batch.push(ringscan_core::RawEntry {
                        name: format!("{}-{i}", synth::dir_name(&mut rng)),
                        is_dir: true,
                        mtime: 1_767_225_600 - rng.below(3_000 * 86_400) as i64,
                        ..Default::default()
                    });
                }
            }
            for i in 0..2 {
                let size = rng.log_normal((40e6f64).ln(), 0.5) as u64;
                batch.push(ringscan_core::RawEntry {
                    name: format!("{}-{i}", synth::file_name(&mut rng)),
                    logical: size,
                    alloc: size.div_ceil(4096) * 4096,
                    mtime: 1_767_225_600 - rng.below(3_000 * 86_400) as i64,
                    ..Default::default()
                });
            }
            for (id, _) in tree.add_batch(dir, &batch) {
                frontier.push((id, depth + 1));
            }
        }
        Scan::with_tree(tree)
    }

    /// Synthetic data: no job, paths must never reach the shell.
    pub fn is_demo(&self) -> bool {
        self.job.is_none()
    }

    /// The engine job of a real scan (for deletes, the Recycle Bin and
    /// drive summaries).
    pub fn job(&self) -> Option<&Arc<ScanJob>> {
        self.job.as_ref()
    }

    /// What this scan was started for (None for demo data).
    pub fn target(&self) -> Option<&Target> {
        self.job.as_ref().map(|j| j.target())
    }

    /// Real file-system path of a node of this scan's tree, already locked
    /// by the caller as `t` (None for virtual nodes, nodes inside
    /// `<orphaned>` and demo data).
    pub fn node_path(&self, t: &Tree, id: NodeId) -> Option<PathBuf> {
        self.job.as_ref()?.node_path_in(t, id)
    }

    /// Rescans `at` in place: a folder is re-walked, a drive (or a node in
    /// a virtual container) is scanned again, "This PC" rescans every
    /// drive, a file rescans its folder.
    pub fn rescan(&mut self, at: NodeId) -> Result<(), RescanError> {
        let Some(job) = &self.job else {
            return Err(RescanError::Demo);
        };
        if self.is_running() {
            return Err(RescanError::Busy);
        }
        let r = job.rescan(at).map_err(RescanError::Failed)?;
        self.rescan = Some(r);
        self.idle = None;
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| !j.is_done())
            || self.rescan.as_ref().is_some_and(|r| !r.is_done())
    }

    /// Asks every job to stop (returns at once; walkers stop within ms).
    pub fn cancel(&mut self) {
        if let Some(j) = &self.job {
            j.cancel();
        }
        if let Some(r) = &self.rescan {
            r.cancel();
        }
    }

    /// Progress of the running job, or the tree's totals when idle (see
    /// the module docs).
    pub fn status(&mut self) -> ScanStatus {
        let Some(job) = self.job.clone() else {
            return self.fixed.clone();
        };
        let main = job.progress();
        self.method = method_of(&main, job.options());
        let units = main
            .units
            .iter()
            .map(|u| UnitStatus {
                name: u.name.clone(),
                method: u.method,
                note: u.note.clone(),
            })
            .collect();
        let rescan = self.rescan.as_ref().map(|r| (r.progress(), r.is_done()));
        let running = match &rescan {
            Some((p, false)) => Some(p),
            _ if !job.is_done() => Some(&main),
            _ => None,
        };
        if let Some(p) = running {
            let mut st = from_progress(p);
            st.units = units;
            return st;
        }
        let generation = self.tree.read_recursive().generation();
        if let Some((g, st)) = &self.idle
            && *g == generation
        {
            return st.clone();
        }
        let main_cancelled = main.cancelled;
        let last = rescan.map_or(main, |(p, _)| p);
        let mut st = tree_totals(&self.tree.read_recursive());
        st.elapsed = last.elapsed;
        st.cancelled = last.cancelled || main_cancelled;
        st.units = units;
        self.idle = Some((generation, st.clone()));
        st
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        // A worker thread may still hold the main job (a delete in
        // progress); cancel explicitly instead of relying on the last drop.
        self.cancel();
    }
}

fn from_progress(p: &ScanProgress) -> ScanStatus {
    let mft = (p.phase == Phase::ReadingMft && p.mft_records_total > 0)
        .then_some((p.mft_records_done, p.mft_records_total));
    ScanStatus {
        files: p.files,
        dirs: p.dirs,
        bytes: p.bytes,
        errors: p.errors,
        current: p.current.clone(),
        running: true,
        cancelled: p.cancelled,
        elapsed: p.elapsed,
        mft,
        units: Vec::new(),
    }
}

/// [`Method`] of a job from the methods its units used.
fn method_of(p: &ScanProgress, opts: &ScanOptions) -> Method {
    let threads = opts.walk_threads();
    let used = |m| p.units.iter().any(|u| u.method == Some(m));
    match (used(engine::Method::Mft), used(engine::Method::Walk)) {
        (true, true) => Method::Mixed { threads },
        (true, false) => Method::Mft,
        (false, true) => Method::Walk { threads },
        (false, false) => Method::Pending,
    }
}

/// Totals of a finished tree: files and folders below the root, scanned
/// bytes (the root's size minus the drives' `<unaccounted>` nodes) and
/// folders flagged with errors.
fn tree_totals(t: &Tree) -> ScanStatus {
    let root = t.root();
    let drives: Vec<NodeId> = if t.node(root).is_virtual() {
        t.children(root).collect()
    } else {
        vec![root]
    };
    let unaccounted: u64 = drives
        .into_iter()
        .filter_map(|d| engine::drive::extras_of(t, d).1)
        .map(|u| t.node(u).alloc())
        .sum();
    let errors = t.live_nodes().filter(|(_, n)| n.has(flags::ERROR)).count() as u64;
    ScanStatus {
        files: t.files(root),
        dirs: t.dirs(root),
        bytes: t
            .size(root, SizeMode::Allocated)
            .saturating_sub(unaccounted),
        errors,
        ..ScanStatus::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_and_restart_arguments() {
        assert_eq!(target_for(Path::new("C:\"")), Target::Drive('C'));
        assert_eq!(target_for(Path::new("d:")), Target::Drive('D'));
        assert_eq!(target_for(Path::new("all")), Target::AllFixedDrives);
        assert_eq!(target_args(&Target::AllFixedDrives), Vec::<String>::new());
        assert_eq!(target_args(&Target::Drive('C')), vec!["C:\\".to_string()]);
        assert_eq!(
            target_args(&Target::Path(PathBuf::from("/tmp/a b"))),
            vec!["/tmp/a b".to_string()]
        );
    }

    #[test]
    fn idle_totals_leave_out_unaccounted_space() {
        let mut t = Tree::new(engine::THIS_PC_NAME, flags::VIRTUAL);
        let c = t.add_dir(0, "C:\\", 0, 0);
        let d = t.add_dir(c, "dir", flags::ERROR, 0);
        t.add_file(d, "a.bin", 100, 4096, 0, 0);
        engine::drive::apply_drive_extras(&mut t, c, 100_000, 50_000);
        let st = tree_totals(&t);
        assert_eq!((st.files, st.dirs, st.errors), (1, 2, 1)); // dirs: C:\ and dir
        assert_eq!(st.bytes, 4096);
        // The drive itself spans the used space (scanned + unaccounted).
        assert_eq!(t.size(c, SizeMode::Allocated), 50_000);
    }

    /// A real scan of a small temporary folder through the engine (std
    /// lister on Linux, Win32 lister on Windows).
    #[test]
    fn scans_and_rescans_a_folder() {
        let dir = std::env::temp_dir()
            .join("ringscan-dev")
            .join(format!("app-scan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), vec![1u8; 10_000]).unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), vec![2u8; 20_000]).unwrap();
        let mut scan = Scan::path(&dir);
        while scan.is_running() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let st = scan.status();
        assert!(!st.running);
        assert_eq!((st.files, st.dirs), (2, 1));
        assert!(
            matches!(scan.method, Method::Walk { .. }),
            "{:?}",
            scan.method
        );
        let sub = scan.tree.read().find_child(0, "sub").unwrap();
        std::fs::write(dir.join("sub").join("c.txt"), vec![3u8; 5_000]).unwrap();
        scan.rescan(sub).unwrap();
        assert!(matches!(scan.rescan(sub), Err(RescanError::Busy) | Ok(())));
        while scan.is_running() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let st = scan.status();
        assert_eq!((st.files, st.dirs), (3, 1));
        let path = {
            let t = scan.tree.read();
            let c = t.find_child(sub, "c.txt").unwrap();
            scan.node_path(&t, c).unwrap()
        };
        assert_eq!(path, dir.join("sub").join("c.txt"));
        drop(scan);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
