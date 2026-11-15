//! Shared test helpers: an in-memory NTFS image builder (for tests that must
//! run without the generated fixtures), an alignment-checking `ReadAt`
//! wrapper, fixture discovery and a small deterministic RNG.
#![allow(dead_code)]

use ringscan_ntfs::ReadAt;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

// ---------------------------------------------------------------- fixtures

/// `<workspace>/fixtures`, or `$RINGSCAN_FIXTURES` (lets the Windows test
/// binary read the images from WSL: `RINGSCAN_FIXTURES=$PWD/fixtures
/// WSLENV=RINGSCAN_FIXTURES/p cargo test --target x86_64-pc-windows-gnu ...`).
pub fn fixtures_dir() -> PathBuf {
    match std::env::var_os("RINGSCAN_FIXTURES") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures"),
    }
}

/// All `(image, ground truth)` pairs, or a printed hint when there are none.
pub fn fixtures() -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(fixtures_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "img") {
                let json = p.with_extension("json");
                if json.exists() {
                    out.push((p, json));
                }
            }
        }
    }
    out.sort();
    if out.is_empty() {
        eprintln!(
            "NOTE: no NTFS fixtures found in {} - run scripts/make-ntfs-fixtures.sh (as root) to generate them; \
             fixture-based checks are skipped",
            fixtures_dir().display()
        );
    }
    out
}

// --------------------------------------------------------- read checking

/// Wraps a volume and fails the test if any read violates the I/O contract:
/// 4096-aligned offset and buffer address, length a multiple of 4096 (or of
/// the sector size when the read ends exactly at the volume end), nothing at
/// or past the volume end.
pub struct AlignChecker<'a> {
    pub inner: &'a dyn ReadAt,
    pub vol_end: u64,
    pub sector: u64,
    pub log: Mutex<Vec<(u64, usize)>>,
}

impl<'a> AlignChecker<'a> {
    pub fn new(inner: &'a dyn ReadAt, vol_end: u64, sector: u64) -> Self {
        AlignChecker {
            inner,
            vol_end,
            sector,
            log: Mutex::new(Vec::new()),
        }
    }
}

impl ReadAt for AlignChecker<'_> {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let len = buf.len() as u64;
        assert_eq!(off % 4096, 0, "unaligned read offset {off}");
        assert_eq!(
            buf.as_ptr() as usize % 4096,
            0,
            "unaligned buffer address for read at {off}"
        );
        assert!(len > 0, "empty read at {off}");
        // The boot sector read happens before the volume size is known.
        if off != 0 || len != 4096 {
            assert!(
                off + len <= self.vol_end,
                "read {off}+{len} past the volume end {}",
                self.vol_end
            );
            assert!(
                len.is_multiple_of(4096)
                    || (off + len == self.vol_end && len.is_multiple_of(self.sector)),
                "read length {len} at {off} is not 4096-aligned"
            );
        }
        assert!(
            len <= ringscan_ntfs::MAX_READ as u64,
            "read of {len} bytes exceeds MAX_READ"
        );
        self.log.lock().unwrap().push((off, buf.len()));
        self.inner.read_at(off, buf)
    }
}

// --------------------------------------------------------------------- rng

/// SplitMix64: tiny, deterministic, good enough for fuzzing.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

// ----------------------------------------------------------- image builder

pub const AT_SI: u32 = 0x10;
pub const AT_LIST: u32 = 0x20;
pub const AT_FN: u32 = 0x30;
pub const AT_DATA: u32 = 0x80;
pub const AT_INDEX_ROOT: u32 = 0x90;
pub const AT_INDEX_ALLOC: u32 = 0xA0;
pub const AT_REPARSE: u32 = 0xC0;

pub const NS_POSIX: u8 = 0;
pub const NS_WIN32: u8 = 1;
pub const NS_DOS: u8 = 2;
pub const NS_WIN32_DOS: u8 = 3;

