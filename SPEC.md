# RingScan — spec

> **Status: the original design document**, written before the code and kept
> for reference. The crate READMEs and module docs describe the program as
> built. The main differences:
>
> * Crates: the MFT reader is its own crate (`crates/ntfs`, not `core/ntfs`);
>   `scan-win` became `crates/win` (Windows primitives) plus
>   `crates/engine` (scan jobs, MFT → tree, CLI). The GUI renders with wgpu
>   on Direct3D 12, not glow.
> * Method choice: the MFT is read whenever the raw volume of a whole NTFS
>   drive can be opened (usually only when elevated, but some drives allow
>   it without), else the drive is walked; the reason is reported per drive
>   (`UnitNote`) and localized in the UI.
> * Hard links are counted once in walk mode too (by file id), not only in
>   MFT mode. The Recycle Bin is the real `$Recycle.Bin` folder (flag
>   RECYCLE), not a virtual node.
> * Drive roots get a third virtual node, `<reserved by Windows>` (the
>   storage reserve and NTFS reservations, from `GetDiskSpaceInformationW`),
>   so `<unaccounted>` holds only what the scan could not see;
>   `DriveInfo` has a `reserved` field for it. Free space is hidden by
>   default (a setting) and written in the center of the chart instead.
> * The reader API is `scan_mft` with options and statistics
>   (`read_mft` remains as the simple form); `MftFile` carries more fields
>   (orphan flag, reparse tag, EA flag, ...).
> * Additions: `ScanJob::rescan` / `delete` / `empty_recycle_bin` /
>   `drives`, verbatim paths for every file-system call on scanned items,
>   stale selections refused (`ItemChanged`), scans kept in memory when
>   switching drives, a scripted GUI test harness, the "Support RingScan"
>   dialog, and Ukrainian counts grouped with U+00A0.

A modern take on Steffen Gerlach's *Scanner* 2.13 (2012): a disk usage
visualizer for Windows built around a sunburst chart (concentric rings, outer
ring = deeper directory level, arc angle proportional to size).

Goals: single portable `.exe`, no installer, no runtime dependencies, fast
enough to feel instant on a 2 TB NVMe drive, Ukrainian + English UI.

## Feature set

Parity with the original Scanner:
- Sunburst of the whole drive or a folder, all levels at once; tiny items hidden.
- Hover tooltip (name, size, % of parent / of focus, file count).
- Left click = zoom into folder; Back; Parent; Rescan focused folder.
- Right-click context menu: Zoom, Rescan, Open, Show in Explorer, Copy path,
  Move to Recycle Bin, Delete permanently (confirmed), Properties.
- Drive list with used/free; summary of all fixed drives.
- Recycle Bin segment + "Empty Recycle Bin"; shortcut to "Installed apps".
- Start zoomed to a path given on the command line; optional Explorer context
  menu entry for folders and drives (HKCU, no admin needed).
- Allocated ("used space") vs logical size; compressed and sparse files counted
  correctly; reparse points (junctions/symlinks) not followed.

Modern additions:
- NTFS fast path: read the MFT directly when elevated (WizTree-class speed);
  otherwise a parallel directory walker. Chart updates live while scanning.
- Smooth GPU-rendered chart (eframe/egui, wgpu), animated zoom transitions,
  per-monitor DPI awareness, dark/light theme following Windows.
- Side panel: children of the focused folder as a sortable list with bars;
  "Largest files" across the focus; "File types" breakdown by extension.
- Color modes: by folder (hue by angle, like the original), by file type,
  by age (last modified).
- Search/filter by name (highlights matching arcs).
- "Unaccounted" segment at drive root = used space reported by Windows minus
  scanned total (system/inaccessible data), plus free space segment.
- Hardlinks counted once (MFT mode); OneDrive/cloud placeholders count by
  allocated size (0 if not local).
- "Run as administrator" button when not elevated (restarts self via `runas`).
- Headless CLI for scripting and testing: `ringscan.exe --cli <path> [--json]`.

## Architecture

Cargo workspace, cross-compiled from WSL to `x86_64-pc-windows-gnu` (mingw).

