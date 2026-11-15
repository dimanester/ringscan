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

/// Converts a failed HRESULT into an `io::Error`. HRESULTs that wrap a Win32
/// error (`0x8007xxxx`) become that Win32 error so `kind()` is meaningful;
/// others keep the HRESULT as the raw code (std formats both via
/// `FormatMessageW`).
pub(crate) fn hresult_error(hr: HRESULT) -> io::Error {
    let code = hr as u32;
    if code & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((code & 0xFFFF) as i32)
    } else {
        io::Error::from_raw_os_error(hr)
    }
}

/// `Ok(())` for success HRESULTs (`>= 0`), the error otherwise.
pub(crate) fn check_hr(hr: HRESULT) -> io::Result<()> {
    if hr >= 0 {
        Ok(())
    } else {
        Err(hresult_error(hr))
    }
}
