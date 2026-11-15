//! Directory listing: [`list_dir`] and [`WinEntry`].
//!
//! ## How a directory is listed
//!
//! The directory is opened once with `CreateFileW(FILE_LIST_DIRECTORY,
//! share read|write|delete, FILE_FLAG_BACKUP_SEMANTICS)` and drained with
//! `GetFileInformationByHandleEx` into a reusable 64 KiB per-thread buffer.
//! Each call returns a batch of variable-length records that carry
//! everything a disk-usage scan needs — `EndOfFile`, `AllocationSize`,
//! attributes, reparse tag, times, file id — so no file is ever opened
//! individually. `FindFirstFileExW` cannot do this: its data has no
//! allocation size.
//!
//! Information classes, fastest first:
//!
//! 1. `FileIdExtdDirectoryInfo` (Windows 8+, NTFS and ReFS): explicit reparse
//!    tag, 128-bit file id, no 8.3 short name.
//! 2. `FileIdBothDirectoryInfo`: the reparse tag sits in `EaSize`. It also
//!    returns the 8.3 short name, which NTFS has to look up for every entry:
//!    measured on `C:\Windows\WinSxS` (34k entries) it takes 46 ms in the
//!    kernel versus 23 ms for class 1 — as fast as `FindFirstFileExW`
//!    with `FindExInfoBasic`. Used only where class 1 is not supported.
//! 3. `FindFirstFileExW(FindExInfoBasic, FIND_FIRST_EX_LARGE_FETCH)` if the
//!    handle-based query is not supported at all (some network file systems
//!    and third-party drivers fail it with `ERROR_INVALID_PARAMETER` & co.).
//!    The allocated size is then estimated as the logical size rounded up to
//!    the volume's cluster size (cached per volume) and `file_id` is 0;
//!    compressed and sparse files are over-estimated.
//!
//! The buffer size made no measurable difference between 64 KiB and 1 MiB on
//! local NTFS, and 64 KiB is the largest single query every SMB server
//! accepts.
//!
//! The records are parsed with explicit little-endian reads at fixed offsets
//! (checked against `windows-sys` at compile time), so the parser is safe
//! Rust, independent of alignment and unit-tested on Linux with synthetic
//! buffers.
//!
//! ## What the sizes mean
//!
//! `logical`/`alloc` come from the directory index, not from the file: NTFS
//! updates that copy when a file is closed, so a file that is being written
//! right now can show a stale size. Directories report the sizes the file
//! system stores for the directory entry itself (0 on NTFS). Small files
//! whose data lives inside their MFT record ("resident", up to ~700 bytes)
//! report `alloc` = `logical` rounded up to 8 bytes, although they occupy
//! no cluster of their own (the MFT reader counts them as 0).

use std::borrow::Cow;
use std::io;
use std::path::{Path, PathBuf};

use crate::attrs::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
    FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT, is_cloud_tag, is_name_surrogate,
};

/// One directory entry as reported by the file system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WinEntry {
    /// File name for display and matching. Always valid UTF-8; if the real
    /// UTF-16 name contains unpaired surrogates (possible on NTFS), they are
    /// replaced by U+FFFD here and the exact name is in [`raw_name`].
    ///
    /// [`raw_name`]: WinEntry::raw_name
    pub name: String,
    /// The exact UTF-16 name, only when `name` is lossy (`None` otherwise, so
    /// it costs nothing in the common case). Use [`WinEntry::path_in`] to
    /// build a path that reaches the real file.
    pub raw_name: Option<Box<[u16]>>,
    pub is_dir: bool,
    /// Logical size (`EndOfFile`), bytes.
    pub logical: u64,
    /// Allocated size on disk (`AllocationSize`), bytes: a multiple of the
    /// cluster size, less than `logical` for compressed and sparse files,
    /// 0 for online-only cloud placeholders. MFT-resident small files report
    /// `logical` rounded up to 8 bytes (not a cluster multiple: that is how
    /// to recognize them).
    pub alloc: u64,
    /// `FILE_ATTRIBUTE_*` bits, see [`crate::attrs`].
    pub attrs: u32,
    /// Reparse tag (`IO_REPARSE_TAG_*`) when `attrs` has
    /// `FILE_ATTRIBUTE_REPARSE_POINT`, else 0.
    pub reparse_tag: u32,
    /// Last write time, Unix seconds (0 = unknown).
    pub mtime: i64,
    /// File system file id (NTFS: MFT reference incl. sequence number;
    /// 0 = unknown, e.g. in the `FindFirstFileExW` fallback).
    pub file_id: u64,
}

