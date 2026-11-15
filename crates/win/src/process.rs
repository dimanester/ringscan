//! Process-level helpers: elevation, restart as administrator, UI language,
//! error-dialog suppression.

use std::io;

/// Whether the process runs elevated (administrator token, `TokenElevation`).
/// Raw volume access (MFT mode) needs this.
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    return imp::is_elevated();
    #[cfg(not(windows))]
    false
}

/// Starts this executable again with administrator rights (the UAC prompt,
/// `ShellExecuteExW` verb `runas`) and the given arguments. Returns once the
/// new process has been launched; the caller should then exit.
///
/// Arguments are quoted with the rules `CommandLineToArgvW` / Rust's
/// `std::env::args` use to split them back, so they arrive verbatim. If the
/// user declines the prompt the error has `raw_os_error() ==
/// Some(1223)` (`ERROR_CANCELLED`). The UAC prompt is owned by this process's
/// foreground window, if it has one, so it pops up in front instead of
/// blinking in the taskbar.
pub fn relaunch_elevated(args: &[String]) -> io::Result<()> {
    #[cfg(windows)]
    return imp::relaunch_elevated(&quote_args(args));
    #[cfg(not(windows))]
    {
        let _ = quote_args(args);
        Err(crate::unsupported())
    }
}

/// Whether the user's Windows display language is Ukrainian
/// (`GetUserDefaultUILanguage`, primary language `LANG_UKRAINIAN`).
pub fn ui_lang_ukrainian() -> bool {
    #[cfg(windows)]
    return imp::ui_lang_ukrainian();
    #[cfg(not(windows))]
    false
}

/// Turns off the system's critical-error and open-file dialogs for the whole
/// process (`SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX)`),
/// so touching an empty card reader or a dead drive fails with an error
/// instead of popping up "There is no disk in the drive". Call once at
/// startup. No-op on other targets.
pub fn disable_error_dialogs() {
    #[cfg(windows)]
    imp::disable_error_dialogs();
}

