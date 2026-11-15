//! Sunburst geometry.
//!
//! [`compute`] turns `(tree, focus, params)` into a flat list of [`Arc`]s:
//! ring 1 (innermost) holds the children of the focus, ring 2 their
//! children and so on, up to `max_depth` rings. Angles are radians in
//! `0..=TAU`; where angle 0 points and the direction are up to the renderer.
//! The focus itself is not an arc: it is the disc in the middle, sized
//! [`Layout::total`].
//!
//! # Algorithm
//!
//! Breadth-first, one ring at a time. For every expandable arc of the
//! previous ring, one pass over its children computes each displayed size
//! and splits them into *visible* (span >= `min_angle`) and *small* ones.
//! Only the visible ones are sorted (size descending, ties by name), and
//! there are at most `TAU / min_angle` of them per ring, so a directory with
//! a million tiny files costs one linear pass and no big sort. Spans are
//! cumulative (`a0 + cum * scale` in f64), so rounding never drifts and the
//! children tile their parent's span exactly; a directory's own size (e.g.
//! an NTFS index allocation) stays as a gap at the end of its span.
//!
//! Because rings are emitted one after another, the arcs of each ring are
//! contiguous in [`Layout::arcs`] and already sorted by `a0`, so
//! [`Layout::hit`] is a binary search and [`Layout::ring`] a slice.
//!
//! # Rest arcs ("N small items")
//!
//! The small children of one parent are merged into one
//! `ArcKind::Rest { parent, count }` placed after the visible children (at
//! the end of the parent's span). It is emitted iff at least one child is
//! small and the merged span is itself `> 0` and `>= min_angle`; otherwise
//! that slice stays empty (it would be invisible anyway). `count` includes
//! zero-size children. Rest arcs are never expanded. Zero-size children are
//! never visible on their own, and spans below [`MIN_SPAN`] (1e-6 rad, far
//! below a pixel) count as small even with `min_angle == 0`, so every arc
//! has `a0 < a1` in f32.
//!
//! # Free space and virtual nodes
//!
//! Displayed size = the tree size, plus, with `show_free`, the sizes of the
//! FREE nodes below the node (tree sizes never include free space; see the
//! aggregation rules in [`crate::tree`]). So with `show_free` a drive root
//! spans used + free, its FREE child is an ordinary leaf arc and a "This PC"
//! root spans the sum of the capacities; without it FREE nodes are skipped
//! and no rescaling is needed. Other virtual leaves (UNACCOUNTED, ...) are
//! ordinary arcs; check the node flags to style them.
//!
//! # Focus
//!
//! Any live node can be the focus, files and virtual nodes included;
//! `total` is its displayed size and a leaf focus has no arcs. A tombstone
//! focus gives an empty layout with `total == 0` (move the focus with
//! [`Tree::live_ancestor`]); so does a zero-size focus.
//!
//! # Cost
//!
//! O(children of the expanded directories) + O(v log v) for the visible
//! arcs. `tests/perf.rs` benchmarks a realistic 2M-node tree at
//! `min_angle = 0.005`, `max_depth = 12` against the 5 ms budget.

use std::f64::consts::TAU;

use crate::flags::FREE;
use crate::tree::{Tree, cmp_names};
use crate::{NONE, NodeId, SizeMode};

/// Smallest span (radians) ever emitted, whatever `Params::min_angle` says:
/// keeps `a0 < a1` true after rounding the angles to f32.
pub const MIN_SPAN: f64 = 1e-6;

/// Layout parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Which size drives the angles.
    pub mode: SizeMode,
    /// Number of rings (0 = no arcs).
    pub max_depth: u8,
    /// Smallest span (radians) of an arc of its own; smaller children are
    /// merged into a Rest arc.
    pub min_angle: f32,
    /// Show FREE (free space) nodes.
    pub show_free: bool,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            mode: SizeMode::Allocated,
            max_depth: 12,
            min_angle: 0.005,
            show_free: true,
        }
    }
}

