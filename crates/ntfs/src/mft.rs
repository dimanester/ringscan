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
//! Pipeline: reader threads (default 1: the calling thread) take chunks in
//! index order (default 4 MiB, a multiple of the record size), read them
//! following the runlist and hand them to N parse workers over a bounded
//! channel; buffers are recycled through a second channel, so memory stays
//! at about `(readers + workers / 2 + 1) * chunk` bytes (32 MiB with 12
//! workers). One reader gives strictly sequential I/O, best for HDDs and as
//! fast as two on everything measured here; parsing is embarrassingly
//! parallel (records are independent until the merge).

use crate::assemble::{ChunkOut, Draft, ExtPart, INFO_BASE, INFO_DIR, INFO_IN_USE};
use crate::io::{AlignedBuf, BOUNCE, Disk, IO_ALIGN, invalid};
use crate::record::{
    self, AT_ATTRIBUTE_LIST, AT_DATA, AttrIter, Body, Ctx, MREF_MASK, REC_DIRECTORY, REC_IN_USE,
    RecordStatus, Scratch,
};
use crate::runlist::{self, Run, RunMap};
use crate::{MftProgress, VolumeInfo, le};
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
}

/// Upper bound for a non-resident `$ATTRIBUTE_LIST` of `$MFT` (NTFS limits
/// attribute lists to 256 KiB; leave generous room).
const MAX_ATTR_LIST: u64 = 16 << 20;

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
                    Body::NonResident(nr) => read_nonresident(disk, vi, nr.mapping_pairs, nr.data)?,
                });
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Reads a small non-resident attribute value (an attribute list).
fn read_nonresident(disk: &Disk, vi: &VolumeInfo, mp: &[u8], size: u64) -> io::Result<Vec<u8>> {
    if size > MAX_ATTR_LIST {
        return Err(invalid("$MFT attribute list is implausibly large"));
    }
    let mut runs = Vec::new();
    runlist::decode(mp, 0, &mut runs)
        .map_err(|e| invalid(format!("$MFT attribute list runlist: {e}")))?;
    let mut map = RunMap {
        runs: Vec::new(),
        cluster: vi.bytes_per_cluster as u64,
    };
    map.extend(&runs, vi.total_clusters)?;
    if map.bytes() < size {
        return Err(invalid(
            "$MFT attribute list runlist is shorter than its size",
        ));
    }
    let mut buf = AlignedBuf::new(size as usize);
    let mut bounce = AlignedBuf::new(BOUNCE);
    map.read(disk, 0, buf.as_mut_slice(), &mut bounce)?;
    Ok(buf.as_slice().to_vec())
}

/// An `$ATTRIBUTE_LIST` entry for an unnamed `$DATA` extent.
struct ListEntry {
    lowest_vcn: u64,
    record: u64,
}

fn data_extents_in_list(list: &[u8]) -> Vec<ListEntry> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let (Some(ty), Some(len)) = (le::u32_at(list, pos), le::u16_at(list, pos + 4)) {
        let len = len as usize;
        if len < 0x1A || pos + len > list.len() {
            break;
        }
        let name_len = list[pos + 6];
        if let (AT_DATA, 0, Some(vcn), Some(mref)) = (
            ty,
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

    if let Some(list) = r0.attr_list {
        layout.attribute_list = true;
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
    Ok(layout)
}

/// Totals collected by the pipeline.
#[derive(Debug, Default)]
pub(crate) struct PipelineOut {
    pub chunks: Vec<ChunkOut>,
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

    let results: Mutex<Vec<ChunkOut>> = Mutex::new(Vec::new());
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
    let mut chunks = results.into_inner().unwrap_or_else(|p| p.into_inner());
    chunks.sort_by_key(|c| c.index);
    Ok(PipelineOut {
        chunks,
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
                    out.drafts.push(Draft::from_scratch(recno, sc));
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
