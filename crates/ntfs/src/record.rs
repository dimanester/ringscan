//! FILE record decoding: update-sequence fixups, the record header, the
//! attribute walk and the per-record size/name extraction.
//!
//! The hot path is [`parse_into`], which fills a reusable [`Scratch`] so the
//! only allocations per record are the decoded names. [`parse_record`] is the
//! public wrapper (inspection, fuzzing).
//!
//! Size rules (SPEC "ringscan-ntfs" Rules):
//! * `$DATA`, unnamed and named (ADS) streams alike: resident -> logical +=
//!   value length, alloc += 0; non-resident -> only the extent with
//!   `lowest_vcn == 0` counts: logical += data_size, alloc += compressed_size
//!   if the attribute is compressed or sparse, else allocated_size.
//! * `$INDEX_ALLOCATION` (any name: `$I30` of directories, `$SDH`/`$SII`/`$O`
//!   /`$Q`/`$R` of view indexes): alloc += allocated_size of the first extent.
//! * Two exceptions found on real volumes: `$BadClus:$Bad` (record 8) is a
//!   volume-sized stream that is all holes but not flagged sparse, so it
//!   counts only its non-sparse (bad) clusters; and the `WofCompressedData`
//!   stream of WOF/CompactOS-compressed files counts towards alloc only (its
//!   logical size is the compressed size, already represented by the
//!   unnamed stream's logical size).

use crate::le::{self, filetime_to_unix, utf16le_eq_ascii, utf16le_to_string};
use crate::runlist;

pub(crate) const AT_STANDARD_INFORMATION: u32 = 0x10;
pub(crate) const AT_ATTRIBUTE_LIST: u32 = 0x20;
pub(crate) const AT_FILE_NAME: u32 = 0x30;
pub(crate) const AT_DATA: u32 = 0x80;
pub(crate) const AT_INDEX_ALLOCATION: u32 = 0xA0;
pub(crate) const AT_REPARSE_POINT: u32 = 0xC0;
const AT_EA_INFORMATION: u32 = 0xD0;
const AT_EA: u32 = 0xE0;
const AT_END: u32 = 0xFFFF_FFFF;

/// Record header flags.
pub(crate) const REC_IN_USE: u16 = 0x0001;
pub(crate) const REC_DIRECTORY: u16 = 0x0002;

/// Attribute header flags.
const ATTR_COMPRESSION_MASK: u16 = 0x00FF;
const ATTR_ENCRYPTED: u16 = 0x4000;
const ATTR_SPARSE: u16 = 0x8000;

/// `$FILE_NAME` namespaces.
pub(crate) const NS_POSIX: u8 = 0;
pub(crate) const NS_WIN32: u8 = 1;
pub(crate) const NS_DOS: u8 = 2;
pub(crate) const NS_WIN32_DOS: u8 = 3;

/// `FILE_ATTRIBUTE_*` bits this crate sets itself.
pub(crate) const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub(crate) const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
pub(crate) const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
pub(crate) const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
pub(crate) const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x4000;
/// On-disk `FILE_ATTRIBUTE_EA`: the file has extended attributes (on a
/// real system drive: every catalog-signed file in `WinSxS`, `System32`,
/// `WindowsApps`, ... carries a `$CI.CATALOGHINT` EA). `winnt.h` gives the
/// in-memory `FILE_ATTRIBUTE_RECALL_ON_OPEN` (a cloud placeholder bit that
/// only directory enumeration reports) the same value, and Windows APIs never
/// report the EA meaning, so the bit is moved to
/// [`mft_flags::EXTENDED_ATTRIBUTES`](crate::mft_flags::EXTENDED_ATTRIBUTES)
/// instead of being passed on as an attribute.
pub(crate) const FILE_ATTRIBUTE_EA: u32 = 0x0004_0000;
/// Bits of `$FILE_NAME`/`$STANDARD_INFORMATION` attributes that are not
/// `FILE_ATTRIBUTE_*` values as Windows APIs report them: the NTFS-internal
/// index-present bits and the on-disk EA bit.
const NTFS_INTERNAL_ATTRS: u32 = 0x1000_0000 | 0x2000_0000 | FILE_ATTRIBUTE_EA;

/// Low 48 bits of a file reference = record number.
pub(crate) const MREF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Outcome of decoding one record slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordStatus {
    /// In use: a base record or an extension record (`base_ref != 0`).
    InUse,
    /// Valid header but not in use, or a never-initialised (zeroed) slot.
    Unused,
    /// No "FILE" signature (garbage or "BAAD", an incomplete multi-sector write).
    BadSignature,
    /// Update sequence array invalid or a sector's check value mismatched
    /// (torn write): the record is skipped.
    BadFixup,
    /// The header's own record number disagrees with the slot it was read
    /// from (misplaced data): the record is skipped.
    WrongRecordNumber,
}

