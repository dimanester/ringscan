//! Fixture-free end-to-end tests on in-memory volumes built byte by byte:
//! a fragmented `$MFT` with an `$ATTRIBUTE_LIST` (extents in extension
//! records, a record spanning two runs, unaligned runs), hard links, DOS
//! names, ADS, sparse/compressed sizes, stale references, cycles, damaged
//! records, `$BadClus` and WOF rules — for three geometries.

mod common;

use common::*;
use ringscan_ntfs::{MftFile, MftOptions, MftProgress, mft_flags, reparse, scan_mft};
use std::sync::atomic::AtomicBool;

fn find(files: &[MftFile], rec: u64) -> Option<&MftFile> {
    files.iter().find(|f| f.record == rec)
}

fn get(files: &[MftFile], rec: u64) -> &MftFile {
    find(files, rec).unwrap_or_else(|| panic!("record {rec} missing"))
}

#[test]
fn synthetic_volumes() {
    for g in &GEOMETRIES {
        let img = build(g);
        let vol_end = CLUSTERS * g.cluster as u64;
        let checker = AlignChecker::new(&img.bytes, vol_end, g.sector as u64);
        let progress = MftProgress::new();
        let cancel = AtomicBool::new(false);
        for opts in [
            MftOptions::default(),
            MftOptions {
                threads: 1,
                chunk_size: 64 << 10,
                readers: 1,
            },
            MftOptions {
                threads: 3,
                chunk_size: 1,
                readers: 4,
            },
        ] {
            let scan = scan_mft(&checker, &opts, &progress, &cancel).expect("scan");
            let (s, files) = (&scan.stats, &scan.files);
            let ctx = format!("cluster {} record {}", g.cluster, g.record);
            assert_eq!(scan.volume.record_size, g.record, "{ctx}");
            assert_eq!(s.records, 64, "{ctx}");
            assert!(s.mft_attribute_list, "{ctx}");
            assert_eq!(s.mft_extension_records, 2, "{ctx}");
            assert_eq!(s.mft_runs, 3, "{ctx}");
            assert_eq!(s.unmapped_records, 0, "{ctx}");
            assert_eq!(s.bitmap_in_use, Some(64), "{ctx}");
            assert_eq!(
                (s.bad_fixup, s.wrong_record_number, s.stale_extensions),
                (1, 1, 1),
                "{ctx}"
            );
            assert_eq!(progress.done.load(std::sync::atomic::Ordering::Relaxed), 64);

            let root = get(files, 5);
            assert_eq!(
                (root.parent, root.name.as_str(), root.is_dir, root.orphan),
                (5, ".", true, false)
            );
            assert_eq!(root.alloc, 4096);
            // $MFT: name and sizes; its extension records are folded in.
            for f in files {
                let meta = f.record < 16 && f.record != 5;
                assert_eq!(
                    f.flags & mft_flags::METAFILE != 0,
                    meta,
                    "{ctx}: record {}",
                    f.record
                );
            }
            let mft = get(files, 0);
            assert_eq!(
                (mft.name.as_str(), mft.logical),
                ("$MFT", 64 * g.record as u64)
            );
            assert!(find(files, 16).is_none() && find(files, 17).is_none());
            // $BadClus counts only its two bad clusters.
            let bad = get(files, 8);
            assert_eq!(
                (bad.logical, bad.alloc),
                (2 * g.cluster as u64, 2 * g.cluster as u64),
                "{ctx}"
            );

            let docs = get(files, 20);
            assert!(
                docs.is_dir && !docs.orphan && docs.attrs & 0x10 != 0,
                "{ctx}: record straddling runs"
            );
            let report = get(files, 21);
            assert_eq!(
                (report.parent, report.logical, report.alloc, report.mtime),
                (20, 100, 0, MTIME)
            );
            assert_eq!(
                report.flags & mft_flags::RESIDENT_DATA,
                mft_flags::RESIDENT_DATA
            );
            let big = get(files, 22);
            assert_eq!(big.name, "big file.bin", "DOS name skipped");
            assert!(big.extra_links.is_empty());
            assert_eq!((big.logical, big.alloc), (5000 + 10 + 3000, 8192 + 3072));
            assert_eq!(big.flags, mft_flags::NAMED_STREAMS);
            let linked = get(files, 23);
            assert_eq!(
                (linked.parent, linked.name.as_str()),
                (5, "link-b"),
                "Win32 link is primary"
            );
            assert_eq!(
                linked.extra_links,
                vec![(20, "link-a".to_string())],
                "stale link dropped"
            );
            let sparse = get(files, 24);
            assert_eq!((sparse.logical, sparse.alloc), (1 << 20, 4096));
            assert_eq!(sparse.attrs & 0x200, 0x200);
            let packed = get(files, 25);
            assert_eq!((packed.logical, packed.alloc), (60000, 8192));
            assert_eq!(packed.attrs & 0x800, 0x800);
            let stale = get(files, 30);
            assert!(stale.orphan);
            assert_eq!((stale.parent, stale.parent_seq), (20, 2));
            assert!(find(files, 31).is_none(), "deleted file");
            let frag = get(files, 32);
            assert_eq!(
                (frag.name.as_str(), frag.parent, frag.logical, frag.alloc),
                ("fragmented.bin", 5, 40000, 40960)
            );
            assert_eq!(
                frag.flags & mft_flags::ATTRIBUTE_LIST,
                mft_flags::ATTRIBUTE_LIST
            );
            for r in [45, 46, 50, 51, 35, 36, 37] {
                assert!(
                    find(files, r).is_none(),
                    "{ctx}: record {r} must not be a file"
                );
            }
            let junction = get(files, 33);
            assert!(junction.is_dir && reparse::is_name_surrogate(junction.reparse_tag));
            assert_eq!(junction.attrs & 0x400, 0x400);
            let dads = get(files, 34);
            assert_eq!((dads.logical, dads.alloc), (700, 1024 + 4096));
            let (a, b) = (get(files, 38), get(files, 39));
            assert!(
                a.orphan ^ b.orphan,
                "exactly one cycle member becomes an orphan"
            );
            assert!(!get(files, 40).orphan);
            assert_eq!(s.cycles_broken, 1);
            let wof = get(files, 41);
            assert_eq!(
                (wof.logical, wof.alloc, wof.reparse_tag),
                (100000, 30720, reparse::WOF)
            );
            // On-disk bit 0x40000 is FILE_ATTRIBUTE_EA, never passed on as
            // the in-memory RECALL_ON_OPEN; genuine cloud bits and tags are.
            let signed = get(files, 42);
            assert_eq!(signed.attrs, 0x20, "{ctx}");
            assert_ne!(signed.flags & mft_flags::EXTENDED_ATTRIBUTES, 0);
            assert_eq!(signed.reparse_tag, 0);
            let online = get(files, 43);
            assert_eq!(online.attrs, 0x0040_0620, "{ctx}");
            assert_eq!(online.flags & mft_flags::EXTENDED_ATTRIBUTES, 0);
            assert!(reparse::is_cloud(online.reparse_tag));
            let cloud_dir = get(files, 44);
            assert_eq!(cloud_dir.attrs, 0x410, "{ctx}");
            assert!(cloud_dir.is_dir && reparse::is_cloud(cloud_dir.reparse_tag));
            assert_ne!(cloud_dir.flags & mft_flags::EXTENDED_ATTRIBUTES, 0);
            assert_eq!(s.orphans, 2, "{ctx}: stale.txt + one cycle member");
            // Non-resident $REPARSE_POINT without a $FILE_NAME tag: "unread",
            // not 0 (which consumers treat as an unknown link).
            for r in [52, 53] {
                let f = get(files, r);
                assert_eq!(f.reparse_tag, reparse::UNREAD, "{ctx}: record {r}");
                assert_eq!(f.attrs & 0x400, 0x400);
                assert!(!reparse::is_name_surrogate(f.reparse_tag));
                assert!(!reparse::is_cloud(f.reparse_tag));
                assert_eq!(f.alloc, 0, "{ctx}: a reparse value is not file data");
            }
            assert!(get(files, 53).is_dir);

            // Every read was aligned (checked by AlignChecker); the MFT runs
            // at odd clusters forced bounce reads in the small-cluster cases.
            assert!(!checker.log.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn missing_extension_record_leaves_tail_unmapped() {
    let g = &GEOMETRIES[0];
    let mut img = build(g);
    // Destroy record 17 (third $DATA extent): records of run 3 are unmapped.
    let blank = vec![0u8; g.record as usize];
    img.put_record(17, &blank);
    let scan = scan_mft(
        &img.bytes,
        &MftOptions::default(),
        &MftProgress::new(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let mapped = (g.runs[0].0 + g.runs[1].0) * g.cluster as u64 / g.record as u64;
    assert_eq!(scan.stats.records, mapped);
    assert_eq!(scan.stats.unmapped_records, 64 - mapped);
    assert!(scan.files.iter().any(|f| f.record == 41) == (41 < mapped));
}

#[test]
fn damaged_record_zero_falls_back_to_mirror() {
    let g = &GEOMETRIES[1];
    let mut img = build(g);
    let cs = g.cluster as usize;
    // Copy record 0 into $MFTMirr (at clusters/2 per the builder), then
    // corrupt the primary copy's fixups.
    let rs = g.record as usize;
    let mirr = (CLUSTERS as usize / 2) * cs;
    let prim = (g.runs[0].1 as usize) * cs;
    let rec0 = img.bytes[prim..prim + rs].to_vec();
    img.bytes[mirr..mirr + rs].copy_from_slice(&rec0);
    img.bytes[prim + 510] ^= 0x55;
    let scan = scan_mft(
        &img.bytes,
        &MftOptions::default(),
        &MftProgress::new(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(scan.stats.records, 64);
    assert!(scan.files.iter().any(|f| f.name == "fragmented.bin"));
    // Without a mirror the scan fails cleanly.
    img.bytes[mirr..mirr + rs].fill(0);
    let err = scan_mft(
        &img.bytes,
        &MftOptions::default(),
        &MftProgress::new(),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn cancel_and_progress() {
    let img = build(&GEOMETRIES[0]);
    let cancel = AtomicBool::new(true);
    let err = scan_mft(
        &img.bytes,
        &MftOptions::default(),
        &MftProgress::new(),
        &cancel,
    )
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
    let p = MftProgress::new();
    ringscan_ntfs::read_mft(&img.bytes, &p, &AtomicBool::new(false)).unwrap();
    assert_eq!(p.total.load(std::sync::atomic::Ordering::Relaxed), 64);
    assert_eq!(p.fraction(), 1.0);
}

#[test]
fn not_ntfs() {
    let zeros = vec![0u8; 1 << 20];
    assert!(ringscan_ntfs::read_volume_info(&zeros).is_err());
    let short = vec![0u8; 100];
    assert!(ringscan_ntfs::read_volume_info(&short).is_err());
}

#[test]
fn file_readat_reads_exactly() {
    // A temp file in Cargo's per-test scratch dir on Linux; under
    // %TEMP%\ringscan-dev when this test runs on Windows.
    let dir = if cfg!(windows) {
        std::env::temp_dir().join("ringscan-dev")
    } else {
        std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
    };
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("ntfs-readat-{}.bin", std::process::id()));
    let data: Vec<u8> = (0..20000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &data).unwrap();
    let f = std::fs::File::open(&path).unwrap();
    let mut buf = vec![0u8; 8192];
    ringscan_ntfs::ReadAt::read_at(&f, 4096, &mut buf).unwrap();
    assert_eq!(&buf[..], &data[4096..4096 + 8192]);
    assert!(
        ringscan_ntfs::ReadAt::read_at(&f, 16384, &mut buf).is_err(),
        "short read is an error"
    );
    drop(f);
    std::fs::remove_file(&path).unwrap();
}
