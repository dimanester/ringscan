//! A console for `ringscan.exe --cli` (and `--help`).
//!
//! `ringscan.exe` is a GUI-subsystem program, so Windows gives it no
//! console: started from Explorer it opens no black window, but started
//! from a terminal its standard handles are empty and `println!` goes
//! nowhere. [`attach`] fixes that for the command-line modes:
//!
//! * **Redirected output stays redirected.** Standard handles that already
//!   point to a file, a pipe or a character device such as `NUL` (`> out.txt`,
//!   `| Out-String`, WSL interop pipes) are kept as they are; if both stdout
//!   and stderr are redirected no console is touched at all.
//! * **Otherwise attach to the parent's console**
//!   (`AttachConsole(ATTACH_PARENT_PROCESS)`: cmd, PowerShell, Windows
//!   Terminal), or create one (`AllocConsole`) when there is none, e.g. a
//!   shortcut with `--cli`. Missing handles are then pointed at the console
//!   (`CONOUT$` / `CONIN$`), and handles that were valid are restored, in
//!   case attaching replaced them.
//! * **A console created for us** would vanish with the process, so
//!   [`Console::finish`] waits for Enter before returning.
//!
//! Caveat of the single-exe design: cmd.exe does not wait for GUI-subsystem
//! programs, so its prompt comes back before the report is printed. Use
//! `start /wait ringscan.exe --cli ...`, redirect the output, or run the
//! console build `ringscan-cli.exe` (same CLI). PowerShell waits when the
//! output is piped (`| Out-String`) or with `Start-Process -Wait
//! -NoNewWindow`.
//!
//! Off Windows (tests) the standard handles are always usable: no-op.

/// What [`attach`] did; call [`Console::finish`] before exiting.
pub struct Console {
    /// A new console window was created for this process.
    allocated: bool,
}

/// Makes stdout / stderr usable for command-line output (see the module
/// docs).
pub fn attach() -> Console {
    Console {
        allocated: imp::attach(),
    }
}

impl Console {
    /// Flushes the output and, if the console was created for this process,
    /// waits for Enter so the user can read it.
    pub fn finish(&self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        if self.allocated {
            eprint!("\nPress Enter to close this window...");
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
        }
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{
        GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK, FILE_TYPE_PIPE,
        FILE_TYPE_UNKNOWN, GetFileType, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole, GetStdHandle, STD_ERROR_HANDLE,
        STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    /// A standard handle as found at startup.
    struct Std {
        id: STD_HANDLE,
        handle: HANDLE,
        /// Refers to an open file, pipe, device or console.
        valid: bool,
        /// A file or a pipe: output goes there no matter what.
        redirected: bool,
    }

    fn std_handle(id: STD_HANDLE) -> Std {
        // SAFETY: plain FFI calls on a handle value we only inspect.
        let handle = unsafe { GetStdHandle(id) };
        let (valid, redirected) = if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            (false, false)
        } else {
            let kind = unsafe { GetFileType(handle) };
            let valid = kind != FILE_TYPE_UNKNOWN || unsafe { GetLastError() } == NO_ERROR;
            (valid, kind == FILE_TYPE_DISK || kind == FILE_TYPE_PIPE)
        };
        Std {
            id,
            handle,
            valid,
            redirected,
        }
    }

    /// Opens `CONOUT$` / `CONIN$` of the attached console.
    fn open_console(name: &str) -> HANDLE {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: NUL-terminated name; the handle is kept for the life of
        // the process as a standard handle.
        unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        }
    }

    /// Returns true if a new console was allocated.
    pub(super) fn attach() -> bool {
        let handles = [
            std_handle(STD_INPUT_HANDLE),
            std_handle(STD_OUTPUT_HANDLE),
            std_handle(STD_ERROR_HANDLE),
        ];
        if handles[1].redirected && handles[2].redirected {
            return false;
        }
        // SAFETY: plain FFI calls; failure just means "no console".
        let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0;
        let allocated = !attached && unsafe { AllocConsole() } != 0;
        if !attached && !allocated {
            return false;
        }
        for h in &handles {
            let handle = if h.valid {
                h.handle
            } else if h.id == STD_INPUT_HANDLE {
                open_console("CONIN$")
            } else {
                open_console("CONOUT$")
            };
            if handle != INVALID_HANDLE_VALUE {
                // SAFETY: setting a standard handle to a valid handle.
                unsafe { SetStdHandle(h.id, handle) };
            }
        }
        allocated
    }
}

#[cfg(not(windows))]
mod imp {
    pub(super) fn attach() -> bool {
        false
    }
}
