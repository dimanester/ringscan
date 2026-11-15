//! Platform seam: every OS action the UI performs (other than scanning and
//! deleting, which go through the engine's `ScanJob`) passes through this
//! module, a thin layer over `ringscan-win`.
//!
//! * [`drives`] — `ringscan_win::drives` (label, file system, kind, sizes;
//!   never waits more than 1.5 s for a dead drive, so the app calls it on a
//!   background thread);
//! * [`ui_lang_ukrainian`], [`is_elevated`], [`relaunch_elevated`];
//! * [`open`], [`reveal`], [`properties`], [`open_installed_apps`],
//!   [`relaunch_elevated`]: shell calls. The app runs them on the **shell
//!   thread** ([`on_shell_thread`]), never on the UI thread: `ShellExecuteEx`
//!   waits for the shell (`SEE_MASK_NOASYNC`) inside its own modal message
//!   loop, and testing showed that with a Properties sheet open "Show in
//!   Explorer" stalled for ~5 s and "Installed apps" never returned, which
//!   froze every frame while the window still looked responsive. The shell
//!   thread is one long-lived single-threaded COM apartment with a message
//!   loop (what these calls need; COM is initialized by `ringscan-win` on
//!   first use and stays initialized), so a slow or stuck shell call delays
//!   only the shell calls queued behind it;
//! * [`empty_recycle_bin`], [`recycle_bin_size`]: used directly only when
//!   there is no scan job (the job's own `empty_recycle_bin` also refreshes
//!   the Recycle Bin nodes of the tree);
//! * [`context_menu_installed`], [`set_context_menu`]: the "Open in
//!   RingScan" Explorer verb (HKCU, no admin). Scripted UI runs
//!   ([`use_test_registry_key`]) use the `RingScanTest` key instead, so a
//!   test can never change the user's real registration;
//! * [`pick_folder`]: the native folder dialog (`rfd`);
//! * [`open_url`]: an `https://` link in the default browser (the
//!   "Support" dialog), also on the shell thread. The app itself never
//!   makes network requests.
//!
//! Errors are plain `io::Error`s; `ringscan_win::shell::is_not_recyclable`
//! identifies "cannot go to the Recycle Bin" failures.
//!
//! Off Windows the `ringscan-win` calls return `Unsupported` errors, empty
//! lists or `false`, so the crate still builds and its unit tests run on
//! Linux.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub use ringscan_win::DriveKind;

/// One drive letter. Sizes are `None` when the drive is not ready.
#[derive(Clone, Debug, PartialEq)]
pub struct DriveInfo {
    pub letter: char,
    /// Volume label ("" if none).
    pub label: String,
    /// File system name ("NTFS", ...; "" if unknown).
    pub fs: String,
    pub kind: DriveKind,
    pub total: Option<u64>,
    pub free: Option<u64>,
    /// Part of a "This PC" scan (a ready fixed drive on a physical disk,
    /// or NTFS / ReFS; not virtual drives such as Google Drive's).
    pub this_pc: bool,
}

impl From<ringscan_win::DriveInfo> for DriveInfo {
    fn from(d: ringscan_win::DriveInfo) -> DriveInfo {
        // `total == 0` means "not ready" (empty card reader, timed out).
        let ready = d.total > 0;
        let this_pc = ringscan_engine::drive::is_scannable_fixed(&d);
        DriveInfo {
            this_pc,
            letter: d.letter,
            label: d.label,
            fs: d.fs,
            kind: d.kind,
            total: ready.then_some(d.total),
            free: ready.then_some(d.free),
        }
    }
}

pub type Result<T> = io::Result<T>;

/// Registry key of the Explorer verb.
const MENU_KEY: &str = ringscan_win::context_menu::KEY;
/// Registry key used by scripted UI runs instead of [`MENU_KEY`].
const TEST_MENU_KEY: &str = "RingScanTest";
static TEST_KEY: AtomicBool = AtomicBool::new(false);

fn menu_key() -> &'static str {
    if TEST_KEY.load(Ordering::Relaxed) {
        TEST_MENU_KEY
    } else {
        MENU_KEY
    }
}