impl WinEntry {
    /// The exact name as UTF-16.
    pub fn name_utf16(&self) -> Cow<'_, [u16]> {
        match &self.raw_name {
            Some(raw) => Cow::Borrowed(raw),
            None => Cow::Owned(self.name.encode_utf16().collect()),
        }
    }

    /// The exact name as an `OsString` (Windows only: other targets cannot
    /// represent unpaired surrogates).
    #[cfg(windows)]
    pub fn os_name(&self) -> std::ffi::OsString {
        use std::os::windows::ffi::OsStringExt;
        match &self.raw_name {
            Some(raw) => std::ffi::OsString::from_wide(raw),
            None => self.name.clone().into(),
        }
    }

    /// `dir` joined with the exact name of this entry. Unlike
    /// `dir.join(&entry.name)` this also works for names with unpaired
    /// surrogates.
    pub fn path_in(&self, dir: &Path) -> PathBuf {
        #[cfg(windows)]
        return dir.join(self.os_name());
        #[cfg(not(windows))]
        dir.join(&self.name)
    }

    pub fn is_reparse_point(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    /// Junction, symbolic link or volume mount point (a name-surrogate
    /// reparse point). Walkers list these but must not descend into them;
    /// other reparse points (cloud placeholders, WOF-compressed and dedup
    /// files) are ordinary files/directories and must be followed.
    pub fn is_link(&self) -> bool {
        self.is_reparse_point() && is_name_surrogate(self.reparse_tag)
    }

    /// Cloud Files placeholder (OneDrive & co.) or offline file. For such
    /// files `alloc` is what is actually stored locally (0 if online-only).
    pub fn is_cloud(&self) -> bool {
        self.attrs
            & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
                | FILE_ATTRIBUTE_RECALL_ON_OPEN
                | FILE_ATTRIBUTE_OFFLINE)
            != 0
            || (self.is_reparse_point() && is_cloud_tag(self.reparse_tag))
    }
}

/// Lists the entries of directory `dir` (without `.` and `..`) and appends
/// them to `out`, in file-system order.
///
/// `dir` may be any absolute or relative path, including verbatim
/// (`\\?\…`) and UNC paths; paths longer than `MAX_PATH` work. If `dir` is a
/// junction or symlink it is followed (the walker decides what to descend
/// into, see [`WinEntry::is_link`]).
///
/// On error `out` is left as it was. Typical errors: `PermissionDenied`
/// (ACL), `NotFound`, `NotADirectory`. The function is thread-safe and meant
/// to be called from many walker threads at once.
pub fn list_dir(dir: &Path, out: &mut Vec<WinEntry>) -> io::Result<()> {
    #[cfg(windows)]
    return imp::list_dir(dir, out);
    #[cfg(not(windows))]
    {
        let _ = (dir, out);
        Err(crate::unsupported())
    }
}

// --- FILE_ID_BOTH_DIR_INFO parsing (portable) -------------------------------

/// The two record layouts the handle-based listing understands. Both start
/// with the same fields (offsets below) and records are 8-byte aligned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    /// `FILE_ID_EXTD_DIR_INFO` (`FileIdExtdDirectoryInfo`, Windows 8+):
    /// explicit reparse tag, 128-bit file id, no 8.3 name.
    IdExtd,
    /// `FILE_ID_BOTH_DIR_INFO` (`FileIdBothDirectoryInfo`): reparse tag in
    /// `EaSize`, 64-bit file id, plus the 8.3 name we do not use.
    IdBoth,
}

const OFF_NEXT_ENTRY: usize = 0;
const OFF_LAST_WRITE: usize = 24;
const OFF_END_OF_FILE: usize = 40;
const OFF_ALLOCATION: usize = 48;
const OFF_ATTRIBUTES: usize = 56;
const OFF_NAME_LENGTH: usize = 60;

impl Layout {
    /// Offset of the reparse tag (IdBoth: `EaSize` doubles as the tag).
    const fn tag(self) -> usize {
        match self {
            Layout::IdExtd => 68,
            Layout::IdBoth => 64,
        }
    }

    const fn file_id(self) -> usize {
        match self {
            Layout::IdExtd => 72,
            Layout::IdBoth => 96,
        }
    }

    const fn name(self) -> usize {
        match self {
            Layout::IdExtd => 88,
            Layout::IdBoth => 104,
        }
    }

