//! Scans an NTFS image (or any file holding a volume) and prints record
//! counts, totals and timings.
//!
//! ```text
//! cargo run -p ringscan-ntfs --release --example dump -- <image> [options]
//!   --threads N    parse workers (default: all cores)
//!   --chunk BYTES  chunk size (default 4 MiB)
//!   --readers N    concurrent reader threads (default 1)
//!   --replay N     after the normal scan, capture every read in memory and
//!                  re-run the scan N times from memory: measures parsing
//!                  without disk I/O
//!   --list N       print the first N files (record, parent, sizes, path)
//!   --path P       print the entry whose rebuilt path is P ("/" separated)
//! ```

use ringscan_ntfs::{MftFile, MftOptions, MftProgress, ReadAt, mft_flags, scan_mft};
use std::collections::HashMap;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Records every read so the scan can be replayed from memory.
struct Recorder<'a> {
    inner: &'a dyn ReadAt,
    reads: Mutex<HashMap<(u64, usize), Vec<u8>>>,
}

impl ReadAt for Recorder<'_> {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.inner.read_at(off, buf)?;
        self.reads
            .lock()
            .unwrap()
            .insert((off, buf.len()), buf.to_vec());
        Ok(())
    }
}

/// Serves exactly the reads captured by [`Recorder`].
struct Replay(HashMap<(u64, usize), Vec<u8>>);

impl ReadAt for Replay {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let data = self
            .0
            .get(&(off, buf.len()))
            .ok_or_else(|| io::Error::other("read not captured"))?;
        buf.copy_from_slice(data);
        Ok(())
    }
}

fn mib(b: u64) -> f64 {
    b as f64 / (1u64 << 20) as f64
}

fn rate(n: u64, d: Duration) -> f64 {
    n as f64 / d.as_secs_f64().max(1e-9) / 1e6
}

