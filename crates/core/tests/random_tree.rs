//! Seeded randomized test: after random adds / batches / virtual nodes /
//! raw builds / removes / clears / grafts / sorts, every live node's
//! aggregated sizes and counts must equal a naive bottom-up recomputation
//! from the own sizes recorded at insertion, tombstones must be unreachable
//! and every live node reachable.

use std::collections::{HashMap, HashSet};

use ringscan_core::flags::*;
use ringscan_core::synth::Rng;
use ringscan_core::{NONE, NodeId, RawEntry, SizeMode, Tree, layout};

/// Own (logical, alloc) of every node ever created, by id.
type Own = HashMap<NodeId, (u64, u64)>;

fn random_size(rng: &mut Rng) -> u64 {
    match rng.below(4) {
        0 => 0,
        1 => rng.below(1000),
        2 => rng.below(1 << 20),
        _ => rng.below(1 << 34),
    }
}

fn random_flags(rng: &mut Rng) -> u16 {
    let mut f = 0;
    for flag in [HIDDEN, SYSTEM, COMPRESSED, HARDLINK, REPARSE, CLOUD, ERROR] {
        if rng.chance(0.08) {
            f |= flag;
        }
    }
    f
}

fn pick_live(t: &Tree, rng: &mut Rng, want_dir: bool) -> NodeId {
    let ids: Vec<NodeId> = t
        .live_nodes()
        .filter(|(_, n)| !want_dir || (n.is_dir() && !n.has(FREE)))
        .map(|(id, _)| id)
        .collect();
    *rng.pick(&ids)
}

fn pick_dead(t: &Tree, rng: &mut Rng) -> Option<NodeId> {
    let ids: Vec<NodeId> = (0..t.len() as NodeId)
        .filter(|&id| !t.is_live(id))
        .collect();
    (!ids.is_empty()).then(|| *rng.pick(&ids))
}

fn name(rng: &mut Rng) -> String {
    ringscan_core::synth::file_name(rng)
}

/// Records the own size a node was created with (HARDLINK -> 0).
fn record(t: &Tree, own: &mut Own, id: NodeId, logical: u64, alloc: u64) {
    let z = t.node(id).has(HARDLINK);
    own.insert(id, if z { (0, 0) } else { (logical, alloc) });
}

/// One random live-insertion step; returns false if nothing was done.
fn live_insert(t: &mut Tree, own: &mut Own, rng: &mut Rng) {
    let parent = pick_live(t, rng, true);
    match rng.below(5) {
        0 => {
            let (l, a) = (random_size(rng), random_size(rng));
            let id = t.add_file(
                parent,
                &name(rng),
                l,
                a,
                random_flags(rng),
                rng.below(1 << 31) as i64,
            );
            record(t, own, id, l, a);
        }
        1 => {
            let id = t.add_dir(parent, &name(rng), random_flags(rng) & !HARDLINK, 0);
            record(t, own, id, 0, 0);
        }
        2 => {
            let (l, a) = (random_size(rng), random_size(rng));
            let id = t.add_node(parent, &name(rng), l, a, DIR, 0);
            record(t, own, id, l, a);
        }
        3 => {
            let kind = *rng.pick(&[FREE, UNACCOUNTED, 0, RECYCLE, DIR]);
            let size = random_size(rng);
            let id = t.add_virtual(parent, &format!("<v{}>", rng.below(100)), kind, size);
            record(t, own, id, size, size);
        }
        _ => {
            let n = rng.below(20) as usize;
            let entries: Vec<RawEntry> = (0..n)
                .map(|_| {
                    let is_dir = rng.chance(0.3);
                    let mut flags = random_flags(rng);
                    if rng.chance(0.05) {
                        flags |= *rng.pick(&[FREE, UNACCOUNTED]);
                    }
                    RawEntry {
                        name: name(rng),
                        is_dir,
                        logical: random_size(rng),
                        alloc: random_size(rng),
                        flags,
                        mtime: 0,
                        file_id: 0,
                    }
                })
                .collect();
            let before = t.len() as NodeId;
            let dirs = t.add_batch(parent, &entries);
            for (i, e) in entries.iter().enumerate() {
                record(t, own, before + i as NodeId, e.logical, e.alloc);
            }
            let want: Vec<usize> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.is_dir)
                .map(|(i, _)| i)
                .collect();
            assert_eq!(dirs.iter().map(|d| d.1).collect::<Vec<_>>(), want);
            for (id, i) in dirs {
                assert_eq!(id, before + i as NodeId);
            }
        }
    }
}

