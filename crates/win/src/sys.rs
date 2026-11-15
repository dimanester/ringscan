//! Small Windows-only helpers shared by the modules: handle ownership and
//! HRESULT -> `io::Error` conversion.

use std::io;
use std::os::windows::io::{FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::core::HRESULT;

/// UTF-16 of `s` followed by a NUL, for `PCWSTR` parameters.
pub(crate) fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Takes ownership of a handle returned by `CreateFileW` & co. On failure
/// (`INVALID_HANDLE_VALUE` or null) returns the thread's last error, so call
/// this immediately after the API call.
pub(crate) fn owned_handle(h: HANDLE) -> io::Result<OwnedHandle> {
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `h` is a valid handle that nobody else owns.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }
}

/// Converts a failed HRESULT into an `io::Error`.
///
/// * `0x8007xxxx` (a wrapped Win32 error) becomes that Win32 error, so
///   `kind()` is meaningful.
/// * Copy-engine errors (`COPYENGINE_E_*`, `0x8027xxxx`, what
///   `IFileOperation` reports per item) are mapped to the equivalent Win32
///   error where one exists (a file in use -> `ERROR_SHARING_VIOLATION`),
///   to `Interrupted` for a cancel, and otherwise to a readable message:
///   `FormatMessageW` has no text for them, so the raw code would show up as
///   "OS Error -2144927705 (FormatMessageW() returned error 317)".
/// * Generic COM codes (`0x8000xxxx`, `0x8004xxxx`) keep the HRESULT as the
///   raw code (std formats them via `FormatMessageW`, which knows them).
/// * Anything else becomes "shell error 0x........".
pub(crate) fn hresult_error(hr: HRESULT) -> io::Error {
    let code = hr as u32;
    match code & 0xFFFF_0000 {
        0x8007_0000 => io::Error::from_raw_os_error((code & 0xFFFF) as i32),
        0x8027_0000 => copy_engine_error(code),
        0x8000_0000 | 0x8004_0000 => io::Error::from_raw_os_error(hr),
        _ => io::Error::other(format!("shell error {code:#010x}")),
    }
}

/// Win32 errors used by [`copy_engine_error`].
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PATH_NOT_FOUND: i32 = 3;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_UNEXP_NET_ERR: i32 = 59;
const ERROR_DISK_FULL: i32 = 112;
const ERROR_DIR_NOT_EMPTY: i32 = 145;
const ERROR_ALREADY_EXISTS: i32 = 183;
const ERROR_FILENAME_EXCED_RANGE: i32 = 206;
const ERROR_ELEVATION_REQUIRED: i32 = 740;

/// `COPYENGINE_E_*` (`0x8027xxxx`, values from `sherrors.h`) -> `io::Error`.
fn copy_engine_error(code: u32) -> io::Error {
    let win32 = match code {
        0x8027_0002 => ERROR_ELEVATION_REQUIRED, // REQUIRES_ELEVATION
        0x8027_001B | 0x8027_001C => ERROR_FILE_NOT_FOUND, // INVALID_FILES_SRC/DEST
        0x8027_001D | 0x8027_001E => ERROR_FILENAME_EXCED_RANGE, // PATH_TOO_DEEP_SRC/DEST
        0x8027_0021 | 0x8027_0022 | 0x8027_003F => ERROR_ACCESS_DENIED, // ACCESS_DENIED_*
        0x8027_0023 | 0x8027_0024 => ERROR_PATH_NOT_FOUND, // PATH_NOT_FOUND_SRC/DEST
        0x8027_0025 | 0x8027_0026 => ERROR_UNEXP_NET_ERR, // NET_DISCONNECT_SRC/DEST
        0x8027_0027 | 0x8027_0028 => ERROR_SHARING_VIOLATION, // SHARING_VIOLATION_SRC/DEST
        0x8027_0029..=0x8027_002C => ERROR_ALREADY_EXISTS, // ALREADY_EXISTS_*
        0x8027_0032 | 0x8027_0033 => ERROR_DISK_FULL, // DISK_FULL(_CLEAN)
        0x8027_003B | 0x8027_003C => ERROR_FILENAME_EXCED_RANGE, // NEW*_NAME_TOO_LONG
        0x8027_003D => ERROR_DIR_NOT_EMPTY,
        0x8027_0000 | 0x8027_0001 => {
            // USER_CANCELLED / CANCELLED
            return io::Error::new(io::ErrorKind::Interrupted, "the operation was cancelled");
        }
        _ => {
            let what = match code {
                0x8027_000D => "the file is too large for the destination",
                0x8027_0035 => "the item cannot be reached or recycled",
                0x8027_0036 => "the item would be deleted permanently",
                0x8027_0037 => "the item is too large for the Recycle Bin",
                0x8027_0038 => "the path is too long for the Recycle Bin",
                0x8027_003A => "the Recycle Bin was not found",
                0x8027_0043 => "items cannot be moved out of the Recycle Bin this way",
                0x8027_0048..=0x8027_004F => "blocked by a data protection policy",
                _ => "the shell could not complete the operation",
            };
            return io::Error::other(format!("{what} (shell error {code:#010x})"));
        }
    };
    io::Error::from_raw_os_error(win32)
}

/// `Ok(())` for success HRESULTs (`>= 0`), the error otherwise.
pub(crate) fn check_hr(hr: HRESULT) -> io::Result<()> {
    if hr >= 0 {
        Ok(())
    } else {
        Err(hresult_error(hr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(code: u32) -> io::Error {
        hresult_error(code as HRESULT)
    }

    #[test]
    fn copy_engine_errors_become_readable() {
        // A file in use (source or destination side).
        assert_eq!(err(0x8027_0027).raw_os_error(), Some(32));
        assert_eq!(err(0x8027_0028).raw_os_error(), Some(32));
        assert_eq!(err(0x8027_0021).kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(err(0x8027_0022).raw_os_error(), Some(5));
        assert_eq!(err(0x8027_0023).kind(), io::ErrorKind::NotFound);
        assert_eq!(err(0x8027_0000).kind(), io::ErrorKind::Interrupted);
        assert_eq!(err(0x8027_0001).kind(), io::ErrorKind::Interrupted);
        // Unknown copy-engine code: readable text with the code, no
        // FormatMessage failure note.
        let unknown = err(0x8027_0099).to_string();
        assert!(unknown.contains("0x80270099"), "{unknown}");
        assert!(!unknown.contains("FormatMessage"), "{unknown}");
        let too_big = err(0x8027_0037).to_string();
        assert!(too_big.contains("Recycle Bin") && too_big.contains("0x80270037"));
        for code in [0x8027_0027u32, 0x8027_0021, 0x8027_0000, 0x8027_0099] {
            let text = err(code).to_string();
            assert!(!text.contains("FormatMessageW"), "{code:#x}: {text}");
        }
    }

    #[test]
    fn other_hresults() {
        // HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED).
        assert_eq!(err(0x8007_0005).raw_os_error(), Some(5));
        // E_FAIL keeps its HRESULT (FormatMessage knows it).
        assert_eq!(err(0x8000_4005).raw_os_error(), Some(0x8000_4005u32 as i32));
        // Some other facility: readable fallback.
        let t = err(0x8033_0001).to_string();
        assert!(t.contains("shell error 0x80330001"), "{t}");
    }
}