    /// The file id as u64: IdBoth has 64 bits; IdExtd has 128, of which
    /// NTFS uses only the low 64 (folded with the high half for ReFS).
    fn read_file_id(self, rec: &[u8]) -> u64 {
        let at = self.file_id();
        match self {
            Layout::IdExtd => read_i64(rec, at) as u64 ^ read_i64(rec, at + 8) as u64,
            Layout::IdBoth => read_i64(rec, at) as u64,
        }
    }
}

#[cfg(windows)]
const _: () = {
    use core::mem::offset_of;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_BOTH_DIR_INFO as B, FILE_ID_EXTD_DIR_INFO as E,
    };
    assert!(
        offset_of!(B, NextEntryOffset) == OFF_NEXT_ENTRY
            && offset_of!(E, NextEntryOffset) == OFF_NEXT_ENTRY
    );
    assert!(
        offset_of!(B, LastWriteTime) == OFF_LAST_WRITE
            && offset_of!(E, LastWriteTime) == OFF_LAST_WRITE
    );
    assert!(
        offset_of!(B, EndOfFile) == OFF_END_OF_FILE && offset_of!(E, EndOfFile) == OFF_END_OF_FILE
    );
    assert!(
        offset_of!(B, AllocationSize) == OFF_ALLOCATION
            && offset_of!(E, AllocationSize) == OFF_ALLOCATION
    );
    assert!(
        offset_of!(B, FileAttributes) == OFF_ATTRIBUTES
            && offset_of!(E, FileAttributes) == OFF_ATTRIBUTES
    );
    assert!(
        offset_of!(B, FileNameLength) == OFF_NAME_LENGTH
            && offset_of!(E, FileNameLength) == OFF_NAME_LENGTH
    );
    assert!(offset_of!(B, EaSize) == Layout::IdBoth.tag());
    assert!(offset_of!(B, FileId) == Layout::IdBoth.file_id());
    assert!(offset_of!(B, FileName) == Layout::IdBoth.name());
    assert!(offset_of!(E, ReparsePointTag) == Layout::IdExtd.tag());
    assert!(offset_of!(E, FileId) == Layout::IdExtd.file_id());
    assert!(offset_of!(E, FileName) == Layout::IdExtd.name());
};

fn read_u32(b: &[u8], at: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(a)
}

fn read_i64(b: &[u8], at: usize) -> i64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    i64::from_le_bytes(a)
}

fn malformed() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "malformed directory information buffer",
    )
}

/// Windows FILETIME (100 ns ticks since 1601) to Unix seconds; 0 stays 0.
pub(crate) fn filetime_to_unix(ft: i64) -> i64 {
    const TICKS_PER_SEC: i64 = 10_000_000;
    const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
    if ft <= 0 {
        0
    } else {
        ft / TICKS_PER_SEC - EPOCH_DIFF_SECS
    }
}

/// Decodes UTF-16 code units into a `String`, plus the exact units when the
/// conversion was lossy (unpaired surrogates).
pub(crate) fn decode_name<I>(units: I) -> (String, Option<Box<[u16]>>)
where
    I: Iterator<Item = u16> + Clone,
{
    if units.clone().all(|u| u < 0x80) {
        return (units.map(|u| char::from(u as u8)).collect(), None);
    }
    let mut lossy = false;
    let name = char::decode_utf16(units.clone())
        .map(|r| {
            r.unwrap_or_else(|_| {
                lossy = true;
                char::REPLACEMENT_CHARACTER
            })
        })
        .collect();
    (name, lossy.then(|| units.collect()))
}

fn is_dot_or_dotdot(name: &[u8]) -> bool {
    matches!(name, [b'.', 0] | [b'.', 0, b'.', 0])
}

/// Parses one buffer filled by `GetFileInformationByHandleEx` with the
/// directory class matching `layout` and appends its entries to `out`.
pub(crate) fn parse_dir_info(
    buf: &[u8],
    layout: Layout,
    out: &mut Vec<WinEntry>,
) -> io::Result<()> {
    let name_at = layout.name();
    let mut pos = 0usize;
    loop {
        let rec = buf
            .get(pos..)
            .filter(|r| r.len() >= name_at)
            .ok_or_else(malformed)?;
        let next = read_u32(rec, OFF_NEXT_ENTRY) as usize;
        let name_len = read_u32(rec, OFF_NAME_LENGTH) as usize;
        if !name_len.is_multiple_of(2) {
            return Err(malformed());
        }
        let name = rec.get(name_at..name_at + name_len).ok_or_else(malformed)?;
        if !is_dot_or_dotdot(name) {
            let (name, raw_name) = decode_name(
                name.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&pair| u16::from_le_bytes(pair)),
            );
            let attrs = read_u32(rec, OFF_ATTRIBUTES);
            let is_reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            out.push(WinEntry {
                name,
                raw_name,
                is_dir: attrs & FILE_ATTRIBUTE_DIRECTORY != 0,
                logical: read_i64(rec, OFF_END_OF_FILE).max(0) as u64,
                alloc: read_i64(rec, OFF_ALLOCATION).max(0) as u64,
                attrs,
                reparse_tag: if is_reparse {
                    read_u32(rec, layout.tag())
                } else {
                    0
                },
                mtime: filetime_to_unix(read_i64(rec, OFF_LAST_WRITE)),
                file_id: layout.read_file_id(rec),
            });
        }
        if next == 0 {
            return Ok(());
        }
        pos = pos.checked_add(next).ok_or_else(malformed)?;
    }
}

