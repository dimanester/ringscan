//! Post-pass: merges extension records into their base records, picks the
//! primary hard link, validates parent references and breaks cycles.
//!
//! Guarantees on the output (relied upon by the engine's tree builder):
//! * sorted by record number, one entry per in-use base record that has a
//!   non-DOS name, plus the root (record 5, `parent == 5`) even if its
//!   record is damaged;
//! * for every entry with `orphan == false` (other than the root), `parent`
//!   is the record of a *directory* entry in the output whose current
//!   sequence number matches the reference, and following `parent` links
//!   from any entry ends at the root or at an orphan (no cycles);
//! * every `extra_links` parent satisfies the same validity rule (links to
//!   invalid parents are dropped; if the primary link is invalid but an
//!   extra link is valid, they swap).

use crate::record::{MREF_MASK, NS_POSIX, NS_WIN32, NS_WIN32_DOS, Scratch};
use crate::{MftFile, mft_flags};

/// Per-record info table bits (low 16 bits = sequence number).
pub(crate) const INFO_IN_USE: u32 = 1 << 16;
pub(crate) const INFO_DIR: u32 = 1 << 17;
pub(crate) const INFO_BASE: u32 = 1 << 18;
const INFO_EMITTED: u32 = 1 << 19;

const NONE: u32 = u32::MAX;
pub(crate) const ROOT: u64 = 5;

/// Record counters of one chunk.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Counts {
    pub extension: u64,
    pub unused: u64,
    pub bad_signature: u64,
    pub bad_fixup: u64,
    pub wrong_record_number: u64,
    pub corrupt: u64,
}

impl Counts {
    pub(crate) fn add(&mut self, o: &Counts) {
        self.extension += o.extension;
        self.unused += o.unused;
        self.bad_signature += o.bad_signature;
        self.bad_fixup += o.bad_fixup;
        self.wrong_record_number += o.wrong_record_number;
        self.corrupt += o.corrupt;
    }
}

/// Parse output of one chunk of consecutive records.
#[derive(Debug, Default)]
pub(crate) struct ChunkOut {
    pub index: u64,
    /// Record number of the chunk's first slot.
    pub first_record: u64,
    /// One entry per record slot: sequence number + `INFO_*` bits.
    pub info: Vec<u32>,
    pub drafts: Vec<Draft>,
    pub exts: Vec<ExtPart>,
    pub counts: Counts,
}

/// A base record on its way to becoming an [`MftFile`]. Until
/// [`finalize`], `file.parent` and the `extra_links` parents hold full file
/// references (sequence number in the high 16 bits).
#[derive(Debug)]
pub(crate) struct Draft {
    pub file: MftFile,
    pub primary_ns: u8,
}

fn is_win32(ns: u8) -> bool {
    ns == NS_WIN32 || ns == NS_WIN32_DOS
}

impl Draft {
    pub(crate) fn from_scratch(record: u64, sc: &mut Scratch) -> Draft {
        let mut d = Draft {
            file: MftFile {
                record,
                is_dir: sc.flags & crate::record::REC_DIRECTORY != 0,
                logical: sc.logical,
                alloc: sc.alloc,
                attrs: sc.attrs(),
                mtime: sc.mtime,
                seq: sc.seq,
                reparse_tag: sc.reparse_tag,
                flags: flags_of(
                    sc.resident_data,
                    sc.named_streams,
                    sc.has_attr_list,
                    sc.corrupt,
                ),
                ..MftFile::default()
            },
            primary_ns: NS_POSIX,
        };
        // Primary = first Win32 / Win32&DOS name, else the first name.
        let primary = sc.names.iter().position(|n| is_win32(n.1)).unwrap_or(0);
        for (i, (parent, ns, name)) in sc.names.drain(..).enumerate() {
            if i == primary {
                d.file.parent = parent;
                d.file.name = name;
                d.primary_ns = ns;
            } else {
                d.file.extra_links.push((parent, name));
            }
        }
        d
    }

    fn add_name(&mut self, parent: u64, ns: u8, name: String) {
        if self.file.name.is_empty() {
            self.file.parent = parent;
            self.file.name = name;
            self.primary_ns = ns;
        } else if !is_win32(self.primary_ns) && is_win32(ns) {
            let old_parent = std::mem::replace(&mut self.file.parent, parent);
            let old_name = std::mem::replace(&mut self.file.name, name);
            self.file.extra_links.push((old_parent, old_name));
            self.primary_ns = ns;
        } else {
            self.file.extra_links.push((parent, name));
        }
    }