/// What an arc represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArcKind {
    /// A tree node (file, directory or virtual node).
    Node(NodeId),
    /// `count` children of `parent` too small to draw individually.
    Rest { parent: NodeId, count: u32 },
}

/// One ring segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Arc {
    pub kind: ArcKind,
    /// Ring, 1 = innermost (children of the focus).
    pub depth: u8,
    /// Start angle, radians (`a0 < a1`).
    pub a0: f32,
    /// End angle, radians (`<= TAU`).
    pub a1: f32,
    /// Displayed size in bytes (per `Params::mode`).
    pub size: u64,
}

impl Arc {
    /// The node of a `Node` arc.
    pub fn node(&self) -> Option<NodeId> {
        match self.kind {
            ArcKind::Node(id) => Some(id),
            ArcKind::Rest { .. } => None,
        }
    }

    /// Angular span in radians.
    pub fn span(&self) -> f32 {
        self.a1 - self.a0
    }
}

/// Result of [`compute`].
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// The focus node (center disc).
    pub focus: NodeId,
    /// Displayed size of the focus; arc angles are `size / total * TAU`.
    pub total: u64,
    /// All arcs, grouped by ring (inner first), each ring sorted by `a0`.
    pub arcs: Vec<Arc>,
    /// Number of non-empty rings.
    pub depth: u8,
    /// `rings[d]` = end index in `arcs` of ring `d` (`rings[0] == 0`).
    rings: Vec<u32>,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            focus: NONE,
            total: 0,
            arcs: Vec::new(),
            depth: 0,
            rings: vec![0],
        }
    }
}

impl Layout {
    /// The arcs of ring `depth` (1-based), sorted by `a0`; empty if out of
    /// range.
    pub fn ring(&self, depth: u8) -> &[Arc] {
        let d = usize::from(depth);
        if d == 0 || d >= self.rings.len() {
            return &[];
        }
        &self.arcs[self.rings[d - 1] as usize..self.rings[d] as usize]
    }

    /// The arc of ring `depth` covering `angle` (radians, any value; wrapped
    /// into `0..TAU`), or None if that spot is empty. Arcs cover `[a0, a1)`.
    pub fn hit(&self, angle: f32, depth: u8) -> Option<&Arc> {
        let ring = self.ring(depth);
        if ring.is_empty() || !angle.is_finite() {
            return None;
        }
        let tau = TAU as f32;
        let mut a = angle.rem_euclid(tau);
        if a >= tau {
            a = 0.0;
        }
        let i = ring.partition_point(|arc| arc.a0 <= a);
        let arc = ring.get(i.checked_sub(1)?)?;
        (a < arc.a1).then_some(arc)
    }

    /// The arc showing node `id`, if it is visible.
    pub fn find(&self, id: NodeId) -> Option<&Arc> {
        self.arcs.iter().find(|a| a.kind == ArcKind::Node(id))
    }
}

/// Sizes of the FREE nodes below each ancestor of a FREE node (tiny: one
/// entry per ancestor of each volume's free-space node).
struct FreeExtra {
    by_node: Vec<(NodeId, u64)>,
}

impl FreeExtra {
    fn build(tree: &Tree, mode: SizeMode) -> FreeExtra {
        let mut by_node: Vec<(NodeId, u64)> = Vec::new();
        for f in tree.free_nodes() {
            let size = tree.size(f, mode);
            if size == 0 {
                continue;
            }
            for a in tree.ancestors(f) {
                match by_node.iter_mut().find(|e| e.0 == a) {
                    Some(e) => e.1 += size,
                    None => by_node.push((a, size)),
                }
            }
        }
        FreeExtra { by_node }
    }

    #[inline]
    fn get(&self, id: NodeId) -> u64 {
        if self.by_node.is_empty() {
            return 0;
        }
        self.by_node.iter().find(|e| e.0 == id).map_or(0, |e| e.1)
    }
}

