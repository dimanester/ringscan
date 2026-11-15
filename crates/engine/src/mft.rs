//! MFT mode: [`VolumeReader`] (the NTFS reader's [`ReadAt`] over a raw
//! volume) and [`build_tree`] (MFT listing → [`Tree`]).
//!
//! # Building the tree
//!
//! The NTFS reader returns one [`MftFile`] per in-use base record with
//! validated parent references (see `ringscan-ntfs`). [`build_tree`] turns
//! that list into a [`Tree`] with core's bulk API: nodes are inserted with
//! `add_node_raw` in [`parent_first_order`] (every parent before its
//! children, so parents always get lower ids, which is what
//! `recompute_aggregates` needs), and one `recompute_aggregates` pass sums
//! everything up. Cost: ~0.1 µs per node plus the aggregation pass.
//!
//! * **Root:** record 5 becomes the tree root, named by the caller (`C:\`).
//!   The root directory's own index allocation (a few KiB) is not
//!   representable on a root node and is left out; it ends up in the
//!   drive's UNACCOUNTED space.
//! * **Directories** carry their own size: the `$INDEX_ALLOCATION` (and any
//!   named streams) as reported by the reader.
//! * **Hard links** are counted once: the primary name carries the sizes;
//!   every entry of [`MftFile::extra_links`] becomes a zero-size HARDLINK
//!   node in its own directory (it still counts as a file, like a directory
//!   entry in walk mode). Extra links are inserted after all primary
//!   entries, because their parents may come later in parent-first order.
//!   Extra links of directories (only possible on a corrupt volume) are
//!   ignored.
//! * **Metafiles** (`$MFT`, `$LogFile`, `$Bitmap`, `$Secure`, `$BadClus`,
//!   `$Extend` and everything below it, ...) stay where NTFS puts them,
//!   directly under the root, flagged METAFILE: they are real allocated
//!   space (`$MFT` alone is gigabytes on a big drive). Their logical size is
//!   capped at the allocated size: the logical size of a sparse metafile
//!   such as `$Extend\$UsnJrnl` (`$J`) describes address space, not data,
//!   and would otherwise dwarf the drive in logical mode. `$BadClus:$Bad`
//!   (volume-sized and mostly holes) is already counted by its bad clusters
//!   only by the reader.
//! * **Orphans** (stale, missing or non-directory parent, members of parent
//!   cycles) go below a virtual `<orphaned>` directory under the root,
//!   created only when needed; their own subtrees follow them.
//! * **Flags** come from the Windows attributes through
//!   [`flags_from_attrs`] (same mapping as walk mode: REPARSE for name
//!   surrogates, CLOUD for cloud placeholders, ...), plus METAFILE, and
//!   RECYCLE for `$Recycle.Bin` at the root. Junction directories have no
//!   children in the MFT, so nothing is counted twice.

use std::io;

use ringscan_core::{NONE, NodeId, Tree, flags};
use ringscan_ntfs::{MftFile, ROOT_RECORD, ReadAt, mft_flags, parent_first_order};

use crate::lister::{RECYCLE_BIN_DIR, flags_from_attrs};

/// Name of the virtual directory that holds orphaned MFT entries.
pub const ORPHANED_NAME: &str = "<orphaned>";

/// [`ReadAt`] over a raw volume (`\\.\C:`), for [`ringscan_ntfs::scan_mft`].
///
/// The NTFS reader only issues 4096-aligned reads (or sector multiples at
/// the volume end) with 4096-aligned buffers, which is exactly what a raw
/// volume handle needs, so this simply delegates.
#[derive(Debug)]
pub struct VolumeReader(pub ringscan_win::Volume);

impl VolumeReader {
    /// Opens drive `letter` for raw reading (needs elevation; see
    /// [`ringscan_win::Volume::open`]).
    pub fn open(letter: char) -> io::Result<VolumeReader> {
        ringscan_win::Volume::open(letter).map(VolumeReader)
    }
}

