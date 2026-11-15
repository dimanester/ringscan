//! # ringscan-core
//!
//! Platform-independent heart of RingScan: the in-memory tree of a scan, the
//! parallel directory walker, the sunburst geometry and the statistics and
//! formatting helpers the UI needs. Everything here builds and is tested on
//! Linux; the Windows-specific listing lives in `ringscan-win` and plugs in
//! through [`walk::DirLister`].
//!
//! ## Modules
//!
//! * [`tree`]   — arena tree ([`Tree`], [`Node`]) with live aggregation,
//!   tombstones, bulk ("raw") building, sorting, grafting, path lookup.
//! * [`walk`]   — parallel work-sharing directory walker over a
//!   [`walk::DirLister`]; inserts one batch per listed directory under a
//!   short write lock so the UI can lay out the tree while it grows.
//! * [`layout`] — sunburst arcs for a focus node (`compute`) and hit testing.
//! * [`stats`]  — extension breakdown, largest files, name search.
//! * [`format`](mod@format) — human-readable sizes, counts and percentages (uk / en).
//! * [`synth`]  — deterministic synthetic trees for tests, benchmarks and
//!   UI demos (no disk access).
//!
//! ## Conventions shared by all crates
//!
//! * The shared tree is a `parking_lot::RwLock<Tree>`; this crate re-exports
//!   [`parking_lot`] so every crate uses the same lock type.
//! * Node ids are `u32` indices into the arena ([`NodeId`]); [`NONE`] means
//!   "no node". Ids are never reused within one `Tree`, so an id held by the
//!   UI stays valid (possibly as a DELETED tombstone) until the tree is
//!   replaced.
//! * Sizes are reported per [`SizeMode`]: allocated ("used space", default)
//!   or logical (file length).
//!
//! See the module docs of [`tree`] for the exact aggregation rules of
//! virtual nodes (free space, unaccounted space, "This PC").
//!
//! ## Deviations from the SPEC.md contract
//!
//! * [`Node`] fields are private (40-byte packed representation); read them
//!   with `parent()`, `flags()`, `has(flag)`, `is_dir()`, `is_live()`,
//!   `is_virtual()`, `is_file()`, `ext()`, `logical()`, `alloc()`,
//!   `size(mode)`, `mtime()`. The file / directory counts live in a
//!   directory-only side table: [`Tree::files`] / [`Tree::dirs`] (u64).
//!   `first_child` / `next_sibling` are not exposed: use
//!   [`Tree::children`], [`Tree::descendants`], [`Tree::has_children`].
//!   `mtime` is stored as u32 seconds (1970..2106; earlier = 0 = unknown).
//! * Storage is chunked (fixed-size blocks) rather than one `Vec<Node>`.
//! * [`format::percent`] takes a ratio and a [`Lang`]:
//!   `percent(0.123, Lang::En) == "12.3%"`.
//! * [`walk::DirLister`] has an extra provided method `list_cancellable`
//!   (defaults to `list`); [`Progress::current`] is a `parking_lot::Mutex`;
//!   `walk` sets `progress.done` when it returns; `progress.bytes` sums
//!   allocated sizes.
//! * [`Tree::add_batch`] returns every directory entry, REPARSE ones
//!   included (the walker skips descending into those).
//! * Additions: `Tree::{get, live_len, generation, files, dirs, is_live,
//!   live_ancestor, has_children, child_count, descendants, ancestors, depth,
//!   is_ancestor, live_nodes, free_nodes, find_child, memory_bytes, add_node,
//!   add_node_raw, set_virtual_size, set_flags, ext_count}`,
//!   `layout::Layout::{ring, find}`, `layout::Arc::{node, span}`,
//!   `walk::{ProgressSnapshot, entry_from_metadata, BATCH_MAX}`,
//!   [`flags::MUTABLE`] and the [`synth`] module.

pub mod format;
pub mod layout;
pub mod stats;
pub mod synth;
pub mod tree;
pub mod walk;

pub use parking_lot;
pub use tree::{Children, Descendants, Node, Tree};
pub use walk::{DirLister, Progress, RawEntry, StdLister};

/// Index of a node in a [`Tree`] arena.
pub type NodeId = u32;

/// "No node" sentinel (parent of the root, empty links).
pub const NONE: NodeId = u32::MAX;

/// Which size of an entry to report and lay out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SizeMode {
    /// Bytes allocated on disk ("used space"; compressed/sparse/cloud aware).
    #[default]
    Allocated,
    /// File length in bytes (end of file).
    Logical,
}

/// UI language for the [`format`](mod@format) helpers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    /// Ukrainian: decimal comma, "Б, КБ, МБ, ГБ, ТБ".
    Uk,
    /// English: decimal point, "B, KB, MB, GB, TB".
    En,
}

/// Bit flags stored in [`Node::flags`] (u16).
///
/// DIR, HARDLINK, FREE, UNACCOUNTED, VIRTUAL and DELETED define how a node is
/// aggregated and are fixed when the node is created; the rest
/// ([`flags::MUTABLE`]) are informational and may be changed later with
/// [`Tree::set_flags`].
pub mod flags {
    /// Directory (may have children). Every non-DIR node is a leaf.
    pub const DIR: u16 = 1 << 0;
    /// Junction / symlink / other name-surrogate reparse point: listed but
    /// never descended into by the walker.
    pub const REPARSE: u16 = 1 << 1;
    /// NTFS-compressed file.
    pub const COMPRESSED: u16 = 1 << 2;
    /// Sparse file.
    pub const SPARSE: u16 = 1 << 3;
    /// Hidden attribute.
    pub const HIDDEN: u16 = 1 << 4;
    /// System attribute.
    pub const SYSTEM: u16 = 1 << 5;
    /// The directory could not be (fully) listed, or the entry's metadata
    /// could not be read.
    pub const ERROR: u16 = 1 << 6;
    /// Cloud placeholder (OneDrive etc.); counted by allocated size.
    pub const CLOUD: u16 = 1 << 7;
    /// Extra hard link of a file counted elsewhere: always zero sizes.
    pub const HARDLINK: u16 = 1 << 8;
    /// Recycle Bin directory (or an engine-made virtual Recycle Bin node).
    pub const RECYCLE: u16 = 1 << 9;
    /// NTFS metadata file ($MFT, $LogFile, ...).
    pub const METAFILE: u16 = 1 << 10;
    /// Virtual: free space of a volume. Implies VIRTUAL. Not aggregated
    /// into ancestors; shown only by layout with `show_free`.
    pub const FREE: u16 = 1 << 11;
    /// Virtual: used space the scan could not see (used − scanned).
    /// Implies VIRTUAL. Aggregated like a file but never counted as one.
    pub const UNACCOUNTED: u16 = 1 << 12;
    /// Synthetic node (incl. a "This PC" root): never counted as a file or
    /// a directory; a VIRTUAL root is left out of [`crate::Tree::path`].
    pub const VIRTUAL: u16 = 1 << 13;
    /// Tombstone of a removed node: unreachable from the root.
    pub const DELETED: u16 = 1 << 14;

    /// Flags that [`crate::Tree::set_flags`] may change after insertion.
    pub const MUTABLE: u16 =
        REPARSE | COMPRESSED | SPARSE | HIDDEN | SYSTEM | ERROR | CLOUD | RECYCLE | METAFILE;
}
