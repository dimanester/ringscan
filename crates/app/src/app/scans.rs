//! Scans kept in memory: switching between "This PC", drives and folders
//! without scanning them again.
//!
//! The app shows one scan at a time (`App::scan`). Every other scan it made
//! stays in [`ScanCache`], a short most-recently-used list, together with
//! its [`View`] (focus, back stack, selection), so going back to it is
//! instant and keeps where the user was. A sidebar card, a breadcrumb above
//! the scanned folder or "Choose folder…" asks for a [`Want`], and
//! [`locate`] decides where it is shown:
//!
//! * **This PC**: the current tree if its root is the virtual "This PC",
//!   else a cached "This PC" scan, else a new scan.
//! * **A drive**: the current tree if it has the drive, else the most
//!   recently used cached scan that has it, else a new scan of the drive.
//! * **A folder** (or a drive root given as a path): a scan of exactly that
//!   folder first (current, then cached), then the current tree if it
//!   contains the folder, then the most recently used cached scan that
//!   contains it, else a new scan. A scan the user stopped (its tree may
//!   miss folders; a completed rescan of its root repairs it) never serves
//!   a folder request; F5 refreshes a folder that was found in memory.
//!
//! The app then navigates within the current tree, swaps a cached scan in
//! (the current one is cached in its place) and focuses the requested node
//! there, pushing the restored focus onto the restored back stack so Back
//! returns to it, or starts a new scan (the current one is cached; cached
//! scans of the same root are dropped, they are superseded). Back stacks
//! stay per tree: Back never crosses into another scan.
//!
//! Never served from memory, so asking again scans again:
//!
//! * a scan that **failed**: it finished without being able to read its
//!   root (a folder that did not exist yet, a drive that was not ready, an
//!   offline share, access denied). It shows nothing, so even as the
//!   current scan it is retried, and it is never cached;
//! * a cached tree of a drive whose **volume may have changed**: only fixed
//!   drives that the drive list still shows with the label, file system and
//!   size the scan read are served from the cache (a fixed drive that did
//!   not answer the drive query in time has no facts to compare and is
//!   not held against it). Removable, network,
//!   optical and RAM drives (and UNC paths) are always scanned again when
//!   they are not the current tree: a USB stick or share can be swapped
//!   behind the same letter, and the drive list is not re-read when that
//!   happens, so nothing tells the trees apart. The current tree is used as
//!   before (F5 refreshes it);
//! * cached synthetic trees (`--demo`): their made-up drives must not
//!   stand in for real ones.
//!
//! A cached scan that is still running keeps running in the background (the
//! user asked for that: going back to it continues where it was). The
//! status bar always describes the current scan and, next to it, counts
//! the scans running in the background with a button that stops them. The
//! cost: each running scan keeps its walker threads (8 per drive) and
//! disk queue, so a background scan of `C:\Windows` and a new "This PC"
//! scan walk `C:` twice and each finishes later than it would alone. Only
//! eviction (the least recently used scan beyond [`ScanCache::CAPACITY`])
//! drops a scan, which cancels its jobs.
//!
//! # Memory
//!
//! A tree costs ~75 bytes per node: the "This PC" scan of the development
//! machine (C: and D:, 1.04 M files, 279 K folders, 1.32 M nodes) holds
//! 97 MB of tree (`tree.memory_mb` / `cache[].memory_mb` in the harness
//! log) plus the engine's per-volume hard-link maps (a few MB); the whole
//! process peaks at 300–370 MB private during that scan, ~186 MB of which
//! is the idle GUI and GPU stack. Keeping up to [`ScanCache::CAPACITY`]
//! scans besides the current one bounds the cost at ~300 MB more in the
//! worst case (four trees of that size); usually it is one "This PC" scan
//! plus drives outside it and folders, which are far smaller. Ids, views
//! and every other per-tree state of a cached scan stay valid because its
//! tree is kept, not rebuilt.
//!
//! # Deletes
//!
//! A recycle or permanent delete runs in the scan it was confirmed in (the
//! engine removes the items from that tree). When it has finished, the
//! worker reads [`Jobs`], the scans in memory at that moment (so a scan
//! started while the delete ran is included), and [`sync_after_delete`]
//! removes every item that is gone on disk from those other trees too,
//! under each tree's write lock, clears a tree whose root folder was
//! deleted and refreshes the free / unaccounted space of their drives. The
//! app drops a cached scan whose root folder was deleted; if that scan is
//! the one shown (the user switched to it while the delete ran), it shows
//! the deleted item's parent folder instead. After "Empty Recycle Bin" it
//! removes the Recycle Bin items that are gone the same way. A recycled
//! item is not added to the cached trees' Recycle Bin nodes (their
//! `<unaccounted>` covers it until a rescan).
//!
//! "Gone" means the disk says *not found*. Access denied means the item is
//! still there: other users' Recycle Bin folders (which an MFT read shows
//! without elevation) and System Volume Information cannot even be looked
//! at, and a failed delete leaves them in place. A user folder of the
//! Recycle Bin that cannot be listed keeps its items, as the engine's own
//! bin refresh does; "Empty Recycle Bin" only empties the current user's
//! folder anyway.

use std::collections::{HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use ringscan_core::parking_lot::{Mutex, RwLock};
use ringscan_core::{NodeId, Tree, flags};
use ringscan_engine::{ScanJob, Target};

use crate::dialogs::ConfirmKind;
use crate::platform::{DriveInfo, DriveKind};
use crate::scan_source::{Scan, target_for};

/// Back stack entries kept per tree.
pub(super) const BACK_LIMIT: usize = 200;

/// Pushes `id` onto a back stack, dropping the oldest entry past
/// [`BACK_LIMIT`].
pub(super) fn push_back(back: &mut Vec<NodeId>, id: NodeId) {
    back.push(id);
    if back.len() > BACK_LIMIT {
        back.remove(0);
    }
}

/// Where the user is in one tree. Its ids belong to that tree only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct View {
    pub focus: NodeId,
    pub back: Vec<NodeId>,
    pub selected: Option<NodeId>,
}

/// A scan that is not shown, with where the user was in it.
pub(super) struct Cached {
    pub scan: Scan,
    pub view: View,
}

/// Scans kept besides the current one, most recently used first. See the
/// [module docs](self).
#[derive(Default)]
pub(super) struct ScanCache {
    items: VecDeque<Cached>,
}

impl ScanCache {
    /// Scans kept besides the current one (four trees in memory at most;
    /// see "Memory" in the module docs).
    pub const CAPACITY: usize = 3;

