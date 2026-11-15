# ringscan-engine

Glue between `ringscan-core` (tree, walker, layout), `ringscan-ntfs` (MFT
reader) and `ringscan-win` (Windows APIs), plus the headless CLI. The GUI
uses one type, `ScanJob`:

```rust
use ringscan_engine::{ScanJob, ScanOptions, Target, DeleteMode};

let job = ScanJob::start(Target::Drive('C'), ScanOptions::default()); // returns at once
let p = job.progress();          // files, dirs, bytes, errors, current path, phase,
                                 // method used per drive, MFT records done/total, elapsed
let tree = job.tree().read();    // Arc<RwLock<Tree>>: lay out the partial tree any time
job.cancel();                    // or drop the job
let p = job.wait();              // blocks until done

let re = job.rescan(node)?;      // clear + re-walk a folder (or a whole drive) in place
job.delete(&[node], DeleteMode::Recycle)?;   // shell + tree + free/unaccounted update
job.empty_recycle_bin(Some('C'))?;
job.drives();                    // per drive: total, free, used, reserved, scanned, unaccounted
```

`Target::Drive('C')`, `Target::Path(path)` (a drive root path becomes a
drive) or `Target::AllFixedDrives` (virtual "This PC" root; drives on
different physical disks are scanned in parallel). `Method::Auto` reads the
MFT of a whole NTFS drive and walks everything else; reading the raw volume
usually needs elevation, and when it is refused (or the MFT read fails) the
drive is walked and the reason is in `UnitProgress::note`, a `UnitNote`
(`NotElevated`, `NotNtfs`, `NoDriveInfo`, `MftFailed`, `FolderOnly`) that the
GUI localizes and the CLI prints in English. Some volumes are readable
without elevation and then get the MFT path anyway.

Design notes are in the module docs (`cargo doc -p ringscan-engine`):
`job` (threads, locking, MFT install, rescans, delete), `lister` (attribute
mapping, hard links in walk mode, names that are not valid Unicode,
WOF-compressed files), `mft` (MFT → tree rules), `drive` (`<free space>`,
`<reserved by Windows>`, `<unaccounted>`, which drives "all" means).

Space at a drive root: `used = total − free` (`GetDiskFreeSpaceExW`)
includes clusters the file system keeps reserved without any file in them:
the Windows storage reserve (~5.4 GB on the development machine's C:) plus
NTFS's own reservations (`TotalReservedAllocationUnits` of
`GetDiskSpaceInformationW`). They get their own node,
`<reserved by Windows>`, so `<unaccounted> = used − reserved − scanned` is
only what the scan could not see. Measured: on an elevated MFT read of C:
5.73 GB was unaccounted before this split, of which 5.49 GB was the reserve;
on D: (MFT) the scan is within 39 clusters (160 KB) of the allocated
clusters, the remaining 4 MB being D:'s reserve. The CLI JSON reports
`reserved` and `allocated` (`used − reserved`) per drive.

Delete safety: paths of scanned items reach the file system in verbatim
form (names ending in a dot or a space are never confused with their
stripped sibling; the Recycle Bin refuses them with `NotRecyclable`), and a
node whose name, or an ancestor's, is not valid Unicode is only deleted (or
opened) when the walk recorded exactly one real item for that spelling;
MFT trees and ambiguous spellings are refused. Ids that are no longer
live (a rescan replaced the item after it was chosen) fail the whole call
with `ItemChanged` (`is_item_changed`) before anything is deleted, because
their path may now name another item. Deleting the link of a
multi-linked file that carries its size moves the size to a surviving link
in the tree, since no space is freed.

## CLI

```text
ringscan --cli <PATH|X:|all> [--json] [--method auto|mft|walk] [--depth N]
               [--top N] [--threads N] [--logical] [--all-links]
               [--max-children N] [--progress] [--dump FILE]
```