/// Makes [`context_menu_installed`] / [`set_context_menu`] use the
/// `RingScanTest` registry key (scripted UI tests).
pub fn use_test_registry_key() {
    TEST_KEY.store(true, Ordering::Relaxed);
}

/// Every drive letter in use, with label, file system, kind and sizes.
/// Blocks for up to 1.5 s when a drive does not answer: call it off the UI
/// thread.
pub fn drives() -> Vec<DriveInfo> {
    ringscan_win::drives()
        .into_iter()
        .map(DriveInfo::from)
        .collect()
}

/// Is the Windows display language Ukrainian (`GetUserDefaultUILanguage`)?
pub fn ui_lang_ukrainian() -> bool {
    ringscan_win::ui_lang_ukrainian()
}

/// Does this process run elevated (administrator token)?
pub fn is_elevated() -> bool {
    ringscan_win::is_elevated()
}

/// Restarts the app elevated with `args` (UAC prompt) and returns once the
/// new process has started; the caller then exits. A declined prompt fails
/// with [`is_cancelled`].
pub fn relaunch_elevated(args: &[String]) -> Result<()> {
    ringscan_win::relaunch_elevated(args)
}

/// The user declined a UAC prompt (`ERROR_CANCELLED`).
pub fn is_cancelled(e: &io::Error) -> bool {
    e.raw_os_error() == Some(1223)
}

/// Opens a file with its default application, or a folder in Explorer.
pub fn open(path: &Path) -> Result<()> {
    ringscan_win::shell::open(path)
}

/// Opens Explorer with `path` selected.
pub fn reveal(path: &Path) -> Result<()> {
    ringscan_win::shell::reveal(path)
}

/// Shows the Explorer "Properties" dialog of `path`.
pub fn properties(path: &Path) -> Result<()> {
    ringscan_win::shell::properties(path)
}

/// Empties the Recycle Bin of one drive, or of all drives for `None`
/// (the app has asked the user first).
pub fn empty_recycle_bin(drive: Option<char>) -> Result<()> {
    ringscan_win::shell::empty_recycle_bin(drive)
}

/// `(bytes, items)` in the Recycle Bin of one drive or all drives; `None`
/// when it cannot be queried.
pub fn recycle_bin_size(drive: Option<char>) -> Option<(u64, u64)> {
    ringscan_win::shell::recycle_bin_size(drive).ok()
}

/// Opens Settings › Apps › Installed apps.
pub fn open_installed_apps() -> Result<()> {
    ringscan_win::shell::open_apps_settings()
}

/// Is the "Open in RingScan" Explorer verb registered (HKCU)?
pub fn context_menu_installed() -> bool {
    ringscan_win::context_menu::is_installed_as(menu_key())
}

/// The exe the registered verb launches, if any.
pub fn context_menu_exe() -> Option<PathBuf> {
    ringscan_win::context_menu::installed_exe_as(menu_key())
}

/// Registers (`true`, for this exe, with the localized `text`) or removes
/// the Explorer context-menu verb. HKCU only, no admin needed.
pub fn set_context_menu(enable: bool, text: &str) -> Result<()> {
    let key = menu_key();
    if enable {
        let exe = std::env::current_exe()?;
        ringscan_win::context_menu::install_as(key, &exe, text)
    } else {
        ringscan_win::context_menu::uninstall_as(key)
    }
}

/// Shows the native "Choose folder" dialog and blocks until it closes.
/// Run it on a background thread.
pub fn pick_folder(title: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        rfd::FileDialog::new().set_title(title).pick_folder()
    }
    #[cfg(not(windows))]
    {
        let _ = title;
        None
    }
}

/// Opens an `https://` link in the default browser (`ShellExecuteW`
/// "open"). Run it on the shell thread; anything but an `https://` URL is
/// refused.
pub fn open_url(url: &str) -> Result<()> {
    if !url_allowed(url) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only https:// links are opened",
        ));
    }
    #[cfg(windows)]
    {
        open_url_windows(url)
    }
    #[cfg(not(windows))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "opening links needs Windows",
        ))
    }
}

