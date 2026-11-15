//! ringscan-ntfs: a fast, platform-independent NTFS MFT reader.
//!
//! Reads the Master File Table of an NTFS volume through the [`ReadAt`]
//! trait (an image file in tests, a raw `\\.\C:` handle in the engine) and
//! returns one [`MftFile`] per file or directory: name, parent, sizes,
//! attributes, mtime and hard links. It never follows the directory indexes;
//! the tree is rebuilt by the consumer from parent references.
//!
//! # Pipeline
//!
//! 1. [`read_volume_info`]: the boot sector (first 4096 bytes).
//! 2. Bootstrap (`mft.rs`): record 0 (`$MFT`, `$MFTMirr` as fallback), its
//!    `$DATA` runlist, and — for large fragmented MFTs — the `$DATA` extents
//!    listed in its `$ATTRIBUTE_LIST`, loaded from extension records in VCN
//!    order.
//! 3. Streaming: the caller's thread reads the MFT sequentially in 4 MiB
//!    chunks ([`MftOptions::chunk_size`]) following the runlist (optionally
//!    with more reader threads, [`MftOptions::readers`]); N worker threads
//!    parse chunks as they arrive (std threads + channels, no dependencies).
//!    Records are independent, so parsing scales with cores.
//! 4. Merge (`assemble.rs`, single-threaded, O(n)): extension records are
//!    folded into their base record, hard links resolved, parent references
//!    validated (sequence numbers) and parent cycles broken.
//!
//! # Memory
//!
//! The file list is the biggest allocation: ~105 bytes per [`MftFile`] plus
//! its name (~225 MB for a 1.4 M-entry system drive). The bootstrap also
//! reads `$MFT:$BITMAP` and counts the in-use records
//! ([`MftStats::bitmap_in_use`]); with that upper bound the list is
//! allocated once and each parsed chunk is appended (in record order) and
//! freed as soon as the chunks before it are in, and the merge works on the
//! list in place. The peak is then the list plus `(readers + workers / 2 +
//! 1)` chunk buffers. Without a readable bitmap the chunks are kept until
//! the end and concatenated, which holds the list twice at the peak (a
//! synthetic 1.4 M-entry volume: 255 MB vs 430 MB private).
//!
//! # I/O contract
//!
//! Every read issued by this crate has a 4096-aligned offset, a 4096-aligned
//! buffer address and a length that is a multiple of 4096 — except a read
//! that ends exactly at the volume end (`total_sectors * bytes_per_sector`)
//! when that is not 4096-aligned, whose length is then a multiple of
//! `bytes_per_sector`. Nothing at or beyond the volume end is read, and no
//! single read exceeds [`MAX_READ`] (16 MiB). Unaligned spans (possible with
//! clusters < 4096) are widened and copied from a bounce buffer. So the
//! engine's `ReadAt` over a raw volume handle can be a plain positional
//! `ReadFile`, even with `FILE_FLAG_NO_BUFFERING`. Order: 4096 bytes at 0
//! (boot sector), the 4096-aligned span around record 0 (plus `$MFTMirr` if
//! that fails, plus `$MFT:$BITMAP`, the attribute list and `$MFT` extension
//! records if any),
//! all from the calling thread; then the MFT sequentially in chunk order,
//! by default from the calling thread too. With [`MftOptions::readers`] > 1
//! several threads read chunks at once (`ReadAt` is `Sync` for this).
//!
//! A failed chunk read does not abort the scan: the span is retried in
//! 64 KiB blocks and unreadable blocks are zero-filled (their records are
//! counted in [`MftStats::unreadable_records`]). If nothing at all could be
//! read, the first error is returned.
//!
//! # Size rules
//!
//! See `record.rs`: `$DATA` streams (unnamed and named) count resident
//! length / non-resident `data_size` as logical and `allocated_size` (or
//! `compressed_size` when compressed or sparse) as alloc, taken only from
//! the extent with `lowest_vcn == 0`; resident data has alloc 0;
//! `$INDEX_ALLOCATION` adds to alloc (not logical). Directories therefore
//! have logical 0 unless they carry named streams. `$BadClus:$Bad` counts
//! only its bad clusters, and `WofCompressedData` streams count as alloc only.
//!
//! # Names, links, orphans
//!
//! DOS-only (8.3) names are skipped. Each remaining `$FILE_NAME` is a hard
//! link; the first Win32 (or Win32&DOS) name is the primary one (else the
//! first POSIX name), all others go to [`MftFile::extra_links`]. A parent
//! reference is valid if it names a directory that is in the output and
//! whose current sequence number matches the one in the reference (0 = not
//! checked). Invalid primary links are swapped with a valid extra link if
//! possible; otherwise the file is flagged [`MftFile::orphan`]. Invalid extra
//! links are dropped. See `assemble.rs` for the exact output guarantees.
//!
//! # Robustness
//!
//! All on-disk values go through bounds-checked readers; arithmetic on them
//! is checked or saturating; every loop advances by at least one byte /
//! record / VCN per step. Nothing panics on corrupted or hostile input (the
//! release profile uses `panic = "abort"`), which is exercised by a fuzz
//! test that corrupts real records 1 000 000 times and whole images
//! thousands of times. Records failing the update-sequence check are skipped and counted.
//!
//! # Deviations from SPEC.md
//!
//! * [`MftFile`] has extra fields: `seq`, `parent_seq`, `orphan`,
//!   `reparse_tag`, `flags` ([`mft_flags`]). `extra_links` keeps the spec
//!   type `(parent record, name)`.
//! * [`VolumeInfo`] also has `mft_mirr_lcn` and `total_sectors`.
//! * [`MftProgress`] counts MFT records (`total` is set after the
//!   bootstrap); it implements `Default`.
//! * [`scan_mft`] is the full-control entry point (threads, chunk size,
//!   statistics); [`read_mft`] is the spec signature on top of it.
//! * Additions: [`parent_first_order`] (insertion order for tree builders),
//!   [`parse_record`] (decode one record), [`reparse`] tag helpers,
//!   [`filetime_to_unix`], `ReadAt` for `[u8]`/`Vec<u8>`/`&T`/`Box`/`Arc`.
//! * Records 12..=15 and other in-use base records without a non-DOS name are
//!   not returned (they are reserved slots or `$MFT` extension records).

