//! `$MFT` bootstrap (record 0, its runlist and attribute list) and the
//! streaming read + parallel parse pipeline.
//!
//! Bootstrap: record 0 is read from `$MFT` (falling back to `$MFTMirr`), its
//! unnamed `$DATA` runlist is decoded, and if it has an `$ATTRIBUTE_LIST`
//! (large fragmented MFTs), the remaining `$DATA` extents are loaded from the
//! extension records it names, in VCN order. Each extension record must lie
//! in the part of the MFT mapped so far, exactly as NTFS itself requires to
//! mount. If an extent cannot be loaded, the scan continues over the mapped
//! part and reports the rest in [`crate::MftStats::unmapped_records`].
//!
//! The bootstrap also reads `$MFT:$BITMAP` (from record 0, or from the
//! extension record the attribute list names) for the in-use record count,
//! the capacity hint of the [`Merger`].
//!
//! Pipeline: reader threads (default 1: the calling thread) take chunks in
//! index order (default 4 MiB, a multiple of the record size), read them
//! following the runlist and hand them to N parse workers over a bounded
//! channel; buffers are recycled through a second channel, so memory stays
//! at about `(readers + workers / 2 + 1) * chunk` bytes (32 MiB with 12
//! workers). Parsed chunks go to the [`Merger`], which appends them to the
//! file list in record order as they complete. One reader gives strictly sequential I/O, best for HDDs and as
//! fast as two on everything measured here; parsing is embarrassingly
//! parallel (records are independent until the merge).

use crate::MftFile;
use crate::assemble::{
    ChunkOut, Counts, ExtPart, INFO_BASE, INFO_DIR, INFO_IN_USE, draft_from_scratch,
};
use crate::io::{AlignedBuf, BOUNCE, Disk, IO_ALIGN, invalid};
use crate::record::{
    self, AT_ATTRIBUTE_LIST, AT_DATA, AttrIter, Body, Ctx, MREF_MASK, REC_DIRECTORY, REC_IN_USE,
    RecordStatus, Scratch,
};
use crate::runlist::{self, Run, RunMap};
use crate::{MftProgress, ROOT_RECORD, VolumeInfo, le};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Instant;

/// Where the MFT lives and how many records to scan.
#[derive(Debug, Default)]
pub(crate) struct Layout {
    pub map: RunMap,
    pub records: u64,
    pub unmapped_records: u64,
    pub attribute_list: bool,
    /// Extension records whose `$DATA` extents were loaded.
    pub extents: u64,
    /// In-use records according to `$MFT:$BITMAP` (base + extension
    /// records), if it could be read: a capacity hint for the merge.
    pub in_use_hint: Option<u64>,
}

/// Upper bound for a non-resident `$ATTRIBUTE_LIST` of `$MFT` (NTFS limits
/// attribute lists to 256 KiB; leave generous room).
const MAX_ATTR_LIST: u64 = 16 << 20;

/// `$BITMAP` attribute type (the in-use bitmap of `$MFT`).
const AT_BITMAP: u32 = 0xB0;

/// Upper bound for `$MFT:$BITMAP` (one bit per record: 256 M records).
const MAX_MFT_BITMAP: u64 = 32 << 20;

/// Records are indexed with u32 in the merge tables.
const MAX_RECORDS: u64 = u32::MAX as u64 - 1;

/// Consecutive completely unreadable chunks after which the scan stops.
const MAX_FAILED_CHUNKS: u32 = 8;

pub(crate) fn bootstrap(disk: &Disk, vi: &VolumeInfo) -> io::Result<Layout> {
    let primary = load_layout(disk, vi, vi.mft_lcn);
    match primary {
        Ok(l) => Ok(l),
        Err(e) if vi.mft_mirr_lcn < vi.total_clusters && vi.mft_mirr_lcn != vi.mft_lcn => {
            load_layout(disk, vi, vi.mft_mirr_lcn).map_err(|_| e)
        }
        Err(e) => Err(e),
    }
}

