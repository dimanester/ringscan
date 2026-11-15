//! The optional Explorer context-menu entry "Open in RingScan" (per user,
//! no administrator rights needed).
//!
//! [`install`] writes three verbs under `HKCU\Software\Classes`:
//!
//! | key                                      | shown on                  | command            |
//! |------------------------------------------|---------------------------|--------------------|
//! | `Directory\shell\RingScan`               | folders                   | `"exe" "%1"`       |
//! | `Drive\shell\RingScan`                   | drives                    | `"exe" "%1"`       |
//! | `Directory\Background\shell\RingScan`    | empty space in a folder   | `"exe" "%V"`       |
//!
//! Each key's default value is the menu text (the app passes the localized
//! "Відкрити в RingScan" / "Open in RingScan") and `Icon` is the exe.
//! Explorer is told to refresh with `SHChangeNotify(SHCNE_ASSOCCHANGED)`.
//!
//! **Drive roots arrive mangled:** Explorer expands `"%1"` for a drive to
//! `"C:\"`, and under the Windows argv rules `\"` is an escaped quote, so
//! the program receives `C:"`. Pass incoming path arguments through
//! [`path_from_arg`], which turns a trailing `"` back into `\` (`"` cannot
//! occur in a real path).
//!
//! The key name is a parameter (the `*_as` functions) so tests and the
//! app's scripted UI runs can use their own (`RingScanTest`) and never touch
//! the user's real registration; it is validated against a strict
//! allowlist, because an empty or `..`-like name would make [`uninstall`]
//! delete someone else's keys.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

/// The registry key name used by the public functions.
pub const KEY: &str = "RingScan";

/// Whether the folder verb is registered (for the current user).
pub fn is_installed() -> bool {
    is_installed_as(KEY)
}

/// Registers the verbs for `exe` with menu text `text` (overwrites an
/// existing registration, e.g. one pointing to an older location).
pub fn install(exe: &Path, text: &str) -> io::Result<()> {
    install_as(KEY, exe, text)
}

/// Removes the verbs (succeeds if they are not there).
pub fn uninstall() -> io::Result<()> {
    uninstall_as(KEY)
}

/// The executable the installed folder verb launches, if any — lets the app
/// notice that it was moved and re-install.
pub fn installed_exe() -> Option<PathBuf> {
    installed_exe_as(KEY)
}

/// Repairs a path argument received from the verbs: a trailing `"` (from
/// `"C:\"` under argv rules) becomes `\`. Other arguments are unchanged.
pub fn path_from_arg(arg: &OsStr) -> PathBuf {
    match arg.to_str() {
        Some(s) if s.ends_with('"') => {
            let mut fixed = s[..s.len() - 1].to_owned();
            if !fixed.ends_with('\\') {
                fixed.push('\\');
            }
            PathBuf::from(fixed)
        }
        _ => PathBuf::from(arg),
    }
}

/// Registry paths (below HKCU) and argument placeholders of the verbs.
fn verb_keys(key: &str) -> [(String, &'static str); 3] {
    [
        (format!(r"Software\Classes\Directory\shell\{key}"), "%1"),
        (format!(r"Software\Classes\Drive\shell\{key}"), "%1"),
        (
            format!(r"Software\Classes\Directory\Background\shell\{key}"),
            "%V",
        ),
    ]
}

/// Only `[A-Za-z0-9_.-]{1,64}` and not `.`/`..`: no path separators, so the
/// key can never address anything but its own leaf.
fn validate_key(key: &str) -> io::Result<()> {
    let ok = (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        && key != "."
        && key != "..";
    if ok {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid registry key name {key:?}"),
        ))
    }
}

/// [`is_installed`] for another key name (tests and scripted UI runs use
/// `RingScanTest` so they never touch the user's real registration).
pub fn is_installed_as(key: &str) -> bool {
    if validate_key(key).is_err() {
        return false;
    }
    #[cfg(windows)]
    return imp::key_exists(&format!(r"{}\command", verb_keys(key)[0].0));
    #[cfg(not(windows))]
    false
}