/// A `$FILE_NAME` as stored in a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileName {
    /// Parent directory reference (record number in the low 48 bits,
    /// sequence number in the high 16).
    pub parent_ref: u64,
    /// 0 POSIX, 1 Win32, 2 DOS, 3 Win32 & DOS.
    pub namespace: u8,
    pub name: String,
}

/// Everything [`parse_into`] extracts from one record.
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    pub seq: u16,
    pub flags: u16,
    pub base_ref: u64,
    pub link_count: u16,
    pub mtime: i64,
    /// `$STANDARD_INFORMATION` file attributes (internal bits cleared).
    pub si_attrs: u32,
    pub has_si: bool,
    /// Non-DOS names in record order (DOS-only names are dropped).
    pub names: Vec<(u64, u8, String)>,
    pub dos_names: u16,
    pub logical: u64,
    pub alloc: u64,
    /// `FILE_ATTRIBUTE_*` bits derived from attributes: sparse/compressed/
    /// encrypted unnamed `$DATA`, reparse point.
    pub derived_attrs: u32,
    pub reparse_tag: u32,
    /// Reparse tag found in a `$FILE_NAME` (fallback when `$REPARSE_POINT`
    /// is non-resident).
    pub fn_reparse_tag: u32,
    /// `$REPARSE_POINT` is non-resident (its tag is not in the record).
    pub reparse_nonresident: bool,
    pub has_attr_list: bool,
    /// Extended attributes: the on-disk EA bit in `$STANDARD_INFORMATION`,
    /// or an `$EA_INFORMATION` / `$EA` attribute.
    pub has_ea: bool,
    pub resident_data: bool,
    pub named_streams: u16,
    /// An attribute was structurally invalid; the walk stopped there.
    pub corrupt: bool,
}

impl Scratch {
    fn reset(&mut self) {
        let mut names = std::mem::take(&mut self.names);
        names.clear();
        *self = Scratch {
            names,
            ..Scratch::default()
        };
    }

    /// Final `FILE_ATTRIBUTE_*` value as Windows APIs would report it.
    pub(crate) fn attrs(&self) -> u32 {
        let mut a = self.si_attrs | self.derived_attrs;
        if self.flags & REC_DIRECTORY != 0 {
            a |= FILE_ATTRIBUTE_DIRECTORY;
        }
        a
    }
}

/// Per-volume parameters the record parser needs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ctx {
    pub cluster: u64,
}

/// Applies the update-sequence fixups in place. The stride (bytes per
/// protected block) is derived from the array size, so both the usual
/// 512-byte stride and a sector-sized one are accepted.
pub(crate) fn apply_fixups(rec: &mut [u8]) -> Result<(), ()> {
    let size = rec.len();
    let usa_ofs = le::u16_at(rec, 4).ok_or(())? as usize;
    let usa_count = le::u16_at(rec, 6).ok_or(())? as usize;
    if usa_count < 2 || !usa_ofs.is_multiple_of(2) {
        return Err(());
    }
    let blocks = usa_count - 1;
    if !size.is_multiple_of(blocks) {
        return Err(());
    }
    let stride = size / blocks;
    if !stride.is_power_of_two() || stride < 512 {
        return Err(());
    }
    // The array must sit inside the first block, before its check value.
    if usa_ofs + 2 * usa_count > stride - 2 || usa_ofs < 0x18 {
        return Err(());
    }
    let usn = [rec[usa_ofs], rec[usa_ofs + 1]];
    for i in 1..=blocks {
        let p = i * stride - 2;
        if rec[p..p + 2] != usn {
            return Err(());
        }
    }
    for i in 1..=blocks {
        let p = i * stride - 2;
        let s = usa_ofs + 2 * i;
        rec[p] = rec[s];
        rec[p + 1] = rec[s + 1];
    }
    Ok(())
}

/// A decoded attribute header.
pub(crate) struct Attr<'a> {
    pub ty: u32,
    pub flags: u16,
    /// UTF-16LE name bytes (empty for unnamed attributes).
    pub name: &'a [u8],
    pub body: Body<'a>,
}

pub(crate) enum Body<'a> {
    Resident(&'a [u8]),
    NonResident(NonResident<'a>),
}

pub(crate) struct NonResident<'a> {
    pub lowest_vcn: u64,
    pub mapping_pairs: &'a [u8],
    pub allocated: u64,
    pub data: u64,
    pub initialized: u64,
    pub compressed: Option<u64>,
}

