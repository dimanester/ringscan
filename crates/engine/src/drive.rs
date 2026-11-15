//! Drives: which ones "all fixed drives" means, and the virtual FREE and
//! UNACCOUNTED nodes at a scanned drive's root.
//!
//! # Drive root extras
//!
//! After a whole drive has been scanned, [`apply_drive_extras`] gives its
//! node up to three virtual children:
//!
//! * `<free space>` (FREE): the free bytes Windows reports. FREE nodes are
//!   never part of an ancestor's size; the layout adds them back when free
//!   space is shown, so the drive then spans its whole capacity.
//! * `<reserved by Windows>` (UNACCOUNTED, named [`RESERVED_NAME`]):
//!   clusters the file system keeps back without any file using them, as
//!   `GetDiskSpaceInformationW` reports them (`TotalReservedAllocationUnits`):
//!   the Windows storage reserve ("reserved storage", ~5 GB on a system
//!   drive, used for updates) plus NTFS's own reservations. The free space
//!   Windows reports already leaves them out, so without this node they
//!   would look like data the scan missed.
//! * `<unaccounted>` (UNACCOUNTED): `used − reserved − scanned`, where
//!   `used = total − free` and `scanned` is the allocated size the scan
//!   found below the drive. It is the space the scan could not see: folders
//!   it was denied (`System Volume Information` with its shadow copies,
//!   other users' profiles), NTFS metadata in walk mode (`$MFT`, `$LogFile`,
//!   directory indexes), alternate data streams, and in MFT mode only the
//!   root directory's own index and a few non-resident attributes the
//!   reader does not count (measured: ~0.2 GB on a 2 TB system drive, 160 KB
//!   on a data drive). With both nodes the drive's size equals the used
//!   space Windows reports.
//!
//!   When the scan finds *more* than the allocated space (possible when
//!   hard links are counted more than once, e.g. with hard-link
//!   de-duplication turned off or after a sub-folder rescan, when files
//!   were deleted while the scan ran, or with deduplicated volumes), the
//!   difference cannot be shown as a negative segment: UNACCOUNTED is set
//!   to 0 and [`DriveSpace::overcount`] reports the excess.
//!
//! Applying the extras again (after a rescan or a delete) updates the same
//! nodes in place, so their ids stay stable; a rescan of the whole drive
//! clears them with everything else and they are re-created.
//!
//! # Which drives "all fixed drives" includes
//!
//! [`fixed_drives`]: `DriveKind::Fixed` drives that are ready (`total > 0`)
//! and either sit on a known physical disk or are NTFS / ReFS. This keeps
//! out virtual drives that claim to be fixed, such as Google Drive's
//! FAT32 drive (no physical disk, reports the system drive's capacity),
//! while keeping NTFS volumes that span disks (dynamic / Storage Spaces).

use ringscan_core::{NodeId, SizeMode, Tree, flags};
use ringscan_win::{DriveInfo, DriveKind};

/// Name of the virtual free-space node of a drive.
pub const FREE_NAME: &str = "<free space>";
/// Name of the virtual "used but not found by the scan" node of a drive.
pub const UNACCOUNTED_NAME: &str = "<unaccounted>";
/// Name of the virtual node for the space the file system keeps reserved
/// (an UNACCOUNTED node with this name; see the [module docs](self)).
pub const RESERVED_NAME: &str = "<reserved by Windows>";

/// Space accounting of one scanned drive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DriveSpace {
    /// Capacity visible to the user.
    pub total: u64,
    pub free: u64,
    /// `total − free`.
    pub used: u64,
    /// Reserved by the file system (part of `used`; 0 when unknown).
    pub reserved: u64,
    /// Allocated bytes found by the scan below the drive node.
    pub scanned: u64,
    /// `max(0, used − reserved − scanned)`: size of the UNACCOUNTED node.
    pub unaccounted: u64,
    /// `max(0, scanned − (used − reserved))`: what the scan found beyond
    /// the allocated space.
    pub overcount: u64,
}