mod assemble;
mod boot;
mod io;
mod le;
mod mft;
mod record;
mod runlist;

pub use boot::VolumeInfo;
pub use io::{IO_ALIGN, MAX_READ, ReadAt};
pub use le::filetime_to_unix;
pub use record::{FileName, Record, RecordStatus, parse_record};

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Record number of the root directory.
pub const ROOT_RECORD: u64 = 5;

/// Bits of [`MftFile::flags`].
pub mod mft_flags {
    /// The unnamed `$DATA` stream is resident (stored in the record, alloc 0).
    pub const RESIDENT_DATA: u16 = 1 << 0;
    /// The file has named `$DATA` streams (ADS); their sizes are included.
    pub const NAMED_STREAMS: u16 = 1 << 1;
    /// The record has an `$ATTRIBUTE_LIST` (attributes spread over
    /// extension records, e.g. a heavily fragmented file).
    pub const ATTRIBUTE_LIST: u16 = 1 << 2;
    /// Some attribute of the file could not be decoded; names or sizes may be
    /// incomplete.
    pub const CORRUPT: u16 = 1 << 3;
    /// NTFS metadata: records 0..=15 other than the root (`$MFT`, `$LogFile`,
    /// `$Bitmap`, ...) and everything below `$Extend` (`$UsnJrnl`, `$Quota`,
    /// `$RmMetadata`, ...).
    pub const METAFILE: u16 = 1 << 4;
    /// The file has extended attributes (EAs, e.g. the `$CI.CATALOGHINT` of
    /// catalog-signed system files). On disk this is attribute bit
    /// `0x40000`, which Windows APIs report as `FILE_ATTRIBUTE_RECALL_ON_OPEN`
    /// (a cloud placeholder bit) in memory; [`MftFile::attrs`](crate::MftFile::attrs)
    /// never carries it.
    pub const EXTENDED_ATTRIBUTES: u16 = 1 << 5;
}