/// Builds a command line (without the program) from `args` so that
/// `CommandLineToArgvW` returns exactly `args` again: arguments that are
/// empty or contain whitespace or quotes are quoted, quotes are escaped and
/// backslashes doubled only where they precede a quote.
pub(crate) fn quote_args(args: &[String]) -> String {
    let mut cmd = String::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            cmd.push(' ');
        }
        let needs_quotes = arg.is_empty() || arg.contains([' ', '\t', '\n', '\x0B', '"']);
        if !needs_quotes {
            cmd.push_str(arg);
            continue;
        }
        cmd.push('"');
        let mut backslashes = 0usize;
        for c in arg.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    // n backslashes + quote -> 2n+1 backslashes + quote.
                    cmd.extend(std::iter::repeat_n('\\', backslashes + 1));
                    backslashes = 0;
                }
                _ => backslashes = 0,
            }
            cmd.push(c);
        }
        // Trailing backslashes precede the closing quote: double them.
        cmd.extend(std::iter::repeat_n('\\', backslashes));
        cmd.push('"');
    }
    cmd
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{HANDLE, HWND};
    use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        SEM_FAILCRITICALERRORS, SEM_NOOPENFILEERRORBOX, SetErrorMode,
    };
    use windows_sys::Win32::System::SystemServices::LANG_UKRAINIAN;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
    };
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, SW_SHOWNORMAL,
    };

    use crate::paths;
    use crate::sys::{owned_handle, wide_z};

    pub(super) fn is_elevated() -> bool {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: the pseudo-handle from GetCurrentProcess needs no closing;
        // `token` is a valid out-pointer.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return false;
        }
        let Ok(token) = owned_handle(token) else {
            return false;
        };
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        // SAFETY: the buffer is a TOKEN_ELEVATION of the size passed.
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenElevation,
                (&raw mut elevation).cast::<c_void>(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
        };
        ok != 0 && elevation.TokenIsElevated != 0
    }

    pub(super) fn ui_lang_ukrainian() -> bool {
        // SAFETY: plain FFI call.
        let lang = unsafe { GetUserDefaultUILanguage() };
        u32::from(lang & 0x3FF) == LANG_UKRAINIAN
    }

    pub(super) fn disable_error_dialogs() {
        // SAFETY: plain FFI calls; keep any other bits already set.
        unsafe {
            let old = SetErrorMode(0);
            SetErrorMode(old | SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX);
        }
    }

    /// The foreground window if it belongs to this process (UAC owner).
    fn own_foreground_window() -> HWND {
        // SAFETY: plain FFI calls; a null HWND is handled.
        unsafe {
            let hwnd = GetForegroundWindow();
            let mut pid = 0u32;
            if !hwnd.is_null()
                && GetWindowThreadProcessId(hwnd, &mut pid) != 0
                && pid == GetCurrentProcessId()
            {
                hwnd
            } else {
                std::ptr::null_mut()
            }
        }
    }

    pub(super) fn relaunch_elevated(params: &str) -> io::Result<()> {
        let exe = paths::plain(&std::env::current_exe()?)?;
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|d| paths::plain(&d).ok());
        let verb = wide_z("runas");
        let params = wide_z(params);
        // SAFETY: plain data; all-zero is a valid "unset" for every field.
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOASYNC;
        info.hwnd = own_foreground_window();
        info.lpVerb = verb.as_ptr();
        info.lpFile = exe.as_ptr();
        info.lpParameters = params.as_ptr();
        info.lpDirectory = cwd.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
        info.nShow = SW_SHOWNORMAL;
        // SAFETY: every string pointer stays alive until the call returns.
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference splitter with the documented `CommandLineToArgvW` rules
    /// for arguments after the program name.
    fn split(cmd: &str) -> Vec<String> {
        let mut args = vec![];
        let mut cur = String::new();
        let mut in_arg = false;
        let mut quoted = false;
        let chars: Vec<char> = cmd.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' {
                let n = chars[i..].iter().take_while(|&&c| c == '\\').count();
                if chars.get(i + n) == Some(&'"') {
                    cur.extend(std::iter::repeat_n('\\', n / 2));
                    if n % 2 == 1 {
                        cur.push('"');
                        i += n + 1;
                    } else {
                        i += n;
                    }
                } else {
                    cur.extend(std::iter::repeat_n('\\', n));
                    i += n;
                }
                in_arg = true;
                continue;
            }
            if c == '"' {
                if quoted && chars.get(i + 1) == Some(&'"') {
                    cur.push('"');
                    i += 2;
                    continue;
                }
                quoted = !quoted;
                in_arg = true;
            } else if (c == ' ' || c == '\t') && !quoted {
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            } else {
                cur.push(c);
                in_arg = true;
            }
            i += 1;
        }
        if in_arg {
            args.push(cur);
        }
        args
    }

    #[test]
    fn quoting_round_trips() {
        let cases: Vec<Vec<String>> = vec![
            vec![],
            vec!["--cli".into(), r"C:\Program Files\x".into()],
            vec!["".into(), "a b".into(), "tab\there".into()],
            vec![
                r"C:\".into(),
                r"C:\dir with space\".into(),
                r"\\server\share\".into(),
            ],
            vec![
                r#"say "hi""#.into(),
                r#"back\"slash"#.into(),
                r#"\\"#.into(),
                r#"a\\"b"#.into(),
            ],
            vec!["Відкрити в RingScan".into(), "😀".into(), "plain".into()],
        ];
        for args in cases {
            let cmd = quote_args(&args);
            assert_eq!(split(&cmd), args, "command line: {cmd}");
        }
        assert_eq!(
            quote_args(&["a".into(), r"C:\x y\".into()]),
            r#"a "C:\x y\\""#
        );
    }

    #[cfg(windows)]
    #[test]
    fn process_queries_do_not_crash() {
        // The automated runs are not elevated; just exercise the calls.
        let elevated = is_elevated();
        let uk = ui_lang_ukrainian();
        eprintln!("is_elevated = {elevated}, ui_lang_ukrainian = {uk}");
        disable_error_dialogs();
    }
}