```
crates/
  core/   ringscan-core — platform-independent, unit-tested on Linux
    tree.rs     arena tree (Vec<Node>, u32 indices), names in one buffer,
                aggregation, sort children by size, path reconstruction
    layout.rs   sunburst geometry: (tree, focus, max_depth, min_angle) -> arcs,
                "N small items" pseudo-segments, hit testing (angle, radius)
    ntfs/       MFT parser over a `ReadAt` trait (boot sector, $MFT runlist,
                record fixups, $STANDARD_INFORMATION/$FILE_NAME/$DATA,
                extension records merged via base reference, sizes only from
                lowest_vcn == 0 segment); tested on mkntfs images
    stats.rs    extension breakdown, largest files
    format.rs   human sizes, counts (uk/en)
  scan-win/ Windows-only: parallel walker (GetFileInformationByHandleEx
            FileIdBothDirectoryInfo -> AllocationSize/EndOfFile without
            per-file opens), volume enumeration, \\.\X: reader for MFT,
            elevation check, shell actions (recycle, open, properties),
            registry for context menu
  app/      ringscan.exe — eframe (glow) UI, i18n, settings, manifest + icon
```

Scanning threads publish batches into the tree builder; UI re-lays out at
~10 Hz while a scan is running and on demand otherwise.

## Verification

- `cargo test -p ringscan-core` on Linux (tree, layout, NTFS parser against
  images created with mkntfs + ntfs-3g: nested dirs, hardlinks, sparse,
  compressed, ADS, fragmented files, many files to force MFT extension).
- Cross-build, run `--cli` on real Windows folders, compare totals with
  PowerShell; screenshot the GUI via PowerShell and inspect.
- MFT mode on a real drive needs an elevated run by the user.

## Crate contracts (cross-crate API)

Crate ownership is strict: each crate is written by one agent; others consume
its *actual* public API after it is done. The signatures below are the agreed
shape — producers may refine names/details but must keep the semantics and
document deviations in the crate's module docs.

Dependency graph (no cycles):
`core` (no deps on others) · `ntfs` (none) · `win` (none) →
`engine` (core + ntfs + win) → `app` (core + engine + win).

### ringscan-core (`crates/core`) — platform independent

