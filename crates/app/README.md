# ringscan (GUI)

The RingScan window: a sunburst of a drive, a folder or the whole PC
(center disc = the focused folder, rings outward = deeper levels, arc angle
∝ size), with a drive sidebar, a toolbar, a details panel and a status bar.
Built on eframe/egui 0.37 (wgpu on Direct3D 12, 4× MSAA), Windows 11 look
(Segoe UI Variable, Segoe Fluent Icons, light/dark following the system),
Ukrainian and English. Scanning and file operations come from
`ringscan-engine`, OS integration from `ringscan-win`.

## Command line

```text
ringscan.exe                     scan all fixed drives under "This PC"
ringscan.exe PATH                scan a folder or a drive (D:\) and focus it
ringscan.exe --cli <PATH|X:|all> [options]
                                 headless report (ringscan-engine's CLI; --cli --help)
ringscan.exe --demo[=NODES]      deterministic synthetic "This PC" (no disk access), default 120 000 nodes
ringscan.exe --perf-tree         balanced synthetic tree with ~40 000 visible arcs (frame-time tests)
ringscan.exe --script "CMDS"     UI test harness (see below); --script-file FILE reads the commands
```

A missing path shows "Path not found" and falls back to "This PC". Path
arguments from the Explorer verb (`"C:\"` arrives as `C:"`) are repaired.

`--cli` runs before any window exists. The exe is a GUI-subsystem program,
so `console.rs` attaches to the parent console (or creates one) unless
stdout/stderr are already redirected to a file or pipe. cmd.exe does not
wait for GUI programs: use `start /wait`, redirect (`> report.txt`), or the
console build `ringscan-cli.exe`. In PowerShell pipe the output
(`| Out-String`, `| Out-File x.txt`) or use `Start-Process -Wait
-NoNewWindow`; PowerShell's own `> file` does not capture GUI-subsystem
programs (verified with 5.1 and 7.6).

## Using it

| Input | Action |
|---|---|
| left click on a folder arc | zoom in (220 ms eased angle/radius animation) |
| left click on the center disc | up one level |
| right click | context menu: Zoom, Open, Show in Explorer, Copy path, Rescan, Move to Recycle Bin…, Delete permanently…, Properties |
| middle click, mouse Back, Backspace, Alt+← | back |
| Alt+↑ | up · F5 rescan the focus · Ctrl+F search · Esc clear search / selection |
| Delete / Shift+Delete | recycle / delete the selected item (always confirmed; focus starts on Cancel) |
| Ctrl+C | copy the path of the hovered or selected item |

While a dialog is open (a confirmation, "Support RingScan") none of these
shortcuts act on the chart or the selection behind it: Esc only closes the
dialog, and Ctrl+C copies text selected in it.

Sidebar: "This PC" (the drives a no-argument start scans) and every drive
letter with label, file system and free space; "Choose folder…"; the
Recycle Bin (size, item count, "Empty…" with confirmation); "Installed
apps" (Settings › Apps); a small "♥ Support" link. The bottom block
(Recycle Bin, apps, link) is laid out first; the drive cards scroll in the
height left above it and "Choose folder…" stays under them, so nothing
overlaps in a low window or with many drives. A card, "Choose folder…" or a breadcrumb above
the scanned folder shows what a scan in memory already has instead of
scanning again (see "Scans in memory" below); F5 refreshes. Toolbar: size mode (on disk / logical), color
mode (folders / file types / age), settings (language, theme, free space,
number of rings, smallest arc, Explorer context menu, restart as
administrator, "Support RingScan…").

Free space is hidden by default: the chart then spans the used space, and
the "Share" column and the tooltip percentages use the same base (on a
drive, Users is ~53 % of the used space rather than ~20 % of the
capacity). The center disc of a drive root or "This PC" still says how
much is free ("вільно 1,15 ТБ" / "1.15 TB free"; for "This PC" the sum over
the scanned drives), and so do the tooltips of the center and of a drive.
Settings carry a `version`: a file without one (written before free space
was hidden by default) gets `show_free = false` once; the toggle stays.

Modification dates (tooltip "Modified", the date column) are local dates
as Explorer shows them: `platform::local_time` converts with the time zone
and the daylight saving rules of that date
(`SystemTimeToTzSpecificLocalTimeEx`). Demo data shows its dates without a
zone, so demo screenshots do not depend on the machine. The smallest-arc
slider uses the language's decimal separator ("0,30°") and accepts either
when typed.