/// Reparse tags and helpers for [`MftFile::reparse_tag`].
pub mod reparse {
    /// Junction / volume mount point (`IO_REPARSE_TAG_MOUNT_POINT`).
    pub const MOUNT_POINT: u32 = 0xA000_0003;
    /// Symbolic link (`IO_REPARSE_TAG_SYMLINK`).
    pub const SYMLINK: u32 = 0xA000_000C;
    /// Deduplicated file (`IO_REPARSE_TAG_DEDUP`): data lives in the chunk store.
    pub const DEDUP: u32 = 0x8000_0013;
    /// WOF-compressed (CompactOS) file (`IO_REPARSE_TAG_WOF`).
    pub const WOF: u32 = 0x8000_0017;
    /// App execution alias (`IO_REPARSE_TAG_APPEXECLINK`).
    pub const APPEXECLINK: u32 = 0x8000_001B;
    /// Plain cloud placeholder tag (`IO_REPARSE_TAG_CLOUD`); variants
    /// `0x9000_101A..=0x9000_F01A` differ in bits 12..16.
    pub const CLOUD: u32 = 0x9000_001A;

    /// Not a real tag: the file has a `$REPARSE_POINT` attribute whose value
    /// is non-resident (not stored in the MFT record; typical for large
    /// buffers such as cloud placeholder blobs) and no `$FILE_NAME` copy of
    /// the tag, so the tag is unknown. Bit 30 is reserved in real tags, and
    /// the value is neither a name surrogate nor a cloud tag. Name
    /// surrogates (junctions, symlinks) have small, resident buffers, so a
    /// consumer may treat such a file as regular; a directory with
    /// children in the MFT is certainly not a junction.
    pub const UNREAD: u32 = 0x4000_0000;

    /// OneDrive / cloud-files placeholder (any `IO_REPARSE_TAG_CLOUD_*`).
    pub fn is_cloud(tag: u32) -> bool {
        tag & 0xFFFF_0FFF == CLOUD
    }

    /// Name surrogates (junctions, symlinks, ...) point elsewhere in the
    /// namespace: list them, never descend into or count their target.
    pub fn is_name_surrogate(tag: u32) -> bool {
        tag & 0x2000_0000 != 0
    }
}

/// One file or directory (one in-use base MFT record).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MftFile {
    /// MFT record number (the NTFS file id without sequence number).
    pub record: u64,
    /// Record number of the parent directory (5 for the root itself). Only
    /// meaningful as a tree link when `orphan` is false.
    pub parent: u64,
    /// Primary name (Win32 preferred; never a DOS 8.3 name). The root is ".".
    pub name: String,
    pub is_dir: bool,
    /// Logical bytes: all `$DATA` streams (0 for most directories).
    pub logical: u64,
    /// Allocated bytes on disk: non-resident streams + directory index.
    pub alloc: u64,
    /// `FILE_ATTRIBUTE_*` as Windows reports them, including the derived
    /// DIRECTORY, SPARSE_FILE, COMPRESSED, ENCRYPTED and REPARSE_POINT bits.
    /// The on-disk EA bit (`0x40000`, the value of the in-memory
    /// `FILE_ATTRIBUTE_RECALL_ON_OPEN`) is never set here; see
    /// [`mft_flags::EXTENDED_ATTRIBUTES`].
    pub attrs: u32,
    /// Last modification time (unix seconds, 0 = unknown).
    pub mtime: i64,
    /// Other hard links: (parent record, name). All parents are valid.
    pub extra_links: Vec<(u64, String)>,
    /// This record's sequence number.
    pub seq: u16,
    /// Sequence number stored in the primary name's parent reference.
    pub parent_seq: u16,
    /// The primary parent reference is invalid: the parent record is gone,
    /// was reused (sequence mismatch), is not a directory, or the file is
    /// part of a parent cycle. Consumers attach it under an "orphaned" node.
    pub orphan: bool,
    /// Reparse tag (0 = not a reparse point or unknown, [`reparse::UNREAD`]
    /// = stored outside the record and not read); see [`reparse`].
    pub reparse_tag: u32,
    /// [`mft_flags`] bits.
    pub flags: u16,
}