/// Reads one record at a volume offset and applies its fixups.
fn read_record_at(disk: &Disk, off: u64, rs: usize) -> io::Result<Vec<u8>> {
    let mut rec = disk.read_vec(off, rs)?;
    check_record(&mut rec)?;
    Ok(rec)
}

fn check_record(rec: &mut [u8]) -> io::Result<()> {
    if rec.get(0..4) != Some(b"FILE".as_slice()) {
        return Err(invalid("$MFT record has no FILE signature"));
    }
    record::apply_fixups(rec).map_err(|_| invalid("$MFT record: update sequence mismatch"))?;
    if le::u16_at(rec, 0x16).unwrap_or(0) & REC_IN_USE == 0 {
        return Err(invalid("$MFT record is not in use"));
    }
    Ok(())
}

/// One unnamed `$DATA` extent of `$MFT`.
struct Extent {
    lowest_vcn: u64,
    runs: Vec<Run>,
}

/// Collects the unnamed `$DATA` extents of a fixed-up `$MFT` record, plus
/// sizes from the first extent and the attribute list (if any).
struct Record0 {
    extents: Vec<Extent>,
    sizes: Option<(u64, u64)>, // (data_size, initialized_size)
    attr_list: Option<Vec<u8>>,
    /// `$MFT:$BITMAP`, if present in this record and readable.
    bitmap: Option<Vec<u8>>,
}

fn scan_mft_record(
    disk: &Disk,
    vi: &VolumeInfo,
    rec: &[u8],
    want_list: bool,
) -> io::Result<Record0> {
    let mut out = Record0 {
        extents: Vec::new(),
        sizes: None,
        attr_list: None,
        bitmap: None,
    };
    for attr in AttrIter::new(rec) {
        let attr = attr.map_err(|_| invalid("$MFT record: corrupt attribute header"))?;
        match (attr.ty, &attr.body) {
            (AT_DATA, Body::NonResident(nr)) if attr.name.is_empty() => {
                let mut runs = Vec::new();
                runlist::decode(nr.mapping_pairs, nr.lowest_vcn, &mut runs)
                    .map_err(|e| invalid(format!("$MFT runlist: {e}")))?;
                if nr.lowest_vcn == 0 {
                    out.sizes = Some((nr.data, nr.initialized));
                }
                out.extents.push(Extent {
                    lowest_vcn: nr.lowest_vcn,
                    runs,
                });
            }
            (AT_DATA, Body::Resident(_)) if attr.name.is_empty() => {
                return Err(invalid("$MFT has resident $DATA"));
            }
            (AT_ATTRIBUTE_LIST, body) if want_list => {
                out.attr_list = Some(match body {
                    Body::Resident(v) => v.to_vec(),
                    Body::NonResident(nr) => {
                        read_nonresident(disk, vi, nr.mapping_pairs, nr.data, MAX_ATTR_LIST)
                            .map_err(|e| invalid(format!("$MFT attribute list: {e}")))?
                    }
                });
            }
            (AT_BITMAP, body) if want_list && attr.name.is_empty() => {
                out.bitmap = bitmap_value(disk, vi, body);
            }
            _ => {}
        }
    }
    Ok(out)
}

/// The value of `$MFT:$BITMAP`. Only a capacity hint: a bitmap that cannot
/// be read is ignored.
fn bitmap_value(disk: &Disk, vi: &VolumeInfo, body: &Body) -> Option<Vec<u8>> {
    match body {
        Body::Resident(v) => Some(v.to_vec()),
        Body::NonResident(nr) if nr.lowest_vcn == 0 => {
            read_nonresident(disk, vi, nr.mapping_pairs, nr.data, MAX_MFT_BITMAP).ok()
        }
        Body::NonResident(_) => None,
    }
}

