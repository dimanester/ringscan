//! Compares the MFT reader against ground truth recorded by ntfs-3g for
//! every image built by `scripts/make-ntfs-fixtures.sh` (skipped with a
//! notice when the fixtures have not been generated).
//!
//! Checked per image: every path (rebuilt from parent references, hard links
//! included) maps to the same MFT record as ntfs-3g's inode number; is_dir;
//! logical size (+ alternate streams); allocated size wherever ntfs-3g
//! reports it the same way (non-resident unnamed data without ADS, and
//! directories); hard-link sets; deleted files absent; reparse tags; DOS
//! names never used; facts about fragmentation; every read aligned; the same
//! result with 1 thread and tiny chunks.

mod common;

use common::*;
use ringscan_ntfs::{MftFile, MftOptions, MftProgress, MftScan, mft_flags, reparse, scan_mft};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::AtomicBool;

/// Rebuilds "a/b/c" paths from parent references (root = "").
struct Paths<'a> {
    by_rec: HashMap<u64, &'a MftFile>,
}

impl<'a> Paths<'a> {
    fn new(files: &'a [MftFile]) -> Self {
        Paths {
            by_rec: files.iter().map(|f| (f.record, f)).collect(),
        }
    }

    /// Path of directory `rec`, or None if it is not attached to the root.
    fn dir_path(&self, rec: u64) -> Option<String> {
        let mut parts = Vec::new();
        let mut cur = *self.by_rec.get(&rec)?;
        for _ in 0..10_000 {
            if cur.record == 5 {
                parts.reverse();
                return Some(parts.join("/"));
            }
            if cur.orphan {
                return None;
            }
            parts.push(cur.name.as_str());
            cur = *self.by_rec.get(&cur.parent)?;
        }
        None
    }

    fn join(&self, parent: u64, name: &str) -> Option<String> {
        let p = self.dir_path(parent)?;
        Some(if p.is_empty() {
            name.to_string()
        } else {
            format!("{p}/{name}")
        })
    }

    /// Records 0..=15 other than the root, and everything under `$Extend`,
    /// are NTFS metafiles that ntfs-3g hides.
    fn is_meta(&self, f: &MftFile) -> bool {
        let mut cur = f;
        for _ in 0..10_000 {
            if cur.record == 5 {
                return false;
            }
            if cur.record < 16 || cur.orphan {
                return true;
            }
            match self.by_rec.get(&cur.parent) {
                Some(p) => cur = p,
                None => return true,
            }
        }
        true
    }
}

fn scan(vol: &dyn ringscan_ntfs::ReadAt, opts: MftOptions) -> MftScan {
    scan_mft(vol, &opts, &MftProgress::new(), &AtomicBool::new(false)).expect("scan_mft")
}

