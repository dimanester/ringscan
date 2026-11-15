//! Chart model: everything the sunburst needs, copied out of the tree.
//!
//! [`ChartModel::build`] runs under one short read lock: it computes the
//! core layout for the focus and copies the few per-arc facts the renderer
//! needs (flags, color, a label for wide arcs, the parent arc). After that
//! the UI draws, hit-tests and animates without touching the tree, so the
//! scanner threads are never blocked by rendering. Models are immutable and
//! shared as `Arc<ChartModel>`; a zoom animation keeps the old and the new
//! one alive at the same time.
//!
//! Parent links between arcs are recovered geometrically: the core layout
//! emits rings breadth-first with each ring sorted by angle, and every child
//! arc lies inside its parent's span, so a binary search on the previous
//! ring finds the parent of each arc in O(log n). With them, colors that
//! depend on content (file types, age) are aggregated bottom-up over the
//! visible arcs in one reverse pass; folders whose content is not visible
//! (outer ring, everything merged into "smaller items") get a bounded walk
//! of their subtree instead.
//!
//! # Extensions
//!
//! A leaf leaves the rings above it empty. For files that is the normal
//! sunburst look, but the virtual leaves `<unaccounted>`, `<reserved by
//! Windows>` and `<free space>` can be several percent of a drive, and the
//! empty wedge above them reads as a hole in the chart. Their arcs are
//! therefore *extended*: the renderer also fills the wedge from their ring
//! up to the outer edge of the deepest ring ([`ChartModel::extended`]), and
//! hit testing maps that wedge back to the arc
//! ([`ChartModel::hit_extension`]).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui::Color32;
use ringscan_core::layout::{self, ArcKind, Layout, Params};
use ringscan_core::{Lang, NodeId, SizeMode, Tree, flags};

use crate::colors::{self, Group};
use crate::settings::ColorMode;

/// "No parent arc" (ring 1).
pub const NO_PARENT: u32 = u32::MAX;

/// Arcs at least this wide (radians) carry their name for labels.
const LABEL_MIN_SPAN: f32 = 0.018;

/// Budget (nodes) for walking the subtrees of folders whose content is not
/// visible, per model build.
const SUBTREE_BUDGET: usize = 400_000;
/// ... and per folder.
const SUBTREE_BUDGET_EACH: usize = 20_000;

/// Search highlight state of an arc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// No search active.
    None,
    /// The node matches the search.
    Match,
    /// A match lies somewhere below.
    Contains,
    /// Unrelated to the matches (drawn dimmed).
    Dim,
}

/// One drawable arc.
#[derive(Clone, Debug)]
pub struct ArcInfo {
    pub kind: ArcKind,
    /// Ring, 1 = innermost.
    pub depth: u8,
    pub a0: f32,
    pub a1: f32,
    pub size: u64,
    /// Node flags (0 for Rest arcs).
    pub flags: u16,
    /// Index of the parent arc, [`NO_PARENT`] on ring 1.
    pub parent: u32,
    pub color: Color32,
    /// A directory with children (zoomable).
    pub zoomable: bool,
    /// Name for in-arc labels (wide arcs only).
    pub label: Option<String>,
    pub mark: Mark,
}

impl ArcInfo {
    pub fn node(&self) -> Option<NodeId> {
        match self.kind {
            ArcKind::Node(id) => Some(id),
            ArcKind::Rest { .. } => None,
        }
    }

    pub fn is_rest(&self) -> bool {
        matches!(self.kind, ArcKind::Rest { .. })
    }

    pub fn mid(&self) -> f32 {
        (self.a0 + self.a1) * 0.5
    }

    pub fn span(&self) -> f32 {
        self.a1 - self.a0
    }
}

/// Facts about the focus (center disc).
#[derive(Clone, Debug, Default)]
pub struct Center {
    pub name: String,
    /// Used bytes of the focus (free space never included).
    pub size: u64,
    /// Displayed size: `size` plus free space below it when shown; the
    /// whole circle.
    pub display: u64,
    /// Free space of the volumes at or below the focus (a drive root,
    /// "This PC"), whether the chart shows it or not; 0 for folders.
    pub free: u64,
    pub files: u64,
    pub dirs: u64,
    pub flags: u16,
    pub is_root: bool,
}