pub fn mref(rec: u64, seq: u16) -> u64 {
    rec | (seq as u64) << 48
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

fn pad8(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(8) {
        v.push(0);
    }
}

/// `$STANDARD_INFORMATION` value (NTFS 3.x layout, 0x48 bytes).
pub fn si_value(mtime_unix: i64, attrs: u32) -> Vec<u8> {
    let ft = ((mtime_unix + 11_644_473_600) * 10_000_000) as u64;
    let mut v = vec![0u8; 0x48];
    for off in [0x00, 0x08, 0x10, 0x18] {
        v[off..off + 8].copy_from_slice(&ft.to_le_bytes());
    }
    v[0x20..0x24].copy_from_slice(&attrs.to_le_bytes());
    v
}

/// `$FILE_NAME` value.
pub fn fn_value(parent: u64, name: &str, ns: u8) -> Vec<u8> {
    let n = utf16(name);
    let mut v = vec![0u8; 0x42];
    v[0..8].copy_from_slice(&parent.to_le_bytes());
    v[0x40] = (n.len() / 2) as u8;
    v[0x41] = ns;
    v.extend_from_slice(&n);
    v
}

/// Resident attribute record.
pub fn resident(ty: u32, name: &str, value: &[u8]) -> Vec<u8> {
    let n = utf16(name);
    let name_off = 0x18;
    let mut value_off = name_off + n.len();
    value_off = value_off.div_ceil(8) * 8;
    let mut a = vec![0u8; value_off];
    a[0..4].copy_from_slice(&ty.to_le_bytes());
    a[8] = 0;
    a[9] = (n.len() / 2) as u8;
    a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
    a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
    a[0x14..0x16].copy_from_slice(&(value_off as u16).to_le_bytes());
    a[name_off..name_off + n.len()].copy_from_slice(&n);
    a.extend_from_slice(value);
    pad8(&mut a);
    let len = a.len() as u32;
    a[4..8].copy_from_slice(&len.to_le_bytes());
    a
}

/// Non-resident attribute sizes.
#[derive(Clone, Copy, Default)]
pub struct Sizes {
    pub alloc: u64,
    pub data: u64,
    pub init: u64,
    /// Present only when compressed/sparse flags are set.
    pub compressed: Option<u64>,
}

/// Non-resident attribute record for VCNs `lowest..=highest`.
pub fn nonresident(
    ty: u32,
    name: &str,
    flags: u16,
    lowest: u64,
    highest: u64,
    mp: &[u8],
    s: Sizes,
) -> Vec<u8> {
    let n = utf16(name);
    let hdr = if s.compressed.is_some() { 0x48 } else { 0x40 };
    let name_off = hdr;
    let mp_off = (name_off + n.len()).div_ceil(8) * 8;
    let mut a = vec![0u8; mp_off];
    a[0..4].copy_from_slice(&ty.to_le_bytes());
    a[8] = 1;
    a[9] = (n.len() / 2) as u8;
    a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
    a[0x0C..0x0E].copy_from_slice(&flags.to_le_bytes());
    a[0x10..0x18].copy_from_slice(&lowest.to_le_bytes());
    a[0x18..0x20].copy_from_slice(&highest.to_le_bytes());
    a[0x20..0x22].copy_from_slice(&(mp_off as u16).to_le_bytes());
    if lowest == 0 {
        a[0x28..0x30].copy_from_slice(&s.alloc.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&s.data.to_le_bytes());
        a[0x38..0x40].copy_from_slice(&s.init.to_le_bytes());
        if let Some(c) = s.compressed {
            a[0x40..0x48].copy_from_slice(&c.to_le_bytes());
        }
    }
    a[name_off..name_off + n.len()].copy_from_slice(&n);
    a.extend_from_slice(mp);
    pad8(&mut a);
    let len = a.len() as u32;
    a[4..8].copy_from_slice(&len.to_le_bytes());
    a
}

/// Mapping pairs for `(length, lcn)` runs (`None` = sparse).
pub fn mapping_pairs(runs: &[(u64, Option<u64>)]) -> Vec<u8> {
    fn signed_bytes(v: i64) -> Vec<u8> {
        let mut n = 1;
        while n < 8 {
            let lo = -(1i64 << (8 * n - 1));
            let hi = (1i64 << (8 * n - 1)) - 1;
            if v >= lo && v <= hi {
                break;
            }
            n += 1;
        }
        v.to_le_bytes()[..n].to_vec()
    }
    let mut out = Vec::new();
    let mut prev = 0i64;
    for &(len, lcn) in runs {
        let l = signed_bytes(len as i64);
        let o = match lcn {
            Some(lcn) => {
                let d = signed_bytes(lcn as i64 - prev);
                prev = lcn as i64;
                d
            }
            None => Vec::new(),
        };
        out.push((o.len() as u8) << 4 | l.len() as u8);
        out.extend_from_slice(&l);
        out.extend_from_slice(&o);
    }
    out.push(0);
    out
}

/// One `$ATTRIBUTE_LIST` entry.
pub fn list_entry(ty: u32, lowest: u64, rec: u64, id: u16) -> Vec<u8> {
    let mut e = vec![0u8; 0x20];
    e[0..4].copy_from_slice(&ty.to_le_bytes());
    e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
    e[6] = 0;
    e[7] = 0x1A;
    e[8..16].copy_from_slice(&lowest.to_le_bytes());
    e[16..24].copy_from_slice(&rec.to_le_bytes());
    e[24..26].copy_from_slice(&id.to_le_bytes());
    e
}

pub const REC_IN_USE: u16 = 1;
pub const REC_DIR: u16 = 2;

/// A FILE record of `size` bytes with fixups applied (512-byte stride).
pub fn file_record(
    size: usize,
    recno: u64,
    seq: u16,
    flags: u16,
    base: u64,
    attrs: &[Vec<u8>],
) -> Vec<u8> {
    let mut r = vec![0u8; size];
    r[0..4].copy_from_slice(b"FILE");
    let usa_ofs = 0x30u16;
    let usa_count = (size / 512 + 1) as u16;
    r[4..6].copy_from_slice(&usa_ofs.to_le_bytes());
    r[6..8].copy_from_slice(&usa_count.to_le_bytes());
    r[0x10..0x12].copy_from_slice(&seq.to_le_bytes());
    r[0x12..0x14].copy_from_slice(&1u16.to_le_bytes());
    let first = (usa_ofs as usize + 2 * usa_count as usize).div_ceil(8) * 8;
    r[0x14..0x16].copy_from_slice(&(first as u16).to_le_bytes());
    r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
    r[0x1C..0x20].copy_from_slice(&(size as u32).to_le_bytes());
    r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
    r[0x2C..0x30].copy_from_slice(&(recno as u32).to_le_bytes());
    let mut pos = first;
    for a in attrs {
        assert!(pos + a.len() + 8 <= size - 2, "record {recno} overflows");
        r[pos..pos + a.len()].copy_from_slice(a);
        pos += a.len();
    }
    r[pos..pos + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    pos += 8;
    r[0x18..0x1C].copy_from_slice(&(pos as u32).to_le_bytes());
    protect(&mut r, usa_ofs as usize, 0x0101 + recno as u16);
    r
}

/// Applies multi-sector protection: saves the last u16 of each 512-byte
/// block into the update sequence array and replaces it with `usn`.
pub fn protect(r: &mut [u8], usa_ofs: usize, usn: u16) {
    let blocks = r.len() / 512;
    r[usa_ofs..usa_ofs + 2].copy_from_slice(&usn.to_le_bytes());
    for i in 1..=blocks {
        let p = i * 512 - 2;
        let (a, b) = (r[p], r[p + 1]);
        r[usa_ofs + 2 * i] = a;
        r[usa_ofs + 2 * i + 1] = b;
        r[p..p + 2].copy_from_slice(&usn.to_le_bytes());
    }
}

/// In-memory volume under construction.
pub struct Image {
    pub bytes: Vec<u8>,
    pub sector: u32,
    pub cluster: u32,
    pub record_size: u32,
    /// $MFT runs as (length in clusters, lcn).
    pub mft_runs: Vec<(u64, u64)>,
    /// Records written so far.
    pub written: Vec<u64>,
}

impl Image {
    /// Volume of `clusters` clusters whose MFT lives in `mft_runs`.
    pub fn new(
        sector: u32,
        cluster: u32,
        record_size: u32,
        clusters: u64,
        mft_runs: Vec<(u64, u64)>,
    ) -> Image {
        // One extra sector at the end, like mkntfs (backup boot sector).
        let size = clusters * cluster as u64 + sector as u64;
        let mut bytes = vec![0u8; size as usize];
        let b = &mut bytes[..512];
        b[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&(sector as u16).to_le_bytes());
        let spc = cluster / sector;
        b[0x0D] = spc as u8;
        b[0x15] = 0xF8;
        let total_sectors = clusters * spc as u64;
        b[0x28..0x30].copy_from_slice(&total_sectors.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&mft_runs[0].1.to_le_bytes());
        b[0x38..0x40].copy_from_slice(&(clusters / 2).to_le_bytes());
        b[0x40] = if record_size >= cluster {
            (record_size / cluster) as u8
        } else {
            (-(record_size.trailing_zeros() as i8)) as u8
        };
        b[0x44] = 1;
        b[0x48..0x50].copy_from_slice(&0x0123_4567_89AB_CDEFu64.to_le_bytes());
        b[0x1FE] = 0x55;
        b[0x1FF] = 0xAA;
        Image {
            bytes,
            sector,
            cluster,
            record_size,
            mft_runs,
            written: Vec::new(),
        }
    }

    pub fn mft_records(&self) -> u64 {
        let clusters: u64 = self.mft_runs.iter().map(|r| r.0).sum();
        clusters * self.cluster as u64 / self.record_size as u64
    }

    /// Writes record `recno` through the MFT runlist (it may span runs).
    pub fn put_record(&mut self, recno: u64, rec: &[u8]) {
        assert_eq!(rec.len(), self.record_size as usize);
        let cs = self.cluster as u64;
        let mut mft_off = recno * self.record_size as u64;
        let mut src = 0usize;
        let mut vcn_base = 0u64;
        for &(len, lcn) in &self.mft_runs {
            let run_bytes = len * cs;
            let run_start = vcn_base * cs;
            while src < rec.len() && mft_off >= run_start && mft_off < run_start + run_bytes {
                let phys = lcn * cs + (mft_off - run_start);
                let n = ((run_start + run_bytes - mft_off) as usize).min(rec.len() - src);
                self.bytes[phys as usize..phys as usize + n].copy_from_slice(&rec[src..src + n]);
                src += n;
                mft_off += n as u64;
            }
            vcn_base += len;
        }
        assert_eq!(src, rec.len(), "record {recno} not mapped by the MFT runs");
        self.written.push(recno);
    }

    /// Fills every slot not written yet with a formatted, not-in-use record.
    pub fn format_free_records(&mut self) {
        for r in 0..self.mft_records() {
            if !self.written.contains(&r) {
                let rec = file_record(self.record_size as usize, r, 1, 0, 0, &[]);
                self.put_record(r, &rec);
            }
        }
    }
}

// ------------------------------------------------- the synthetic volume

/// MFT layouts of the synthetic test volume.
pub struct Geometry {
    pub sector: u32,
    pub cluster: u32,
    pub record: u32,
    /// $MFT runs (clusters, lcn): 64 records in total, extension records 16
    /// and 17 inside the first run.
    pub runs: [(u64, u64); 3],
    pub nonresident_list: bool,
}

pub const GEOMETRIES: [Geometry; 3] = [
    // 512-byte clusters: runs start at odd clusters (not 4 KiB aligned) and
    // record 20 straddles runs 1 and 2.
    Geometry {
        sector: 512,
        cluster: 512,
        record: 1024,
        runs: [(41, 37), (49, 301), (38, 777)],
        nonresident_list: false,
    },
    // 4Kn disk: 4 KiB sectors, clusters and records.
    Geometry {
        sector: 4096,
        cluster: 4096,
        record: 4096,
        runs: [(20, 37), (24, 301), (20, 777)],
        nonresident_list: true,
    },
    // 1 KiB clusters with 4 KiB records: 4 clusters per record, half a
    // record at the end of run 1.
    Geometry {
        sector: 512,
        cluster: 1024,
        record: 4096,
        runs: [(82, 37), (98, 301), (76, 777)],
        nonresident_list: true,
    },
];

pub const CLUSTERS: u64 = 2048;
pub const LIST_LCN: u64 = 1200;
pub const BAD_LCN: u64 = 1500;
pub const MTIME: i64 = 1_700_000_000;

pub fn build(g: &Geometry) -> Image {
    let rs = g.record as usize;
    let cs = g.cluster as u64;
    let mut img = Image::new(
        g.sector,
        g.cluster,
        g.record,
        CLUSTERS,
        g.runs.map(|r| (r.0, r.1)).to_vec(),
    );
    let n_rec = img.mft_records();
    assert_eq!(n_rec, 64);
    let mft_bytes = n_rec * rs as u64;
    let root = mref(5, 5);
    let si = |attrs: u32| resident(AT_SI, "", &si_value(MTIME, attrs));
    let name = |parent: u64, n: &str, ns: u8| resident(AT_FN, "", &fn_value(parent, n, ns));
    let put = |img: &mut Image, recno: u64, seq: u16, flags: u16, base: u64, attrs: &[Vec<u8>]| {
        img.put_record(recno, &file_record(rs, recno, seq, flags, base, attrs));
    };
    let (r0, r1, r2) = (g.runs[0], g.runs[1], g.runs[2]);
    let vcn1 = r0.0;
    let vcn2 = r0.0 + r1.0;
    let last = vcn2 + r2.0 - 1;

    // Record 0: $MFT with an attribute list; $DATA extents in records 16, 17.
    let mut list = Vec::new();
    list.extend(list_entry(AT_SI, 0, mref(0, 1), 0));
    list.extend(list_entry(AT_FN, 0, mref(0, 1), 1));
    list.extend(list_entry(AT_DATA, 0, mref(0, 1), 2));
    list.extend(list_entry(AT_DATA, vcn1, mref(16, 1), 0));
    list.extend(list_entry(AT_DATA, vcn2, mref(17, 1), 0));
    let list_attr = if g.nonresident_list {
        // Stored in one cluster at LIST_LCN.
        let at = (LIST_LCN * cs) as usize;
        img.bytes[at..at + list.len()].copy_from_slice(&list);
        let s = Sizes {
            alloc: cs,
            data: list.len() as u64,
            init: list.len() as u64,
            compressed: None,
        };
        nonresident(
            AT_LIST,
            "",
            0,
            0,
            0,
            &mapping_pairs(&[(1, Some(LIST_LCN))]),
            s,
        )
    } else {
        resident(AT_LIST, "", &list)
    };
    let mft_sizes = Sizes {
        alloc: mft_bytes,
        data: mft_bytes,
        init: mft_bytes,
        compressed: None,
    };
    put(
        &mut img,
        0,
        1,
        REC_IN_USE,
        0,
        &[
            si(0x06),
            list_attr,
            name(root, "$MFT", NS_WIN32_DOS),
            nonresident(
                AT_DATA,
                "",
                0,
                0,
                vcn1 - 1,
                &mapping_pairs(&[(r0.0, Some(r0.1))]),
                mft_sizes,
            ),
        ],
    );
    put(
        &mut img,
        16,
        1,
        REC_IN_USE,
        mref(0, 1),
        &[nonresident(
            AT_DATA,
            "",
            0,
            vcn1,
            vcn2 - 1,
            &mapping_pairs(&[(r1.0, Some(r1.1))]),
            Sizes::default(),
        )],
    );
    put(
        &mut img,
        17,
        1,
        REC_IN_USE,
        mref(0, 1),
        &[nonresident(
            AT_DATA,
            "",
            0,
            vcn2,
            last,
            &mapping_pairs(&[(r2.0, Some(r2.1))]),
            Sizes::default(),
        )],
    );

    // Root directory with an index allocation.
    let ia = Sizes {
        alloc: 4096,
        data: 4096,
        init: 4096,
        compressed: None,
    };
    put(
        &mut img,
        5,
        5,
        REC_IN_USE | REC_DIR,
        0,
        &[
            si(0x06),
            name(root, ".", NS_WIN32_DOS),
            resident(AT_INDEX_ROOT, "$I30", &[0u8; 32]),
            nonresident(
                AT_INDEX_ALLOC,
                "$I30",
                0,
                0,
                0,
                &mapping_pairs(&[(1, Some(1300))]),
                ia,
            ),
        ],
    );

    // $BadClus: volume-sized $Bad stream, all holes but 2 bad clusters.
    let vol_bytes = CLUSTERS * cs;
    let bad_runs = mapping_pairs(&[(100, None), (2, Some(BAD_LCN)), (CLUSTERS - 102, None)]);
    let bad = Sizes {
        alloc: vol_bytes,
        data: vol_bytes,
        init: 0,
        compressed: None,
    };
    put(
        &mut img,
        8,
        8,
        REC_IN_USE,
        0,
        &[
            si(0x06),
            name(root, "$BadClus", NS_WIN32_DOS),
            resident(AT_DATA, "", &[]),
            nonresident(AT_DATA, "$Bad", 0, 0, CLUSTERS - 1, &bad_runs, bad),
        ],
    );

    // Record 20 straddles MFT runs 1 and 2 (in the 512/1024 geometries).
    put(
        &mut img,
        20,
        3,
        REC_IN_USE | REC_DIR,
        0,
        &[si(0x10), name(root, "docs", NS_WIN32)],
    );
    put(
        &mut img,
        21,
        1,
        REC_IN_USE,
        0,
        &[
            si(0x20),
            name(mref(20, 3), "report.txt", NS_WIN32_DOS),
            resident(AT_DATA, "", &[b'r'; 100]),
        ],
    );
    let data = Sizes {
        alloc: 8192,
        data: 5000,
        init: 5000,
        compressed: None,
    };
    let ads = Sizes {
        alloc: 3072,
        data: 3000,
        init: 3000,
        compressed: None,
    };
    put(
        &mut img,
        22,
        1,
        REC_IN_USE,
        0,
        &[
            si(0x20),
            name(mref(20, 3), "big file.bin", NS_WIN32),
            name(mref(20, 3), "BIGFIL~1.BIN", NS_DOS),
            nonresident(
                AT_DATA,
                "",
                0,
                0,
                1,
                &mapping_pairs(&[(2, Some(1400))]),
                data,
            ),
            resident(AT_DATA, "meta", &[1u8; 10]),
            nonresident(
                AT_DATA,
                "nonres",
                0,
                0,
                0,
                &mapping_pairs(&[(1, Some(1410))]),
                ads,
            ),
        ],
    );
    // Hard links: POSIX link in docs, Win32 link in root (becomes primary),
    // and a link whose parent reference is stale (dropped).
    put(
        &mut img,
        23,
        1,
        REC_IN_USE,
        0,
        &[
            si(0x20),
            name(mref(20, 3), "link-a", NS_POSIX),
            name(root, "link-b", NS_WIN32),
            name(mref(20, 2), "link-c", NS_POSIX),
            resident(AT_DATA, "", b"linked"),
        ],
    );
    let sparse = Sizes {
        alloc: 1 << 20,
        data: 1 << 20,
        init: 1 << 20,
        compressed: Some(4096),
    };
    put(
        &mut img,
        24,
        1,
        REC_IN_USE,
        0,
        &[
            si(0x220),
            name(root, "sparse.vhd", NS_WIN32),
            nonresident(
                AT_DATA,
                "",
                0x8000,
                0,
                255,
                &mapping_pairs(&[(255, None), (1, Some(1420))]),
                sparse,
            ),
        ],
    );
    let packed = Sizes {
        alloc: 65536,
        data: 60000,
        init: 60000,
        compressed: Some(8192),
    };
    put(
        &mut img,
        25,
        1,
        REC_IN_USE,
        0,
        &[
            si(0x820),
            name(root, "packed.dat", NS_WIN32),
            nonresident(
                AT_DATA,
                "",
                0x0001,
                0,
                15,
                &mapping_pairs(&[(2, Some(1430)), (14, None)]),
                packed,
            ),
        ],
    );
    // Stale parent reference: docs (record 20) is at sequence 3 now.
    put(
        &mut img,
        30,
        1,
        REC_IN_USE,
        0,
        &[si(0x20), name(mref(20, 2), "stale.txt", NS_WIN32)],
    );
    // Deleted file (not in use) - must not appear.
    put(
        &mut img,
        31,
        4,
        0,
        0,
        &[si(0x20), name(root, "deleted.txt", NS_WIN32)],
    );
    // Base record whose name and data live in extension records.
    put(
        &mut img,
        32,
        1,
        REC_IN_USE,
        0,
        &[si(0x20), resident(AT_LIST, "", &[0u8; 0x20])],
    );
    put(
        &mut img,
        45,
        1,
        REC_IN_USE,
        mref(32, 1),
        &[name(root, "fragmented.bin", NS_WIN32)],
    );
    let frag = Sizes {
        alloc: 40960,
        data: 40000,
        init: 40000,
        compressed: None,
    };
    let runs: Vec<(u64, Option<u64>)> = (0..10).map(|i| (1, Some(1440 + 2 * i))).collect();
    put(
        &mut img,
        46,
        1,
        REC_IN_USE,
        mref(32, 1),
        &[nonresident(
            AT_DATA,
            "",
            0,
            0,
            9,
            &mapping_pairs(&runs),
            frag,
        )],
    );
    put(
        &mut img,
        50,
        1,
        REC_IN_USE,
        mref(32, 1),
        &[nonresident(
            AT_DATA,
            "",
            0,
            10,
            19,
            &mapping_pairs(&[(10, Some(1470))]),
            Sizes {
                alloc: 999,
                data: 999,
                init: 999,
                compressed: None,
            },
        )],
    );
    // Extension record pointing at a reused base: ignored.
    put(
        &mut img,
        51,
        1,
        REC_IN_USE,
        mref(32, 9),
        &[resident(AT_DATA, "junk", &[0u8; 77])],
    );
    // Junction.
    let mut rp = 0xA000_0003u32.to_le_bytes().to_vec();
    rp.extend_from_slice(&[0u8; 12]);
    put(
        &mut img,
        33,
        1,
        REC_IN_USE | REC_DIR,
        0,
        &[
            si(0x10),
            name(root, "junction", NS_WIN32),
            resident(AT_REPARSE, "", &rp),
        ],
    );
    // Directory with an ADS and an index allocation.
    let dads = Sizes {
        alloc: 1024,
        data: 700,
        init: 700,
        compressed: None,
    };
    put(
        &mut img,
        34,
        1,
        REC_IN_USE | REC_DIR,
        0,
        &[
            si(0x10),
            name(root, "dir with stream", NS_WIN32),
            nonresident(
                AT_DATA,
                "meta",
                0,
                0,
                0,
                &mapping_pairs(&[(1, Some(1480))]),
                dads,
            ),
            nonresident(
                AT_INDEX_ALLOC,
                "$I30",
                0,
                0,
                0,
                &mapping_pairs(&[(1, Some(1481))]),
                ia,
            ),
        ],
    );
    // Header names another record number: misplaced, skipped.
    let mut wrong = file_record(
        rs,
        35,
        1,
        REC_IN_USE,
        0,
        &[si(0x20), name(root, "misplaced.txt", NS_WIN32)],
    );
    wrong[0x2C..0x30].copy_from_slice(&99u32.to_le_bytes());
    img.put_record(35, &wrong);
    // Torn write: one sector's check value does not match.
    let mut torn = file_record(
        rs,
        36,
        1,
        REC_IN_USE,
        0,
        &[si(0x20), name(root, "torn.txt", NS_WIN32)],
    );
    torn[1022] ^= 0xFF;
    img.put_record(36, &torn);
    // DOS-only name: not a file.
    put(
        &mut img,
        37,
        1,
        REC_IN_USE,
        0,
        &[si(0x20), name(root, "DOSONLY.TXT", NS_DOS)],
    );
    // A parent cycle, and a file below it.
    put(
        &mut img,
        38,
        1,
        REC_IN_USE | REC_DIR,
        0,
        &[si(0x10), name(mref(39, 1), "cycle-a", NS_WIN32)],
    );
    put(
        &mut img,
        39,
        1,
        REC_IN_USE | REC_DIR,
        0,
        &[si(0x10), name(mref(38, 1), "cycle-b", NS_WIN32)],
    );
    put(
        &mut img,
        40,
        1,
        REC_IN_USE,
        0,
        &[si(0x20), name(mref(38, 1), "in-cycle.txt", NS_WIN32)],
    );
    // WOF-compressed system file: sparse unnamed stream + WofCompressedData.
    let wof_main = Sizes {
        alloc: 102400,
        data: 100000,
        init: 100000,
        compressed: Some(0),
    };
    let wof_data = Sizes {
        alloc: 30720,
        data: 30000,
        init: 30000,
        compressed: None,
    };
    let mut wof_rp = ringscan_ntfs::reparse::WOF.to_le_bytes().to_vec();
    wof_rp.extend_from_slice(&[0u8; 20]);
    put(
        &mut img,
        41,
        1,
        REC_IN_USE,
        0,
        &[
            si(0x20),
            name(root, "wof.dll", NS_WIN32_DOS),
            nonresident(
                AT_DATA,
                "",
                0x8000,
                0,
                24,
                &mapping_pairs(&[(25, None)]),
                wof_main,
            ),
            nonresident(
                AT_DATA,
                "WofCompressedData",
                0,
                0,
                7,
                &mapping_pairs(&[(8, Some(1490))]),
                wof_data,
            ),
            resident(AT_REPARSE, "", &wof_rp),
        ],
    );
    img.format_free_records();
    img
}
