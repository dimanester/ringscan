//! Directory listers for the walker.
//!
//! * [`WinLister`] implements [`DirLister`] over [`ringscan_win::list_dir`]:
//!   one `GetFileInformationByHandleEx` drain per directory, no per-file
//!   opens. Windows attributes become core flags with [`flags_from_attrs`]
//!   (the MFT builder uses the same function, so both scan methods flag
//!   entries identically).
//! * [`HardLinkFilter`] wraps any lister and marks every repeated file id
//!   HARDLINK, so a walk counts each hard-linked file once.
//!
//! # Attribute mapping
//!
//! * HIDDEN, SYSTEM, SPARSE, COMPRESSED map one to one.
//! * CLOUD: `OFFLINE`, `RECALL_ON_OPEN` or `RECALL_ON_DATA_ACCESS`, or a
//!   Cloud Files reparse tag (`IO_REPARSE_TAG_CLOUD*`). Cloud entries are
//!   ordinary files and directories: they are counted by `AllocationSize`
//!   (0 for online-only files) and cloud directories are descended.
//! * REPARSE (listed, never descended): name-surrogate reparse points only,
//!   i.e. junctions, symbolic links (also WSL's) and volume mount points.
//!   Other reparse points (WOF/CompactOS-compressed files, dedup files, app
//!   execution aliases, ProjFS/WCI placeholders, ...) stand for their own
//!   data and are treated as regular entries. As a safety net a reparse
//!   point whose tag is unknown (0) and that is not a cloud entry also gets
//!   REPARSE, so an unrecognised link can never cause a loop.
//! * RECYCLE: a directory named `$Recycle.Bin` directly at a drive root.
//!
//! # Allocated sizes of MFT-resident files
//!
//! NTFS stores files of up to ~700 bytes (1 KiB records) inside their MFT
//! record. The directory index reports their `AllocationSize` as the length
//! rounded up to 8 bytes although they occupy no cluster of their own
//! (Explorer shows "Size on disk: 0"). When the cluster size is known,
//! [`WinLister`] reports such sizes (not a multiple of the cluster) as 0,
//! which is also what the MFT reader reports, so both scan methods agree.
//!
//! # Names that are not valid Unicode
//!
//! NTFS names may contain unpaired UTF-16 surrogates. Core's [`RawEntry`]
//! only has a `String` name, which `ringscan-win` makes lossy (U+FFFD), so
//! the walker would build a path that does not exist for such a directory.
//! [`WinLister`] remembers the real path of each such directory in a
//! [`LossyPaths`] table and translates every path it is asked to list; the
//! table is shared with the scan job so delete and rescan reach the real
//! items too. The table is empty (and costs one atomic load per listing) on
//! virtually every system.
//!
//! # Hard links in walk mode
//!
//! A directory listing does not report link counts, so [`HardLinkFilter`]
//! remembers every file id it sees, with a hash of the directory whose link
//! carries the size ([`dir_hash`]; 64 shards of a `HashMap<u64, u64>`, about
//! 20–30 bytes per file while the walk runs), and flags the second and
//! later occurrences HARDLINK (zero size, still counted as a file). Which
//! link carries the size depends on the walk order, which is not
//! deterministic with several threads; totals are. Entries without a file id
//! (0, e.g. from the `FindFirstFileExW` fallback) and directories are never
//! flagged. File ids are per volume, so use one filter per volume.
//!
//! After the walk, [`HardLinkFilter::into_links`] keeps only the files that
//! really have several links ([`LinkMap`]: file id → directory of the counted
//! link; a few MB even for `C:\Windows`). The scan job keeps that map per
//! volume, so a later rescan of one folder can seed its own filter with the
//! files whose counted link lies *outside* the folder
//! ([`HardLinkFilter::with_counted`]) and recount the ones whose counted
//! link was inside: each file stays counted exactly once.

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::{BuildHasherDefault, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ringscan_core::flags;
use ringscan_core::parking_lot::Mutex;
use ringscan_core::walk::{DirLister, RawEntry};
use ringscan_win::WinEntry;
use ringscan_win::attrs::{
    FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_OFFLINE,
    FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SPARSE_FILE, FILE_ATTRIBUTE_SYSTEM, is_cloud_tag,
    is_name_surrogate,
};

/// Name of the Recycle Bin folder at the root of a drive.
pub const RECYCLE_BIN_DIR: &str = "$Recycle.Bin";

/// Core [`flags`] for Windows `FILE_ATTRIBUTE_*` bits and a reparse tag
/// (see the [module docs](self) for the rules). Used for both walk and MFT
/// entries.
pub fn flags_from_attrs(attrs: u32, reparse_tag: u32) -> u16 {
    let mut f = 0;
    for (attr, flag) in [
        (FILE_ATTRIBUTE_HIDDEN, flags::HIDDEN),
        (FILE_ATTRIBUTE_SYSTEM, flags::SYSTEM),
        (FILE_ATTRIBUTE_SPARSE_FILE, flags::SPARSE),
        (FILE_ATTRIBUTE_COMPRESSED, flags::COMPRESSED),
    ] {
        if attrs & attr != 0 {
            f |= flag;
        }
    }
    let reparse_point = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    let cloud = attrs
        & (FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
        != 0
        || (reparse_point && is_cloud_tag(reparse_tag));
    if cloud {
        f |= flags::CLOUD;
    } else if reparse_point && (reparse_tag == 0 || is_name_surrogate(reparse_tag)) {
        f |= flags::REPARSE;
    }
    f
}

/// Converts one [`WinEntry`] to a walker entry.
///
/// `cluster` is the volume's cluster size (0 = unknown: sizes are kept as
/// reported); `at_drive_root` says whether the listed directory is a drive
/// root (for the RECYCLE flag). The name is moved out of `e`.
pub fn raw_entry(e: &mut WinEntry, cluster: u64, at_drive_root: bool) -> RawEntry {
    let mut f = flags_from_attrs(e.attrs, e.reparse_tag);
    if at_drive_root && e.is_dir && e.name.eq_ignore_ascii_case(RECYCLE_BIN_DIR) {
        f |= flags::RECYCLE;
    }
    let alloc = if cluster > 0 && !e.alloc.is_multiple_of(cluster) {
        0 // MFT-resident data: no cluster of its own
    } else {
        e.alloc
    };
    RawEntry {
        name: std::mem::take(&mut e.name),
        is_dir: e.is_dir,
        logical: e.logical,
        alloc,
        flags: f,
        mtime: e.mtime,
        file_id: e.file_id,
    }
}

/// The drive letter if `path` is a drive root: `C:`, `C:\`, `c:/`,
/// `\\?\C:\` or `\\.\C:\` (upper-cased). Works on any platform (plain string
/// check), so tests and the CLI can use it on Linux too.
pub fn drive_root_letter(path: &str) -> Option<char> {
    let s = path
        .strip_prefix(r"\\?\")
        .or_else(|| path.strip_prefix(r"\\.\"))
        .unwrap_or(path);
    let s = s.trim_end_matches(['\\', '/']);
    let mut chars = s.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(l), Some(':'), None) if l.is_ascii_alphabetic() => Some(l.to_ascii_uppercase()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Lossy names
// ---------------------------------------------------------------------------

/// Real paths of directories whose names are not valid Unicode, keyed by
/// the lossy path the walker builds for them (see the [module docs](self)).
#[derive(Debug, Default)]
pub struct LossyPaths {
    any: AtomicBool,
    map: Mutex<HashMap<PathBuf, PathBuf>>,
}

impl LossyPaths {
    pub fn new() -> LossyPaths {
        LossyPaths::default()
    }

    /// Records that the lossy path `lossy` stands for the real path `real`.
    pub fn insert(&self, lossy: PathBuf, real: PathBuf) {
        self.map.lock().entry(lossy).or_insert(real);
        self.any.store(true, Ordering::Release);
    }

    /// Number of recorded directories.
    pub fn len(&self) -> usize {
        self.map.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        !self.any.load(Ordering::Acquire)
    }

    /// The real path for `path`: the deepest recorded ancestor (or `path`
    /// itself) is replaced by its real path. Returns `path` unchanged when
    /// nothing applies.
    pub fn translate<'a>(&self, path: &'a Path) -> Cow<'a, Path> {
        if self.is_empty() {
            return Cow::Borrowed(path);
        }
        let map = self.map.lock();
        for anc in path.ancestors() {
            if let Some(real) = map.get(anc) {
                return match path.strip_prefix(anc) {
                    Ok(rest) if !rest.as_os_str().is_empty() => Cow::Owned(real.join(rest)),
                    _ => Cow::Owned(real.clone()),
                };
            }
        }
        Cow::Borrowed(path)
    }
}

// ---------------------------------------------------------------------------
// WinLister
// ---------------------------------------------------------------------------

/// [`DirLister`] over [`ringscan_win::list_dir`] (Windows; on other targets
/// every listing fails with `Unsupported`). See the [module docs](self).
#[derive(Debug, Default)]
pub struct WinLister {
    cluster: u64,
    lossy: std::sync::Arc<LossyPaths>,
}

impl WinLister {
    /// A lister for a volume with the given cluster size (0 = unknown) that
    /// records lossy directory names in `lossy`.
    pub fn new(cluster: u32, lossy: std::sync::Arc<LossyPaths>) -> WinLister {
        WinLister {
            cluster: u64::from(cluster),
            lossy,
        }
    }

    /// The lossy-name table this lister fills.
    pub fn lossy_paths(&self) -> &std::sync::Arc<LossyPaths> {
        &self.lossy
    }
}

thread_local! {
    /// Per-thread listing buffer (its capacity is reused across directories).
    static ENTRIES: std::cell::RefCell<Vec<WinEntry>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl DirLister for WinLister {
    fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()> {
        let real = self.lossy.translate(dir);
        let at_root = real.to_str().and_then(drive_root_letter).is_some();
        ENTRIES.with_borrow_mut(|buf| {
            buf.clear();
            ringscan_win::list_dir(&real, buf)?;
            out.reserve(buf.len());
            for e in buf.iter_mut() {
                if e.is_dir && e.raw_name.is_some() {
                    self.lossy.insert(dir.join(&e.name), e.path_in(&real));
                }
                out.push(raw_entry(e, self.cluster, at_root));
            }
            buf.clear();
            Ok(())
        })
    }

    fn list_cancellable(
        &self,
        dir: &Path,
        out: &mut Vec<RawEntry>,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        // One directory is one system-call drain that cannot be interrupted;
        // just avoid starting (and converting) work after a cancel.
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.list(dir, out)
    }
}

// ---------------------------------------------------------------------------
// Hard links
// ---------------------------------------------------------------------------

const SHARDS: usize = 64;

/// Cheap mixer for file ids (sequential MFT record numbers in the low bits):
/// one multiply and a fold, so both hashbrown's bucket index (low bits) and
/// tag (high bits) are well distributed.
#[derive(Default)]
pub struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b) ^ self.0.rotate_left(8));
        }
    }
    fn write_u64(&mut self, v: u64) {
        let m = u128::from(v) * 0x9E37_79B9_7F4A_7C15;
        self.0 = (m as u64) ^ ((m >> 64) as u64);
    }
}