/// Search hits for highlighting: matching nodes and their ancestors.
#[derive(Clone, Debug, Default)]
pub struct Highlight {
    pub matches: HashSet<NodeId>,
    pub ancestors: HashSet<NodeId>,
}

/// Inputs that identify a model; a cached model is reused while equal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelKey {
    /// The scan whose tree the model shows (`Scan::id`): generations and
    /// node ids of different trees must never match.
    pub scan: u64,
    pub generation: u64,
    pub focus: NodeId,
    pub params: Params,
    pub color_mode: ColorMode,
    pub dark: bool,
    /// UI language: labels and the center name hold localized names of
    /// virtual nodes ("This PC", free space, unaccounted).
    pub lang: Lang,
    /// Serial of the search highlight (0 = none).
    pub search: u64,
    /// Reference time for age colors (unix seconds).
    pub now: i64,
}

/// See the [module docs](self).
pub struct ChartModel {
    /// Unique per build (cache key for meshes).
    pub serial: u64,
    pub key: ModelKey,
    pub layout: Layout,
    pub arcs: Vec<ArcInfo>,
    /// `ring_start[d - 1]..ring_start[d]` = arcs of ring d.
    pub ring_start: Vec<u32>,
    pub center: Center,
    /// Indices of the arcs drawn with an extension up to the outer edge
    /// of ring [`ChartModel::depth`] (see [Extensions](self#extensions)):
    /// virtual leaves below the deepest ring. A handful per model.
    pub extended: Vec<u32>,
    by_node: HashMap<NodeId, u32>,
}

/// Does a node with these flags get an extension when its arc is below the
/// deepest ring? Virtual leaves only: unaccounted and reserved space
/// (UNACCOUNTED) and free space (FREE).
pub fn is_extended(node_flags: u16) -> bool {
    node_flags & flags::VIRTUAL != 0
        && node_flags & flags::DIR == 0
        && node_flags & (flags::UNACCOUNTED | flags::FREE) != 0
}

static SERIAL: AtomicU64 = AtomicU64::new(1);

/// Display name of a node: localized names for virtual nodes.
pub fn display_name(tree: &Tree, id: NodeId, names: &VirtualNames) -> String {
    let n = tree.node(id);
    if n.has(flags::FREE) {
        names.free.to_string()
    } else if n.has(flags::UNACCOUNTED) && tree.name(id) == ringscan_engine::RESERVED_NAME {
        names.reserved.to_string()
    } else if n.has(flags::UNACCOUNTED) {
        names.unaccounted.to_string()
    } else if id == tree.root() && n.is_virtual() {
        names.this_pc.to_string()
    } else if n.is_virtual() && n.is_dir() && tree.name(id) == ringscan_engine::ORPHANED_NAME {
        names.orphaned.to_string()
    } else {
        tree.name(id).to_string()
    }
}

/// Free space of the volumes below `id`: the sum of its FREE descendants
/// (one per scanned drive; tree sizes never include them).
pub fn free_below(tree: &Tree, id: NodeId, mode: SizeMode) -> u64 {
    tree.free_nodes()
        .filter(|&f| f != id && tree.is_ancestor(id, f))
        .map(|f| tree.node(f).size(mode))
        .sum()
}

/// Like [`display_name`], but a scanned root folder (`C:\Users\x`, whose
/// name is its full path) shows only its last component (`x`).
pub fn short_name(tree: &Tree, id: NodeId, names: &VirtualNames) -> String {
    let name = display_name(tree, id, names);
    if id != tree.root() || tree.node(id).is_virtual() {
        return name;
    }
    let trimmed = name.trim_end_matches(['\\', '/']);
    match trimmed.rsplit_once(['\\', '/']) {
        Some((_, last)) if !last.is_empty() => last.to_string(),
        _ => name,
    }
}

