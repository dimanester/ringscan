//! Arena tree of scanned file-system entries with live aggregation.
//!
//! [`Tree`] stores every file, directory and synthetic ("virtual") entry of a
//! scan in one arena and keeps the sizes and counts of all ancestors exact
//! after every mutation, so the UI can lay out a consistent chart at any
//! moment of a running scan.
//!
//! # Memory layout
//!
//! A scan of a big drive holds tens of millions of entries, so the per-node
//! footprint is the main design constraint (target: <= 64 bytes per node
//! including its name):
//!
//! * [`Node`] is 40 bytes: logical + allocated size (2 x u64), parent and
//!   next-sibling links, name offset, directory-record index, mtime (u32
//!   seconds), flags (u16) and extension id (u16).
//! * Directory-only data (child-list tail, file and directory counts) lives
//!   in a side table of 12-byte records indexed by the node, because ~90 % of
//!   the nodes of a real drive are files that need none of it.
//! * Names are stored once in a shared byte buffer as `varint length + UTF-8`
//!   and referenced by a 32-bit offset: no per-name allocation and about one
//!   byte of overhead per name.
//! * Extensions are interned, lowercased, into a u16 id (0 = none or
//!   directory); past 65 535 distinct extensions new ones fall back to 0.
//! * Nodes, directory records and names live in fixed-size chunks instead of
//!   one growing `Vec`. Growth never copies existing data (no multi-hundred-MB
//!   `memcpy` while the walker holds the write lock), never over-allocates by
//!   up to 2x like `Vec` doubling, and indexing stays cheap (`id >> 16`).
//!
//! `tests/perf.rs` measures a 5M-node synthetic build (bytes per node and
//! insert throughput).
//!
//! # Child lists
//!
//! Children form a circular singly linked list: the directory record points
//! to the *last* child and the last child's `next` points back to the first.
//! Appending is O(1) and keeps insertion order without a `first_child` field.
//! [`Tree::children`] yields insertion order until [`Tree::sort_children`] or
//! [`Tree::sort_all`] relinks the list by size (descending, ties by name);
//! nodes added afterwards are appended at the end. Unlinking one child is
//! O(number of siblings).
//!
//! # Aggregation rules
//!
//! * A file's `logical` / `alloc` are its own sizes. HARDLINK nodes (extra
//!   links of a file counted elsewhere) always have zero sizes but still count
//!   as files (they are real directory entries).
//! * A directory's `logical` / `alloc` = its own size (normally 0; the NTFS
//!   index allocation in MFT mode) + the sizes of all its children, except
//!   FREE nodes.
//! * [`Tree::files`] / [`Tree::dirs`] count the files / directories strictly
//!   below a node (the node itself is not included; a file has 0 / 0).
//! * Virtual nodes (VIRTUAL; FREE and UNACCOUNTED imply it):
//!   - never count as files or directories, at any level;
//!   - FREE (free space of a volume) is **not** added to its ancestors, so the
//!     size of any node is always "used" bytes; [`crate::layout`] adds FREE
//!     nodes back when `Params::show_free` is set, which only affects charts
//!     whose focus contains a FREE node (a drive root, "This PC");
//!   - every other virtual leaf (UNACCOUNTED = used space the scan could not
//!     see, an engine-made Recycle Bin estimate, ...) is aggregated like a
//!     file, so a drive root's size equals the used space Windows reports
//!     once the engine has added UNACCOUNTED;
//!   - virtual directories (a "This PC" root, an `<orphaned>` container)
//!     aggregate their children like any directory.
//! * Live insertion ([`Tree::add_node`] and friends) updates all ancestors
//!   immediately; [`Tree::add_batch`] does a single ancestor walk per batch.
//!   The raw API ([`Tree::add_node_raw`] and friends) skips that and leaves
//!   ancestors stale until [`Tree::recompute_aggregates`] (one pass over the
//!   pending nodes). Any other mutation finalizes pending raw nodes first.
//!
//! # Removal and tombstones
//!
//! [`Tree::remove`] and [`Tree::clear_children`] unlink the subtree, subtract
//! its contribution from every ancestor and flag each node in it DELETED.
//! Tombstones keep their ids (a stale id held by the UI can still be
//! inspected; see [`Tree::is_live`] and [`Tree::live_ancestor`]) but are never
//! reachable from the root, so iterators, layout and stats never visit them.
//! Inserting below a tombstone creates another tombstone (and
//! [`Tree::add_batch`] ignores the batch), so a walker racing with a delete
//! cannot corrupt live totals. Their memory is reclaimed only when the tree is
//! dropped.
//!
//! # Generation
//!
//! Every mutation increments [`Tree::generation`]; a reader that remembers
//! the generation of its last layout can skip work when nothing changed.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::AddAssign;

use crate::flags::{self, DELETED, DIR, ERROR, FREE, HARDLINK, UNACCOUNTED, VIRTUAL};
use crate::walk::RawEntry;
use crate::{NONE, NodeId, SizeMode};

/// Id of the root node of every tree.
const ROOT: NodeId = 0;

// ---------------------------------------------------------------------------
// Node
// ---------------------------------------------------------------------------

/// One entry of a [`Tree`] (40 bytes). Read it through [`Tree::node`].
///
/// The fields are private so the compact representation can evolve; the
/// accessors return the values in the units of the cross-crate contract.
/// Name, counts and children are per-tree data: see [`Tree::name`],
/// [`Tree::files`], [`Tree::dirs`], [`Tree::children`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    /// Logical bytes: own for leaves, aggregated for directories.
    logical: u64,
    /// Allocated bytes: own for leaves, aggregated for directories.
    alloc: u64,
    parent: NodeId,
    /// Next sibling in the parent's circular child list (NONE if unlinked).
    next: NodeId,
    /// Offset of the name in the [`NameStore`].
    name: u32,
    /// DIR nodes: index of their [`DirInfo`] record; other nodes: NONE.
    dir: u32,
    /// Last modification, unix seconds; 0 = unknown.
    mtime: u32,
    flags: u16,
    ext: u16,
}

const _: () = assert!(std::mem::size_of::<Node>() == 40);

impl Node {
    /// Parent node ([`NONE`] for the root).
    #[inline]
    pub fn parent(&self) -> NodeId {
        self.parent
    }
    /// All flag bits (see [`crate::flags`]).
    #[inline]
    pub fn flags(&self) -> u16 {
        self.flags
    }
    /// True if any of the bits in `flag` is set.
    #[inline]
    pub fn has(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }
    /// Directory (may have children).
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & DIR != 0
    }
    /// Not a DELETED tombstone.
    #[inline]
    pub fn is_live(&self) -> bool {
        self.flags & DELETED == 0
    }
    /// Synthetic node (VIRTUAL, FREE or UNACCOUNTED).
    #[inline]
    pub fn is_virtual(&self) -> bool {
        self.flags & VIRTUAL != 0
    }
    /// Counts as a file: neither a directory nor virtual.
    #[inline]
    pub fn is_file(&self) -> bool {
        self.flags & (DIR | VIRTUAL) == 0
    }
    /// Interned extension id ([`Tree::ext_name`]); 0 = none / directory.
    #[inline]
    pub fn ext(&self) -> u16 {
        self.ext
    }
    /// Logical size (own for leaves, aggregated for directories).
    #[inline]
    pub fn logical(&self) -> u64 {
        self.logical
    }
    /// Allocated size (own for leaves, aggregated for directories).
    #[inline]
    pub fn alloc(&self) -> u64 {
        self.alloc
    }
    /// Size in the given mode.
    #[inline]
    pub fn size(&self, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Allocated => self.alloc,
            SizeMode::Logical => self.logical,
        }
    }
    /// Last modification time, unix seconds; 0 = unknown. Stored as u32, so
    /// times before 1970 read as 0 and times after 2106 are clamped.
    #[inline]
    pub fn mtime(&self) -> i64 {
        i64::from(self.mtime)
    }
}

/// Directory-only data, kept out of [`Node`] to save memory on files.
#[derive(Clone, Copy, Debug)]
struct DirInfo {
    /// Last child of the circular child list (NONE = no children).
    tail: NodeId,
    /// Files strictly below the directory. u32 is enough: every counted
    /// file is a node of this tree and node ids are u32.
    files: u32,
    /// Directories strictly below the directory.
    dirs: u32,
}

/// What one node adds to its parent's aggregates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Delta {
    logical: u64,
    alloc: u64,
    files: u32,
    dirs: u32,
}

impl Delta {
    const ZERO: Delta = Delta {
        logical: 0,
        alloc: 0,
        files: 0,
        dirs: 0,
    };

    /// Contribution of a node without descendants.
    #[inline]
    fn leaf(flags: u16, logical: u64, alloc: u64) -> Delta {
        if flags & DELETED != 0 {
            return Delta::ZERO;
        }
        let counted = flags & VIRTUAL == 0;
        let sized = flags & FREE == 0;
        Delta {
            logical: if sized { logical } else { 0 },
            alloc: if sized { alloc } else { 0 },
            files: u32::from(counted && flags & DIR == 0),
            dirs: u32::from(counted && flags & DIR != 0),
        }
    }
}

impl AddAssign for Delta {
    #[inline]
    fn add_assign(&mut self, o: Delta) {
        self.logical += o.logical;
        self.alloc += o.alloc;
        self.files += o.files;
        self.dirs += o.dirs;
    }
}