/// Reads a small non-resident attribute value of `$MFT` (the attribute
/// list, the in-use bitmap) of at most `max` bytes.
fn read_nonresident(
    disk: &Disk,
    vi: &VolumeInfo,
    mp: &[u8],
    size: u64,
    max: u64,
) -> io::Result<Vec<u8>> {
    if size > max {
        return Err(invalid("implausibly large"));
    }
    let mut runs = Vec::new();
    runlist::decode(mp, 0, &mut runs).map_err(|e| invalid(format!("runlist: {e}")))?;
    let mut map = RunMap {
        runs: Vec::new(),
        cluster: vi.bytes_per_cluster as u64,
    };
    map.extend(&runs, vi.total_clusters)?;
    if map.bytes() < size {
        return Err(invalid("runlist is shorter than the value"));
    }
    let mut buf = AlignedBuf::new(size as usize);
    let mut bounce = AlignedBuf::new(BOUNCE);
    map.read(disk, 0, buf.as_mut_slice(), &mut bounce)?;
    Ok(buf.as_slice().to_vec())
}

/// `$MFT:$BITMAP` from the extension record the attribute list names (a big
/// `$MFT` whose base record overflowed), if that record lies in the part of
/// the MFT mapped by record 0's own extents.
fn bitmap_in_extension(disk: &Disk, vi: &VolumeInfo, map: &RunMap, list: &[u8]) -> Option<Vec<u8>> {
    let rs = vi.record_size as u64;
    let e = entries_in_list(list, AT_BITMAP)
        .into_iter()
        .find(|e| e.lowest_vcn == 0 && e.record != 0)?;
    let start = e
        .record
        .checked_mul(rs)
        .filter(|s| s.saturating_add(rs) <= map.bytes())?;
    let mut buf = AlignedBuf::new(rs as usize);
    map.read(
        disk,
        start,
        buf.as_mut_slice(),
        &mut AlignedBuf::new(BOUNCE),
    )
    .ok()?;
    let rec = buf.as_mut_slice();
    check_record(rec).ok()?;
    AttrIter::new(rec)
        .map_while(Result::ok)
        .find(|a| a.ty == AT_BITMAP && a.name.is_empty())
        .and_then(|a| bitmap_value(disk, vi, &a.body))
}

/// Set bits among the first `records` bits of an in-use bitmap.
fn count_in_use(bitmap: &[u8], records: u64) -> u64 {
    let full = usize::try_from(records / 8).unwrap_or(usize::MAX);
    let mut n: u64 = bitmap
        .iter()
        .take(full)
        .map(|b| u64::from(b.count_ones()))
        .sum();
    if let Some(&last) = bitmap.get(full) {
        let tail = (records % 8) as u32;
        n += u64::from((last & ((1u16 << tail) - 1) as u8).count_ones());
    }
    n
}

/// An `$ATTRIBUTE_LIST` entry for an unnamed `$DATA` extent.
struct ListEntry {
    lowest_vcn: u64,
    record: u64,
}

/// Unnamed attributes of type `want` listed in an `$ATTRIBUTE_LIST`.
fn data_extents_in_list(list: &[u8]) -> Vec<ListEntry> {
    entries_in_list(list, AT_DATA)
}

fn entries_in_list(list: &[u8], want: u32) -> Vec<ListEntry> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let (Some(ty), Some(len)) = (le::u32_at(list, pos), le::u16_at(list, pos + 4)) {
        let len = len as usize;
        if len < 0x1A || pos + len > list.len() {
            break;
        }
        let name_len = list[pos + 6];
        if let (true, 0, Some(vcn), Some(mref)) = (
            ty == want,
            name_len,
            le::u64_at(list, pos + 8),
            le::u64_at(list, pos + 0x10),
        ) {
            out.push(ListEntry {
                lowest_vcn: vcn,
                record: mref & MREF_MASK,
            });
        }
        pos += len;
    }
    out
}