"Support RingScan" is a dialog that opens only on request (no reminders,
no network requests): the Monobank jar as the main button (opens in the
default browser on the shell thread; card, Apple Pay, Google Pay from any
country; a failure shows "Could not open the link (code N)" in the UI
language), USDT (TRC-20) and BTC with address, copy button, a QR code drawn
locally (`qrcodegen`, black on white in both themes) and the network
warning, and a link to the account-wide DONATE.md. Addresses show in two
rows of equal length (copying selected text gives them verbatim). Below
640 pt of window height the dialog is compact (111 pt QR codes, no intro
paragraph), and its body scrolls if it still does not fit; the title and
Close always show. Unit tests check the addresses' checksums (bech32,
Base58Check) and, headless, that the dialog fits 760×520. The details panel has "Contents" (sortable, double-click
zooms, hover syncs with the chart), "Largest files" (top 100 under the
focus) and "File types"; a search replaces it with the result list while
the chart highlights the matches. The status bar shows progress (files,
folders, bytes, MFT %, rate, errors) and the method; hovering the method
lists each drive's method and why the MFT was not used.

With synthetic data (`--demo`, `--perf-tree`) every file action and
"Empty Recycle Bin" are disabled: made-up paths never reach the shell.

## Architecture

| Module | Role |
|---|---|
| `main.rs` | arguments, `--cli` routing, window and GPU options, fatal-error message box |
| `console.rs` | console attach for `--cli` / `--help` without clobbering redirected output |
| `app/` | state and frame loop (`mod.rs`), chart area (`chart_view.rs`), actions (`apply.rs`), chrome inputs (`chrome.rs`), scans kept in memory (`scans.rs`), harness glue and JSON state dump (`scripting.rs`), whole-frame tests of the real `App` on demo data, run headless through `Context::run_ui` (`tests.rs`: shortcuts behind dialogs, dialog and sidebar fit at 760×520) |
| `scan_source.rs` | `Scan`: the engine's `ScanJob` (or a synthetic tree), rescans, status aggregation, method summary |
| `platform.rs` | OS seam over `ringscan-win`: drives, UI language, elevation, shell calls on the **shell thread**, Recycle Bin, Explorer verb, folder picker |
| `model.rs` | `ChartModel`: core layout + per-arc colors/labels/parent links, built under one short read lock |
| `chart.rs` | geometry, tessellation into one mesh, labels, zoom transition, center disc, legends |
| `colors.rs` | OKLCH palettes for the three color modes and the special arcs |
| `panels.rs`, `details.rs`, `dialogs.rs`, `widgets.rs` | sidebar, toolbar, status bar, right panel, confirmations, toasts, custom widgets |
| `theme.rs`, `i18n.rs`, `settings.rs` | fonts/styles, all strings (uk + en) and number/plural helpers, persisted settings and their one-time migrations |
| `support.rs` | donation channels, QR codes, the "Support RingScan" dialog |
| `jobs.rs` | background queries (largest files, file types, search, folder contents) |
| `harness.rs` | `--script` parsing and input injection |
| `icon_art.rs` | the logo, shared by `build.rs` (multi-size `.ico`) and the window/taskbar icon |

Data flow: the engine's scanner threads write the shared
`Arc<RwLock<Tree>>` (walk: one short write lock per directory; MFT: one
install at the end; final sort: 8192 directories per write lock) → each
frame, if the inputs changed (focus, settings, search, theme, or the tree
generation — at most 8 Hz while scanning), `ChartModel::build` runs under
one `read_recursive` lock → the mesh is rebuilt only when the model or the
geometry changed → one `Shape::Mesh` per frame. Nothing holds the tree lock
across a frame; heavy queries run in `jobs` threads with their own locks.

Design decisions made during integration:

* **Engine jobs.** "This PC" is `Target::AllFixedDrives` (drives on
  different physical disks in parallel; MFT where the raw volume is
  readable, else the directory walk); a path argument is
  `Target::parse(path)`. The main job lives in an `Arc` so worker threads
  can run its blocking `delete` / `empty_recycle_bin`; a rescan is a second
  job on the same tree. When idle, the status bar shows the tree's own
  totals (files, folders, scanned bytes without `<unaccounted>` and
  `<reserved by Windows>`, folders with errors), so it stays right after
  rescans and deletes. The method label's tooltip shows the engine's
  reason per drive (`UnitNote`) in the UI language.
* **Drive extras.** A drive root shows `<reserved by Windows>` ("Резерв
  Windows": the storage reserve and NTFS reservations, no files in it,
  with its own tooltip note) next to `<unaccounted>` (what the scan could
  not see); both use the unaccounted color.