/// A small random tree to graft, with its own-size map.
fn graft_source(rng: &mut Rng) -> (Tree, Own) {
    let mut t = Tree::new("src", 0);
    let mut own = Own::new();
    own.insert(0, (0, 0));
    for _ in 0..rng.below(30) {
        live_insert(&mut t, &mut own, rng);
    }
    if rng.chance(0.5) && t.len() > 1 {
        let victim = pick_live(&t, rng, false);
        t.remove(victim);
    }
    (t, own)
}

/// Naive recomputation + structural checks.
fn check(t: &Tree, own: &Own) {
    // Reachability and parent links via an explicit DFS over children().
    let mut order = vec![t.root()];
    let mut seen = HashSet::new();
    let mut stack = vec![t.root()];
    seen.insert(t.root());
    while let Some(n) = stack.pop() {
        for c in t.children(n) {
            assert_eq!(t.node(c).parent(), n, "parent link of {c}");
            assert!(t.is_live(c), "tombstone {c} reachable");
            assert!(seen.insert(c), "node {c} reachable twice (list cycle)");
            order.push(c);
            stack.push(c);
        }
    }
    let live: HashSet<NodeId> = t.live_nodes().map(|(id, _)| id).collect();
    assert_eq!(seen, live, "reachable set == live set");
    assert_eq!(t.live_len(), live.len());
    // descendants() is a pre-order over the same set.
    let desc: HashSet<NodeId> = t.descendants(t.root()).collect();
    assert_eq!(desc.len() + 1, live.len());

    // Bottom-up expected values (children before parents).
    let mut exp: HashMap<NodeId, (u64, u64, u64, u64)> = HashMap::new();
    let pre: Vec<NodeId> = std::iter::once(t.root())
        .chain(t.descendants(t.root()))
        .collect();
    for &n in pre.iter().rev() {
        let (mut l, mut a) = own[&n];
        let (mut files, mut dirs) = (0, 0);
        for c in t.children(n) {
            let cn = t.node(c);
            let (cl, ca, cf, cd) = exp[&c];
            if !cn.has(FREE) {
                l += cl;
                a += ca;
            }
            files += cf + u64::from(cn.is_file());
            dirs += cd + u64::from(cn.is_dir() && !cn.is_virtual());
        }
        exp.insert(n, (l, a, files, dirs));
        let node = t.node(n);
        assert_eq!(
            (node.logical(), node.alloc(), t.files(n), t.dirs(n)),
            (l, a, files, dirs),
            "aggregates of {n} ({})",
            t.name(n)
        );
        if node.has(HARDLINK) {
            assert_eq!((node.logical(), node.alloc()), (0, 0));
        }
    }
    // Free list == live FREE nodes.
    let mut free: Vec<NodeId> = t.free_nodes().collect();
    free.sort_unstable();
    let mut want: Vec<NodeId> = live
        .iter()
        .copied()
        .filter(|&id| t.node(id).has(FREE))
        .collect();
    want.sort_unstable();
    assert_eq!(free, want);
}