/// Localized names of the virtual nodes.
#[derive(Clone, Copy)]
pub struct VirtualNames {
    pub this_pc: &'static str,
    pub free: &'static str,
    pub unaccounted: &'static str,
    /// The engine's `<reserved by Windows>` node (an UNACCOUNTED leaf).
    pub reserved: &'static str,
    /// The engine's `<orphaned>` folder (MFT records without a parent).
    pub orphaned: &'static str,
}

impl ChartModel {
    /// An empty model (no tree yet).
    pub fn empty(key: ModelKey) -> ChartModel {
        ChartModel {
            serial: SERIAL.fetch_add(1, Ordering::Relaxed),
            key,
            layout: Layout::default(),
            arcs: Vec::new(),
            ring_start: vec![0],
            center: Center::default(),
            extended: Vec::new(),
            by_node: HashMap::new(),
        }
    }

    /// Builds the model for `key.focus`. Call under a read lock and keep it
    /// short: O(arcs log arcs) plus bounded subtree walks.
    pub fn build(
        tree: &Tree,
        key: ModelKey,
        highlight: Option<&Highlight>,
        names: &VirtualNames,
    ) -> ChartModel {
        let focus = if tree.is_live(key.focus) {
            key.focus
        } else {
            tree.live_ancestor(key.focus)
        };
        let layout = layout::compute(tree, focus, &key.params);
        let mode = key.params.mode;
        let n = layout.arcs.len();
        let mut ring_start = vec![0u32];
        for d in 1..=layout.depth {
            ring_start.push(ring_start[usize::from(d) - 1] + layout.ring(d).len() as u32);
        }
        let mut arcs: Vec<ArcInfo> = Vec::with_capacity(n);
        let mut by_node = HashMap::with_capacity(n);
        for (i, a) in layout.arcs.iter().enumerate() {
            let (flags, zoomable, label) = match a.kind {
                ArcKind::Node(id) => {
                    let node = tree.node(id);
                    by_node.insert(id, i as u32);
                    let label = (a.span() >= LABEL_MIN_SPAN).then(|| display_name(tree, id, names));
                    (node.flags(), node.is_dir() && tree.has_children(id), label)
                }
                ArcKind::Rest { .. } => (0, false, None),
            };
            let parent = if a.depth <= 1 {
                NO_PARENT
            } else {
                let d = usize::from(a.depth) - 1;
                let (s, e) = (ring_start[d - 1] as usize, ring_start[d] as usize);
                let ring = &layout.arcs[s..e];
                let mid = (a.a0 + a.a1) * 0.5;
                let k = ring.partition_point(|p| p.a0 <= mid).saturating_sub(1);
                (s + k) as u32
            };
            let mark = match (highlight, a.kind) {
                (None, _) => Mark::None,
                (Some(h), ArcKind::Node(id)) if h.matches.contains(&id) => Mark::Match,
                (Some(h), ArcKind::Node(id)) if h.ancestors.contains(&id) => Mark::Contains,
                (Some(h), ArcKind::Rest { parent, .. }) if h.ancestors.contains(&parent) => {
                    Mark::Contains
                }
                (Some(_), _) => Mark::Dim,
            };
            arcs.push(ArcInfo {
                kind: a.kind,
                depth: a.depth,
                a0: a.a0,
                a1: a.a1,
                size: a.size,
                flags,
                parent,
                color: Color32::GRAY,
                zoomable,
                label,
                mark,
            });
        }
        color_arcs(tree, &mut arcs, key.color_mode, key.dark, mode, key.now);
        let extended = arcs
            .iter()
            .enumerate()
            .filter(|(_, a)| a.depth < layout.depth && a.node().is_some() && is_extended(a.flags))
            .map(|(i, _)| i as u32)
            .collect();

        let fnode = tree.node(focus);
        let center = Center {
            name: short_name(tree, focus, names),
            size: if fnode.has(flags::FREE) {
                layout.total
            } else {
                fnode.size(mode)
            },
            display: layout.total,
            free: free_below(tree, focus, mode),
            files: tree.files(focus),
            dirs: tree.dirs(focus),
            flags: fnode.flags(),
            is_root: focus == tree.root(),
        };
        ChartModel {
            serial: SERIAL.fetch_add(1, Ordering::Relaxed),
            key: ModelKey { focus, ..key },
            layout,
            arcs,
            ring_start,
            center,
            extended,
            by_node,
        }
    }