```rust
pub type NodeId = u32;            pub const NONE: NodeId = u32::MAX;
pub enum SizeMode { Allocated /*default*/, Logical }
pub enum Lang { Uk, En }
pub mod flags {  // u16 bit set on Node
  DIR, REPARSE /*junction/symlink: listed, not followed*/, COMPRESSED, SPARSE,
  HIDDEN, SYSTEM, ERROR /*could not be (fully) listed*/, CLOUD /*placeholder*/,
  HARDLINK /*extra link, sizes 0*/, RECYCLE /*recycle bin dir*/, METAFILE /*$MFT…*/,
  FREE /*virtual: free space*/, UNACCOUNTED /*virtual: used − scanned*/,
  VIRTUAL /*any synthetic node incl. "This PC" root*/, DELETED /*tombstone*/
}
pub struct Node { parent, first_child, next_sibling: NodeId, flags: u16,
                  ext: u16 /*Tree::ext_name(id); 0 = none*/,
                  logical: u64, alloc: u64 /*files: own; dirs: aggregated*/,
                  files: u64, dirs: u64 /*aggregated counts under the node*/,
                  mtime: i64 /*unix seconds, 0 unknown*/, /* name: private */ }
pub struct Tree;  // arena; names in one shared buffer; ~56 B/node target
impl Tree {
  fn new(root_name: &str, root_flags: u16) -> Tree;     fn root(&self) -> NodeId;
  fn len(&self) -> usize;  fn node(&self, NodeId) -> &Node;  fn name(&self, NodeId) -> &str;
  fn ext_name(&self, u16) -> &str;  fn size(&self, NodeId, SizeMode) -> u64;
  fn children(&self, NodeId) -> impl Iterator<Item = NodeId>;
  fn path(&self, NodeId) -> String;   // "C:\Users\x"; virtual root not included
  fn find_path(&self, &str) -> Option<NodeId>;          // case-insensitive
  // Live insertion: sizes/counts of all ancestors are updated immediately, so
  // the tree is always consistent while a scan runs (UI reads it at ~10 Hz).
  fn add_dir(&mut self, parent, name, flags, mtime) -> NodeId;
  fn add_file(&mut self, parent, name, logical, alloc, flags, mtime) -> NodeId;
  fn add_batch(&mut self, parent, &[RawEntry]) -> Vec<(NodeId, usize /*index in batch*/)>; // returns dirs
  fn add_virtual(&mut self, parent, name, kind_flag, size) -> NodeId;
  // Bulk build (MFT): insert without ancestor updates, then one O(n) pass.
  fn add_dir_raw / add_file_raw(...) -> NodeId;  fn recompute_aggregates(&mut self);
  fn sort_children(&mut self, NodeId, SizeMode); fn sort_all(&mut self, SizeMode);
  fn remove(&mut self, NodeId);        // after delete/recycle: unlink + subtract up
  fn clear_children(&mut self, NodeId); // before rescan of a folder
  fn graft(&mut self, at: NodeId, other: &Tree); // copy other's root children under `at`
}
pub mod walk {
  pub struct RawEntry { name: String, is_dir: bool, logical: u64, alloc: u64,
                        flags: u16, mtime: i64, file_id: u64 /*0 unknown*/ }
  pub trait DirLister: Send + Sync {
    fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()>; }
  pub struct StdLister;  // std::fs fallback (also used by Linux tests)
  pub struct Progress { files, dirs, bytes, errors: AtomicU64,
                        current: Mutex<String>, done: AtomicBool }
  // Parallel work-stealing walk; one write-lock per listed directory.
  pub fn walk(tree: &RwLock<Tree>, at: NodeId, path: &Path, lister: &dyn DirLister,
              threads: usize, progress: &Progress, cancel: &AtomicBool);
}
pub mod layout {
  pub struct Params { mode: SizeMode, max_depth: u8, min_angle: f32 /*rad*/, show_free: bool }
  pub enum ArcKind { Node(NodeId), Rest { parent: NodeId, count: u32 } }
  pub struct Arc { kind: ArcKind, depth: u8 /*1 = innermost ring*/, a0: f32, a1: f32, size: u64 }
  pub struct Layout { focus: NodeId, total: u64, arcs: Vec<Arc>, depth: u8 }
  pub fn compute(tree: &Tree, focus: NodeId, p: &Params) -> Layout; // angles 0..TAU
  impl Layout { fn hit(&self, angle: f32, depth: u8) -> Option<&Arc>; }
}
pub mod stats { ext_breakdown(tree, under, mode) -> Vec<ExtStat{ext, files, size}>;
                largest_files(tree, under, mode, n) -> Vec<NodeId>;
                search(tree, under, query, limit) -> Vec<NodeId>; }
pub mod format { size(bytes, Lang) -> String /*"1,23 ГБ" | "1.23 GB"*/;
                 count(n, Lang) -> String; percent(f64) -> String; }
```

### ringscan-ntfs (`crates/ntfs`) — platform independent MFT reader

```rust
pub trait ReadAt: Sync { fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()>; }
impl ReadAt for std::fs::File;      // image files in tests
pub struct VolumeInfo { bytes_per_sector, bytes_per_cluster, record_size: u32,
                        mft_lcn, total_clusters, serial: u64 }
pub struct MftFile { record: u64, parent: u64, name: String, is_dir: bool,
                     logical: u64, alloc: u64, attrs: u32 /*FILE_ATTRIBUTE_* */,
                     mtime: i64 /*unix secs*/, extra_links: Vec<(u64, String)> }
pub struct MftProgress { total: AtomicU64, done: AtomicU64 }
pub fn read_volume_info(vol: &dyn ReadAt) -> io::Result<VolumeInfo>;
pub fn read_mft(vol: &dyn ReadAt, progress: &MftProgress, cancel: &AtomicBool)
    -> io::Result<(VolumeInfo, Vec<MftFile>)>;   // root dir = record 5
```
Rules: apply update-sequence fixups; skip records not in use; merge extension
records into their base record; take sizes only from the attribute segment with
lowest_vcn == 0; non-resident compressed/sparse → alloc = compressed_size;
resident $DATA → alloc 0; skip DOS-only names (namespace 2), keep one primary
name per (parent) and report other hard links in `extra_links`; named data
streams (ADS) add to logical/alloc; directory $INDEX_ALLOCATION counts as the
directory's own alloc. Raw volume handles need sector-aligned reads — the
parser must only issue reads aligned to `bytes_per_sector` (multiples of 4096
are always safe) so the Windows `ReadAt` can stay thin.

