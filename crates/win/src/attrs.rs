//! File attribute bits and reparse tags, as plain constants.
//!
//! These are copies of the Win32 values (`winnt.h`), so consumers can decode
//! [`WinEntry::attrs`](crate::WinEntry::attrs) and
//! [`WinEntry::reparse_tag`](crate::WinEntry::reparse_tag) on any target
//! without depending on `windows-sys` themselves. On Windows they are
//! checked against `windows-sys` at compile time.

pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
pub const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0000_0004;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
pub const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x0000_0100;
pub const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x0000_0200;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
pub const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x0000_0800;
/// Data is not available locally (classic HSM / some cloud providers).
pub const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
pub const FILE_ATTRIBUTE_NOT_CONTENT_INDEXED: u32 = 0x0000_2000;
pub const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x0000_4000;
/// Opening the item triggers a download (cloud placeholder directories).
/// Reported only by directory enumeration; the same bit stored on disk
/// means [`FILE_ATTRIBUTE_EA`].
pub const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
/// The file has extended attributes. This is the on-disk meaning of
/// `0x40000` (NTFS `$STANDARD_INFORMATION`, `$FILE_NAME`); `winnt.h` reuses
/// the value for [`FILE_ATTRIBUTE_RECALL_ON_OPEN`], so attributes read from
/// the MFT must not treat it as a cloud bit.
pub const FILE_ATTRIBUTE_EA: u32 = 0x0004_0000;
/// "Always keep on this device" (OneDrive).
pub const FILE_ATTRIBUTE_PINNED: u32 = 0x0008_0000;
/// "Free up space" requested (OneDrive).
pub const FILE_ATTRIBUTE_UNPINNED: u32 = 0x0010_0000;
/// Reading the data triggers a download: the file is an online-only
/// placeholder and occupies (almost) no local space.
pub const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// Junctions and volume mount points.
pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// Symbolic links (file or directory).
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// Symbolic links created by WSL (`ntifs.h`; not in windows-sys).
pub const IO_REPARSE_TAG_LX_SYMLINK: u32 = 0xA000_001D;
/// Windows Overlay Filter: CompactOS / `compact /exe` compressed files.
pub const IO_REPARSE_TAG_WOF: u32 = 0x8000_0017;
/// Data deduplication.
pub const IO_REPARSE_TAG_DEDUP: u32 = 0x8000_0013;
/// Store app execution aliases (`%LOCALAPPDATA%\Microsoft\WindowsApps`).
pub const IO_REPARSE_TAG_APPEXECLINK: u32 = 0x8000_001B;
/// Cloud Files placeholders (OneDrive & co.); the tag has 16 variants
/// `0x9000_X01A`, see [`is_cloud_tag`].
pub const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001A;

/// Bit 29 of a reparse tag: the reparse point is a *name surrogate*, i.e. it
/// stands for another named entity (junction, mount point, symlink). Walkers
/// list such entries but must not descend into them.
pub const fn is_name_surrogate(tag: u32) -> bool {
    tag & 0x2000_0000 != 0
}

/// Whether `tag` is one of the 16 Cloud Files tags (`0x9000_X01A`).
pub const fn is_cloud_tag(tag: u32) -> bool {
    tag & 0xFFFF_0FFF == IO_REPARSE_TAG_CLOUD
}

#[cfg(windows)]
const _: () = {
    use windows_sys::Win32::Storage::FileSystem as fs;
    use windows_sys::Win32::System::SystemServices as ss;
    assert!(FILE_ATTRIBUTE_READONLY == fs::FILE_ATTRIBUTE_READONLY);
    assert!(FILE_ATTRIBUTE_HIDDEN == fs::FILE_ATTRIBUTE_HIDDEN);
    assert!(FILE_ATTRIBUTE_SYSTEM == fs::FILE_ATTRIBUTE_SYSTEM);
    assert!(FILE_ATTRIBUTE_DIRECTORY == fs::FILE_ATTRIBUTE_DIRECTORY);
    assert!(FILE_ATTRIBUTE_ARCHIVE == fs::FILE_ATTRIBUTE_ARCHIVE);
    assert!(FILE_ATTRIBUTE_NORMAL == fs::FILE_ATTRIBUTE_NORMAL);
    assert!(FILE_ATTRIBUTE_TEMPORARY == fs::FILE_ATTRIBUTE_TEMPORARY);
    assert!(FILE_ATTRIBUTE_SPARSE_FILE == fs::FILE_ATTRIBUTE_SPARSE_FILE);
    assert!(FILE_ATTRIBUTE_REPARSE_POINT == fs::FILE_ATTRIBUTE_REPARSE_POINT);
    assert!(FILE_ATTRIBUTE_COMPRESSED == fs::FILE_ATTRIBUTE_COMPRESSED);
    assert!(FILE_ATTRIBUTE_OFFLINE == fs::FILE_ATTRIBUTE_OFFLINE);
    assert!(FILE_ATTRIBUTE_NOT_CONTENT_INDEXED == fs::FILE_ATTRIBUTE_NOT_CONTENT_INDEXED);
    assert!(FILE_ATTRIBUTE_ENCRYPTED == fs::FILE_ATTRIBUTE_ENCRYPTED);
    assert!(FILE_ATTRIBUTE_RECALL_ON_OPEN == fs::FILE_ATTRIBUTE_RECALL_ON_OPEN);
    assert!(FILE_ATTRIBUTE_EA == fs::FILE_ATTRIBUTE_EA);
    assert!(FILE_ATTRIBUTE_PINNED == fs::FILE_ATTRIBUTE_PINNED);
    assert!(FILE_ATTRIBUTE_UNPINNED == fs::FILE_ATTRIBUTE_UNPINNED);
    assert!(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS == fs::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS);
    assert!(IO_REPARSE_TAG_MOUNT_POINT == ss::IO_REPARSE_TAG_MOUNT_POINT);
    assert!(IO_REPARSE_TAG_SYMLINK == ss::IO_REPARSE_TAG_SYMLINK);
    assert!(IO_REPARSE_TAG_WOF == ss::IO_REPARSE_TAG_WOF);
    assert!(IO_REPARSE_TAG_DEDUP == ss::IO_REPARSE_TAG_DEDUP);
    assert!(IO_REPARSE_TAG_APPEXECLINK == ss::IO_REPARSE_TAG_APPEXECLINK);
    assert!(IO_REPARSE_TAG_CLOUD == ss::IO_REPARSE_TAG_CLOUD);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_classes() {
        assert!(is_name_surrogate(IO_REPARSE_TAG_MOUNT_POINT));
        assert!(is_name_surrogate(IO_REPARSE_TAG_SYMLINK));
        assert!(is_name_surrogate(IO_REPARSE_TAG_LX_SYMLINK));
        assert!(!is_name_surrogate(IO_REPARSE_TAG_WOF));
        assert!(!is_name_surrogate(IO_REPARSE_TAG_CLOUD));
        assert!(is_cloud_tag(IO_REPARSE_TAG_CLOUD));
        assert!(is_cloud_tag(0x9000_F01A));
        assert!(is_cloud_tag(0x9000_101A));
        assert!(!is_cloud_tag(IO_REPARSE_TAG_WOF));
        assert!(!is_cloud_tag(0x9001_001A));
    }
}