/// Allocation estimate for the `FindFirstFileExW` fallback.
pub(crate) fn round_up_to_cluster(logical: u64, cluster: Option<u32>) -> u64 {
    match cluster {
        Some(c) if c > 0 && logical > 0 => logical
            .div_ceil(u64::from(c))
            .checked_mul(u64::from(c))
            .unwrap_or(logical),
        _ => logical,
    }
}

// --- Windows implementation --------------------------------------------------

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::sync::Mutex;

    use windows_sys::Win32::Foundation::{
        ERROR_CALL_NOT_IMPLEMENTED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_FUNCTION,
        ERROR_INVALID_LEVEL, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED,
        GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FIND_FIRST_EX_LARGE_FETCH, FileIdBothDirectoryInfo,
        FileIdBothDirectoryRestartInfo, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo,
        FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
        GetDiskFreeSpaceW, GetFileInformationByHandleEx, GetVolumePathNameW, OPEN_EXISTING,
        WIN32_FIND_DATAW,
    };

    use super::{Layout, WinEntry, decode_name, filetime_to_unix, parse_dir_info};
    use crate::attrs::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};
    use crate::paths;
    use crate::sys::owned_handle;

    /// Per-thread listing buffer size (see the module docs for the choice).
    const BUF_BYTES: usize = 64 * 1024;

    thread_local! {
        /// `u64` elements give the 8-byte alignment the API requires.
        static BUF: RefCell<Box<[u64]>> = RefCell::new(vec![0u64; BUF_BYTES / 8].into_boxed_slice());
    }

    enum Failure {
        /// The handle-based query is not supported here: use FindFirstFileExW.
        Unsupported,
        Io(io::Error),
    }

    pub(super) fn list_dir(dir: &Path, out: &mut Vec<WinEntry>) -> io::Result<()> {
        let path = paths::verbatim(dir)?;
        let start = out.len();
        let result = match by_handle(&path, out) {
            Ok(()) => Ok(()),
            Err(Failure::Io(e)) => Err(e),
            Err(Failure::Unsupported) => {
                out.truncate(start);
                by_find(&path, out)
            }
        };
        if result.is_err() {
            out.truncate(start);
        }
        result
    }

    /// The fast path: one handle, batched `FILE_ID_BOTH_DIR_INFO` queries.
    fn by_handle(path: &[u16], out: &mut Vec<WinEntry>) -> Result<(), Failure> {
        // SAFETY: `path` is NUL-terminated; the other arguments are plain values.
        let handle = owned_handle(unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        })
        .map_err(Failure::Io)?;
        let h = handle.as_raw_handle() as HANDLE;
        BUF.with(|cell| match cell.try_borrow_mut() {
            Ok(mut buf) => drain_fastest(h, &mut buf, out),
            // Re-entrant use cannot happen today; stay correct if it ever does.
            Err(_) => drain_fastest(h, &mut vec![0u64; BUF_BYTES / 8], out),
        })
    }

    /// `FileIdExtdDirectoryInfo` where supported (NTFS, ReFS), else
    /// `FileIdBothDirectoryInfo`. Nothing has been appended to `out` when a
    /// class is reported unsupported (that is only decided on the first call).
    fn drain_fastest(h: HANDLE, buf: &mut [u64], out: &mut Vec<WinEntry>) -> Result<(), Failure> {
        match drain(h, buf, Layout::IdExtd, out) {
            Err(Failure::Unsupported) => drain(h, buf, Layout::IdBoth, out),
            other => other,
        }
    }

    fn drain(
        h: HANDLE,
        buf: &mut [u64],
        layout: Layout,
        out: &mut Vec<WinEntry>,
    ) -> Result<(), Failure> {
        let bytes_len = std::mem::size_of_val(buf);
        let (mut class, next_class) = match layout {
            Layout::IdExtd => (FileIdExtdDirectoryRestartInfo, FileIdExtdDirectoryInfo),
            Layout::IdBoth => (FileIdBothDirectoryRestartInfo, FileIdBothDirectoryInfo),
        };
        let mut first = true;
        loop {
            // SAFETY: `buf` is writable for `bytes_len` bytes and 8-byte aligned.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    h,
                    class,
                    buf.as_mut_ptr().cast::<c_void>(),
                    bytes_len as u32,
                )
            };
            if ok == 0 {
                // SAFETY: plain FFI call.
                let err = unsafe { GetLastError() };
                return match err {
                    ERROR_NO_MORE_FILES => Ok(()),
                    // A directory without any entry (not even `.`), e.g. an
                    // empty FAT root.
                    ERROR_FILE_NOT_FOUND if first => Ok(()),
                    ERROR_INVALID_PARAMETER
                    | ERROR_INVALID_FUNCTION
                    | ERROR_NOT_SUPPORTED
                    | ERROR_INVALID_LEVEL
                    | ERROR_CALL_NOT_IMPLEMENTED
                        if first =>
                    {
                        Err(Failure::Unsupported)
                    }
                    _ => Err(Failure::Io(io::Error::from_raw_os_error(err as i32))),
                };
            }
            // SAFETY: viewing initialized `u64`s as bytes is always valid.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), bytes_len) };
            parse_dir_info(bytes, layout, out).map_err(Failure::Io)?;
            class = next_class;
            first = false;
        }
    }

    #[cfg(test)]
    pub(super) fn by_layout(
        path: &[u16],
        layout: Layout,
        out: &mut Vec<WinEntry>,
    ) -> io::Result<()> {
        // SAFETY: as in `by_handle`.
        let handle = owned_handle(unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        })?;
        let mut buf = vec![0u64; BUF_BYTES / 8];
        match drain(handle.as_raw_handle() as HANDLE, &mut buf, layout, out) {
            Ok(()) => Ok(()),
            Err(Failure::Io(e)) => Err(e),
            Err(Failure::Unsupported) => Err(io::Error::from(io::ErrorKind::Unsupported)),
        }
    }

    /// Closes a `FindFirstFileExW` handle.
    struct FindHandle(HANDLE);

    impl Drop for FindHandle {
        fn drop(&mut self) {
            // SAFETY: the handle came from a successful FindFirstFileExW.
            unsafe { FindClose(self.0) };
        }
    }

    /// The fallback: `FindFirstFileExW`, allocation estimated from the
    /// cluster size.
    pub(super) fn by_find(path: &[u16], out: &mut Vec<WinEntry>) -> io::Result<()> {
        let dir = &path[..path.len() - 1];
        let mut pattern = dir.to_vec();
        if pattern.last() != Some(&(b'\\' as u16)) {
            pattern.push(b'\\' as u16);
        }
        pattern.extend_from_slice(&[b'*' as u16, 0]);

        // SAFETY: WIN32_FIND_DATAW is plain data.
        let mut fd: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        // SAFETY: `pattern` is NUL-terminated, `fd` is a valid out-pointer.
        let h = unsafe {
            FindFirstFileExW(
                pattern.as_ptr(),
                FindExInfoBasic,
                (&raw mut fd).cast::<c_void>(),
                FindExSearchNameMatch,
                std::ptr::null(),
                FIND_FIRST_EX_LARGE_FETCH,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            // SAFETY: plain FFI call.
            let err = unsafe { GetLastError() };
            return if err == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(err as i32))
            };
        }
        let find = FindHandle(h);
        let cluster = cluster_size(path);
        loop {
            push_find_data(&fd, cluster, out);
            // SAFETY: `find.0` is a live find handle, `fd` a valid out-pointer.
            if unsafe { FindNextFileW(find.0, &mut fd) } == 0 {
                // SAFETY: plain FFI call.
                let err = unsafe { GetLastError() };
                return if err == ERROR_NO_MORE_FILES {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(err as i32))
                };
            }
        }
    }

    fn push_find_data(fd: &WIN32_FIND_DATAW, cluster: Option<u32>, out: &mut Vec<WinEntry>) {
        let len = fd
            .cFileName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(fd.cFileName.len());
        let units = &fd.cFileName[..len];
        if units == [b'.' as u16] || units == [b'.' as u16, b'.' as u16] {
            return;
        }
        let (name, raw_name) = decode_name(units.iter().copied());
        let attrs = fd.dwFileAttributes;
        let logical = (u64::from(fd.nFileSizeHigh) << 32) | u64::from(fd.nFileSizeLow);
        let ft = (i64::from(fd.ftLastWriteTime.dwHighDateTime) << 32)
            | i64::from(fd.ftLastWriteTime.dwLowDateTime);
        let is_reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        out.push(WinEntry {
            name,
            raw_name,
            is_dir: attrs & FILE_ATTRIBUTE_DIRECTORY != 0,
            logical,
            alloc: super::round_up_to_cluster(logical, cluster),
            attrs,
            reparse_tag: if is_reparse { fd.dwReserved0 } else { 0 },
            mtime: filetime_to_unix(ft),
            file_id: 0,
        });
    }

    /// Volume root (from `GetVolumePathNameW`) -> cluster size in bytes.
    static CLUSTERS: Mutex<Vec<(Vec<u16>, Option<u32>)>> = Mutex::new(Vec::new());

    /// Cluster size of the volume holding `path` (NUL-terminated), cached
    /// per volume. `None` if it cannot be determined.
    fn cluster_size(path: &[u16]) -> Option<u32> {
        let mut root = vec![0u16; path.len() + 1];
        // SAFETY: `path` is NUL-terminated, `root` is writable for its length.
        if unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
            return None;
        }
        let n = root.iter().position(|&c| c == 0).unwrap_or(root.len());
        root.truncate(n + 1);
        let cached = |root: &[u16]| {
            let cache = CLUSTERS.lock().unwrap_or_else(|e| e.into_inner());
            cache.iter().find(|(r, _)| r == root).map(|(_, c)| *c)
        };
        if let Some(c) = cached(&root) {
            return c;
        }
        // Not under the lock: this can block for a while on a slow share.
        let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
        // SAFETY: `root` is NUL-terminated, the out-pointers are valid.
        let ok =
            unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut free, &mut total) };
        let cluster = (ok != 0)
            .then(|| spc.saturating_mul(bps))
            .filter(|&c| c > 0);
        let mut cache = CLUSTERS.lock().unwrap_or_else(|e| e.into_inner());
        if !cache.iter().any(|(r, _)| *r == root) {
            cache.push((root, cluster));
        }
        cluster
    }
}