    pub fn focus(&self) -> NodeId {
        self.layout.focus
    }

    pub fn depth(&self) -> u8 {
        self.layout.depth
    }

    /// Index of the arc showing `id`, if visible.
    pub fn index_of(&self, id: NodeId) -> Option<usize> {
        self.by_node.get(&id).map(|&i| i as usize)
    }

    /// Index of the arc at `angle` (radians, clockwise from 12 o'clock) on
    /// ring `depth`.
    pub fn hit(&self, angle: f32, depth: u8) -> Option<usize> {
        let d = usize::from(depth);
        if d == 0 || d >= self.ring_start.len() {
            return None;
        }
        let (s, e) = (self.ring_start[d - 1] as usize, self.ring_start[d] as usize);
        let tau = std::f32::consts::TAU;
        let a = angle.rem_euclid(tau);
        let ring = &self.arcs[s..e];
        let k = ring.partition_point(|x| x.a0 <= a).checked_sub(1)?;
        (a < ring[k].a1).then_some(s + k)
    }

    /// Index of the extended arc whose extension covers `angle` on ring
    /// `depth` (rings above the arc's own, up to [`ChartModel::depth`]).
    /// Linear in the number of extensions (a handful).
    pub fn hit_extension(&self, angle: f32, depth: u8) -> Option<usize> {
        if depth > self.depth() {
            return None;
        }
        let a = angle.rem_euclid(std::f32::consts::TAU);
        self.extended.iter().map(|&i| i as usize).find(|&i| {
            let arc = &self.arcs[i];
            depth > arc.depth && arc.a0 <= a && a < arc.a1
        })
    }

    /// [`ChartModel::hit`], else [`ChartModel::hit_extension`]: what the
    /// pointer at (`angle`, ring `depth`) is over.
    pub fn hit_any(&self, angle: f32, depth: u8) -> Option<usize> {
        self.hit(angle, depth)
            .or_else(|| self.hit_extension(angle, depth))
    }

    /// Ring-1 ancestor arc of arc `i`.
    pub fn top_of(&self, mut i: usize) -> usize {
        while self.arcs[i].parent != NO_PARENT {
            i = self.arcs[i].parent as usize;
        }
        i
    }
}

