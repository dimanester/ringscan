# ringscan-win

Thin, safe wrappers over the Windows APIs RingScan needs. Every public item
compiles on every target; off Windows the functions return
`io::ErrorKind::Unsupported`, an empty list or `false`. The design notes are
in the module docs (`cargo doc -p ringscan-win --target x86_64-pc-windows-gnu`).

| API | What it does |
|-----|--------------|
| `list_dir(dir, &mut Vec<WinEntry>)` | One directory with logical **and** allocated sizes, attributes, reparse tag, mtime, file id. `FileIdExtdDirectoryInfo` → `FileIdBothDirectoryInfo` → `FindFirstFileExW` fallback. Long paths and odd names work. |
| `WinEntry` | `name` (UTF-8, lossy only for unpaired surrogates), `raw_name` (exact UTF-16 when lossy), `path_in(dir)`, `is_link()` (don't descend), `is_cloud()` |
| `Volume::open('C')`, `read_at`, `sector_size` | Raw positional reads of `\\.\C:` for the MFT reader; `Send + Sync`, parallel reads; needs elevation |
| `drives()` | Letters with label, fs, kind, total, free, cluster size, physical disk number; never waits more than 1.5 s |
| `is_elevated`, `relaunch_elevated`, `ui_lang_ukrainian`, `disable_error_dialogs` | Process helpers |
| `shell::{open, reveal, properties, copy_to_clipboard, recycle, delete_permanently, empty_recycle_bin, recycle_bin_size, open_apps_settings}` | Shell actions; `recycle` never deletes permanently (returns `NotRecyclable` instead) |
| `context_menu::{install, uninstall, is_installed, installed_exe, path_from_arg}` | "Open in RingScan" for folders, drives and folder backgrounds (HKCU) |
| `attrs::*` | `FILE_ATTRIBUTE_*` / `IO_REPARSE_TAG_*` values usable on any target |

## Tests

```bash
cargo test -p ringscan-win                                       # Linux: parser, paths, quoting
cargo test -p ringscan-win --target x86_64-pc-windows-gnu -- --nocapture   # on the Windows host
```

The Windows tests build fixtures under `%TEMP%\ringscan-dev\win-tests-<pid>`
and delete them afterwards. One test moves a small `ringscan-test-<pid>.tmp`
file to the Recycle Bin (and leaves it there). Optional:

* `RINGSCAN_TEST_UNC_DIR` — a writable UNC directory without a Recycle Bin
  (e.g. inside the WSL file system) to test that `recycle` refuses instead of
  deleting, and UNC listing. From WSL:

  ```bash
  mkdir -p target/win-unc-tests
  RINGSCAN_TEST_UNC_DIR=$PWD/target/win-unc-tests WSLENV=RINGSCAN_TEST_UNC_DIR/p \
    cargo test -p ringscan-win --target x86_64-pc-windows-gnu -- --nocapture
  ```

* `-- --ignored` also runs the clipboard round trip (it overwrites the
  clipboard and restores its text; skipped when clipboard history is on).

Benchmark (read-only):

```bash
cargo run --release -p ringscan-win --target x86_64-pc-windows-gnu --example bench
```
