//! Conversions between user-facing Windows paths and the two forms Win32
//! wants.
//!
//! * **Verbatim** (`\\?\C:\dir`, `\\?\UNC\server\share\dir`) for file-system
//!   calls (`CreateFileW`, `FindFirstFileExW`, ...). It lifts the 260-char
//!   `MAX_PATH` limit and disables Win32 normalization, which also keeps
//!   names that end in a dot or a space reachable. Because no normalization
//!   happens, the input must already be absolute and clean: a path that is
//!   relative or contains `/`, `.`, `..` or empty components is first run
//!   through `GetFullPathNameW` (the same thing Win32 would do implicitly).
//!   Paths that already are verbatim (`\\?\`, `\??\`) or device paths
//!   (`\\.\`) are passed through untouched.
//! * **Plain** (`C:\dir`, `\\server\share\dir`) for the Shell
//!   (`ShellExecuteExW`, `SHCreateItemFromParsingName`, registry commands),
//!   which does not understand the verbatim prefix.
//!
//! The classification works on UTF-16 slices so it is unit-tested on Linux;
//! only the `GetFullPathNameW` step is Windows-specific.

const SEP: u16 = b'\\' as u16;
const SLASH: u16 = b'/' as u16;
const DOT: u16 = b'.' as u16;
const COLON: u16 = b':' as u16;

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn starts_with_ascii(p: &[u16], prefix: &str) -> bool {
    p.len() >= prefix.len()
        && p.iter()
            .zip(prefix.bytes())
            .all(|(&a, b)| a == u16::from(b))
}