    /// Most recently used first.
    pub fn iter(&self) -> impl Iterator<Item = &Cached> {
        self.items.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Cached> {
        self.items.iter_mut()
    }

    /// Caches `scan` as the most recently used one. The least recently used
    /// scans beyond [`Self::CAPACITY`] are dropped, which cancels their jobs.
    /// A scan that failed is dropped instead: it has nothing to show and is
    /// never served again (see the [module docs](self)).
    pub fn park(&mut self, scan: Scan, view: View) {
        if is_failed(&scan) {
            return;
        }
        self.items.push_front(Cached { scan, view });
        self.items.truncate(Self::CAPACITY);
    }

    /// Makes cached scan `i` the current one: `current` and `view` are
    /// exchanged with it and the scan that was current is cached as the most
    /// recently used one. False (nothing changed) if there is no scan `i`.
    pub fn swap_in(&mut self, i: usize, current: &mut Option<Scan>, view: &mut View) -> bool {
        let Some(mut c) = self.items.remove(i) else {
            return false;
        };
        std::mem::swap(view, &mut c.view);
        if let Some(old) = current.replace(c.scan) {
            // `c.view` holds the old current view now.
            self.park(old, c.view);
        }
        true
    }

    /// Makes the new `scan` the current one with an empty view. The scan
    /// that was current is cached unless it has the same root (a scan being
    /// redone); cached scans with that root are dropped.
    pub fn start(&mut self, scan: Scan, current: &mut Option<Scan>, view: &mut View) {
        let old_view = std::mem::take(view);
        self.items.retain(|c| !same_root(&c.scan, &scan));
        if let Some(old) = current.take()
            && !same_root(&old, &scan)
        {
            self.park(old, old_view);
        }
        *current = Some(scan);
    }

    /// Drops the cached scans for which `keep` is false.
    pub fn retain(&mut self, keep: impl FnMut(&Cached) -> bool) {
        self.items.retain(keep);
    }
}

/// Both are real scans of the same folder, drive or "This PC" (by the root
/// of their trees, ignoring case).
fn same_root(a: &Scan, b: &Scan) -> bool {
    if a.is_demo() || b.is_demo() {
        return false;
    }
    let key = |s: &Scan| {
        let t = s.tree.read_recursive();
        let root = t.root();
        (t.node(root).is_virtual(), t.name(root).to_lowercase())
    };
    key(a) == key(b)
}

/// The tree of a finished scan that could not read its root (a missing
/// folder, a drive that is not ready, access denied): the root is flagged
/// ERROR and holds nothing real (a drive's `<free space>` at most).
fn root_failed(t: &Tree) -> bool {
    let root = t.root();
    t.node(root).has(flags::ERROR) && t.children(root).all(|c| t.node(c).is_virtual())
}

/// `scan` finished without being able to read its root (see the
/// [module docs](self)).
fn is_failed(scan: &Scan) -> bool {
    !scan.is_running() && root_failed(&scan.tree.read_recursive())
}

/// What a sidebar card, a breadcrumb or "Choose folder…" asks to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Want {
    /// Every fixed drive under the virtual "This PC" root.
    ThisPc,
    /// A drive, by its letter.
    Drive(char),
    /// A folder (or a drive root) by absolute path.
    Folder(String),
}

impl Want {
    /// The request for a folder to scan ("Choose folder…", a breadcrumb
    /// above the scanned root).
    pub fn for_path(path: &Path) -> Want {
        match target_for(path) {
            Target::AllFixedDrives => Want::ThisPc,
            Target::Drive(c) => Want::Folder(format!("{c}:\\")),
            Target::Path(p) => {
                let abs = std::path::absolute(&p).unwrap_or(p);
                Want::Folder(abs.to_string_lossy().into_owned())
            }
        }
    }
}

/// A drive a scan read as a whole, as it saw it then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Volume {
    pub letter: char,
    pub label: String,
    pub fs: String,
    pub total: u64,
}

/// A scan [`locate`] may choose.
pub(super) struct Candidate<'a> {
    pub tree: &'a RwLock<Tree>,
    /// Synthetic data: only ever used as the current tree.
    pub demo: bool,
    /// Stopped by the user: its tree may miss folders.
    pub cancelled: bool,
    /// Finished without being able to read its root: never served.
    pub failed: bool,
    /// The drives the scan read as a whole (a drive or "This PC" scan).
    pub volumes: Vec<Volume>,
}

