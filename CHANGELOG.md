# Changelog

All notable changes to RingScan are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-11-15

First public release: a disk usage visualizer for Windows 10 and 11 (x64)
that shows where your space went as a sunburst chart.

### Added

- **Sunburst chart** of the whole PC ("This PC": all fixed drives at once),
  a single drive or any folder. Rings go outward one folder level at a
  time and each arc's angle is proportional to its size. Hover for size,
  share, file and folder counts and date; click to zoom in (animated),
  click the center to go back up.
- **Fast scanning with a live chart.** Run as administrator and NTFS drives
  are read straight from the master file table (MFT): about a million files
  in 4 to 5 seconds, including folders a normal scan is denied. Without
  elevation, drives are walked in parallel (several threads per disk,
  different disks at once). The chart grows while the scan runs, and the
  status bar shows the method used per drive and why.
- **Scans kept in memory**: switching between drives, "This PC" and folders
  shows the earlier scan instantly instead of scanning again (F5 rescans).
- **Side panels**: the focused folder's contents (sortable), the largest
  files and a breakdown by file type; drives with free space; the Recycle
  Bin with its size and "Empty"; a shortcut to Settings > Installed apps.
- **Search** by name, with the matches highlighted in the chart.
- **Color modes**: by folder, by file type or by age.
- **Ukrainian and English** interface, light and dark themes following
  Windows, per-monitor DPI, Windows 11 look.
- **Honest totals**: each drive shows `<reserved by Windows>` (the storage
  reserve Windows keeps for updates, about 5 GB on a system drive) and
  `<unaccounted>` (used space the scan could not see), so a drive adds up to
  what Windows reports. Free space is written in the center of the chart;
  a setting draws it as an arc instead. These areas fill their wedge out to
  the edge of the chart (hatched for space no file explains), so they never
  look like a hole in it.
- **Accurate sizes**: size on disk or logical size; hard links counted
  once; junctions and symbolic links listed but not followed; cloud
  placeholders (OneDrive and similar) counted by what they use locally.
- **Act on what you find**: Open, Show in Explorer, Copy path, Properties,
  Rescan, Move to Recycle Bin and Delete permanently, from the context menu
  or the keyboard. Totals and free space update right away.
- **Safe deletes.** Every delete is confirmed. Moving to the Recycle Bin
  never silently turns into a permanent delete: if the bin refuses an item,
  RingScan says so and asks separately. Names ending in a dot or a space
  are handled exactly, and an item that changed after you selected it is
  not deleted.
- Optional **"Open in RingScan"** entry in the Explorer context menu for
  folders and drives (per user, no administrator rights needed).
- **Headless command line**: `RingScan.exe --cli <PATH|X:|all>` prints a
  report; `--json` for machine-readable output, `--dump FILE` to also
  write every file to a tab-separated list.
- A single **portable exe** (about 14 MB): no installer, no runtime
  dependencies; settings live in `%APPDATA%\RingScan`. RingScan makes no
  network requests.

### Notes

- The exe is not code-signed, so Windows SmartScreen may warn on first
  start ("Windows protected your PC" > **More info** > **Run anyway**).
  Releases come with a SHA-256 file and a GitHub build provenance
  attestation instead; see [Download](https://github.com/dimanester/ringscan#download).

[Unreleased]: https://github.com/dimanester/ringscan/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/dimanester/ringscan/releases/tag/v0.1.0
