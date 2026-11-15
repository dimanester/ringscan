//! Drives: which ones "all fixed drives" means, and the virtual FREE and
//! UNACCOUNTED nodes at a scanned drive's root.
//!
//! # Drive root extras
//!
//! After a whole drive has been scanned, [`apply_drive_extras`] gives its
//! node two virtual children:
//!
//! * `<free space>` (FREE): the free bytes Windows reports. FREE nodes are
//!   never part of an ancestor's size; the layout adds them back when free
//!   space is shown, so the drive then spans its whole capacity.
//! * `<unaccounted>` (UNACCOUNTED): `used − scanned`, where `used = total −
//!   free` and `scanned` is the allocated size the scan found below the
//!   drive. It is the space the scan could not see: folders it was denied
//!   (`System Volume Information`, other users' profiles), NTFS metadata in
//!   walk mode (`$MFT`, `$LogFile`, directory indexes), alternate data
//!   streams, and in MFT mode the root directory's own index. With it the
//!   drive's size equals the used space Windows reports.
//!
//!   When the scan finds *more* than Windows reports as used (possible when
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

/// Space accounting of one scanned drive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DriveSpace {
    /// Capacity visible to the user.
    pub total: u64,
    pub free: u64,
    /// `total − free`.
    pub used: u64,
    /// Allocated bytes found by the scan below the drive node.
    pub scanned: u64,
    /// `max(0, used − scanned)`: size of the UNACCOUNTED node.
    pub unaccounted: u64,
    /// `max(0, scanned − used)`: what the scan found beyond the used space.
    pub overcount: u64,
}

/// Adds or updates the FREE and UNACCOUNTED children of drive node `node`
/// (see the [module docs](self)) and returns the resulting accounting.
/// Nodes are only created when their size is non-zero; existing ones are
/// updated (possibly to 0). No-op on a dead or non-directory node.
pub fn apply_drive_extras(t: &mut Tree, node: NodeId, total: u64, free: u64) -> DriveSpace {
    if !t.is_live(node) || !t.node(node).is_dir() {
        return DriveSpace::default();
    }
    let (free_id, unacc_id) = extras_of(t, node);
    let unacc_now = unacc_id.map_or(0, |id| t.node(id).alloc());
    let scanned = t.size(node, SizeMode::Allocated).saturating_sub(unacc_now);
    let used = total.saturating_sub(free);
    let space = DriveSpace {
        total,
        free,
        used,
        scanned,
        unaccounted: used.saturating_sub(scanned),
        overcount: scanned.saturating_sub(used),
    };
    match unacc_id {
        Some(id) => t.set_virtual_size(id, space.unaccounted),
        None if space.unaccounted > 0 => {
            t.add_virtual(
                node,
                UNACCOUNTED_NAME,
                flags::UNACCOUNTED,
                space.unaccounted,
            );
        }
        None => {}
    }
    match free_id {
        Some(id) => t.set_virtual_size(id, free),
        None if free > 0 => {
            t.add_virtual(node, FREE_NAME, flags::FREE, free);
        }
        None => {}
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
    let free = free_id.map_or(0, |id| t.node(id).alloc());
    let unaccounted = unacc_id.map_or(0, |id| t.node(id).alloc());
    let scanned = t
        .size(node, SizeMode::Allocated)
        .saturating_sub(unaccounted);
    let used = total.saturating_sub(free);
    DriveSpace {
        total,
        free,
        used,
        scanned,
        unaccounted,
        overcount: scanned.saturating_sub(used),
    }
}

/// The FREE and UNACCOUNTED children of `node`, if present.
pub fn extras_of(t: &Tree, node: NodeId) -> (Option<NodeId>, Option<NodeId>) {
    let (mut free, mut unacc) = (None, None);
    for c in t.children(node) {
        let n = t.node(c);
        if n.has(flags::FREE) {
            free = Some(c);
        } else if n.has(flags::UNACCOUNTED) {
            unacc = Some(c);
        }
    }
    (free, unacc)
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
            cluster: 4096,
            disk_number: disk,
        }
    }

    #[test]
    fn extras_are_added_then_updated_in_place() {
        let mut t = Tree::new("C:\\", 0);
        t.add_file(0, "a", 100, 4000, 0, 0);
        let s = apply_drive_extras(&mut t, 0, 10_000, 5_000);
        assert_eq!(
            s,
            DriveSpace {
                total: 10_000,
                free: 5_000,
                used: 5_000,
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
        let s = apply_drive_extras(&mut t, 0, 10_000, 4_000);
        assert_eq!((s.scanned, s.unaccounted), (6_000, 0));
        assert_eq!(extras_of(&t, 0), (Some(f), Some(u)));
        assert_eq!(t.node(u).alloc(), 0);
        assert_eq!(t.node(f).alloc(), 4_000);
        assert_eq!(t.size(0, SizeMode::Allocated), 6_000);
        // Over-count: scanned exceeds used.
        t.add_file(0, "c", 100, 3000, 0, 0);
        let s = apply_drive_extras(&mut t, 0, 10_000, 4_000);
        assert_eq!((s.unaccounted, s.overcount), (0, 3_000));
    }

    #[test]
    fn extras_skip_zero_sizes_and_dead_nodes() {
        let mut t = Tree::new("D:\\", 0);
        t.add_file(0, "a", 100, 4000, 0, 0);
        let s = apply_drive_extras(&mut t, 0, 4_000, 0);
        assert_eq!((s.unaccounted, s.free), (0, 0));
        assert_eq!(extras_of(&t, 0), (None, None));
        let d = t.add_dir(0, "x", 0, 0);
        t.remove(d);
        assert_eq!(apply_drive_extras(&mut t, d, 1, 1), DriveSpace::default());
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