fn load_layout(disk: &Disk, vi: &VolumeInfo, lcn: u64) -> io::Result<Layout> {
    let rs = vi.record_size as u64;
    let cs = vi.bytes_per_cluster as u64;
    let off = lcn
        .checked_mul(cs)
        .ok_or_else(|| invalid("$MFT offset overflow"))?;
    let rec0 = read_record_at(disk, off, rs as usize)?;
    let r0 = scan_mft_record(disk, vi, &rec0, true)?;
    let (data_size, initialized) = r0
        .sizes
        .ok_or_else(|| invalid("$MFT record 0 has no $DATA extent at VCN 0"))?;

    let mut layout = Layout {
        map: RunMap {
            runs: Vec::new(),
            cluster: cs,
        },
        ..Layout::default()
    };
    let mut extents = r0.extents;
    extents.sort_by_key(|e| e.lowest_vcn);
    for e in &extents {
        if e.lowest_vcn == layout.map.clusters() {
            layout.map.extend(&e.runs, vi.total_clusters)?;
        }
    }
    if layout.map.clusters() == 0 {
        return Err(invalid("$MFT has no mapped clusters"));
    }

    let mut bitmap = r0.bitmap;
    if let Some(list) = r0.attr_list {
        layout.attribute_list = true;
        if bitmap.is_none() {
            bitmap = bitmap_in_extension(disk, vi, &layout.map, &list);
        }
        let mut entries = data_extents_in_list(&list);
        entries.sort_by_key(|e| e.lowest_vcn);
        entries.dedup_by_key(|e| e.lowest_vcn);
        let mut bounce = AlignedBuf::new(BOUNCE);
        for e in entries {
            if e.lowest_vcn < layout.map.clusters() || e.record == 0 {
                continue; // already mapped (record 0's own extents)
            }
            if e.lowest_vcn > layout.map.clusters() {
                break; // gap: the rest stays unmapped
            }
            // The extension record must be readable through what is mapped.
            let Some(start) = e
                .record
                .checked_mul(rs)
                .filter(|s| s.saturating_add(rs) <= layout.map.bytes())
            else {
                break;
            };
            let mut buf = AlignedBuf::new(rs as usize);
            if layout
                .map
                .read(disk, start, buf.as_mut_slice(), &mut bounce)
                .is_err()
            {
                break;
            }
            let rec = buf.as_mut_slice();
            if check_record(rec).is_err() || le::u64_at(rec, 0x20).unwrap_or(0) & MREF_MASK != 0 {
                break; // not an extension record of $MFT
            }
            let Ok(found) = scan_mft_record(disk, vi, rec, false) else {
                break;
            };
            let before = layout.map.clusters();
            for x in found.extents.iter().filter(|x| x.lowest_vcn == before) {
                if layout.map.extend(&x.runs, vi.total_clusters).is_err() {
                    break;
                }
            }
            if layout.map.clusters() == before {
                break;
            }
            layout.extents += 1;
        }
    }

    // Use the initialized size if it is plausible (records past it are not
    // valid even if allocated).
    let valid_bytes = if initialized >= 16 * rs && initialized < data_size {
        initialized
    } else {
        data_size
    };
    let mapped = layout.map.bytes();
    layout.records = valid_bytes.min(mapped) / rs;
    layout.unmapped_records = valid_bytes.saturating_sub(mapped) / rs;
    if layout.records > MAX_RECORDS {
        return Err(invalid(format!("$MFT claims {} records", layout.records)));
    }
    if layout.records <= 5 {
        return Err(invalid("$MFT is too small to hold the root directory"));
    }
    layout.in_use_hint = bitmap
        .map(|b| count_in_use(&b, layout.records))
        .filter(|&n| n > ROOT_RECORD);
    Ok(layout)
}

/// Parse results of all chunks, concatenated in record order.
#[derive(Debug, Default)]
pub(crate) struct Merged {
    /// One entry per record slot (see [`ChunkOut::info`]).
    pub info: Vec<u32>,
    pub drafts: Vec<MftFile>,
    pub ns: Vec<u8>,
    pub exts: Vec<ExtPart>,
    pub counts: Counts,
    /// Times the draft list outgrew the capacity hint and was extended by a
    /// small margin (see [`Merger`]).
    pub hint_overflows: u64,
}