    fn merge(&mut self, e: ExtPart) {
        let f = &mut self.file;
        f.logical = f.logical.saturating_add(e.logical);
        f.alloc = f.alloc.saturating_add(e.alloc);
        f.attrs |= e.derived_attrs;
        if f.reparse_tag == 0 {
            f.reparse_tag = e.reparse_tag;
        }
        f.flags |= e.flags;
        for (parent, ns, name) in e.names {
            self.add_name(parent, ns, name);
        }
    }
}

fn flags_of(resident: bool, named_streams: u16, attr_list: bool, corrupt: bool) -> u16 {
    let mut f = 0;
    if resident {
        f |= mft_flags::RESIDENT_DATA;
    }
    if named_streams > 0 {
        f |= mft_flags::NAMED_STREAMS;
    }
    if attr_list {
        f |= mft_flags::ATTRIBUTE_LIST;
    }
    if corrupt {
        f |= mft_flags::CORRUPT;
    }
    f
}

/// What an extension record contributes to its base record.
#[derive(Debug)]
pub(crate) struct ExtPart {
    pub base_ref: u64,
    pub names: Vec<(u64, u8, String)>,
    pub logical: u64,
    pub alloc: u64,
    pub derived_attrs: u32,
    pub reparse_tag: u32,
    pub flags: u16,
}

impl ExtPart {
    pub(crate) fn from_scratch(sc: &mut Scratch) -> ExtPart {
        ExtPart {
            base_ref: sc.base_ref,
            names: std::mem::take(&mut sc.names),
            logical: sc.logical,
            alloc: sc.alloc,
            derived_attrs: sc.derived_attrs,
            reparse_tag: sc.reparse_tag,
            flags: flags_of(sc.resident_data, sc.named_streams, false, sc.corrupt),
        }
    }
}

/// Counters produced by [`finalize`].
#[derive(Debug, Default)]
pub(crate) struct FinalCounts {
    pub stale_extensions: u64,
    pub nameless: u64,
    pub orphans: u64,
    pub dropped_links: u64,
    pub cycles_broken: u64,
    pub root_synthesized: bool,
}

