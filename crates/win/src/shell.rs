//! Shell actions: open, show in Explorer, properties, clipboard, Recycle
//! Bin, permanent delete, "Installed apps".
//!
//! ## Safety rails for destructive calls
//!
//! [`recycle`] and [`delete_permanently`] validate *all* paths before
//! touching any: each must be absolute, without `.`/`..` components, and not
//! a root (`C:\`, `\\server\share`). Missing items fail with `NotFound`
//! before anything is done.
//!
//! [`recycle`] never deletes permanently. It uses `IFileOperation` with a
//! progress sink that vetoes any deletion the shell would not route to the
//! Recycle Bin (item too large for the bin, drive or network share without
//! a bin, bin disabled) — `SHFileOperationW` with `FOF_ALLOWUNDO |
//! FOF_NOCONFIRMATION` would silently destroy such items instead. A vetoed
//! item yields an error carrying [`NotRecyclable`] (test with
//! [`is_not_recyclable`]) so the app can ask for a confirmed
//! [`delete_permanently`]. Details in `com.rs`.
//!
//! [`delete_permanently`] uses std's file-system calls (long paths, no shell
//! dialogs): links (junctions, symlinks, mount points) are removed as links
//! — their targets are never touched — directories recursively, read-only
//! files too. Explorer is notified afterwards.
//!
//! Items are processed in order; if an error stops the batch, earlier items
//! may already be gone, so callers should re-check (rescan) all of them.
//!
//! ## Threads and COM
//!
//! [`open`], [`reveal`], [`properties`] and [`open_apps_settings`] initialize
//! COM (STA) on the calling thread if needed and deliberately leave it
//! initialized, because the shell may finish parts of the request (folder
//! windows, property sheets) after the call returns. Call them from the UI
//! thread. [`recycle`] runs on its own short-lived STA thread and can be
//! called from anywhere. The shell does not support `\\?\` paths, so these
//! calls are limited to paths the shell itself can handle (usually
//! < 260 characters); [`delete_permanently`] has no such limit.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Error payload of [`recycle`] when an item cannot go to the Recycle Bin
/// and would have been deleted permanently instead. Nothing was deleted
/// from that item on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotRecyclable {
    /// The item the shell wanted to destroy.
    pub path: PathBuf,
}

impl fmt::Display for NotRecyclable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot be moved to the Recycle Bin (it would be deleted permanently): {}",
            self.path.display()
        )
    }
}

impl std::error::Error for NotRecyclable {}

/// Whether `err` (from [`recycle`]) means "this item cannot be recycled".
pub fn is_not_recyclable(err: &io::Error) -> bool {
    err.get_ref().is_some_and(|e| e.is::<NotRecyclable>())
}

/// Opens a file with its default application, or a folder in Explorer — the
/// same as a double click (`ShellExecuteExW` with the default verb, which is
/// "open" for nearly every type).
pub fn open(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    return imp::open(path);
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(crate::unsupported())
    }
}

/// Opens the containing folder in Explorer with the item selected
/// (`SHOpenFolderAndSelectItems`, falling back to `explorer /select,`).
pub fn reveal(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    return imp::reveal(path);
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(crate::unsupported())
    }
}

/// Shows the Explorer "Properties" dialog of a file, folder or drive root.
/// The dialog belongs to this process and closes when it exits.
pub fn properties(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    return imp::properties(path);
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(crate::unsupported())
    }
}

/// Puts `text` on the clipboard as `CF_UNICODETEXT`.
pub fn copy_to_clipboard(text: &str) -> io::Result<()> {
    #[cfg(windows)]
    return imp::copy_to_clipboard(text);
    #[cfg(not(windows))]
    {
        let _ = text;
        Err(crate::unsupported())
    }
}

/// Moves files and folders to the Recycle Bin, never deleting anything
/// permanently (see the module docs). An item that cannot be recycled fails
/// with an error for which [`is_not_recyclable`] is true.
pub fn recycle(paths: &[PathBuf]) -> io::Result<()> {
    #[cfg(windows)]
    return imp::recycle(paths);
    #[cfg(not(windows))]
    {
        let _ = paths;
        Err(crate::unsupported())
    }
}