/// Error payload of [`open_url`] when `ShellExecuteW` fails: its error code
/// (0–32). The app shows it with a localized text ([`link_error_code`]).
#[derive(Debug)]
pub struct LinkError(pub u32);

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "could not open the link (ShellExecute error {})", self.0)
    }
}

impl std::error::Error for LinkError {}

/// The `ShellExecuteW` code of an [`open_url`] error, if it is one.
pub fn link_error_code(e: &io::Error) -> Option<u32> {
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<LinkError>())
        .map(|l| l.0)
}

/// `unix` (seconds since 1970 UTC) as local wall-clock seconds since 1970,
/// for showing dates the way Explorer does: the time zone and the daylight
/// saving rules that applied on that date (`SystemTimeToTzSpecificLocalTimeEx`
/// with the dynamic time zone read once at the first call). Unchanged off
/// Windows or where the conversion fails.
pub fn local_time(unix: i64) -> i64 {
    #[cfg(windows)]
    {
        unix + local_offset::get(unix).unwrap_or(0)
    }
    #[cfg(not(windows))]
    {
        unix
    }
}

#[cfg(windows)]
mod local_offset {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{
        DYNAMIC_TIME_ZONE_INFORMATION, FileTimeToSystemTime, GetDynamicTimeZoneInformation,
        SystemTimeToFileTime, SystemTimeToTzSpecificLocalTimeEx,
    };

    /// Seconds from 1601-01-01 (FILETIME epoch) to 1970-01-01.
    const EPOCH_DIFF: i64 = 11_644_473_600;
    const TICKS: i64 = 10_000_000;
    /// Cached offsets (per minute: zone offsets and their changes fall on
    /// whole minutes); a details list asks for the same rows every frame.
    const CACHE_MAX: usize = 16_384;

    thread_local! {
        static CACHE: RefCell<HashMap<i64, i64>> = RefCell::new(HashMap::new());
    }

    fn zone() -> Option<&'static DYNAMIC_TIME_ZONE_INFORMATION> {
        static ZONE: OnceLock<Option<DYNAMIC_TIME_ZONE_INFORMATION>> = OnceLock::new();
        ZONE.get_or_init(|| {
            // SAFETY: plain C struct, all zero is a valid value.
            let mut tz: DYNAMIC_TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: `tz` is a valid out pointer.
            let id = unsafe { GetDynamicTimeZoneInformation(&mut tz) };
            (id != u32::MAX).then_some(tz)
        })
        .as_ref()
    }

    /// Local time minus UTC at `unix`, in seconds.
    pub fn get(unix: i64) -> Option<i64> {
        let minute = unix.div_euclid(60);
        if let Some(off) = CACHE.with(|c| c.borrow().get(&minute).copied()) {
            return Some(off);
        }
        let off = compute(minute * 60)?;
        CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.len() >= CACHE_MAX {
                c.clear();
            }
            c.insert(minute, off);
        });
        Some(off)
    }

    fn compute(unix: i64) -> Option<i64> {
        let ticks = unix.checked_add(EPOCH_DIFF)?.checked_mul(TICKS)?;
        let ticks = u64::try_from(ticks).ok()?;
        let ft = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        // SAFETY: plain C structs; every pointer is valid for the call.
        unsafe {
            let mut utc: SYSTEMTIME = std::mem::zeroed();
            let mut local: SYSTEMTIME = std::mem::zeroed();
            let mut local_ft: FILETIME = std::mem::zeroed();
            if FileTimeToSystemTime(&ft, &mut utc) == 0
                || SystemTimeToTzSpecificLocalTimeEx(zone()?, &utc, &mut local) == 0
                || SystemTimeToFileTime(&local, &mut local_ft) == 0
            {
                return None;
            }
            let local_ticks =
                (u64::from(local_ft.dwHighDateTime) << 32) | u64::from(local_ft.dwLowDateTime);
            Some((local_ticks as i64 - ticks as i64) / TICKS)
        }
    }
}

