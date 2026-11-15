# RingScan

Disk usage visualizer for Windows: the whole PC, a drive or a folder as a
sunburst chart (concentric rings, outer ring = deeper folder level, arc
angle ∝ size). A modern remake of Steffen Gerlach's *Scanner* 2.13: one
portable `.exe`, no installer, no runtime dependencies, Ukrainian and
English UI.

See [SPEC.md](SPEC.md) for the original scope and the cross-crate
contracts; each crate's README has its design notes and measurements.

## Features

* **Sunburst of everything at once**: all fixed drives under "This PC", a
  drive, or any folder; hover for size, share of parent / focus, file and
  folder counts, date; click to zoom in (animated), click the center to go
  up; tiny items are merged into "N smaller items".
* **Fast scanning, live chart**: a parallel directory walker (8 threads per
  physical disk; disks in parallel), or the NTFS master file table (MFT)
  when the volume can be read raw — usually only as administrator — which
  scans a whole drive in seconds and also sees folders a walk is denied.
  The chart grows while the scan runs.
* **Honest numbers**: allocated ("on disk") or logical size; hard links
  counted once; junctions and symlinks listed but not followed; cloud
  placeholders by their local size; each scanned drive shows `<free space>`
  and `<unaccounted>` (used space the scan could not see: system areas,
  other users' folders, NTFS metadata), so a drive adds up to what Windows
  reports.
* **Side panels**: drives with free space, Recycle Bin (size, empty), a link
  to Settings › Installed apps; the focused folder's contents (sortable),
  the largest files, and a file-type breakdown; search by name with
  highlighted matches.
* **Act on what you find**: Open, Show in Explorer, Copy path, Properties,
  Rescan, Move to Recycle Bin, Delete permanently (always confirmed; the
  Recycle Bin path never falls back to a permanent delete). Free space and
  totals update right away.
* **Color modes**: by folder, by file type, by age. Light/dark following
  Windows, per-monitor DPI, Windows 11 look.
* **Explorer integration** (optional, per user, no admin): "Open in
  RingScan" on folders, drives and folder backgrounds.
* **Headless CLI** for scripts: `ringscan.exe --cli <PATH|X:|all> [--json]`.

## Usage

```text
RingScan.exe                     scan all fixed drives ("This PC")
RingScan.exe D:\                 scan a drive (or any folder) and focus it
RingScan.exe --cli <PATH|X:|all> [--json] [--method auto|mft|walk] [--depth N] [--top N]
                                 [--threads N] [--logical] [--all-links] [--max-children N]
                                 [--progress] [--dump FILE]
RingScan.exe --cli --help        CLI options
```

| Input | Action |
|---|---|
| click a folder arc · click the center | zoom in · up one level |
| right click | context menu (zoom, open, show in Explorer, copy path, rescan, recycle, delete, properties) |
| Backspace, Alt+←, mouse Back, middle click | back |
| Alt+↑ | up one level |
| F5 | rescan the focused folder |
| Ctrl+F · Esc | search · clear search / selection |
| Delete · Shift+Delete | move the selected item to the Recycle Bin · delete permanently (both confirmed) |
| Ctrl+C | copy the path of the hovered or selected item |

Settings (gear icon): language, theme, free space on/off, number of rings,
smallest arc, the Explorer context-menu entry, and "Restart as
administrator".

### Administrator / MFT mode

Without elevation Windows usually refuses raw volume reads, so drives are
walked folder by folder (a 2 TB system drive with ~1 M files: ~25–30 s) and
folders you may not open end up in `<unaccounted>`. Run RingScan as
administrator (Settings › "Restart as administrator", or right-click the
exe › "Run as administrator") and NTFS drives are read from the MFT
instead. The status bar shows the method per drive; hover it for the
reason a drive was walked.

To compare both methods on `C:` from an **elevated PowerShell**:

```powershell
$rs = Join-Path ([Environment]::GetFolderPath('Desktop')) 'RingScan\RingScan.exe'
& $rs --cli C: --method mft  --depth 1 --top 20 --dump "$env:TEMP\c-mft.tsv"  | Out-String
& $rs --cli C: --method walk --depth 1 --top 20 --dump "$env:TEMP\c-walk.tsv" | Out-String
```

### Command line from a terminal

`RingScan.exe` is a GUI program that attaches to the calling console for
`--cli`. cmd.exe does not wait for GUI programs: use
`start /wait RingScan.exe --cli ...` to get the report before the prompt
returns, or redirect it to a file
(`RingScan.exe --cli C:\Windows > report.txt`). In PowerShell pipe it
(`| Out-String`, `| Out-File report.txt`) or use
`Start-Process -Wait -NoNewWindow`; PowerShell's own `> file` does not
capture GUI programs. Exit codes: 0 scanned, 1 nothing could be scanned,
2 usage error. `ringscan-cli.exe` (built from `crates/engine`) is the same
CLI as a console program.

## Build (from WSL or Linux)

```bash
rustup target add x86_64-pc-windows-gnu          # plus: apt install gcc-mingw-w64-x86-64
cargo build --release --target x86_64-pc-windows-gnu -p ringscan
# -> target/x86_64-pc-windows-gnu/release/ringscan.exe (single portable exe, ~14 MB)
cargo build --release --target x86_64-pc-windows-gnu -p ringscan-engine --bin ringscan-cli
```

`.cargo/config.toml` links statically with mingw-w64 (no extra DLLs) and
sets `scripts/winrun.sh` as the runner, so `cargo run` / `cargo test` with
`--target x86_64-pc-windows-gnu` execute on the Windows host when building
from WSL.

## Test

```bash
cargo test -p ringscan-core -p ringscan-ntfs -p ringscan-win -p ringscan-engine -p ringscan   # Linux
cargo test --target x86_64-pc-windows-gnu -p ringscan-win          # Windows host (see crates/win/README.md)
cargo test --target x86_64-pc-windows-gnu -p ringscan-engine       # Windows host
cargo test --target x86_64-pc-windows-gnu -p ringscan              # Windows host
sudo scripts/make-ntfs-fixtures.sh                                  # NTFS images for the MFT tests
```

Build one crate at a time with `-p` (the GUI crate builds on Linux only for
its unit tests). The GUI has a scripted test harness (`--script` /
`--script-file`, see [crates/app/README.md](crates/app/README.md)) that
injects input, takes screenshots and dumps the state as JSON; scripted runs
never touch the user's settings, clipboard or Explorer menu entry, and never
show a UAC prompt. Destructive tests only ever touch files they created
under `%TEMP%\ringscan-dev`.

## Layout

| Crate | What |
|---|---|
| `crates/core` (`ringscan-core`) | platform-independent: arena tree, parallel walker, sunburst layout, stats, formatting |
| `crates/ntfs` (`ringscan-ntfs`) | MFT reader over a `ReadAt` trait, tested on mkntfs images |
| `crates/win` (`ringscan-win`) | Windows primitives: directory listing with allocated sizes, raw volume reads, drives, shell actions, Recycle Bin, registry verb |
| `crates/engine` (`ringscan-engine`) | scan jobs (MFT or walk per drive, hard links once, free/unaccounted space, rescan, delete) and the CLI |
| `crates/app` (`ringscan`) | the GUI exe (eframe/egui on Direct3D 12) |

## License

MIT.
