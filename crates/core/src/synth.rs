//! Deterministic synthetic trees for tests, benchmarks and UI demos.
//!
//! [`tree`] grows a drive-like tree to an exact node count from a seed,
//! without touching the disk: each directory holds a log-normal number of
//! files (median 6, heavy tail into the thousands) and ~1.3 subdirectories
//! on average; file sizes are log-normal (median ~16 KB, tail into GBs);
//! names look like real file names (syllables, numbers, common extensions).
//! Directories are expanded in random order, so depths vary like on a real
//! drive. The same seed always gives the same tree. [`this_pc`] puts two
//! such drives under a VIRTUAL "This PC" root with FREE and UNACCOUNTED
//! nodes, handy for UI work without scanning anything.
//!
//! [`Rng`] is a tiny SplitMix64 generator so tests need no extra crates.

use std::collections::HashSet;

use crate::flags::{DIR, FREE, HIDDEN, UNACCOUNTED, VIRTUAL};
use crate::tree::Tree;
use crate::walk::RawEntry;
use crate::{NodeId, SizeMode};

/// SplitMix64 pseudo-random generator (deterministic, not cryptographic).
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x2545_f491_4f6c_dd1d)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (0 if `n == 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// True with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    /// Standard normal sample (Box-Muller).
    pub fn normal(&mut self) -> f64 {
        let u1 = self.unit().max(f64::MIN_POSITIVE);
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }

    /// `exp(mu + sigma * N(0,1))`.
    pub fn log_normal(&mut self, mu: f64, sigma: f64) -> f64 {
        (mu + sigma * self.normal()).exp()
    }

    /// A uniformly chosen element (panics on an empty slice).
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

const SYLLABLES: &[&str] = &[
    "app", "data", "win", "sys", "lib", "core", "net", "user", "doc", "img", "log", "temp",
    "cache", "pack", "res", "ui", "api", "test", "src", "build", "main", "util", "conf", "font",
    "media", "shell", "web", "help", "driver", "update", "setup", "index", "module", "plugin",
    "theme", "asset", "store", "sync", "node", "micro", "soft", "office", "game", "save", "photo",
    "video", "music", "backup", "report", "project",
];

/// Extensions with relative weights.
const EXTS: &[(&str, u32)] = &[
    ("dll", 10),
    ("js", 10),
    ("png", 8),
    ("json", 6),
    ("txt", 6),
    ("jpg", 6),
    ("h", 5),
    ("xml", 5),
    ("cpp", 4),
    ("mui", 4),
    ("cs", 3),
    ("py", 3),
    ("exe", 3),
    ("log", 3),
    ("dat", 3),
    ("html", 3),
    ("ts", 3),
    ("svg", 3),
    ("bin", 2),
    ("pdf", 2),
    ("css", 2),
    ("ini", 2),
    ("sys", 2),
    ("tmp", 2),
    ("md", 2),
    ("manifest", 2),
    ("cat", 2),
    ("pyc", 2),
    ("map", 2),
    ("mp4", 1),
    ("mp3", 1),
    ("docx", 1),
    ("zip", 1),
    ("cab", 1),
    ("msi", 1),
    ("db", 1),
    ("ttf", 1),
    ("lnk", 1),
];

/// 2026-01-01 00:00:00 UTC; synthetic mtimes go back ten years from here.
const BASE_TIME: i64 = 1_767_225_600;

fn base_name(rng: &mut Rng, max_syllables: u64) -> String {
    let mut s = String::new();
    let parts = 1 + rng.below(max_syllables);
    let sep = *rng.pick(&["", "_", "-"]);
    for i in 0..parts {
        if i > 0 {
            s.push_str(sep);
        }
        let syl = *rng.pick(SYLLABLES);
        if rng.chance(0.3) {
            let mut c = syl.chars();
            if let Some(first) = c.next() {
                s.extend(first.to_uppercase());
                s.push_str(c.as_str());
            }
        } else {
            s.push_str(syl);
        }
    }
    if rng.chance(0.45) {
        let digits = 1 + rng.below(5);
        s.push(if rng.chance(0.5) { '_' } else { '-' });
        for _ in 0..digits {
            s.push(char::from(b'0' + rng.below(10) as u8));
        }
    }
    s
}

/// A plausible file name ("Core_media-042.dll").
pub fn file_name(rng: &mut Rng) -> String {
    let mut s = base_name(rng, 4);
    if rng.chance(0.96) {
        let total: u32 = EXTS.iter().map(|e| e.1).sum();
        let mut x = rng.below(u64::from(total)) as u32;
        for &(ext, w) in EXTS {
            if x < w {
                s.push('.');
                s.push_str(ext);
                break;
            }
            x -= w;
        }
    }
    s
}

/// A plausible directory name ("Project-sync").
pub fn dir_name(rng: &mut Rng) -> String {
    base_name(rng, 3)
}

/// Appends the entries of one synthetic directory at `depth` (0 = drive
/// root) to `out`: subdirectories first, then files; names are unique.
pub fn listing(rng: &mut Rng, depth: u32, out: &mut Vec<RawEntry>) {
    let subdirs = match depth {
        0 => 12 + rng.below(8),
        1..=19 => (rng.log_normal(0.6, 0.9) - 1.0).clamp(0.0, 40.0) as u64,
        _ => 0,
    };
    let files = if depth == 0 {
        rng.below(6)
    } else {
        rng.log_normal(6f64.ln(), 1.3).min(5000.0) as u64
    };
    let mut seen = HashSet::new();
    let mut unique = |mut name: String| {
        let mut i = 2;
        let stem_len = name.len();
        while !seen.insert(name.to_ascii_lowercase()) {
            name.truncate(stem_len);
            name.push_str(&format!(" ({i})"));
            i += 1;
        }
        name
    };
    for _ in 0..subdirs {
        let name = unique(dir_name(rng));
        out.push(RawEntry {
            name,
            is_dir: true,
            flags: if rng.chance(0.02) { HIDDEN } else { 0 },
            mtime: BASE_TIME - rng.below(10 * 365 * 86_400) as i64,
            ..RawEntry::default()
        });
    }
    for _ in 0..files {
        let name = unique(file_name(rng));
        let logical = rng
            .log_normal(16_384f64.ln(), 2.3)
            .min(64.0 * (1u64 << 30) as f64) as u64;
        // Tiny files live in the MFT record (no clusters).
        let alloc = if logical < 600 {
            0
        } else {
            logical.div_ceil(4096) * 4096
        };
        out.push(RawEntry {
            name,
            is_dir: false,
            logical,
            alloc,
            flags: if rng.chance(0.01) { HIDDEN } else { 0 },
            mtime: BASE_TIME - rng.below(10 * 365 * 86_400) as i64,
            file_id: 0,
        });
    }
}

/// A synthetic drive "C:\" with exactly `nodes` nodes (at least 1).
pub fn tree(seed: u64, nodes: usize) -> Tree {
    let mut t = Tree::new("C:\\", 0);
    grow(&mut t, 0, seed, nodes);
    t
}

/// Adds synthetic content below directory `at` until `tree.len()` reaches
/// `nodes`, inserting one [`listing`] per directory with
/// [`Tree::add_batch`] like the walker does.
pub fn grow(tree: &mut Tree, at: NodeId, seed: u64, nodes: usize) {
    let mut rng = Rng::new(seed);
    let base_depth = tree.depth(at) as u32;
    let mut frontier: Vec<(NodeId, u32)> = vec![(at, base_depth)];
    let mut buf = Vec::new();
    while tree.len() < nodes {
        if frontier.is_empty() {
            let id = tree.add_dir(at, &dir_name(&mut rng), 0, BASE_TIME);
            frontier.push((id, base_depth + 1));
            continue;
        }
        let i = rng.below(frontier.len() as u64) as usize;
        let (dir, depth) = frontier.swap_remove(i);
        buf.clear();
        listing(&mut rng, depth, &mut buf);
        buf.truncate(nodes - tree.len());
        for (id, _) in tree.add_batch(dir, &buf) {
            frontier.push((id, depth + 1));
        }
    }
}

/// A VIRTUAL "This PC" root with drives "C:\" (~80 % of `nodes`) and "D:\",
/// each with an UNACCOUNTED node (1 % of the scanned size, about what an
/// elevated scan of a real system drive leaves unexplained) and a FREE
/// node (the drive is ~2/3 full). C: also has the engine's
/// `<reserved by Windows>` UNACCOUNTED node (0.5 % of the scanned size), as
/// a system drive does.
pub fn this_pc(seed: u64, nodes: usize) -> Tree {
    let mut t = Tree::new("This PC", VIRTUAL);
    let c = t.add_dir(0, "C:\\", 0, 0);
    grow(&mut t, c, seed, (nodes * 4 / 5).max(2));
    let d = t.add_dir(0, "D:\\", 0, 0);
    grow(&mut t, d, seed.wrapping_add(1), nodes.max(4));
    for drive in [c, d] {
        let scanned = t.size(drive, SizeMode::Allocated);
        let unaccounted = scanned / 100;
        // Same name as ringscan-engine's RESERVED_NAME (the app tells the
        // node apart by it).
        let reserved = if drive == c { scanned / 200 } else { 0 };
        let used = scanned + unaccounted + reserved;
        if reserved > 0 {
            t.add_virtual(drive, "<reserved by Windows>", UNACCOUNTED, reserved);
        }
        t.add_virtual(drive, "<unaccounted>", UNACCOUNTED, unaccounted);
        t.add_virtual(drive, "<free space>", FREE, used / 2);
    }
    debug_assert!(t.node(c).has(DIR));
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_and_bounded() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(1);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        for _ in 0..1000 {
            assert!(a.below(7) < 7);
            let u = a.unit();
            assert!((0.0..1.0).contains(&u));
        }
        assert_eq!(a.below(0), 0);
        let mean: f64 = (0..10_000).map(|_| a.normal()).sum::<f64>() / 10_000.0;
        assert!(mean.abs() < 0.05);
    }

    #[test]
    fn tree_has_exact_size_and_is_reproducible() {
        let a = tree(42, 5000);
        let b = tree(42, 5000);
        assert_eq!(a.len(), 5000);
        assert_eq!(a.size(0, SizeMode::Logical), b.size(0, SizeMode::Logical));
        assert_eq!(a.path(4999), b.path(4999));
        assert!(a.dirs(0) > 100 && a.files(0) > 3000);
        assert!(tree(43, 5000).size(0, SizeMode::Logical) != a.size(0, SizeMode::Logical));
        // Names are unique within a directory.
        for (id, n) in a.live_nodes() {
            if n.is_dir() {
                let mut names: Vec<String> = a
                    .children(id)
                    .map(|c| a.name(c).to_ascii_lowercase())
                    .collect();
                let len = names.len();
                names.sort();
                names.dedup();
                assert_eq!(names.len(), len);
            }
        }
        assert_eq!(tree(1, 1).len(), 1);
    }

    #[test]
    fn this_pc_has_drives_free_and_unaccounted() {
        let t = this_pc(5, 3000);
        assert!(t.node(0).is_virtual());
        let drives: Vec<_> = t.children(0).collect();
        assert_eq!(drives.len(), 2);
        assert_eq!(t.free_nodes().count(), 2);
        assert_eq!(t.path(drives[0]), "C:\\");
        for (k, &drive) in drives.iter().enumerate() {
            let un = t.find_child(drive, "<unaccounted>").unwrap();
            assert!(t.node(un).has(UNACCOUNTED));
            // C: only: reserved space, 0.5 % of what was scanned.
            let res = t.find_child(drive, "<reserved by Windows>");
            assert_eq!(res.is_some(), k == 0);
            let res_size = res.map_or(0, |r| {
                assert!(t.node(r).has(UNACCOUNTED));
                t.node(r).size(SizeMode::Allocated)
            });
            // Unaccounted: about 1 % of what was scanned.
            let un_size = t.node(un).size(SizeMode::Allocated);
            let scanned = t.size(drive, SizeMode::Allocated) - un_size - res_size;
            assert_eq!(un_size, scanned / 100);
            assert_eq!(res_size, if k == 0 { scanned / 200 } else { 0 });
        }
    }
}