/// Walks the attributes of a fixed-up record. Yields `Err(())` once (and
/// then stops) when an attribute header is invalid.
pub(crate) struct AttrIter<'a> {
    rec: &'a [u8],
    pos: usize,
    end: usize,
    done: bool,
}

impl<'a> AttrIter<'a> {
    /// `rec` must already have its fixups applied.
    pub(crate) fn new(rec: &'a [u8]) -> AttrIter<'a> {
        let first = le::u16_at(rec, 0x14).unwrap_or(0) as usize;
        let used = le::u32_at(rec, 0x18).unwrap_or(0) as usize;
        let end = used.min(rec.len());
        AttrIter {
            rec,
            pos: first,
            end,
            done: first < 0x18,
        }
    }
}

impl<'a> Iterator for AttrIter<'a> {
    type Item = Result<Attr<'a>, ()>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let r = parse_attr(self.rec, self.pos, self.end);
        match r {
            Ok(Some((attr, len))) => {
                self.pos += len;
                Some(Ok(attr))
            }
            Ok(None) => {
                self.done = true;
                None
            }
            Err(()) => {
                self.done = true;
                Some(Err(()))
            }
        }
    }
}

/// Decodes the attribute at `pos`; `Ok(None)` at the end marker.
fn parse_attr(rec: &[u8], pos: usize, end: usize) -> Result<Option<(Attr<'_>, usize)>, ()> {
    let ty = le::u32_at(rec, pos).ok_or(())?;
    if ty == AT_END {
        return Ok(None);
    }
    let len = le::u32_at(rec, pos + 4).ok_or(())? as usize;
    if len < 0x18 || pos.checked_add(len).is_none_or(|e| e > end) {
        return Err(());
    }
    let a = &rec[pos..pos + len];
    let non_resident = a[8] != 0;
    let name_len = a[9] as usize * 2;
    let name_off = le::u16_at(a, 0x0A).ok_or(())? as usize;
    let flags = le::u16_at(a, 0x0C).ok_or(())?;
    let name = if name_len == 0 {
        &[][..]
    } else {
        a.get(name_off..name_off + name_len).ok_or(())?
    };
    let body = if !non_resident {
        let vlen = le::u32_at(a, 0x10).ok_or(())? as usize;
        let voff = le::u16_at(a, 0x14).ok_or(())? as usize;
        Body::Resident(a.get(voff..voff.checked_add(vlen).ok_or(())?).ok_or(())?)
    } else {
        if len < 0x40 {
            return Err(());
        }
        let mp_off = le::u16_at(a, 0x20).ok_or(())? as usize;
        let compressed = if flags & (ATTR_COMPRESSION_MASK | ATTR_SPARSE) != 0 && len >= 0x48 {
            le::u64_at(a, 0x40)
        } else {
            None
        };
        Body::NonResident(NonResident {
            lowest_vcn: le::u64_at(a, 0x10).ok_or(())?,
            mapping_pairs: a.get(mp_off..).ok_or(())?,
            allocated: le::u64_at(a, 0x28).ok_or(())?,
            data: le::u64_at(a, 0x30).ok_or(())?,
            initialized: le::u64_at(a, 0x38).ok_or(())?,
            compressed,
        })
    };
    Ok(Some((
        Attr {
            ty,
            flags,
            name,
            body,
        },
        len,
    )))
}

/// Sizes are signed 64-bit on disk; anything above 2^62 is corruption and
/// would only overflow the sums.
const MAX_SIZE: u64 = 1 << 62;