impl DriveSpace {
    /// `used − reserved`: what files and file-system metadata occupy.
    pub fn allocated(&self) -> u64 {
        self.used.saturating_sub(self.reserved)
    }
}

/// Whether node `id` is the `<reserved by Windows>` node of a drive.
pub fn is_reserved(t: &Tree, id: NodeId) -> bool {
    t.node(id).has(flags::UNACCOUNTED) && t.name(id) == RESERVED_NAME
}

/// Adds or updates the FREE, reserved and UNACCOUNTED children of drive
/// node `node` (see the [module docs](self)) and returns the resulting
/// accounting. `reserved` is clamped to the used space. Nodes are only
/// created when their size is non-zero; existing ones are updated
/// (possibly to 0). No-op on a dead or non-directory node.
pub fn apply_drive_extras(
    t: &mut Tree,
    node: NodeId,
    total: u64,
    free: u64,
    reserved: u64,
) -> DriveSpace {
    if !t.is_live(node) || !t.node(node).is_dir() {
        return DriveSpace::default();
    }
    let (free_id, unacc_id) = extras_of(t, node);
    let reserved_id = reserved_of(t, node);
    let virtual_now = unacc_id.map_or(0, |id| t.node(id).alloc())
        + reserved_id.map_or(0, |id| t.node(id).alloc());
    let scanned = t
        .size(node, SizeMode::Allocated)
        .saturating_sub(virtual_now);
    let used = total.saturating_sub(free);
    let reserved = reserved.min(used);
    let allocated = used - reserved;
    let space = DriveSpace {
        total,
        free,
        used,
        reserved,
        scanned,
        unaccounted: allocated.saturating_sub(scanned),
        overcount: scanned.saturating_sub(allocated),
    };
    for (id, name, flag, size) in [
        (
            unacc_id,
            UNACCOUNTED_NAME,
            flags::UNACCOUNTED,
            space.unaccounted,
        ),
        (reserved_id, RESERVED_NAME, flags::UNACCOUNTED, reserved),
        (free_id, FREE_NAME, flags::FREE, free),
    ] {
        match id {
            Some(id) => t.set_virtual_size(id, size),
            None if size > 0 => {
                t.add_virtual(node, name, flag, size);
            }
            None => {}
        }
    }
    space
}

/// The accounting of a drive node as the tree shows it now (`total` and
/// `free` as last applied; `used` derived from them).
pub fn drive_space(t: &Tree, node: NodeId, total: u64) -> DriveSpace {
    if !t.is_live(node) {
        return DriveSpace::default();
    }
    let (free_id, unacc_id) = extras_of(t, node);
    let size_of = |id: Option<NodeId>| id.map_or(0, |id| t.node(id).alloc());
    let free = size_of(free_id);
    let unaccounted = size_of(unacc_id);
    let reserved = size_of(reserved_of(t, node));
    let scanned = t
        .size(node, SizeMode::Allocated)
        .saturating_sub(unaccounted + reserved);
    let used = total.saturating_sub(free);
    DriveSpace {
        total,
        free,
        used,
        reserved,
        scanned,
        unaccounted,
        overcount: scanned.saturating_sub(used.saturating_sub(reserved)),
    }
}

/// The FREE and UNACCOUNTED children of `node`, if present (not the
/// reserved node: see [`reserved_of`]).
pub fn extras_of(t: &Tree, node: NodeId) -> (Option<NodeId>, Option<NodeId>) {
    let (mut free, mut unacc) = (None, None);
    for c in t.children(node) {
        let n = t.node(c);
        if n.has(flags::FREE) {
            free = Some(c);
        } else if n.has(flags::UNACCOUNTED) && t.name(c) != RESERVED_NAME {
            unacc = Some(c);
        }
    }
    (free, unacc)
}

/// The `<reserved by Windows>` child of `node`, if present.
pub fn reserved_of(t: &Tree, node: NodeId) -> Option<NodeId> {
    t.children(node).find(|&c| is_reserved(t, c))
}