/// Concatenates chunk results in chunk order while the pipeline runs.
///
/// The draft list is the biggest allocation of a scan (~100 bytes per file
/// plus its name). Collecting every chunk's drafts first and concatenating
/// them at the end would hold the list twice at the peak; with a capacity
/// hint (`$MFT:$BITMAP`'s in-use count, an upper bound on base records) the
/// list is allocated once and each chunk is appended and freed as soon as
/// all chunks before it are in, so only the few chunks completed out of
/// order wait. Without a hint the chunks are kept and concatenated at the
/// end into an exactly sized list (the old behaviour, twice the list at the
/// peak), because growing a list of that size by doubling costs more.
///
/// The hint is read before the records, so files created in between can
/// push the list past it. Such an overflow never relies on `Vec`'s
/// amortized growth, which would double the list (old + doubled list at
/// once during the copy, and the doubled capacity kept to the end): the
/// list grows by [`overflow_margin`] instead, and the event is counted in
/// [`MftStats::size_hint_overflows`](crate::MftStats::size_hint_overflows).
pub(crate) struct Merger {
    next: u64,
    streaming: bool,
    pending: Vec<ChunkOut>,
    out: Merged,
}

impl Merger {
    /// `records`: record slots to be scanned; `hint`: expected base records.
    pub(crate) fn new(records: u64, hint: Option<u64>) -> Merger {
        let cap = |n: u64| usize::try_from(n).unwrap_or(0);
        let drafts = hint.map_or(0, |h| cap(h.min(records)));
        Merger {
            next: 0,
            streaming: hint.is_some(),
            pending: Vec::new(),
            out: Merged {
                info: Vec::with_capacity(cap(records)),
                drafts: Vec::with_capacity(drafts),
                ns: Vec::with_capacity(drafts),
                ..Merged::default()
            },
        }
    }

    pub(crate) fn push(&mut self, chunk: ChunkOut) {
        if !self.streaming || chunk.index != self.next {
            self.pending.push(chunk);
            return;
        }
        self.append(chunk);
        while let Some(i) = self.pending.iter().position(|c| c.index == self.next) {
            let c = self.pending.swap_remove(i);
            self.append(c);
        }
    }

    pub(crate) fn finish(mut self) -> Merged {
        let mut rest = std::mem::take(&mut self.pending);
        rest.sort_by_key(|c| c.index);
        let drafts: usize = rest.iter().map(|c| c.drafts.len()).sum();
        self.out.drafts.reserve_exact(drafts);
        self.out.ns.reserve_exact(drafts);
        for c in rest {
            self.append(c);
        }
        self.out
    }

    fn append(&mut self, mut c: ChunkOut) {
        let o = &mut self.out;
        o.counts.add(&c.counts);
        // Chunks are contiguous unless reading stopped early; keep the
        // record-number indexing of `info` exact anyway.
        if (o.info.len() as u64) < c.first_record {
            o.info.resize(c.first_record as usize, 0);
        }
        o.info.append(&mut c.info);
        let need = o.drafts.len() + c.drafts.len();
        if self.streaming && need > o.drafts.capacity() {
            let extra = overflow_margin(o.drafts.capacity()).max(c.drafts.len());
            o.drafts.reserve_exact(extra);
            o.ns.reserve_exact(o.drafts.capacity().saturating_sub(o.ns.len()));
            o.hint_overflows += 1;
        }
        o.drafts.append(&mut c.drafts);
        o.ns.append(&mut c.ns);
        o.exts.append(&mut c.exts);
        self.next = c.index + 1;
    }
}

/// Extra draft slots reserved when the list outgrows its capacity hint:
/// ~3 % of the list (at least 64 K entries), so a hint that is a few
/// records short costs one small reallocation instead of a doubling.
pub(crate) fn overflow_margin(capacity: usize) -> usize {
    (capacity / 32).max(1 << 16)
}

/// Totals collected by the pipeline.
#[derive(Debug, Default)]
pub(crate) struct PipelineOut {
    pub merged: Merged,
    pub lost_bytes: u64,
    pub parse_nanos: u64,
    pub workers: usize,
    pub readers: usize,
    pub chunk_bytes: u64,
}

pub(crate) struct PipelineCfg {
    pub threads: usize,
    pub readers: usize,
    pub chunk_bytes: u64,
}