* **Scans in memory.** Leaving a scan (another drive, "This PC", a
  folder) keeps it, with its focus, back stack and selection, in a
  most-recently-used cache of 3 besides the current scan (`app/scans.rs`).
  A drive card shows the drive from the current tree, else from the most
  recently used cached tree that has it, else scans it; "This PC" likewise;
  a folder ("Choose folder…", a breadcrumb above the scanned root) prefers
  a scan of exactly that folder, then any tree that contains it, and never
  uses a scan the user stopped (until a rescan of its root completes).
  Never served from memory, so asking again scans again: a scan that
  failed (its root could not be read: a folder that did not exist yet, a
  drive that was not ready; it is not cached either), and a cached tree of
  a removable, network, optical or RAM drive or a UNC path, or of a fixed
  drive whose label, file system or size no longer match (the volume
  behind the letter may have been swapped). Switching restores the scan's
  view and focuses the requested node, with the restored focus on the
  back stack (Back stays within a tree); no rescan, no lost position.
  Cached scans that are still running keep running (each with its walker
  threads, so a background scan and a new scan of the same disk slow each
  other down); the status bar counts them ("1 scan in the background",
  hover for which) with a button that stops them; eviction cancels them.
  Everything else that holds node ids (model, mesh, zoom animation, hover,
  list and search jobs, a pending confirmation) is reset on a switch, and
  models, search results and the details panel carry the scan id, so ids
  never cross trees. Memory: a "This PC" tree of the development machine
  (1.04 M files, 279 K folders, 1.32 M nodes) holds 97 MB, so four such
  trees would add ~300 MB; in practice the cache holds one "This PC" plus
  smaller drives and folders. A delete or recycle runs in the scan it was
  confirmed in; when it has finished, the worker removes what is gone on
  disk from the other trees then in memory (scans started meanwhile
  included; Recycle Bin items after "Empty"), refreshes their drives' free
  / unaccounted space, and the app drops a cached scan whose root folder
  was deleted; if that is the scan shown, it shows the deleted item's
  parent folder instead. Only "not found" counts as gone: other users'
  Recycle Bin folders and System Volume Information answer "access
  denied" and stay, as does what a failed delete left behind.
* **Deletes** go through `ScanJob::delete` on a worker thread: the engine
  removes the items from the tree and refreshes the drive's `<free space>`
  / `<unaccounted>` (and its `$Recycle.Bin` node after a recycle); the
  worker then re-reads the drive list and the Recycle Bin size for the
  sidebar. "Cannot go to the Recycle Bin" gets its own message (nothing is
  ever deleted instead) and, for the item the bin refused (a name ending
  in a dot or a space, a drive without a bin), opens the "Delete
  permanently" confirmation, which the user may cancel. Before running a
  confirmed delete the app checks that every item is still in the tree
  under the id and path it was confirmed with; a rescan that replaced it
  while the dialog was open gives "The item changed after it was
  selected… nothing was deleted" instead of a false "Deleted" (the engine
  refuses stale ids as well). "Gone on disk" checks use verbatim paths, so
  `x.` is never mistaken for `x`.
* **Shell thread.** Open, Show in Explorer, Properties, Installed apps and
  Restart as administrator run on one long-lived STA thread with a message
  loop, never on the UI thread: with a Properties sheet open,
  `ShellExecuteEx` stalled "Show in Explorer" for ~5 s and never returned
  from "Installed apps", freezing the window.