/// A node whose children go into the next ring.
#[derive(Clone, Copy)]
struct Parent {
    id: NodeId,
    a0: f64,
    a1: f64,
    size: u64,
}

/// Computes the sunburst for `focus`. See the [module docs](self).
pub fn compute(tree: &Tree, focus: NodeId, p: &Params) -> Layout {
    let mut out = Layout {
        focus,
        ..Layout::default()
    };
    if !tree.is_live(focus) {
        return out;
    }
    let extra = if p.show_free {
        FreeExtra::build(tree, p.mode)
    } else {
        FreeExtra {
            by_node: Vec::new(),
        }
    };
    let fnode = tree.node(focus);
    out.total = if fnode.has(FREE) && !p.show_free {
        0
    } else {
        fnode.size(p.mode) + extra.get(focus)
    };
    if out.total == 0 || p.max_depth == 0 {
        return out;
    }
    // Below ~2 f32 ulps near TAU an arc could collapse to a0 == a1.
    let min_angle = f64::from(p.min_angle).max(MIN_SPAN);
    let mut level = vec![Parent {
        id: focus,
        a0: 0.0,
        a1: TAU,
        size: out.total,
    }];
    let mut next = Vec::new();
    let mut visible: Vec<(u64, NodeId)> = Vec::new();
    for depth in 1..=p.max_depth {
        let expand_further = depth < p.max_depth;
        for par in &level {
            expand(
                tree,
                par,
                depth,
                p,
                &extra,
                min_angle,
                expand_further,
                &mut visible,
                &mut out.arcs,
                &mut next,
            );
        }
        let start = *out.rings.last().unwrap_or(&0) as usize;
        if out.arcs.len() == start {
            break;
        }
        out.rings.push(out.arcs.len() as u32);
        out.depth = depth;
        std::mem::swap(&mut level, &mut next);
        next.clear();
        if level.is_empty() {
            break;
        }
    }
    out
}

