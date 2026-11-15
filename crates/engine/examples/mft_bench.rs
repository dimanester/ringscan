//! MFT path benchmark at system-drive scale, without elevation: writes a
//! synthetic NTFS image shaped like a real Windows system drive, then reads
//! it with `ringscan_ntfs::scan_mft` and builds the tree with
//! [`ringscan_engine::build_tree`], exactly like an MFT scan of a drive,
//! printing time and memory after each phase.
//!
//! ```text
//! cargo run --release --target x86_64-pc-windows-gnu -p ringscan-engine --example mft_bench -- \
//!     gen <image> [--files N]     # default 1,080,000 files (+29% dirs, +8% extra hard links)
//! cargo run --release --target x86_64-pc-windows-gnu -p ringscan-engine --example mft_bench -- \
//!     scan <image> [--threads N]  # default 8, like the scan job
//! ```
//!
//! The default shape follows a measured Windows 11 C: (1.08 M files,
//! 315 K directories, 90 K extra hard links, 3.15 M MFT record slots,
//! file names of 29 characters on average). Record slots that hold no file
//! are left zeroed (never-initialised records), so the image file is
//! `slots * 1 KiB` (about 3.2 GB by default); delete it afterwards.
//!
//! "private" is the process's committed private memory (Windows
//! `PagefileUsage`; Linux: resident set), "peak" its high-water mark.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use ringscan_core::synth::{Rng, dir_name, file_name};
use ringscan_ntfs::{MftOptions, MftProgress, scan_mft};

const RECORD: usize = 1024;
const CLUSTER: u64 = 4096;
const MFT_LCN: u64 = 4;
const ROOT: u64 = 5;
const FIRST_USER_RECORD: u64 = 24;

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str, default: usize| -> usize {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.replace('_', "").parse().ok())
            .unwrap_or(default)
    };
    match (args.first().map(String::as_str), args.get(1)) {
        (Some("gen"), Some(path)) => generate(path, opt("--files", 1_080_000)),
        (Some("scan"), Some(path)) => scan(path, opt("--threads", 8)),
        _ => {
            eprintln!("usage: mft_bench gen <image> [--files N] | scan <image> [--threads N]");
            std::process::exit(2);
        }
    }
}

// ------------------------------------------------------------------ scan

fn scan(path: &str, threads: usize) -> io::Result<()> {
    let image = File::open(path)?;
    report("start", None);
    let t = Instant::now();
    let opts = MftOptions {
        threads,
        ..MftOptions::default()
    };
    let scan = scan_mft(&image, &opts, &MftProgress::new(), &AtomicBool::new(false))?;
    let files = scan.files.len();
    let links: usize = scan.files.iter().map(|f| f.extra_links.len()).sum();
    report(
        &format!(
            "read: {} slots, {files} entries, {links} extra links, bitmap in use {:?}, parse {:.0} ms cpu, merge {:.0} ms",
            scan.stats.records,
            scan.stats.bitmap_in_use,
            scan.stats.parse_time.as_secs_f64() * 1e3,
            scan.stats.merge_time.as_secs_f64() * 1e3
        ),
        Some(t),
    );
    let t = Instant::now();
    let build = ringscan_engine::build_tree(scan.files, "C:\\");
    report(
        &format!(
            "build: {} nodes, tree {:.1} MiB",
            build.tree.live_len(),
            mib(build.tree.memory_bytes() as u64)
        ),
        Some(t),
    );
    drop(build);
    report("end", None);
    Ok(())
}

fn mib(b: u64) -> f64 {
    b as f64 / (1u64 << 20) as f64
}

fn report(what: &str, since: Option<Instant>) {
    let (now, peak) = memory();
    let time = since.map_or(String::new(), |t| {
        format!("{:>7.0} ms  ", t.elapsed().as_secs_f64() * 1e3)
    });
    println!(
        "{time}private {:>7.1} MiB  peak {:>7.1} MiB  {what}",
        mib(now),
        mib(peak)
    );
}

#[cfg(windows)]
fn memory() -> (u64, u64) {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain-data struct; zero is a valid bit pattern.
    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    c.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: the current-process pseudo handle and a correctly sized struct.
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } == 0 {
        return (0, 0);
    }
    (c.PagefileUsage as u64, c.PeakPagefileUsage as u64)
}

#[cfg(not(windows))]
fn memory() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let kib = |key: &str| -> u64 {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap_or(0)
            * 1024
    };
    (kib("VmRSS:"), kib("VmHWM:"))
}

