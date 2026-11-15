//! Drive enumeration: [`drives`].
//!
//! Querying a disconnected network drive or an empty card reader can block
//! for tens of seconds, so every letter is queried on its own short-lived
//! thread and the caller waits at most a deadline (1.5 s by default). Each
//! worker publishes fields as soon as it learns them, so a drive that is
//! still stuck at the deadline is returned with whatever is known (at least
//! its letter and usually its kind); its thread finishes in the background.
//! Workers suppress the "There is no disk in the drive" system dialog.
//!
//! Caveat: Windows cannot finish terminating a process while one of its
//! threads waits on a request inside a file-system driver that does not
//! honour cancellation. A process that exits while a worker is stuck that
//! way lingers (unkillable) until the driver completes the request. During
//! testing this happened once — after a deliberate zero-timeout call that
//! let the process exit mid-query, probably on a virtual cloud drive — and
//! never with the default timeout, where workers have already returned
//! unless the drive itself hangs.
//!
//! Per drive: `GetDriveTypeW` (kind), `GetVolumeInformationW` (label, file
//! system), `GetDiskFreeSpaceExW` (total, free), `GetDiskFreeSpaceW` (cluster
//! size) and `IOCTL_STORAGE_GET_DEVICE_NUMBER` on `\\.\X:` opened with zero
//! access rights (works without elevation) for the physical disk number, so
//! the engine can scan different physical disks in parallel.

use std::time::Duration;

/// What kind of device a drive letter points to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    CdRom,
    Ram,
    Unknown,
}

/// One drive letter. Fields that could not be determined (no media, query
/// timed out, no permission) are empty / 0 / `None`; in particular
/// `total == 0` means "not ready".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveInfo {
    /// Upper-case letter, `'C'`.
    pub letter: char,
    /// Volume label, may be empty.
    pub label: String,
    /// File system name: "NTFS", "ReFS", "FAT32", "exFAT", ...
    pub fs: String,
    pub kind: DriveKind,
    /// Total size visible to the user, bytes.
    pub total: u64,
    /// Free bytes on the volume (not reduced by per-user quotas).
    pub free: u64,
    /// Cluster (allocation unit) size, bytes.
    pub cluster: u32,
    /// Physical disk number (`\\.\PhysicalDriveN`), for fixed/removable
    /// disks whose volume lives on exactly one disk.
    pub disk_number: Option<u32>,
}

impl DriveInfo {
    fn unknown(letter: char) -> DriveInfo {
        DriveInfo {
            letter,
            label: String::new(),
            fs: String::new(),
            kind: DriveKind::Unknown,
            total: 0,
            free: 0,
            cluster: 0,
            disk_number: None,
        }
    }

    /// `"C:\"`, the root directory of the drive.
    pub fn root(&self) -> std::path::PathBuf {
        format!(r"{}:\", self.letter).into()
    }
}

/// All drive letters in use, sorted by letter, waiting at most 1.5 s for
/// slow drives (see the module docs).
pub fn drives() -> Vec<DriveInfo> {
    drives_with_timeout(Duration::from_millis(1500))
}

/// [`drives`] with a custom deadline. Very short deadlines return before
/// the workers are done; see the module docs for why a process should not
/// exit while they still run.
pub fn drives_with_timeout(timeout: Duration) -> Vec<DriveInfo> {
    #[cfg(windows)]
    return imp::drives(timeout);
    #[cfg(not(windows))]
    {
        let _ = (timeout, DriveInfo::unknown);
        Vec::new()
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_DEVICE_DISK, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDiskFreeSpaceExW,
        GetDiskFreeSpaceW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        SEM_FAILCRITICALERRORS, SEM_NOOPENFILEERRORBOX, SetThreadErrorMode,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER,
    };
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
    };

    use super::{DriveInfo, DriveKind};
    use crate::sys::{owned_handle, wide_z};

    type Slot = Arc<Mutex<DriveInfo>>;

    /// Counts finished workers; the caller waits on it with a deadline.
    #[derive(Default)]
    struct Done {
        count: Mutex<usize>,
        cv: Condvar,
    }