fn is_verbatim(p: &[u16]) -> bool {
    starts_with_ascii(p, r"\\?\") || starts_with_ascii(p, r"\??\")
}

fn is_device(p: &[u16]) -> bool {
    starts_with_ascii(p, r"\\.\")
}

fn is_drive_absolute(p: &[u16]) -> bool {
    p.len() >= 3
        && p[0] < 0x80
        && (p[0] as u8).is_ascii_alphabetic()
        && p[1] == COLON
        && p[2] == SEP
}

/// `\\server...` but not `\\?\` or `\\.\` (checked before this is called).
fn is_unc(p: &[u16]) -> bool {
    p.len() > 2 && p[0] == SEP && p[1] == SEP && p[2] != SEP && p[2] != SLASH
}

/// True when the components after the root are all "real" names: no `/`,
/// no `.` or `..`, no empty component except a single trailing separator.
fn components_clean(rest: &[u16]) -> bool {
    if rest.contains(&SLASH) {
        return false;
    }
    let rest = rest.strip_suffix(&[SEP]).unwrap_or(rest);
    if rest.is_empty() {
        return true;
    }
    rest.split(|&c| c == SEP)
        .all(|c| !c.is_empty() && c != [DOT] && c != [DOT, DOT])
}

/// Verbatim form of `p` (no terminating NUL). `full_path` is the
/// `GetFullPathNameW` step, applied only to paths that are not already
/// absolute and clean.
pub(crate) fn to_verbatim(
    p: &[u16],
    full_path: impl FnOnce(&[u16]) -> std::io::Result<Vec<u16>>,
) -> std::io::Result<Vec<u16>> {
    if is_verbatim(p) || is_device(p) {
        return Ok(p.to_vec());
    }
    if is_drive_absolute(p) && components_clean(&p[3..]) {
        return Ok([w(r"\\?\").as_slice(), p].concat());
    }
    if is_unc(p) && components_clean(&p[2..]) && p[2..].contains(&SEP) {
        return Ok([w(r"\\?\UNC\").as_slice(), &p[2..]].concat());
    }
    let full = full_path(p)?;
    if is_verbatim(&full) || is_device(&full) {
        Ok(full)
    } else if is_drive_absolute(&full) {
        Ok([w(r"\\?\").as_slice(), &full].concat())
    } else if is_unc(&full) {
        Ok([w(r"\\?\UNC\").as_slice(), &full[2..]].concat())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path cannot be converted to an absolute Win32 path",
        ))
    }
}

/// Plain (non-verbatim) form of a path: `\\?\C:\x` -> `C:\x`,
/// `\\?\UNC\s\x` -> `\\s\x`; anything else is returned unchanged.
pub(crate) fn to_plain(p: &[u16]) -> Vec<u16> {
    if starts_with_ascii(p, r"\\?\UNC\") || starts_with_ascii(p, r"\??\UNC\") {
        return [w(r"\\").as_slice(), &p[8..]].concat();
    }
    if is_verbatim(p) && is_drive_absolute(&p[4..]) {
        return p[4..].to_vec();
    }
    p.to_vec()
}

#[cfg(windows)]
pub(crate) use win::*;

#[cfg(windows)]
mod win {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::GetFullPathNameW;

    /// NUL-terminated verbatim form of `p` for file-system calls.
    pub(crate) fn verbatim(p: &Path) -> io::Result<Vec<u16>> {
        let wide: Vec<u16> = p.as_os_str().encode_wide().collect();
        if wide.is_empty() || wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty path or path with NUL",
            ));
        }
        let mut v = super::to_verbatim(&wide, full_path_name)?;
        v.push(0);
        Ok(v)
    }

    /// NUL-terminated plain absolute form of `p` for Shell calls.
    pub(crate) fn plain(p: &Path) -> io::Result<Vec<u16>> {
        let wide: Vec<u16> = p.as_os_str().encode_wide().collect();
        if wide.is_empty() || wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty path or path with NUL",
            ));
        }
        let abs = if super::is_verbatim(&wide) || super::is_device(&wide) {
            wide
        } else {
            full_path_name(&wide)?
        };
        let mut v = super::to_plain(&abs);
        v.push(0);
        Ok(v)
    }

    /// `GetFullPathNameW`: absolute, normalized path (no NUL in or out).
    pub(crate) fn full_path_name(p: &[u16]) -> io::Result<Vec<u16>> {
        let input: Vec<u16> = p.iter().copied().chain(Some(0)).collect();
        let mut buf = vec![0u16; 512];
        loop {
            // SAFETY: `input` is NUL-terminated, `buf` is writable for its length.
            let n = unsafe {
                GetFullPathNameW(
                    input.as_ptr(),
                    buf.len() as u32,
                    buf.as_mut_ptr(),
                    std::ptr::null_mut(),
                )
            } as usize;
            if n == 0 {
                return Err(io::Error::last_os_error());
            }
            if n < buf.len() {
                buf.truncate(n);
                return Ok(buf);
            }
            // Too small: `n` is the required size including the NUL.
            buf.resize(n, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[u16]) -> String {
        String::from_utf16(v).unwrap()
    }

    fn verb(p: &str) -> String {
        s(&to_verbatim(&w(p), |x| Ok([w("<full>").as_slice(), x].concat())).unwrap())
    }

    #[test]
    fn clean_absolute_paths_are_prefixed_without_normalization() {
        assert_eq!(verb(r"C:\"), r"\\?\C:\");
        assert_eq!(verb(r"C:\Windows"), r"\\?\C:\Windows");
        assert_eq!(verb(r"c:\a\b\"), r"\\?\c:\a\b\");
        // Trailing dots/spaces are kept: only verbatim paths can reach them.
        assert_eq!(verb(r"C:\dir\name. "), r"\\?\C:\dir\name. ");
        assert_eq!(verb(r"C:\a\.hidden\..x"), r"\\?\C:\a\.hidden\..x");
        assert_eq!(verb(r"\\server\share\dir"), r"\\?\UNC\server\share\dir");
        assert_eq!(
            verb(r"\\wsl.localhost\Ubuntu\root"),
            r"\\?\UNC\wsl.localhost\Ubuntu\root"
        );
    }

    #[test]
    fn verbatim_and_device_paths_pass_through() {
        assert_eq!(verb(r"\\?\C:\x/y"), r"\\?\C:\x/y");
        assert_eq!(verb(r"\\?\UNC\s\share"), r"\\?\UNC\s\share");
        assert_eq!(verb(r"\??\C:\x"), r"\??\C:\x");
        assert_eq!(verb(r"\\.\C:"), r"\\.\C:");
        assert_eq!(verb(r"\\?\Volume{1234}\"), r"\\?\Volume{1234}\");
    }

    #[test]
    fn unclean_paths_go_through_full_path_name() {
        // A GetFullPathNameW result that is still not absolute -> error...
        assert!(to_verbatim(&w(r"C:\a\..\b"), |_| Ok(w("relative"))).is_err());
        // ...and a real-looking result gets prefixed.
        let fix = |_: &[u16]| Ok(w(r"C:\b"));
        for p in [
            r"C:\a\..\b",
            r"C:\a\.\b",
            r"C:/a/b",
            r"C:\a\\b",
            r"b",
            r"C:b",
            r"\b",
            r"C:",
        ] {
            assert_eq!(s(&to_verbatim(&w(p), fix).unwrap()), r"\\?\C:\b", "{p}");
        }
        let unc = |_: &[u16]| Ok(w(r"\\srv\share\x"));
        assert_eq!(
            s(&to_verbatim(&w("//srv/share/x"), unc).unwrap()),
            r"\\?\UNC\srv\share\x"
        );
        // A bare `\\server` (no share) is not opened as-is.
        assert_eq!(
            s(&to_verbatim(&w(r"\\srv"), unc).unwrap()),
            r"\\?\UNC\srv\share\x"
        );
    }

    #[test]
    fn plain_strips_verbatim_prefixes() {
        assert_eq!(s(&to_plain(&w(r"\\?\C:\x\y"))), r"C:\x\y");
        assert_eq!(s(&to_plain(&w(r"\\?\UNC\srv\share\x"))), r"\\srv\share\x");
        assert_eq!(s(&to_plain(&w(r"C:\x"))), r"C:\x");
        assert_eq!(s(&to_plain(&w(r"\\?\Volume{1}\x"))), r"\\?\Volume{1}\x");
    }
}