// ---------------------------------------------------------------------------
// Chunked storage
// ---------------------------------------------------------------------------

/// log2 of the number of elements per storage chunk.
const CHUNK_BITS: u32 = 16;
const CHUNK_LEN: usize = 1 << CHUNK_BITS;
/// Capacity of the first chunk; it doubles up to `CHUNK_LEN` so small trees
/// (folder rescans, tests) stay small.
const FIRST_CHUNK_LEN: usize = 64;

/// Append-only vector made of fixed-size chunks: pushing never moves
/// existing elements and wastes at most one partially filled chunk.
#[derive(Clone, Debug)]
struct ChunkVec<T> {
    chunks: Vec<Vec<T>>,
    len: usize,
}

impl<T> ChunkVec<T> {
    const fn new() -> Self {
        ChunkVec {
            chunks: Vec::new(),
            len: 0,
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    fn push(&mut self, value: T) -> usize {
        match self.chunks.last_mut() {
            Some(c) if c.len() < CHUNK_LEN => {
                if c.len() == c.capacity() {
                    // Only the first chunk starts small; grow it geometrically
                    // but never past one chunk.
                    let target = (c.capacity() * 2).clamp(FIRST_CHUNK_LEN, CHUNK_LEN);
                    c.reserve_exact(target - c.len());
                }
                c.push(value);
            }
            _ => {
                let cap = if self.chunks.is_empty() {
                    FIRST_CHUNK_LEN
                } else {
                    CHUNK_LEN
                };
                let mut c = Vec::with_capacity(cap);
                c.push(value);
                self.chunks.push(c);
            }
        }
        self.len += 1;
        self.len - 1
    }

    #[inline]
    fn get(&self, i: usize) -> &T {
        &self.chunks[i >> CHUNK_BITS][i & (CHUNK_LEN - 1)]
    }

    #[inline]
    fn get_mut(&mut self, i: usize) -> &mut T {
        &mut self.chunks[i >> CHUNK_BITS][i & (CHUNK_LEN - 1)]
    }

    fn iter(&self) -> impl Iterator<Item = &T> {
        self.chunks.iter().flatten()
    }

    fn heap_bytes(&self) -> usize {
        self.chunks.capacity() * size_of::<Vec<T>>()
            + self
                .chunks
                .iter()
                .map(|c| c.capacity() * size_of::<T>())
                .sum::<usize>()
    }
}

/// log2 of the size of one name chunk (1 MiB).
const NAME_CHUNK_BITS: u32 = 20;
const NAME_CHUNK: usize = 1 << NAME_CHUNK_BITS;
/// Offsets are u32, so at most 4096 chunks = 4 GiB of names.
const MAX_NAME_CHUNKS: usize = 1 << (32 - NAME_CHUNK_BITS);
/// Longer names are truncated (on a char boundary). Real names are at most
/// 765 bytes (255 UTF-16 units); this only guards against absurd roots.
const MAX_NAME_LEN: usize = 64 * 1024;

/// All names of a tree, `varint(len) + UTF-8 bytes`, in 1 MiB chunks. A name
/// never straddles two chunks, so its offset is `chunk << 20 | position`.
#[derive(Clone, Debug, Default)]
struct NameStore {
    chunks: Vec<Vec<u8>>,
}

impl NameStore {
    fn push(&mut self, name: &str) -> u32 {
        let name = truncate_utf8(name, MAX_NAME_LEN);
        let need = varint_len(name.len()) + name.len();
        if self
            .chunks
            .last()
            .is_none_or(|c| NAME_CHUNK - c.len() < need)
        {
            assert!(
                self.chunks.len() < MAX_NAME_CHUNKS,
                "ringscan: name storage exhausted (4 GiB)"
            );
            let cap = if self.chunks.is_empty() {
                1024
            } else {
                NAME_CHUNK
            };
            self.chunks.push(Vec::with_capacity(cap));
        }
        let ci = self.chunks.len() - 1;
        let chunk = &mut self.chunks[ci];
        if chunk.capacity() - chunk.len() < need {
            let target = (chunk.capacity() * 2)
                .max(chunk.len() + need)
                .min(NAME_CHUNK);
            chunk.reserve_exact(target - chunk.len());
        }
        let offset = (ci << NAME_CHUNK_BITS) | chunk.len();
        write_varint(chunk, name.len());
        chunk.extend_from_slice(name.as_bytes());
        offset as u32
    }

    #[inline]
    fn get(&self, offset: u32) -> &str {
        let chunk = &self.chunks[(offset >> NAME_CHUNK_BITS) as usize];
        let pos = offset as usize & (NAME_CHUNK - 1);
        let (len, n) = read_varint(&chunk[pos..]);
        let bytes = &chunk[pos + n..pos + n + len];
        // SAFETY: `push` copied these bytes verbatim from a `&str` that was
        // truncated on a char boundary, and stored bytes are never modified.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    fn heap_bytes(&self) -> usize {
        self.chunks.capacity() * size_of::<Vec<u8>>()
            + self.chunks.iter().map(Vec::capacity).sum::<usize>()
    }
}

fn varint_len(mut v: usize) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

fn write_varint(buf: &mut Vec<u8>, mut v: usize) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

#[inline]
fn read_varint(b: &[u8]) -> (usize, usize) {
    if b[0] < 0x80 {
        return (b[0] as usize, 1);
    }
    let mut v = 0usize;
    let mut shift = 0;
    for (i, &byte) in b.iter().enumerate() {
        v |= ((byte & 0x7f) as usize) << shift;
        if byte < 0x80 {
            return (v, i + 1);
        }
        shift += 7;
    }
    unreachable!("corrupt name length")
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ---------------------------------------------------------------------------
// Extension interning
// ---------------------------------------------------------------------------

/// FxHash (rustc's hasher): much faster than SipHash for short keys, and the
/// extension table never sees adversarial input that would need DoS safety.
#[derive(Clone, Copy, Default)]
struct FxHasher(u64);

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..chunk.len()].copy_from_slice(chunk);
            self.add(u64::from_le_bytes(w));
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

type FxBuild = BuildHasherDefault<FxHasher>;

/// Longest extension that is interned; longer suffixes (hash-like names,
/// "report.final version") are treated as "no extension".
const MAX_EXT_LEN: usize = 16;

/// Extension of a file name following Windows rules (`PathFindExtension`):
/// the text after the last '.', which may be the first character
/// (".gitignore" -> "gitignore"); empty, whitespace-containing or overlong
/// suffixes are no extension.
fn extension(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    let ext = &name[dot + 1..];
    if ext.is_empty() || ext.len() > MAX_EXT_LEN || ext.chars().any(char::is_whitespace) {
        return None;
    }
    Some(ext)
}

#[derive(Clone, Debug)]
struct ExtTable {
    /// id -> lowercase extension; index 0 is "" (none).
    names: Vec<Box<str>>,
    ids: HashMap<Box<str>, u16, FxBuild>,
}

impl ExtTable {
    fn new() -> Self {
        ExtTable {
            names: vec!["".into()],
            ids: HashMap::default(),
        }
    }

    /// Interned id of the extension of `file_name` (0 = none).
    fn id_for_name(&mut self, file_name: &str) -> u16 {
        let Some(ext) = extension(file_name) else {
            return 0;
        };
        if ext.is_ascii() {
            let mut buf = [0u8; MAX_EXT_LEN];
            let buf = &mut buf[..ext.len()];
            buf.copy_from_slice(ext.as_bytes());
            buf.make_ascii_lowercase();
            // ASCII stays valid UTF-8 after lowercasing.
            self.intern(std::str::from_utf8(buf).unwrap_or_default())
        } else {
            self.intern(&ext.to_lowercase())
        }
    }

    /// Id of an already lowercase extension; 0 once the table is full.
    fn intern(&mut self, ext: &str) -> u16 {
        if ext.is_empty() {
            return 0;
        }
        if let Some(&id) = self.ids.get(ext) {
            return id;
        }
        if self.names.len() > usize::from(u16::MAX) {
            return 0;
        }
        let id = self.names.len() as u16;
        self.names.push(ext.into());
        self.ids.insert(ext.into(), id);
        id
    }

    fn name(&self, id: u16) -> &str {
        self.names.get(usize::from(id)).map_or("", |s| s)
    }

    fn heap_bytes(&self) -> usize {
        let strings: usize = self.names.iter().map(|s| s.len()).sum::<usize>() * 2;
        self.names.capacity() * size_of::<Box<str>>()
            + self.ids.capacity() * (size_of::<Box<str>>() + size_of::<u16>() + 8)
            + strings
    }
}

// ---------------------------------------------------------------------------
// Name comparison helpers (shared with layout / stats)
// ---------------------------------------------------------------------------

/// Case-insensitive name order with a byte-order tie break (deterministic).
pub(crate) fn cmp_names(a: &str, b: &str) -> Ordering {
    let ci = if a.is_ascii() && b.is_ascii() {
        let la = a.bytes().map(|c| c.to_ascii_lowercase());
        let lb = b.bytes().map(|c| c.to_ascii_lowercase());
        la.cmp(lb)
    } else {
        let la = a.chars().flat_map(char::to_lowercase);
        let lb = b.chars().flat_map(char::to_lowercase);
        la.cmp(lb)
    };
    ci.then_with(|| a.cmp(b))
}

/// Case-insensitive equality (Unicode simple lowercase).
pub(crate) fn eq_ci(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(b);
    }
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

/// Strips a Win32 verbatim prefix (`\\?\C:\x`, `\\?\UNC\srv\share`).
fn strip_verbatim(s: &str) -> &str {
    for prefix in [r"\\?\", r"\??\", "//?/"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            if rest
                .get(..4)
                .is_some_and(|p| p.eq_ignore_ascii_case(r"UNC\"))
            {
                return &rest[4..];
            }
            return rest;
        }
    }
    s
}

/// Non-empty path components split on '\' and '/'.
fn components(s: &str) -> impl Iterator<Item = &str> {
    strip_verbatim(s)
        .split(['\\', '/'])
        .filter(|c| !c.is_empty())
}

/// If the components of `name` are a case-insensitive prefix of `rest`,
/// returns the remainder. Names without components never match.
fn strip_name<'q>(rest: &'q [&'q str], name: &str) -> Option<&'q [&'q str]> {
    let mut i = 0;
    for c in components(name) {
        if !rest.get(i).is_some_and(|q| eq_ci(q, c)) {
            return None;
        }
        i += 1;
    }
    (i > 0).then(|| &rest[i..])
}

/// Clamps unix seconds into the stored u32 range (0 = unknown).
fn clamp_mtime(mtime: i64) -> u32 {
    mtime.clamp(0, i64::from(u32::MAX)) as u32
}

/// Normalizes caller-supplied flags: callers cannot create tombstones,
/// FREE / UNACCOUNTED nodes are always virtual and HARDLINK applies to real
/// files only (directories cannot be hard-linked, virtual nodes are not
/// links).
fn normalize_flags(mut flags: u16) -> u16 {
    flags &= !DELETED;
    if flags & (FREE | UNACCOUNTED) != 0 {
        flags |= VIRTUAL;
    }
    if flags & (DIR | VIRTUAL) != 0 {
        flags &= !HARDLINK;
    }
    flags
}

// ---------------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------------

/// Arena tree of a scan. See the [module docs](self) for storage and
/// aggregation rules.
#[derive(Clone, Debug)]
pub struct Tree {
    nodes: ChunkVec<Node>,
    dirs: ChunkVec<DirInfo>,
    names: NameStore,
    exts: ExtTable,
    /// FREE nodes (may contain tombstones until the next prune).
    free_nodes: Vec<NodeId>,
    /// First node of the pending raw range `raw_start..len`, or NONE.
    raw_start: NodeId,
    /// Number of tombstones.
    dead: usize,
    generation: u64,
}

/// Node data for the internal push.
struct NewNode<'a> {
    parent: NodeId,
    name: &'a str,
    logical: u64,
    alloc: u64,
    flags: u16,
    mtime: u32,
    /// Pre-computed extension id (graft) or None to derive it from the name.
    ext: Option<u16>,
}

#[allow(clippy::len_without_is_empty)] // a tree always has its root
impl Tree {
    /// Creates a tree with only a root. The root is always a directory; pass
    /// [`flags::VIRTUAL`] for a synthetic root such as "This PC" (it is then
    /// left out of [`Tree::path`]). A non-virtual root's name is the path of
    /// the scanned folder, e.g. `C:\` or `C:\Users\x`.
    pub fn new(root_name: &str, root_flags: u16) -> Tree {
        let mut tree = Tree {
            nodes: ChunkVec::new(),
            dirs: ChunkVec::new(),
            names: NameStore::default(),
            exts: ExtTable::new(),
            free_nodes: Vec::new(),
            raw_start: NONE,
            dead: 0,
            generation: 0,
        };
        let flags = (root_flags | DIR) & !(DELETED | FREE | UNACCOUNTED | HARDLINK);
        tree.push_node(NewNode {
            parent: NONE,
            name: root_name,
            logical: 0,
            alloc: 0,
            flags,
            mtime: 0,
            ext: Some(0),
        });
        tree
    }

    // ----- reading ---------------------------------------------------------

    /// The root node (always id 0).
    #[inline]
    pub fn root(&self) -> NodeId {
        ROOT
    }

    /// Number of nodes in the arena, tombstones included. Valid ids are
    /// `0..len()`.
    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Number of live (non-DELETED) nodes, the root included.
    #[inline]
    pub fn live_len(&self) -> usize {
        self.nodes.len() - self.dead
    }

    /// Incremented by every mutation.
    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The node `id`. Panics if `id >= len()`.
    #[inline]
    pub fn node(&self, id: NodeId) -> &Node {
        self.nodes.get(id as usize)
    }

    /// The node `id`, or None if the id is out of range (e.g. an id kept
    /// from a previous tree).
    #[inline]
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        ((id as usize) < self.nodes.len()).then(|| self.nodes.get(id as usize))
    }

    #[inline]
    fn node_mut(&mut self, id: NodeId) -> &mut Node {
        self.nodes.get_mut(id as usize)
    }

    /// Name of the node (for the root: the scanned path or e.g. "This PC").
    #[inline]
    pub fn name(&self, id: NodeId) -> &str {
        self.names.get(self.node(id).name)
    }

    /// Lowercase extension for an id from [`Node::ext`] ("" for 0).
    pub fn ext_name(&self, ext: u16) -> &str {
        self.exts.name(ext)
    }

    /// Number of extension ids in use, including 0 (so ids are `0..count`).
    pub fn ext_count(&self) -> usize {
        self.exts.names.len()
    }

    /// Size of `id` in `mode` (aggregated for directories; FREE nodes report
    /// their own size but are never part of an ancestor's size).
    #[inline]
    pub fn size(&self, id: NodeId, mode: SizeMode) -> u64 {
        self.node(id).size(mode)
    }

    /// Files strictly below `id` (0 for leaves).
    pub fn files(&self, id: NodeId) -> u64 {
        let n = self.node(id);
        if n.dir == NONE {
            0
        } else {
            u64::from(self.dirs.get(n.dir as usize).files)
        }
    }

    /// Directories strictly below `id` (0 for leaves).
    pub fn dirs(&self, id: NodeId) -> u64 {
        let n = self.node(id);
        if n.dir == NONE {
            0
        } else {
            u64::from(self.dirs.get(n.dir as usize).dirs)
        }
    }

    /// True if `id` is in range and not a tombstone.
    #[inline]
    pub fn is_live(&self, id: NodeId) -> bool {
        self.get(id).is_some_and(Node::is_live)
    }

    /// `id` itself if live, else its nearest live ancestor (the root at
    /// worst). Use it to move the UI focus off a removed node.
    pub fn live_ancestor(&self, id: NodeId) -> NodeId {
        let mut cur = if (id as usize) < self.len() { id } else { ROOT };
        while !self.node(cur).is_live() {
            cur = self.node(cur).parent;
        }
        cur
    }

    /// Children of `id` (sorted order after a sort, insertion order
    /// otherwise). Empty for leaves and tombstones.
    pub fn children(&self, id: NodeId) -> Children<'_> {
        let n = self.node(id);
        let tail = if n.dir != NONE && n.is_live() {
            self.dirs.get(n.dir as usize).tail
        } else {
            NONE
        };
        let next = if tail == NONE {
            NONE
        } else {
            self.node(tail).next
        };
        Children {
            tree: self,
            next,
            tail,
        }
    }

    /// Number of direct children (O(children)).
    pub fn child_count(&self, id: NodeId) -> usize {
        self.children(id).count()
    }

    /// True if `id` has at least one child (O(1)).
    #[inline]
    pub fn has_children(&self, id: NodeId) -> bool {
        self.children(id).next != NONE
    }

    /// All nodes strictly below `id`, depth-first pre-order, in child order.
    pub fn descendants(&self, id: NodeId) -> Descendants<'_> {
        let mut stack = Vec::new();
        let first = self.children(id);
        if first.next != NONE {
            stack.push((first.next, first.tail));
        }
        Descendants { tree: self, stack }
    }

