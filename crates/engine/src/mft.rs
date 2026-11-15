//! MFT mode: [`VolumeReader`] (the NTFS reader's [`ReadAt`] over a raw
//! volume) and [`build_tree`] (MFT listing → [`Tree`]).
//!
//! # Building the tree
//!
//! The NTFS reader returns one [`MftFile`] per in-use base record with
//! validated parent references (see `ringscan-ntfs`). [`build_tree`] turns
//! that list into a [`Tree`] with core's bulk API (`add_node_raw`, then one
//! `recompute_aggregates` pass), which needs every parent to get a lower id
//! than its children. It does so in two passes: first the directories,
//! breadth-first from the root (only they need ordering, and only their
//! positions are indexed: a few MB); then all files in record order, since
//! every directory exists by then. Each file's names are freed as soon as
//! it is in the tree, and the listing is dropped before `build_tree`
//! returns, so the peak is about listing + tree rather than listing + tree +
//! ordering tables. Cost: ~0.35 µs per node including the aggregation pass
//! (1.48 M nodes in ~0.5 s).
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
//!   entry in walk mode), inserted right after the primary entry. Extra
//!   links of directories (only possible on a corrupt volume) are ignored.
//!   Which link is primary is the reader's choice (the first Win32 name in
//!   the record), so it often differs from walk mode, whose counted link is
//!   the first one the parallel walk happens to list: same totals, the size
//!   sits in another folder (25.7 K of 90 K multi-linked files on a real
//!   C:, mostly `WinSxS` vs `System32` / `servicing`). A shared
//!   deterministic rule (say, shortest path) would be cheap here but not in
//!   walk mode: a listing has no link counts, so the walk cannot know that
//!   a better link will turn up; it would need a pass after the walk that
//!   finds every counted link again (a name hash per file in the filter,
//!   a directory-hash → node map), moves sizes between nodes with ancestor
//!   updates and rewrites the rescan [`LinkMap`](crate::lister::LinkMap),
//!   with visible size shifts at the end of each walk.
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
//!   surrogates, CLOUD for cloud placeholders, ...) without the on-disk EA
//!   bit that shares its value with `RECALL_ON_OPEN` (see [`entry_flags`]),
//!   plus METAFILE, and
//!   RECYCLE for `$Recycle.Bin` at the root. Junction directories have no
//!   children in the MFT, so nothing is counted twice.

use std::io;

use ringscan_core::{NONE, NodeId, Tree, flags};
use ringscan_ntfs::{MftFile, ROOT_RECORD, ReadAt, mft_flags, reparse};

use crate::lister::{RECYCLE_BIN_DIR, flags_from_attrs};
use ringscan_win::attrs::FILE_ATTRIBUTE_RECALL_ON_OPEN;

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
    /// Entries whose reparse tag is stored outside their MFT record and was
    /// not read ([`ringscan_ntfs::reparse::UNREAD`]).
    pub unread_reparse_tags: u64,
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
///
/// Consumes the listing: each entry's names are freed as soon as they are
/// in the tree, and the listing itself before this returns.
pub fn build_tree(mut files: Vec<MftFile>, root_name: &str) -> MftBuild {
    let mut b = Builder {
        tree: Tree::new(root_name, 0),
        stats: MftBuildStats::default(),
        orphaned: NONE,
        node_of: Vec::new(),
    };
    let root = b.tree.root();
    let max_record = files.iter().map(|f| f.record).max().unwrap_or(0);
    let order = dirs_parent_first(&files, max_record);
    // Dense record -> directory node map (u32 per MFT record; transient).
    b.node_of = vec![NONE; usize::try_from(max_record).unwrap_or(0) + 1];
    if let Some(slot) = b.node_of.get_mut(ROOT_RECORD as usize) {
        *slot = root;
    }

    // 1. Directories, every parent before its children.
    let mut unread_dirs = Vec::new();
    for i in order {
        let f = &mut files[i as usize];
        let id = b.add(f);
        b.node_of[f.record as usize] = id;
        if f.reparse_tag == reparse::UNREAD {
            unread_dirs.push(id);
        }
    }
    // 2. Files in record order (all directories exist now), each followed
    // by its extra hard links. Parents keep lower ids than their children.
    let mut links = Vec::new();
    for f in files.iter_mut().filter(|f| !f.is_dir) {
        let primary = b.add(f);
        let extra = std::mem::take(&mut f.extra_links);
        if extra.is_empty() {
            continue;
        }
        links.push((
            f.record | (u64::from(f.seq) << 48),
            b.tree.node(primary).parent(),
        ));
        let fl = entry_flags(f) | flags::HARDLINK;
        for (parent, name) in extra {
            match b.dir_node(parent) {
                Some(p) => {
                    b.tree.add_node_raw(p, &name, 0, 0, fl, f.mtime);
                    b.stats.hardlinks += 1;
                    b.stats.nodes += 1;
                }
                None => b.stats.skipped_links += 1,
            }
        }
    }
    drop(files);
    drop(b.node_of);
    let mut tree = b.tree;
    tree.recompute_aggregates();
    // An unread reparse tag on a directory without children may be a
    // junction (whose tag would normally be resident): never descend.
    for id in unread_dirs {
        if !tree.has_children(id) {
            tree.set_flags(id, flags::REPARSE, 0);
        }
    }
    MftBuild {
        tree,
        stats: b.stats,
        links,
    }
}