fn run(seed: u64, steps: usize) {
    let mut rng = Rng::new(seed);
    let mut t = Tree::new("C:\\", if rng.chance(0.5) { VIRTUAL } else { 0 });
    let mut own = Own::new();
    own.insert(0, (0, 0));
    let mut generation = t.generation();
    for step in 0..steps {
        let op = rng.below(100);
        let before = (
            t.len(),
            t.live_len(),
            t.size(t.root(), SizeMode::Logical),
            t.files(t.root()),
        );
        match op {
            0..=44 => live_insert(&mut t, &mut own, &mut rng),
            45..=54 => {
                // Raw build of a small subtree, finalized by recompute or by
                // the next live operation.
                let mut dirs = vec![pick_live(&t, &mut rng, true)];
                for _ in 0..rng.below(40) {
                    let parent = *rng.pick(&dirs);
                    let (l, a) = (random_size(&mut rng), random_size(&mut rng));
                    if rng.chance(0.3) {
                        let id = t.add_node_raw(parent, &name(&mut rng), l, a, DIR, 0);
                        record(&t, &mut own, id, l, a);
                        dirs.push(id);
                    } else if rng.chance(0.1) {
                        let id = t.add_dir_raw(parent, &name(&mut rng), 0, 0);
                        record(&t, &mut own, id, 0, 0);
                        dirs.push(id);
                    } else {
                        let id = t.add_file_raw(
                            parent,
                            &name(&mut rng),
                            l,
                            a,
                            random_flags(&mut rng),
                            0,
                        );
                        record(&t, &mut own, id, l, a);
                    }
                }
                if rng.chance(0.5) {
                    t.recompute_aggregates();
                } else {
                    live_insert(&mut t, &mut own, &mut rng);
                }
            }
            55..=66 => {
                if t.live_len() > 1 {
                    let mut victim = pick_live(&t, &mut rng, false);
                    if victim == t.root() {
                        victim = t.children(t.root()).next().unwrap_or(victim);
                    }
                    t.remove(victim);
                    assert!(victim == t.root() || !t.is_live(victim));
                }
            }
            67..=71 => {
                let d = pick_live(&t, &mut rng, true);
                t.clear_children(d);
                assert_eq!(t.children(d).count(), 0);
            }
            72..=79 => {
                let (src, src_own) = graft_source(&mut rng);
                let at = pick_live(&t, &mut rng, true);
                let before = t.len() as NodeId;
                t.graft(at, &src);
                // New nodes are src's live nodes in pre-order.
                for (i, s) in src.descendants(src.root()).enumerate() {
                    let id = before + i as NodeId;
                    assert_eq!(t.name(id), src.name(s));
                    own.insert(id, src_own[&s]);
                }
                assert_eq!(
                    t.len() as NodeId - before,
                    src.descendants(src.root()).count() as NodeId
                );
            }
            80..=84 => {
                let virt: Vec<NodeId> = t
                    .live_nodes()
                    .filter(|(_, n)| n.is_virtual() && !n.is_dir())
                    .map(|(id, _)| id)
                    .collect();
                if let Some(&v) = virt.get(rng.below(virt.len().max(1) as u64) as usize) {
                    let size = random_size(&mut rng);
                    t.set_virtual_size(v, size);
                    own.insert(v, (size, size));
                } else {
                    t.set_flags(t.root(), ERROR, 0);
                }
            }
            85..=89 => {
                let d = pick_live(&t, &mut rng, true);
                let mode = if rng.chance(0.5) {
                    SizeMode::Logical
                } else {
                    SizeMode::Allocated
                };
                t.sort_children(d, mode);
                let sizes: Vec<u64> = t.children(d).map(|c| t.size(c, mode)).collect();
                assert!(sizes.windows(2).all(|w| w[0] >= w[1]), "sorted desc");
            }
            90..=91 => t.sort_all(SizeMode::Allocated),
            _ => {
                // Insertion below a tombstone must not change live totals.
                if let Some(dead) = pick_dead(&t, &mut rng) {
                    let total = t.size(t.root(), SizeMode::Logical);
                    let id = t.add_file(dead, "ghost", 1 << 40, 1 << 40, 0, 0);
                    own.insert(id, (1 << 40, 1 << 40));
                    assert!(!t.is_live(id));
                    assert!(
                        t.add_batch(
                            dead,
                            &[RawEntry {
                                name: "g".into(),
                                logical: 5,
                                ..Default::default()
                            }]
                        )
                        .is_empty()
                    );
                    assert_eq!(t.size(t.root(), SizeMode::Logical), total);
                } else {
                    t.set_flags(t.root(), HIDDEN, 0);
                }
            }
        }
        // Every observable change bumps the generation; no-ops may not.
        let after = (
            t.len(),
            t.live_len(),
            t.size(t.root(), SizeMode::Logical),
            t.files(t.root()),
        );
        if after != before {
            assert!(
                t.generation() > generation,
                "step {step} op {op}: generation must grow"
            );
        }
        assert!(t.generation() >= generation);
        generation = t.generation();
        check(&t, &own);
        if step % 50 == 0 {
            // Layout never sees tombstones and its ring 1 matches the root.
            let l = layout::compute(
                &t,
                t.root(),
                &layout::Params {
                    min_angle: 0.0,
                    ..Default::default()
                },
            );
            for a in &l.arcs {
                if let Some(id) = a.node() {
                    assert!(t.is_live(id));
                }
            }
            assert!(l.arcs.iter().all(|a| a.kind != layout::ArcKind::Node(NONE)));
        }
    }
    eprintln!(
        "seed {seed}: {steps} steps, arena {} nodes, {} live, {} files, {} dirs, {} free nodes",
        t.len(),
        t.live_len(),
        t.files(t.root()),
        t.dirs(t.root()),
        t.free_nodes().count()
    );
}

#[test]
fn random_operations_keep_aggregates_exact() {
    for seed in 0..12 {
        run(seed, 400);
    }
}

#[test]
fn long_random_run() {
    run(0xfeed, 2500);
}