    /// Ancestors of `id` from its parent up to the root.
    pub fn ancestors(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        // `successors` computes the next item eagerly, so guard NONE inside.
        let first = self.node(id).parent;
        std::iter::successors((first != NONE).then_some(first), |&p| {
            let up = self.node(p).parent;
            (up != NONE).then_some(up)
        })
    }

    /// Distance from the root (root = 0).
    pub fn depth(&self, id: NodeId) -> usize {
        self.ancestors(id).count()
    }

    /// True if `anc` is `id` or one of its ancestors.
    pub fn is_ancestor(&self, anc: NodeId, id: NodeId) -> bool {
        anc == id || self.ancestors(id).any(|a| a == anc)
    }

    /// All live nodes in arena order (root first).
    pub fn live_nodes(&self) -> impl Iterator<Item = (NodeId, &Node)> + '_ {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.is_live())
            .map(|(i, n)| (i as NodeId, n))
    }

    /// Live FREE nodes.
    pub fn free_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.free_nodes
            .iter()
            .copied()
            .filter(|&f| self.node(f).is_live())
    }

    /// Full path with Windows separators: `C:\Users\x`. A VIRTUAL root is
    /// left out (so under "This PC" a drive node "C:\" yields `C:\`); a
    /// separator is not doubled after names that end in one (drive roots).
    /// Virtual nodes below the root are included by name.
    pub fn path(&self, id: NodeId) -> String {
        let mut chain = vec![id];
        chain.extend(self.ancestors(id));
        if chain.last() == Some(&ROOT) && self.node(ROOT).is_virtual() {
            chain.pop();
        }
        let mut out = String::new();
        for &n in chain.iter().rev() {
            if !out.is_empty() && !out.ends_with(['\\', '/']) {
                out.push('\\');
            }
            out.push_str(self.name(n));
        }
        out
    }

    /// Finds a live node by path, case-insensitively. Accepts `\` and `/`,
    /// repeated and trailing separators and the `\\?\` prefix: `C:\x`,
    /// `c:/X/`, `\\?\C:\x`. The path must start with the root's name unless
    /// the root is VIRTUAL. An exact-case child match is preferred.
    pub fn find_path(&self, path: &str) -> Option<NodeId> {
        let query: Vec<&str> = components(path).collect();
        let mut rest = &query[..];
        if !self.node(ROOT).is_virtual() {
            rest = strip_name(rest, self.name(ROOT))?;
        }
        let mut cur = ROOT;
        while let Some(&first) = rest.first() {
            let mut found: Option<(NodeId, &[&str])> = None;
            for c in self.children(cur) {
                let name = self.name(c);
                if name == first {
                    found = Some((c, &rest[1..]));
                    break;
                }
                if found.is_none()
                    && let Some(r) = strip_name(rest, name)
                {
                    found = Some((c, r));
                }
            }
            let (child, r) = found?;
            cur = child;
            rest = r;
        }
        Some(cur)
    }

    /// Child of `parent` with the given name (case-insensitive, exact case
    /// preferred).
    pub fn find_child(&self, parent: NodeId, name: &str) -> Option<NodeId> {
        let mut ci = None;
        for c in self.children(parent) {
            let n = self.name(c);
            if n == name {
                return Some(c);
            }
            if ci.is_none() && eq_ci(n, name) {
                ci = Some(c);
            }
        }
        ci
    }

    /// Approximate heap bytes owned by the tree (arena chunks, names,
    /// extension table).
    pub fn memory_bytes(&self) -> usize {
        self.nodes.heap_bytes()
            + self.dirs.heap_bytes()
            + self.names.heap_bytes()
            + self.exts.heap_bytes()
            + self.free_nodes.capacity() * size_of::<NodeId>()
    }

    // ----- live insertion --------------------------------------------------

    /// Adds a directory (no own size) and updates all ancestors.
    pub fn add_dir(&mut self, parent: NodeId, name: &str, flags: u16, mtime: i64) -> NodeId {
        self.insert(parent, name, 0, 0, flags | DIR, mtime, false)
    }

    /// Adds a file (non-DIR leaf) and updates all ancestors. HARDLINK files
    /// get zero sizes.
    pub fn add_file(
        &mut self,
        parent: NodeId,
        name: &str,
        logical: u64,
        alloc: u64,
        flags: u16,
        mtime: i64,
    ) -> NodeId {
        self.insert(parent, name, logical, alloc, flags & !DIR, mtime, false)
    }

    /// General live insertion: a directory if `flags` has DIR (`logical` /
    /// `alloc` are then its *own* size), a leaf otherwise.
    ///
    /// The parent must be a live directory; otherwise the new node is
    /// created as an unlinked tombstone (its id is valid but dead).
    pub fn add_node(
        &mut self,
        parent: NodeId,
        name: &str,
        logical: u64,
        alloc: u64,
        flags: u16,
        mtime: i64,
    ) -> NodeId {
        self.insert(parent, name, logical, alloc, flags, mtime, false)
    }

    /// Adds a synthetic leaf: `kind_flag` is FREE, UNACCOUNTED, RECYCLE, 0...
    /// (VIRTUAL is added). Logical and allocated size are both `size`.
    /// Pass DIR in `kind_flag` for a virtual container (e.g. `<orphaned>`).
    pub fn add_virtual(&mut self, parent: NodeId, name: &str, kind_flag: u16, size: u64) -> NodeId {
        self.insert(parent, name, size, size, kind_flag | VIRTUAL, 0, false)
    }

    /// Inserts one listed directory's entries below `parent` in order, with a
    /// single ancestor update for the whole batch. Returns `(id, index in
    /// entries)` of every directory entry (REPARSE ones included; the walker
    /// skips those). A tombstone or non-directory parent ignores the batch
    /// and returns nothing.
    pub fn add_batch(&mut self, parent: NodeId, entries: &[RawEntry]) -> Vec<(NodeId, usize)> {
        self.finalize_raw();
        let p = *self.node(parent);
        if !p.is_live() || !p.is_dir() || entries.is_empty() {
            return Vec::new();
        }
        let mut dirs = Vec::new();
        let mut total = Delta::ZERO;
        for (i, e) in entries.iter().enumerate() {
            let flags = normalize_flags(if e.is_dir {
                e.flags | DIR
            } else {
                e.flags & !DIR
            });
            let (logical, alloc) = if flags & HARDLINK != 0 {
                (0, 0)
            } else {
                (e.logical, e.alloc)
            };
            let id = self.push_node(NewNode {
                parent,
                name: &e.name,
                logical,
                alloc,
                flags,
                mtime: clamp_mtime(e.mtime),
                ext: None,
            });
            self.link_child(parent, id);
            total += Delta::leaf(flags, logical, alloc);
            if flags & FREE != 0 {
                self.free_nodes.push(id);
            }
            if e.is_dir {
                dirs.push((id, i));
            }
        }
        self.add_up(parent, total);
        self.generation += 1;
        dirs
    }

    /// Changes the size of a virtual leaf (e.g. free space after a delete)
    /// and adjusts the ancestors accordingly (not for FREE nodes, which are
    /// never aggregated). Ignored for other nodes.
    pub fn set_virtual_size(&mut self, id: NodeId, size: u64) {
        self.finalize_raw();
        let n = *self.node(id);
        if !n.is_live() || !n.is_virtual() || n.is_dir() {
            return;
        }
        let old = self.contrib(id);
        let m = self.node_mut(id);
        m.logical = size;
        m.alloc = size;
        let new = self.contrib(id);
        self.sub_up(n.parent, old);
        self.add_up(n.parent, new);
        self.generation += 1;
    }

    /// Sets and clears informational flags ([`flags::MUTABLE`]; other bits
    /// in `set` / `clear` are ignored). No-op on tombstones.
    pub fn set_flags(&mut self, id: NodeId, set: u16, clear: u16) {
        let n = self.node_mut(id);
        if n.is_live() {
            n.flags = (n.flags | (set & flags::MUTABLE)) & !(clear & flags::MUTABLE);
        }
        self.generation += 1;
    }

    // ----- bulk (raw) insertion -------------------------------------------

    /// Raw variant of [`Tree::add_dir`]: ancestors are not updated until
    /// [`Tree::recompute_aggregates`].
    pub fn add_dir_raw(&mut self, parent: NodeId, name: &str, flags: u16, mtime: i64) -> NodeId {
        self.insert(parent, name, 0, 0, flags | DIR, mtime, true)
    }

    /// Raw variant of [`Tree::add_file`].
    pub fn add_file_raw(
        &mut self,
        parent: NodeId,
        name: &str,
        logical: u64,
        alloc: u64,
        flags: u16,
        mtime: i64,
    ) -> NodeId {
        self.insert(parent, name, logical, alloc, flags & !DIR, mtime, true)
    }

    /// Raw variant of [`Tree::add_node`] (a DIR's sizes are its own size,
    /// e.g. the NTFS index allocation).
    pub fn add_node_raw(
        &mut self,
        parent: NodeId,
        name: &str,
        logical: u64,
        alloc: u64,
        flags: u16,
        mtime: i64,
    ) -> NodeId {
        self.insert(parent, name, logical, alloc, flags, mtime, true)
    }

    /// Aggregates all nodes added with the raw API since the last call, in
    /// one pass over them (children always have larger ids than their
    /// parents, so a reverse sweep is a valid bottom-up order). Every other
    /// mutating method calls this first, so mixing raw and live insertion is
    /// safe; readers just see stale ancestors until then.
    pub fn recompute_aggregates(&mut self) {
        let start = self.raw_start;
        if start == NONE {
            return;
        }
        self.raw_start = NONE;
        // Contributions to nodes outside the raw range must also reach all
        // their ancestors; collect them and walk up once per such node.
        let mut boundary: Vec<(NodeId, Delta)> = Vec::new();
        for id in (start..self.len() as NodeId).rev() {
            let n = *self.node(id);
            if !n.is_live() {
                continue;
            }
            let d = self.contrib(id);
            if n.parent >= start {
                self.add_one(n.parent, d);
            } else {
                boundary.push((n.parent, d));
            }
        }
        boundary.sort_unstable_by_key(|&(p, _)| p);
        let mut i = 0;
        while i < boundary.len() {
            let (p, mut d) = boundary[i];
            i += 1;
            while i < boundary.len() && boundary[i].0 == p {
                d += boundary[i].1;
                i += 1;
            }
            self.add_up(p, d);
        }
        self.generation += 1;
    }

    #[inline]
    fn finalize_raw(&mut self) {
        if self.raw_start != NONE {
            self.recompute_aggregates();
        }
    }

    // ----- reordering ------------------------------------------------------

    /// Sorts the children of `id` by size in `mode`, descending (ties by name,
    /// case-insensitive). Later insertions are appended unsorted.
    pub fn sort_children(&mut self, id: NodeId, mode: SizeMode) {
        self.finalize_raw();
        let mut buf = Vec::new();
        self.sort_list(id, mode, &mut buf);
        self.generation += 1;
    }

    /// Sorts the children of every directory (see [`Tree::sort_children`]).
    pub fn sort_all(&mut self, mode: SizeMode) {
        self.finalize_raw();
        let mut buf = Vec::new();
        for id in 0..self.len() as NodeId {
            let n = self.node(id);
            if n.is_live() && n.dir != NONE {
                self.sort_list(id, mode, &mut buf);
            }
        }
        self.generation += 1;
    }

    fn sort_list(&mut self, id: NodeId, mode: SizeMode, buf: &mut Vec<(u64, NodeId)>) {
        buf.clear();
        buf.extend(self.children(id).map(|c| (self.node(c).size(mode), c)));
        if buf.len() < 2 {
            return;
        }
        buf.sort_unstable_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| cmp_names(self.name(a.1), self.name(b.1)))
                .then(a.1.cmp(&b.1))
        });
        for w in buf.windows(2) {
            self.node_mut(w[0].1).next = w[1].1;
        }
        let first = buf[0].1;
        let last = buf[buf.len() - 1].1;
        self.node_mut(last).next = first;
        let dir = self.node(id).dir as usize;
        self.dirs.get_mut(dir).tail = last;
    }

    // ----- removal ---------------------------------------------------------

    /// Removes `id` and its subtree (after a delete / recycle): unlinks it,
    /// subtracts its sizes and counts from all ancestors and turns every node
    /// of the subtree into a DELETED tombstone. No-op for the root and for
    /// tombstones.
    pub fn remove(&mut self, id: NodeId) {
        self.finalize_raw();
        if id == ROOT || !self.is_live(id) {
            return;
        }
        let parent = self.node(id).parent;
        let d = self.contrib(id);
        self.unlink_child(parent, id);
        self.sub_up(parent, d);
        self.kill_subtree(id);
        self.prune_free();
        self.generation += 1;
    }

    /// Removes all children of `id` (before rescanning a folder); `id` keeps
    /// its own size and loses its ERROR flag.
    pub fn clear_children(&mut self, id: NodeId) {
        self.finalize_raw();
        if !self.is_live(id) || !self.node(id).is_dir() {
            return;
        }
        let kids: Vec<NodeId> = self.children(id).collect();
        let mut d = Delta::ZERO;
        for &c in &kids {
            d += self.contrib(c);
        }
        for &c in &kids {
            self.kill_subtree(c);
        }
        let dir = self.node(id).dir as usize;
        self.dirs.get_mut(dir).tail = NONE;
        self.sub_up(id, d);
        self.node_mut(id).flags &= !ERROR;
        self.prune_free();
        self.generation += 1;
    }

    /// Copies the live children of `other`'s root, with their subtrees, below
    /// `at` (appended in `other`'s child order; nodes are appended to the
    /// arena in depth-first pre-order) and adds their totals to `at` and its
    /// ancestors. Names, sizes, counts, flags, mtimes and extensions are
    /// preserved; `other`'s root itself (name, flags, own size) is not
    /// copied. `other` should have no pending raw nodes. No-op if `at` is not
    /// a live directory.
    ///
    /// Typical use: rescan a folder into a private tree without contending
    /// for the shared lock, then `clear_children(at)` + `graft(at, &fresh)`.
    pub fn graft(&mut self, at: NodeId, other: &Tree) {
        self.finalize_raw();
        debug_assert!(
            other.raw_start == NONE,
            "graft source has pending raw nodes"
        );
        if !self.is_live(at) || !self.node(at).is_dir() {
            return;
        }
        let top = other.root();
        let mut map = vec![NONE; other.len()];
        map[top as usize] = at;
        let mut ext_map = vec![u16::MAX; other.ext_count()];
        ext_map[0] = 0;
        let mut total = Delta::ZERO;
        let mut cur = other.next_in_subtree(top, top);
        while cur != NONE {
            let o = *other.node(cur);
            let ext = match ext_map[usize::from(o.ext)] {
                u16::MAX => {
                    let e = self.exts.intern(other.ext_name(o.ext));
                    ext_map[usize::from(o.ext)] = e;
                    e
                }
                e => e,
            };
            let id = self.push_node(NewNode {
                parent: map[o.parent as usize],
                name: other.name(cur),
                logical: o.logical,
                alloc: o.alloc,
                flags: o.flags,
                mtime: o.mtime,
                ext: Some(ext),
            });
            if o.dir != NONE {
                let src = other.dirs.get(o.dir as usize);
                let dst = self.node(id).dir as usize;
                let dst = self.dirs.get_mut(dst);
                dst.files = src.files;
                dst.dirs = src.dirs;
            }
            self.link_child(map[o.parent as usize], id);
            if o.flags & FREE != 0 {
                self.free_nodes.push(id);
            }
            if o.parent == top {
                total += self.contrib(id);
            }
            map[cur as usize] = id;
            cur = other.next_in_subtree(cur, top);
        }
        self.add_up(at, total);
        self.generation += 1;
    }

    // ----- internals -------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn insert(
        &mut self,
        parent: NodeId,
        name: &str,
        logical: u64,
        alloc: u64,
        flags: u16,
        mtime: i64,
        raw: bool,
    ) -> NodeId {
        if !raw {
            self.finalize_raw();
        }
        let mut flags = normalize_flags(flags);
        let (logical, alloc) = if flags & HARDLINK != 0 {
            (0, 0)
        } else {
            (logical, alloc)
        };
        let p = *self.node(parent);
        let attach = p.is_live() && p.is_dir();
        if !attach {
            flags |= DELETED;
        }
        let id = self.push_node(NewNode {
            parent,
            name,
            logical,
            alloc,
            flags,
            mtime: clamp_mtime(mtime),
            ext: None,
        });
        if attach {
            self.link_child(parent, id);
            if flags & FREE != 0 {
                self.free_nodes.push(id);
            }
            if raw {
                if self.raw_start == NONE {
                    self.raw_start = id;
                }
            } else {
                self.add_up(parent, Delta::leaf(flags, logical, alloc));
            }
        } else {
            self.dead += 1;
        }
        self.generation += 1;
        id
    }

    fn push_node(&mut self, n: NewNode<'_>) -> NodeId {
        assert!(
            self.nodes.len() < NONE as usize,
            "ringscan: too many nodes for u32 ids"
        );
        let name = self.names.push(n.name);
        let ext = match n.ext {
            Some(e) => e,
            None if n.flags & (DIR | VIRTUAL) != 0 => 0,
            None => self.exts.id_for_name(n.name),
        };
        let dir = if n.flags & DIR != 0 {
            self.dirs.push(DirInfo {
                tail: NONE,
                files: 0,
                dirs: 0,
            }) as u32
        } else {
            NONE
        };
        self.nodes.push(Node {
            logical: n.logical,
            alloc: n.alloc,
            parent: n.parent,
            next: NONE,
            name,
            dir,
            mtime: n.mtime,
            flags: n.flags,
            ext,
        }) as NodeId
    }

    /// Appends `id` at the end of `parent`'s circular child list.
    fn link_child(&mut self, parent: NodeId, id: NodeId) {
        let dir = self.node(parent).dir as usize;
        let tail = self.dirs.get(dir).tail;
        if tail == NONE {
            self.node_mut(id).next = id;
        } else {
            let head = self.node(tail).next;
            self.node_mut(id).next = head;
            self.node_mut(tail).next = id;
        }
        self.dirs.get_mut(dir).tail = id;
    }

    /// Removes `id` from `parent`'s child list (O(siblings)).
    fn unlink_child(&mut self, parent: NodeId, id: NodeId) {
        let dir = self.node(parent).dir as usize;
        let tail = self.dirs.get(dir).tail;
        let mut pred = tail;
        while self.node(pred).next != id {
            pred = self.node(pred).next;
            debug_assert!(pred != tail, "node not in its parent's child list");
        }
        if pred == id {
            self.dirs.get_mut(dir).tail = NONE;
        } else {
            let next = self.node(id).next;
            self.node_mut(pred).next = next;
            if tail == id {
                self.dirs.get_mut(dir).tail = pred;
            }
        }
        self.node_mut(id).next = NONE;
    }

    /// What `id` (with its subtree) contributes to its parent.
    fn contrib(&self, id: NodeId) -> Delta {
        let n = self.node(id);
        let mut d = Delta::leaf(n.flags, n.logical, n.alloc);
        if n.dir != NONE && n.is_live() {
            let di = self.dirs.get(n.dir as usize);
            d.files += di.files;
            d.dirs += di.dirs;
        }
        d
    }

    /// Adds `d` to node `p` only.
    fn add_one(&mut self, p: NodeId, d: Delta) {
        let n = self.node_mut(p);
        n.logical += d.logical;
        n.alloc += d.alloc;
        let dir = n.dir as usize;
        let di = self.dirs.get_mut(dir);
        di.files += d.files;
        di.dirs += d.dirs;
    }

    /// Adds `d` to `p` and all its ancestors.
    fn add_up(&mut self, mut p: NodeId, d: Delta) {
        if d == Delta::ZERO {
            return;
        }
        while p != NONE {
            self.add_one(p, d);
            p = self.node(p).parent;
        }
    }

    /// Subtracts `d` from `p` and all its ancestors.
    fn sub_up(&mut self, mut p: NodeId, d: Delta) {
        if d == Delta::ZERO {
            return;
        }
        while p != NONE {
            let n = self.node_mut(p);
            n.logical -= d.logical;
            n.alloc -= d.alloc;
            let (dir, parent) = (n.dir as usize, n.parent);
            let di = self.dirs.get_mut(dir);
            di.files -= d.files;
            di.dirs -= d.dirs;
            p = parent;
        }
    }

    /// Next node after `cur` in a pre-order walk of the subtree of `top`
    /// (NONE when done). Follows links only, so it also works on tombstones.
    fn next_in_subtree(&self, cur: NodeId, top: NodeId) -> NodeId {
        let n = self.node(cur);
        if n.dir != NONE {
            let tail = self.dirs.get(n.dir as usize).tail;
            if tail != NONE {
                return self.node(tail).next;
            }
        }
        let mut c = cur;
        loop {
            if c == top {
                return NONE;
            }
            let cn = self.node(c);
            let ptail = self.dirs.get(self.node(cn.parent).dir as usize).tail;
            if c != ptail {
                return cn.next;
            }
            c = cn.parent;
        }
    }

    /// Flags `top` and everything below it DELETED.
    fn kill_subtree(&mut self, top: NodeId) {
        let mut cur = top;
        while cur != NONE {
            let next = self.next_in_subtree(cur, top);
            let n = self.node_mut(cur);
            if n.flags & DELETED == 0 {
                n.flags |= DELETED;
                self.dead += 1;
            }
            cur = next;
        }
    }

    fn prune_free(&mut self) {
        let nodes = &self.nodes;
        self.free_nodes.retain(|&f| nodes.get(f as usize).is_live());
    }
}