/// State of [`build_tree`].
struct Builder {
    tree: Tree,
    stats: MftBuildStats,
    /// The `<orphaned>` container, once created.
    orphaned: NodeId,
    /// MFT record -> directory node.
    node_of: Vec<NodeId>,
}

impl Builder {
    /// Directory node of MFT record `record`, if it is one in the tree.
    fn dir_node(&self, record: u64) -> Option<NodeId> {
        usize::try_from(record)
            .ok()
            .and_then(|r| self.node_of.get(r).copied())
            .filter(|&p| p != NONE)
    }

    /// Adds the primary entry of `f` (below its directory, or below
    /// `<orphaned>`), frees its name and returns the new node.
    fn add(&mut self, f: &mut MftFile) -> NodeId {
        let root = self.tree.root();
        let parent = match self.dir_node(f.parent).filter(|_| !f.orphan) {
            Some(p) => p,
            None => {
                if self.orphaned == NONE {
                    self.orphaned = self.tree.add_node_raw(
                        root,
                        ORPHANED_NAME,
                        0,
                        0,
                        flags::DIR | flags::VIRTUAL,
                        0,
                    );
                }
                self.stats.orphans += 1;
                self.orphaned
            }
        };
        let mut fl = entry_flags(f);
        if f.reparse_tag == reparse::UNREAD {
            self.stats.unread_reparse_tags += 1;
        }
        let mut logical = f.logical;
        if f.flags & mft_flags::METAFILE != 0 {
            self.stats.metafiles += 1;
            if !f.is_dir {
                logical = logical.min(f.alloc);
            }
        }
        if parent == root && f.is_dir && f.name.eq_ignore_ascii_case(RECYCLE_BIN_DIR) {
            fl |= flags::RECYCLE;
        }
        if f.is_dir {
            fl |= flags::DIR;
            self.stats.dirs += 1;
        } else {
            self.stats.files += 1;
        }
        let name = std::mem::take(&mut f.name);
        self.stats.nodes += 1;
        self.tree
            .add_node_raw(parent, &name, logical, f.alloc, fl, f.mtime)
    }
}