### ringscan-win (`crates/win`) — Windows primitives; compiles to stubs elsewhere

```rust
pub struct WinEntry { name: String, is_dir: bool, logical: u64, alloc: u64,
                      attrs: u32, reparse_tag: u32, mtime: i64, file_id: u64 }
pub fn list_dir(dir: &Path, out: &mut Vec<WinEntry>) -> io::Result<()>;
    // FindExInfoBasic is NOT enough (no AllocationSize): use
    // GetFileInformationByHandleEx(FileIdBothDirectoryInfo / FileFullDirectoryInfo)
    // with a large buffer; long-path (\\?\) safe; "." and ".." skipped.
pub struct Volume; impl Volume { fn open(letter: char) -> io::Result<Volume>;
                                 fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()>; }
pub enum DriveKind { Fixed, Removable, Network, CdRom, Ram, Unknown }
pub struct DriveInfo { letter: char, label: String, fs: String, kind: DriveKind,
                       total: u64, free: u64, cluster: u32, disk_number: Option<u32> }
pub fn drives() -> Vec<DriveInfo>;
pub fn is_elevated() -> bool;  pub fn relaunch_elevated(args: &[String]) -> io::Result<()>;
pub fn ui_lang_ukrainian() -> bool;
pub mod shell { open, reveal, properties, copy_to_clipboard(text),
                recycle(&[PathBuf]), delete_permanently(&[PathBuf]),
                empty_recycle_bin(Option<char>), recycle_bin_size(Option<char>) -> (u64, u64),
                open_apps_settings() }      // all -> io::Result<_>
pub mod context_menu { is_installed() -> bool, install(exe: &Path), uninstall() } // HKCU only
```

### ringscan-engine (`crates/engine`) — glue + CLI logic

Implements `DirLister` over `win::list_dir` (attrs → core flags, reparse points
listed but not followed, cloud placeholders by alloc), `ReadAt` over
`win::Volume`, MFT → `Tree` building (hard links once, metafiles under the
root, orphans under a "<orphaned>" node, virtual FREE/UNACCOUNTED/RECYCLE at
drive roots), method choice (MFT if elevated + NTFS + whole drive, else walk),
`ScanJob { tree: Arc<RwLock<Tree>>, progress, cancel, method, join }`, drive
summary ("This PC" virtual root, physical disks scanned in parallel) and the
headless CLI: `ringscan --cli <path> [--json] [--mft|--walk] [--depth N] [--top N]`.

### ringscan (`crates/app`) — the GUI exe

eframe 0.37 (default wgpu renderer, MSAA), Windows manifest + icon, `#![windows_subsystem = "windows"]`
(CLI mode re-attaches to the parent console). Built-in UI test harness:
`--script "<cmds>"` injects input through `raw_input_hook` and saves screenshots
via `ViewportCommand::Screenshot` (`wait 1.5; move 600 400; shot a.png; click
600 400; key Escape; quit`), so agents can verify UI without a human.

## Dev workflow

- Linux: `cargo test -p ringscan-core -p ringscan-ntfs`.
- Windows: `cargo build --release --target x86_64-pc-windows-gnu -p ringscan`;
  `cargo test/run --target x86_64-pc-windows-gnu -p <crate>` executes on the
  Windows host through `scripts/winrun.sh` (scratch: `%TEMP%\ringscan-dev`).
- Never delete, recycle or modify user files in tests. Destructive actions are
  tested only on files the test creates inside `%TEMP%\ringscan-dev`.
- Code comments and docs in English; README.md kept current.
