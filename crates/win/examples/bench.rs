//! Read-only benchmark of `list_dir` on real Windows directories.
//!
//! ```text
//! cargo run --release -p ringscan-win --target x86_64-pc-windows-gnu --example bench -- [BIG_DIR] [TREE] [ROUNDS]
//! ```
//!
//! 1. `list_dir(BIG_DIR)` (default `C:\Windows\WinSxS`, ~35k entries) in a
//!    loop: entries/s for one large flat directory.
//! 2. A recursive single-threaded walk of `TREE` (default
//!    `C:\Windows\System32`) that lists links but does not follow them:
//!    entries/s.
//!
//! 3. The same walk with N threads sharing a work stack, to show how much
//!    parallelism hides the per-directory open cost (a data point for the
//!    engine's walker, not its implementation).
//!
//! 1 and 2 are compared with `std::fs::read_dir`, which uses
//! `FindFirstFileExW(FindExInfoBasic, FIND_FIRST_EX_LARGE_FETCH)` — about
//! the fastest Win32 enumeration, but without allocation sizes. The first
//! round may hit the disk; the best of the remaining rounds is reported.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Default, Debug, Clone, Copy)]
struct Walk {
    entries: u64,
    dirs: u64,
    errors: u64,
    bytes: u64,
}

fn walk_list_dir(root: &Path) -> Walk {
    let mut w = Walk::default();
    let mut stack = vec![root.to_path_buf()];
    let mut buf = Vec::new();
    while let Some(dir) = stack.pop() {
        buf.clear();
        if ringscan_win::list_dir(&dir, &mut buf).is_err() {
            w.errors += 1;
            continue;
        }
        w.dirs += 1;
        for e in &buf {
            w.entries += 1;
            w.bytes += e.alloc;
            if e.is_dir && !e.is_link() {
                stack.push(e.path_in(&dir));
            }
        }
    }
    w
}

fn walk_std(root: &Path) -> Walk {
    let mut w = Walk::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            w.errors += 1;
            continue;
        };
        w.dirs += 1;
        for e in rd.flatten() {
            w.entries += 1;
            // Metadata comes from the find data: no extra system call.
            if let Ok(m) = e.metadata() {
                w.bytes += m.len();
                if m.is_dir() {
                    stack.push(e.path());
                }
            }
        }
    }
    w
}

/// Naive parallel walk: a shared stack of directories plus a count of
/// directories being listed; workers stop when both are empty.
fn walk_parallel(root: &Path, threads: usize) -> Walk {
    use std::sync::{Condvar, Mutex};
    struct Shared {
        stack: Vec<PathBuf>,
        busy: usize,
        walk: Walk,
    }
    let shared = Mutex::new(Shared {
        stack: vec![root.to_path_buf()],
        busy: 0,
        walk: Walk::default(),
    });
    let cv = Condvar::new();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut buf = Vec::new();
                loop {
                    let dir = {
                        let mut g = shared.lock().unwrap();
                        loop {
                            if let Some(d) = g.stack.pop() {
                                g.busy += 1;
                                break d;
                            }
                            if g.busy == 0 {
                                cv.notify_all();
                                return;
                            }
                            g = cv.wait(g).unwrap();
                        }
                    };
                    buf.clear();
                    let ok = ringscan_win::list_dir(&dir, &mut buf).is_ok();
                    let mut g = shared.lock().unwrap();
                    g.busy -= 1;
                    if !ok {
                        g.walk.errors += 1;
                    } else {
                        g.walk.dirs += 1;
                        for e in &buf {
                            g.walk.entries += 1;
                            g.walk.bytes += e.alloc;
                            if e.is_dir && !e.is_link() {
                                g.stack.push(e.path_in(&dir));
                            }
                        }
                    }
                    cv.notify_all();
                }
            });
        }
    });
    shared.into_inner().unwrap().walk
}

fn rate(n: u64, d: Duration) -> String {
    format!("{:>10.0} entries/s", n as f64 / d.as_secs_f64())
}

/// Runs `f` `rounds` times; returns (first, best) durations and the result.
fn time<T>(rounds: usize, mut f: impl FnMut() -> T) -> (Duration, Duration, T) {
    let t = Instant::now();
    let mut out = f();
    let first = t.elapsed();
    let mut best = first;
    for _ in 1..rounds {
        let t = Instant::now();
        out = f();
        best = best.min(t.elapsed());
    }
    (first, best, out)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let big = PathBuf::from(args.get(1).map_or(r"C:\Windows\WinSxS", String::as_str));
    let tree = PathBuf::from(args.get(2).map_or(r"C:\Windows\System32", String::as_str));
    let rounds: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);

    println!("== list_dir on {} ({rounds} rounds)", big.display());
    let (first, best, n) = time(rounds, || {
        let mut v = Vec::new();
        ringscan_win::list_dir(&big, &mut v).expect("list_dir");
        v.len() as u64
    });
    println!(
        "list_dir      : {n} entries, first {first:?}, best {best:?} = {}",
        rate(n, best)
    );
    let (first, best, n) = time(rounds, || {
        std::fs::read_dir(&big).expect("read_dir").count() as u64
    });
    println!(
        "std read_dir  : {n} entries, first {first:?}, best {best:?} = {}",
        rate(n, best)
    );

    println!(
        "== recursive single-thread walk of {} ({rounds} rounds)",
        tree.display()
    );
    let (first, best, w) = time(rounds, || walk_list_dir(&tree));
    println!(
        "list_dir walk : {} entries in {} dirs ({} errors, {:.2} GB allocated), first {first:?}, best {best:?} = {}",
        w.entries,
        w.dirs,
        w.errors,
        w.bytes as f64 / 1e9,
        rate(w.entries, best)
    );
    let (first, best, w) = time(rounds, || walk_std(&tree));
    println!(
        "std walk      : {} entries in {} dirs ({} errors, {:.2} GB logical), first {first:?}, best {best:?} = {}",
        w.entries,
        w.dirs,
        w.errors,
        w.bytes as f64 / 1e9,
        rate(w.entries, best)
    );

    println!(
        "== parallel walk of {} with list_dir ({rounds} rounds)",
        tree.display()
    );
    for threads in [2, 4, 8, 12] {
        let (_, best, w) = time(rounds, || walk_parallel(&tree, threads));
        println!(
            "{threads:>2} threads    : {} entries, best {best:?} = {}",
            w.entries,
            rate(w.entries, best)
        );
    }
}
