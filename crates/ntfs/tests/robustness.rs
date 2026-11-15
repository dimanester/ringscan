//! Robustness: randomly corrupted records and volumes must never panic,
//! overflow or hang — the release build aborts on panic, so a single
//! out-of-bounds index on a hostile disk would kill the app.
//!
//! * `fuzz_records`: 1 000 000 mutated copies of real FILE records (from the
//!   fixture images when present, plus the synthetic volume's records, which
//!   cover attribute lists, ADS, sparse/compressed and reparse attributes)
//!   through `parse_record`.
//! * `fuzz_volumes`: thousands of full scans of the synthetic volumes with
//!   corrupted boot sector, `$MFT` record 0, its extension records,
//!   attribute list and random records; plus a few hundred on real fixture
//!   images through a copy-on-write overlay.

mod common;

use common::*;
use ringscan_ntfs::{MftOptions, MftProgress, ReadAt, RecordStatus, parse_record, scan_mft};
use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Interesting values for overwritten fields.
const MAGIC: [u64; 14] = [
    0,
    1,
    2,
    7,
    8,
    0x18,
    0x7F,
    0x80,
    0xFF,
    0x100,
    0x7FFF,
    0xFFFF,
    0x7FFF_FFFF,
    u64::MAX,
];

/// Applies 1..=8 random corruptions to `buf`.
fn mutate(rng: &mut Rng, buf: &mut [u8]) {
    if buf.is_empty() {
        return;
    }
    let n = 1 + rng.below(8);
    for _ in 0..n {
        let len = buf.len() as u64;
        match rng.below(6) {
            // Random byte anywhere.
            0 => buf[rng.below(len) as usize] = rng.next() as u8,
            // Bit flip.
            1 => buf[rng.below(len) as usize] ^= 1 << rng.below(8),
            // Header fields (offsets, sizes, flags, base reference).
            2 => {
                let off = rng.below(0x30.min(len)) as usize;
                buf[off] = rng.next() as u8;
            }
            // A 2/4/8-byte field set to a boundary value.
            _ => {
                let w = [2usize, 4, 8][rng.below(3) as usize];
                let off = (rng.below(len) as usize) & !(w - 1);
                let v = MAGIC[rng.below(MAGIC.len() as u64) as usize].to_le_bytes();
                let end = (off + w).min(buf.len());
                buf[off..end].copy_from_slice(&v[..end - off]);
            }
        }
    }
}

/// In-use records of the synthetic volumes (all geometries).
fn synthetic_records() -> Vec<(u64, u32, Vec<u8>)> {
    let mut out = Vec::new();
    for g in &GEOMETRIES {
        let img = build(g);
        let rs = g.record as usize;
        // Reassemble every record through the runlist.
        let cs = g.cluster as usize;
        let mut mft = Vec::new();
        for &(len, lcn) in &g.runs {
            let at = lcn as usize * cs;
            mft.extend_from_slice(&img.bytes[at..at + len as usize * cs]);
        }
        for (i, rec) in mft.chunks_exact(rs).enumerate() {
            if rec[0x16] & 1 != 0 {
                out.push((i as u64, g.cluster, rec.to_vec()));
            }
        }
    }
    out
}

/// Up to `max` in-use records from the start of each fixture's MFT (the
/// first run always holds the first records; the header self-check filters
/// out anything past it).
fn fixture_records(max: usize) -> Vec<(u64, u32, Vec<u8>)> {
    let mut out = Vec::new();
    for (img, _) in fixtures() {
        let f = std::fs::File::open(&img).unwrap();
        let vi = ringscan_ntfs::read_volume_info(&f).unwrap();
        let rs = vi.record_size as usize;
        let start = vi.mft_lcn * vi.bytes_per_cluster as u64;
        let n = max.min(((vi.volume_size() - start) / rs as u64) as usize);
        let mut buf = vec![0u8; n * rs];
        if f.read_at(start, &mut buf).is_err() {
            continue;
        }
        for (i, rec) in buf.chunks_exact(rs).enumerate() {
            let own = u32::from_le_bytes(rec[0x2C..0x30].try_into().unwrap());
            if &rec[0..4] == b"FILE" && rec[0x16] & 1 != 0 && own == i as u32 {
                out.push((i as u64, vi.bytes_per_cluster, rec.to_vec()));
            }
        }
    }
    out
}

#[test]
fn fuzz_records() {
    let mut corpus = synthetic_records();
    corpus.extend(fixture_records(3000));
    // Keep records that parse cleanly before mutation (the synthetic volume
    // also holds deliberately damaged ones).
    corpus.retain(|(recno, cluster, rec)| {
        let r = parse_record(&mut rec.clone(), *recno, *cluster);
        r.status == RecordStatus::InUse && !r.corrupt
    });
    assert!(corpus.len() > 50);
    let mut rng = Rng(0x5EED_1234);
    let t = Instant::now();
    let iterations = 1_000_000;
    let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
    for _ in 0..iterations {
        let (recno, cluster, rec) = &corpus[rng.below(corpus.len() as u64) as usize];
        let mut buf = rec.clone();
        mutate(&mut rng, &mut buf);
        // Sometimes hand over a buffer of the wrong length.
        if rng.below(50) == 0 {
            let n = rng.below(buf.len() as u64 + 1) as usize;
            buf.truncate(n);
        }
        let r = parse_record(&mut buf, *recno, *cluster);
        if r.status != RecordStatus::InUse {
            assert!(r.names.is_empty() && r.logical == 0 && r.alloc == 0);
        }
        *by_status
            .entry(format!(
                "{:?}{}",
                r.status,
                if r.corrupt { "+corrupt" } else { "" }
            ))
            .or_default() += 1;
    }
    eprintln!(
        "{iterations} mutated records ({} in corpus) in {:?}: {by_status:?}",
        corpus.len(),
        t.elapsed()
    );
    // The mutations must reach the attribute parser, not just the checks.
    assert!(by_status.get("InUse").copied().unwrap_or(0) > iterations / 5);
    assert!(by_status.get("InUse+corrupt").copied().unwrap_or(0) > 1000);
}

