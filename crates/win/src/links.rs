//! Hard links and on-disk sizes of single files, for bookkeeping after a
//! delete and for files a directory listing reports wrongly.
//!
//! * [`file_id_and_links`]: the file id (the value directory listings
//!   report as `WinEntry::file_id`) and link count of one path.
//! * [`file_by_id`]: sizes and every name of a file found by its id
//!   (`OpenFileById`, then `FindFirstFileNameW`), so that after one link of
//!   a multi-linked file was deleted its size can be moved to a surviving
//!   link.
//! * [`compressed_size`]: `GetCompressedFileSizeW`, the on-disk size of a
//!   WOF-compressed (CompactOS, `compact /exe`) file, whose directory entry
//!   reports an allocation of 0.
//!
//! All paths are passed to Windows in verbatim form.

use std::io;
use std::path::{Path, PathBuf};

/// Sizes and names of one file (see [`file_by_id`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkedFile {
    /// Logical size (`EndOfFile`), bytes.
    pub logical: u64,
    /// Allocated size (`AllocationSize`), bytes.
    pub alloc: u64,
    /// Every name (hard link) of the file as an absolute path (`C:\dir\x`).
    pub names: Vec<PathBuf>,
}

/// File id and number of hard links of the item at `path` (links are not
/// followed: a junction or symlink reports its own id).
pub fn file_id_and_links(path: &Path) -> io::Result<(u64, u32)> {
    #[cfg(windows)]
    return imp::file_id_and_links(path);
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(crate::unsupported())
    }
}

/// The file with id `file_id` on drive `letter`: its sizes and all its
/// names. `NotFound` when the volume has no such file (any more), including
/// a file whose last link is being deleted.
pub fn file_by_id(letter: char, file_id: u64) -> io::Result<LinkedFile> {
    #[cfg(windows)]
    return imp::file_by_id(letter, file_id);
    #[cfg(not(windows))]
    {
        let _ = (letter, file_id);
        Err(crate::unsupported())
    }
}