fn check_image(img_path: &std::path::Path, json_path: &std::path::Path) {
    let name = img_path.file_stem().unwrap().to_string_lossy().to_string();
    let file = std::fs::File::open(img_path).unwrap();
    let vi = ringscan_ntfs::read_volume_info(&file).unwrap();
    let checker = AlignChecker::new(&file, vi.volume_size(), vi.bytes_per_sector as u64);
    let t = std::time::Instant::now();
    let res = scan(&checker, MftOptions::default());
    let elapsed = t.elapsed();
    let (s, files) = (&res.stats, &res.files);
    let truth: Value = serde_json::from_slice(&std::fs::read(json_path).unwrap()).unwrap();
    let gt_files = truth["files"].as_array().unwrap();
    eprintln!(
        "{name}: {} records, {} files in {:.1} ms, {} reads ({} MiB), $MFT runs {} attr list {}",
        s.records,
        files.len(),
        elapsed.as_secs_f64() * 1e3,
        s.reads,
        s.bytes_read >> 20,
        s.mft_runs,
        s.mft_attribute_list
    );

    // Volume-level sanity.
    assert_eq!(
        s.bad_fixup + s.bad_signature + s.wrong_record_number + s.corrupt,
        0,
        "{name}: damaged records"
    );
    assert_eq!(
        s.stale_extensions
            + s.orphans
            + s.cycles_broken
            + s.unmapped_records
            + s.unreadable_records,
        0,
        "{name}"
    );
    assert_eq!(s.dropped_links, 0, "{name}");
    assert!(!s.root_synthesized);
    let root = files.iter().find(|f| f.record == 5).unwrap();
    assert_eq!(
        (root.parent, root.name.as_str(), root.is_dir),
        (5, ".", true)
    );
    assert!(
        files.windows(2).all(|w| w[0].record < w[1].record),
        "{name}: sorted by record"
    );

    // Every link of every file, by path.
    let paths = Paths::new(files);
    let mut ours: HashMap<String, &MftFile> = HashMap::new();
    let mut links_of: HashMap<u64, BTreeSet<String>> = HashMap::new();
    for f in files {
        assert_eq!(
            f.flags & mft_flags::METAFILE != 0,
            paths.is_meta(f),
            "{name}: METAFILE flag of {}",
            f.record
        );
    }
    let extend_children = files
        .iter()
        .filter(|f| f.parent == 11 && f.record != 11)
        .count();
    assert!(
        extend_children >= 3,
        "{name}: $Extend\\$Quota, $ObjId, $Reparse"
    );
    for f in files.iter().filter(|f| f.flags & mft_flags::METAFILE == 0) {
        let mut all = vec![if f.record == 5 {
            Some(String::new())
        } else {
            paths.join(f.parent, &f.name)
        }];
        all.extend(f.extra_links.iter().map(|(p, n)| paths.join(*p, n)));
        for p in all {
            let p = p.unwrap_or_else(|| panic!("{name}: record {} has a detached link", f.record));
            assert!(
                ours.insert(p.clone(), f).is_none(),
                "{name}: duplicate path {p}"
            );
            links_of.entry(f.record).or_default().insert(p);
        }
    }

    // Ground truth -> ours.
    let mut streams: HashMap<&str, (u64, u64, usize)> = HashMap::new();
    for st in truth["streams"].as_array().unwrap() {
        let e = streams.entry(st["path"].as_str().unwrap()).or_default();
        e.0 += st["size"].as_u64().unwrap();
        e.1 += st["alloc"].as_u64().unwrap();
        e.2 += 1;
    }
    let mut gt_links: HashMap<u64, BTreeSet<String>> = HashMap::new();
    let mut compared_alloc = 0usize;
    for g in gt_files {
        let path = g["path"].as_str().unwrap();
        let ino = g["ino"].as_u64().unwrap();
        let ours_f = ours
            .get(path)
            .unwrap_or_else(|| panic!("{name}: {path:?} (inode {ino}) not found"));
        assert_eq!(ours_f.record, ino, "{name}: {path}: record vs inode");
        gt_links.entry(ino).or_default().insert(path.to_string());
        if g["symlink"].as_bool().unwrap() {
            continue; // Interix symlinks and reparse points: lstat shows the link
        }
        let is_dir = g["dir"].as_bool().unwrap();
        assert_eq!(ours_f.is_dir, is_dir, "{name}: {path}: is_dir");
        let (ads_size, ads_alloc, n_ads) = streams.get(path).copied().unwrap_or_default();
        let size = g["size"].as_u64().unwrap();
        let alloc = g["alloc"].as_u64().unwrap();
        if !is_dir {
            assert_eq!(
                ours_f.logical,
                size + ads_size,
                "{name}: {path}: logical size"
            );
        } else if n_ads > 0 {
            assert_eq!(
                ours_f.logical, ads_size,
                "{name}: {path}: directory streams"
            );
        }
        // ntfs-3g reports resident data as allocated and ignores streams in
        // st_blocks; compare alloc only where both count the same thing.
        let comparable = ours_f.flags & (mft_flags::RESIDENT_DATA | mft_flags::NAMED_STREAMS) == 0
            && (is_dir || ours_f.logical > 0 || alloc > 0);
        if comparable {
            assert_eq!(ours_f.alloc, alloc, "{name}: {path}: allocated size");
            compared_alloc += 1;
        } else if n_ads > 0 && ours_f.flags & mft_flags::RESIDENT_DATA == 0 && !is_dir {
            // Non-resident data plus streams: at least the unnamed stream's
            // allocation, at most that plus every stream's.
            assert!(
                ours_f.alloc >= alloc && ours_f.alloc <= alloc + ads_alloc,
                "{name}: {path}: alloc with streams"
            );
        }
    }
    assert!(compared_alloc > 0);

    // Ours -> ground truth, and identical hard-link sets.
    let gt_paths: std::collections::HashSet<&str> = gt_files
        .iter()
        .map(|g| g["path"].as_str().unwrap())
        .collect();
    for p in ours.keys() {
        assert!(gt_paths.contains(p.as_str()), "{name}: extra path {p}");
    }
    for (ino, set) in &gt_links {
        assert_eq!(
            links_of.get(ino),
            Some(set),
            "{name}: hard links of record {ino}"
        );
    }

    // Manifest-driven checks.
    let m = &truth["manifest"];
    for d in m["deleted"].as_array().unwrap() {
        assert!(
            !ours.contains_key(d.as_str().unwrap()),
            "{name}: deleted {d} is present"
        );
    }
    for (p, tag) in m["reparse"].as_object().unwrap() {
        let f = ours[p.as_str()];
        assert_eq!(
            f.reparse_tag as u64,
            tag.as_u64().unwrap(),
            "{name}: {p}: reparse tag"
        );
        assert_ne!(f.attrs & 0x400, 0, "{name}: {p}: REPARSE_POINT attribute");
        if p.contains("cloud") {
            assert!(reparse::is_cloud(f.reparse_tag));
        }
    }
    for (p, short) in m["dos_names"].as_object().unwrap() {
        let dir = p.rsplit_once('/').map_or("", |x| x.0);
        let short_path = if dir.is_empty() {
            short.as_str().unwrap().to_string()
        } else {
            format!("{dir}/{}", short.as_str().unwrap())
        };
        assert!(ours.contains_key(p.as_str()), "{name}: long name {p}");
        assert!(
            !ours.contains_key(&short_path),
            "{name}: DOS name {short_path} must be skipped"
        );
        assert!(ours[p.as_str()].extra_links.is_empty());
    }
    for p in m["compressed"].as_array().unwrap() {
        assert_ne!(
            ours[p.as_str().unwrap()].attrs & 0x800,
            0,
            "{name}: {p} compressed"
        );
    }
    for p in m["sparse"].as_array().unwrap() {
        assert_ne!(
            ours[p.as_str().unwrap()].attrs & 0x200,
            0,
            "{name}: {p} sparse"
        );
    }
    // Facts from ntfsinfo: fragmentation and attribute lists.
    for (key, fact) in truth["facts"].as_object().unwrap() {
        let runs = fact["runs"].as_u64().unwrap();
        let has_list = fact["attribute_list"].as_bool().unwrap();
        if key == "mft" {
            assert_eq!(
                s.mft_attribute_list, has_list,
                "{name}: $MFT attribute list"
            );
            assert!(
                s.mft_runs >= 1 && s.mft_runs <= runs,
                "{name}: $MFT runs {} vs ntfsinfo {runs}",
                s.mft_runs
            );
            if runs > 1 {
                assert!(s.mft_runs > 1, "{name}: $MFT fragments");
            }
        } else {
            let p = fact["path"].as_str().unwrap().trim_start_matches('/');
            let f = ours[p];
            assert_eq!(
                f.flags & mft_flags::ATTRIBUTE_LIST != 0,
                has_list,
                "{name}: {p}: attribute list"
            );
            eprintln!(
                "{name}: {p}: {runs} fragments, attribute list {has_list}, logical {}",
                f.logical
            );
        }
    }

    // parent_first_order: a permutation with parents before children.
    let order = ringscan_ntfs::parent_first_order(files);
    let mut pos = vec![usize::MAX; files.len()];
    for (k, &i) in order.iter().enumerate() {
        assert_eq!(pos[i], usize::MAX, "{name}: index {i} twice");
        pos[i] = k;
    }
    let idx: HashMap<u64, usize> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.record, i))
        .collect();
    for (i, f) in files.iter().enumerate() {
        if f.record != 5 {
            assert!(
                pos[idx[&f.parent]] < pos[i],
                "{name}: parent of {} after it",
                f.record
            );
        }
    }

    // Same result single-threaded, sequential, with minimum-size chunks.
    let small = scan(
        &file,
        MftOptions {
            threads: 1,
            chunk_size: 64 << 10,
            readers: 1,
        },
    );
    assert!(
        small.files == res.files,
        "{name}: result depends on threads/chunking"
    );
}

#[test]
fn fixtures_match_ground_truth() {
    for (img, json) in fixtures() {
        check_image(&img, &json);
    }
}