/// Decodes one record slot (`rec.len()` = record size) in place: applies
/// fixups and extracts names, sizes and attributes into `sc`.
pub(crate) fn parse_into(rec: &mut [u8], recno: u64, ctx: Ctx, sc: &mut Scratch) -> RecordStatus {
    sc.reset();
    match rec.get(0..4) {
        Some(b"FILE") => {}
        Some([0, 0, 0, 0]) => return RecordStatus::Unused,
        _ => return RecordStatus::BadSignature,
    }
    if apply_fixups(rec).is_err() {
        return RecordStatus::BadFixup;
    }
    let rec = &*rec;
    sc.flags = le::u16_at(rec, 0x16).unwrap_or(0);
    if sc.flags & REC_IN_USE == 0 {
        return RecordStatus::Unused;
    }
    // NTFS 3.1 headers (update sequence array at 0x30) store the record's
    // own number at 0x2C: a mismatch means the slot holds misplaced data.
    let usa_ofs = le::u16_at(rec, 4).unwrap_or(0);
    let own = le::u32_at(rec, 0x2C).unwrap_or(0);
    if usa_ofs >= 0x30 && own != 0 && own != recno as u32 {
        return RecordStatus::WrongRecordNumber;
    }
    sc.seq = le::u16_at(rec, 0x10).unwrap_or(0);
    sc.link_count = le::u16_at(rec, 0x12).unwrap_or(0);
    sc.base_ref = le::u64_at(rec, 0x20).unwrap_or(0);
    if sc.base_ref & MREF_MASK == recno {
        sc.base_ref = 0; // self-referencing "extension": treat as a base record
    }

    for attr in AttrIter::new(rec) {
        let Ok(attr) = attr else {
            sc.corrupt = true;
            break;
        };
        match attr.ty {
            AT_STANDARD_INFORMATION => {
                if let (Body::Resident(v), false) = (&attr.body, sc.has_si)
                    && let (Some(mt), Some(fa)) = (le::u64_at(v, 0x08), le::u32_at(v, 0x20))
                {
                    sc.mtime = filetime_to_unix(mt);
                    sc.has_ea |= fa & FILE_ATTRIBUTE_EA != 0;
                    sc.si_attrs = fa & !NTFS_INTERNAL_ATTRS;
                    sc.has_si = true;
                }
            }
            AT_ATTRIBUTE_LIST => sc.has_attr_list = true,
            AT_EA_INFORMATION | AT_EA => sc.has_ea = true,
            AT_FILE_NAME => {
                let Body::Resident(v) = attr.body else {
                    continue;
                };
                let (Some(parent), Some(&n), Some(&ns)) =
                    (le::u64_at(v, 0), v.get(0x40), v.get(0x41))
                else {
                    sc.corrupt = true;
                    continue;
                };
                if le::u32_at(v, 0x38).unwrap_or(0) & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    && sc.fn_reparse_tag == 0
                {
                    sc.fn_reparse_tag = le::u32_at(v, 0x3C).unwrap_or(0);
                }
                let Some(raw) = v.get(0x42..0x42 + 2 * n as usize).filter(|r| !r.is_empty()) else {
                    sc.corrupt = true;
                    continue;
                };
                if ns == NS_DOS {
                    sc.dos_names = sc.dos_names.saturating_add(1);
                } else {
                    sc.names.push((parent, ns, utf16le_to_string(raw)));
                }
            }
            AT_DATA => data_attr(&attr, recno, ctx, sc),
            AT_INDEX_ALLOCATION => {
                if let Body::NonResident(nr) = &attr.body
                    && nr.lowest_vcn == 0
                    && nr.allocated <= MAX_SIZE
                {
                    sc.alloc = sc.alloc.saturating_add(nr.allocated);
                }
            }
            AT_REPARSE_POINT => {
                sc.derived_attrs |= FILE_ATTRIBUTE_REPARSE_POINT;
                match attr.body {
                    Body::Resident(v) => sc.reparse_tag = le::u32_at(v, 0).unwrap_or(0),
                    Body::NonResident(_) => sc.reparse_nonresident = true,
                }
            }
            _ => {}
        }
    }
    if sc.derived_attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 && sc.reparse_tag == 0 {
        // A non-resident value (big buffers such as cloud placeholder
        // blobs) is not read: fall back to the `$FILE_NAME` copy of the
        // tag, else say "unread" rather than 0 (which means "unknown, treat
        // as a link" to consumers).
        sc.reparse_tag = match sc.fn_reparse_tag {
            0 if sc.reparse_nonresident => crate::reparse::UNREAD,
            tag => tag,
        };
    }
    RecordStatus::InUse
}