/// Multi-linked files of one volume: file id → [`dir_hash`] of the
/// directory whose link carries the file's size.
pub type LinkMap = HashMap<u64, u64, BuildHasherDefault<IdHasher>>;

/// Map value bit: a second link of the file was seen.
const MULTI: u64 = 1 << 63;

fn mix(id: u64) -> u64 {
    let mut h = IdHasher::default();
    h.write_u64(id);
    h.finish()
}

/// Stable 63-bit hash of a directory path exactly as the walker spells it
/// (`parent.join(name)` from the scanned root; FNV-1a over the encoded
/// bytes).
pub fn dir_hash(dir: &Path) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in dir.as_os_str().as_encoded_bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3);
    }
    h & !MULTI
}

/// Wraps a lister and flags repeated file ids HARDLINK (see the
/// [module docs](self)).
pub struct HardLinkFilter<L> {
    inner: L,
    /// file id -> dir_hash of the counted link, | MULTI once repeated.
    shards: Box<[Mutex<LinkMap>]>,
}

impl<L> HardLinkFilter<L> {
    pub fn new(inner: L) -> HardLinkFilter<L> {
        HardLinkFilter {
            inner,
            shards: (0..SHARDS)
                .map(|_| Mutex::new(LinkMap::default()))
                .collect(),
        }
    }