* **System facts** (drive list, Recycle Bin size) are queried on a
  background thread at startup and after deletes (a dead network drive can
  take 1.5 s). The "This PC" card sums only the drives a "This PC" scan
  includes (not virtual drives such as Google Drive's FAT32 letter).
* **GPU.** Only the DX12 backend is compiled in on Windows (Vulkan/GL
  dropped the exe from 15.9 to 14.2 MB); DX12 falls back to the WARP
  software adapter. The low-power adapter is preferred (no waking a
  laptop's discrete GPU) and wgpu's allocator is asked for small blocks
  (`MemoryHints::MemoryUsage`): idle private memory went from 328 to
  186 MB. `WGPU_POWER_PREF` still overrides the adapter.

## Test harness

`--script "cmd; cmd; ..."` (or `--script-file`) drives the real app through
egui's input pipeline: OS input is dropped, scripted pointer/keyboard events
are injected in `raw_input_hook`, screenshots go through
`Context::request_screenshot` and are saved as PNG. Coordinates are egui
points (= pixels at 100 % scaling). Scripted runs are sandboxed: state in
`%TEMP%\ringscan-dev\script-state.ron` (wiped at start unless
`RINGSCAN_KEEP_STATE` is set; from WSL pass it with
`WSLENV="$WSLENV:RINGSCAN_KEEP_STATE"`), copied paths go to the log instead
of the clipboard, the Explorer-menu toggle uses the `RingScanTest` registry
key, and "Restart as administrator" only records its arguments (no UAC
prompt).

| Command | Effect |
|---|---|
| `wait <sec>` / `wait-scan [timeout]` | let frames run / until every job of the current scan is done (cached scans may still run) |
| `size <w> <h>` | resize the window |
| `move x y`, `click x y [left\|right\|middle\|back]`, `dblclick x y`, `scroll dy` | pointer |
| `move @probe`, `click @probe [button]`, `dblclick @probe` | pointer at a probe point: `@dir`, `@file`, `@free`, `@rest`, `@center`, `@support` (the sidebar link), `@choose-folder`, `@recycle-bin`, `@ok` / `@cancel` while a confirmation is open, `@jar`, `@copy-usdt`, `@copy-btc`, `@donate-page`, `@close` while the Support dialog is open (links are logged, never opened, in scripts) |
| `key <Name> [ctrl] [shift] [alt]`, `type <text>` | keyboard (egui key names: `Escape`, `F5`, `Delete`, `ArrowLeft`, `C`, ...) |
| `shot <file.png>` | screenshot (the script waits until it is written) |
| `lang uk\|en\|auto`, `theme light\|dark\|system`, `colors folders\|types\|age`, `tab 0..2` | settings |
| `set min-arc <deg>`, `set depth <n>`, `set free on\|off`, `set size disk\|logical`, `set panels on\|off` | more settings |
| `zoom <path>` / `select <path>` | focus a folder (`/` = root) / select a node (then `key Delete` asks to recycle it) |
| `scan <path>` / `drive <X>` / `this-pc` | what "Choose folder…" does with that folder / a click on that drive card / on "This PC" (switch to a scan in memory, or scan) |
| `log <file.json>` | state dump: focus, hover, selection, layout/mesh stats, frame times, scan progress with per-drive method and note, scan id and target, the number of scans running in the background, the cached scans (id, target, running, stopped, files, tree generation and memory, saved focus and back stack), drives, per-drive space accounting, Recycle Bin, elevation, Explorer verb, process peak memory, settings, window, toasts, confirmation, probes |
| `quit` | close (scripts also stop after 600 s) |

Never confirm a destructive dialog in a script on anything but files the
test created itself. Prefer `--script-file` over long `--script` strings.

## Measurements

Release build, Windows 11 host (12 cores, NVMe system drive), 1280×800 at
100 %, not elevated, warm caches:

| Case | Result |
|---|---|
| Start without arguments ("This PC": C: walked, D: read from the MFT, in parallel) | done in 24.7–26.3 s; 907–917 K files, 279 K folders, 1.48 TB scanned; process peak working set 251–255 MB, peak private 341–347 MB (of which ~104 / 186 MB is the idle GUI and GPU stack) |
| same, frame times while scanning | CPU p50 0.45–0.64 ms, p95 1.4 ms; frame interval p95 17.0 ms (vsync); longest frame at the end of the scan 69 ms (was 241 ms before the engine sorted in chunks) |
| `ringscan.exe C:\Windows` vs `ringscan.exe --cli C:\Windows --json` | 248,619 files, 204,957 folders, 50 errors on both; allocated bytes equal to within 8 KB (files changed between the runs); GUI 19.3 s |
| rescan of `C:\Program Files` from the chart (F5) | 0.81 s; tree totals identical before and after |
| permanent delete of a 64 MiB file inside a `C:` scan | scanned bytes −67,108,864 exactly; C:'s `<free space>` and the sidebar card +66.9 MB (other activity accounts for the rest) |
| `--perf-tree`, min arc 0.01° (from the GUI author) | 41 559 arcs / 168 k vertices; static frames CPU p95 0.90 ms; zoom animation mesh build p95 7.6 ms, still 60 fps |
| exe | 14.5 MB (14,468,096 bytes), imports only system DLLs (kernel32, user32, gdi32, shell32, ole32, combase, dxgi, dwrite, uiautomationcore, …); `.rsrc` holds icons (16–256 px), VERSIONINFO and the manifest |

## Build notes

`build.rs` rasterizes the logo at 16/24/32/48/64/256 px into a `.ico`,
writes a resource script with it, `assets/ringscan.manifest`
(PerMonitorV2, longPathAware, UTF-8 code page, asInvoker, Common Controls
v6, Windows 10/11) and a VERSIONINFO block, and links it via
`x86_64-w64-mingw32-windres` (override with `RINGSCAN_WINDRES`).

eframe is built without default features: `accesskit`, `clipboard`,
`default_fonts`, `links`, `system_fonts`, `persistence` and
`wgpu_no_default_features`, plus `wgpu` with only `dx12` on Windows. The
crate also builds on Linux (x11 + wgpu defaults there) so its unit tests
run in WSL; the GUI itself only targets Windows.