/// [`install`] under another key name (see [`is_installed_as`]).
pub fn install_as(key: &str, exe: &Path, text: &str) -> io::Result<()> {
    validate_key(key)?;
    #[cfg(windows)]
    return imp::install(key, exe, text);
    #[cfg(not(windows))]
    {
        let _ = (exe, text);
        Err(crate::unsupported())
    }
}

/// [`uninstall`] for another key name (see [`is_installed_as`]).
pub fn uninstall_as(key: &str) -> io::Result<()> {
    validate_key(key)?;
    #[cfg(windows)]
    return imp::uninstall(key);
    #[cfg(not(windows))]
    Err(crate::unsupported())
}

/// [`installed_exe`] for another key name (see [`is_installed_as`]).
pub fn installed_exe_as(key: &str) -> Option<PathBuf> {
    validate_key(key).ok()?;
    #[cfg(windows)]
    {
        let cmd = imp::get_string(&format!(r"{}\command", verb_keys(key)[0].0))?;
        // `"C:\path\ringscan.exe" "%1"` -> the first quoted part.
        let rest = cmd.strip_prefix('"')?;
        Some(PathBuf::from(&rest[..rest.find('"')?]))
    }
    #[cfg(not(windows))]
    None
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
        RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegGetValueW, RegOpenKeyExW,
        RegSetValueExW,
    };
    use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};

    use super::verb_keys;
    use crate::paths;
    use crate::sys::wide_z;

    fn check(code: WIN32_ERROR) -> io::Result<()> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code as i32))
        }
    }

    /// An open registry key, closed on drop.
    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the key was opened by us.
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn create(path: &str) -> io::Result<Key> {
        let mut hkey: HKEY = std::ptr::null_mut();
        // SAFETY: NUL-terminated path, valid out-pointer.
        check(unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wide_z(path).as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                std::ptr::null(),
                &mut hkey,
                std::ptr::null_mut(),
            )
        })?;
        Ok(Key(hkey))
    }

    /// Sets a REG_SZ value (`name` = None: the default value).
    fn set(key: &Key, name: Option<&str>, value: &[u16]) -> io::Result<()> {
        let name = name.map(wide_z);
        let data: Vec<u16> = value.iter().copied().chain(Some(0)).collect();
        // SAFETY: the data pointer covers `data.len() * 2` bytes.
        check(unsafe {
            RegSetValueExW(
                key.0,
                name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
                0,
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        })
    }

    pub(super) fn key_exists(path: &str) -> bool {
        let mut hkey: HKEY = std::ptr::null_mut();
        // SAFETY: NUL-terminated path, valid out-pointer.
        let ok = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide_z(path).as_ptr(),
                0,
                KEY_READ,
                &mut hkey,
            )
        };
        if ok == ERROR_SUCCESS {
            drop(Key(hkey));
            true
        } else {
            false
        }
    }

    /// Default REG_SZ value of `path`, if present.
    pub(super) fn get_string(path: &str) -> Option<String> {
        let path = wide_z(path);
        let mut buf = vec![0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: `buf` is writable for `bytes` bytes.
        let code = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                std::ptr::null(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if code != ERROR_SUCCESS {
            return None;
        }
        let n = (bytes as usize / 2).min(buf.len());
        let s = &buf[..n];
        let end = s.iter().position(|&c| c == 0).unwrap_or(n);
        Some(String::from_utf16_lossy(&s[..end]))
    }

    fn notify_assoc_changed() {
        // SAFETY: documented use: no items for SHCNE_ASSOCCHANGED.
        unsafe {
            SHChangeNotify(
                SHCNE_ASSOCCHANGED as i32,
                SHCNF_IDLIST,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
    }

    pub(super) fn install(key: &str, exe: &Path, text: &str) -> io::Result<()> {
        // Plain absolute path: Explorer passes the command to CreateProcess.
        let mut exe = paths::plain(exe)?;
        exe.pop(); // NUL
        let text: Vec<u16> = text.encode_utf16().collect();
        let result = (|| {
            for (path, placeholder) in verb_keys(key) {
                let verb = create(&path)?;
                set(&verb, None, &text)?;
                set(&verb, Some("Icon"), &exe)?;
                let mut command: Vec<u16> = "\"".encode_utf16().collect();
                command.extend_from_slice(&exe);
                command.extend(format!("\" \"{placeholder}\"").encode_utf16());
                set(&create(&format!(r"{path}\command"))?, None, &command)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = uninstall(key);
        } else {
            notify_assoc_changed();
        }
        result
    }

    pub(super) fn uninstall(key: &str) -> io::Result<()> {
        let mut first_err = None;
        for (path, _) in verb_keys(key) {
            // SAFETY: NUL-terminated subkey of HKCU; `key` was validated, so
            // this is exactly `...\shell\<key>` and its subtree.
            let code = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide_z(&path).as_ptr()) };
            if code != ERROR_SUCCESS && code != ERROR_FILE_NOT_FOUND && first_err.is_none() {
                first_err = Some(io::Error::from_raw_os_error(code as i32));
            }
        }
        notify_assoc_changed();
        first_err.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_are_validated() {
        for good in ["RingScan", "RingScanTest", "a", "x-y_z.1"] {
            assert!(validate_key(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            r"a\b",
            "a/b",
            "shell\\",
            "has space",
            &"x".repeat(65),
        ] {
            assert!(validate_key(bad).is_err(), "{bad}");
        }
        assert!(!is_installed_as(""));
        assert!(uninstall_as(r"..\..").is_err());
    }

    #[test]
    fn drive_root_arguments_are_repaired() {
        assert_eq!(path_from_arg(OsStr::new("C:\"")), PathBuf::from(r"C:\"));
        assert_eq!(
            path_from_arg(OsStr::new(r#"C:\dir\""#)),
            PathBuf::from(r"C:\dir\")
        );
        assert_eq!(
            path_from_arg(OsStr::new(r"C:\dir")),
            PathBuf::from(r"C:\dir")
        );
        assert_eq!(
            path_from_arg(OsStr::new(r"\\srv\share")),
            PathBuf::from(r"\\srv\share")
        );
    }

    /// Writes and removes the "RingScanTest" verbs (HKCU only).
    #[cfg(windows)]
    #[test]
    fn install_and_uninstall_test_key() {
        let key = "RingScanTest";
        let exe = std::env::current_exe().unwrap();
        let _ = uninstall_as(key);
        assert!(!is_installed_as(key));
        install_as(key, &exe, "Відкрити в RingScanTest").unwrap();
        let result = std::panic::catch_unwind(|| {
            assert!(is_installed_as(key));
            assert_eq!(installed_exe_as(key).as_deref(), Some(exe.as_path()));
            for (path, placeholder) in verb_keys(key) {
                assert_eq!(
                    imp::get_string(&path).as_deref(),
                    Some("Відкрити в RingScanTest")
                );
                let cmd = imp::get_string(&format!(r"{path}\command")).unwrap();
                assert_eq!(cmd, format!("\"{}\" \"{placeholder}\"", exe.display()));
            }
            // The real key is untouched by the test key.
            assert_eq!(
                is_installed(),
                imp::key_exists(r"Software\Classes\Directory\shell\RingScan\command")
            );
        });
        uninstall_as(key).unwrap();
        for (path, _) in verb_keys(key) {
            assert!(!imp::key_exists(&path), "{path} left behind");
        }
        assert!(!is_installed_as(key));
        // Uninstalling twice is fine.
        uninstall_as(key).unwrap();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