    /// A filter for a rescan: every `(file id, dir hash)` in `counted` is a
    /// file whose size is already carried by a link elsewhere (outside the
    /// rescanned folder), so all its links found by this walk are flagged.
    pub fn with_counted(
        inner: L,
        counted: impl IntoIterator<Item = (u64, u64)>,
    ) -> HardLinkFilter<L> {
        let f = HardLinkFilter::new(inner);
        for (id, dir) in counted {
            f.shard(id).lock().insert(id, dir | MULTI);
        }
        f
    }

    pub fn inner(&self) -> &L {
        &self.inner
    }

    fn shard(&self, id: u64) -> &Mutex<LinkMap> {
        &self.shards[(mix(id) >> 58) as usize % SHARDS]
    }

    /// Distinct file ids seen so far (seeded ones included).
    pub fn distinct(&self) -> usize {
        self.shards.iter().map(|s| s.lock().len()).sum()
    }

    /// The files seen with more than one link (seeded ones included) plus
    /// those for which `also_keep(id)` is true, each with the [`dir_hash`]
    /// of its counted link.
    pub fn into_links(self, also_keep: impl Fn(u64) -> bool) -> LinkMap {
        let mut out = LinkMap::default();
        for shard in self.shards {
            for (id, v) in shard.into_inner() {
                if v & MULTI != 0 || also_keep(id) {
                    out.insert(id, v & !MULTI);
                }
            }
        }
        out
    }