/// Emits the arcs of the children of `par` at ring `depth`.
#[allow(clippy::too_many_arguments)]
fn expand(
    tree: &Tree,
    par: &Parent,
    depth: u8,
    p: &Params,
    extra: &FreeExtra,
    min_angle: f64,
    expand_further: bool,
    visible: &mut Vec<(u64, NodeId)>,
    arcs: &mut Vec<Arc>,
    next: &mut Vec<Parent>,
) {
    let scale = (par.a1 - par.a0) / par.size as f64;
    // Only ancestors of a FREE node can have children with extra size.
    let with_extra = extra.get(par.id) > 0;
    visible.clear();
    let mut rest_count = 0u32;
    let mut rest_size = 0u64;
    for c in tree.children(par.id) {
        let n = tree.node(c);
        if n.has(FREE) && !p.show_free {
            continue;
        }
        let size = n.size(p.mode) + if with_extra { extra.get(c) } else { 0 };
        if size > 0 && size as f64 * scale >= min_angle {
            visible.push((size, c));
        } else {
            rest_count += 1;
            rest_size += size;
        }
    }
    visible.sort_unstable_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| cmp_names(tree.name(a.1), tree.name(b.1)))
            .then(a.1.cmp(&b.1))
    });
    let mut cum = 0u64;
    for &(size, c) in visible.iter() {
        let a0 = par.a0 + cum as f64 * scale;
        cum += size;
        let a1 = par.a0 + cum as f64 * scale;
        arcs.push(Arc {
            kind: ArcKind::Node(c),
            depth,
            a0: a0 as f32,
            a1: a1 as f32,
            size,
        });
        if expand_further && tree.has_children(c) {
            next.push(Parent {
                id: c,
                a0,
                a1,
                size,
            });
        }
    }
    if rest_count > 0 && rest_size > 0 {
        let a0 = par.a0 + cum as f64 * scale;
        let a1 = par.a0 + (cum + rest_size) as f64 * scale;
        if a1 - a0 >= min_angle {
            arcs.push(Arc {
                kind: ArcKind::Rest {
                    parent: par.id,
                    count: rest_count,
                },
                depth,
                a0: a0 as f32,
                a1: a1 as f32,
                size: rest_size,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags::*;

    const T: f32 = TAU as f32;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    fn params(max_depth: u8, min_angle: f32, show_free: bool) -> Params {
        Params {
            mode: SizeMode::Logical,
            max_depth,
            min_angle,
            show_free,
        }
    }

    #[test]
    fn spans_are_proportional_and_sorted() {
        let mut t = Tree::new("r", 0);
        let a = t.add_file(0, "a", 100, 0, 0, 0);
        let b = t.add_file(0, "b", 300, 0, 0, 0);
        let c = t.add_file(0, "c", 600, 0, 0, 0);
        let l = compute(&t, 0, &params(5, 0.0, true));
        assert_eq!(l.total, 1000);
        assert_eq!(l.depth, 1);
        let ids: Vec<_> = l.arcs.iter().map(|x| x.node().unwrap()).collect();
        assert_eq!(ids, vec![c, b, a]);
        assert!(approx(l.arcs[0].a0, 0.0) && approx(l.arcs[0].a1, 0.6 * T));
        assert!(approx(l.arcs[1].a1, 0.9 * T));
        assert!(approx(l.arcs[2].a1, T));
        assert_eq!(l.arcs[2].size, 100);
        // Allocated mode: all zero -> nothing.
        let l = compute(
            &t,
            0,
            &Params {
                mode: SizeMode::Allocated,
                ..params(5, 0.0, true)
            },
        );
        assert_eq!(l.total, 0);
        assert!(l.arcs.is_empty());
    }

    #[test]
    fn nested_rings_and_max_depth() {
        let mut t = Tree::new("r", 0);
        let mut parent = 0;
        let mut chain = vec![];
        for i in 0..6 {
            parent = t.add_dir(parent, &format!("d{i}"), 0, 0);
            chain.push(parent);
            t.add_file(parent, "f", 10, 10, 0, 0);
        }
        let l = compute(&t, 0, &params(3, 0.0, true));
        assert_eq!(l.depth, 3);
        assert!(l.arcs.iter().all(|a| a.depth <= 3));
        assert_eq!(l.ring(1).len(), 1);
        assert_eq!(l.ring(2).len(), 2); // d1 + f
        assert_eq!(l.ring(4).len(), 0);
        assert_eq!(l.ring(0).len(), 0);
        // d0 contains 60 bytes; its file f (10) is in ring 2 after d1 (50).
        let r2 = l.ring(2);
        assert_eq!(r2[0].node(), Some(chain[1]));
        assert!(approx(r2[0].a1, 50.0 / 60.0 * T));
        // Focus deeper.
        let l = compute(&t, chain[4], &params(12, 0.0, true));
        assert_eq!(l.total, 20);
        assert_eq!(l.depth, 2);
        assert_eq!(compute(&t, 0, &params(0, 0.0, true)).arcs.len(), 0);
    }

    #[test]
    fn small_children_merge_into_rest() {
        let mut t = Tree::new("r", 0);
        let big = t.add_file(0, "big", 10_000, 0, 0, 0);
        for i in 0..100 {
            t.add_file(0, &format!("s{i}"), 1, 0, 0, 0);
        }
        t.add_file(0, "empty", 0, 0, 0, 0);
        // Each small file spans TAU/10100 = 0.00062 rad; together 0.062 rad.
        let l = compute(&t, 0, &params(1, 0.005, true));
        assert_eq!(l.arcs.len(), 2);
        assert_eq!(l.arcs[0].node(), Some(big));
        assert_eq!(
            l.arcs[1].kind,
            ArcKind::Rest {
                parent: 0,
                count: 101
            }
        );
        assert_eq!(l.arcs[1].size, 100);
        assert!(approx(l.arcs[1].a1, T));
        // Rest itself below min_angle -> not emitted.
        let l = compute(&t, 0, &params(1, 0.1, true));
        assert_eq!(l.arcs.len(), 1);
        // min_angle 0: every non-empty child is visible; the empty one is not.
        let l = compute(&t, 0, &params(1, 0.0, true));
        assert_eq!(l.arcs.len(), 101);
    }

    #[test]
    fn free_space_shown_or_hidden() {
        let mut t = Tree::new("This PC", VIRTUAL);
        let c = t.add_dir(0, "C:\\", 0, 0);
        let d = t.add_dir(0, "D:\\", 0, 0);
        let w = t.add_dir(c, "Windows", 0, 0);
        t.add_file(w, "big", 500, 500, 0, 0);
        t.add_virtual(c, "<unaccounted>", UNACCOUNTED, 100);
        let fc = t.add_virtual(c, "<free>", FREE, 400);
        t.add_file(d, "x", 50, 50, 0, 0);
        let fd = t.add_virtual(d, "<free>", FREE, 950);

        let on = compute(&t, 0, &params(3, 0.0, true));
        assert_eq!(on.total, 2000);
        let r1 = on.ring(1);
        assert_eq!(r1[0].node(), Some(c), "1000 incl. free");
        assert_eq!(r1[0].size, 1000);
        assert_eq!(r1[1].size, 1000);
        let r2 = on.ring(2);
        assert_eq!(r2.len(), 5);
        assert!(r2.iter().any(|a| a.node() == Some(fc) && a.size == 400));
        assert!(r2.iter().any(|a| a.node() == Some(fd) && a.size == 950));

        let off = compute(&t, 0, &params(3, 0.0, false));
        assert_eq!(off.total, 650);
        assert_eq!(off.ring(1)[0].size, 600);
        assert!(
            off.arcs
                .iter()
                .all(|a| a.node() != Some(fc) && a.node() != Some(fd))
        );
        assert!(approx(off.ring(1)[0].a1, 600.0 / 650.0 * T));

        // Focus on the drive / below it / on the FREE node itself.
        assert_eq!(compute(&t, c, &params(3, 0.0, true)).total, 1000);
        assert_eq!(compute(&t, c, &params(3, 0.0, false)).total, 600);
        assert_eq!(compute(&t, w, &params(3, 0.0, true)).total, 500);
        assert_eq!(compute(&t, fc, &params(3, 0.0, true)).total, 400);
        assert_eq!(compute(&t, fc, &params(3, 0.0, false)).total, 0);
    }

    #[test]
    fn focus_on_file_dead_node_and_ties() {
        let mut t = Tree::new("r", 0);
        let f = t.add_file(0, "f", 7, 7, 0, 0);
        let l = compute(&t, f, &params(3, 0.0, true));
        assert_eq!((l.total, l.arcs.len(), l.focus), (7, 0, f));
        let d = t.add_dir(0, "d", 0, 0);
        let b = t.add_file(d, "B", 5, 5, 0, 0);
        let a = t.add_file(d, "a", 5, 5, 0, 0);
        let l = compute(&t, d, &params(3, 0.0, true));
        assert_eq!(
            l.arcs.iter().map(|x| x.node().unwrap()).collect::<Vec<_>>(),
            vec![a, b]
        );
        t.remove(d);
        let l = compute(&t, d, &params(3, 0.0, true));
        assert_eq!((l.total, l.arcs.len()), (0, 0));
        let l = compute(&t, 12345, &params(3, 0.0, true));
        assert_eq!(l.total, 0);
    }

    #[test]
    fn sub_ulp_spans_are_never_emitted() {
        let mut t = Tree::new("r", 0);
        t.add_file(0, "huge", 1 << 50, 1 << 50, 0, 0);
        t.add_file(0, "tiny", 1, 1, 0, 0);
        let l = compute(&t, 0, &params(1, 0.0, true));
        assert_eq!(l.arcs.len(), 1, "1-byte file next to 1 PB spans 6e-15 rad");
        assert!(l.arcs.iter().all(|a| a.a0 < a.a1));
        let l = compute(&t, 0, &params(1, f32::NAN, true));
        assert_eq!(l.arcs.len(), 1, "NaN min_angle behaves like 0");
    }

    #[test]
    fn own_size_leaves_gap() {
        let mut t = Tree::new("r", 0);
        let d = t.add_node(0, "d", 50, 50, DIR, 0);
        t.add_file(d, "f", 50, 50, 0, 0);
        let l = compute(&t, 0, &params(2, 0.0, true));
        let f = l.ring(2)[0];
        assert!(approx(f.a0, 0.0) && approx(f.a1, T / 2.0));
    }

    #[test]
    fn hit_testing() {
        let mut t = Tree::new("r", 0);
        let a = t.add_file(0, "a", 3, 3, 0, 0);
        let d = t.add_dir(0, "d", 0, 0);
        let x = t.add_file(d, "x", 1, 1, 0, 0);
        let l = compute(&t, 0, &params(4, 0.0, true));
        // ring 1: a [0, 0.75T), d [0.75T, T); ring 2: x [0.75T, T)
        assert_eq!(l.hit(0.0, 1).unwrap().node(), Some(a));
        assert_eq!(l.hit(0.5 * T, 1).unwrap().node(), Some(a));
        assert_eq!(l.hit(0.75 * T, 1).unwrap().node(), Some(d));
        assert_eq!(l.hit(0.9 * T, 1).unwrap().node(), Some(d));
        assert_eq!(
            l.hit(-0.1, 1).unwrap().node(),
            Some(d),
            "wraps negative angles"
        );
        assert_eq!(
            l.hit(T + 0.1, 1).unwrap().node(),
            Some(a),
            "wraps large angles"
        );
        assert!(l.hit(0.5 * T, 2).is_none(), "empty spot in ring 2");
        assert_eq!(l.hit(0.8 * T, 2).unwrap().node(), Some(x));
        assert!(l.hit(0.8 * T, 3).is_none());
        assert!(l.hit(0.8 * T, 0).is_none());
        assert!(l.hit(f32::NAN, 1).is_none());
        assert_eq!(l.find(x).unwrap().depth, 2);
        assert!(l.find(12345).is_none());
        assert!(approx(l.find(d).unwrap().span(), 0.25 * T));
    }

    #[test]
    fn rings_tile_parents_on_random_trees() {
        let t = crate::synth::tree(7, 20_000);
        for &min_angle in &[0.0f32, 0.002, 0.05] {
            let l = compute(
                &t,
                0,
                &Params {
                    min_angle,
                    ..Params::default()
                },
            );
            assert!(l.depth >= 3);
            for d in 1..=l.depth {
                let ring = l.ring(d);
                for w in ring.windows(2) {
                    assert!(w[0].a1 <= w[1].a0 + 1e-4, "sorted, non-overlapping");
                }
                for a in ring {
                    assert!(a.a0 < a.a1 && a.a0 >= 0.0 && a.a1 <= T + 1e-4);
                    assert!(f64::from(a.span()) + 1e-4 >= f64::from(min_angle));
                    assert_eq!(a.depth, d);
                    // Children stay within the parent's span.
                    let parent = match a.kind {
                        ArcKind::Node(id) => t.node(id).parent(),
                        ArcKind::Rest { parent, .. } => parent,
                    };
                    if d > 1 {
                        let pa = l.find(parent).unwrap();
                        assert!(a.a0 >= pa.a0 - 1e-4 && a.a1 <= pa.a1 + 1e-4);
                    } else {
                        assert_eq!(parent, 0);
                    }
                }
            }
            // Ring 1 + its Rest covers the whole circle (root has no own size).
            let covered: f32 = l.ring(1).iter().map(Arc::span).sum();
            if min_angle == 0.0 {
                assert!((covered - T).abs() < 1e-3, "covered {covered}");
            }
        }
    }
}