/// Turns drafts (sorted by record) into the final file list; see the module
/// docs for the guarantees.
pub(crate) fn finalize(
    mut info: Vec<u32>,
    mut drafts: Vec<Draft>,
    exts: Vec<ExtPart>,
    fc: &mut FinalCounts,
) -> Vec<MftFile> {
    if info.len() <= ROOT as usize {
        info.resize(ROOT as usize + 1, 0);
    }
    let n = info.len();
    let mut index = vec![NONE; n];
    for (i, d) in drafts.iter().enumerate() {
        if let Some(slot) = index.get_mut(d.file.record as usize) {
            *slot = i as u32;
        }
    }

    // 1. Extension records -> base records (sequence-checked).
    for e in exts {
        let b = e.base_ref & MREF_MASK;
        let s = (e.base_ref >> 48) as u16;
        let ok = (b as usize) < n
            && index[b as usize] != NONE
            && (s == 0 || s == info[b as usize] as u16);
        if ok {
            drafts[index[b as usize] as usize].merge(e);
        } else {
            fc.stale_extensions += 1;
        }
    }

    // 2. The root is always present.
    let root_synth = match index[ROOT as usize] {
        NONE => {
            let pos = drafts.partition_point(|d| d.file.record < ROOT);
            let root = MftFile {
                record: ROOT,
                is_dir: true,
                attrs: 0x16,
                ..MftFile::default()
            };
            drafts.insert(
                pos,
                Draft {
                    file: root,
                    primary_ns: NS_WIN32_DOS,
                },
            );
            true
        }
        _ => false,
    };
    fc.root_synthesized = root_synth;
    info[ROOT as usize] |= INFO_IN_USE | INFO_DIR | INFO_BASE;
    {
        let at = drafts.partition_point(|d| d.file.record < ROOT);
        let root = &mut drafts[at].file;
        root.is_dir = true;
        root.attrs |= 0x10;
        if root.name.is_empty() {
            root.name = ".".to_string();
        }
        root.parent = ROOT | ((root.seq as u64) << 48);
    }

    // 3. Nameless base records are not files (reserved slots, extension-only
    // data whose names are gone).
    let before = drafts.len();
    drafts.retain(|d| !d.file.name.is_empty());
    fc.nameless = (before - drafts.len()) as u64;
    index.fill(NONE);
    for (i, d) in drafts.iter().enumerate() {
        let r = d.file.record as usize;
        if let (Some(slot), Some(inf)) = (index.get_mut(r), info.get_mut(r)) {
            *slot = i as u32;
            *inf |= INFO_EMITTED;
        }
    }

    // 4. Parent references: present, a directory, same sequence number.
    let valid = |child: u64, r: u64| -> bool {
        let p = r & MREF_MASK;
        let s = (r >> 48) as u16;
        if p == child {
            return false;
        }
        let Some(&inf) = info.get(p as usize) else {
            return false;
        };
        inf & INFO_EMITTED != 0
            && inf & INFO_DIR != 0
            && (s == 0 || s == inf as u16 || (p == ROOT && root_synth))
    };
    for d in drafts.iter_mut() {
        let f = &mut d.file;
        if f.record == ROOT {
            f.orphan = false;
        } else {
            if !valid(f.record, f.parent)
                && let Some(k) = f.extra_links.iter().position(|l| valid(f.record, l.0))
            {
                let (p, nm) = f.extra_links.swap_remove(k);
                let old_p = std::mem::replace(&mut f.parent, p);
                let old_n = std::mem::replace(&mut f.name, nm);
                f.extra_links.push((old_p, old_n));
            }
            f.orphan = !valid(f.record, f.parent);
            let rec = f.record;
            let before = f.extra_links.len();
            f.extra_links.retain(|l| valid(rec, l.0));
            fc.dropped_links += (before - f.extra_links.len()) as u64;
        }
        f.parent_seq = (f.parent >> 48) as u16;
        f.parent &= MREF_MASK;
        for l in f.extra_links.iter_mut() {
            l.0 &= MREF_MASK;
        }
    }

    // 5. Break parent cycles among directories: walk up from each directory;
    // reaching a node already on the current path means a cycle, which is
    // cut by making that node an orphan.
    let mut state = vec![0u8; drafts.len()]; // 0 new, 1 on path, 2 done
    let mut path = Vec::new();
    for start in 0..drafts.len() {
        if state[start] != 0 || !drafts[start].file.is_dir {
            continue;
        }
        let mut cur = start;
        loop {
            match state[cur] {
                2 => break,
                1 => {
                    drafts[cur].file.orphan = true;
                    fc.cycles_broken += 1;
                    break;
                }
                _ => {}
            }
            state[cur] = 1;
            path.push(cur);
            let f = &drafts[cur].file;
            if f.record == ROOT || f.orphan {
                break;
            }
            match index.get(f.parent as usize) {
                Some(&i) if i != NONE => cur = i as usize,
                _ => {
                    drafts[cur].file.orphan = true;
                    break;
                }
            }
        }
        for i in path.drain(..) {
            state[i] = 2;
        }
    }

    // 6. Metafiles: records 0..=15 except the root, and everything whose
    // parent chain (acyclic now) passes through one of them ($Extend).
    let mut meta = vec![0u8; drafts.len()]; // 0 unknown, 1 meta, 2 regular
    for start in 0..drafts.len() {
        let mut cur = start;
        let verdict = loop {
            if meta[cur] != 0 {
                break meta[cur];
            }
            let f = &drafts[cur].file;
            if f.record < 16 && f.record != ROOT {
                break 1;
            }
            path.push(cur);
            if f.record == ROOT || f.orphan || path.len() > drafts.len() {
                break 2;
            }
            match index.get(f.parent as usize) {
                Some(&i) if i != NONE => cur = i as usize,
                _ => break 2,
            }
        };
        meta[cur] = verdict;
        for i in path.drain(..) {
            meta[i] = verdict;
        }
    }
    for (d, &m) in drafts.iter_mut().zip(&meta) {
        if m == 1 {
            d.file.flags |= mft_flags::METAFILE;
        }
    }

    fc.orphans = drafts.iter().filter(|d| d.file.orphan).count() as u64;
    drafts.into_iter().map(|d| d.file).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mref(rec: u64, seq: u16) -> u64 {
        rec | (seq as u64) << 48
    }

    fn draft(record: u64, seq: u16, parent: u64, name: &str, is_dir: bool) -> Draft {
        Draft {
            file: MftFile {
                record,
                seq,
                parent,
                name: name.into(),
                is_dir,
                ..MftFile::default()
            },
            primary_ns: NS_POSIX,
        }
    }

    fn info_for(drafts: &[Draft], n: usize) -> Vec<u32> {
        let mut info = vec![0u32; n];
        for d in drafts {
            let mut v = d.file.seq as u32 | INFO_IN_USE | INFO_BASE;
            if d.file.is_dir {
                v |= INFO_DIR;
            }
            info[d.file.record as usize] = v;
        }
        info
    }

    fn by_record(files: &[MftFile], r: u64) -> &MftFile {
        files.iter().find(|f| f.record == r).unwrap()
    }

    #[test]
    fn stale_parent_is_orphan_and_extra_link_is_promoted() {
        let drafts = vec![
            draft(5, 5, mref(5, 5), ".", true),
            draft(20, 3, mref(5, 5), "dir", true),
            draft(21, 1, mref(20, 2), "stale-child", false), // dir 20 is now seq 3
            {
                let mut d = draft(22, 1, mref(20, 2), "a", false);
                d.file.extra_links.push((mref(20, 3), "b".into()));
                d.file.extra_links.push((mref(99, 1), "c".into()));
                d
            },
            draft(23, 1, mref(24, 1), "parent-is-file", false),
            draft(24, 1, mref(5, 5), "file", false),
            draft(25, 1, mref(25, 1), "self-parent", true),
            draft(26, 1, mref(20, 0), "seq-zero-ok", false),
        ];
        let info = info_for(&drafts, 30);
        let mut fc = FinalCounts::default();
        let files = finalize(info, drafts, Vec::new(), &mut fc);
        assert!(!by_record(&files, 5).orphan);
        assert_eq!(by_record(&files, 5).parent, 5);
        assert!(!by_record(&files, 20).orphan);
        let f21 = by_record(&files, 21);
        assert!(f21.orphan);
        assert_eq!((f21.parent, f21.parent_seq), (20, 2));
        let f22 = by_record(&files, 22);
        assert!(!f22.orphan);
        assert_eq!((f22.parent, f22.name.as_str()), (20, "b"));
        assert!(
            f22.extra_links.is_empty(),
            "stale and dangling links dropped"
        );
        assert!(by_record(&files, 23).orphan);
        assert!(by_record(&files, 25).orphan);
        assert!(!by_record(&files, 26).orphan);
        assert_eq!(fc.orphans, 3);
        assert_eq!(fc.dropped_links, 2);
    }

    #[test]
    fn cycles_are_broken() {
        let drafts = vec![
            draft(5, 5, mref(5, 5), ".", true),
            draft(30, 1, mref(31, 1), "a", true),
            draft(31, 1, mref(32, 1), "b", true),
            draft(32, 1, mref(30, 1), "c", true),
            draft(33, 1, mref(31, 1), "under-cycle", false),
            draft(34, 1, mref(5, 5), "ok", true),
            draft(35, 1, mref(34, 1), "deep", true),
        ];
        let info = info_for(&drafts, 40);
        let mut fc = FinalCounts::default();
        let files = finalize(info, drafts, Vec::new(), &mut fc);
        assert_eq!(fc.cycles_broken, 1);
        assert_eq!(files.iter().filter(|f| f.orphan).count(), 1);
        // Every chain now ends at the root or at an orphan.
        for f in &files {
            let mut cur = f;
            for _ in 0..files.len() + 1 {
                if cur.record == 5 || cur.orphan {
                    break;
                }
                cur = by_record(&files, cur.parent);
            }
            assert!(cur.record == 5 || cur.orphan, "record {} loops", f.record);
        }
    }

    #[test]
    fn extensions_merge_with_sequence_check_and_root_is_synthesized() {
        let drafts = vec![
            draft(40, 7, mref(5, 5), "", false),
            draft(41, 2, mref(5, 5), "x", false),
        ];
        let info = info_for(&drafts, 50);
        let exts = vec![
            ExtPart {
                base_ref: mref(40, 7),
                names: vec![
                    (mref(5, 5), NS_POSIX, "posix".into()),
                    (mref(5, 5), NS_WIN32, "win".into()),
                ],
                logical: 10,
                alloc: 4096,
                derived_attrs: 0x200,
                reparse_tag: 0,
                flags: mft_flags::NAMED_STREAMS,
            },
            ExtPart {
                base_ref: mref(41, 1), // stale: base is seq 2 now
                names: vec![],
                logical: 99,
                alloc: 99,
                derived_attrs: 0,
                reparse_tag: 0,
                flags: 0,
            },
        ];
        let mut fc = FinalCounts::default();
        let files = finalize(info, drafts, exts, &mut fc);
        assert!(fc.root_synthesized);
        assert_eq!(fc.stale_extensions, 1);
        assert_eq!(files[0].record, 5);
        assert_eq!(files[0].name, ".");
        let f40 = by_record(&files, 40);
        assert_eq!(f40.name, "win", "Win32 name preferred over POSIX");
        assert_eq!(f40.extra_links, vec![(5, "posix".to_string())]);
        assert_eq!((f40.logical, f40.alloc, f40.attrs), (10, 4096, 0x200));
        assert!(!f40.orphan, "refs to a synthesized root are accepted");
        assert_eq!(by_record(&files, 41).logical, 0);
    }
}