    /// Flags entries whose file id was seen before.
    fn mark(&self, dir: &Path, entries: &mut [RawEntry]) {
        let mut here = None;
        for e in entries {
            if e.is_dir || e.file_id == 0 || e.flags & flags::HARDLINK != 0 {
                continue;
            }
            let h = *here.get_or_insert_with(|| dir_hash(dir));
            match self.shard(e.file_id).lock().entry(e.file_id) {
                Entry::Vacant(v) => {
                    v.insert(h);
                }
                Entry::Occupied(mut o) => {
                    *o.get_mut() |= MULTI;
                    e.flags |= flags::HARDLINK;
                }
            }
        }
    }
}

impl<L: DirLister> DirLister for HardLinkFilter<L> {
    fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()> {
        let start = out.len();
        let r = self.inner.list(dir, out);
        self.mark(dir, &mut out[start..]);
        r
    }

    fn list_cancellable(
        &self,
        dir: &Path,
        out: &mut Vec<RawEntry>,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        let start = out.len();
        let r = self.inner.list_cancellable(dir, out, cancel);
        if !cancel.load(Ordering::Relaxed) {
            self.mark(dir, &mut out[start..]);
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringscan_win::attrs::*;

    fn entry(name: &str, is_dir: bool, attrs: u32, tag: u32, alloc: u64) -> WinEntry {
        WinEntry {
            name: name.into(),
            raw_name: None,
            is_dir,
            logical: 100,
            alloc,
            attrs,
            reparse_tag: tag,
            mtime: 1_700_000_000,
            file_id: 42,
        }
    }

    #[test]
    fn attribute_mapping() {
        assert_eq!(flags_from_attrs(FILE_ATTRIBUTE_ARCHIVE, 0), 0);
        assert_eq!(
            flags_from_attrs(FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM, 0),
            flags::HIDDEN | flags::SYSTEM
        );
        assert_eq!(
            flags_from_attrs(FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_COMPRESSED, 0),
            flags::SPARSE | flags::COMPRESSED
        );
        let rp = FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY;
        for tag in [
            IO_REPARSE_TAG_MOUNT_POINT,
            IO_REPARSE_TAG_SYMLINK,
            IO_REPARSE_TAG_LX_SYMLINK,
        ] {
            assert_eq!(flags_from_attrs(rp, tag), flags::REPARSE, "{tag:#x}");
        }
        // Reparse points that stand for their own data are regular entries.
        for tag in [
            IO_REPARSE_TAG_WOF,
            IO_REPARSE_TAG_DEDUP,
            IO_REPARSE_TAG_APPEXECLINK,
        ] {
            assert_eq!(flags_from_attrs(rp, tag), 0, "{tag:#x}");
        }
        // Cloud: by tag or by attribute, never REPARSE.
        assert_eq!(flags_from_attrs(rp, 0x9000_601A), flags::CLOUD);
        assert_eq!(flags_from_attrs(rp, IO_REPARSE_TAG_CLOUD), flags::CLOUD);
        assert_eq!(
            flags_from_attrs(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, 0),
            flags::CLOUD
        );
        assert_eq!(
            flags_from_attrs(FILE_ATTRIBUTE_RECALL_ON_OPEN | rp, 0),
            flags::CLOUD
        );
        assert_eq!(flags_from_attrs(FILE_ATTRIBUTE_OFFLINE, 0), flags::CLOUD);
        // Unknown tag: never descend.
        assert_eq!(flags_from_attrs(rp, 0), flags::REPARSE);
        // A tag without the attribute is ignored.
        assert_eq!(flags_from_attrs(0, IO_REPARSE_TAG_SYMLINK), 0);
    }

    #[test]
    fn raw_entry_conversion() {
        let mut e = entry("a.txt", false, FILE_ATTRIBUTE_HIDDEN, 0, 4096);
        let r = raw_entry(&mut e, 4096, false);
        assert_eq!(
            (r.name.as_str(), r.is_dir, r.logical, r.alloc, r.flags),
            ("a.txt", false, 100, 4096, flags::HIDDEN)
        );
        assert_eq!((r.mtime, r.file_id), (1_700_000_000, 42));
        // MFT-resident: alloc rounded to 8 bytes -> 0; unknown cluster keeps it.
        let r = raw_entry(&mut entry("t", false, 0, 0, 104), 4096, false);
        assert_eq!(r.alloc, 0);
        let r = raw_entry(&mut entry("t", false, 0, 0, 104), 0, false);
        assert_eq!(r.alloc, 104);
        let r = raw_entry(&mut entry("t", false, 0, 0, 8192), 4096, false);
        assert_eq!(r.alloc, 8192);
        // Recycle Bin only at a drive root and only for directories.
        let bin = |root, dir| {
            raw_entry(
                &mut entry("$RECYCLE.BIN", dir, FILE_ATTRIBUTE_HIDDEN, 0, 0),
                4096,
                root,
            )
            .flags
        };
        assert_eq!(bin(true, true), flags::RECYCLE | flags::HIDDEN);
        assert_eq!(bin(false, true), flags::HIDDEN);
        assert_eq!(bin(true, false), flags::HIDDEN);
    }

    #[test]
    fn drive_roots() {
        for s in ["C:", "c:", r"C:\", "C:/", r"\\?\C:\", r"\\.\d:\", r"C:\\"] {
            assert!(drive_root_letter(s).is_some(), "{s}");
        }
        assert_eq!(drive_root_letter("d:"), Some('D'));
        for s in [
            r"C:\x",
            "C",
            "",
            r"\\server\share",
            "/",
            "1:",
            r"\\?\UNC\a\b",
        ] {
            assert_eq!(drive_root_letter(s), None, "{s}");
        }
    }

    #[test]
    fn lossy_translation() {
        let l = LossyPaths::new();
        let p = Path::new("/r/a\u{FFFD}b/c");
        assert_eq!(l.translate(p), Path::new("/r/a\u{FFFD}b/c"));
        l.insert("/r/a\u{FFFD}b".into(), "/r/REAL".into());
        assert_eq!(l.translate(p), Path::new("/r/REAL/c"));
        assert_eq!(
            l.translate(Path::new("/r/a\u{FFFD}b")),
            Path::new("/r/REAL")
        );
        assert_eq!(l.translate(Path::new("/r/other")), Path::new("/r/other"));
        assert_eq!(l.len(), 1);
    }

    struct Fixed(Vec<RawEntry>);
    impl DirLister for Fixed {
        fn list(&self, _dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()> {
            out.extend(self.0.iter().cloned());
            Ok(())
        }
    }

    #[test]
    fn hard_link_filter_flags_repeats() {
        let e = |name: &str, id: u64, is_dir: bool| RawEntry {
            name: name.into(),
            is_dir,
            logical: 10,
            alloc: 4096,
            file_id: id,
            ..RawEntry::default()
        };
        let f = HardLinkFilter::new(Fixed(vec![
            e("a", 7, false),
            e("b", 7, false),
            e("c", 0, false),
            e("d", 0, false),
            e("dir", 9, true),
            e("dir2", 9, true),
            e("x", 8, false),
        ]));
        let mut out = Vec::new();
        f.list(Path::new("p"), &mut out).unwrap();
        let hl: Vec<bool> = out.iter().map(|e| e.flags & flags::HARDLINK != 0).collect();
        assert_eq!(hl, [false, true, false, false, false, false, false]);
        // A second listing (another directory with the same files): all repeats.
        let mut out2 = Vec::new();
        f.list(Path::new("q"), &mut out2).unwrap();
        let hl: Vec<bool> = out2
            .iter()
            .map(|e| e.flags & flags::HARDLINK != 0)
            .collect();
        assert_eq!(hl, [true, true, false, false, false, false, true]);
        assert_eq!(f.distinct(), 2);
        // Both ids were seen twice: both are multi-linked, counted in "p".
        let links = f.into_links(|_| false);
        assert_eq!(links.len(), 2);
        assert_eq!(links[&7], dir_hash(Path::new("p")));
        assert_eq!(links[&8], dir_hash(Path::new("p")));
    }

    #[test]
    fn seeded_filter_flags_files_counted_elsewhere() {
        let e = |name: &str, id: u64| RawEntry {
            name: name.into(),
            logical: 10,
            file_id: id,
            ..RawEntry::default()
        };
        let outside = dir_hash(Path::new("elsewhere"));
        let f = HardLinkFilter::with_counted(Fixed(vec![e("a", 1), e("b", 2)]), [(1, outside)]);
        let mut out = Vec::new();
        f.list(Path::new("q"), &mut out).unwrap();
        assert_eq!(out[0].flags & flags::HARDLINK, flags::HARDLINK);
        assert_eq!(out[1].flags & flags::HARDLINK, 0);
        // Seeded entries keep their location; single links only on request.
        let links = f.into_links(|id| id == 2);
        assert_eq!(links[&1], outside);
        assert_eq!(links[&2], dir_hash(Path::new("q")));
        assert_ne!(dir_hash(Path::new("q")), dir_hash(Path::new("Q")));
    }

    #[test]
    fn id_hash_spreads_sequential_ids() {
        let mut used = [0u32; SHARDS];
        for id in 0..64_000u64 {
            used[(mix(id) >> 58) as usize % SHARDS] += 1;
        }
        assert!(used.iter().all(|&n| (500..1500).contains(&n)), "{used:?}");
    }
}