/// Progress of a running scan, in MFT records. `total` is set once the MFT
/// size is known (after the bootstrap); `done` grows as chunks are parsed.
#[derive(Debug, Default)]
pub struct MftProgress {
    pub total: AtomicU64,
    pub done: AtomicU64,
}

impl MftProgress {
    pub fn new() -> MftProgress {
        MftProgress::default()
    }

    /// `done / total` in 0..=1 (0 while `total` is unknown).
    pub fn fraction(&self) -> f64 {
        let t = self.total.load(Ordering::Relaxed);
        if t == 0 {
            0.0
        } else {
            (self.done.load(Ordering::Relaxed) as f64 / t as f64).min(1.0)
        }
    }
}

/// Tuning knobs for [`scan_mft`]; the defaults suit real volumes.
#[derive(Debug, Clone, Copy, Default)]
pub struct MftOptions {
    /// Parse worker threads; 0 = available parallelism (max 64).
    pub threads: usize,
    /// MFT bytes per chunk read; 0 = 4 MiB. Clamped to 64 KiB..=16 MiB and
    /// rounded to whole records and 4 KiB blocks.
    pub chunk_size: usize,
    /// Threads issuing chunk reads concurrently (the calling thread is one
    /// of them); 0 = 1, i.e. strictly sequential I/O, one request in flight.
    /// 2 keeps two 4 MiB requests in flight, which may help NVMe drives and
    /// fragmented MFTs; it measured no faster on cached images here (and
    /// slower when everything is in memory), so it is opt-in.
    pub readers: usize,
}

/// Counters and timings of one scan.
#[derive(Debug, Clone, Default)]
pub struct MftStats {
    /// Record slots scanned (MFT size / record size, within the mapped part).
    pub records: u64,
    /// In-use records (base + extension).
    pub in_use: u64,
    pub extension_records: u64,
    /// Not-in-use or never-initialised slots.
    pub unused: u64,
    /// Slots without a "FILE" signature (incl. "BAAD").
    pub bad_signature: u64,
    /// Records skipped because the update-sequence check failed.
    pub bad_fixup: u64,
    /// Records skipped because their header names another record number.
    pub wrong_record_number: u64,
    /// In-use records with an undecodable attribute (partially used).
    pub corrupt: u64,
    /// Extension records whose base record is gone or was reused.
    pub stale_extensions: u64,
    /// In-use base records without a non-DOS name (not returned).
    pub nameless: u64,
    /// Returned files flagged `orphan`.
    pub orphans: u64,
    /// Hard links dropped because their parent reference was invalid.
    pub dropped_links: u64,
    pub cycles_broken: u64,
    /// Record 5 was unusable and the root entry was synthesized.
    pub root_synthesized: bool,
    /// Fragments (runs) of `$MFT::$DATA` after merging adjacent runs.
    pub mft_runs: u64,
    /// `$MFT` has an `$ATTRIBUTE_LIST`.
    pub mft_attribute_list: bool,
    /// Extension records whose `$MFT::$DATA` extents were loaded.
    pub mft_extension_records: u64,
    /// Records past the part of the runlist that could be loaded (not scanned).
    pub unmapped_records: u64,
    /// In-use records according to `$MFT:$BITMAP`, if it could be read (it
    /// sizes the file list up front; see the crate docs, "Memory").
    pub bitmap_in_use: Option<u64>,
    /// Times the file list outgrew that size (files created between the
    /// bitmap read and the record read) and was extended by a small margin.
    pub size_hint_overflows: u64,
    /// Record slots zero-filled because their sectors could not be read.
    pub unreadable_records: u64,
    pub reads: u64,
    pub bytes_read: u64,
    pub max_read: u64,
    pub read_errors: u64,
    pub threads: usize,
    pub readers: usize,
    pub chunk_size: u64,
    /// Time spent inside `ReadAt::read_at` (all reads).
    pub io_time: Duration,
    /// Parse time summed over all workers (CPU time, can exceed wall time).
    pub parse_time: Duration,
    /// Wall time of the read + parse pipeline.
    pub pipeline_time: Duration,
    /// Wall time of the single-threaded merge pass.
    pub merge_time: Duration,
    /// Wall time of the whole scan.
    pub total_time: Duration,
}