/// Copy-on-write view of a volume.
struct Overlay<'a> {
    base: &'a dyn ReadAt,
    patches: BTreeMap<u64, u8>,
}

impl ReadAt for Overlay<'_> {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.base.read_at(off, buf)?;
        for (&p, &v) in self.patches.range(off..off + buf.len() as u64) {
            buf[(p - off) as usize] = v;
        }
        Ok(())
    }
}

/// Byte ranges worth corrupting: boot sector, record 0 and its extension
/// records, the attribute list, and the first records.
fn hot_ranges(g: &Geometry) -> Vec<(u64, u64)> {
    let cs = g.cluster as u64;
    let rs = g.record as u64;
    let mft = g.runs[0].1 * cs;
    vec![
        (0, 512),
        (mft, rs),
        (mft + 16 * rs, 2 * rs),
        (LIST_LCN * cs, 256),
        (mft, 64 * rs.min(cs * g.runs[0].0 / 64)),
    ]
}

fn scan_once(vol: &dyn ReadAt, rng: &mut Rng) -> io::Result<ringscan_ntfs::MftScan> {
    let opts = MftOptions {
        threads: 1 + rng.below(3) as usize,
        chunk_size: [0, 1, 200_000][rng.below(3) as usize],
        readers: rng.below(4) as usize,
    };
    let t = Instant::now();
    let r = scan_mft(vol, &opts, &MftProgress::new(), &AtomicBool::new(false));
    assert!(
        t.elapsed() < Duration::from_secs(20),
        "scan took {:?}",
        t.elapsed()
    );
    if let Ok(s) = &r {
        // Output guarantees hold even on garbage.
        assert!(s.files.iter().any(|f| f.record == 5 && f.parent == 5));
        assert!(s.files.windows(2).all(|w| w[0].record < w[1].record));
    }
    r
}

#[test]
fn fuzz_volumes() {
    let mut rng = Rng(0xD15C_0000);
    let mut ok = 0u32;
    let mut err = 0u32;
    let t = Instant::now();
    for g in &GEOMETRIES {
        let base = build(g).bytes;
        let ranges = hot_ranges(g);
        for _ in 0..1500 {
            let mut img = base.clone();
            let (start, len) = ranges[rng.below(ranges.len() as u64) as usize];
            let end = (start + len).min(img.len() as u64);
            mutate(&mut rng, &mut img[start as usize..end as usize]);
            if rng.below(4) == 0 {
                let (s2, l2) = ranges[rng.below(ranges.len() as u64) as usize];
                let e2 = (s2 + l2).min(img.len() as u64);
                mutate(&mut rng, &mut img[s2 as usize..e2 as usize]);
            }
            match scan_once(&img, &mut rng) {
                Ok(_) => ok += 1,
                Err(_) => err += 1,
            }
        }
    }
    eprintln!(
        "synthetic volumes: {ok} scans ok, {err} rejected, {:?}",
        t.elapsed()
    );
    assert!(ok > 1000 && err > 100);

    // Real images through a copy-on-write overlay (fewer iterations: each
    // scan reads the whole MFT).
    for (path, _) in fixtures() {
        let f = std::fs::File::open(&path).unwrap();
        let vi = ringscan_ntfs::read_volume_info(&f).unwrap();
        if vi.volume_size() > 1 << 30 {
            continue;
        }
        let rs = vi.record_size as u64;
        let mft = vi.mft_lcn * vi.bytes_per_cluster as u64;
        let ranges = [
            (0u64, 512u64),
            (mft, rs),
            (mft + 15 * rs, 9 * rs),
            (mft, 200 * rs),
        ];
        let (mut ok, mut err) = (0, 0);
        let t = Instant::now();
        for _ in 0..60 {
            let mut patches = BTreeMap::new();
            for _ in 0..1 + rng.below(3) {
                let (start, len) = ranges[rng.below(ranges.len() as u64) as usize];
                let mut chunk = vec![0u8; len as usize];
                f.read_at(start, &mut chunk).unwrap();
                mutate(&mut rng, &mut chunk);
                for (i, b) in chunk.into_iter().enumerate() {
                    patches.insert(start + i as u64, b);
                }
            }
            match scan_once(&Overlay { base: &f, patches }, &mut rng) {
                Ok(_) => ok += 1,
                Err(_) => err += 1,
            }
        }
        eprintln!(
            "{}: {ok} scans ok, {err} rejected, {:?}",
            path.display(),
            t.elapsed()
        );
    }
}