/// `https://`, no whitespace or control characters (nothing that could
/// pass as a second argument or a different target).
fn url_allowed(url: &str) -> bool {
    url.len() > "https://".len()
        && url.starts_with("https://")
        && !url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"')
}

#[cfg(windows)]
fn open_url_windows(url: &str) -> Result<()> {
    use std::cell::Cell;

    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
    };
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    thread_local! {
        static COM: Cell<bool> = const { Cell::new(false) };
    }
    // ShellExecute wants COM on the calling thread; the shell thread is a
    // single-threaded apartment that stays initialized (see the module
    // docs), so this only matters when a link is the first shell call.
    COM.with(|done| {
        if !done.get() {
            // SAFETY: plain FFI call. A failure (another apartment model
            // already set) leaves the thread as it was, which is fine too;
            // a success is intentionally never undone.
            unsafe {
                CoInitializeEx(
                    std::ptr::null(),
                    (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
                );
            }
            done.set(true);
        }
    });
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (verb, file) = (wide("open"), wide(url));
    // SAFETY: both strings are NUL-terminated and outlive the call; the
    // other pointers may be null.
    let h = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Values up to 32 are error codes.
    let code = h as usize;
    if code <= 32 {
        return Err(io::Error::other(LinkError(code as u32)));
    }
    Ok(())
}

/// A job for the shell thread.
type Job = Box<dyn FnOnce() + Send>;

/// Runs `job` on the shell thread (see the module docs), in order with the
/// jobs queued before it. Returns false if the thread could not be started
/// (the job did not run).
pub fn on_shell_thread(job: impl FnOnce() + Send + 'static) -> bool {
    shell_thread::submit(Box::new(job))
}