/// Whether `d` belongs in "all fixed drives" (see the [module docs](self)).
pub fn is_scannable_fixed(d: &DriveInfo) -> bool {
    let fs_ok = d.fs.eq_ignore_ascii_case("NTFS") || d.fs.eq_ignore_ascii_case("ReFS");
    d.kind == DriveKind::Fixed && d.total > 0 && (d.disk_number.is_some() || fs_ok)
}

/// The fixed drives to scan for "This PC", sorted by letter.
pub fn fixed_drives() -> Vec<DriveInfo> {
    ringscan_win::drives()
        .into_iter()
        .filter(is_scannable_fixed)
        .collect()
}

/// Fresh information about drive `letter` (`None` if there is no such
/// drive, or on non-Windows targets).
pub fn drive_info(letter: char) -> Option<DriveInfo> {
    let letter = letter.to_ascii_uppercase();
    ringscan_win::drives()
        .into_iter()
        .find(|d| d.letter == letter)
}

/// Groups items by physical disk: items on the same disk end up in one
/// group (to be scanned one after another), items with an unknown disk get
/// a group of their own. Groups keep the input order.
pub fn group_by_disk<T>(items: Vec<T>, disk: impl Fn(&T) -> Option<u32>) -> Vec<Vec<T>> {
    let mut groups: Vec<(Option<u32>, Vec<T>)> = Vec::new();
    for item in items {
        let key = disk(&item);
        match groups.iter_mut().find(|(k, _)| key.is_some() && *k == key) {
            Some((_, g)) => g.push(item),
            None => groups.push((key, vec![item])),
        }
    }
    groups.into_iter().map(|(_, g)| g).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(letter: char, fs: &str, kind: DriveKind, total: u64, disk: Option<u32>) -> DriveInfo {
        DriveInfo {
            letter,
            label: String::new(),
            fs: fs.into(),
            kind,
            total,
            free: total / 2,
            reserved: None,
            cluster: 4096,
            disk_number: disk,
        }
    }

    #[test]
    fn extras_are_added_then_updated_in_place() {
        let mut t = Tree::new("C:\\", 0);
        t.add_file(0, "a", 100, 4000, 0, 0);
        let s = apply_drive_extras(&mut t, 0, 10_000, 5_000, 0);
        assert_eq!(
            s,
            DriveSpace {
                total: 10_000,
                free: 5_000,
                used: 5_000,
                reserved: 0,
                scanned: 4_000,
                unaccounted: 1_000,
                overcount: 0
            }
        );
        let (f, u) = extras_of(&t, 0);
        let (f, u) = (f.unwrap(), u.unwrap());
        assert_eq!(t.name(f), FREE_NAME);
        assert_eq!(t.name(u), UNACCOUNTED_NAME);
        // Drive size = used space; free is not aggregated.
        assert_eq!(t.size(0, SizeMode::Allocated), 5_000);
        assert_eq!(t.files(0), 1, "virtual nodes are not files");
        assert_eq!(drive_space(&t, 0, 10_000), s);
        // More data found, less free space: same nodes, new sizes.
        t.add_file(0, "b", 100, 2000, 0, 0);
        let s = apply_drive_extras(&mut t, 0, 10_000, 4_000, 0);
        assert_eq!((s.scanned, s.unaccounted), (6_000, 0));
        assert_eq!(extras_of(&t, 0), (Some(f), Some(u)));
        assert_eq!(t.node(u).alloc(), 0);
        assert_eq!(t.node(f).alloc(), 4_000);
        assert_eq!(t.size(0, SizeMode::Allocated), 6_000);
        // Over-count: scanned exceeds used.
        t.add_file(0, "c", 100, 3000, 0, 0);
        let s = apply_drive_extras(&mut t, 0, 10_000, 4_000, 0);
        assert_eq!((s.unaccounted, s.overcount), (0, 3_000));
    }

    #[test]
    fn reserved_space_is_its_own_node() {
        // 10 000 total, 4 000 free: 6 000 used, of which 1 500 reserved.
        let mut t = Tree::new("C:\\", 0);
        t.add_file(0, "a", 100, 4000, 0, 0);
        let s = apply_drive_extras(&mut t, 0, 10_000, 4_000, 1_500);
        assert_eq!(
            (s.used, s.reserved, s.allocated(), s.scanned, s.unaccounted),
            (6_000, 1_500, 4_500, 4_000, 500)
        );
        let r = reserved_of(&t, 0).expect("reserved node");
        assert!(is_reserved(&t, r));
        assert_eq!(t.node(r).alloc(), 1_500);
        let (_, u) = extras_of(&t, 0);
        let u = u.expect("unaccounted node");
        assert_ne!(u, r);
        assert!(!is_reserved(&t, u));
        assert_eq!(t.node(u).alloc(), 500);
        // The drive still spans the used space; the extras are no files.
        assert_eq!(t.size(0, SizeMode::Allocated), 6_000);
        assert_eq!(t.files(0), 1);
        assert_eq!(drive_space(&t, 0, 10_000), s);
        // Re-applied with a smaller reserve: same nodes, scanned unchanged.
        let s2 = apply_drive_extras(&mut t, 0, 10_000, 4_000, 1_000);
        assert_eq!(
            (s2.scanned, s2.unaccounted, s2.reserved),
            (4_000, 1_000, 1_000)
        );
        assert_eq!(reserved_of(&t, 0), Some(r));
        assert_eq!(extras_of(&t, 0).1, Some(u));
        // A reserve beyond the used space is clamped; the scan over-counts.
        let s3 = apply_drive_extras(&mut t, 0, 10_000, 4_000, 9_000);
        assert_eq!(
            (s3.reserved, s3.unaccounted, s3.overcount),
            (6_000, 0, 4_000)
        );
        assert_eq!(
            s3.scanned + s3.unaccounted + s3.reserved + s3.free,
            s3.total + s3.overcount
        );
    }

    #[test]
    fn extras_skip_zero_sizes_and_dead_nodes() {
        let mut t = Tree::new("D:\\", 0);
        t.add_file(0, "a", 100, 4000, 0, 0);
        let s = apply_drive_extras(&mut t, 0, 4_000, 0, 0);
        assert_eq!((s.unaccounted, s.free), (0, 0));
        assert_eq!(extras_of(&t, 0), (None, None));
        assert_eq!(reserved_of(&t, 0), None);
        let d = t.add_dir(0, "x", 0, 0);
        t.remove(d);
        assert_eq!(
            apply_drive_extras(&mut t, d, 1, 1, 0),
            DriveSpace::default()
        );
    }

    #[test]
    fn fixed_drive_filter() {
        assert!(is_scannable_fixed(&drive(
            'C',
            "NTFS",
            DriveKind::Fixed,
            1,
            Some(0)
        )));
        assert!(is_scannable_fixed(&drive(
            'E',
            "exFAT",
            DriveKind::Fixed,
            1,
            Some(2)
        )));
        assert!(is_scannable_fixed(&drive(
            'F',
            "NTFS",
            DriveKind::Fixed,
            1,
            None
        )));
        // Google Drive style virtual FAT32 drive.
        assert!(!is_scannable_fixed(&drive(
            'G',
            "FAT32",
            DriveKind::Fixed,
            1,
            None
        )));
        assert!(!is_scannable_fixed(&drive(
            'H',
            "NTFS",
            DriveKind::Fixed,
            0,
            Some(1)
        )));
        assert!(!is_scannable_fixed(&drive(
            'U',
            "NTFS",
            DriveKind::Removable,
            1,
            Some(3)
        )));
        assert!(!is_scannable_fixed(&drive(
            'Z',
            "NTFS",
            DriveKind::Network,
            1,
            None
        )));
    }

    #[test]
    fn grouping_by_disk() {
        let items = vec![
            ('C', Some(0)),
            ('D', Some(1)),
            ('E', Some(0)),
            ('F', None),
            ('G', None),
        ];
        let groups = group_by_disk(items, |i| i.1);
        let letters: Vec<String> = groups
            .iter()
            .map(|g| g.iter().map(|i| i.0).collect())
            .collect();
        assert_eq!(letters, ["CE", "D", "F", "G"]);
    }
}
