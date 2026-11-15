//! # ringscan-engine
//!
//! Glue between the platform-independent core (`ringscan-core`), the MFT
//! reader (`ringscan-ntfs`) and the Windows primitives (`ringscan-win`), plus
//! the headless CLI. The GUI drives everything through [`ScanJob`]:
//!
//! ```no_run
//! use ringscan_engine::{ScanJob, ScanOptions, Target};
//! let job = ScanJob::start(Target::Drive('C'), ScanOptions::default());
//! while !job.is_done() {
//!     let p = job.progress(); // files, bytes, phase, method, MFT records ...
//!     let tree = job.tree().read(); // lay out the partial tree (short lock)
//!     # drop((p, tree));
//!     # break;
//! }
//! ```
//!
//! ## Modules
//!
//! * [`job`] — [`ScanJob`]: start / progress / cancel / wait, method choice
//!   (MFT for a whole NTFS drive when the raw volume is readable, else walk,
//!   with the reason recorded),
//!   "This PC" (drives on different disks in parallel), rescan, delete,
//!   Recycle Bin.
//! * [`lister`] — [`WinLister`] (core's `DirLister` over
//!   `ringscan_win::list_dir`: attributes → flags, cloud placeholders,
//!   links not followed, `$Recycle.Bin`) and [`HardLinkFilter`] (walk mode
//!   counts each hard-linked file once).
//! * [`mft`] — [`VolumeReader`] (`ReadAt` over a raw volume) and
//!   [`build_tree`] (MFT listing → tree: hard links once, metafiles,
//!   `<orphaned>`). Platform independent; cross-validated on Linux against
//!   ntfs-3g mounts of the NTFS test images (`tests/mft_crosscheck.rs`).
//! * [`drive`] — `<free space>` / `<unaccounted>` at drive roots, which
//!   fixed drives "all" means, disk grouping.
//! * [`cli`] — [`run_cli`] for `ringscan --cli` and the `ringscan-cli` binary.
//! * [`sys`] — peak memory of the process.
//!
//! ## Deviations from SPEC.md
//!
//! * `ScanJob` is an opaque handle rather than a struct with public fields:
//!   `tree()`, `progress()` (a snapshot struct, [`ScanProgress`], with
//!   per-drive [`UnitProgress`]), `cancel()`, `is_done()`, `wait()`; the
//!   method actually used is reported per unit (MFT and walk can mix in a
//!   "This PC" scan).
//! * REPARSE is set only for name-surrogate reparse points (junctions,
//!   symlinks, mount points) and for reparse points with an unknown tag;
//!   other reparse points (WOF-compressed, dedup, cloud, app-exec aliases)
//!   are regular entries, so they are descended / counted.
//! * `Method::Auto` also *tries* the MFT without elevation (SPEC: only when
//!   elevated). A refused raw open costs well under a millisecond and the
//!   drive is walked as specified; but volumes that grant raw read access to
//!   users are then scanned from the MFT (on the development machine the
//!   1 TB data drive: 1.3 s and 4 MB unaccounted, versus a 6.3 s walk that
//!   could not see 2.4 GB).
//! * The Recycle Bin is the real `$Recycle.Bin` folder (flag RECYCLE), not a
//!   virtual node.
//! * Walk mode also counts hard links once (by file id), not only MFT mode.
//! * The CLI also accepts `--method mft|walk|auto` (plus `--mft` / `--walk`),
//!   `--logical`, `--all-links`, `--max-children` and `--progress`.
//! * Additions: [`ScanJob::rescan`], [`ScanJob::delete`],
//!   [`ScanJob::empty_recycle_bin`], [`ScanJob::drives`],
//!   [`ScanJob::node_path`], [`drive::apply_drive_extras`],
//!   [`lister::LossyPaths`] (directories whose names are not valid Unicode
//!   stay reachable).

pub mod cli;
pub mod drive;
pub mod job;
pub mod lister;
pub mod mft;
pub mod sys;

pub use cli::run_cli;
pub use drive::{DriveSpace, FREE_NAME, RESERVED_NAME, UNACCOUNTED_NAME};
pub use job::{
    DEFAULT_WALK_THREADS, DeleteMode, DriveSummary, ItemChanged, Method, MftSummary, Phase,
    ScanJob, ScanOptions, ScanProgress, THIS_PC_NAME, Target, UnitNote, UnitProgress,
    is_item_changed,
};
pub use lister::{HardLinkFilter, WinLister};
pub use mft::{MftBuild, MftBuildStats, ORPHANED_NAME, VolumeReader, build_tree};