/// Deletes files and folders permanently, bypassing the Recycle Bin. Links
/// are removed without touching their targets. The caller must have asked
/// the user to confirm.
pub fn delete_permanently(paths: &[PathBuf]) -> io::Result<()> {
    #[cfg(windows)]
    return imp::delete_permanently(paths);
    #[cfg(not(windows))]
    {
        let _ = paths;
        Err(crate::unsupported())
    }
}

/// Empties the Recycle Bin of one drive (`Some('C')`) or of all drives
/// (`None`), without a confirmation prompt — the app asks first — but with
/// the shell's progress window for long runs. Succeeds if the bin is
/// already empty.
pub fn empty_recycle_bin(drive: Option<char>) -> io::Result<()> {
    #[cfg(windows)]
    return imp::empty_recycle_bin(drive);
    #[cfg(not(windows))]
    {
        let _ = drive;
        Err(crate::unsupported())
    }
}

/// `(bytes, items)` in the Recycle Bin of one drive or of all drives.
pub fn recycle_bin_size(drive: Option<char>) -> io::Result<(u64, u64)> {
    #[cfg(windows)]
    return imp::recycle_bin_size(drive);
    #[cfg(not(windows))]
    {
        let _ = drive;
        Err(crate::unsupported())
    }
}

/// Opens Settings › Apps › Installed apps (`ms-settings:appsfeatures`).
pub fn open_apps_settings() -> io::Result<()> {
    #[cfg(windows)]
    return imp::open_apps_settings();
    #[cfg(not(windows))]
    Err(crate::unsupported())
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::fs::FileTypeExt;
    use std::path::{Component, Path, PathBuf};

    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
    use windows_sys::Win32::UI::Shell::{
        ILCreateFromPathW, ILFree, SEE_MASK_INVOKEIDLIST, SEE_MASK_NOASYNC, SHCNE_DELETE,
        SHCNE_RMDIR, SHCNF_PATHW, SHChangeNotify, SHELLEXECUTEINFOW, SHERB_NOCONFIRMATION,
        SHEmptyRecycleBinW, SHOpenFolderAndSelectItems, SHQUERYRBINFO, SHQueryRecycleBinW,
        ShellExecuteExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    use super::NotRecyclable;
    use crate::com::{self, ComInit};
    use crate::paths;
    use crate::sys::{check_hr, hresult_error, wide_z};

    /// COM for shell calls on this thread, intentionally left initialized
    /// (see the module docs).
    fn ensure_com() {
        if let Ok(guard) = ComInit::sta() {
            std::mem::forget(guard);
        }
    }

    fn shell_execute(verb: Option<&str>, file: &[u16], mask: u32) -> io::Result<()> {
        ensure_com();
        let verb = verb.map(wide_z);
        // SAFETY: plain data; all-zero is a valid "unset" for every field.
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = mask;
        info.lpVerb = verb.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());
        info.lpFile = file.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        // SAFETY: all string pointers stay alive until the call returns.
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn open(path: &Path) -> io::Result<()> {
        shell_execute(None, &paths::plain(path)?, SEE_MASK_NOASYNC)
    }

    pub(super) fn properties(path: &Path) -> io::Result<()> {
        shell_execute(
            Some("properties"),
            &paths::plain(path)?,
            SEE_MASK_INVOKEIDLIST,
        )
    }

    pub(super) fn open_apps_settings() -> io::Result<()> {
        shell_execute(
            Some("open"),
            &wide_z("ms-settings:appsfeatures"),
            SEE_MASK_NOASYNC,
        )
    }

    pub(super) fn reveal(path: &Path) -> io::Result<()> {
        ensure_com();
        let plain = paths::plain(path)?;
        // SAFETY: `plain` is NUL-terminated; the PIDL is freed below.
        let pidl = unsafe { ILCreateFromPathW(plain.as_ptr()) };
        if !pidl.is_null() {
            // SAFETY: `pidl` is a valid absolute PIDL; cidl = 0 selects it
            // inside its parent folder.
            let hr = unsafe { SHOpenFolderAndSelectItems(pidl, 0, std::ptr::null(), 0) };
            // SAFETY: allocated by ILCreateFromPathW.
            unsafe { ILFree(pidl) };
            if hr >= 0 {
                return Ok(());
            }
        }
        // Fallback: `explorer.exe /select,"<path>"` (raw: std's quoting
        // would put quotes around the whole switch).
        use std::os::windows::ffi::OsStringExt;
        use std::os::windows::process::CommandExt;
        let mut arg = std::ffi::OsString::from("/select,\"");
        arg.push(std::ffi::OsString::from_wide(&plain[..plain.len() - 1]));
        arg.push("\"");
        std::process::Command::new("explorer.exe")
            .raw_arg(arg)
            .spawn()
            .map(drop)
    }

    /// Holds the clipboard open; closes it on drop.
    struct OpenedClipboard;

    impl OpenedClipboard {
        /// Another process may hold the clipboard for a moment: retry briefly.
        fn open() -> io::Result<OpenedClipboard> {
            for attempt in 0.. {
                // SAFETY: plain FFI call (no owner window).
                if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                    return Ok(OpenedClipboard);
                }
                if attempt == 20 {
                    return Err(io::Error::last_os_error());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            unreachable!()
        }
    }

    impl Drop for OpenedClipboard {
        fn drop(&mut self) {
            // SAFETY: we opened it.
            unsafe { CloseClipboard() };
        }
    }

    pub(super) fn copy_to_clipboard(text: &str) -> io::Result<()> {
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let bytes = wide.len() * 2;
        let _clip = OpenedClipboard::open()?;
        // SAFETY: the clipboard is open.
        if unsafe { EmptyClipboard() } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: plain FFI call.
        let mem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if mem.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `mem` is a fresh movable block of `bytes` bytes.
        let dst = unsafe { GlobalLock(mem) };
        if dst.is_null() {
            let err = io::Error::last_os_error();
            // SAFETY: we still own `mem`.
            unsafe { GlobalFree(mem) };
            return Err(err);
        }
        // SAFETY: `dst` is writable for `bytes` bytes; regions do not overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst.cast::<u16>(), wide.len());
            GlobalUnlock(mem);
        }
        // SAFETY: on success the system owns `mem`; on failure we free it.
        if unsafe { SetClipboardData(u32::from(CF_UNICODETEXT), mem as HANDLE) }.is_null() {
            let err = io::Error::last_os_error();
            // SAFETY: still ours.
            unsafe { GlobalFree(mem) };
            return Err(err);
        }
        Ok(())
    }

    /// The common checks for destructive calls (see the module docs).
    fn validate_targets(items: &[PathBuf]) -> io::Result<()> {
        for p in items {
            let refuse = |why: &str| {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{why}: {}", p.display()),
                ))
            };
            if !p.is_absolute() {
                return refuse("refusing a relative path");
            }
            if p.components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
            {
                return refuse("refusing a path with . or .. components");
            }
            if p.file_name().is_none() {
                return refuse("refusing to delete a root");
            }
        }
        for p in items {
            // NotFound (or PermissionDenied) before anything is touched.
            p.symlink_metadata()?;
        }
        Ok(())
    }

    fn with_path(err: io::Error, path: &Path) -> io::Error {
        io::Error::new(err.kind(), format!("{err}: {}", path.display()))
    }

    pub(super) fn recycle(items: &[PathBuf]) -> io::Result<()> {
        recycle_observed(items).0
    }

    /// [`recycle`] plus what the progress sink saw (for tests/diagnostics).
    pub(super) fn recycle_observed(items: &[PathBuf]) -> (io::Result<()>, com::Observed) {
        if items.is_empty() {
            return (Ok(()), com::Observed::default());
        }
        let report = validate_targets(items).and_then(|()| {
            let plain: Vec<Vec<u16>> = items
                .iter()
                .map(|p| paths::plain(p))
                .collect::<io::Result<_>>()?;
            com::on_sta_thread(|| com::recycle(&plain))
        });
        match report {
            Ok(report) => (judge(items, &report), report.observed),
            Err(e) => (Err(e), com::Observed::default()),
        }
    }

    /// Turns the operation report into the final result. Even when the
    /// shell reports success, every item must really be gone.
    fn judge(items: &[PathBuf], report: &com::RecycleReport) -> io::Result<()> {
        if let Some(path) = &report.observed.refused {
            return Err(io::Error::other(NotRecyclable { path: path.clone() }));
        }
        if let Some((path, hr)) = report.observed.failed.first() {
            let err = hresult_error(*hr);
            return Err(match path {
                Some(p) => with_path(err, p),
                None => err,
            });
        }
        check_hr(report.hr)?;
        if report.aborted {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "the recycle operation was aborted",
            ));
        }
        if let Some(p) = items.iter().find(|p| p.symlink_metadata().is_ok()) {
            return Err(io::Error::other(format!(
                "still present after recycling: {}",
                p.display()
            )));
        }
        Ok(())
    }

    pub(super) fn delete_permanently(items: &[PathBuf]) -> io::Result<()> {
        validate_targets(items)?;
        for p in items {
            let ft = p.symlink_metadata()?.file_type();
            let was_dir = ft.is_dir() || ft.is_symlink_dir();
            let result = if ft.is_symlink_dir() {
                // Junction / directory symlink / mount point: remove the
                // link itself, never what it points to.
                std::fs::remove_dir(p)
            } else if ft.is_dir() {
                std::fs::remove_dir_all(p)
            } else {
                remove_file(p)
            };
            result.map_err(|e| with_path(e, p))?;
            notify_deleted(p, was_dir);
        }
        Ok(())
    }

    /// `remove_file`, clearing the read-only attribute if that is what
    /// stands in the way.
    fn remove_file(p: &Path) -> io::Result<()> {
        match std::fs::remove_file(p) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                let meta = p.symlink_metadata()?;
                let mut perms = meta.permissions();
                if !perms.readonly() {
                    return Err(e);
                }
                #[allow(clippy::permissions_set_readonly_false)]
                // Windows: clears FILE_ATTRIBUTE_READONLY only
                perms.set_readonly(false);
                std::fs::set_permissions(p, perms)?;
                std::fs::remove_file(p)
            }
            other => other,
        }
    }

    /// Tells Explorer windows that `p` is gone.
    fn notify_deleted(p: &Path, was_dir: bool) {
        if let Ok(plain) = paths::plain(p) {
            let event = if was_dir { SHCNE_RMDIR } else { SHCNE_DELETE };
            // SAFETY: `plain` is a NUL-terminated path, as SHCNF_PATHW requires.
            unsafe {
                SHChangeNotify(
                    event as i32,
                    SHCNF_PATHW,
                    plain.as_ptr().cast(),
                    std::ptr::null(),
                )
            };
        }
    }

    fn bin_root(drive: Option<char>) -> io::Result<Option<Vec<u16>>> {
        match drive {
            None => Ok(None),
            Some(c) if c.is_ascii_alphabetic() => {
                Ok(Some(wide_z(&format!(r"{}:\", c.to_ascii_uppercase()))))
            }
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "drive letter must be A-Z",
            )),
        }
    }

    pub(super) fn recycle_bin_size(drive: Option<char>) -> io::Result<(u64, u64)> {
        let root = bin_root(drive)?;
        let mut info = SHQUERYRBINFO {
            cbSize: size_of::<SHQUERYRBINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `root` is NUL-terminated or null; `info.cbSize` is set.
        check_hr(unsafe {
            SHQueryRecycleBinW(
                root.as_ref().map_or(std::ptr::null(), |r| r.as_ptr()),
                &mut info,
            )
        })?;
        Ok((info.i64Size.max(0) as u64, info.i64NumItems.max(0) as u64))
    }

    pub(super) fn empty_recycle_bin(drive: Option<char>) -> io::Result<()> {
        // SHEmptyRecycleBinW fails with E_UNEXPECTED on an empty bin.
        if recycle_bin_size(drive)?.1 == 0 {
            return Ok(());
        }
        let root = bin_root(drive)?;
        ensure_com();
        // SAFETY: `root` is NUL-terminated or null.
        check_hr(unsafe {
            SHEmptyRecycleBinW(
                std::ptr::null_mut(),
                root.as_ref().map_or(std::ptr::null(), |r| r.as_ptr()),
                SHERB_NOCONFIRMATION,
            )
        })
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::test_util::Scratch;

    /// `TSF_DELETE_RECYCLE_IF_POSSIBLE`: the shell's "this goes to the bin".
    const RECYCLE_FLAG: u32 = 0x80;

    /// The one real Recycle Bin move of the suite: a small test-created
    /// file named ringscan-test-*.tmp. It stays in the bin (restoring or
    /// purging single bin items is deliberately not done by tests).
    #[test]
    fn recycle_moves_a_test_file_to_the_bin() {
        let scratch = Scratch::new("recycle");
        let dir = scratch.path();
        let file = dir.join(format!("ringscan-test-{}.tmp", std::process::id()));
        std::fs::write(&file, b"RingScan recycle test file, safe to delete.\r\n").unwrap();
        let before = recycle_bin_size(Some('C')).unwrap();
        let (result, observed) = imp::recycle_observed(std::slice::from_ref(&file));
        let after = recycle_bin_size(Some('C')).unwrap();
        eprintln!(
            "recycle: {result:?}; observed {observed:?}; bin before {before:?} after {after:?}"
        );
        result.unwrap();
        assert!(!file.exists());
        // The sink was consulted and the shell said "recycle" for the item.
        assert!(!observed.pre_delete_flags.is_empty());
        assert!(
            observed
                .pre_delete_flags
                .iter()
                .all(|f| f & RECYCLE_FLAG != 0)
        );
        assert!(after.1 >= 1, "the bin should hold at least our file");
    }

    /// Items on a share without a Recycle Bin must be refused, not deleted.
    /// Needs RINGSCAN_TEST_UNC_DIR = a writable UNC dir inside the project
    /// (e.g. `\\wsl.localhost\<distro>\...\target\win-unc`), passed from WSL with
    /// `WSLENV=RINGSCAN_TEST_UNC_DIR/p`. Skipped when unset.
    #[test]
    fn recycle_refuses_items_without_a_bin() {
        let Some(base) = std::env::var_os("RINGSCAN_TEST_UNC_DIR") else {
            eprintln!("RINGSCAN_TEST_UNC_DIR not set: skipped");
            return;
        };
        let base = PathBuf::from(base);
        assert!(
            base.to_string_lossy().starts_with(r"\\"),
            "expected a UNC path: {}",
            base.display()
        );
        let dir = base.join(format!("ringscan-unc-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        let file = dir.join("ringscan-test-unc.tmp");
        std::fs::write(&file, b"x").unwrap();
        std::fs::write(dir.join("folder").join("inner.tmp"), b"y").unwrap();

        for item in [file.clone(), dir.join("folder")] {
            let (result, observed) = imp::recycle_observed(std::slice::from_ref(&item));
            eprintln!("{}: {result:?}; observed {observed:?}", item.display());
            let err = result.unwrap_err();
            assert!(is_not_recyclable(&err), "{err}");
            assert!(
                item.exists(),
                "{} must survive a refused recycle",
                item.display()
            );
            assert!(
                observed
                    .pre_delete_flags
                    .iter()
                    .all(|f| f & RECYCLE_FLAG == 0)
            );
        }
        assert!(dir.join("folder").join("inner.tmp").exists());
        // Cleanup through the permanent delete (also exercises UNC paths).
        delete_permanently(&[file.clone(), dir.join("folder")]).unwrap();
        assert!(!file.exists() && !dir.join("folder").exists());
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn destructive_calls_validate_every_path_first() {
        let scratch = Scratch::new("validate");
        let dir = scratch.path();
        let keep = dir.join("keep.tmp");
        std::fs::write(&keep, b"keep").unwrap();
        let bad: Vec<PathBuf> = vec![
            "relative.tmp".into(),
            r"C:\".into(),
            r"\\?\C:\".into(),
            r"\\server\share".into(),
            dir.join("..").join("validate").join("keep.tmp"),
            dir.join("does-not-exist.tmp"),
        ];
        for b in bad {
            let list = [keep.clone(), b.clone()];
            let e1 = delete_permanently(&list).unwrap_err();
            let e2 = recycle(&list).unwrap_err();
            eprintln!("{}: {e1} / {e2}", b.display());
            assert!(
                keep.exists(),
                "nothing may be deleted when one path is bad ({})",
                b.display()
            );
        }
        assert!(recycle(&[]).is_ok() && delete_permanently(&[]).is_ok());
        delete_permanently(std::slice::from_ref(&keep)).unwrap();
        assert!(!keep.exists());
    }

    #[test]
    fn recycle_bin_size_queries() {
        let all = recycle_bin_size(None).unwrap();
        let c = recycle_bin_size(Some('c')).unwrap();
        eprintln!("bin all drives {all:?}, C: {c:?}");
        assert!(all.1 >= c.1);
        assert_eq!(
            recycle_bin_size(Some('?')).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