/// Test hook: list `dir` with the `FindFirstFileExW` fallback only.
#[cfg(all(windows, test))]
pub(crate) fn list_dir_fallback(dir: &Path, out: &mut Vec<WinEntry>) -> io::Result<()> {
    let path = crate::paths::verbatim(dir)?;
    imp::by_find(&path, out)
}

/// Test hook: list `dir` with one specific handle-based class.
#[cfg(all(windows, test))]
pub(crate) fn list_dir_layout(
    dir: &Path,
    layout: Layout,
    out: &mut Vec<WinEntry>,
) -> io::Result<()> {
    imp::by_layout(&crate::paths::verbatim(dir)?, layout, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One record of `layout` (8-byte padded): `tag` goes where the layout
    /// keeps the reparse tag; IdExtd gets an unrelated EaSize of 77.
    #[allow(clippy::too_many_arguments)]
    fn record(
        layout: Layout,
        name: &[u16],
        attrs: u32,
        eof: i64,
        alloc: i64,
        tag: u32,
        id: u128,
        mtime: i64,
    ) -> Vec<u8> {
        let n = layout.name();
        let mut r = vec![0u8; n + name.len() * 2];
        r[OFF_LAST_WRITE..OFF_LAST_WRITE + 8].copy_from_slice(&mtime.to_le_bytes());
        r[OFF_END_OF_FILE..OFF_END_OF_FILE + 8].copy_from_slice(&eof.to_le_bytes());
        r[OFF_ALLOCATION..OFF_ALLOCATION + 8].copy_from_slice(&alloc.to_le_bytes());
        r[OFF_ATTRIBUTES..OFF_ATTRIBUTES + 4].copy_from_slice(&attrs.to_le_bytes());
        r[OFF_NAME_LENGTH..OFF_NAME_LENGTH + 4]
            .copy_from_slice(&((name.len() * 2) as u32).to_le_bytes());
        if layout == Layout::IdExtd {
            r[64..68].copy_from_slice(&77u32.to_le_bytes());
        }
        r[layout.tag()..layout.tag() + 4].copy_from_slice(&tag.to_le_bytes());
        let f = layout.file_id();
        match layout {
            Layout::IdExtd => r[f..f + 16].copy_from_slice(&id.to_le_bytes()),
            Layout::IdBoth => r[f..f + 8].copy_from_slice(&(id as u64).to_le_bytes()),
        }
        for (i, u) in name.iter().enumerate() {
            r[n + 2 * i..n + 2 * i + 2].copy_from_slice(&u.to_le_bytes());
        }
        while !r.len().is_multiple_of(8) {
            r.push(0);
        }
        r
    }

    fn chain(records: &[Vec<u8>]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (i, r) in records.iter().enumerate() {
            let mut r = r.clone();
            let next = if i + 1 == records.len() {
                0
            } else {
                r.len() as u32
            };
            r[..4].copy_from_slice(&next.to_le_bytes());
            buf.extend_from_slice(&r);
        }
        // Stale bytes after the last record must be ignored.
        buf.extend_from_slice(&[0xAB; 64]);
        buf
    }

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    // 2024-01-01T00:00:00Z
    const FT_2024: i64 = 133_485_408_000_000_000;
    const UNIX_2024: i64 = 1_704_067_200;

    #[test]
    fn parses_both_layouts_and_skips_dots() {
        for layout in [Layout::IdExtd, Layout::IdBoth] {
            let buf = chain(&[
                record(layout, &u("."), 0x10, 0, 0, 0, 5, FT_2024),
                record(layout, &u(".."), 0x10, 0, 0, 0, 5, FT_2024),
                record(
                    layout,
                    &u("file.txt"),
                    0x20,
                    4097,
                    8192,
                    0,
                    0x0001_0000_0000_002A,
                    FT_2024,
                ),
                record(
                    layout,
                    &u("Привіт 😀"),
                    0x20 | 0x800,
                    1 << 20,
                    64 << 10,
                    0,
                    7,
                    0,
                ),
                record(
                    layout,
                    &u("junction"),
                    0x10 | 0x400,
                    0,
                    0,
                    0xA000_0003,
                    9,
                    FT_2024,
                ),
            ]);
            let mut out = vec![];
            parse_dir_info(&buf, layout, &mut out).unwrap();
            assert_eq!(out.len(), 3, "{layout:?}");
            let f = &out[0];
            assert_eq!(
                (f.name.as_str(), f.is_dir, f.logical, f.alloc),
                ("file.txt", false, 4097, 8192)
            );
            assert_eq!(
                (f.attrs, f.reparse_tag, f.mtime, f.file_id),
                (0x20, 0, UNIX_2024, 0x0001_0000_0000_002A)
            );
            assert_eq!(f.raw_name, None);
            assert_eq!(out[1].name, "Привіт 😀");
            assert_eq!((out[1].raw_name.as_ref(), out[1].mtime), (None, 0));
            let j = &out[2];
            assert!(
                j.is_dir && j.is_reparse_point() && j.is_link(),
                "{layout:?}"
            );
            assert_eq!(j.reparse_tag, crate::attrs::IO_REPARSE_TAG_MOUNT_POINT);
        }
    }

    #[test]
    fn extd_file_ids_fold_to_64_bits() {
        let buf = chain(&[record(
            Layout::IdExtd,
            &u("x"),
            0x20,
            1,
            8,
            0,
            (3u128 << 64) | 5,
            0,
        )]);
        let mut out = vec![];
        parse_dir_info(&buf, Layout::IdExtd, &mut out).unwrap();
        assert_eq!(out[0].file_id, 5 ^ 3);
    }

    #[test]
    fn unpaired_surrogates_keep_the_exact_name() {
        let name = vec![b'a' as u16, 0xD800, b'b' as u16];
        let buf = chain(&[record(Layout::IdBoth, &name, 0x20, 1, 8, 0, 1, FT_2024)]);
        let mut out = vec![];
        parse_dir_info(&buf, Layout::IdBoth, &mut out).unwrap();
        assert_eq!(out[0].name, "a\u{FFFD}b");
        assert_eq!(out[0].raw_name.as_deref(), Some(name.as_slice()));
        assert_eq!(&*out[0].name_utf16(), name.as_slice());
        assert_eq!(out[0].reparse_tag, 0, "no reparse attribute -> no tag");
    }

    #[test]
    fn rejects_malformed_buffers() {
        let l = Layout::IdExtd;
        let mut out = vec![];
        assert!(parse_dir_info(&[0u8; 50], l, &mut out).is_err());
        // Name running past the end of the buffer.
        let mut r = record(l, &u("abc"), 0x20, 1, 1, 0, 1, 0);
        r[OFF_NAME_LENGTH..OFF_NAME_LENGTH + 4].copy_from_slice(&1000u32.to_le_bytes());
        assert!(parse_dir_info(&r, l, &mut out).is_err());
        // Odd name length.
        let mut r = record(l, &u("abc"), 0x20, 1, 1, 0, 1, 0);
        r[OFF_NAME_LENGTH..OFF_NAME_LENGTH + 4].copy_from_slice(&5u32.to_le_bytes());
        assert!(parse_dir_info(&r, l, &mut out).is_err());
        // NextEntryOffset pointing past the end.
        let mut r = record(l, &u("abc"), 0x20, 1, 1, 0, 1, 0);
        r[..4].copy_from_slice(&4096u32.to_le_bytes());
        assert!(parse_dir_info(&r, l, &mut out).is_err());
        assert!(out.len() <= 1);
    }

    #[test]
    fn helpers() {
        assert_eq!(filetime_to_unix(0), 0);
        assert_eq!(filetime_to_unix(FT_2024), UNIX_2024);
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), 0); // 1970 itself
        assert_eq!(round_up_to_cluster(0, Some(4096)), 0);
        assert_eq!(round_up_to_cluster(1, Some(4096)), 4096);
        assert_eq!(round_up_to_cluster(4096, Some(4096)), 4096);
        assert_eq!(round_up_to_cluster(4097, Some(4096)), 8192);
        assert_eq!(round_up_to_cluster(4097, None), 4097);
        assert_eq!(round_up_to_cluster(u64::MAX, Some(4096)), u64::MAX);
        let (s, raw) = decode_name(u("plain").into_iter());
        assert_eq!((s.as_str(), raw), ("plain", None));
    }

    #[test]
    fn entry_paths() {
        let e = WinEntry {
            name: "x.txt".into(),
            raw_name: None,
            is_dir: false,
            logical: 0,
            alloc: 0,
            attrs: crate::attrs::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
            reparse_tag: 0,
            mtime: 0,
            file_id: 0,
        };
        assert!(e.path_in(Path::new("dir")).ends_with("x.txt"));
        assert!(e.is_cloud() && !e.is_link());
    }
}