/// Result of [`scan_mft`].
#[derive(Debug, Clone, Default)]
pub struct MftScan {
    pub volume: VolumeInfo,
    /// Sorted by record number; see [`MftFile`].
    pub files: Vec<MftFile>,
    pub stats: MftStats,
}

/// Reads and validates the boot sector (one 4096-byte read at offset 0).
pub fn read_volume_info(vol: &dyn ReadAt) -> std::io::Result<VolumeInfo> {
    let mut buf = io::AlignedBuf::new(IO_ALIGN as usize);
    vol.read_at(0, buf.as_mut_slice())?;
    boot::parse_boot_sector(buf.as_slice())
}

/// Reads the whole MFT with default options. Returns the volume geometry and
/// all files sorted by record number (root = record 5, parent = itself).
/// Fails with `ErrorKind::Interrupted` when `cancel` is set.
pub fn read_mft(
    vol: &dyn ReadAt,
    progress: &MftProgress,
    cancel: &AtomicBool,
) -> std::io::Result<(VolumeInfo, Vec<MftFile>)> {
    let scan = scan_mft(vol, &MftOptions::default(), progress, cancel)?;
    Ok((scan.volume, scan.files))
}

/// [`read_mft`] with options and statistics.
pub fn scan_mft(
    vol: &dyn ReadAt,
    opts: &MftOptions,
    progress: &MftProgress,
    cancel: &AtomicBool,
) -> std::io::Result<MftScan> {
    let t0 = Instant::now();
    let vi = read_volume_info(vol)?;
    let counters = io::IoCounters::default();
    let disk = io::Disk {
        vol,
        end: vi.volume_size(),
        io: &counters,
    };
    let layout = mft::bootstrap(&disk, &vi)?;
    progress.total.store(layout.records, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);

    let threads = match opts.threads {
        0 => std::thread::available_parallelism().map_or(4, |n| n.get()),
        n => n,
    };
    let chunk = match opts.chunk_size {
        0 => 4 << 20,
        n => n.clamp(64 << 10, MAX_READ),
    };
    let cfg = mft::PipelineCfg {
        threads,
        readers: opts.readers.max(1),
        chunk_bytes: chunk as u64,
    };
    let t_pipe = Instant::now();
    let out = mft::run(&disk, &vi, &layout, &cfg, progress, cancel)?;
    let pipeline_time = t_pipe.elapsed();

    let t_merge = Instant::now();
    let mft::Merged {
        info,
        drafts,
        ns,
        exts,
        counts,
        hint_overflows,
    } = out.merged;
    let records = info.len() as u64;
    let in_use = info
        .iter()
        .filter(|&&i| i & assemble::INFO_IN_USE != 0)
        .count() as u64;
    let mut fc = assemble::FinalCounts::default();
    let files = assemble::finalize(info, drafts, ns, exts, &mut fc);
    let merge_time = t_merge.elapsed();
    if cancel.load(Ordering::Relaxed) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "MFT scan cancelled",
        ));
    }

    let stats = MftStats {
        records,
        in_use,
        extension_records: counts.extension,
        unused: counts.unused,
        bad_signature: counts.bad_signature,
        bad_fixup: counts.bad_fixup,
        wrong_record_number: counts.wrong_record_number,
        corrupt: counts.corrupt,
        stale_extensions: fc.stale_extensions,
        nameless: fc.nameless,
        orphans: fc.orphans,
        dropped_links: fc.dropped_links,
        cycles_broken: fc.cycles_broken,
        root_synthesized: fc.root_synthesized,
        mft_runs: layout.map.runs.len() as u64,
        mft_attribute_list: layout.attribute_list,
        mft_extension_records: layout.extents,
        unmapped_records: layout.unmapped_records,
        bitmap_in_use: layout.in_use_hint,
        size_hint_overflows: hint_overflows,
        unreadable_records: out.lost_bytes / vi.record_size as u64,
        reads: counters.reads.load(Ordering::Relaxed) + 1, // + boot sector
        bytes_read: counters.bytes.load(Ordering::Relaxed) + IO_ALIGN,
        max_read: counters.max_read.load(Ordering::Relaxed),
        read_errors: counters.errors.load(Ordering::Relaxed),
        threads: out.workers,
        readers: out.readers,
        chunk_size: out.chunk_bytes,
        io_time: counters.time(),
        parse_time: Duration::from_nanos(out.parse_nanos),
        pipeline_time,
        merge_time,
        total_time: t0.elapsed(),
    };
    Ok(MftScan {
        volume: vi,
        files,
        stats,
    })
}