/// On-disk size of the file at `path` as `GetCompressedFileSizeW` reports
/// it: the compressed size for NTFS-compressed and WOF-compressed files,
/// the allocated size of sparse files, otherwise the logical size.
pub fn compressed_size(path: &Path) -> io::Result<u64> {
    #[cfg(windows)]
    return imp::compressed_size(path);
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(crate::unsupported())
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsString;
    use std::io;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::{AsRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{
        ERROR_HANDLE_EOF, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, GetLastError,
        INVALID_HANDLE_VALUE, NO_ERROR,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0,
        FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileIdType, FileStandardInfo, FindClose,
        FindFirstFileNameW, FindNextFileNameW, GetCompressedFileSizeW, GetFileInformationByHandle,
        GetFileInformationByHandleEx, GetFinalPathNameByHandleW, INVALID_FILE_SIZE, OPEN_EXISTING,
        OpenFileById, VOLUME_NAME_DOS,
    };

    use super::LinkedFile;
    use crate::paths;
    use crate::sys::{owned_handle, wide_z};

    const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    const FLAGS: u32 = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;

    fn open_attrs(path: &[u16]) -> io::Result<OwnedHandle> {
        // SAFETY: `path` is NUL-terminated; the handle is owned below.
        owned_handle(unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_READ_ATTRIBUTES,
                SHARE_ALL,
                std::ptr::null(),
                OPEN_EXISTING,
                FLAGS,
                std::ptr::null_mut(),
            )
        })
    }

    pub(super) fn file_id_and_links(path: &Path) -> io::Result<(u64, u32)> {
        let h = open_attrs(&paths::verbatim(path)?)?;
        // SAFETY: plain data; filled by the call.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `h` is a valid handle, `info` is writable.
        if unsafe { GetFileInformationByHandle(h.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let id = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        Ok((id, info.nNumberOfLinks))
    }

    fn not_found() -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, "no file with this id")
    }

    pub(super) fn file_by_id(letter: char, file_id: u64) -> io::Result<LinkedFile> {
        if !letter.is_ascii_alphabetic() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "drive letter must be A-Z",
            ));
        }
        let root = open_attrs(&wide_z(&format!(r"\\?\{}:\", letter.to_ascii_uppercase())))?;
        let desc = FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 {
                FileId: file_id as i64,
            },
        };
        // SAFETY: `root` is a handle on the volume, `desc` is initialized.
        let h = owned_handle(unsafe {
            OpenFileById(
                root.as_raw_handle(),
                &desc,
                FILE_READ_ATTRIBUTES,
                SHARE_ALL,
                std::ptr::null(),
                FLAGS,
            )
        })
        .map_err(|e| {
            // A free or reused record: "invalid parameter" / "not found".
            if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32)
                || e.kind() == io::ErrorKind::NotFound
            {
                not_found()
            } else {
                e
            }
        })?;
        let mut std_info = FILE_STANDARD_INFO::default();
        // SAFETY: `h` is valid; the buffer matches the class.
        if unsafe {
            GetFileInformationByHandleEx(
                h.as_raw_handle(),
                FileStandardInfo,
                (&raw mut std_info).cast(),
                size_of::<FILE_STANDARD_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if std_info.NumberOfLinks == 0 || std_info.DeletePending {
            return Err(not_found());
        }
        let any_name = final_path(&h)?;
        let names = link_names(&any_name, letter)?;
        Ok(LinkedFile {
            logical: std_info.EndOfFile.max(0) as u64,
            alloc: std_info.AllocationSize.max(0) as u64,
            names,
        })
    }

    /// `GetFinalPathNameByHandleW` (`\\?\C:\...`, NUL-terminated).
    fn final_path(h: &OwnedHandle) -> io::Result<Vec<u16>> {
        let mut buf = vec![0u16; 512];
        loop {
            // SAFETY: `buf` is writable for its length.
            let n = unsafe {
                GetFinalPathNameByHandleW(
                    h.as_raw_handle(),
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
                )
            } as usize;
            if n == 0 {
                return Err(io::Error::last_os_error());
            }
            if n < buf.len() {
                buf.truncate(n);
                buf.push(0);
                return Ok(buf);
            }
            buf.resize(n + 1, 0);
        }
    }

    /// Every name of the file at `path` (`FindFirstFileNameW` reports them
    /// without the drive: `\dir\name`).
    fn link_names(path: &[u16], letter: char) -> io::Result<Vec<PathBuf>> {
        let mut names = Vec::new();
        let mut buf = vec![0u16; 512];
        let push = |names: &mut Vec<PathBuf>, buf: &[u16], len: u32| {
            let len = (len as usize).min(buf.len());
            let name = &buf[..len];
            let name = name.strip_suffix(&[0]).unwrap_or(name);
            let mut p = OsString::from(format!("{}:", letter.to_ascii_uppercase()));
            p.push(OsString::from_wide(name));
            names.push(PathBuf::from(p));
        };
        let find = loop {
            let mut len = buf.len() as u32;
            // SAFETY: `path` is NUL-terminated; `buf` is writable for `len`.
            let h = unsafe { FindFirstFileNameW(path.as_ptr(), 0, &mut len, buf.as_mut_ptr()) };
            if h != INVALID_HANDLE_VALUE {
                push(&mut names, &buf, len);
                break h;
            }
            // SAFETY: plain FFI call.
            if unsafe { GetLastError() } == ERROR_MORE_DATA && len as usize > buf.len() {
                buf.resize(len as usize, 0);
                continue;
            }
            return Err(io::Error::last_os_error());
        };
        let result = loop {
            let mut len = buf.len() as u32;
            // SAFETY: `find` is a valid find handle; `buf` is writable.
            if unsafe { FindNextFileNameW(find, &mut len, buf.as_mut_ptr()) } != 0 {
                push(&mut names, &buf, len);
                continue;
            }
            // SAFETY: plain FFI call.
            match unsafe { GetLastError() } {
                ERROR_HANDLE_EOF => break Ok(()),
                ERROR_MORE_DATA if len as usize > buf.len() => buf.resize(len as usize, 0),
                _ => break Err(io::Error::last_os_error()),
            }
        };
        // SAFETY: opened above, closed once.
        unsafe { FindClose(find) };
        result.map(|()| names)
    }

    pub(super) fn compressed_size(path: &Path) -> io::Result<u64> {
        let p = paths::verbatim(path)?;
        let mut high = 0u32;
        // SAFETY: `p` is NUL-terminated; `high` is writable.
        let low = unsafe { GetCompressedFileSizeW(p.as_ptr(), &mut high) };
        // INVALID_FILE_SIZE is also a valid low half: check the last error.
        // SAFETY: plain FFI call.
        if low == INVALID_FILE_SIZE && unsafe { GetLastError() } != NO_ERROR {
            return Err(io::Error::last_os_error());
        }
        Ok((u64::from(high) << 32) | u64::from(low))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::test_util::Scratch;

    #[test]
    fn hard_links_are_found_by_id() {
        let scratch = Scratch::new("links");
        let dir = scratch.path();
        let a = dir.join("a.bin");
        let b = dir.join("sub").join("b.bin");
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(&a, vec![7u8; 100_000]).unwrap();
        std::fs::hard_link(&a, &b).unwrap();

        let (id, links) = file_id_and_links(&a).unwrap();
        assert_eq!(links, 2);
        assert_eq!(file_id_and_links(&b).unwrap(), (id, 2));
        // The same id a directory listing reports.
        let mut out = Vec::new();
        crate::list_dir(dir, &mut out).unwrap();
        let listed = out.iter().find(|e| e.name == "a.bin").unwrap();
        assert_eq!(listed.file_id, id);

        let letter = a.to_str().unwrap().chars().next().unwrap();
        let f = file_by_id(letter, id).unwrap();
        assert_eq!(f.logical, 100_000);
        assert!(f.alloc >= 100_000, "{f:?}");
        let lower = |p: &Path| p.to_string_lossy().to_lowercase();
        let names: Vec<String> = f.names.iter().map(|p| lower(p)).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        // The temp dir itself may be spelled differently (8.3 names, case).
        assert!(
            names
                .iter()
                .all(|n| n.starts_with(&format!("{}:\\", letter.to_ascii_lowercase())))
        );
        assert!(
            names.iter().any(|n| n.ends_with(r"\links\a.bin")),
            "{names:?}"
        );
        assert!(
            names.iter().any(|n| n.ends_with(r"\links\sub\b.bin")),
            "{names:?}"
        );

        // One link deleted: the other is still found; both gone: NotFound.
        std::fs::remove_file(&a).unwrap();
        let f = file_by_id(letter, id).unwrap();
        assert_eq!(f.names.len(), 1);
        assert!(lower(&f.names[0]).ends_with(r"\sub\b.bin"));
        std::fs::remove_file(&b).unwrap();
        assert_eq!(
            file_by_id(letter, id).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            file_id_and_links(&a).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn compressed_size_of_a_plain_file_is_its_length() {
        let scratch = Scratch::new("cfs");
        let f = scratch.path().join("plain.bin");
        std::fs::write(&f, vec![1u8; 12_345]).unwrap();
        assert_eq!(compressed_size(&f).unwrap(), 12_345);
        assert_eq!(
            compressed_size(&scratch.path().join("missing"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }
}