#[cfg(all(test, windows))]
mod win_tests {
    use super::*;

    fn sorted(mut v: Vec<WinEntry>) -> Vec<WinEntry> {
        v.sort_by(|a, b| a.name_utf16().cmp(&b.name_utf16()));
        v
    }

    /// Both record layouts must decode to identical entries (on NTFS the
    /// 128-bit id of IdExtd has the 64-bit id of IdBoth in its low half).
    #[test]
    fn both_layouts_agree_on_real_directories() {
        for dir in [r"C:\Windows\System32\drivers", r"C:\Users", r"C:\"] {
            let (mut extd, mut both) = (vec![], vec![]);
            list_dir_layout(Path::new(dir), Layout::IdExtd, &mut extd).unwrap();
            list_dir_layout(Path::new(dir), Layout::IdBoth, &mut both).unwrap();
            let (extd, both) = (sorted(extd), sorted(both));
            assert!(!extd.is_empty());
            assert_eq!(extd, both, "{dir}");
            assert!(extd.iter().all(|e| e.file_id != 0));
        }
    }

    /// The fallback must report the same entries as the handle-based path,
    /// with the allocation estimated from the cluster size.
    #[test]
    fn fallback_agrees_with_handle_listing() {
        for dir in [
            r"C:\Windows\System32\drivers",
            r"C:\Program Files",
            r"C:\Windows\Web",
        ] {
            let (mut fast, mut slow) = (vec![], vec![]);
            list_dir(Path::new(dir), &mut fast).unwrap();
            list_dir_fallback(Path::new(dir), &mut slow).unwrap();
            let (fast, slow) = (sorted(fast), sorted(slow));
            assert!(!fast.is_empty(), "{dir} is empty?");
            assert_eq!(fast.len(), slow.len(), "{dir}");
            for (a, b) in fast.iter().zip(&slow) {
                assert_eq!(
                    (
                        &a.name,
                        a.is_dir,
                        a.logical,
                        a.attrs,
                        a.reparse_tag,
                        a.mtime
                    ),
                    (
                        &b.name,
                        b.is_dir,
                        b.logical,
                        b.attrs,
                        b.reparse_tag,
                        b.mtime
                    ),
                    "{dir}"
                );
                assert_eq!(
                    b.alloc,
                    round_up_to_cluster(b.logical, Some(4096)),
                    "{dir}\\{}",
                    b.name
                );
                assert_eq!(b.file_id, 0);
                assert_ne!(a.file_id, 0);
            }
        }
    }
}