`ringscan-cli` (this crate's binary) is the same entry point
(`ringscan_engine::run_cli`) without the GUI. Exit code 0 = scanned, 1 =
nothing could be scanned, 2 = usage error. `--json` prints one object with
raw byte counts (`files`, `dirs`, `errors`, `elapsed_ms`, `total`,
`entries_by_flag`, `error_paths`, `memory`, `units`, `drives`, `tree`,
`largest_files`). `--dump` writes every file as `logical<TAB>alloc<TAB>flags<TAB>path`.

```bash
cargo run --release --target x86_64-pc-windows-gnu -p ringscan-engine --bin ringscan-cli -- 'C:\Windows' --top 20
```

### MFT mode (needs an elevated terminal)

```bat
copy target\x86_64-pc-windows-gnu\release\ringscan-cli.exe %TEMP%\
%TEMP%\ringscan-cli.exe --cli C: --method mft --depth 1 --top 20 --dump %TEMP%\c-mft.tsv
%TEMP%\ringscan-cli.exe --cli C: --method walk --depth 1 --top 20 --dump %TEMP%\c-walk.tsv
```

`--method mft` reports a fallback instead of failing when the MFT cannot be
read (the note says why). Compare the two dumps / reports; the expected
differences are listed under "Tests" below.

## Tests

```bash
cargo test -p ringscan-engine                                   # Linux
cargo test -p ringscan-engine --target x86_64-pc-windows-gnu    # Windows host
```

* Unit tests: attribute mapping (incl. the on-disk EA bit that MFT mode
  must not read as `RECALL_ON_OPEN`), resident-file sizes, Recycle Bin flag,
  hard-link filter (incl. rescan seeding), lossy names, MFT → tree rules
  (hard links, metafiles, sparse metafile logical cap, orphans, cloud,
  junctions, random listings rebuilt path by path, detached directory
  subtrees), "gone" after a failed delete (only not-found), drive extras,
  disk grouping, CLI parsing.
* `tests/job.rs`: `ScanJob` on a fixture folder (hard link, directory link,
  empty dir): totals, link counted once, link not followed, `--all-links`,
  rescan in place (with a hard link that crosses the rescanned folder),
  missing target, cancel and drop, delete refusals, the CLI binary (Linux).
  On Windows also: `WinLister` flags (junction, hidden, MFT-resident size 0,
  hard links), permanent delete, and `RINGSCAN_TEST_UNC_DIR` (see
  `crates/win/README.md`) for "no Recycle Bin on a share: nothing deleted".
* `tests/mft_crosscheck.rs` (Linux, root, ntfs-3g; needs the fixtures from
  `sudo scripts/make-ntfs-fixtures.sh`): builds the tree from each image's
  MFT, mounts the image read-only with ntfs-3g under
  `fixtures/.mnt-engine-*` (always unmounted again), walks the mount with
  core's walker, and compares both trees path by path. Every difference
  must have a known cause, and the MFT total must be within 32 clusters of
  the used space ntfs-3g reports.

Expected MFT ↔ walk differences (each counted and checked by the test):

| Difference | Cause |
|---|---|
| reparse points look like symlinks to ntfs-3g | junction / symlink / cloud placeholder; sizes are of the link text |
| Interix symlink | a plain file with an `IntxLNK` body to Windows, a symlink to ntfs-3g |
| MFT-resident files: alloc 0 vs `st_blocks` | no clusters of their own; ntfs-3g rounds the length to 512 |
| directory logical size | NTFS directories have logical size 0 (+ named streams); ntfs-3g reports the index size |
| named streams (ADS) | the MFT counts them; a walk sees only the unnamed stream |
| used − MFT total: a few clusters | non-resident `$MFT:$BITMAP`, `$SECURITY_DESCRIPTOR`, `$ATTRIBUTE_LIST` are not counted by the reader |

On Windows the same causes apply between the two methods (walk mode sees
directories as 0 bytes, no named streams and no metafiles; they end up in
`<unaccounted>`). Flags, as measured on an elevated C: (1.08 M files):

* CLOUD: the same 14,295 OneDrive placeholder files in both modes. On disk,
  attribute bit `0x40000` means "has extended attributes"
  (`FILE_ATTRIBUTE_EA`; 117,785 catalog-signed files in `WindowsApps`,
  `WinSxS`, `System32`, ... carry a `$CI.CATALOGHINT` EA), the same value
  as the enumeration-only `RECALL_ON_OPEN`, so MFT mode ignores it. MFT
  mode may still flag OneDrive *folders* (their Cloud Files reparse tag is
  on disk), which a walk shows as cloud only while they are not fully
  populated (`RECALL_ON_OPEN`): the Cloud Files filter hides placeholder
  reparse points from directory listings.
* SPARSE: the MFT shows the on-disk state (OneDrive placeholders,
  WOF-compressed files); the filters hide it from a walk. WOF-compressed
  files also list an allocation of 0 (192 files, 16.5 MB on that C:, mostly
  Chrome locales and `WsxPacks`; gigabytes on a CompactOS system); the walk
  asks `GetCompressedFileSizeW` for them, so both modes count them.
* REPARSE: a `$REPARSE_POINT` value that is not resident in the MFT record
  (large cloud placeholder blobs) has no readable tag in MFT mode; such
  entries are not treated as links (only a childless directory keeps
  REPARSE, as a safety net). The JSON report counts them per drive
  (`mft.unread_reparse_tags`).
* Hard links: both count each file once, but the counted link often
  differs (25.7 K of 90 K multi-linked files): see `mft` module docs.

## Measured (development machine: Windows 11, 12 cores, NVMe C:, second disk D:, not elevated)

Release build, warm caches, default options unless noted.

| Scan | Result |
|---|---|
| `C:\Windows`, walk, every link counted (`--all-links`) | 248,619 files, 64,069,403,378 bytes: **identical** to `Get-ChildItem -Recurse -Force -File \| Measure-Object Length -Sum` (also path by path); 22 s vs 127 s for PowerShell 7 |
| `C:\Windows`, walk, links once (default) | 51.7 GB logical (71,429 extra hard links, WinSxS); 50 folders denied (same set PowerShell skips) |
| `C:\Program Files`, walk, `--all-links` | 78,424 files, 24,862,608,318 bytes: identical to PowerShell, path by path (0.8 s vs 6.9 s). 9 junctions + 5 directory symlinks listed but not followed by either; 20 file symlinks counted with length 0 by both |
| `C:` whole drive, walk | 849K files + 278K dirs in 28–30 s (28K files/s, 37K entries/s; ~21 s of it is `C:\Windows`); peak 146 MB private / 137 MB working set (tree 89 MB for 1.13M nodes); scanned 690 GB + unaccounted 83 GB + free 1273 GB = total 2047 GB |
| `D:` whole drive, MFT (raw volume readable without elevation on this machine) | 107K MFT records, 53.6K files in 1.3–1.7 s; unaccounted 4.35 MB of 787 GB used. Walk of the same drive: 6.3 s, 2.46 GB unaccounted (48 denied folders) |
| `all` (This PC) | C: (walk) and D: (MFT) in parallel: 28.5 s total |

PowerShell 5.1 and 7 behave the same here: `-Recurse` does not descend into
junctions or directory symlinks, and lists every hard link of a file.

MFT vs walk on D:, path by path: nothing only in the walk; only in the MFT:
18 NTFS metafiles (243 MB: `$MFT`, `$LogFile`, `$Bitmap`, `$Extend\...`) and
586 entries inside the 48 folders the walk was denied (2.2 GB); size
differences: 1,270 files with alternate data streams (`Zone.Identifier`,
`CachedTiles`, ...: +113,813 logical bytes, +131,072 allocated for the 32
non-resident ones).

Walker threads on `C:\Windows` (453,576 entries; two rounds): 1 → 33–36 s,
2 → 28–33 s, 4 → 21–24 s, 8 → 22–27 s, 12 → 22–25 s, 16 → 22 s, 32 → 21 s.
Throughput flattens at ~21K entries/s from 4 threads on (opening each of the
205K directories dominates), so the default is 8.

Hard-link de-duplication costs ~25 bytes per file while a walk runs (C:: 150
vs 129 MB peak private) and keeps 15.5 GB of duplicate links out of the
total, so `<unaccounted>` stays honest.

### MFT mode at system-drive scale (`examples/mft_bench.rs`, no elevation)

```bash
cargo run --release --target x86_64-pc-windows-gnu -p ringscan-engine --example mft_bench -- gen 'C:\...\synth.img'
cargo run --release --target x86_64-pc-windows-gnu -p ringscan-engine --example mft_bench -- scan 'C:\...\synth.img'
```

`gen` writes a synthetic NTFS image shaped like the measured C: (1.08 M
files, 313 K directories, 90 K extra hard links, 3.13 M record slots, names
of ~30 characters; 3 GB, delete it afterwards); `scan` runs the real MFT
path on it (`scan_mft` + `build_tree`, 8 parse threads) and prints private
memory per phase. Tree: 1.48 M nodes, 106 MB.

| Version | read + parse | tree build | peak private |
|---|---|---|---|
| before (chunks concatenated at the end, `parent_first_order` over all entries) | 0.58–0.73 s | 0.65–0.78 s | 433–437 MB |
| list sized by `$MFT:$BITMAP` and filled while parsing; directories-first build that frees names as it goes | 0.51–0.52 s | 0.50–0.54 s | 276–281 MB |
| same, image without `$MFT:$BITMAP` (fallback) | 0.55–0.64 s | 0.50–0.52 s | 426–431 MB |

The elevated CLI scan of the real C: (1.08 M files) peaked at 440 MB
private before the change (walk: 210 MB); on D: (54 K files) the peak is
21 MB either way (chunk buffers dominate). Per phase now: reading peaks at
~255 MB (the file list, ~225 MB, plus chunk buffers), the build at ~280 MB
(list structs + growing tree; names are freed as nodes are added).