impl<'a> Candidate<'a> {
    pub fn of(scan: &'a Scan) -> Candidate<'a> {
        let volumes = scan
            .job()
            .map(|j| {
                j.drives()
                    .into_iter()
                    .map(|d| Volume {
                        letter: d.info.letter,
                        label: d.info.label,
                        fs: d.info.fs,
                        total: d.info.total,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Candidate {
            tree: &scan.tree,
            demo: scan.is_demo(),
            cancelled: scan.is_cancelled(),
            failed: is_failed(scan),
            volumes,
        }
    }
}

/// Drive letter of an absolute Windows path (`C:\x`, `\\?\C:\x`).
fn path_letter(path: &str) -> Option<char> {
    let b = path.strip_prefix(r"\\?\").unwrap_or(path).as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        .then(|| b[0].to_ascii_uppercase() as char)
}

/// May cached scan `c` serve `want`? Only if the volume behind the
/// requested drive letter is still the one it read: a fixed drive that
/// `drives` (the drive list) shows with the label, file system and size the
/// scan recorded (see the [module docs](self)). A fixed drive the list has
/// no facts for (it did not answer within the drive query's deadline) is
/// not held against the cache: rescanning C: because of a slow query is
/// what the cache exists to avoid.
fn same_volume(c: &Candidate, want: &Want, drives: &[DriveInfo]) -> bool {
    let letter = match want {
        // "This PC" is every fixed drive.
        Want::ThisPc => return true,
        Want::Drive(l) => l.to_ascii_uppercase(),
        Want::Folder(p) => match path_letter(p) {
            Some(l) => l,
            // A UNC path is a network location.
            None if p.starts_with(r"\\") || p.starts_with("//") => return false,
            // No drive letters on other platforms (unit tests).
            None => return true,
        },
    };
    let Some(d) = drives.iter().find(|d| d.letter == letter) else {
        return false;
    };
    let Some(total) = d.total else {
        return d.kind == DriveKind::Fixed;
    };
    d.kind == DriveKind::Fixed
        && c.volumes
            .iter()
            .filter(|v| v.letter == letter)
            .all(|v| v.label == d.label && v.fs == d.fs && v.total == total)
}

/// Where a [`Want`] is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Place {
    /// This node of the current tree.
    Current(NodeId),
    /// This node of the cached scan with this index.
    Cached(usize, NodeId),
    /// No tree in memory has it: scan it.
    NewScan,
    /// Nothing to do: synthetic data has no such drive (and real drives are
    /// never scanned in its place).
    Nowhere,
}

/// Decides where `want` is shown (see the [module docs](self)); `cached`
/// is most recently used first, `drives` is the drive list the sidebar
/// shows.
pub(super) fn locate(
    current: Option<&Candidate>,
    cached: &[Candidate],
    want: &Want,
    drives: &[DriveInfo],
) -> Place {
    let demo_current = current.is_some_and(|c| c.demo);
    // A failed scan has nothing to show: asking again scans again.
    let current = current.filter(|c| !c.failed);
    let cached = || {
        cached
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.demo && !c.failed && same_volume(c, want, drives))
    };
    match want {
        Want::ThisPc => {
            if let Some(c) = current
                && let Some(root) = this_pc_root(c)
            {
                return Place::Current(root);
            }
            for (i, c) in cached() {
                if let Some(root) = this_pc_root(c) {
                    return Place::Cached(i, root);
                }
            }
            Place::NewScan
        }
        Want::Drive(letter) => {
            let path = format!("{letter}:\\");
            if let Some(c) = current
                && let Some(id) = folder_in(c, &path)
            {
                return Place::Current(id);
            }
            for (i, c) in cached() {
                if let Some(id) = folder_in(c, &path) {
                    return Place::Cached(i, id);
                }
            }
            if demo_current {
                Place::Nowhere
            } else {
                Place::NewScan
            }
        }
        Want::Folder(path) => {
            let usable = |c: &&Candidate| !c.demo && !c.cancelled;
            let current = current.filter(usable);
            let usable_cached = || cached().filter(|(_, c)| usable(c));
            // A scan of exactly this folder, then any tree containing it.
            if let Some(c) = current
                && let Some(root) = root_at(c, path)
            {
                return Place::Current(root);
            }
            for (i, c) in usable_cached() {
                if let Some(root) = root_at(c, path) {
                    return Place::Cached(i, root);
                }
            }
            if let Some(c) = current
                && let Some(id) = folder_in(c, path)
            {
                return Place::Current(id);
            }
            for (i, c) in usable_cached() {
                if let Some(id) = folder_in(c, path) {
                    return Place::Cached(i, id);
                }
            }
            Place::NewScan
        }
    }
}

/// The root of a "This PC" tree.
fn this_pc_root(c: &Candidate) -> Option<NodeId> {
    let t = c.tree.read_recursive();
    t.node(t.root()).is_virtual().then_some(t.root())
}

/// The root, if the tree is a scan of exactly `path`.
fn root_at(c: &Candidate, path: &str) -> Option<NodeId> {
    let t = c.tree.read_recursive();
    let root = t.root();
    (!t.node(root).is_virtual() && t.find_path(path) == Some(root)).then_some(root)
}

/// A real folder of the tree at `path` that can be shown (not a junction or
/// symlink, whose target is not below it).
fn folder_in(c: &Candidate, path: &str) -> Option<NodeId> {
    let t = c.tree.read_recursive();
    let id = t.find_path(path)?;
    let n = t.node(id);
    let shown = n.is_dir() && !n.has(flags::REPARSE) && (id == t.root() || !n.is_virtual());
    shown.then_some(id)
}

/// True if `path` is `folder` itself or lies below it (case-insensitive,
/// `\` or `/`, `\\?\` ignored).
pub(super) fn path_within(path: &str, folder: &str) -> bool {
    let parts = |s: &str| -> Vec<String> {
        let s = s.strip_prefix(r"\\?\").unwrap_or(s);
        s.split(['\\', '/'])
            .filter(|c| !c.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    let (p, f) = (parts(path), parts(folder));
    !f.is_empty() && p.len() >= f.len() && p[..f.len()] == f[..]
}

/// What [`remove_paths`] did to a tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Removed {
    /// Nodes (with their subtrees) taken out.
    pub nodes: usize,
    /// The tree's root folder is one of the paths or below one of them: its
    /// children were cleared.
    pub root_gone: bool,
}

/// Removes the nodes at `gone` (paths deleted on disk) from `tree` under one
/// write lock; a tree whose root folder is gone loses all its children.
/// Drive roots are never removed.
pub(super) fn remove_paths(tree: &RwLock<Tree>, gone: &[String]) -> Removed {
    let mut out = Removed::default();
    if gone.is_empty() {
        return out;
    }
    let mut t = tree.write();
    let root = t.root();
    let root_path = (!t.node(root).is_virtual()).then(|| t.name(root).to_string());
    for p in gone {
        if ringscan_engine::lister::drive_root_letter(p).is_some() {
            continue;
        }
        if root_path.as_deref().is_some_and(|r| path_within(r, p)) {
            out.root_gone = true;
            continue;
        }
        if let Some(id) = t.find_path(p)
            && id != root
            && !t.node(id).is_virtual()
        {
            t.remove(id);
            out.nodes += 1;
        }
    }
    if out.root_gone {
        t.clear_children(root);
    }
    out
}

/// The user folders of the Recycle Bins of a tree's drives
/// (`X:\$Recycle.Bin\<user>`; the engine flags the bin RECYCLE).
fn bin_user_folders(t: &Tree) -> Vec<NodeId> {
    let root = t.root();
    let drives: Vec<NodeId> = if t.node(root).is_virtual() {
        t.children(root).collect()
    } else {
        vec![root]
    };
    let mut users = Vec::new();
    for d in drives {
        let bins = t
            .children(d)
            .filter(|&b| t.node(b).is_dir() && t.node(b).has(flags::RECYCLE));
        for bin in bins {
            users.extend(t.children(bin).filter(|&u| t.node(u).is_dir()));
        }
    }
    users
}

/// What the delete sync asks the disk; [`OnDisk`] in the app, a fake in
/// tests (where "access denied" cannot be produced: they run as root).
pub(super) trait Disk {
    /// The item at `path` no longer exists. Only "not found" counts: access
    /// denied means it is still there (see the [module docs](self)).
    fn gone(&self, path: &Path) -> bool;
    /// The names in folder `dir`, lower case.
    fn list(&self, dir: &Path) -> io::Result<HashSet<String>>;
}

/// The real disk.
pub(super) struct OnDisk;

impl Disk for OnDisk {
    fn gone(&self, path: &Path) -> bool {
        // ERROR_FILE_NOT_FOUND and ERROR_PATH_NOT_FOUND (a deleted parent)
        // are both NotFound.
        matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
    }

    fn list(&self, dir: &Path) -> io::Result<HashSet<String>> {
        let mut names = HashSet::new();
        for e in std::fs::read_dir(dir)? {
            names.insert(e?.file_name().to_string_lossy().to_lowercase());
        }
        Ok(names)
    }
}

/// What the disk says about a user folder of the Recycle Bin.
#[derive(Debug)]
enum Listing {
    /// Listed: the names in it, lower case.
    Names(HashSet<String>),
    /// The folder itself is gone.
    Gone,
    /// It cannot be listed (another user's bin when not elevated), or it
    /// has no path: its items are kept.
    Unreadable,
}

impl Listing {
    fn of(dir: Option<&Path>, disk: &dyn Disk) -> Listing {
        match dir.map(|d| disk.list(d)) {
            Some(Ok(names)) => Listing::Names(names),
            Some(Err(e)) if e.kind() == io::ErrorKind::NotFound => Listing::Gone,
            _ => Listing::Unreadable,
        }
    }
}

/// The Recycle Bin items of `t` that are gone on disk according to
/// `listings` (user folder, listing): items missing from their folder's
/// listing and user folders that are gone.
fn gone_bin_items(t: &Tree, listings: &[(NodeId, Listing)]) -> Vec<NodeId> {
    let mut gone = Vec::new();
    for (user, listing) in listings {
        if !t.is_live(*user) {
            continue;
        }
        match listing {
            Listing::Names(names) => gone.extend(
                t.children(*user)
                    .filter(|&i| !names.contains(&t.name(i).to_lowercase())),
            ),
            Listing::Gone => gone.push(*user),
            Listing::Unreadable => {}
        }
    }
    gone
}

/// Lists the Recycle Bin user folders of a scan's tree. The disk is asked
/// without holding the tree lock; the ids stay valid (a node removed
/// meanwhile is a tombstone, which [`gone_bin_items`] skips).
fn bin_listings(job: &ScanJob, disk: &dyn Disk) -> Vec<(NodeId, Listing)> {
    let users: Vec<(NodeId, Option<PathBuf>)> = {
        let t = job.tree().read();
        bin_user_folders(&t)
            .into_iter()
            .map(|u| (u, job.node_path_in(&t, u)))
            .collect()
    };
    users
        .into_iter()
        .map(|(u, dir)| (u, Listing::of(dir.as_deref(), disk)))
        .collect()
}

/// Removes the Recycle Bin items that are gone on disk from a scan's tree;
/// returns how many nodes were removed (each with its subtree).
fn remove_gone_bin_items(job: &ScanJob, disk: &dyn Disk) -> usize {
    let listings = bin_listings(job, disk);
    if listings.is_empty() {
        return 0;
    }
    let mut t = job.tree().write();
    let gone = gone_bin_items(&t, &listings);
    for &id in &gone {
        t.remove(id);
    }
    gone.len()
}

/// After a recycle, a permanent delete or "Empty Recycle Bin" (`kind`, on
/// `paths`) ran in one scan: brings the other scans in memory (`others`,
/// with their ids) in line with the disk, see "Deletes" in the
/// [module docs](self). Returns the ids of the scans whose root folder is
/// gone. Blocking (disk checks, drive queries): call it on a worker thread.
pub(super) fn sync_after_delete(
    others: &[(u64, Arc<ScanJob>)],
    kind: &ConfirmKind,
    paths: &[PathBuf],
    disk: &dyn Disk,
) -> Vec<u64> {
    let gone: Vec<String> = paths
        .iter()
        .filter(|p| disk.gone(p))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let letters: Vec<char> = gone.iter().filter_map(|p| path_letter(p)).collect();
    let empty_bin = *kind == ConfirmKind::EmptyBin;
    if gone.is_empty() && !empty_bin {
        return Vec::new();
    }
    let mut roots_gone = Vec::new();
    for (id, job) in others {
        let removed = remove_paths(job.tree(), &gone);
        let bin_items = if empty_bin {
            remove_gone_bin_items(job, disk)
        } else {
            0
        };
        // The free / unaccounted space of the drives the operation touched
        // (a scan still running adds them when its drives finish).
        let touched = empty_bin
            || removed.nodes > 0
            || job
                .drives()
                .iter()
                .any(|d| letters.contains(&d.info.letter));
        if (touched || bin_items > 0) && job.is_done() {
            job.refresh_drive_extras();
        }
        if removed.root_gone {
            roots_gone.push(*id);
        }
    }
    roots_gone
}

/// The engine jobs of the scans in memory (the current one and the cached
/// ones), shared with the worker of a delete: it syncs the trees that are
/// in memory when its operation has finished, including a scan started
/// while it ran. Weak, so an evicted scan is freed (and skipped) as usual.
#[derive(Clone, Default)]
pub(super) struct Jobs(Arc<Mutex<Vec<JobRef>>>);

/// A scan id and its engine job, if the scan is still in memory.
type JobRef = (u64, Weak<ScanJob>);

impl Jobs {
    /// Registers a new scan (synthetic data has no job and is skipped).
    pub fn add(&self, scan: &Scan) {
        let Some(job) = scan.job() else { return };
        let mut jobs = self.0.lock();
        jobs.retain(|(_, j)| j.strong_count() > 0);
        jobs.push((scan.id, Arc::downgrade(job)));
    }

    /// The jobs of the scans still in memory, except scan `except`'s.
    pub fn alive(&self, except: u64) -> Vec<(u64, Arc<ScanJob>)> {
        self.0
            .lock()
            .iter()
            .filter(|(id, _)| *id != except)
            .filter_map(|(id, j)| Some((*id, j.upgrade()?)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringscan_core::SizeMode;

    /// A synthetic "This PC" with drives C: and D:.
    fn this_pc() -> Tree {
        let mut t = Tree::new(ringscan_engine::THIS_PC_NAME, flags::VIRTUAL);
        for drive in ["C:\\", "D:\\"] {
            let d = t.add_dir(0, drive, 0, 0);
            let w = t.add_dir(d, "Windows", 0, 0);
            let web = t.add_dir(w, "Web", 0, 0);
            t.add_file(web, "a.jpg", 100, 4096, 0, 0);
            t.add_dir(d, "Docs", flags::REPARSE, 0);
            t.add_file(d, "x.txt", 10, 4096, 0, 0);
        }
        t
    }

    /// A synthetic folder scan rooted at `root`.
    fn folder(root: &str) -> Tree {
        let mut t = Tree::new(root, 0);
        let s = t.add_dir(0, "Screen", 0, 0);
        t.add_file(s, "img.jpg", 100, 4096, 0, 0);
        t
    }

    fn lock(t: Tree) -> RwLock<Tree> {
        RwLock::new(t)
    }

    fn cand(tree: &RwLock<Tree>) -> Candidate<'_> {
        Candidate {
            tree,
            demo: false,
            cancelled: false,
            failed: false,
            volumes: Vec::new(),
        }
    }

    /// The drive list: these letters as ready fixed NTFS drives.
    fn fixed(letters: &str) -> Vec<DriveInfo> {
        letters
            .chars()
            .map(|letter| DriveInfo {
                letter,
                label: String::new(),
                fs: "NTFS".into(),
                kind: DriveKind::Fixed,
                total: Some(1000),
                free: Some(500),
                this_pc: true,
            })
            .collect()
    }

    /// [`locate`] with C:, D: and G: as fixed drives.
    fn loc(current: Option<&Candidate>, cached: &[Candidate], want: &Want) -> Place {
        locate(current, cached, want, &fixed("CDG"))
    }

    fn find(tree: &RwLock<Tree>, path: &str) -> NodeId {
        tree.read().find_path(path).expect(path)
    }

    #[test]
    fn drives_come_from_the_current_tree_then_the_cache() {
        let pc = lock(this_pc());
        let web = lock(folder("C:\\Windows\\Web"));
        let g = lock(folder("G:\\"));
        // C: is in the current "This PC": navigate, never scan.
        let at = loc(Some(&cand(&pc)), &[cand(&g)], &Want::Drive('C'));
        assert_eq!(at, Place::Current(find(&pc, "C:\\")));
        // Back from G: to C: (the reported bug): the cached "This PC".
        let at = loc(Some(&cand(&g)), &[cand(&web), cand(&pc)], &Want::Drive('C'));
        assert_eq!(at, Place::Cached(1, find(&pc, "C:\\")));
        // Lower case letters and the current drive scan itself.
        assert_eq!(
            loc(Some(&cand(&g)), &[], &Want::Drive('g')),
            Place::Current(0)
        );
        // A folder scan inside C: does not contain the drive.
        assert_eq!(
            loc(Some(&cand(&web)), &[], &Want::Drive('C')),
            Place::NewScan
        );
        // Nothing has E:.
        assert_eq!(
            loc(Some(&cand(&g)), &[cand(&pc)], &Want::Drive('E')),
            Place::NewScan
        );
        // The most recently used cached scan wins.
        let c_scan = lock(folder("C:\\"));
        let at = loc(
            Some(&cand(&g)),
            &[cand(&c_scan), cand(&pc)],
            &Want::Drive('C'),
        );
        assert_eq!(at, Place::Cached(0, 0));
    }

    #[test]
    fn this_pc_is_reused() {
        let pc = lock(this_pc());
        let g = lock(folder("G:\\"));
        assert_eq!(
            loc(Some(&cand(&pc)), &[cand(&g)], &Want::ThisPc),
            Place::Current(0)
        );
        assert_eq!(
            loc(Some(&cand(&g)), &[cand(&g), cand(&pc)], &Want::ThisPc),
            Place::Cached(1, 0)
        );
        assert_eq!(loc(Some(&cand(&g)), &[], &Want::ThisPc), Place::NewScan);
        // Even a stopped "This PC" is shown again (F5 rescans it).
        let stopped = Candidate {
            cancelled: true,
            ..cand(&pc)
        };
        assert_eq!(
            loc(Some(&cand(&g)), &[stopped], &Want::ThisPc),
            Place::Cached(0, 0)
        );
    }

    #[test]
    fn folders_prefer_their_own_scan_and_skip_stopped_ones() {
        let pc = lock(this_pc());
        let web = lock(folder("C:\\Windows\\Web"));
        let want = Want::Folder("c:/windows/web/".into());
        // The scan of exactly this folder beats the current "This PC".
        assert_eq!(
            loc(Some(&cand(&pc)), &[cand(&web)], &want),
            Place::Cached(0, 0)
        );
        // Without it, "This PC" has the folder.
        let at = loc(Some(&cand(&pc)), &[], &want);
        assert_eq!(at, Place::Current(find(&pc, "C:\\Windows\\Web")));
        let at = loc(
            Some(&cand(&web)),
            &[cand(&pc)],
            &Want::Folder("C:\\Windows".into()),
        );
        assert_eq!(at, Place::Cached(0, find(&pc, "C:\\Windows")));
        // A folder below the current scan's root.
        let at = loc(
            Some(&cand(&web)),
            &[cand(&pc)],
            &Want::Folder("C:\\Windows\\Web\\Screen".into()),
        );
        assert_eq!(at, Place::Current(find(&web, "C:\\Windows\\Web\\Screen")));
        // A stopped scan never serves a folder: its tree may miss it.
        let stopped = Candidate {
            cancelled: true,
            ..cand(&pc)
        };
        assert_eq!(loc(Some(&stopped), &[], &want), Place::NewScan);
        assert_eq!(
            loc(
                Some(&cand(&web)),
                &[stopped],
                &Want::Folder("C:\\Windows".into())
            ),
            Place::NewScan
        );
        // Junctions are listed but not followed: scan their target instead.
        assert_eq!(
            loc(Some(&cand(&pc)), &[], &Want::Folder("C:\\Docs".into())),
            Place::NewScan
        );
        // Files are not folders.
        assert_eq!(
            loc(Some(&cand(&pc)), &[], &Want::Folder("C:\\x.txt".into())),
            Place::NewScan
        );
        // A drive root as a path.
        let at = loc(
            Some(&cand(&web)),
            &[cand(&pc)],
            &Want::Folder("D:\\".into()),
        );
        assert_eq!(at, Place::Cached(0, find(&pc, "D:\\")));
    }

    #[test]
    fn synthetic_trees_never_stand_in_for_real_ones() {
        let demo_pc = lock(this_pc());
        let demo = || Candidate {
            demo: true,
            ..cand(&demo_pc)
        };
        let g = lock(folder("G:\\"));
        // Cached demo data is never matched.
        assert_eq!(
            loc(Some(&cand(&g)), &[demo()], &Want::Drive('C')),
            Place::NewScan
        );
        assert_eq!(
            loc(Some(&cand(&g)), &[demo()], &Want::ThisPc),
            Place::NewScan
        );
        // The current demo tree navigates its own drives, scans nothing else
        // and never serves a real folder.
        let at = loc(Some(&demo()), &[], &Want::Drive('D'));
        assert_eq!(at, Place::Current(find(&demo_pc, "D:\\")));
        assert_eq!(loc(Some(&demo()), &[], &Want::Drive('E')), Place::Nowhere);
        assert_eq!(loc(Some(&demo()), &[], &Want::ThisPc), Place::Current(0));
        assert_eq!(
            loc(Some(&demo()), &[], &Want::Folder("C:\\Windows".into())),
            Place::NewScan
        );
    }

    #[test]
    fn wants_for_paths() {
        assert_eq!(Want::for_path(Path::new("all")), Want::ThisPc);
        assert_eq!(Want::for_path(Path::new("d:")), Want::Folder("D:\\".into()));
        assert_eq!(
            Want::for_path(Path::new("C:\"")),
            Want::Folder("C:\\".into())
        );
        let abs = std::env::temp_dir().join("x");
        assert_eq!(
            Want::for_path(&abs),
            Want::Folder(abs.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn paths_within_folders() {
        assert!(path_within("C:\\a\\b", "c:\\A"));
        assert!(path_within("C:\\a", "C:\\a\\"));
        assert!(path_within("\\\\?\\C:\\a\\b", "C:/a"));
        assert!(!path_within("C:\\ab", "C:\\a"));
        assert!(!path_within("C:\\", "C:\\a"));
        assert!(!path_within("C:\\a", ""));
    }

    #[test]
    fn deleted_paths_leave_other_trees() {
        let pc = lock(this_pc());
        let before = pc.read().size(0, SizeMode::Allocated);
        let gone = vec![
            "C:\\Windows\\Web".to_string(),
            "D:\\x.txt".to_string(),
            "C:\\not-scanned".to_string(),
            "D:\\".to_string(),
        ];
        let r = remove_paths(&pc, &gone);
        assert_eq!(
            r,
            Removed {
                nodes: 2,
                root_gone: false
            }
        );
        let t = pc.read();
        assert!(t.find_path("C:\\Windows\\Web").is_none());
        assert!(t.find_path("C:\\Windows").is_some());
        assert!(t.find_path("D:\\x.txt").is_none());
        assert!(t.find_path("D:\\").is_some(), "drive roots stay");
        assert_eq!(t.size(0, SizeMode::Allocated), before - 2 * 4096);
        drop(t);
        // The root folder of a cached scan was deleted (itself or a parent).
        for p in ["C:\\Windows", "c:\\windows\\web"] {
            let web = lock(folder("C:\\Windows\\Web"));
            let r = remove_paths(&web, &[p.to_string()]);
            assert!(r.root_gone, "{p}");
            let t = web.read();
            assert!(!t.has_children(0));
            assert_eq!(t.size(0, SizeMode::Allocated), 0);
        }
        // Unrelated paths change nothing.
        let web = lock(folder("C:\\Windows\\Web"));
        let generation = web.read().generation();
        let r = remove_paths(&web, &["C:\\Windows\\Webcam".to_string()]);
        assert_eq!(r, Removed::default());
        assert_eq!(web.read().generation(), generation);
    }

    /// A fake disk: `gone` paths are not found, `denied` paths (and
    /// everything below them) cannot be looked at, every other path exists;
    /// `folders` lists what a folder holds.
    #[derive(Default)]
    struct FakeDisk {
        gone: Vec<PathBuf>,
        denied: Vec<PathBuf>,
        folders: Vec<(PathBuf, Vec<&'static str>)>,
    }

    impl FakeDisk {
        fn within(p: &Path, list: &[PathBuf]) -> bool {
            list.iter()
                .any(|x| path_within(&p.to_string_lossy(), &x.to_string_lossy()))
        }
    }

    impl Disk for FakeDisk {
        fn gone(&self, p: &Path) -> bool {
            Self::within(p, &self.gone) && !Self::within(p, &self.denied)
        }

        fn list(&self, dir: &Path) -> io::Result<HashSet<String>> {
            if Self::within(dir, &self.denied) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            if Self::within(dir, &self.gone) {
                return Err(io::ErrorKind::NotFound.into());
            }
            let names = self
                .folders
                .iter()
                .find(|(f, _)| f == dir)
                .map(|(_, n)| n.iter().map(|n| n.to_lowercase()).collect());
            Ok(names.unwrap_or_default())
        }
    }

    #[test]
    fn recycle_bin_user_folders() {
        let mut t = this_pc();
        let c = t.find_path("C:\\").unwrap();
        let bin = t.add_dir(c, "$Recycle.Bin", flags::RECYCLE, 0);
        let user = t.add_dir(bin, "S-1-5-21-1", 0, 0);
        t.add_file(bin, "desktop.ini", 1, 4096, 0, 0);
        // A folder of that name below the drive root is not the bin.
        let w = t.find_path("C:\\Windows").unwrap();
        let fake = t.add_dir(w, "$Recycle.Bin", 0, 0);
        t.add_dir(fake, "S-1-5-21-1", 0, 0);
        assert_eq!(bin_user_folders(&t), vec![user]);
        // A drive scan: the root is the drive.
        let mut d = Tree::new("E:\\", 0);
        let bin = d.add_dir(0, "$RECYCLE.BIN", flags::RECYCLE, 0);
        let user = d.add_dir(bin, "S-1-5-21-2", 0, 0);
        assert_eq!(bin_user_folders(&d), vec![user]);
        assert!(bin_user_folders(&folder("C:\\Windows\\Web")).is_empty());
    }

    /// "Empty Recycle Bin" with other users' bins in the tree (an MFT read
    /// shows them without elevation): only what a listing proves gone goes.
    #[test]
    fn emptied_bin_keeps_what_cannot_be_listed() {
        let mut t = this_pc();
        let c = t.find_path("C:\\").unwrap();
        let bin = t.add_dir(c, "$Recycle.Bin", flags::RECYCLE, 0);
        // Mine, listable: one item was emptied, one is new on disk only.
        let mine = t.add_dir(bin, "S-1-5-21-1", 0, 0);
        let emptied = t.add_dir(mine, "$R1X2", 0, 0);
        t.add_file(emptied, "inside.txt", 1, 4096, 0, 0);
        let kept = t.add_file(mine, "desktop.ini", 100, 4096, 0, 0);
        // Another user's: access denied, so it is still there.
        let other = t.add_dir(bin, "S-1-5-21-2", 0, 0);
        let foreign = t.add_file(other, "$IAB", 100, 4096, 0, 0);
        // A user folder that is gone altogether.
        let vanished = t.add_dir(bin, "S-1-5-21-3", 0, 0);
        t.add_file(vanished, "$RQ", 100, 4096, 0, 0);
        let disk = FakeDisk {
            gone: vec![PathBuf::from("C:\\$Recycle.Bin\\S-1-5-21-3")],
            denied: vec![PathBuf::from("C:\\$Recycle.Bin\\S-1-5-21-2")],
            folders: vec![(
                PathBuf::from("C:\\$Recycle.Bin\\S-1-5-21-1"),
                vec!["DESKTOP.INI", "$RNEW"],
            )],
        };
        let listings: Vec<(NodeId, Listing)> = bin_user_folders(&t)
            .into_iter()
            .map(|u| (u, Listing::of(Some(Path::new(&t.path(u))), &disk)))
            .collect();
        assert!(matches!(listings[1].1, Listing::Unreadable), "{listings:?}");
        let gone = gone_bin_items(&t, &listings);
        assert_eq!(gone, vec![emptied, vanished]);
        for id in gone {
            t.remove(id);
        }
        assert!(t.is_live(kept) && t.is_live(foreign) && t.is_live(other));
        // No path (a node inside a virtual container): kept.
        assert!(matches!(Listing::of(None, &disk), Listing::Unreadable));
    }

    /// [`OnDisk`] on a real disk: an existing file is not gone, a missing
    /// one (or one in a missing folder) is.
    #[test]
    fn on_disk_gone_means_not_found() {
        let dir = std::env::temp_dir()
            .join("ringscan-dev")
            .join(format!("app-disk-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        assert!(!OnDisk.gone(&dir.join("a.txt")));
        assert!(OnDisk.gone(&dir.join("missing.txt")));
        // A missing parent folder (ERROR_PATH_NOT_FOUND on Windows).
        assert!(OnDisk.gone(&dir.join("no-dir").join("x.txt")));
        let names = OnDisk.list(&dir).unwrap();
        assert_eq!(names, HashSet::from(["a.txt".to_string()]));
        assert_eq!(
            OnDisk.list(&dir.join("no-dir")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The real "access denied" cases on Windows (not elevated): another
    /// user's Recycle Bin folder and System Volume Information are still
    /// there. Skipped where they do not answer "access denied" (elevated,
    /// a single-user machine).
    #[cfg(windows)]
    #[test]
    fn access_denied_is_not_gone() {
        let denied = |p: &Path| matches!(std::fs::read_dir(p), Err(e) if e.kind() == io::ErrorKind::PermissionDenied);
        let mut checked = 0;
        let svi = Path::new(r"C:\System Volume Information");
        if denied(svi) {
            assert!(!OnDisk.gone(&svi.join("IndexerVolumeGuid")));
            assert!(matches!(
                Listing::of(Some(svi), &OnDisk),
                Listing::Unreadable
            ));
            checked += 1;
        }
        for letter in ['C', 'D'] {
            let bin = PathBuf::from(format!(r"{letter}:\$Recycle.Bin"));
            let Ok(users) = std::fs::read_dir(&bin) else {
                continue;
            };
            for user in users.flatten().map(|e| e.path()).filter(|p| denied(p)) {
                assert!(
                    !OnDisk.gone(&user.join("desktop.ini")),
                    "{}",
                    user.display()
                );
                assert!(!OnDisk.gone(&user));
                assert!(matches!(
                    Listing::of(Some(&user), &OnDisk),
                    Listing::Unreadable
                ));
                checked += 1;
            }
        }
        eprintln!("access denied checked on {checked} folders");
    }

    /// A drive whose MFT can be read without elevation (the development
    /// machine's D:) shows every user's Recycle Bin items. Nothing was
    /// emptied, so none of them may count as gone (before the fix every
    /// other user's item did). Reads the whole drive, so run it by hand:
    /// `cargo test --target x86_64-pc-windows-gnu -p ringscan -- --ignored
    /// real_bin` (`RINGSCAN_BIN_DRIVE=E` for another drive).
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn real_bin_items_are_not_gone() {
        let letter = std::env::var("RINGSCAN_BIN_DRIVE")
            .ok()
            .and_then(|s| s.chars().next())
            .unwrap_or('D');
        let job = ScanJob::start(
            Target::Drive(letter),
            ringscan_engine::ScanOptions::default(),
        );
        let p = job.wait();
        let listings = bin_listings(&job, &OnDisk);
        let t = job.tree().read();
        let items: usize = listings.iter().map(|(u, _)| t.child_count(*u)).sum();
        let unreadable = listings
            .iter()
            .filter(|(_, l)| matches!(l, Listing::Unreadable))
            .count();
        let gone = gone_bin_items(&t, &listings);
        // What the old check (any metadata error = gone) would have removed.
        let old_gone = listings
            .iter()
            .flat_map(|(u, _)| t.children(*u))
            .filter_map(|i| job.node_path_in(&t, i))
            .filter(|p| std::fs::symlink_metadata(p).is_err())
            .count();
        eprintln!(
            "{letter}: {:?}, {} user folders ({unreadable} unreadable), {items} items, \
             {} gone (old check: {old_gone})",
            p.method,
            listings.len(),
            gone.len()
        );
        assert!(
            gone.is_empty(),
            "{:?}",
            gone.iter().map(|&g| t.path(g)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn failed_scans_are_scanned_again() {
        // A scan of a folder that did not exist (or a drive that was not
        // ready): ERROR on the root and nothing real below it.
        let mut t = Tree::new("C:\\data\\later", 0);
        t.set_flags(0, flags::ERROR, 0);
        let failed_tree = lock(t);
        let failed = || Candidate {
            failed: true,
            ..cand(&failed_tree)
        };
        let want = Want::Folder("C:\\data\\later".into());
        let pc = lock(this_pc());
        // Not as the current scan, not from the cache: scan again.
        assert_eq!(loc(Some(&failed()), &[cand(&pc)], &want), Place::NewScan);
        assert_eq!(loc(Some(&cand(&pc)), &[failed()], &want), Place::NewScan);
        let mut e = Tree::new("E:\\", 0);
        e.set_flags(0, flags::ERROR, 0);
        let e = lock(e);
        let e_failed = Candidate {
            failed: true,
            ..cand(&e)
        };
        let drives = fixed("CE");
        let want = Want::Drive('E');
        assert_eq!(locate(Some(&e_failed), &[], &want, &drives), Place::NewScan);
        assert_eq!(
            locate(Some(&cand(&pc)), &[e_failed], &want, &drives),
            Place::NewScan
        );
        // What counts as failed.
        let mut t = Tree::new("E:\\", 0);
        assert!(!root_failed(&t), "an empty folder is fine");
        t.set_flags(0, flags::ERROR, 0);
        t.add_virtual(0, ringscan_engine::FREE_NAME, flags::FREE, 100);
        assert!(
            root_failed(&t),
            "only <free space> below an unreadable root"
        );
        t.add_file(0, "a.txt", 1, 4096, 0, 0);
        assert!(!root_failed(&t), "partly listed");
    }

    /// A real scan of a folder that does not exist fails, is never served
    /// and is not kept in the cache.
    #[test]
    fn failed_scans_are_not_cached() {
        let missing = std::env::temp_dir()
            .join("ringscan-dev")
            .join(format!("app-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let scan = real_scan(&missing);
        assert!(is_failed(&scan));
        assert!(Candidate::of(&scan).failed);
        let mut cache = ScanCache::default();
        cache.park(scan, View::default());
        assert_eq!(cache.iter().count(), 0);
        // Once the folder exists, a new scan of it is fine.
        std::fs::create_dir_all(&missing).unwrap();
        std::fs::write(missing.join("file.bin"), b"x").unwrap();
        let scan = real_scan(&missing);
        assert!(!is_failed(&scan));
        cache.park(scan, View::default());
        assert_eq!(cache.iter().count(), 1);
        drop(cache);
        std::fs::remove_dir_all(&missing).unwrap();
    }

    #[test]
    fn cached_trees_need_the_same_volume() {
        let e = lock(folder("E:\\"));
        let pc = lock(this_pc());
        let stamped = |label: &str| Candidate {
            volumes: vec![Volume {
                letter: 'E',
                label: label.into(),
                fs: "NTFS".into(),
                total: 1000,
            }],
            ..cand(&e)
        };
        let want = Want::Drive('E');
        let mut drives = fixed("CDE");
        // The same fixed volume: served from the cache.
        let at = locate(Some(&cand(&pc)), &[stamped("")], &want, &drives);
        assert_eq!(at, Place::Cached(0, 0));
        // Another volume behind E: (label, file system or size differ).
        let at = locate(Some(&cand(&pc)), &[stamped("USB")], &want, &drives);
        assert_eq!(at, Place::NewScan);
        drives[2].total = Some(2000);
        let at = locate(Some(&cand(&pc)), &[stamped("")], &want, &drives);
        assert_eq!(at, Place::NewScan);
        // A fixed drive that did not answer the drive query in time (no
        // facts) is not taken for another volume.
        let mut slow = fixed("CDE");
        slow[2].total = None;
        slow[2].label = String::new();
        let at = locate(Some(&cand(&pc)), &[stamped("USB")], &want, &slow);
        assert_eq!(at, Place::Cached(0, 0));
        // Removable (also for a folder scan on it, which has no stamp) and
        // missing drives are never served from the cache...
        drives[2].total = Some(1000);
        drives[2].kind = DriveKind::Removable;
        let at = locate(Some(&cand(&pc)), &[stamped("")], &want, &drives);
        assert_eq!(at, Place::NewScan);
        let photos = lock(folder("E:\\Photos"));
        let at = locate(
            Some(&cand(&pc)),
            &[cand(&photos)],
            &Want::Folder("E:\\Photos\\Screen".into()),
            &drives,
        );
        assert_eq!(at, Place::NewScan);
        let at = locate(Some(&cand(&pc)), &[stamped("")], &want, &fixed("CD"));
        assert_eq!(at, Place::NewScan);
        // ...but the current tree is used as before.
        let at = locate(Some(&stamped("")), &[], &want, &drives);
        assert_eq!(at, Place::Current(0));
        // Network paths are never served from the cache either.
        let share = lock(folder("\\\\server\\share"));
        let at = locate(
            Some(&cand(&pc)),
            &[cand(&share)],
            &Want::Folder("\\\\server\\share\\Screen".into()),
            &drives,
        );
        assert_eq!(at, Place::NewScan);
        // "This PC" holds fixed drives only: no check.
        let at = locate(Some(&cand(&e)), &[cand(&pc)], &Want::ThisPc, &[]);
        assert_eq!(at, Place::Cached(0, 0));
    }

    /// A delete (or a failed one) in one scan, synced into another scan of
    /// the same folder: only paths that are not found leave the tree.
    #[test]
    fn sync_counts_only_not_found_as_gone() {
        let dir = std::env::temp_dir()
            .join("ringscan-dev")
            .join(format!("app-sync-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub").join("inner")).unwrap();
        for f in ["a.txt", "b.txt"] {
            std::fs::write(dir.join(f), vec![1u8; 1000]).unwrap();
        }
        std::fs::write(dir.join("sub").join("inner").join("c.txt"), b"x").unwrap();
        let outer = real_scan(&dir);
        let inner = real_scan(&dir.join("sub").join("inner"));
        let jobs = Jobs::default();
        jobs.add(&outer);
        jobs.add(&inner);
        let others = jobs.alive(0);
        assert_eq!(others.len(), 2);
        // a.txt and sub were deleted; b.txt could not be (access denied).
        let disk = FakeDisk {
            gone: vec![dir.join("a.txt"), dir.join("sub")],
            denied: vec![dir.join("b.txt")],
            ..FakeDisk::default()
        };
        let paths = [dir.join("a.txt"), dir.join("b.txt"), dir.join("sub")];
        let roots_gone = sync_after_delete(&others, &ConfirmKind::Delete, &paths, &disk);
        assert_eq!(roots_gone, vec![inner.id]);
        let t = outer.tree.read();
        assert!(t.find_child(0, "a.txt").is_none());
        assert!(t.find_child(0, "b.txt").is_some(), "denied is not gone");
        assert!(t.find_child(0, "sub").is_none());
        assert!(!inner.tree.read().has_children(0));
        drop(t);
        // Nothing gone: nothing changes.
        let generation = outer.tree.read().generation();
        let disk = FakeDisk {
            denied: vec![dir.join("b.txt")],
            ..FakeDisk::default()
        };
        let paths = [dir.join("b.txt")];
        let roots_gone = sync_after_delete(&others, &ConfirmKind::Recycle, &paths, &disk);
        assert!(roots_gone.is_empty());
        assert_eq!(outer.tree.read().generation(), generation);
        drop((others, outer, inner));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn jobs_lists_the_scans_still_in_memory() {
        let dir = std::env::temp_dir()
            .join("ringscan-dev")
            .join(format!("app-jobs-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let jobs = Jobs::default();
        let a = real_scan(&dir);
        let b = real_scan(&dir);
        jobs.add(&a);
        jobs.add(&b);
        jobs.add(&demo_scan("X:\\"));
        let ids = |v: Vec<(u64, Arc<ScanJob>)>| v.into_iter().map(|j| j.0).collect::<Vec<_>>();
        assert_eq!(ids(jobs.alive(0)), vec![a.id, b.id]);
        assert_eq!(ids(jobs.alive(a.id)), vec![b.id]);
        // An evicted (dropped) scan is gone from the list.
        let b_id = b.id;
        drop(b);
        assert_eq!(ids(jobs.alive(0)), vec![a.id]);
        let c = real_scan(&dir);
        jobs.add(&c);
        assert_eq!(jobs.0.lock().len(), 2, "dead entries are pruned");
        assert!(ids(jobs.alive(0)).iter().all(|&id| id != b_id));
        drop((a, c));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn demo_scan(root: &str) -> Scan {
        Scan::with_tree(folder(root))
    }

    fn view(focus: NodeId) -> View {
        View {
            focus,
            back: vec![focus + 100],
            selected: Some(focus + 200),
        }
    }

    #[test]
    fn swapping_keeps_each_view_with_its_tree() {
        let a = demo_scan("A:\\");
        let b = demo_scan("B:\\");
        let (a_id, b_id) = (a.id, b.id);
        let mut cache = ScanCache::default();
        cache.park(b, view(2));
        let mut current = Some(a);
        let mut v = view(1);
        assert!(cache.swap_in(0, &mut current, &mut v));
        assert_eq!(current.as_ref().unwrap().id, b_id);
        assert_eq!(v, view(2));
        let cached: Vec<(u64, &View)> = cache.iter().map(|c| (c.scan.id, &c.view)).collect();
        assert_eq!(cached, vec![(a_id, &view(1))]);
        // And back.
        assert!(cache.swap_in(0, &mut current, &mut v));
        assert_eq!((current.as_ref().unwrap().id, &v), (a_id, &view(1)));
        assert!(!cache.swap_in(5, &mut current, &mut v));
        assert_eq!(cache.iter().count(), 1);
    }

    #[test]
    fn least_recently_used_scans_are_dropped() {
        let mut cache = ScanCache::default();
        let scans: Vec<Scan> = (0..5)
            .map(|i| demo_scan(&format!("{}:\\", (b'K' + i) as char)))
            .collect();
        let ids: Vec<u64> = scans.iter().map(|s| s.id).collect();
        for (i, s) in scans.into_iter().enumerate() {
            cache.park(s, view(i as NodeId));
        }
        assert_eq!(cache.iter().count(), ScanCache::CAPACITY);
        let kept: Vec<u64> = cache.iter().map(|c| c.scan.id).collect();
        assert_eq!(kept, vec![ids[4], ids[3], ids[2]]);
        // Swapping one in makes the current one the most recently used.
        let current_scan = demo_scan("Z:\\");
        let z = current_scan.id;
        let mut current = Some(current_scan);
        let mut v = View::default();
        cache.swap_in(2, &mut current, &mut v);
        assert_eq!(current.as_ref().unwrap().id, ids[2]);
        let kept: Vec<u64> = cache.iter().map(|c| c.scan.id).collect();
        assert_eq!(kept, vec![z, ids[4], ids[3]]);
    }

    /// A real scan of a temporary folder (the engine's std lister on Linux).
    fn real_scan(dir: &Path) -> Scan {
        let s = Scan::path(dir);
        while s.is_running() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        s
    }

    #[test]
    fn new_scans_supersede_scans_of_the_same_root() {
        let dir = std::env::temp_dir()
            .join("ringscan-dev")
            .join(format!("app-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("a.txt"), vec![1u8; 1000]).unwrap();
        let mut cache = ScanCache::default();
        let mut view = View::default();
        let mut current = None;
        // Folder, then its subfolder: both kept.
        let outer = real_scan(&dir);
        let outer_id = outer.id;
        cache.start(outer, &mut current, &mut view);
        view.focus = 3;
        let inner = real_scan(&dir.join("sub"));
        inner.mark_stopped();
        assert!(inner.is_cancelled());
        let inner_id = inner.id;
        cache.start(inner, &mut current, &mut view);
        assert_eq!(view, View::default());
        assert_eq!(cache.iter().map(|c| c.view.focus).collect::<Vec<_>>(), [3]);
        // The stopped subfolder scan is not offered for the subfolder...
        let want = Want::for_path(&dir.join("sub"));
        let place = {
            let cur = current.as_ref().map(Candidate::of);
            let cached: Vec<Candidate> = cache.iter().map(|c| Candidate::of(&c.scan)).collect();
            loc(cur.as_ref(), &cached, &want)
        };
        // ...but the outer folder's scan contains it.
        assert!(
            matches!(place, Place::Cached(0, id) if id != 0),
            "{place:?}"
        );
        // Scanning the subfolder again replaces the stopped scan instead of
        // caching it.
        let again = real_scan(&dir.join("sub"));
        cache.start(again, &mut current, &mut view);
        let cached: Vec<u64> = cache.iter().map(|c| c.scan.id).collect();
        assert_eq!(cached, vec![outer_id]);
        assert_ne!(current.as_ref().unwrap().id, inner_id);
        // And a new scan of the outer folder drops the cached one.
        let outer2 = real_scan(&dir);
        cache.start(outer2, &mut current, &mut view);
        assert_eq!(cache.iter().count(), 1, "the subfolder scan is cached");
        assert!(cache.iter().all(|c| c.scan.id != outer_id));
        drop(current);
        drop(cache);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