fn data_attr(attr: &Attr, recno: u64, ctx: Ctx, sc: &mut Scratch) {
    let named = !attr.name.is_empty();
    match &attr.body {
        Body::Resident(v) => {
            if named {
                sc.named_streams = sc.named_streams.saturating_add(1);
            } else {
                sc.resident_data = true;
            }
            sc.logical = sc.logical.saturating_add(v.len() as u64);
        }
        Body::NonResident(nr) => {
            if nr.lowest_vcn != 0 {
                return; // sizes live in the first extent only
            }
            if named {
                sc.named_streams = sc.named_streams.saturating_add(1);
            } else {
                if attr.flags & ATTR_SPARSE != 0 {
                    sc.derived_attrs |= FILE_ATTRIBUTE_SPARSE_FILE;
                }
                if attr.flags & ATTR_COMPRESSION_MASK != 0 {
                    sc.derived_attrs |= FILE_ATTRIBUTE_COMPRESSED;
                }
                if attr.flags & ATTR_ENCRYPTED != 0 {
                    sc.derived_attrs |= FILE_ATTRIBUTE_ENCRYPTED;
                }
            }
            let owner = if sc.base_ref != 0 {
                sc.base_ref & MREF_MASK
            } else {
                recno
            };
            if owner == 8 && utf16le_eq_ascii(attr.name, "$Bad") {
                // Volume-sized, all holes except bad clusters, not flagged
                // sparse: count only the clusters that are really mapped.
                let bad = mapped_clusters(nr.mapping_pairs).saturating_mul(ctx.cluster);
                sc.logical = sc.logical.saturating_add(bad);
                sc.alloc = sc.alloc.saturating_add(bad);
                return;
            }
            let alloc = if attr.flags & (ATTR_COMPRESSION_MASK | ATTR_SPARSE) != 0 {
                nr.compressed.unwrap_or(nr.allocated)
            } else {
                nr.allocated
            };
            if nr.data > MAX_SIZE || alloc > MAX_SIZE {
                sc.corrupt = true;
                return;
            }
            sc.alloc = sc.alloc.saturating_add(alloc);
            if !(named && utf16le_eq_ascii(attr.name, "WofCompressedData")) {
                sc.logical = sc.logical.saturating_add(nr.data);
            }
        }
    }
}

/// Non-sparse clusters of a mapping-pairs array (decoding stops at the
/// first error).
fn mapped_clusters(mp: &[u8]) -> u64 {
    let mut runs = Vec::new();
    let _ = runlist::decode(mp, 0, &mut runs);
    runs.iter()
        .filter(|r| r.lcn.is_some())
        .fold(0u64, |a, r| a.saturating_add(r.len))
}

/// Result of [`parse_record`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub status: RecordStatus,
    /// Sequence number (incremented each time the slot is reused).
    pub seq: u16,
    /// Header flags: 0x1 in use, 0x2 directory, 0x4 in `$Extend`, 0x8 view index.
    pub flags: u16,
    /// Base record reference; 0 for a base record.
    pub base_ref: u64,
    pub link_count: u16,
    /// `$STANDARD_INFORMATION` modification time, unix seconds (0 = unknown).
    pub mtime: i64,
    /// `FILE_ATTRIBUTE_*` as Windows reports them (directory, sparse,
    /// compressed, encrypted and reparse bits derived from the record; the
    /// on-disk EA bit `0x40000` is not passed on, see `has_ea`).
    pub attrs: u32,
    /// The record has extended attributes (on-disk `FILE_ATTRIBUTE_EA` bit,
    /// `$EA_INFORMATION` or `$EA`).
    pub has_ea: bool,
    /// Non-DOS names in record order.
    pub names: Vec<FileName>,
    /// Number of DOS-only (8.3) names skipped.
    pub dos_names: u16,
    /// Size contributions of this record alone (see the module docs).
    pub logical: u64,
    pub alloc: u64,
    pub reparse_tag: u32,
    pub has_attribute_list: bool,
    pub resident_data: bool,
    pub named_streams: u16,
    /// An attribute header was invalid; what follows it was ignored.
    pub corrupt: bool,
}

/// Decodes one MFT record in place (applies fixups to `buf`, whose length
/// must be the volume's record size). `record` is the slot number, used for
/// the header self-check and the `$BadClus` rule; `cluster_size` is needed
/// only for that rule. Never panics, whatever the content.
pub fn parse_record(buf: &mut [u8], record: u64, cluster_size: u32) -> Record {
    let mut sc = Scratch::default();
    let status = parse_into(
        buf,
        record,
        Ctx {
            cluster: cluster_size as u64,
        },
        &mut sc,
    );
    let attrs = sc.attrs();
    Record {
        status,
        seq: sc.seq,
        flags: sc.flags,
        base_ref: sc.base_ref,
        link_count: sc.link_count,
        mtime: sc.mtime,
        attrs,
        has_ea: sc.has_ea,
        names: sc
            .names
            .drain(..)
            .map(|(parent_ref, namespace, name)| FileName {
                parent_ref,
                namespace,
                name,
            })
            .collect(),
        dos_names: sc.dos_names,
        logical: sc.logical,
        alloc: sc.alloc,
        reparse_tag: sc.reparse_tag,
        has_attribute_list: sc.has_attr_list,
        resident_data: sc.resident_data,
        named_streams: sc.named_streams,
        corrupt: sc.corrupt,
    }
}