/// Indices into `files` of the directories other than the root, every
/// parent before its children: breadth-first from the root, then each
/// directory that is not reachable from it (orphan, missing or
/// non-directory parent, a cycle in a filtered listing) starts a subtree of
/// its own, in record order. `max_record` bounds the records in `files`.
fn dirs_parent_first(files: &[MftFile], max_record: u64) -> Vec<u32> {
    let dirs: Vec<u32> = (0..files.len() as u32)
        .filter(|&i| {
            let f = &files[i as usize];
            f.is_dir && f.record != ROOT_RECORD
        })
        .collect();
    let n = dirs.len();
    let virtual_root = n as u32;
    // Record -> position in `dirs` (dense; transient).
    let mut pos = vec![NONE; usize::try_from(max_record).unwrap_or(0) + 1];
    for (k, &i) in dirs.iter().enumerate() {
        pos[files[i as usize].record as usize] = k as u32;
    }
    // Children as linked lists over positions; `n` stands for the root.
    let mut first_child = vec![NONE; n + 1];
    let mut next = vec![NONE; n];
    let mut detached = Vec::new();
    for k in (0..n).rev() {
        let f = &files[dirs[k] as usize];
        let parent = if f.orphan {
            None
        } else if f.parent == ROOT_RECORD {
            Some(virtual_root)
        } else {
            usize::try_from(f.parent)
                .ok()
                .and_then(|p| pos.get(p).copied())
                .filter(|&p| p != NONE && p as usize != k)
        };
        match parent {
            Some(p) => {
                next[k] = first_child[p as usize];
                first_child[p as usize] = k as u32;
            }
            None => detached.push(k as u32),
        }
    }
    drop(pos);
    detached.reverse();
    let mut seen = vec![false; n];
    let mut order: Vec<u32> = Vec::with_capacity(n);
    // Breadth-first below `start` (the root itself is not listed).
    let mut visit = |start: u32, order: &mut Vec<u32>| {
        let mut head = order.len();
        let mut cur = start;
        if start != virtual_root {
            if std::mem::replace(&mut seen[start as usize], true) {
                return;
            }
            order.push(start);
            head += 1;
        }
        loop {
            let mut c = first_child[cur as usize];
            while c != NONE {
                if !std::mem::replace(&mut seen[c as usize], true) {
                    order.push(c);
                }
                c = next[c as usize];
            }
            let Some(&n) = order.get(head) else { break };
            cur = n;
            head += 1;
        }
    };
    visit(virtual_root, &mut order);
    for k in detached {
        visit(k, &mut order);
    }
    // Members of parent cycles that nothing above reaches.
    for k in 0..n as u32 {
        visit(k, &mut order);
    }
    order.into_iter().map(|k| dirs[k as usize]).collect()
}