/// Read-side bookkeeping shared by the reader threads.
#[derive(Default)]
struct ReadState {
    lost_bytes: u64,
    ok_bytes: u64,
    /// Bytes of chunks handed to the parsers (read, salvaged or sparse).
    done_bytes: u64,
    first_err: Option<io::Error>,
    failed_in_a_row: u32,
}

type Job = (u64, u64, AlignedBuf, usize);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub(crate) fn run(
    disk: &Disk,
    vi: &VolumeInfo,
    layout: &Layout,
    cfg: &PipelineCfg,
    progress: &MftProgress,
    cancel: &AtomicBool,
) -> io::Result<PipelineOut> {
    let rs = vi.record_size as u64;
    let ctx = Ctx {
        cluster: vi.bytes_per_cluster as u64,
    };
    let total = layout.records * rs;
    // Chunks are whole records and whole 4 KiB blocks of the MFT.
    let unit = rs.max(IO_ALIGN);
    let chunk_bytes = (cfg.chunk_bytes / unit).max(1) * unit;
    let n_chunks = total.div_ceil(chunk_bytes);
    let workers = cfg.threads.clamp(1, 64).min(n_chunks.max(1) as usize);
    let readers = cfg.readers.clamp(1, 8).min(n_chunks.max(1) as usize);
    let max_pool = readers + workers.div_ceil(2) + 1;

    let results = Mutex::new(Merger::new(layout.records, layout.in_use_hint));
    let parse_nanos = AtomicU64::new(0);
    let state = Mutex::new(ReadState::default());
    let next_chunk = AtomicU64::new(0);
    let pool = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);

    let (job_tx, job_rx) = mpsc::sync_channel::<Job>(workers);
    let job_rx = Mutex::new(job_rx);
    let (free_tx, free_rx) = mpsc::channel::<AlignedBuf>();
    let free_rx = Mutex::new(free_rx);

    // Reads chunks in index order (several readers interleave) and hands
    // them to the parsers.
    let reader = |job_tx: mpsc::SyncSender<Job>| {
        let mut bounce = AlignedBuf::new(BOUNCE);
        while !cancel.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
            let index = next_chunk.fetch_add(1, Ordering::Relaxed);
            if index >= n_chunks {
                break;
            }
            // Reuse a returned buffer when possible: fresh ones cost page faults.
            let recycled = {
                let rx = lock(&free_rx);
                match rx.try_recv() {
                    Ok(b) => Some(b),
                    Err(_) if pool.fetch_add(1, Ordering::Relaxed) < max_pool => None,
                    Err(_) => {
                        pool.fetch_sub(1, Ordering::Relaxed);
                        match rx.recv() {
                            Ok(b) => Some(b),
                            Err(_) => break,
                        }
                    }
                }
            };
            let mut buf = recycled.unwrap_or_else(|| AlignedBuf::new(chunk_bytes as usize));
            let start = index * chunk_bytes;
            let end = (start + chunk_bytes).min(total);
            let len = (end - start) as usize;
            let dst = &mut buf.as_mut_slice()[..len];
            let (mut lost, mut ok, mut err) = (0u64, 0u64, None);
            layout.map.pieces(start, end, |p| {
                let d = &mut dst[p.dst as usize..(p.dst + p.len) as usize];
                match p.phys {
                    None => d.fill(0),
                    Some(off) => {
                        let (l, e) = disk.read_span_salvage(off, d, p.dst as usize, &mut bounce);
                        lost += l;
                        ok += p.len - l;
                        if err.is_none() {
                            err = e;
                        }
                    }
                }
            });
            {
                let mut st = lock(&state);
                st.lost_bytes += lost;
                st.ok_bytes += ok;
                st.done_bytes += len as u64;
                if st.first_err.is_none() {
                    st.first_err = err;
                }
                if lost == len as u64 {
                    st.failed_in_a_row += 1;
                    if st.failed_in_a_row >= MAX_FAILED_CHUNKS {
                        // Nothing readable for a while: the runlist points
                        // past the real device. Stop reading.
                        stop.store(true, Ordering::Relaxed);
                    }
                } else {
                    st.failed_in_a_row = 0;
                }
            }
            if job_tx.send((index, start / rs, buf, len)).is_err() {
                break;
            }
        }
    };

    std::thread::scope(|s| {
        for _ in 0..workers {
            let free_tx = free_tx.clone();
            let (job_rx, results, parse_nanos) = (&job_rx, &results, &parse_nanos);
            s.spawn(move || {
                let mut sc = Scratch::default();
                loop {
                    let job = lock(job_rx).recv();
                    let Ok((index, first, mut buf, len)) = job else {
                        break;
                    };
                    if !cancel.load(Ordering::Relaxed) {
                        let t = Instant::now();
                        let mut out = ChunkOut {
                            index,
                            first_record: first,
                            ..ChunkOut::default()
                        };
                        parse_chunk(
                            &mut buf.as_mut_slice()[..len],
                            first,
                            rs as usize,
                            ctx,
                            &mut sc,
                            &mut out,
                        );
                        parse_nanos.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        progress
                            .done
                            .fetch_add(out.info.len() as u64, Ordering::Relaxed);
                        lock(results).push(out);
                    }
                    if free_tx.send(buf).is_err() {
                        break;
                    }
                }
            });
        }
        drop(free_tx);
        for _ in 1..readers {
            let tx = job_tx.clone();
            s.spawn(|| reader(tx));
        }
        reader(job_tx);
    });

    if cancel.load(Ordering::Relaxed) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "MFT scan cancelled",
        ));
    }
    let st = state.into_inner().unwrap_or_else(|p| p.into_inner());
    if let Some(e) = st.first_err
        && st.ok_bytes == 0
    {
        return Err(e);
    }
    let merged = results.into_inner().unwrap_or_else(|p| p.into_inner());
    Ok(PipelineOut {
        merged: merged.finish(),
        // Chunks never read after an early stop count as unreadable.
        lost_bytes: st.lost_bytes + (total - st.done_bytes.min(total)),
        parse_nanos: parse_nanos.into_inner(),
        workers,
        readers,
        chunk_bytes,
    })
}