// -------------------------------------------------------------- generate

/// One synthetic entry: its directory, names and size.
struct Entry {
    is_dir: bool,
    names: Vec<(u32, String)>,
    logical: u64,
    resident: bool,
}

fn generate(path: &str, files: usize) -> io::Result<()> {
    let t = Instant::now();
    let mut rng = Rng::new(0x5EED);
    let dirs = files * 29 / 100;
    let extra_links = files * 83 / 1000;
    let entries = dirs + files;
    // Real system drives keep many free record slots (2.25 slots per entry).
    let slots = FIRST_USER_RECORD + (entries as u64 * 9 / 4);

    // Directory tree: index 0 is the root; parents come first and are
    // mostly recent, which gives realistic depth (10-20 levels).
    let mut list: Vec<Entry> = Vec::with_capacity(entries + 1);
    list.push(Entry {
        is_dir: true,
        names: Vec::new(),
        logical: 0,
        resident: false,
    });
    for i in 1..=dirs {
        let parent = if rng.chance(0.3) {
            rng.below(i as u64)
        } else {
            i as u64 - 1 - rng.below((i as u64).min(40))
        } as u32;
        let name = dir_name(&mut rng);
        let name = long_name(&mut rng, name, 34);
        list.push(Entry {
            is_dir: true,
            names: vec![(parent, name)],
            logical: 0,
            resident: false,
        });
    }
    for _ in 0..files {
        let parent = 1 + rng.below(dirs as u64) as u32;
        let name = file_name(&mut rng);
        let name = long_name(&mut rng, name, 29);
        let logical = rng.log_normal(9.0, 2.5) as u64;
        list.push(Entry {
            is_dir: false,
            names: vec![(parent, name)],
            logical,
            resident: logical < 600,
        });
    }
    for _ in 0..extra_links {
        let f = dirs + 1 + rng.below(files as u64) as usize;
        let parent = 1 + rng.below(dirs as u64) as u32;
        let name = list[f].names[0].1.clone();
        list[f].names.push((parent, name));
    }

    // Record numbers: entries spread over the slots in random order, so
    // children often come before their parents, as on a real volume.
    let mut record_of: Vec<u64> = (0..entries as u64)
        .map(|i| FIRST_USER_RECORD + i * (slots - FIRST_USER_RECORD) / entries as u64)
        .collect();
    for i in (1..record_of.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        record_of.swap(i, j);
    }
    let record = |i: u32| {
        if i == 0 {
            ROOT
        } else {
            record_of[i as usize - 1]
        }
    };
    let mut entry_at = vec![u32::MAX; slots as usize];
    for (i, &r) in record_of.iter().enumerate() {
        entry_at[r as usize] = i as u32 + 1;
    }

    let mft_bytes = slots * RECORD as u64;
    let mft_clusters = mft_bytes.div_ceil(CLUSTER);
    let mut out = BufWriter::with_capacity(4 << 20, File::create(path)?);
    let mut boot = vec![0u8; (MFT_LCN * CLUSTER) as usize];
    boot_sector(&mut boot[..512], mft_clusters);
    out.write_all(&boot)?;
    let zero = [0u8; RECORD];
    // $MFT:$BITMAP (in-use records) after the MFT (and its mirror cluster).
    let bitmap_bytes = slots.div_ceil(64) * 8;
    let mut bitmap = vec![0u8; bitmap_bytes.div_ceil(CLUSTER) as usize * CLUSTER as usize];
    for r in (0..slots).filter(|&r| r == 0 || r == ROOT || entry_at[r as usize] != u32::MAX) {
        bitmap[(r / 8) as usize] |= 1 << (r % 8);
    }
    let bitmap_lcn = MFT_LCN + mft_clusters + 8;
    let bitmap_clusters = (bitmap.len() as u64).div_ceil(CLUSTER);
    let mut data_lcn = bitmap_lcn + bitmap_clusters + 8;
    for r in 0..slots {
        let attrs: Vec<Vec<u8>> = match r {
            0 => vec![
                si(0x06),
                file_name_attr(ROOT, "$MFT"),
                nonresident(0x80, mft_clusters, MFT_LCN, mft_bytes),
                nonresident(0xB0, bitmap_clusters, bitmap_lcn, bitmap_bytes),
            ],
            ROOT => vec![si(0x16), file_name_attr(ROOT, ".")],
            _ => match entry_at[r as usize] {
                u32::MAX => {
                    out.write_all(&zero)?;
                    continue;
                }
                i => {
                    let e = &list[i as usize];
                    // A sixth of the files carry the on-disk EA bit, like
                    // catalog-signed system files.
                    let fa = if e.is_dir {
                        0x10
                    } else if r % 6 == 0 {
                        0x0004_0020
                    } else {
                        0x20
                    };
                    let mut a = vec![si(fa)];
                    for (p, n) in &e.names {
                        a.push(file_name_attr(record(*p), n));
                    }
                    if e.is_dir {
                        if r % 3 == 0 {
                            a.push(nonresident(0xA0, 1 + r % 4, data_lcn, 0));
                        }
                    } else if e.resident {
                        a.push(resident_data(e.logical as usize));
                    } else {
                        let clusters = e.logical.div_ceil(CLUSTER).max(1);
                        a.push(nonresident(0x80, clusters, data_lcn, e.logical));
                        data_lcn += clusters;
                    }
                    a
                }
            },
        };
        let flags = if r == ROOT
            || entry_at
                .get(r as usize)
                .is_some_and(|&i| i != u32::MAX && list[i as usize].is_dir)
        {
            3
        } else {
            1
        };
        let seq = if r == ROOT { 5 } else { 1 };
        out.write_all(&file_record(r, seq, flags, &attrs))?;
    }
    let written = (MFT_LCN * CLUSTER) + mft_bytes;
    io::copy(
        &mut io::repeat(0).take(bitmap_lcn * CLUSTER - written),
        &mut out,
    )?;
    out.write_all(&bitmap)?;
    out.flush()?;
    println!(
        "{path}: {files} files, {dirs} dirs, {extra_links} extra links, {slots} slots, {:.0} MiB in {:.1} s",
        mib(mft_bytes),
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Pads a generated name to the given mean length (real names on a
/// system drive are long: `amd64_microsoft-windows-..._31bf3856ad364e35_...`).
fn long_name(rng: &mut Rng, mut name: String, mean: usize) -> String {
    let target = (mean as f64 * (0.3 + 1.4 * rng.unit())) as usize;
    if name.len() < target {
        let ext = name
            .rfind('.')
            .map(|i| name.split_off(i))
            .unwrap_or_default();
        while name.len() + ext.len() < target {
            name.push(char::from(b"0123456789abcdef_"[rng.below(17) as usize]));
        }
        name.push_str(&ext);
    }
    name.truncate(200);
    name
}

fn boot_sector(b: &mut [u8], mft_clusters: u64) {
    b[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    b[3..11].copy_from_slice(b"NTFS    ");
    b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
    b[0x0D] = (CLUSTER / 512) as u8;
    b[0x15] = 0xF8;
    // A 2 TiB volume; only the MFT is ever read.
    let total_sectors: u64 = (2u64 << 40) / 512;
    b[0x28..0x30].copy_from_slice(&total_sectors.to_le_bytes());
    b[0x30..0x38].copy_from_slice(&MFT_LCN.to_le_bytes());
    b[0x38..0x40].copy_from_slice(&(MFT_LCN + mft_clusters + 1).to_le_bytes());
    b[0x40] = (-10i8) as u8; // 1 KiB records
    b[0x44] = 1;
    b[0x1FE] = 0x55;
    b[0x1FF] = 0xAA;
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn pad8(v: &mut Vec<u8>) {
    v.resize(v.len().div_ceil(8) * 8, 0);
}

fn resident(ty: u32, value: &[u8]) -> Vec<u8> {
    let mut a = vec![0u8; 0x18];
    a[0..4].copy_from_slice(&ty.to_le_bytes());
    a[0x0A..0x0C].copy_from_slice(&0x18u16.to_le_bytes());
    a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
    a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
    a.extend_from_slice(value);
    pad8(&mut a);
    let len = a.len() as u32;
    a[4..8].copy_from_slice(&len.to_le_bytes());
    a
}

/// `$STANDARD_INFORMATION` with a fixed mtime (2025-01-01).
fn si(attrs: u32) -> Vec<u8> {
    let ft: u64 = (1_735_689_600 + 11_644_473_600) * 10_000_000;
    let mut v = vec![0u8; 0x48];
    for off in [0x00, 0x08, 0x10, 0x18] {
        v[off..off + 8].copy_from_slice(&ft.to_le_bytes());
    }
    v[0x20..0x24].copy_from_slice(&attrs.to_le_bytes());
    resident(0x10, &v)
}

/// `$FILE_NAME` (Win32 namespace) below directory record `parent` (seq 1,
/// the root's is 5).
fn file_name_attr(parent: u64, name: &str) -> Vec<u8> {
    let seq: u64 = if parent == ROOT { 5 } else { 1 };
    let n = utf16(name);
    let mut v = vec![0u8; 0x42];
    v[0..8].copy_from_slice(&(parent | seq << 48).to_le_bytes());
    v[0x40] = (n.len() / 2) as u8;
    v[0x41] = 1;
    v.extend_from_slice(&n);
    resident(0x30, &v)
}

fn resident_data(len: usize) -> Vec<u8> {
    resident(0x80, &vec![b'x'; len])
}

/// Non-resident attribute of type `ty` with one run of `clusters` at `lcn`;
/// `data` bytes of data (0 = the whole allocation, for indexes).
fn nonresident(ty: u32, clusters: u64, lcn: u64, data: u64) -> Vec<u8> {
    let alloc = clusters * CLUSTER;
    let data = if data == 0 { alloc } else { data };
    let mut a = vec![0u8; 0x40];
    a[0..4].copy_from_slice(&ty.to_le_bytes());
    a[8] = 1;
    if ty == 0xA0 {
        // Named "$I30", like a directory index.
        let name = utf16("$I30");
        a[9] = 4;
        a[0x0A..0x0C].copy_from_slice(&0x40u16.to_le_bytes());
        a.extend_from_slice(&name);
        pad8(&mut a);
    }
    let mp_off = a.len() as u16;
    a[0x18..0x20].copy_from_slice(&(clusters - 1).to_le_bytes());
    a[0x20..0x22].copy_from_slice(&mp_off.to_le_bytes());
    a[0x28..0x30].copy_from_slice(&alloc.to_le_bytes());
    a[0x30..0x38].copy_from_slice(&data.to_le_bytes());
    a[0x38..0x40].copy_from_slice(&data.to_le_bytes());
    let (l, o) = (min_bytes(clusters), min_bytes(lcn));
    a.push((o.len() as u8) << 4 | l.len() as u8);
    a.extend_from_slice(&l);
    a.extend_from_slice(&o);
    a.push(0);
    pad8(&mut a);
    let len = a.len() as u32;
    a[4..8].copy_from_slice(&len.to_le_bytes());
    a
}

/// Little-endian bytes of a positive value as a minimal signed number.
fn min_bytes(v: u64) -> Vec<u8> {
    let mut n = 1;
    while n < 8 && v >= 1u64 << (8 * n - 1) {
        n += 1;
    }
    v.to_le_bytes()[..n].to_vec()
}

/// A FILE record with update-sequence protection (512-byte sectors).
fn file_record(recno: u64, seq: u16, flags: u16, attrs: &[Vec<u8>]) -> Vec<u8> {
    let mut r = vec![0u8; RECORD];
    r[0..4].copy_from_slice(b"FILE");
    let usa_count = (RECORD / 512 + 1) as u16;
    r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
    r[6..8].copy_from_slice(&usa_count.to_le_bytes());
    r[0x10..0x12].copy_from_slice(&seq.to_le_bytes());
    r[0x12..0x14].copy_from_slice(&1u16.to_le_bytes());
    let first = (0x30 + 2 * usa_count as usize).div_ceil(8) * 8;
    r[0x14..0x16].copy_from_slice(&(first as u16).to_le_bytes());
    r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
    r[0x1C..0x20].copy_from_slice(&(RECORD as u32).to_le_bytes());
    r[0x2C..0x30].copy_from_slice(&(recno as u32).to_le_bytes());
    let mut pos = first;
    for a in attrs {
        // Records that would overflow (very long names + links) lose their
        // last attributes; rare and harmless for a benchmark.
        if pos + a.len() + 8 > RECORD - 2 {
            break;
        }
        r[pos..pos + a.len()].copy_from_slice(a);
        pos += a.len();
    }
    r[pos..pos + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    pos += 8;
    r[0x18..0x1C].copy_from_slice(&(pos as u32).to_le_bytes());
    let usn = 0x0101u16.wrapping_add(recno as u16);
    r[0x30..0x32].copy_from_slice(&usn.to_le_bytes());
    for i in 1..=RECORD / 512 {
        let end = i * 512;
        let (save, slot) = (0x30 + 2 * i, end - 2);
        let v = [r[slot], r[slot + 1]];
        r[save..save + 2].copy_from_slice(&v);
        r[slot..slot + 2].copy_from_slice(&usn.to_le_bytes());
    }
    r
}