#[cfg(windows)]
mod shell_thread {
    use std::sync::{OnceLock, mpsc};

    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW,
        TranslateMessage, WM_APP, WM_USER,
    };

    use super::Job;

    /// Thread message that says "jobs are waiting".
    const WM_JOBS: u32 = WM_APP + 1;

    struct Worker {
        tx: mpsc::Sender<Job>,
        thread_id: u32,
    }

    static WORKER: OnceLock<Option<Worker>> = OnceLock::new();

    fn start() -> Option<Worker> {
        let (tx, rx) = mpsc::channel::<Job>();
        let (id_tx, id_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("ringscan-shell".into())
            .spawn(move || {
                // SAFETY: plain FFI calls with a valid MSG out-pointer. The
                // first PeekMessage creates the thread's message queue, so
                // posts made after the id is announced cannot get lost.
                let mut msg: MSG = unsafe { std::mem::zeroed() };
                unsafe {
                    PeekMessageW(
                        &mut msg,
                        std::ptr::null_mut(),
                        WM_USER,
                        WM_USER,
                        PM_NOREMOVE,
                    )
                };
                let _ = id_tx.send(unsafe { GetCurrentThreadId() });
                loop {
                    // Jobs first: a modal loop inside a previous job may have
                    // swallowed the WM_JOBS that announced them.
                    while let Ok(job) = rx.try_recv() {
                        job();
                    }
                    // SAFETY: as above.
                    let got = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
                    if got == 0 || got == -1 {
                        return;
                    }
                    if msg.hwnd.is_null() && msg.message == WM_JOBS {
                        continue;
                    }
                    // SAFETY: dispatching a message this thread received.
                    unsafe {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            })
            .ok()?;
        let thread_id = id_rx.recv().ok()?;
        Some(Worker { tx, thread_id })
    }

    pub(super) fn submit(job: Job) -> bool {
        let Some(w) = WORKER.get_or_init(start) else {
            return false;
        };
        if w.tx.send(job).is_err() {
            return false;
        }
        // SAFETY: posting a thread message to a thread with a queue. If the
        // post fails, the job still runs with the next one.
        unsafe { PostThreadMessageW(w.thread_id, WM_JOBS, 0, 0) };
        true
    }
}

#[cfg(not(windows))]
mod shell_thread {
    use super::Job;

    /// No shell off Windows: just run the job on its own thread.
    pub(super) fn submit(job: Job) -> bool {
        std::thread::Builder::new()
            .name("ringscan-shell".into())
            .spawn(job)
            .is_ok()
    }
}

/// What a finished shell call was, for the app to react to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellCall {
    /// Open, reveal, properties, installed apps: report errors only.
    Action,
    /// "Restart as administrator": close the window on success.
    Relaunch,
    /// A link ([`open_url`]): errors get their own localized text.
    OpenLink,
}

/// Snapshot of the slow system facts the sidebar shows, gathered on a
/// background thread ([`SystemInfo::query`] can take up to ~1.5 s).
#[derive(Clone, Debug, Default)]
pub struct SystemInfo {
    pub drives: Vec<DriveInfo>,
    pub recycle: Option<(u64, u64)>,
}

impl SystemInfo {
    pub fn query() -> SystemInfo {
        SystemInfo {
            drives: drives(),
            recycle: recycle_bin_size(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_https_links_open() {
        assert!(url_allowed("https://send.monobank.ua/jar/4w6egVrfe2"));
        for bad in [
            "http://example.com",
            "https://",
            "file:///C:/Windows",
            "C:\\Windows\\notepad.exe",
            "https://a b",
            "https://a\"b",
            "https://a\u{0}",
            "ms-settings:appsfeatures",
        ] {
            assert!(!url_allowed(bad), "{bad}");
            assert!(open_url(bad).is_err(), "{bad}");
            assert_eq!(link_error_code(&open_url(bad).unwrap_err()), None);
        }
    }

    #[test]
    fn link_errors_carry_their_code() {
        let e = io::Error::other(LinkError(31));
        assert_eq!(link_error_code(&e), Some(31));
        assert_eq!(link_error_code(&io::Error::other("x")), None);
    }

    #[test]
    fn local_time_is_a_whole_zone_offset() {
        // Winter and summer of the same year; both within a day of UTC and
        // on a quarter-hour offset (as every time zone is).
        for unix in [1_768_435_200, 1_783_987_200] {
            let off = local_time(unix) - unix;
            assert!(off.abs() <= 14 * 3600, "{off}");
            assert_eq!(off % 900, 0, "{off}");
            // Cached: the same answer for any second of that minute.
            assert_eq!(local_time(unix + 59) - (unix + 59), off);
        }
        #[cfg(not(windows))]
        assert_eq!(local_time(1_768_435_200), 1_768_435_200);
    }

    /// Compares with what Windows itself shows for a summer and a winter
    /// date (`DateTime.ToLocalTime`, the same rules as Explorer). Skipped
    /// where PowerShell is not available.
    #[cfg(windows)]
    #[test]
    fn local_time_matches_windows() {
        for unix in [1_768_435_200i64, 1_783_987_200] {
            let script = format!(
                "[DateTimeOffset]::FromUnixTimeSeconds({unix}).UtcDateTime.ToLocalTime().ToString('yyyy-MM-dd HH:mm')"
            );
            let Ok(out) = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .output()
            else {
                return;
            };
            let shown = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let local = local_time(unix);
            let (y, m, d) = crate::i18n::civil_from_days(local.div_euclid(86_400));
            let secs = local.rem_euclid(86_400);
            let ours = format!("{y}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, secs / 60 % 60);
            assert_eq!(ours, shown, "unix {unix}");
        }
    }

    #[test]
    fn drive_info_conversion_marks_unready_drives() {
        let mut d = ringscan_win::DriveInfo {
            letter: 'E',
            label: String::new(),
            fs: String::new(),
            kind: DriveKind::Removable,
            total: 0,
            free: 0,
            reserved: None,
            cluster: 0,
            disk_number: None,
        };
        let e = DriveInfo::from(d.clone());
        assert_eq!((e.total, e.free, e.this_pc), (None, None, false));
        d.total = 100;
        d.free = 40;
        let e = DriveInfo::from(d.clone());
        assert_eq!((e.total, e.free, e.this_pc), (Some(100), Some(40), false));
        d.kind = DriveKind::Fixed;
        d.fs = "NTFS".into();
        assert!(DriveInfo::from(d).this_pc);
    }
}