/// Parses all records of one chunk (`buf` holds whole records, the first
/// being record `first`).
pub(crate) fn parse_chunk(
    buf: &mut [u8],
    first: u64,
    rs: usize,
    ctx: Ctx,
    sc: &mut Scratch,
    out: &mut ChunkOut,
) {
    out.info.reserve(buf.len() / rs);
    for (i, rec) in buf.chunks_exact_mut(rs).enumerate() {
        let recno = first + i as u64;
        let status = record::parse_into(rec, recno, ctx, sc);
        let mut info = 0u32;
        match status {
            RecordStatus::InUse => {
                info = sc.seq as u32 | INFO_IN_USE;
                if sc.flags & REC_DIRECTORY != 0 {
                    info |= INFO_DIR;
                }
                if sc.corrupt {
                    out.counts.corrupt += 1;
                }
                if sc.base_ref == 0 {
                    info |= INFO_BASE;
                    let (draft, ns) = draft_from_scratch(recno, sc);
                    out.drafts.push(draft);
                    out.ns.push(ns);
                } else {
                    out.counts.extension += 1;
                    out.exts.push(ExtPart::from_scratch(sc));
                }
            }
            RecordStatus::Unused => out.counts.unused += 1,
            RecordStatus::BadSignature => out.counts.bad_signature += 1,
            RecordStatus::BadFixup => out.counts.bad_fixup += 1,
            RecordStatus::WrongRecordNumber => out.counts.wrong_record_number += 1,
        }
        out.info.push(info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Chunk `index` of 4 records; records 4i+1 and 4i+3 are base records.
    fn chunk(index: u64) -> ChunkOut {
        let first = index * 4;
        let mut c = ChunkOut {
            index,
            first_record: first,
            info: vec![INFO_IN_USE; 4],
            ..ChunkOut::default()
        };
        for r in [first + 1, first + 3] {
            c.drafts.push(MftFile {
                record: r,
                name: format!("r{r}"),
                ..MftFile::default()
            });
            c.ns.push(1);
        }
        c.counts.unused = 2;
        c
    }

    fn merge(order: &[u64], hint: Option<u64>) -> Merged {
        let mut m = Merger::new(40, hint);
        for &i in order {
            m.push(chunk(i));
        }
        m.finish()
    }

    #[test]
    fn merger_concatenates_in_chunk_order_with_any_hint() {
        let order = [2, 0, 5, 1, 3, 4, 9, 7, 6, 8];
        let expect: Vec<u64> = (0..10).flat_map(|i| [4 * i + 1, 4 * i + 3]).collect();
        // Exact hint, too small a hint (the list grows), no hint (kept
        // until the end, then concatenated).
        for hint in [Some(20), Some(3), None] {
            let m = merge(&order, hint);
            let got: Vec<u64> = m.drafts.iter().map(|f| f.record).collect();
            assert_eq!(got, expect, "{hint:?}");
            assert_eq!(m.ns, vec![1; 20]);
            assert_eq!(m.info.len(), 40);
            assert_eq!(m.counts.unused, 20);
            if hint == Some(20) {
                assert_eq!(m.drafts.capacity(), 20, "allocated once");
            }
        }
    }

    #[test]
    fn a_short_hint_grows_the_list_by_a_margin_not_by_doubling() {
        // Big chunks: 10 x 20_000 drafts, hint one short of the total.
        let big = |i: u64| {
            let first = i * 20_000;
            ChunkOut {
                index: i,
                first_record: first,
                info: vec![INFO_IN_USE; 20_000],
                drafts: (0..20_000)
                    .map(|r| MftFile {
                        record: first + r,
                        ..MftFile::default()
                    })
                    .collect(),
                ns: vec![1; 20_000],
                ..ChunkOut::default()
            }
        };
        let total = 200_000usize;
        for hint in [total as u64 - 1, total as u64 - 30_000] {
            let mut m = Merger::new(total as u64, Some(hint));
            for i in 0..10 {
                m.push(big(i));
            }
            let m = m.finish();
            assert_eq!(m.drafts.len(), total);
            assert_eq!(m.ns.len(), total);
            assert!(m.hint_overflows >= 1, "{hint}");
            let cap = m.drafts.capacity();
            let limit = hint as usize + overflow_margin(hint as usize) + 20_000;
            assert!(cap <= limit, "hint {hint}: capacity {cap} > {limit}");
            assert!(cap < 2 * total - 30_000, "not doubled: {cap}");
            assert!(m.ns.capacity() <= limit);
        }
        // An exact hint never overflows.
        let mut m = Merger::new(total as u64, Some(total as u64));
        for i in 0..10 {
            m.push(big(i));
        }
        let m = m.finish();
        assert_eq!((m.hint_overflows, m.drafts.capacity()), (0, total));
    }

    #[test]
    fn merger_keeps_record_indexing_across_missing_chunks() {
        // Chunk 2 never arrives (reading stopped): later chunks still land
        // at their record numbers.
        for hint in [Some(20), None] {
            let m = merge(&[0, 1, 4, 3], hint);
            assert_eq!(m.info.len(), 20);
            assert_eq!(m.info[8..12], [0; 4]);
            let got: Vec<u64> = m.drafts.iter().map(|f| f.record).collect();
            assert_eq!(got, [1, 3, 5, 7, 13, 15, 17, 19]);
        }
    }

    #[test]
    fn in_use_count_stops_at_the_record_count() {
        assert_eq!(count_in_use(&[0xFF, 0xFF], 16), 16);
        assert_eq!(count_in_use(&[0xFF, 0xFF], 11), 11);
        assert_eq!(count_in_use(&[0b1010_1010, 0xFF], 4), 2);
        assert_eq!(count_in_use(&[0xFF], 64), 8, "short bitmap");
        assert_eq!(count_in_use(&[], 64), 0);
    }
}