/// Iterator over the children of a node; see [`Tree::children`].
#[derive(Clone)]
pub struct Children<'a> {
    tree: &'a Tree,
    next: NodeId,
    tail: NodeId,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        if self.next == NONE {
            return None;
        }
        let cur = self.next;
        self.next = if cur == self.tail {
            NONE
        } else {
            self.tree.node(cur).next
        };
        Some(cur)
    }
}

/// Pre-order iterator over a subtree; see [`Tree::descendants`].
///
/// Keeps one `(next sibling, last sibling)` cursor per open level, so each
/// step reads one node (plus the directory record of a directory) instead
/// of re-reading the parent's record for every leaf.
#[derive(Clone)]
pub struct Descendants<'a> {
    tree: &'a Tree,
    stack: Vec<(NodeId, NodeId)>,
}

impl Iterator for Descendants<'_> {
    type Item = NodeId;

    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        let (cur, tail) = *self.stack.last()?;
        let n = self.tree.node(cur);
        if cur == tail {
            self.stack.pop();
        } else if let Some(top) = self.stack.last_mut() {
            top.0 = n.next;
        }
        if n.dir != NONE {
            let child_tail = self.tree.dirs.get(n.dir as usize).tail;
            if child_tail != NONE {
                self.stack
                    .push((self.tree.node(child_tail).next, child_tail));
            }
        }
        Some(cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags::*;

    fn entry(name: &str, is_dir: bool, size: u64) -> RawEntry {
        RawEntry {
            name: name.into(),
            is_dir,
            logical: size,
            alloc: size,
            ..Default::default()
        }
    }

    fn names(t: &Tree, ids: impl Iterator<Item = NodeId>) -> Vec<String> {
        ids.map(|i| t.name(i).to_string()).collect()
    }

    #[test]
    fn node_is_40_bytes() {
        assert_eq!(size_of::<Node>(), 40);
        assert_eq!(size_of::<DirInfo>(), 12);
    }

    #[test]
    fn new_tree_has_dir_root() {
        let t = Tree::new("C:\\", 0);
        assert_eq!(t.root(), 0);
        assert_eq!(t.len(), 1);
        assert_eq!(t.live_len(), 1);
        assert_eq!(t.name(t.root()), "C:\\");
        assert!(t.node(0).is_dir());
        assert_eq!(t.node(0).parent(), NONE);
        assert_eq!(t.children(0).count(), 0);
        assert!(t.get(1).is_none());
        let t = Tree::new("This PC", VIRTUAL | DELETED | FREE);
        assert!(t.node(0).is_virtual() && t.node(0).is_live() && !t.node(0).has(FREE));
    }

    #[test]
    fn live_aggregation_and_counts() {
        let mut t = Tree::new("C:\\", 0);
        let a = t.add_dir(0, "a", 0, 100);
        let b = t.add_dir(a, "b", HIDDEN, 0);
        let f1 = t.add_file(b, "x.TXT", 10, 4096, 0, 5);
        let _f2 = t.add_file(a, "y.rs", 20, 4096, 0, 0);
        assert_eq!(t.size(0, SizeMode::Logical), 30);
        assert_eq!(t.size(0, SizeMode::Allocated), 8192);
        assert_eq!(t.size(a, SizeMode::Logical), 30);
        assert_eq!(t.size(b, SizeMode::Logical), 10);
        assert_eq!((t.files(0), t.dirs(0)), (2, 2));
        assert_eq!((t.files(a), t.dirs(a)), (2, 1));
        assert_eq!((t.files(f1), t.dirs(f1)), (0, 0));
        assert_eq!(t.node(f1).mtime(), 5);
        assert_eq!(t.node(a).mtime(), 100);
        assert!(t.node(b).has(HIDDEN));
        assert_eq!(t.ext_name(t.node(f1).ext()), "txt");
        assert_eq!(t.node(a).ext(), 0);
    }

    #[test]
    fn hardlinks_have_zero_size_but_count() {
        let mut t = Tree::new("r", 0);
        let h = t.add_file(0, "link.dll", 100, 100, HARDLINK, 0);
        assert_eq!(t.size(h, SizeMode::Logical), 0);
        assert_eq!(t.size(0, SizeMode::Allocated), 0);
        assert_eq!(t.files(0), 1);
        let ids = t.add_batch(
            0,
            &[RawEntry {
                flags: HARDLINK,
                ..entry("l2", false, 7)
            }],
        );
        assert!(ids.is_empty());
        assert_eq!(t.size(0, SizeMode::Logical), 0);
        assert_eq!(t.files(0), 2);
    }

    #[test]
    fn virtual_nodes_rules() {
        let mut t = Tree::new("This PC", VIRTUAL);
        let c = t.add_dir(0, "C:\\", 0, 0);
        t.add_file(c, "f", 600, 600, 0, 0);
        let free = t.add_virtual(c, "<free>", FREE, 400);
        let un = t.add_virtual(c, "<unaccounted>", UNACCOUNTED, 50);
        let orphan = t.add_virtual(c, "<orphaned>", DIR, 0);
        t.add_file(orphan, "o", 5, 5, 0, 0);
        // FREE is not aggregated, UNACCOUNTED is; neither is counted.
        assert_eq!(t.size(c, SizeMode::Allocated), 655);
        assert_eq!(t.size(0, SizeMode::Allocated), 655);
        assert_eq!(t.size(free, SizeMode::Allocated), 400);
        assert_eq!(t.size(un, SizeMode::Logical), 50);
        assert_eq!(t.files(c), 2);
        assert_eq!(t.dirs(c), 0, "virtual container is not a dir");
        assert_eq!(t.dirs(0), 1);
        assert!(t.node(free).is_virtual() && t.node(un).is_virtual());
        assert_eq!(t.free_nodes().collect::<Vec<_>>(), vec![free]);
        t.set_virtual_size(un, 70);
        assert_eq!(t.size(c, SizeMode::Allocated), 675);
        t.set_virtual_size(free, 10);
        assert_eq!(t.size(free, SizeMode::Allocated), 10);
        assert_eq!(t.size(c, SizeMode::Allocated), 675);
        t.remove(free);
        assert_eq!(t.free_nodes().count(), 0);
        assert_eq!(t.size(c, SizeMode::Allocated), 675);
    }

    #[test]
    fn children_insertion_order_and_sort() {
        let mut t = Tree::new("r", 0);
        let ids: Vec<_> = [("b", 5), ("a", 5), ("C", 9), ("d", 1)]
            .iter()
            .map(|&(n, s)| t.add_file(0, n, s, s, 0, 0))
            .collect();
        assert_eq!(t.children(0).collect::<Vec<_>>(), ids);
        t.sort_children(0, SizeMode::Logical);
        assert_eq!(names(&t, t.children(0)), ["C", "a", "b", "d"]);
        let e = t.add_file(0, "e", 100, 100, 0, 0);
        assert_eq!(t.children(0).last(), Some(e));
        t.sort_all(SizeMode::Logical);
        assert_eq!(names(&t, t.children(0)), ["e", "C", "a", "b", "d"]);
        assert_eq!(t.child_count(0), 5);
        assert!(t.has_children(0) && !t.has_children(e));
        assert_eq!(t.ext_count(), 1, "no extensions yet");
        let x = t.add_file(0, "x.Rs", 0, 0, 0, 0);
        assert_eq!(t.ext_count(), 2);
        assert_eq!(t.ext_name(t.node(x).ext()), "rs");
    }

    #[test]
    fn sort_all_uses_mode_and_recurses() {
        let mut t = Tree::new("r", 0);
        let d = t.add_dir(0, "d", 0, 0);
        t.add_file(d, "x", 1, 100, 0, 0);
        t.add_file(d, "y", 50, 4, 0, 0);
        t.sort_all(SizeMode::Allocated);
        assert_eq!(names(&t, t.children(d)), ["x", "y"]);
        t.sort_all(SizeMode::Logical);
        assert_eq!(names(&t, t.children(d)), ["y", "x"]);
    }

    #[test]
    fn add_batch_returns_dirs_and_walks_once() {
        let mut t = Tree::new("r", 0);
        let es = vec![
            entry("one.txt", false, 10),
            entry("sub", true, 0),
            RawEntry {
                flags: REPARSE,
                ..entry("junction", true, 0)
            },
            entry("two.TXT", false, 5),
        ];
        let g = t.generation();
        let dirs = t.add_batch(0, &es);
        assert_eq!(t.generation(), g + 1);
        assert_eq!(dirs.iter().map(|d| d.1).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(t.size(0, SizeMode::Logical), 15);
        assert_eq!((t.files(0), t.dirs(0)), (2, 2));
        assert!(t.node(dirs[1].0).has(REPARSE | DIR));
        let a = t.node(dirs[0].0 + 2).ext();
        assert_eq!(t.ext_name(a), "txt");
        assert_eq!(t.node(1).ext(), a, "extensions interned case-insensitively");
        assert_eq!(
            names(&t, t.children(0)),
            ["one.txt", "sub", "junction", "two.TXT"]
        );
        // Dir entries with own size count it.
        let d2 = t.add_batch(dirs[0].0, &[entry("own", true, 4096)]);
        assert_eq!(t.size(d2[0].0, SizeMode::Allocated), 4096);
        assert_eq!(t.size(0, SizeMode::Allocated), 15 + 4096);
    }

    #[test]
    fn extension_rules() {
        assert_eq!(extension("a.txt"), Some("txt"));
        assert_eq!(extension("archive.tar.GZ"), Some("GZ"));
        assert_eq!(extension(".gitignore"), Some("gitignore"));
        assert_eq!(extension("noext"), None);
        assert_eq!(extension("trailing."), None);
        assert_eq!(extension("report.final version"), None);
        assert_eq!(extension("x.abcdefghijklmnopq"), None);
        assert_eq!(extension("фото.ЖПГ"), Some("ЖПГ"));
        let mut t = Tree::new("r", 0);
        let f = t.add_file(0, "фото.ЖПГ", 1, 1, 0, 0);
        assert_eq!(t.ext_name(t.node(f).ext()), "жпг");
        let d = t.add_dir(0, "dir.with.dots", 0, 0);
        assert_eq!(t.node(d).ext(), 0);
        let v = t.add_virtual(0, "<x.y>", 0, 1);
        assert_eq!(t.node(v).ext(), 0);
        assert_eq!(t.ext_name(0), "");
        assert_eq!(t.ext_name(60000), "");
    }

    #[test]
    fn extension_table_overflow_falls_back_to_zero() {
        let mut e = ExtTable::new();
        for i in 0..70_000u32 {
            let id = e.intern(&format!("e{i}"));
            if i < 65_535 {
                assert_eq!(u32::from(id), i + 1);
            } else {
                assert_eq!(id, 0);
            }
        }
        assert_eq!(e.intern("e5"), 6);
    }

    #[test]
    fn names_roundtrip_including_long_and_unicode() {
        let mut t = Tree::new("r", 0);
        let long = "я".repeat(400); // 800 bytes -> 2-byte varint
        let huge = "x".repeat(MAX_NAME_LEN + 10);
        let a = t.add_file(0, &long, 0, 0, 0, 0);
        let b = t.add_file(0, "", 0, 0, 0, 0);
        let c = t.add_file(0, &huge, 0, 0, 0, 0);
        assert_eq!(t.name(a), long);
        assert_eq!(t.name(b), "");
        assert_eq!(t.name(c).len(), MAX_NAME_LEN);
        // Fill several 1 MiB chunks; names must never straddle.
        let ids: Vec<_> = (0..40_000)
            .map(|i| t.add_file(0, &format!("{i:0>60}"), 0, 0, 0, 0))
            .collect();
        assert!(t.names.chunks.len() > 2);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(t.name(*id), format!("{i:0>60}"));
        }
        assert_eq!(truncate_utf8("ab€", 3), "ab");
    }

    #[test]
    fn varint_roundtrip() {
        for v in [0usize, 1, 127, 128, 300, 16_383, 16_384, 65_536, 1 << 20] {
            let mut b = Vec::new();
            write_varint(&mut b, v);
            assert_eq!(b.len(), varint_len(v));
            assert_eq!(read_varint(&b), (v, b.len()));
        }
    }

    #[test]
    fn mtime_clamping() {
        let mut t = Tree::new("r", 0);
        let a = t.add_file(0, "a", 0, 0, 0, -5);
        let b = t.add_file(0, "b", 0, 0, 0, i64::MAX);
        let c = t.add_file(0, "c", 0, 0, 0, 1_700_000_000);
        assert_eq!(t.node(a).mtime(), 0);
        assert_eq!(t.node(b).mtime(), i64::from(u32::MAX));
        assert_eq!(t.node(c).mtime(), 1_700_000_000);
    }

    #[test]
    fn paths() {
        let mut t = Tree::new("C:\\", 0);
        let u = t.add_dir(0, "Users", 0, 0);
        let x = t.add_dir(u, "x", 0, 0);
        let f = t.add_file(x, "Файл.txt", 1, 1, 0, 0);
        assert_eq!(t.path(0), "C:\\");
        assert_eq!(t.path(u), "C:\\Users");
        assert_eq!(t.path(f), "C:\\Users\\x\\Файл.txt");

        let mut v = Tree::new("This PC", VIRTUAL);
        let c = v.add_dir(0, "C:\\", 0, 0);
        let d = v.add_dir(0, "D:\\", 0, 0);
        let w = v.add_dir(c, "Windows", 0, 0);
        assert_eq!(v.path(0), "");
        assert_eq!(v.path(c), "C:\\");
        assert_eq!(v.path(w), "C:\\Windows");
        assert_eq!(v.path(d), "D:\\");

        let mut s = Tree::new("C:\\Users\\x", 0);
        let a = s.add_dir(0, "a", 0, 0);
        assert_eq!(s.path(a), "C:\\Users\\x\\a");
        let u = Tree::new("/tmp/x/", 0);
        assert_eq!(u.path(0), "/tmp/x/");
    }

    #[test]
    fn find_path_variants() {
        let mut t = Tree::new("C:\\", 0);
        let u = t.add_dir(0, "Users", 0, 0);
        let x = t.add_dir(u, "Ölaf", 0, 0);
        let f = t.add_file(x, "Notes.TXT", 1, 1, 0, 0);
        for q in [
            "C:\\Users\\Ölaf\\Notes.TXT",
            "c:/users/ölaf/notes.txt",
            "C:\\\\Users//ÖLAF\\notes.txt\\",
            r"\\?\C:\Users\Ölaf\Notes.txt",
        ] {
            assert_eq!(t.find_path(q), Some(f), "{q}");
        }
        assert_eq!(t.find_path("C:\\"), Some(0));
        assert_eq!(t.find_path("c:"), Some(0));
        assert_eq!(t.find_path("C:/Users/"), Some(u));
        assert_eq!(t.find_path("D:\\Users"), None);
        assert_eq!(t.find_path("C:\\Users\\nobody"), None);
        assert_eq!(t.find_path(""), None);

        let mut v = Tree::new("This PC", VIRTUAL);
        let c = v.add_dir(0, "C:\\", 0, 0);
        let w = v.add_dir(c, "Windows", 0, 0);
        assert_eq!(v.find_path("c:\\windows"), Some(w));
        assert_eq!(v.find_path("C:"), Some(c));
        assert_eq!(v.find_path(""), Some(0));

        let mut s = Tree::new("C:\\Users\\x", 0);
        let a = s.add_dir(0, "a", 0, 0);
        assert_eq!(s.find_path("c:/users/X/A"), Some(a));
        assert_eq!(s.find_path("C:\\Users"), None);

        let mut unc = Tree::new(r"\\server\share", 0);
        let d = unc.add_dir(0, "dir", 0, 0);
        assert_eq!(unc.find_path(r"\\SERVER\share\dir"), Some(d));
        assert_eq!(unc.find_path(r"\\?\UNC\server\share\dir"), Some(d));

        // Exact case wins over an earlier case-insensitive match.
        let mut cs = Tree::new("/r", 0);
        let lower = cs.add_file(0, "a", 0, 0, 0, 0);
        let upper = cs.add_file(0, "A", 0, 0, 0, 0);
        assert_eq!(cs.find_path("/r/A"), Some(upper));
        assert_eq!(cs.find_path("/r/a"), Some(lower));
        assert_eq!(cs.find_child(0, "A"), Some(upper));
        assert_eq!(cs.find_child(0, "a"), Some(lower));
    }

    #[test]
    fn remove_and_tombstones() {
        let mut t = Tree::new("r", 0);
        let a = t.add_dir(0, "a", 0, 0);
        let b = t.add_dir(a, "b", 0, 0);
        let f = t.add_file(b, "f", 10, 10, 0, 0);
        let g = t.add_file(a, "g", 5, 5, 0, 0);
        let h = t.add_file(0, "h", 1, 1, 0, 0);
        t.remove(b);
        assert!(!t.is_live(b) && !t.is_live(f));
        assert!(t.is_live(a) && t.is_live(g));
        assert_eq!(t.size(0, SizeMode::Logical), 6);
        assert_eq!((t.files(0), t.dirs(0)), (2, 1));
        assert_eq!(t.children(a).collect::<Vec<_>>(), vec![g]);
        assert_eq!(t.live_len(), 4);
        assert_eq!(t.live_ancestor(f), a);
        assert_eq!(t.children(b).count(), 0);
        assert_eq!(t.descendants(b).count(), 0);
        // Inserting under a tombstone gives another tombstone and no totals.
        let z = t.add_file(b, "z", 100, 100, 0, 0);
        assert!(!t.is_live(z));
        assert!(t.add_batch(b, &[entry("q", false, 9)]).is_empty());
        assert_eq!(t.size(0, SizeMode::Logical), 6);
        // Removing the root or a tombstone is a no-op.
        t.remove(0);
        t.remove(f);
        assert_eq!(t.size(0, SizeMode::Logical), 6);
        // Remove first / last / only child.
        t.remove(h);
        assert_eq!(t.children(0).collect::<Vec<_>>(), vec![a]);
        t.remove(g);
        assert_eq!(t.children(a).count(), 0);
        assert_eq!(t.size(0, SizeMode::Logical), 0);
        assert_eq!((t.files(0), t.dirs(0)), (0, 1));
        assert_eq!(t.descendants(0).collect::<Vec<_>>(), vec![a]);
        assert!(t.live_nodes().all(|(id, _)| id == 0 || id == a));
    }

    #[test]
    fn clear_children_keeps_own_size_and_resets_error() {
        let mut t = Tree::new("r", 0);
        let d = t.add_node(0, "d", 7, 4096, DIR | ERROR, 0);
        t.add_file(d, "a", 1, 1, 0, 0);
        let s = t.add_dir(d, "s", 0, 0);
        t.add_file(s, "b", 2, 2, 0, 0);
        assert_eq!(t.size(0, SizeMode::Logical), 10);
        t.clear_children(d);
        assert_eq!(t.size(d, SizeMode::Logical), 7);
        assert_eq!(t.size(0, SizeMode::Allocated), 4096);
        assert_eq!((t.files(0), t.dirs(0)), (0, 1));
        assert!(!t.node(d).has(ERROR));
        assert_eq!(t.children(d).count(), 0);
        let n = t.add_file(d, "new", 3, 3, 0, 0);
        assert_eq!(t.children(d).collect::<Vec<_>>(), vec![n]);
        assert_eq!(t.size(0, SizeMode::Logical), 10);
    }

    #[test]
    fn raw_build_and_recompute() {
        let mut t = Tree::new("r", 0);
        let live = t.add_dir(0, "live", 0, 0);
        t.add_file(live, "l", 1, 1, 0, 0);
        let a = t.add_dir_raw(0, "a", 0, 0);
        let b = t.add_node_raw(a, "b", 0, 512, DIR, 0);
        t.add_file_raw(b, "x.bin", 100, 4096, 0, 0);
        t.add_file_raw(a, "y.bin", 10, 0, 0, 0);
        t.add_file_raw(live, "z", 1000, 1000, 0, 0);
        assert_eq!(t.size(0, SizeMode::Logical), 1, "stale until recompute");
        t.recompute_aggregates();
        assert_eq!(t.size(b, SizeMode::Allocated), 4608);
        assert_eq!(t.size(a, SizeMode::Logical), 110);
        assert_eq!(t.size(live, SizeMode::Logical), 1001);
        assert_eq!(t.size(0, SizeMode::Logical), 1111);
        assert_eq!((t.files(0), t.dirs(0)), (4, 3));
        // Recompute is idempotent.
        t.recompute_aggregates();
        assert_eq!(t.size(0, SizeMode::Logical), 1111);
        // A live op finalizes pending raw nodes automatically.
        t.add_file_raw(a, "w", 5, 5, 0, 0);
        t.add_file(a, "v", 1, 1, 0, 0);
        assert_eq!(t.size(0, SizeMode::Logical), 1117);
    }

    #[test]
    fn graft_copies_subtrees() {
        let mut src = Tree::new("C:\\fresh", 0);
        let d = src.add_dir(0, "d", HIDDEN, 42);
        src.add_file(d, "a.MP4", 100, 4096, 0, 7);
        src.add_file(0, "b.mp4", 1, 1, 0, 0);
        src.add_virtual(0, "<free>", FREE, 1000);
        let gone = src.add_file(0, "gone", 50, 50, 0, 0);
        src.remove(gone);

        let mut t = Tree::new("C:\\", 0);
        t.add_file(0, "keep.txt", 3, 3, 0, 0);
        let at = t.add_dir(0, "fresh", 0, 0);
        t.add_file(at, "old", 9, 9, 0, 0);
        t.clear_children(at);
        t.graft(at, &src);
        assert_eq!(names(&t, t.children(at)), ["d", "b.mp4", "<free>"]);
        let nd = t.find_child(at, "d").unwrap();
        assert!(t.node(nd).has(HIDDEN));
        assert_eq!(t.node(nd).mtime(), 42);
        let na = t.find_child(nd, "a.MP4").unwrap();
        assert_eq!(t.ext_name(t.node(na).ext()), "mp4");
        assert_eq!(t.size(at, SizeMode::Logical), 101);
        assert_eq!(t.size(0, SizeMode::Logical), 104);
        assert_eq!((t.files(0), t.dirs(0)), (3, 2));
        assert_eq!((t.files(nd), t.dirs(nd)), (1, 0));
        assert_eq!(t.free_nodes().count(), 1);
        assert_eq!(t.path(na), "C:\\fresh\\d\\a.MP4");
    }

    #[test]
    fn generation_increments_on_every_mutation() {
        let mut t = Tree::new("r", 0);
        let mut last = t.generation();
        let mut check = |t: &Tree| {
            assert!(t.generation() > last);
            last = t.generation();
        };
        let d = t.add_dir(0, "d", 0, 0);
        check(&t);
        t.add_file(d, "f", 1, 1, 0, 0);
        check(&t);
        t.add_batch(d, &[entry("g", false, 1)]);
        check(&t);
        let v = t.add_virtual(0, "v", UNACCOUNTED, 1);
        check(&t);
        t.set_virtual_size(v, 2);
        check(&t);
        t.set_flags(d, ERROR, 0);
        check(&t);
        t.sort_children(d, SizeMode::Logical);
        check(&t);
        t.sort_all(SizeMode::Logical);
        check(&t);
        t.add_file_raw(d, "r", 1, 1, 0, 0);
        check(&t);
        t.recompute_aggregates();
        check(&t);
        t.graft(d, &Tree::new("x", 0));
        check(&t);
        t.clear_children(d);
        check(&t);
        t.remove(d);
        check(&t);
    }

    #[test]
    fn set_flags_only_changes_mutable_bits() {
        let mut t = Tree::new("r", 0);
        let f = t.add_file(0, "f", 1, 1, 0, 0);
        t.set_flags(f, ERROR | DIR | DELETED | FREE, 0);
        assert_eq!(t.node(f).flags(), ERROR);
        t.set_flags(f, HIDDEN, ERROR);
        assert_eq!(t.node(f).flags(), HIDDEN);
        assert_eq!(t.size(0, SizeMode::Logical), 1);
    }

    #[test]
    fn ancestors_depth_descendants() {
        let mut t = Tree::new("r", 0);
        let a = t.add_dir(0, "a", 0, 0);
        let b = t.add_dir(a, "b", 0, 0);
        let c = t.add_file(b, "c", 0, 0, 0, 0);
        let d = t.add_file(a, "d", 0, 0, 0, 0);
        assert_eq!(t.ancestors(c).collect::<Vec<_>>(), vec![b, a, 0]);
        assert_eq!(t.depth(c), 3);
        assert_eq!(t.depth(0), 0);
        assert!(t.is_ancestor(a, c) && t.is_ancestor(c, c) && !t.is_ancestor(d, c));
        assert_eq!(t.descendants(0).collect::<Vec<_>>(), vec![a, b, c, d]);
        assert_eq!(t.descendants(b).collect::<Vec<_>>(), vec![c]);
        assert_eq!(t.descendants(c).count(), 0);
    }

    #[test]
    fn memory_is_reported() {
        let mut t = Tree::new("r", 0);
        for i in 0..1000 {
            t.add_file(0, &format!("file{i}.txt"), 1, 1, 0, 0);
        }
        assert!(t.memory_bytes() > 1000 * 40);
        assert!(t.memory_bytes() < 1000 * 200);
    }

    #[test]
    fn name_compare_helpers() {
        assert_eq!(cmp_names("abc", "ABD"), Ordering::Less);
        assert_eq!(cmp_names("B", "a"), Ordering::Greater);
        assert_eq!(
            cmp_names("a", "A"),
            Ordering::Greater,
            "byte tie-break: 'A' < 'a'"
        );
        assert_eq!(cmp_names("ябко", "Ясен"), Ordering::Less);
        assert!(eq_ci("ПрИвІт", "привіт"));
        assert!(!eq_ci("abc", "abd"));
    }
}