fn path_of(files: &[MftFile], by_rec: &HashMap<u64, usize>, f: &MftFile) -> String {
    let mut parts = vec![f.name.as_str()];
    let mut cur = f;
    for _ in 0..4096 {
        if cur.record == 5 || cur.orphan {
            break;
        }
        match by_rec.get(&cur.parent) {
            Some(&i) if files[i].record != 5 => {
                cur = &files[i];
                parts.push(cur.name.as_str());
            }
            _ => break,
        }
    }
    if cur.orphan {
        parts.push("<orphaned>");
    }
    parts.reverse();
    parts.join("/")
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(image) = args.first() else {
        eprintln!(
            "usage: dump <image> [--threads N] [--chunk BYTES] [--readers N] [--replay N] [--list N] [--path P]"
        );
        std::process::exit(2);
    };
    let opt = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let num = |name: &str| opt(name).and_then(|v| v.parse::<usize>().ok());
    let opts = MftOptions {
        threads: num("--threads").unwrap_or(0),
        chunk_size: num("--chunk").unwrap_or(0),
        readers: num("--readers").unwrap_or(0),
    };

    let file = std::fs::File::open(image)?;
    let progress = MftProgress::new();
    let cancel = AtomicBool::new(false);
    let rec = Recorder {
        inner: &file,
        reads: Mutex::new(HashMap::new()),
    };
    let replay_n = num("--replay").unwrap_or(0);
    let vol: &dyn ReadAt = if replay_n > 0 { &rec } else { &file };
    let scan = scan_mft(vol, &opts, &progress, &cancel)?;
    let (v, s, files) = (&scan.volume, &scan.stats, &scan.files);

    println!("image            {image}");
    println!(
        "volume           {:.1} MiB, sector {} B, cluster {} B, record {} B, $MFT at LCN {}, serial {:016X}",
        mib(v.volume_size()),
        v.bytes_per_sector,
        v.bytes_per_cluster,
        v.record_size,
        v.mft_lcn,
        v.serial
    );
    println!(
        "$MFT             {} records ({:.1} MiB), {} runs, attribute list: {}, {} extension records",
        s.records,
        mib(s.records * v.record_size as u64),
        s.mft_runs,
        s.mft_attribute_list,
        s.mft_extension_records
    );
    println!(
        "records          in use {} (extension {}), unused {}, bad signature {}, bad fixup {}, wrong number {}, corrupt {}",
        s.in_use,
        s.extension_records,
        s.unused,
        s.bad_signature,
        s.bad_fixup,
        s.wrong_record_number,
        s.corrupt
    );
    println!(
        "                 nameless {}, stale extensions {}, unmapped {}, unreadable {}",
        s.nameless, s.stale_extensions, s.unmapped_records, s.unreadable_records
    );
    let dirs = files.iter().filter(|f| f.is_dir).count();
    let links: usize = files.iter().map(|f| f.extra_links.len()).sum();
    let logical: u64 = files.iter().map(|f| f.logical).sum();
    let alloc: u64 = files.iter().map(|f| f.alloc).sum();
    let ads = files
        .iter()
        .filter(|f| f.flags & mft_flags::NAMED_STREAMS != 0)
        .count();
    let attr_lists = files
        .iter()
        .filter(|f| f.flags & mft_flags::ATTRIBUTE_LIST != 0)
        .count();
    let reparse = files.iter().filter(|f| f.reparse_tag != 0).count();
    println!(
        "files            {} entries: {} files, {} dirs, {} extra hard links, {} with ADS, {} with attribute list, {} reparse points",
        files.len(),
        files.len() - dirs,
        dirs,
        links,
        ads,
        attr_lists,
        reparse
    );
    println!(
        "                 orphans {}, dropped links {}, cycles broken {}, root synthesized {}",
        s.orphans, s.dropped_links, s.cycles_broken, s.root_synthesized
    );
    println!(
        "totals           logical {:.2} MiB, allocated {:.2} MiB ({logical} / {alloc} bytes)",
        mib(logical),
        mib(alloc)
    );
    println!(
        "I/O              {} reads, {:.1} MiB, largest {} KiB, errors {}, {:.1} ms in read_at",
        s.reads,
        mib(s.bytes_read),
        s.max_read / 1024,
        s.read_errors,
        s.io_time.as_secs_f64() * 1e3
    );
    println!(
        "timing           total {:.1} ms (pipeline {:.1} ms, merge {:.1} ms), parse CPU {:.1} ms on {} threads, {} readers, chunk {} KiB",
        s.total_time.as_secs_f64() * 1e3,
        s.pipeline_time.as_secs_f64() * 1e3,
        s.merge_time.as_secs_f64() * 1e3,
        s.parse_time.as_secs_f64() * 1e3,
        s.threads,
        s.readers,
        s.chunk_size / 1024
    );
    println!(
        "throughput       {:.2} M records/s end to end, {:.2} M records/s per parse thread",
        rate(s.records, s.total_time),
        rate(s.records, s.parse_time)
    );

    if replay_n > 0 {
        let replay = Replay(rec.reads.into_inner().unwrap());
        let mut best = Duration::MAX;
        let mut best_pipe = Duration::MAX;
        for _ in 0..replay_n {
            let t = Instant::now();
            let r = scan_mft(&replay, &opts, &progress, &cancel)?;
            best = best.min(t.elapsed());
            best_pipe = best_pipe.min(r.stats.pipeline_time);
            assert_eq!(r.files.len(), files.len());
        }
        println!(
            "replay (no I/O)  best of {replay_n}: {:.1} ms total = {:.2} M records/s; read+parse pipeline {:.1} ms = {:.2} M records/s",
            best.as_secs_f64() * 1e3,
            rate(s.records, best),
            best_pipe.as_secs_f64() * 1e3,
            rate(s.records, best_pipe)
        );
    }

    let by_rec: HashMap<u64, usize> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.record, i))
        .collect();
    if let Some(n) = num("--list") {
        for f in files.iter().take(n) {
            println!(
                "{:>8} p={:<8} {} logical={:<10} alloc={:<10} attrs={:#07x} flags={:#x} links={} tag={:#x} {}",
                f.record,
                f.parent,
                if f.is_dir { 'd' } else { '-' },
                f.logical,
                f.alloc,
                f.attrs,
                f.flags,
                f.extra_links.len(),
                f.reparse_tag,
                path_of(files, &by_rec, f)
            );
        }
    }
    if let Some(p) = opt("--path") {
        for f in files.iter().filter(|f| path_of(files, &by_rec, f) == p) {
            println!("{f:#?}");
        }
    }
    Ok(())
}