/// Flags of one MFT entry (without DIR / HARDLINK / RECYCLE).
///
/// A reparse tag the reader could not read ([`reparse::UNREAD`]: the value
/// is non-resident, as for big cloud placeholder blobs, while junction and
/// symlink buffers are small and resident) is not a name surrogate, so the
/// entry gets no REPARSE here; [`build_tree`] adds REPARSE to such a
/// directory only when it has no children in the MFT.
///
/// `RECALL_ON_OPEN` is masked out: on disk its value (`0x40000`) means
/// "has extended attributes" (`FILE_ATTRIBUTE_EA`, set on ~120K catalog-signed
/// system files on a real C:), and only directory enumeration through the
/// Cloud Files filter reports the cloud meaning. The NTFS reader already
/// drops the bit; this keeps a hand-made [`MftFile`] honest too. Cloud
/// evidence in MFT mode is therefore a Cloud Files reparse tag,
/// `RECALL_ON_DATA_ACCESS` or `OFFLINE`, as stored on disk.
pub fn entry_flags(f: &MftFile) -> u16 {
    let mut fl = flags_from_attrs(f.attrs & !FILE_ATTRIBUTE_RECALL_ON_OPEN, f.reparse_tag);
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
    use std::collections::HashMap;

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
    fn unread_reparse_tags_are_not_treated_as_links() {
        let rp = FILE_ATTRIBUTE_REPARSE_POINT;
        let files = vec![
            f(5, 5, ".", true, 0, 0),
            MftFile {
                attrs: rp | FILE_ATTRIBUTE_ARCHIVE,
                reparse_tag: reparse::UNREAD,
                ..f(40, 5, "big.iso", false, 1 << 20, 1 << 20)
            },
            MftFile {
                attrs: rp | FILE_ATTRIBUTE_DIRECTORY,
                reparse_tag: reparse::UNREAD,
                ..f(41, 5, "placeholder-dir", true, 0, 0)
            },
            f(42, 41, "inside.txt", false, 10, 4096),
            MftFile {
                attrs: rp | FILE_ATTRIBUTE_DIRECTORY,
                reparse_tag: reparse::UNREAD,
                ..f(43, 5, "empty-unknown", true, 0, 0)
            },
            // A genuinely unknown tag (0) keeps the safety net.
            MftFile {
                attrs: rp,
                reparse_tag: 0,
                ..f(44, 5, "tag0", false, 1, 0)
            },
        ];
        let MftBuild { tree: t, stats, .. } = build_tree(files, "X:\\");
        let get = |name: &str| t.find_child(0, name).unwrap();
        assert_eq!(t.node(get("big.iso")).flags() & flags::REPARSE, 0);
        assert_eq!(t.node(get("placeholder-dir")).flags() & flags::REPARSE, 0);
        assert_eq!(
            t.node(get("empty-unknown")).flags() & flags::REPARSE,
            flags::REPARSE
        );
        assert_eq!(t.node(get("tag0")).flags() & flags::REPARSE, flags::REPARSE);
        assert_eq!(stats.unread_reparse_tags, 3);
        assert_eq!(t.size(get("big.iso"), SizeMode::Allocated), 1 << 20);
    }

    #[test]
    fn builds_tree_with_all_rules() {
        let files = sample();
        let MftBuild {
            tree: t,
            stats: s,
            links,
        } = build_tree(files.clone(), "C:\\");
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
                unread_reparse_tags: 0,
            }
        );
        // The multi-linked file, with the directory of its primary name.
        assert_eq!(links, [(34 | 1 << 48, dir)]);
        // 7 primary files + 2 hard links; `<orphaned>` is not a directory.
        assert_eq!((t.files(0), t.dirs(0)), (9, 5));
        assert_eq!(t.live_len(), 1 + 1 + 14);
    }

    #[test]
    fn cloud_flag_needs_genuine_cloud_evidence() {
        let file = |attrs: u32, reparse_tag: u32| MftFile {
            attrs,
            reparse_tag,
            ..f(40, 5, "x", false, 1, 0)
        };
        let has_cloud = |m: MftFile| entry_flags(&m) & flags::CLOUD != 0;
        // On-disk 0x40000 = FILE_ATTRIBUTE_EA (catalog-signed system files),
        // even if a hand-made listing still carries it.
        assert_eq!(FILE_ATTRIBUTE_EA, FILE_ATTRIBUTE_RECALL_ON_OPEN);
        assert!(!has_cloud(file(
            FILE_ATTRIBUTE_EA | FILE_ATTRIBUTE_ARCHIVE,
            0
        )));
        assert!(!has_cloud(file(
            FILE_ATTRIBUTE_EA | FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_HIDDEN,
            0
        )));
        assert_eq!(
            entry_flags(&file(FILE_ATTRIBUTE_EA | FILE_ATTRIBUTE_SYSTEM, 0)),
            flags::SYSTEM
        );
        // Genuine evidence: a Cloud Files tag, RECALL_ON_DATA_ACCESS, OFFLINE.
        let rp = FILE_ATTRIBUTE_REPARSE_POINT;
        assert!(has_cloud(file(
            rp | FILE_ATTRIBUTE_SPARSE_FILE,
            0x9000_701A
        )));
        assert!(has_cloud(file(
            rp | FILE_ATTRIBUTE_EA,
            IO_REPARSE_TAG_CLOUD
        )));
        assert!(has_cloud(file(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, 0)));
        assert!(has_cloud(file(FILE_ATTRIBUTE_OFFLINE, 0)));
        // Other reparse points are not cloud.
        assert!(!has_cloud(file(rp, IO_REPARSE_TAG_WOF)));
        assert_eq!(
            entry_flags(&file(rp | FILE_ATTRIBUTE_EA, IO_REPARSE_TAG_SYMLINK)),
            flags::REPARSE
        );
        // Walk mode keeps RECALL_ON_OPEN: there only the Cloud Files filter
        // reports it, for placeholder directories.
        assert_eq!(
            flags_from_attrs(FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_DIRECTORY, 0),
            flags::CLOUD
        );
    }

    /// Every node's parent has a lower id (what `recompute_aggregates`
    /// needs), and every node below the root is reachable.
    fn assert_parents_first(t: &Tree) {
        for (id, n) in t.live_nodes() {
            if id != t.root() {
                assert!(n.parent() < id, "node {id} before its parent");
            }
        }
    }

    #[test]
    fn random_listings_rebuild_every_path() {
        use ringscan_core::synth::Rng;
        let mut rng = Rng::new(42);
        for round in 0..20 {
            let (dirs, files) = (1 + rng.below(300), rng.below(900));
            // Shuffled record numbers: children often come before parents.
            let mut recs: Vec<u64> = (16..16 + dirs + files).map(|r| r * 3).collect();
            for i in (1..recs.len()).rev() {
                recs.swap(i, rng.below(i as u64 + 1) as usize);
            }
            let mut v = vec![f(5, 5, ".", true, 0, 0)];
            let mut path_of: HashMap<u64, String> = HashMap::from([(5, "R:".to_string())]);
            for k in 0..dirs as usize {
                let parent = if k == 0 {
                    5
                } else {
                    recs[rng.below(k as u64) as usize]
                };
                let name = format!("d{k}");
                path_of.insert(recs[k], format!("{}\\{name}", path_of[&parent]));
                v.push(f(recs[k], parent, &name, true, 0, 4096));
            }
            let mut extra = 0;
            for k in dirs as usize..recs.len() {
                let dir = |rng: &mut Rng| recs[rng.below(dirs) as usize];
                let parent = dir(&mut rng);
                let name = format!("f{k}");
                path_of.insert(recs[k], format!("{}\\{name}", path_of[&parent]));
                let mut e = f(recs[k], parent, &name, false, k as u64, 4096);
                if rng.chance(0.1) {
                    e.extra_links.push((dir(&mut rng), format!("l{k}")));
                    extra += 1;
                }
                v.push(e);
            }
            v.sort_by_key(|e| e.record);
            let b = build_tree(v, "R:");
            let t = &b.tree;
            assert_parents_first(t);
            assert_eq!(b.stats.orphans, 0, "round {round}");
            assert_eq!((b.stats.dirs, b.stats.files), (dirs, files));
            assert_eq!(b.stats.hardlinks, extra);
            assert_eq!(b.links.len() as u64, extra);
            assert_eq!(t.live_len() as u64, 1 + dirs + files + extra);
            for p in path_of.values() {
                assert!(t.find_path(p).is_some(), "round {round}: {p}");
            }
            let total: u64 = (dirs..dirs + files).sum();
            assert_eq!(t.size(0, SizeMode::Logical), total);
        }
    }

    #[test]
    fn detached_directories_keep_their_subtrees() {
        let files = vec![
            f(5, 5, ".", true, 0, 0),
            // A chain whose records run against the tree order.
            f(20, 21, "c3", true, 0, 0),
            f(21, 22, "c2", true, 0, 0),
            f(22, 5, "c1", true, 0, 0),
            f(23, 20, "deep.txt", false, 3, 0),
            // A parent cycle (possible only in a filtered listing), with a file.
            f(30, 31, "cyc-a", true, 0, 0),
            f(31, 30, "cyc-b", true, 0, 0),
            f(32, 31, "in-cycle.txt", false, 5, 0),
            // A directory whose parent is a file, with a child.
            f(40, 23, "under-file", true, 0, 0),
            f(41, 40, "x.txt", false, 7, 0),
        ];
        let b = build_tree(files, "Y:\\");
        let t = &b.tree;
        assert_parents_first(t);
        assert!(t.find_path(r"Y:\c1\c2\c3\deep.txt").is_some());
        let orphaned = orphaned_node(t).expect("orphaned");
        // One cycle member and the directory below a file start subtrees.
        assert_eq!(b.stats.orphans, 2);
        assert_eq!(t.files(orphaned), 2);
        assert_eq!(t.size(orphaned, SizeMode::Logical), 12);
        assert!(t.find_path(r"Y:\<orphaned>\under-file\x.txt").is_some());
        assert_eq!(t.files(0), 3);
    }

    #[test]
    fn missing_parent_goes_to_orphaned() {
        // Not flagged orphan, but the parent record is missing (filtered input).
        let files = vec![f(5, 5, ".", true, 0, 0), f(70, 66, "stray", false, 1, 4096)];
        let b = build_tree(files, "X:\\");
        let (t, s) = (b.tree, b.stats);
        assert_eq!(s.orphans, 1);
        assert!(t.find_path(r"X:\<orphaned>\stray").is_some());
    }

    #[test]
    fn empty_listing_gives_root_only() {
        let b = build_tree(Vec::new(), "E:\\");
        assert_eq!(b.tree.live_len(), 1);
        assert_eq!(b.stats, MftBuildStats::default());
        assert!(b.links.is_empty());
    }
}