impl ReadAt for VolumeReader {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.0.read_at(off, buf)
    }
}

/// What [`build_tree`] produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MftBuildStats {
    /// Nodes added (the root and `<orphaned>` excluded).
    pub nodes: u64,
    /// Files (primary names).
    pub files: u64,
    pub dirs: u64,
    /// HARDLINK nodes added for extra links.
    pub hardlinks: u64,
    /// Entries attached below `<orphaned>` (their subtrees not counted).
    pub orphans: u64,
    /// Entries flagged METAFILE.
    pub metafiles: u64,
    /// Extra links whose directory is not in the tree (skipped).
    pub skipped_links: u64,
}

/// Result of [`build_tree`].
#[derive(Debug)]
pub struct MftBuild {
    pub tree: Tree,
    pub stats: MftBuildStats,
    /// Files with more than one link: (file id as a directory listing
    /// reports it, i.e. `record | seq << 48`; directory node of the primary
    /// link, which carries the size). Lets walk-mode rescans of a folder
    /// keep counting each such file once.
    pub links: Vec<(u64, NodeId)>,
}

/// Builds a tree from an MFT listing (as returned by
/// [`ringscan_ntfs::read_mft`] / [`ringscan_ntfs::scan_mft`]); the root is
/// record 5 and is named `root_name` (e.g. `C:\`). See the
/// [module docs](self) for the rules.
pub fn build_tree(files: &[MftFile], root_name: &str) -> MftBuild {
    let mut tree = Tree::new(root_name, 0);
    let mut stats = MftBuildStats::default();
    let mut links = Vec::new();
    let root = tree.root();
    let max_record = files.iter().map(|f| f.record).max().unwrap_or(0);
    // Dense record -> node map (u32 per MFT record; transient).
    let mut node_of = vec![NONE; usize::try_from(max_record).unwrap_or(0) + 1];
    if let Some(slot) = node_of.get_mut(ROOT_RECORD as usize) {
        *slot = root;
    }
    let mut orphaned = NONE;

    for i in parent_first_order(files) {
        let f = &files[i];
        if f.record == ROOT_RECORD {
            continue;
        }
        let parent = node_of
            .get(f.parent as usize)
            .copied()
            .filter(|&p| !f.orphan && p != NONE && tree.node(p).is_dir());
        let parent = match parent {
            Some(p) => p,
            None => {
                if orphaned == NONE {
                    orphaned = tree.add_node_raw(
                        root,
                        ORPHANED_NAME,
                        0,
                        0,
                        flags::DIR | flags::VIRTUAL,
                        0,
                    );
                }
                stats.orphans += 1;
                orphaned
            }
        };
        let mut fl = entry_flags(f);
        let mut logical = f.logical;
        if f.flags & mft_flags::METAFILE != 0 {
            stats.metafiles += 1;
            if !f.is_dir {
                logical = logical.min(f.alloc);
            }
        }
        if parent == root && f.is_dir && f.name.eq_ignore_ascii_case(RECYCLE_BIN_DIR) {
            fl |= flags::RECYCLE;
        }
        if f.is_dir {
            fl |= flags::DIR;
            stats.dirs += 1;
        } else {
            stats.files += 1;
        }
        let id = tree.add_node_raw(parent, &f.name, logical, f.alloc, fl, f.mtime);
        node_of[f.record as usize] = id;
        stats.nodes += 1;
    }

    // Extra hard links, after every primary entry exists (their parents may
    // come later in parent-first order). Appended last, so every parent id
    // is still lower than its children's.
    for f in files
        .iter()
        .filter(|f| !f.is_dir && !f.extra_links.is_empty())
    {
        let primary = node_of[f.record as usize];
        if primary == NONE {
            continue;
        }
        links.push((
            f.record | (u64::from(f.seq) << 48),
            tree.node(primary).parent(),
        ));
        let fl = entry_flags(f) | flags::HARDLINK;
        for (parent, name) in &f.extra_links {
            let p = node_of
                .get(*parent as usize)
                .copied()
                .filter(|&p| p != NONE && tree.node(p).is_dir());
            match p {
                Some(p) => {
                    tree.add_node_raw(p, name, 0, 0, fl, f.mtime);
                    stats.hardlinks += 1;
                    stats.nodes += 1;
                }
                None => stats.skipped_links += 1,
            }
        }
    }
    tree.recompute_aggregates();
    MftBuild { tree, stats, links }
}