/// Fills `ArcInfo::color` for the color mode.
fn color_arcs(
    tree: &Tree,
    arcs: &mut [ArcInfo],
    mode: ColorMode,
    dark: bool,
    size_mode: SizeMode,
    now: i64,
) {
    let (rest_a, _) = colors::rest_colors(dark);
    // Special arcs first; the modes below skip them.
    let special = |a: &ArcInfo| -> Option<Color32> {
        if a.is_rest() {
            Some(rest_a)
        } else if a.flags & flags::FREE != 0 {
            Some(colors::free_colors(dark).0)
        } else if a.flags & flags::UNACCOUNTED != 0 {
            Some(colors::unaccounted_color(dark))
        } else {
            None
        }
    };
    match mode {
        ColorMode::Folders => {
            // Each arc's top-level (ring 1) ancestor; parents have lower
            // indices, so one forward pass suffices.
            let mut top = vec![0usize; arcs.len()];
            for i in 0..arcs.len() {
                let p = arcs[i].parent;
                top[i] = if p == NO_PARENT { i } else { top[p as usize] };
                let a = &arcs[i];
                let hue = colors::folder_hue_at(arcs[top[i]].mid(), a.mid());
                let is_file = a.flags & flags::DIR == 0;
                arcs[i].color =
                    special(a).unwrap_or_else(|| colors::folder_color(hue, a.depth, is_file, dark));
            }
        }
        ColorMode::Types => {
            // Bytes per group, aggregated bottom-up over visible arcs.
            let mut hist = vec![[0f32; 9]; arcs.len()];
            let mut budget = SUBTREE_BUDGET;
            for i in (0..arcs.len()).rev() {
                let a = &arcs[i];
                if let (Some(id), None) = (a.node(), special(a)) {
                    let node = tree.node(id);
                    if !node.is_dir() {
                        let g = Group::of_ext(tree.ext_name(node.ext()));
                        hist[i][g.index()] += a.size as f32;
                    } else if hist[i].iter().all(|&v| v == 0.0) && budget > 0 {
                        // Content not visible: walk the subtree (bounded).
                        let mut used = 0;
                        for d in tree.descendants(id).take(SUBTREE_BUDGET_EACH.min(budget)) {
                            used += 1;
                            let dn = tree.node(d);
                            if dn.is_file() {
                                let g = Group::of_ext(tree.ext_name(dn.ext()));
                                hist[i][g.index()] += dn.size(size_mode) as f32;
                            }
                        }
                        budget = budget.saturating_sub(used);
                    }
                }
                let p = arcs[i].parent;
                if p != NO_PARENT {
                    let h = hist[i];
                    for (dst, v) in hist[p as usize].iter_mut().zip(h) {
                        *dst += v;
                    }
                }
            }
            for (i, a) in arcs.iter_mut().enumerate() {
                a.color = special(a).unwrap_or_else(|| {
                    let h = &hist[i];
                    let total: f32 = h.iter().sum();
                    if total <= 0.0 {
                        return colors::neutral(dark, a.depth);
                    }
                    let (gi, &best) = h
                        .iter()
                        .enumerate()
                        .max_by(|x, y| x.1.total_cmp(y.1))
                        .unwrap_or((8, &0.0));
                    let g = Group::ALL[gi];
                    if a.flags & flags::DIR == 0 {
                        g.color(dark)
                    } else {
                        colors::types_folder_color(g, best / total, a.depth, dark)
                    }
                });
            }
        }
        ColorMode::Age => {
            // Size-weighted mean of the gradient position (log age).
            let mut acc = vec![(0f64, 0f64); arcs.len()];
            let mut budget = SUBTREE_BUDGET;
            let t_of = |mtime: i64| -> Option<f32> {
                (mtime > 0).then(|| colors::age_t((now - mtime) as f32 / 86_400.0))
            };
            for i in (0..arcs.len()).rev() {
                let a = &arcs[i];
                if let (Some(id), None) = (a.node(), special(a)) {
                    let node = tree.node(id);
                    if !node.is_dir() {
                        if let Some(t) = t_of(node.mtime()) {
                            let w = (a.size as f64).max(1.0);
                            acc[i].0 += f64::from(t) * w;
                            acc[i].1 += w;
                        }
                    } else if acc[i].1 == 0.0 && budget > 0 {
                        let mut used = 0;
                        for d in tree.descendants(id).take(SUBTREE_BUDGET_EACH.min(budget)) {
                            used += 1;
                            let dn = tree.node(d);
                            if dn.is_file()
                                && let Some(t) = t_of(dn.mtime())
                            {
                                let w = (dn.size(size_mode) as f64).max(1.0);
                                acc[i].0 += f64::from(t) * w;
                                acc[i].1 += w;
                            }
                        }
                        budget = budget.saturating_sub(used);
                        if acc[i].1 == 0.0
                            && let Some(t) = t_of(node.mtime())
                        {
                            acc[i] = (f64::from(t), 1.0);
                        }
                    }
                }
                let p = arcs[i].parent;
                if p != NO_PARENT {
                    let v = acc[i];
                    acc[p as usize].0 += v.0;
                    acc[p as usize].1 += v.1;
                }
            }
            for (i, a) in arcs.iter_mut().enumerate() {
                a.color = special(a).unwrap_or_else(|| {
                    let (s, w) = acc[i];
                    if w <= 0.0 {
                        colors::neutral(dark, a.depth)
                    } else {
                        colors::age_color((s / w) as f32, dark)
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringscan_core::synth;
    use std::f32::consts::TAU;

    const NAMES: VirtualNames = VirtualNames {
        this_pc: "This PC",
        free: "Free",
        unaccounted: "Unaccounted",
        reserved: "Reserved",
        orphaned: "Orphaned",
    };

    #[test]
    fn reserved_space_has_its_own_name() {
        let mut t = Tree::new("C:\\", 0);
        t.add_file(0, "a", 1, 4096, 0, 0);
        ringscan_engine::drive::apply_drive_extras(&mut t, 0, 100_000, 50_000, 8_192);
        let r = ringscan_engine::drive::reserved_of(&t, 0).expect("reserved node");
        let u = ringscan_engine::drive::extras_of(&t, 0)
            .1
            .expect("unaccounted node");
        assert_eq!(display_name(&t, r, &NAMES), "Reserved");
        assert_eq!(display_name(&t, u, &NAMES), "Unaccounted");
    }

    fn key(tree: &Tree, focus: NodeId, mode: ColorMode) -> ModelKey {
        ModelKey {
            scan: 1,
            generation: tree.generation(),
            focus,
            params: Params::default(),
            color_mode: mode,
            dark: false,
            lang: Lang::En,
            search: 0,
            now: 1_767_225_600,
        }
    }

    #[test]
    fn parents_rings_and_hits() {
        let tree = synth::this_pc(9, 20_000);
        let m = ChartModel::build(&tree, key(&tree, 0, ColorMode::Folders), None, &NAMES);
        assert_eq!(m.arcs.len(), m.layout.arcs.len());
        assert_eq!(m.center.name, "This PC");
        assert!(m.center.display > m.center.size, "free space is shown");
        assert_eq!(*m.ring_start.last().unwrap() as usize, m.arcs.len());
        for (i, a) in m.arcs.iter().enumerate() {
            if a.depth == 1 {
                assert_eq!(a.parent, NO_PARENT);
                continue;
            }
            let p = &m.arcs[a.parent as usize];
            assert_eq!(p.depth + 1, a.depth);
            assert!(p.a0 <= a.mid() && a.mid() < p.a1);
            if let (Some(pid), Some(id)) = (p.node(), a.node()) {
                assert_eq!(tree.node(id).parent(), pid, "arc {i}");
            }
            // hit() at the arc's middle finds the arc itself.
            assert_eq!(m.hit(a.mid(), a.depth), Some(i));
            assert_eq!(m.top_of(i), m.top_of(a.parent as usize));
        }
        for (i, a) in m.arcs.iter().enumerate() {
            if let Some(id) = a.node() {
                assert_eq!(m.index_of(id), Some(i));
            }
        }
    }

    #[test]
    fn colors_per_mode_and_special_arcs() {
        let tree = synth::this_pc(4, 20_000);
        for mode in [ColorMode::Folders, ColorMode::Types, ColorMode::Age] {
            let m = ChartModel::build(&tree, key(&tree, 0, mode), None, &NAMES);
            let distinct: HashSet<[u8; 4]> = m.arcs.iter().map(|a| a.color.to_array()).collect();
            assert!(distinct.len() > 5, "{mode:?}: {} colors", distinct.len());
            for a in &m.arcs {
                if a.flags & flags::FREE != 0 {
                    assert_eq!(a.color, colors::free_colors(false).0);
                } else if a.flags & flags::UNACCOUNTED != 0 {
                    assert_eq!(a.color, colors::unaccounted_color(false));
                } else if a.is_rest() {
                    assert_eq!(a.color, colors::rest_colors(false).0);
                }
            }
        }
    }

    #[test]
    fn search_marks_and_dead_focus() {
        let mut tree = synth::tree(5, 5_000);
        let target = tree
            .children(0)
            .filter(|&c| tree.node(c).is_dir() && tree.has_children(c))
            .max_by_key(|&c| tree.size(c, SizeMode::Allocated))
            .unwrap();
        let mut hl = Highlight::default();
        hl.matches.insert(target);
        let m = ChartModel::build(&tree, key(&tree, 0, ColorMode::Folders), Some(&hl), &NAMES);
        let i = m.index_of(target).expect("visible");
        assert_eq!(m.arcs[i].mark, Mark::Match);
        assert!(m.arcs.iter().any(|a| a.mark == Mark::Dim));
        // A removed focus falls back to its live ancestor.
        let child = tree.children(target).next();
        tree.remove(target);
        let m = ChartModel::build(
            &tree,
            key(&tree, child.unwrap_or(target), ColorMode::Folders),
            None,
            &NAMES,
        );
        assert_eq!(m.focus(), 0);
    }

    #[test]
    fn center_free_space_with_free_hidden_or_shown() {
        let tree = synth::this_pc(9, 20_000);
        let mode = SizeMode::Allocated;
        let total_free: u64 = tree.free_nodes().map(|f| tree.node(f).size(mode)).sum();
        assert!(total_free > 0);
        for show_free in [false, true] {
            let k = |focus| ModelKey {
                params: Params {
                    show_free,
                    ..Params::default()
                },
                ..key(&tree, focus, ColorMode::Folders)
            };
            // "This PC": the sum over the drives.
            let m = ChartModel::build(&tree, k(0), None, &NAMES);
            assert_eq!(m.center.free, total_free);
            let shown = if show_free { total_free } else { 0 };
            assert_eq!(m.center.display, m.center.size + shown);
            // A drive: its own free space; a folder on it: none.
            let drive = tree
                .children(0)
                .find(|&d| tree.children(d).any(|c| tree.node(c).has(flags::FREE)))
                .expect("a drive with free space");
            let free = tree
                .children(drive)
                .find(|&c| tree.node(c).has(flags::FREE))
                .map(|f| tree.node(f).size(mode))
                .unwrap();
            let m = ChartModel::build(&tree, k(drive), None, &NAMES);
            assert_eq!(m.center.free, free);
            assert_eq!(free_below(&tree, drive, mode), free);
            let folder = tree
                .children(drive)
                .find(|&c| tree.node(c).is_dir() && !tree.node(c).is_virtual())
                .unwrap();
            let m = ChartModel::build(&tree, k(folder), None, &NAMES);
            assert_eq!(m.center.free, 0);
            assert_eq!(m.center.display, m.center.size);
        }
    }

    #[test]
    fn extensions_of_virtual_leaves() {
        let tree = synth::this_pc(9, 20_000);
        let m = ChartModel::build(&tree, key(&tree, 0, ColorMode::Folders), None, &NAMES);
        // Unaccounted and free space of both drives and C:'s reserved space
        // (named like the engine's node), on ring 2.
        assert_eq!(m.extended.len(), 5);
        let names: Vec<String> = m
            .extended
            .iter()
            .map(|&i| display_name(&tree, m.arcs[i as usize].node().unwrap(), &NAMES))
            .collect();
        assert_eq!(names.iter().filter(|n| *n == NAMES.reserved).count(), 1);
        assert_eq!(names.iter().filter(|n| *n == NAMES.unaccounted).count(), 2);
        assert!(m.depth() > 2);
        for &i in &m.extended {
            let a = &m.arcs[i as usize];
            assert_eq!(a.depth, 2);
            assert!(is_extended(a.flags));
            assert!(a.flags & (flags::FREE | flags::UNACCOUNTED) != 0);
            // The own ring is a plain hit; every ring above, up to the
            // deepest one, maps to the arc through its extension.
            assert_eq!(m.hit(a.mid(), a.depth), Some(i as usize));
            assert_eq!(m.hit_extension(a.mid(), a.depth), None);
            for d in a.depth + 1..=m.depth() {
                assert_eq!(m.hit(a.mid(), d), None, "ring {d} above a leaf is empty");
                assert_eq!(m.hit_extension(a.mid(), d), Some(i as usize));
                assert_eq!(m.hit_any(a.mid(), d), Some(i as usize));
                // Angles are wrapped like `hit` does.
                assert_eq!(m.hit_extension(a.mid() + TAU, d), Some(i as usize));
            }
            assert_eq!(m.hit_extension(a.mid(), m.depth() + 1), None);
            // Just outside the arc's span: not this extension.
            assert_ne!(m.hit_extension(a.a1 + 1e-4, a.depth + 1), Some(i as usize));
            assert_ne!(m.hit_extension(a.a0 - 1e-4, a.depth + 1), Some(i as usize));
        }
        // Regular arcs keep winning where they are.
        for (i, a) in m.arcs.iter().enumerate() {
            assert_eq!(m.hit_any(a.mid(), a.depth), Some(i));
        }
        // Folders and files are never extended.
        for (i, a) in m.arcs.iter().enumerate() {
            if !m.extended.contains(&(i as u32)) && a.depth < m.depth() {
                assert!(a.is_rest() || !is_extended(a.flags));
            }
        }
    }

    #[test]
    fn extensions_follow_depth_free_and_focus() {
        let tree = synth::this_pc(9, 20_000);
        let with = |focus, max_depth, show_free| {
            let k = ModelKey {
                params: Params {
                    max_depth,
                    show_free,
                    ..Params::default()
                },
                ..key(&tree, focus, ColorMode::Folders)
            };
            ChartModel::build(&tree, k, None, &NAMES)
        };
        // Virtual leaves on the deepest ring have nothing above them.
        let m = with(0, 2, true);
        assert_eq!(m.depth(), 2);
        assert!(m.extended.is_empty());
        assert_eq!(m.hit_extension(1.0, 3), None);
        let m = with(0, 3, true);
        assert_eq!(m.extended.len(), 5);
        // Free space hidden: only the unaccounted and reserved arcs.
        let m = with(0, 12, false);
        assert_eq!(m.extended.len(), 3);
        assert!(
            m.extended
                .iter()
                .all(|&i| m.arcs[i as usize].flags & flags::UNACCOUNTED != 0)
        );
        // A drive: its own unaccounted and reserved space, on ring 1.
        let c = tree.children(0).next().unwrap();
        let m = with(c, 12, false);
        assert_eq!(m.extended.len(), 2);
        let a = &m.arcs[m.extended[0] as usize];
        assert_eq!(a.depth, 1);
        assert_eq!(m.hit_any(a.mid(), m.depth()), Some(m.extended[0] as usize));
        // A folder on it: no virtual leaves, no extensions, no hits there.
        let folder = tree
            .children(c)
            .filter(|&f| tree.node(f).is_dir() && tree.has_children(f))
            .max_by_key(|&f| tree.size(f, SizeMode::Allocated))
            .unwrap();
        let m = with(folder, 12, true);
        assert!(m.extended.is_empty());
        for a in &m.arcs {
            assert_eq!(m.hit_extension(a.mid(), a.depth + 1), None);
        }
    }

    #[test]
    fn reserved_space_is_extended() {
        let mut t = Tree::new("C:\\", 0);
        let dir = t.add_dir(0, "Windows", 0, 0);
        t.add_file(dir, "a", 1, 40_960, 0, 0);
        ringscan_engine::drive::apply_drive_extras(&mut t, 0, 1_000_000, 500_000, 81_920);
        let r = ringscan_engine::drive::reserved_of(&t, 0).expect("reserved node");
        assert!(is_extended(t.node(r).flags()));
        assert!(!is_extended(t.node(dir).flags()));
        let k = ModelKey {
            params: Params {
                min_angle: 0.0,
                ..Params::default()
            },
            ..key(&t, 0, ColorMode::Folders)
        };
        let m = ChartModel::build(&t, k, None, &NAMES);
        let ri = m.index_of(r).expect("reserved arc");
        assert!(m.extended.contains(&(ri as u32)));
        assert_eq!(m.hit_any(m.arcs[ri].mid(), 2), Some(ri));
    }

    #[test]
    fn short_names() {
        let t = Tree::new("C:\\Program Files", 0);
        assert_eq!(short_name(&t, 0, &NAMES), "Program Files");
        let t = Tree::new("C:\\", 0);
        assert_eq!(short_name(&t, 0, &NAMES), "C:\\");
    }
}
