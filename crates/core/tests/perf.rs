//! Release-mode measurements (ignored by default; slow in debug builds):
//!
//! ```text
//! cargo test --release -p ringscan-core --test perf -- --ignored --nocapture --test-threads=1
//! ```
//!
//! * `memory_and_insert_throughput_5m`: builds a 5M-node drive-like tree
//!   with `add_batch` (like the walker) and reports bytes per node (from a
//!   counting global allocator and from `Tree::memory_bytes`) and the insert
//!   throughput of `add_batch` alone.
//! * `layout_budget_2m`: lays out a realistic 2M-node tree at
//!   `min_angle = 0.005`, `max_depth = 12` (budget: 5 ms) plus a few
//!   variants, and times the stats functions and `sort_all`.
//!
//! * `walk_real_dir`: walks a real directory read-only with `StdLister`
//!   (`RINGSCAN_WALK_DIR`, default `C:\Windows` on Windows, `/usr`
//!   elsewhere) at several thread counts while laying it out.
//!
//! The global allocator here counts live heap bytes, so run this binary with
//! `--test-threads=1` for clean numbers.

use std::alloc::{GlobalAlloc, Layout as AllocLayout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use ringscan_core::layout::{self, Params};
use ringscan_core::parking_lot::RwLock;
use ringscan_core::synth::{self, Rng};
use ringscan_core::walk::{Progress, StdLister, walk};
use ringscan_core::{NodeId, RawEntry, SizeMode, Tree, stats};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: AllocLayout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: AllocLayout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: AllocLayout, new_size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new_size) };
        if !q.is_null() {
            LIVE.fetch_add(new_size, Ordering::Relaxed);
            LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[test]
#[ignore = "slow in debug; run with --release --ignored"]
fn memory_and_insert_throughput_5m() {
    const NODES: usize = 5_000_000;
    let before = LIVE.load(Ordering::Relaxed);
    let mut tree = Tree::new("C:\\", 0);
    let mut rng = Rng::new(2026);
    let mut frontier: Vec<(NodeId, u32)> = vec![(0, 0)];
    let mut buf: Vec<RawEntry> = Vec::new();
    let mut insert_time = Duration::ZERO;
    let mut batches = 0u64;
    let mut name_bytes = 0usize;
    let total = Instant::now();
    while tree.len() < NODES {
        let i = rng.below(frontier.len() as u64) as usize;
        let (dir, depth) = frontier.swap_remove(i);
        buf.clear();
        synth::listing(&mut rng, depth, &mut buf);
        buf.truncate(NODES - tree.len());
        name_bytes += buf.iter().map(|e| e.name.len()).sum::<usize>();
        let t0 = Instant::now();
        let dirs = tree.add_batch(dir, &buf);
        insert_time += t0.elapsed();
        batches += 1;
        frontier.extend(dirs.into_iter().map(|(id, _)| (id, depth + 1)));
        if frontier.is_empty() {
            frontier.push((tree.add_dir(0, "more", 0, 0), 1));
        }
    }
    let total = total.elapsed();
    drop(buf);
    drop(frontier);
    let heap = LIVE.load(Ordering::Relaxed) - before;
    let n = tree.len();
    let dirs = tree.dirs(0) as usize;
    eprintln!(
        "5M build: {n} nodes ({} files, {dirs} dirs, {batches} batches)",
        tree.files(0)
    );
    eprintln!(
        "  avg name length {:.1} bytes",
        name_bytes as f64 / n as f64
    );
    eprintln!(
        "  heap {:.1} MB = {:.1} bytes/node (allocator); Tree::memory_bytes {:.1} bytes/node",
        heap as f64 / 1e6,
        heap as f64 / n as f64,
        tree.memory_bytes() as f64 / n as f64
    );
    eprintln!(
        "  add_batch: {:.0} ms total = {:.1} M nodes/s ({:.0} ns/node); whole build incl. name generation {:.0} ms",
        ms(insert_time),
        n as f64 / insert_time.as_secs_f64() / 1e6,
        insert_time.as_nanos() as f64 / n as f64,
        ms(total)
    );
    assert_eq!(n, NODES);
    assert!(heap as f64 / (n as f64) < 64.0, "target <= 64 bytes/node");
    // Sanity: aggregates are exact after the live build.
    let sum: u64 = tree
        .children(0)
        .map(|c| tree.size(c, SizeMode::Logical))
        .sum();
    assert_eq!(sum, tree.size(0, SizeMode::Logical));

    drop(tree);

    // Raw (MFT-style) build, short names, for comparison.
    let t0 = Instant::now();
    let mut raw = Tree::new("C:\\", 0);
    let mut stack: Vec<NodeId> = vec![0];
    let mut i = 0u64;
    while raw.len() < NODES {
        let parent = stack[(i as usize * 7919) % stack.len()];
        i += 1;
        if i.is_multiple_of(12) {
            stack.push(raw.add_dir_raw(parent, "dir", 0, 0));
        } else {
            raw.add_file_raw(parent, "file.bin", i, i, 0, 0);
        }
    }
    let insert = t0.elapsed();
    let t1 = Instant::now();
    raw.recompute_aggregates();
    eprintln!(
        "  raw build: {:.0} ms for {} nodes + recompute_aggregates {:.0} ms",
        ms(insert),
        raw.len(),
        ms(t1.elapsed())
    );
}

fn bench_layout(name: &str, tree: &Tree, focus: NodeId, p: &Params, runs: usize) -> Duration {
    let mut times = Vec::with_capacity(runs);
    let mut arcs = 0;
    for _ in 0..runs {
        let t0 = Instant::now();
        let l = layout::compute(tree, focus, p);
        times.push(t0.elapsed());
        arcs = l.arcs.len();
    }
    let max = *times.iter().max().unwrap();
    let med = median(times);
    eprintln!(
        "  {name}: median {:.2} ms, max {:.2} ms, {arcs} arcs",
        ms(med),
        ms(max)
    );
    med
}

#[test]
#[ignore = "slow in debug; run with --release --ignored"]
fn layout_budget_2m() {
    let t0 = Instant::now();
    let tree = synth::tree(7, 2_000_000);
    eprintln!(
        "2M tree built in {:.0} ms: {} files, {} dirs, {:.1} GB",
        ms(t0.elapsed()),
        tree.files(0),
        tree.dirs(0),
        tree.size(0, SizeMode::Allocated) as f64 / 1e9
    );
    let p = Params {
        mode: SizeMode::Allocated,
        max_depth: 12,
        min_angle: 0.005,
        show_free: true,
    };
    eprintln!("layout (min_angle 0.005, max_depth 12):");
    let med = bench_layout("root, allocated", &tree, 0, &p, 50);
    bench_layout(
        "root, logical",
        &tree,
        0,
        &Params {
            mode: SizeMode::Logical,
            ..p
        },
        50,
    );
    bench_layout(
        "root, show_free off",
        &tree,
        0,
        &Params {
            show_free: false,
            ..p
        },
        50,
    );
    bench_layout(
        "root, min_angle 0.001",
        &tree,
        0,
        &Params {
            min_angle: 0.001,
            ..p
        },
        20,
    );
    let big_child = tree
        .children(0)
        .max_by_key(|&c| tree.size(c, SizeMode::Allocated))
        .unwrap();
    bench_layout("largest top-level dir", &tree, big_child, &p, 50);
    if !cfg!(debug_assertions) {
        assert!(
            med <= Duration::from_millis(5),
            "layout budget exceeded: {med:?}"
        );
    }

    let pc = synth::this_pc(9, 2_000_000);
    bench_layout("This PC (2 drives, free shown)", &pc, 0, &p, 50);

    // A directory with one million files next to normal content.
    let mut flat = synth::tree(11, 500_000);
    let d = flat.add_dir(0, "flat", 0, 0);
    let mut rng = Rng::new(5);
    let entries: Vec<RawEntry> = (0..1_000_000)
        .map(|i| RawEntry {
            name: format!("f{i}.tmp"),
            logical: rng.below(200_000),
            alloc: 4096,
            ..Default::default()
        })
        .collect();
    flat.add_batch(d, &entries);
    bench_layout("root with a 1M-file dir", &flat, 0, &p, 20);
    bench_layout("focus = the 1M-file dir", &flat, d, &p, 20);

    eprintln!("stats on the 2M tree:");
    let t = Instant::now();
    let exts = stats::ext_breakdown(&tree, 0, SizeMode::Allocated);
    eprintln!(
        "  ext_breakdown: {:.1} ms ({} extensions)",
        ms(t.elapsed()),
        exts.len()
    );
    let t = Instant::now();
    let top = stats::largest_files(&tree, 0, SizeMode::Allocated, 100);
    eprintln!("  largest_files(100): {:.1} ms", ms(t.elapsed()));
    assert_eq!(top.len(), 100);
    let t = Instant::now();
    let n = tree.descendants(0).count();
    eprintln!(
        "  descendants() traversal alone: {:.1} ms ({n} nodes)",
        ms(t.elapsed())
    );
    let t = Instant::now();
    let hits = stats::search(&tree, 0, "Cache", 100_000);
    eprintln!(
        "  search(\"Cache\"): {:.1} ms ({} hits)",
        ms(t.elapsed()),
        hits.len()
    );
    let t = Instant::now();
    let hits = stats::search(&tree, 0, "ґ", 10);
    eprintln!(
        "  search(non-ASCII query, ASCII names): {:.1} ms ({} hits)",
        ms(t.elapsed()),
        hits.len()
    );
    let mut uk = synth::tree(8, 200_000);
    for i in 0..200_000u32 {
        uk.add_file(0, &format!("Звіт_{i}.ДОК"), 1, 1, 0, 0);
    }
    let t = Instant::now();
    let hits = stats::search(&uk, 0, "звіт_1999", 1000);
    eprintln!(
        "  search(non-ASCII) over 200K Cyrillic + 200K ASCII names: {:.1} ms ({} hits)",
        ms(t.elapsed()),
        hits.len()
    );
    assert_eq!(hits.len(), 111); // 1999, 19990..19999, 199900..199999
    let mut sorted = tree.clone();
    let t = Instant::now();
    sorted.sort_all(SizeMode::Allocated);
    eprintln!("  sort_all: {:.0} ms", ms(t.elapsed()));
    let t = Instant::now();
    let found = tree.find_path(&tree.path(1_999_999));
    eprintln!(
        "  path + find_path of a deep node: {:.3} ms",
        ms(t.elapsed())
    );
    assert_eq!(found, Some(1_999_999));
}

#[test]
#[ignore = "walks a real directory; run with --release --ignored"]
fn walk_real_dir() {
    let dir = std::env::var_os("RINGSCAN_WALK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { r"C:\Windows" } else { "/usr" }));
    for threads in [1, 4, 8, 12] {
        let tree = RwLock::new(Tree::new(dir.to_str().unwrap(), 0));
        let progress = Progress::new();
        let t0 = Instant::now();
        walk(
            &tree,
            0,
            &dir,
            &StdLister,
            threads,
            &progress,
            &AtomicBool::new(false),
        );
        let elapsed = t0.elapsed();
        let s = progress.snapshot();
        let t = tree.read();
        let t1 = Instant::now();
        let l = layout::compute(&t, 0, &Params::default());
        let layout_time = t1.elapsed();
        eprintln!(
            "walk {} with {threads:>2} threads: {:>6.0} ms, {} files, {} dirs, {:.2} GB, {} errors = {:.0}K entries/s; layout {:.2} ms ({} arcs)",
            dir.display(),
            ms(elapsed),
            s.files,
            s.dirs,
            s.bytes as f64 / 1e9,
            s.errors,
            (s.files + s.dirs) as f64 / elapsed.as_secs_f64() / 1e3,
            ms(layout_time),
            l.arcs.len()
        );
        assert_eq!((t.files(0), t.dirs(0)), (s.files, s.dirs));
        let sum: u64 = t.children(0).map(|c| t.size(c, SizeMode::Allocated)).sum();
        assert_eq!(sum, t.size(0, SizeMode::Allocated));
    }
}