/// Flags of one MFT entry (without DIR / HARDLINK / RECYCLE).
fn entry_flags(f: &MftFile) -> u16 {
    let mut fl = flags_from_attrs(f.attrs, f.reparse_tag);
    if f.flags & mft_flags::METAFILE != 0 {
        fl |= flags::METAFILE;
    }
    fl
}

/// Id of the `<orphaned>` container below `root`, if any.
pub fn orphaned_node(tree: &Tree) -> Option<NodeId> {
    tree.children(tree.root()).find(|&c| {
        let n = tree.node(c);
        n.is_virtual() && n.is_dir() && tree.name(c) == ORPHANED_NAME
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringscan_core::SizeMode;
    use ringscan_win::attrs::*;

    fn f(record: u64, parent: u64, name: &str, is_dir: bool, logical: u64, alloc: u64) -> MftFile {
        MftFile {
            record,
            parent,
            name: name.into(),
            is_dir,
            logical,
            alloc,
            seq: 1,
            ..MftFile::default()
        }
    }

    fn sample() -> Vec<MftFile> {
        let mut v = vec![
            // Metafiles (records 0..15 have the root as parent).
            MftFile {
                flags: mft_flags::METAFILE,
                ..f(0, 5, "$MFT", false, 1 << 20, 1 << 20)
            },
            MftFile {
                flags: mft_flags::METAFILE,
                ..f(5, 5, ".", true, 0, 4096)
            },
            MftFile {
                flags: mft_flags::METAFILE,
                ..f(11, 5, "$Extend", true, 0, 0)
            },
            // A sparse metafile with a huge logical size.
            MftFile {
                flags: mft_flags::METAFILE,
                attrs: FILE_ATTRIBUTE_SPARSE_FILE,
                ..f(40, 11, "$UsnJrnl", false, 50 << 30, 32 << 20)
            },
            // A child listed before its parent (record order != tree order).
            f(20, 30, "child.txt", false, 100, 4096),
            f(30, 5, "dir", true, 0, 8192),
            f(31, 5, RECYCLE_BIN_DIR, true, 0, 0),
            MftFile {
                attrs: FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY,
                reparse_tag: IO_REPARSE_TAG_MOUNT_POINT,
                ..f(32, 5, "junction", true, 0, 0)
            },
            MftFile {
                attrs: FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_REPARSE_POINT,
                reparse_tag: 0x9000_601A,
                ..f(33, 30, "cloud.docx", false, 1 << 20, 0)
            },
            // Hard link: primary in "dir", extra links in root and in "dir".
            MftFile {
                extra_links: vec![(5, "link-in-root.bin".into()), (30, "link2.bin".into())],
                attrs: FILE_ATTRIBUTE_HIDDEN,
                ..f(34, 30, "primary.bin", false, 5000, 8192)
            },
            // Orphans: a directory with a child, and a file.
            MftFile {
                orphan: true,
                ..f(50, 99, "lost-dir", true, 0, 4096)
            },
            f(51, 50, "lost-child", false, 10, 4096),
            MftFile {
                orphan: true,
                ..f(52, 77, "lost-file", false, 7, 0)
            },
        ];
        v.sort_by_key(|f| f.record);
        v
    }

    #[test]
    fn builds_tree_with_all_rules() {
        let files = sample();
        let MftBuild {
            tree: t,
            stats: s,
            links,
        } = build_tree(&files, "C:\\");
        assert_eq!(t.name(0), "C:\\");
        let find = |p: &str| t.find_path(p).unwrap_or_else(|| panic!("{p}"));
        // Sizes and aggregation.
        let dir = find(r"C:\dir");
        assert_eq!(t.files(dir), 4, "child, cloud, primary, link2");
        assert_eq!(t.size(dir, SizeMode::Allocated), 8192 + 4096 + 8192);
        assert_eq!(t.size(dir, SizeMode::Logical), 100 + (1 << 20) + 5000);
        // Hard links: zero-size nodes, flags kept.
        let l1 = find(r"C:\link-in-root.bin");
        let n = t.node(l1);
        assert!(n.has(flags::HARDLINK) && n.has(flags::HIDDEN));
        assert_eq!((n.logical(), n.alloc()), (0, 0));
        assert!(t.node(find(r"C:\dir\link2.bin")).has(flags::HARDLINK));
        assert!(!t.node(find(r"C:\dir\primary.bin")).has(flags::HARDLINK));
        // Flags.
        assert!(t.node(find(r"C:\junction")).has(flags::REPARSE));
        let cloud = t.node(find(r"C:\dir\cloud.docx"));
        assert!(cloud.has(flags::CLOUD) && !cloud.has(flags::REPARSE));
        assert!(t.node(find(r"C:\$Recycle.Bin")).has(flags::RECYCLE));
        // Metafiles under the root; sparse metafile logical capped at alloc.
        let usn = t.node(find(r"C:\$Extend\$UsnJrnl"));
        assert!(usn.has(flags::METAFILE));
        assert_eq!((usn.logical(), usn.alloc()), (32 << 20, 32 << 20));
        assert!(t.node(find(r"C:\$MFT")).has(flags::METAFILE));
        // Orphans under a virtual container, subtree preserved.
        let orphaned = orphaned_node(&t).expect("orphaned container");
        assert!(t.node(orphaned).is_virtual());
        assert_eq!(t.files(orphaned), 2);
        assert_eq!(t.dirs(orphaned), 1);
        assert!(t.find_path(r"C:\<orphaned>\lost-dir\lost-child").is_some());
        // Totals: every allocated byte exactly once (the root's own index
        // allocation is not representable).
        let alloc: u64 = files
            .iter()
            .filter(|f| f.record != 5)
            .map(|f| f.alloc)
            .sum();
        assert_eq!(t.size(0, SizeMode::Allocated), alloc);
        assert_eq!(
            s,
            MftBuildStats {
                nodes: 14,
                files: 7,
                dirs: 5,
                hardlinks: 2,
                orphans: 2,
                metafiles: 3,
                skipped_links: 0,
            }
        );
        // The multi-linked file, with the directory of its primary name.
        assert_eq!(links, [(34 | 1 << 48, dir)]);
        // 7 primary files + 2 hard links; `<orphaned>` is not a directory.
        assert_eq!((t.files(0), t.dirs(0)), (9, 5));
        assert_eq!(t.live_len(), 1 + 1 + 14);
    }

    #[test]
    fn missing_parent_goes_to_orphaned() {
        // Not flagged orphan, but the parent record is missing (filtered input).
        let files = vec![f(5, 5, ".", true, 0, 0), f(70, 66, "stray", false, 1, 4096)];
        let b = build_tree(&files, "X:\\");
        let (t, s) = (b.tree, b.stats);
        assert_eq!(s.orphans, 1);
        assert!(t.find_path(r"X:\<orphaned>\stray").is_some());
    }

    #[test]
    fn empty_listing_gives_root_only() {
        let b = build_tree(&[], "E:\\");
        assert_eq!(b.tree.live_len(), 1);
        assert_eq!(b.stats, MftBuildStats::default());
        assert!(b.links.is_empty());
    }
}
