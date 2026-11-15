//! Raw volume access for the MFT reader: [`Volume`].
//!
//! `\\.\C:` is opened with `GENERIC_READ`, share read|write (the volume
//! stays fully usable by everyone else) and `FILE_FLAG_OVERLAPPED`. Every
//! [`Volume::read_at`] is a positional read with its own `OVERLAPPED` and
//! event, so any number of threads can read concurrently through one shared
//! `&Volume` (it is `Send + Sync`) and the device sees real parallel I/O.
//!
//! Volume handles bypass the cache: offsets and lengths must be multiples of
//! [`Volume::sector_size`] (checked, `InvalidInput` otherwise; multiples of
//! 4096 are always fine). The buffer itself may have any alignment —
//! misaligned buffers are served through an aligned bounce buffer, so pass
//! sector-aligned memory if you want to skip that copy.
//!
//! Opening a volume needs administrator rights: without elevation `open`
//! fails with `PermissionDenied`.

use std::io;

/// An open volume (`\\.\X:`) for raw, positional reads.
#[derive(Debug)]
pub struct Volume {
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
    sector: u32,
}

impl Volume {
    /// Opens volume `letter` (`'C'` or `'c'`) for raw reading. Needs
    /// elevation: `PermissionDenied` otherwise; `NotFound` for a letter
    /// with no volume; `InvalidInput` for a non-letter.
    pub fn open(letter: char) -> io::Result<Volume> {
        if !letter.is_ascii_alphabetic() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "drive letter must be A-Z",
            ));
        }
        #[cfg(windows)]
        return imp::open(letter.to_ascii_uppercase());
        #[cfg(not(windows))]
        Err(crate::unsupported())
    }

    /// Logical sector size in bytes (512 or 4096 in practice). Offsets and
    /// lengths passed to [`read_at`](Volume::read_at) must be multiples of it.
    pub fn sector_size(&self) -> u32 {
        self.sector
    }

    /// Reads exactly `buf.len()` bytes at byte offset `off` of the volume.
    ///
    /// `off` and `buf.len()` must be multiples of [`sector_size`]
    /// (`InvalidInput` otherwise). Reading past the end of the volume gives
    /// `UnexpectedEof`. Safe to call from many threads at once.
    ///
    /// [`sector_size`]: Volume::sector_size
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let sector = u64::from(self.sector);
        if !off.is_multiple_of(sector) || !(buf.len() as u64).is_multiple_of(sector) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("volume reads must be aligned to the {sector}-byte sector size"),
            ));
        }
        if buf.is_empty() {
            return Ok(());
        }
        #[cfg(windows)]
        return imp::read_at(self, off, buf);
        #[cfg(not(windows))]
        Err(crate::unsupported())
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::io::{AsRawHandle, OwnedHandle};

    use windows_sys::Win32::Foundation::{
        ERROR_HANDLE_EOF, ERROR_IO_PENDING, GENERIC_READ, GetLastError, HANDLE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDiskFreeSpaceW,
        OPEN_EXISTING, ReadFile,
    };
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::CreateEventW;

    use super::Volume;
    use crate::sys::{owned_handle, wide_z};

    /// Largest single `ReadFile` (a multiple of every sector size).
    const MAX_CHUNK: usize = 16 << 20;
    /// Largest bounce buffer for misaligned destination buffers.
    const MAX_BOUNCE: usize = 1 << 20;

    pub(super) fn open(letter: char) -> io::Result<Volume> {
        let device = wide_z(&format!(r"\\.\{letter}:"));
        // SAFETY: `device` is NUL-terminated; the rest are plain values.
        let handle = owned_handle(unsafe {
            CreateFileW(
                device.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        })?;
        Ok(Volume {
            handle,
            sector: sector_size(letter),
        })
    }

    /// Bytes per sector from `GetDiskFreeSpaceW`, 4096 if unknown (a safe
    /// over-approximation: every sector size divides it).
    fn sector_size(letter: char) -> u32 {
        let root = wide_z(&format!(r"{letter}:\"));
        let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
        // SAFETY: `root` is NUL-terminated; the out-pointers are valid.
        let ok =
            unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut free, &mut total) };
        if ok != 0 && bps.is_power_of_two() {
            bps
        } else {
            4096
        }
    }

    pub(super) fn read_at(vol: &Volume, mut off: u64, buf: &mut [u8]) -> io::Result<()> {
        let sector = vol.sector as usize;
        let h = vol.handle.as_raw_handle() as HANDLE;
        if (buf.as_ptr() as usize).is_multiple_of(sector) {
            for chunk in buf.chunks_mut(MAX_CHUNK) {
                read_exact_at(h, off, chunk)?;
                off += chunk.len() as u64;
            }
        } else {
            // Over-allocate and use the aligned window: no unsafe needed.
            let window = buf.len().min(MAX_BOUNCE);
            let mut bounce = vec![0u8; window + sector];
            let start = bounce.as_ptr().align_offset(sector);
            let aligned = &mut bounce[start..start + window];
            for chunk in buf.chunks_mut(window) {
                let tmp = &mut aligned[..chunk.len()];
                read_exact_at(h, off, tmp)?;
                chunk.copy_from_slice(tmp);
                off += chunk.len() as u64;
            }
        }
        Ok(())
    }

    /// Owned event handle for one overlapped request.
    fn new_event() -> io::Result<OwnedHandle> {
        // SAFETY: plain FFI call; manual-reset, initially non-signaled.
        owned_handle(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) })
    }

    /// One positional read of `buf.len()` (<= MAX_CHUNK) bytes at `off`.
    fn read_exact_at(h: HANDLE, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let event = new_event()?;
        // SAFETY: OVERLAPPED is plain data.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.Anonymous.Anonymous.Offset = off as u32;
        ov.Anonymous.Anonymous.OffsetHigh = (off >> 32) as u32;
        ov.hEvent = event.as_raw_handle() as HANDLE;
        // SAFETY: `buf` and `ov` outlive the request: we always wait for its
        // completion below before returning.
        let ok = unsafe {
            ReadFile(
                h,
                buf.as_mut_ptr(),
                buf.len() as u32,
                std::ptr::null_mut(),
                &mut ov,
            )
        };
        if ok == 0 {
            // SAFETY: plain FFI call.
            let err = unsafe { GetLastError() };
            if err == ERROR_HANDLE_EOF {
                return Err(eof());
            }
            if err != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(err as i32));
            }
        }
        let mut got = 0u32;
        // SAFETY: waits (bWait = TRUE) until the request has completed.
        if unsafe { GetOverlappedResult(h, &ov, &mut got, 1) } == 0 {
            // SAFETY: plain FFI call.
            let err = unsafe { GetLastError() };
            return Err(if err == ERROR_HANDLE_EOF {
                eof()
            } else {
                io::Error::from_raw_os_error(err as i32)
            });
        }
        if got as usize != buf.len() {
            return Err(eof());
        }
        Ok(())
    }

    fn eof() -> io::Error {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "read past the end of the volume",
        )
    }

    /// Test hook: a `Volume` over an ordinary file, to exercise `read_at`
    /// without administrator rights.
    #[cfg(test)]
    pub(super) fn open_file(path: &std::path::Path, sector: u32) -> io::Result<Volume> {
        let p = crate::paths::verbatim(path)?;
        // SAFETY: as in `open`.
        let handle = owned_handle(unsafe {
            CreateFileW(
                p.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        })?;
        Ok(Volume { handle, sector })
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn volume_requires_elevation() {
        match Volume::open('C') {
            Ok(v) => {
                // Elevated run: the boot sector must say NTFS.
                assert!(crate::is_elevated());
                let mut boot = vec![0u8; 4096];
                v.read_at(0, &mut boot).unwrap();
                assert_eq!(&boot[3..11], b"NTFS    ");
            }
            Err(e) => {
                assert!(!crate::is_elevated());
                assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
            }
        }
        assert_eq!(
            Volume::open('1').unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn positional_reads_are_aligned_concurrent_and_exact() {
        let scratch = crate::test_util::Scratch::new("volume");
        let path = scratch.path().join("volume-read.bin");
        let data: Vec<u8> = (0..(3u32 << 20) + 4096)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        std::fs::write(&path, &data).unwrap();
        let vol = imp::open_file(&path, 512).unwrap();
        assert_eq!(vol.sector_size(), 512);

        // Alignment rules.
        let mut b = vec![0u8; 1024];
        assert_eq!(
            vol.read_at(100, &mut b).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            vol.read_at(512, &mut b[..100]).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        vol.read_at(512, &mut b[..0]).unwrap();

        // Misaligned destination buffer -> bounce path, still exact.
        let mut big = vec![0u8; (2 << 20) + 513];
        let dst = &mut big[1..1 + (2 << 20)];
        vol.read_at(4096, dst).unwrap();
        assert!(dst == &data[4096..4096 + (2 << 20)]);

        // Many threads reading different places through one &Volume.
        std::thread::scope(|s| {
            for t in 0..8u64 {
                let (vol, data) = (&vol, &data);
                s.spawn(move || {
                    for i in 0..64u64 {
                        let off = ((t * 64 + i) * 4096) % (3 << 20);
                        let mut b = vec![0u8; 8192];
                        vol.read_at(off, &mut b).unwrap();
                        assert!(b == data[off as usize..off as usize + 8192]);
                    }
                });
            }
        });

        // Past the end.
        let mut b = vec![0u8; 8192];
        assert_eq!(
            vol.read_at(3 << 20, &mut b).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            vol.read_at(64 << 20, &mut b).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        drop(vol);
    }
}