/// Indices into `files` (as returned by [`read_mft`]) ordered so that every
/// non-orphan entry comes after its parent directory: the root first, then
/// breadth-first; each orphan starts a new subtree after everything reachable
/// from the root. Lets a tree builder insert nodes in one pass without
/// forward references. Entries whose parent is missing from `files` (only
/// possible if `files` was filtered) are treated like orphans.
pub fn parent_first_order(files: &[MftFile]) -> Vec<usize> {
    const NONE: u32 = u32::MAX;
    let mut index = std::collections::HashMap::with_capacity(files.len());
    for (i, f) in files.iter().enumerate() {
        index.insert(f.record, i as u32);
    }
    // Children as linked lists: first_child[i] -> next[child] -> ...
    let mut first_child = vec![NONE; files.len()];
    let mut next = vec![NONE; files.len()];
    let mut starts = Vec::new();
    for (i, f) in files.iter().enumerate().rev() {
        let parent = index.get(&f.parent).copied();
        match parent {
            Some(p) if !f.orphan && f.record != ROOT_RECORD && p as usize != i => {
                next[i] = first_child[p as usize];
                first_child[p as usize] = i as u32;
            }
            _ => starts.push(i),
        }
    }
    // Root first, then the other subtree starts in record order.
    starts.sort_by_key(|&i| (files[i].record != ROOT_RECORD, files[i].record));
    let mut order = Vec::with_capacity(files.len());
    let mut seen = vec![false; files.len()];
    for s in starts {
        let mut head = order.len();
        order.push(s);
        seen[s] = true;
        while head < order.len() {
            let mut c = first_child[order[head]];
            while c != NONE {
                if !std::mem::replace(&mut seen[c as usize], true) {
                    order.push(c as usize);
                }
                c = next[c as usize];
            }
            head += 1;
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(record: u64, parent: u64, orphan: bool) -> MftFile {
        MftFile {
            record,
            parent,
            orphan,
            is_dir: true,
            name: format!("r{record}"),
            ..MftFile::default()
        }
    }

    #[test]
    fn parent_first_order_puts_parents_first() {
        // Children with lower record numbers than their parents, an orphan
        // subtree, and the root in the middle of the list.
        let files = vec![
            file(3, 40, false),
            file(5, 5, false),
            file(7, 3, false),
            file(9, 77, true),
            file(10, 9, false),
            file(40, 5, false),
        ];
        let order = parent_first_order(&files);
        assert_eq!(order.len(), files.len());
        let pos = |rec: u64| order.iter().position(|&i| files[i].record == rec).unwrap();
        assert_eq!(pos(5), 0);
        for f in files.iter().filter(|f| !f.orphan && f.record != 5) {
            assert!(pos(f.parent) < pos(f.record), "record {}", f.record);
        }
        assert!(pos(9) < pos(10));
    }
}