    pub(super) fn drives(timeout: Duration) -> Vec<DriveInfo> {
        // SAFETY: plain FFI call.
        let mask = unsafe { GetLogicalDrives() };
        let letters: Vec<char> = (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| char::from(b'A' + i))
            .collect();
        let slots: Vec<Slot> = letters
            .iter()
            .map(|&l| Arc::new(Mutex::new(DriveInfo::unknown(l))))
            .collect();
        let done = Arc::new(Done::default());
        let mut spawned = 0usize;
        for slot in &slots {
            let (slot2, done2) = (Arc::clone(slot), Arc::clone(&done));
            let worker = std::thread::Builder::new()
                .name("ringscan-drive".into())
                .spawn(move || {
                    query(&slot2);
                    *done2.count.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                    done2.cv.notify_all();
                });
            match worker {
                Ok(_) => spawned += 1,
                Err(_) => query(slot), // no thread available: query inline
            }
        }
        let deadline = Instant::now() + timeout;
        let mut count = done.count.lock().unwrap_or_else(|e| e.into_inner());
        while *count < spawned {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            count = done
                .cv
                .wait_timeout(count, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        drop(count);
        slots
            .iter()
            .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .collect()
    }

    fn from_wide_buf(buf: &[u16]) -> String {
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n])
    }

    /// Stores results; the lock is never held across a (possibly hanging)
    /// system call.
    fn update(slot: &Slot, f: impl FnOnce(&mut DriveInfo)) {
        f(&mut slot.lock().unwrap_or_else(|e| e.into_inner()));
    }

    /// Fills `slot` step by step, so a timed-out query still reports what
    /// it learned before blocking.
    fn query(slot: &Slot) {
        let mut letter = 'A';
        update(slot, |d| letter = d.letter);
        // No "insert a disk" dialogs from this thread.
        let mut old = 0u32;
        // SAFETY: plain FFI call with a valid out-pointer.
        unsafe { SetThreadErrorMode(SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX, &mut old) };
        let root = wide_z(&format!(r"{letter}:\"));

        // SAFETY: `root` is NUL-terminated.
        let kind = match unsafe { GetDriveTypeW(root.as_ptr()) } {
            DRIVE_FIXED => DriveKind::Fixed,
            DRIVE_REMOVABLE => DriveKind::Removable,
            DRIVE_REMOTE => DriveKind::Network,
            DRIVE_CDROM => DriveKind::CdRom,
            DRIVE_RAMDISK => DriveKind::Ram,
            _ => DriveKind::Unknown,
        };
        update(slot, |d| d.kind = kind);

        let mut label = [0u16; 261];
        let mut fs = [0u16; 261];
        // SAFETY: buffers are writable for the lengths passed.
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok != 0 {
            update(slot, |d| {
                d.label = from_wide_buf(&label);
                d.fs = from_wide_buf(&fs);
            });
        }

        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: valid out-pointers.
        if unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) } != 0 {
            update(slot, |d| {
                d.total = total;
                d.free = free;
            });
        }

        let (mut spc, mut bps, mut fc, mut tc) = (0u32, 0u32, 0u32, 0u32);
        // SAFETY: valid out-pointers.
        if unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut fc, &mut tc) } != 0 {
            update(slot, |d| d.cluster = spc.saturating_mul(bps));
        }

        if matches!(kind, DriveKind::Fixed | DriveKind::Removable) {
            let number = disk_number(letter);
            update(slot, |d| d.disk_number = number);
        }
    }

    /// Physical disk number of the volume, via a zero-access device handle.
    fn disk_number(letter: char) -> Option<u32> {
        let device = wide_z(&format!(r"\\.\{letter}:"));
        // SAFETY: `device` is NUL-terminated. Zero access rights: allowed for
        // non-elevated callers, enough for this IOCTL.
        let handle = owned_handle(unsafe {
            CreateFileW(
                device.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        })
        .ok()?;
        use std::os::windows::io::AsRawHandle;
        // SAFETY: plain data.
        let mut sdn: STORAGE_DEVICE_NUMBER = unsafe { std::mem::zeroed() };
        let mut returned = 0u32;
        // SAFETY: output buffer is `sdn`, sized correctly; synchronous call.
        let ok = unsafe {
            DeviceIoControl(
                handle.as_raw_handle(),
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                std::ptr::null(),
                0,
                (&raw mut sdn).cast(),
                size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        (ok != 0 && sdn.DeviceType == FILE_DEVICE_DISK).then_some(sdn.DeviceNumber)
    }
}
