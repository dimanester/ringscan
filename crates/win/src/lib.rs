//! # ringscan-win
//!
//! Thin, safe wrappers over the Windows APIs RingScan needs:
//!
//! * [`list_dir`] — one directory listing with logical *and* allocated sizes,
//!   attributes, reparse tags, mtime and file ids, without opening each file;
//! * [`Volume`] — positional raw reads from `\\.\C:` for the MFT reader;
//! * [`drives`](fn@drives) — drive letters with label, file system, totals, cluster size
//!   and physical disk number, never hanging on dead network drives;
//! * [`file_id_and_links`], [`file_by_id`] — hard links of one file (to move
//!   a deleted link's size to a surviving one); [`compressed_size`] — the
//!   on-disk size of WOF-compressed files;
//! * [`is_elevated`], [`relaunch_elevated`], [`ui_lang_ukrainian`];
//! * [`shell`] — open, reveal, properties, clipboard, Recycle Bin, delete;
//! * [`context_menu`] — the optional "Open in RingScan" Explorer verb (HKCU).
//!
//! ## Design decisions
//!
//! * **Portable surface, Windows body.** Every public item exists on every
//!   target. The real code sits behind `#[cfg(windows)]`; elsewhere functions
//!   return [`std::io::ErrorKind::Unsupported`], an empty list or `false`. The
//!   workspace therefore type-checks on Linux, and the pure logic (directory
//!   entry parsing, `\\?\` path conversion, command-line quoting, argument
//!   repair) is unit-tested there; everything else is tested on Windows with
//!   `cargo test -p ringscan-win --target x86_64-pc-windows-gnu`.
//! * **windows-sys only.** The one COM API used (`IFileOperation`, for safe
//!   Recycle Bin moves) is called through a few hand-declared vtables in
//!   `com.rs` instead of the `windows` crate, whose Shell module is several
//!   MB of generated code that every consumer would have to compile. The
//!   vtable layouts are asserted at compile time.
//! * **Errors** are [`std::io::Error`]s built from the raw Win32 code (or the
//!   HRESULT), so `kind()` uses std's mapping (`PermissionDenied`,
//!   `NotFound`, ...) and `raw_os_error()` keeps the exact code.
//! * **Paths** are handed to file-system APIs in verbatim form (`\\?\C:\…`,
//!   `\\?\UNC\server\share\…`): no `MAX_PATH` limit and no Win32 name
//!   munging, so names ending in a dot or a space stay reachable. The Shell
//!   does not understand `\\?\`, so shell calls get the plain form instead,
//!   and refuse names ending in a dot or a space, which the plain form
//!   would silently turn into another item's name. Callers that pass tree
//!   paths to `std::fs` should use [`paths::verbatim_path`] for the same
//!   reason.
//! * **Names** are UTF-16 on Windows and may contain unpaired surrogates.
//!   [`WinEntry::name`] is always valid UTF-8 (lossy only in that rare case);
//!   the exact name is kept in [`WinEntry::raw_name`] and used by
//!   [`WinEntry::path_in`], so such files can still be listed and deleted.
//! * **Safety rails.** Destructive calls refuse relative paths, `.`/`..`
//!   components and roots; [`shell::recycle`] never falls back to a permanent
//!   delete (see that function for how this is enforced).

// The portable helpers (entry parser, path conversion, ...) are called only by
// the Windows implementation; elsewhere they exist for their unit tests.
#![cfg_attr(not(windows), allow(dead_code))]

pub mod attrs;
pub mod context_menu;
pub mod shell;

mod dir;
mod drives;
mod links;
pub mod paths;
mod process;
mod volume;

#[cfg(windows)]
mod com;
#[cfg(windows)]
mod sys;
#[cfg(all(test, windows))]
mod test_util;

pub use dir::{WinEntry, list_dir};
pub use drives::{DriveInfo, DriveKind, drives, drives_with_timeout};
pub use links::{LinkedFile, compressed_size, file_by_id, file_id_and_links};
pub use process::{disable_error_dialogs, is_elevated, relaunch_elevated, ui_lang_ukrainian};
pub use volume::Volume;

/// The error every stub returns on non-Windows targets.
#[cfg(not(windows))]
fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "ringscan-win: only available on Windows",
    )
}
