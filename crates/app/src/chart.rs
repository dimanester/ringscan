//! Sunburst rendering: geometry, tessellation, labels, hit testing and the
//! animated zoom transition.
//!
//! # Geometry
//!
//! Angles run clockwise from 12 o'clock (the core layout's `0..TAU`). The
//! focus is a disc of radius `r0`; ring `d` spans `bounds[d-1]..bounds[d]`.
//! Ring widths shrink geometrically (×0.93 per level) so ten or more levels
//! fit while the inner, most important rings stay wide. The rings fill the
//! available radius for the depth actually present (at least
//! [`MIN_GEOMETRY_RINGS`], or the model's ring count if that is smaller:
//! [`Geometry::of`]), so a shallow folder is not drawn as a thin band
//! around a huge empty disc.
//!
//! # Ring count
//!
//! How many levels the layout computes is a setting: a fixed number, or
//! (the default) [`auto_rings`]: as many as the chart's radius allows while
//! the outermost ring stays [`AUTO_RING_MIN_WIDTH`] wide, enough for one
//! line of label text ([`AUTO_RING_MIN_WIDTH_COLORS`] when arcs are
//! colored by file type or age, where the colors matter more than names).
//! With a fixed 12 rings a 1280×640 window had ~18-pt rings and room for 3
//! names; the automatic count draws 8 there, and 3 at the window's minimum
//! size, where 5 or more rings had no room for any name. The count changes
//! only when the chart crosses a size threshold (with
//! [`auto_rings_sticky`]'s hysteresis), and the model is cached by it, so
//! resizing does not re-layout per pixel. A change animates
//! ([`Transition::ring_count`]).
//!
//! # Labels
//!
//! A name is drawn only where it fits whole (or shortened with "…") inside
//! its arc with [`LABEL_MARGIN`] to spare, so labels never overflow their
//! arc or overlap each other. The binding constraints in thin rings are the
//! line height (15 pt at 11.5 pt, 17 pt at 12.5 pt on ring 1) plus margins,
//! and the ring's curvature: a straight line of text across a curved ring
//! touches the outer edge with its corners, so it is moved inward as far as
//! the margin allows before giving up.
//!
//! # Tessellation
//!
//! All arcs go into one [`Mesh`] per frame: each arc is a triangle strip
//! whose segment count is proportional to its outer arc length (≈4 px per
//! segment), so a 1-px sliver costs two triangles and a full ring a few
//! hundred. Arcs are inset by half a pixel on every side, which leaves a
//! hairline of background between neighbours; for slivers the inset shrinks
//! proportionally so they never vanish. Edges are smoothed by the window's
//! 4× MSAA instead of egui's feathering (which only applies to paths). The
//! mesh is cached and rebuilt only when the model, the geometry or the
//! highlight state changes; during a zoom animation it is rebuilt every
//! frame from two cached models.
//!
//! # Zoom transition
//!
//! Zooming into arc `A` (span `A0..A1` on ring `dA` of the outer model)
//! interpolates a piecewise-affine map: at progress `p` an outer-model angle
//! `a` is drawn at `lerp(a, Z(a), p)` with `Z(a) = clamp((a-A0)/(A1-A0)·τ)`,
//! and ring boundary `k` at `lerp(R_outer(k), R_inner(k-dA), p)`. The arcs
//! of the inner model (A's subtree, laid out for the new focus) are mapped
//! back into outer coordinates and drawn with the same map, so at `p = 0`
//! they sit exactly where A's subtree was and at `p = 1` they are the new
//! chart; outer arcs outside `A0..A1` shrink to nothing at the edges and the
//! rings inside `dA` slide under the center disc. Zooming out plays the same
//! animation backwards. Labels are hidden while the animation runs. A
//! change of the ring count alone is the same animation with the whole
//! circle at the focus as the anchor (`dA = 0`): only the ring boundaries
//! move.
//!
//! # Extensions
//!
//! The arcs in [`ChartModel::extended`] (virtual leaves: unaccounted and
//! reserved space, free space when shown) also fill the wedge above them,
//! from their ring's outer edge to the outer edge of the deepest ring, so a
//! few percent of a drive that no file explains does not leave a hole in
//! the chart. Unaccounted and reserved space get a faint tint of their
//! color with diagonal hatching ("cannot be attributed"), free space the
//! same near-transparent fill as its own arc. The extensions are ordinary
//! [`DrawArc`]s in the same mesh (built once per model and geometry, a
//! handful per chart); during a zoom their outer radius is interpolated
//! between the two charts' outer edges like every ring boundary. Hatch
//! lines are clipped analytically to the sector (inset like the fill), so
//! the hairlines between neighbours stay clean.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::f32::consts::TAU;
use std::sync::Arc;

use eframe::egui::{
    self, Align2, Color32, FontId, Mesh, Pos2, Rect, Shape, Stroke, epaint::TextShape,
};
use ringscan_core::flags;

use crate::colors;
use crate::model::{ArcInfo, ChartModel, Mark};
use crate::settings::ColorMode;
use crate::theme::Pal;

/// Rings the geometry always reserves room for.
pub const MIN_GEOMETRY_RINGS: u8 = 5;
/// Width ratio of consecutive rings.
const RING_SHRINK: f32 = 0.93;
/// Target length of one tessellation segment, in points.
const SEG_LEN: f32 = 4.0;
/// Duration of the zoom animation, seconds.
pub const ZOOM_SECS: f32 = 0.22;

/// Radii of the disc and rings for one chart rectangle.
#[derive(Clone, Debug, PartialEq)]
pub struct Geometry {
    pub center: Pos2,
    /// `bounds[0]` = disc radius, `bounds[d]` = outer radius of ring `d`.
    pub bounds: Vec<f32>,
}

impl Geometry {
    /// Room for `rings` rings, at least [`MIN_GEOMETRY_RINGS`] (what
    /// [`Geometry::of`] gives a model laid out for that many or more).
    #[cfg(test)]
    pub fn new(rect: Rect, rings: u8) -> Geometry {
        Self::exact(rect, rings.max(MIN_GEOMETRY_RINGS))
    }

    /// The geometry `model` is drawn with: room for its depth, and at
    /// least [`MIN_GEOMETRY_RINGS`] unless it was laid out for fewer rings
    /// (a small chart in Auto, or a fixed count below that), which then
    /// get the whole radius.
    pub fn of(rect: Rect, model: &ChartModel) -> Geometry {
        let reserve = MIN_GEOMETRY_RINGS.min(model.key.params.max_depth);
        Self::exact(rect, model.depth().max(reserve))
    }

    /// Room for exactly `rings` rings (at least one).
    pub fn exact(rect: Rect, rings: u8) -> Geometry {
        let rings = usize::from(rings.max(1));
        let avail = (rect.width().min(rect.height()) * 0.5 - 14.0).max(60.0);
        let r0 = (avail * 0.2).clamp(48.0, 112.0).min(avail * 0.45);
        let sum: f32 = (0..rings).map(|i| RING_SHRINK.powi(i as i32)).sum();
        let w1 = (avail - r0) / sum;
        let mut bounds = Vec::with_capacity(rings + 1);
        bounds.push(r0);
        let mut r = r0;
        for i in 0..rings {
            r += w1 * RING_SHRINK.powi(i as i32);
            bounds.push(r);
        }
        Geometry {
            center: rect.center(),
            bounds,
        }
    }

    pub fn r0(&self) -> f32 {
        self.bounds[0]
    }

    pub fn outer(&self) -> f32 {
        *self.bounds.last().unwrap_or(&0.0)
    }

    /// Radius of ring boundary `k` (0 = disc edge), extrapolating outward
    /// with zero width past the last ring and inward into the disc.
    pub fn bound(&self, k: i32) -> f32 {
        if k < 0 {
            // Inside the disc: compressed toward its edge, hidden by it.
            (self.r0() * (1.0 + 0.12 * k as f32)).max(0.0)
        } else {
            *self.bounds.get(k as usize).unwrap_or(&self.outer())
        }
    }

    /// Ring under radius `r` (1-based), 0 for the disc, None outside.
    pub fn ring_at(&self, r: f32) -> Option<u8> {
        if r < self.r0() {
            return Some(0);
        }
        self.bounds
            .windows(2)
            .position(|w| r >= w[0] && r < w[1])
            .map(|i| (i + 1) as u8)
    }

    /// Polar coordinates (radius, clockwise angle from 12 o'clock) of `pos`.
    pub fn polar(&self, pos: Pos2) -> (f32, f32) {
        let d = pos - self.center;
        (d.length(), d.x.atan2(-d.y).rem_euclid(TAU))
    }

    pub fn point(&self, r: f32, angle: f32) -> Pos2 {
        let (s, c) = angle.sin_cos();
        Pos2::new(self.center.x + r * s, self.center.y - r * c)
    }
}

/// Range of the automatic ring count ([`auto_rings`]): from the fewest
/// rings a fixed count may have (so a chart at the window's minimum size
/// still has room for names) to what a 4K screen fills.
pub const AUTO_RINGS: std::ops::RangeInclusive<u8> = 3..=16;

/// Thinnest ring the automatic ring count allows in Folders mode, points:
/// one line of label text in the UI font (11.5-pt Segoe UI: a 15-pt line)
/// with [`LABEL_MARGIN`] on both sides, plus 1 pt for the curvature of a
/// short name. Measured on the demo data, 20 pt gives the most labels at
/// 1280×640, 1280×800 and 1920×1080 (thinner rings show more levels but
/// fewer names; thicker ones fewer of both).
pub const AUTO_RING_MIN_WIDTH: f32 = 20.0;

/// Thinnest ring the automatic ring count allows when arcs are colored by
/// file type or age, points. There the color carries the information,
/// and files sit deep in the tree, so a few more, thinner rings show more
/// of it than names would (labels still go wherever they fit): about as
/// many rings as the old fixed 12 at 1280×800, more on bigger screens.
pub const AUTO_RING_MIN_WIDTH_COLORS: f32 = 13.0;

/// The thinnest ring [`auto_rings`] allows in color mode `mode`.
pub fn auto_ring_min_width(mode: ColorMode) -> f32 {
    match mode {
        ColorMode::Folders => AUTO_RING_MIN_WIDTH,
        ColorMode::Types | ColorMode::Age => AUTO_RING_MIN_WIDTH_COLORS,
    }
}

/// How far (points of radius) the chart may grow or shrink before the
/// automatic ring count leaves its current value ([`auto_rings_sticky`]),
/// so a window resized across a threshold does not re-layout back and
/// forth.
const AUTO_RINGS_SLACK: f32 = 6.0;

/// Width of the outermost ring of `rings` rings in `rect`.
fn outermost_ring(rect: Rect, rings: u8) -> f32 {
    let g = Geometry::exact(rect, rings);
    let k = g.bounds.len() - 1;
    g.bounds[k] - g.bounds[k - 1]
}

/// The automatic ring count for chart rectangle `rect`: the most rings
/// (within [`AUTO_RINGS`]) whose thinnest, outermost ring is still
/// `min_width` wide ([`auto_ring_min_width`]). Grows with the rectangle;
/// a folder shallower than that is drawn with its own depth (see
/// [`Geometry::of`]).
pub fn auto_rings(rect: Rect, min_width: f32) -> u8 {
    // The outermost ring narrows as rings are added: the last that fits.
    AUTO_RINGS
        .clone()
        .take_while(|&n| n == *AUTO_RINGS.start() || outermost_ring(rect, n) >= min_width)
        .last()
        .unwrap_or(*AUTO_RINGS.start())
}

/// [`auto_rings`] with hysteresis: keeps `current` while the chart is
/// within [`AUTO_RINGS_SLACK`] of a size that gives it.
pub fn auto_rings_sticky(rect: Rect, current: u8, min_width: f32) -> u8 {
    let smaller = auto_rings(rect.shrink(AUTO_RINGS_SLACK), min_width);
    let bigger = auto_rings(rect.expand(AUTO_RINGS_SLACK), min_width);
    if (smaller..=bigger).contains(&current) {
        current
    } else {
        auto_rings(rect, min_width)
    }
}

/// How an arc is filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    Solid,
    /// Two-tone stripes along the arc ("N smaller items").
    Stripes(Color32),
    /// Nearly transparent fill plus an outline (free space).
    Outlined(Color32),
    /// Diagonal hatch lines of this color over the fill (the extension of
    /// unaccounted space).
    Hatched(Color32),
}

/// Dark theme: share of white mixed into the background for the extension
/// of free space (~12 levels over the default background).
const FREE_EXTENSION_LIFT: f32 = 0.056;

/// Distance between hatch lines, points.
const HATCH_SPACING: f32 = 6.0;
/// Width of a hatch line, points.
const HATCH_WIDTH: f32 = 1.0;
/// Hatch line direction ("/" on screen) and its normal.
const HATCH_DIR: egui::Vec2 = egui::vec2(
    std::f32::consts::FRAC_1_SQRT_2,
    -std::f32::consts::FRAC_1_SQRT_2,
);
const HATCH_NORMAL: egui::Vec2 = egui::vec2(
    std::f32::consts::FRAC_1_SQRT_2,
    std::f32::consts::FRAC_1_SQRT_2,
);

/// One arc ready to tessellate (screen radii).
#[derive(Clone, Copy, Debug)]
pub struct DrawArc {
    pub a0: f32,
    pub a1: f32,
    pub r0: f32,
    pub r1: f32,
    pub color: Color32,
    pub fill: Fill,
}

/// Appends an annular sector to `mesh`. `gap` is the hairline left between
/// neighbours (points).
pub fn tessellate(mesh: &mut Mesh, center: Pos2, a: &DrawArc, gap: f32) {
    let span = a.a1 - a.a0;
    let thick = a.r1 - a.r0;
    if span <= 0.0 || thick <= 0.05 || a.color.a() == 0 {
        return;
    }
    // Radial inset, at most a quarter of the thickness on each side.
    let rg = (gap * 0.5).min(thick * 0.25);
    let (ri, ro) = (a.r0 + rg, a.r1 - rg);
    // Angular inset as a constant pixel width: different angles at the
    // inner and outer edge, at most a quarter of the span.
    let gi = (gap * 0.5 / ri.max(1.0)).min(span * 0.25);
    let go = (gap * 0.5 / ro.max(1.0)).min(span * 0.25);
    let (i0, i1) = (a.a0 + gi, a.a1 - gi);
    let (o0, o1) = (a.a0 + go, a.a1 - go);
    let n = ((span * ro) / SEG_LEN).ceil().clamp(1.0, 720.0) as usize;
    match a.fill {
        Fill::Stripes(other) => {
            // Stripes about 6 px long at the mid radius, each its own quad.
            let rm = (ri + ro) * 0.5;
            let stripes = ((span * rm) / 6.0).round().clamp(1.0, 400.0) as usize;
            for s in 0..stripes {
                let t0 = s as f32 / stripes as f32;
                let t1 = (s + 1) as f32 / stripes as f32;
                let color = if s.is_multiple_of(2) { a.color } else { other };
                let sub = n.div_ceil(stripes).max(1);
                strip(
                    mesh,
                    center,
                    (ri, i0 + (i1 - i0) * t0, i0 + (i1 - i0) * t1),
                    (ro, o0 + (o1 - o0) * t0, o0 + (o1 - o0) * t1),
                    sub,
                    color,
                );
            }
        }
        Fill::Solid | Fill::Outlined(_) => {
            strip(mesh, center, (ri, i0, i1), (ro, o0, o1), n, a.color);
        }
        Fill::Hatched(lines) => {
            strip(mesh, center, (ri, i0, i1), (ro, o0, o1), n, a.color);
            if lines.a() > 0 {
                hatch(mesh, center, (a.a0, a.a1), (ri, ro), gap * 0.5, lines);
            }
        }
    }
}

/// Parameter intervals `t` of the hatch line `HATCH_NORMAL·o + HATCH_DIR·t`
/// (relative to the center) inside the ring `r0..r1` and the wedge
/// `c0..c1` (span at most π), keeping `h0` / `h1` points inside the
/// wedge's start / end edge. The inner circle can cut the line in two.
fn clip_line(
    o: f32,
    (r0, r1): (f32, f32),
    (c0, c1): (f32, f32),
    (h0, h1): (f32, f32),
) -> [Option<(f32, f32)>; 2] {
    let mut out = [None, None];
    if o.abs() >= r1 {
        return out;
    }
    let t_out = (r1 * r1 - o * o).sqrt();
    let (mut lo, mut hi) = (-t_out, t_out);
    // A point q is clockwise of the edge at angle c (by less than π) iff
    // q·v(c) > 0 with v(c) = (cos c, sin c), the derivative of the edge's
    // unit vector (sin c, -cos c). Start edge: q·v(c0) >= h0; end edge:
    // q·(-v(c1)) >= h1.
    for (ang, h, sign) in [(c0, h0, 1.0f32), (c1, h1, -1.0)] {
        let v = egui::vec2(ang.cos(), ang.sin()) * sign;
        let along = HATCH_DIR.dot(v);
        let across = HATCH_NORMAL.dot(v) * o;
        if along.abs() < 1e-6 {
            if across < h {
                return out;
            }
            continue;
        }
        let t = (h - across) / along;
        if along > 0.0 {
            lo = lo.max(t);
        } else {
            hi = hi.min(t);
        }
    }
    if lo >= hi {
        return out;
    }
    if o.abs() < r0 {
        let t_in = (r0 * r0 - o * o).sqrt();
        if lo < -t_in {
            out[0] = Some((lo, hi.min(-t_in)));
        }
        if hi > t_in {
            out[1] = Some((lo.max(t_in), hi));
        }
    } else {
        out[0] = Some((lo, hi));
    }
    out
}

/// Diagonal hatch lines over the annular sector `a0..a1`, `r0..r1`, kept
/// `inset` points inside its straight edges. Lines are anchored to the
/// center, so they do not crawl while a zoom animation moves the sector.
/// Each line is a quad whose long edges are clipped separately, so its ends
/// lie on the sector's boundary.
fn hatch(
    mesh: &mut Mesh,
    center: Pos2,
    (a0, a1): (f32, f32),
    (r0, r1): (f32, f32),
    inset: f32,
    color: Color32,
) {
    let span = a1 - a0;
    if span <= 0.0 || r1 - r0 < HATCH_WIDTH * 2.0 {
        return;
    }
    // Wedges wider than a quarter turn are hatched in pieces (the clip
    // needs convex wedges); the pieces' lines meet exactly.
    let pieces = (span / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let hw = HATCH_WIDTH * 0.5;
    let k0 = (-r1 / HATCH_SPACING).ceil() as i32;
    let k1 = (r1 / HATCH_SPACING).floor() as i32;
    let at = |o: f32, t: f32| center + HATCH_NORMAL * o + HATCH_DIR * t;
    for piece in 0..pieces {
        let c0 = a0 + span * piece as f32 / pieces as f32;
        let c1 = a0 + span * (piece + 1) as f32 / pieces as f32;
        let h0 = if piece == 0 { inset } else { 0.0 };
        let h1 = if piece + 1 == pieces { inset } else { 0.0 };
        for k in k0..=k1 {
            let o = k as f32 * HATCH_SPACING;
            // A line grazing the inner circle would pair unlike pieces of
            // its two edges: skip it (a sub-pixel stub).
            if (o.abs() - hw < r0) != (o.abs() + hw < r0) {
                continue;
            }
            let lo = clip_line(o - hw, (r0, r1), (c0, c1), (h0, h1));
            let hi = clip_line(o + hw, (r0, r1), (c0, c1), (h0, h1));
            for (x, y) in lo.into_iter().zip(hi) {
                let (Some(x), Some(y)) = (x, y) else { continue };
                let base = mesh.vertices.len() as u32;
                mesh.colored_vertex(at(o - hw, x.0), color);
                mesh.colored_vertex(at(o - hw, x.1), color);
                mesh.colored_vertex(at(o + hw, y.1), color);
                mesh.colored_vertex(at(o + hw, y.0), color);
                mesh.add_triangle(base, base + 1, base + 2);
                mesh.add_triangle(base, base + 2, base + 3);
            }
        }
    }
}

/// Triangle strip between an inner and an outer arc.
fn strip(
    mesh: &mut Mesh,
    center: Pos2,
    inner: (f32, f32, f32),
    outer: (f32, f32, f32),
    n: usize,
    color: Color32,
) {
    let base = mesh.vertices.len() as u32;
    mesh.reserve_vertices(2 * (n + 1));
    mesh.reserve_triangles(2 * n);
    // Walk both edges by rotating a unit vector instead of calling
    // sin_cos per vertex (the mesh of a 40k-arc chart has ~170k vertices
    // and is rebuilt every frame of a zoom animation). The accumulated
    // rounding error over 720 steps stays far below a pixel.
    let (mut si, mut ci) = inner.1.sin_cos();
    let (sdi, cdi) = ((inner.2 - inner.1) / n as f32).sin_cos();
    let (mut so, mut co) = outer.1.sin_cos();
    let (sdo, cdo) = ((outer.2 - outer.1) / n as f32).sin_cos();
    for _ in 0..=n {
        mesh.colored_vertex(
            Pos2::new(center.x + inner.0 * si, center.y - inner.0 * ci),
            color,
        );
        mesh.colored_vertex(
            Pos2::new(center.x + outer.0 * so, center.y - outer.0 * co),
            color,
        );
        (si, ci) = (si * cdi + ci * sdi, ci * cdi - si * sdi);
        (so, co) = (so * cdo + co * sdo, co * cdo - so * sdo);
    }
    for k in 0..n as u32 {
        let i = base + 2 * k;
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i + 1, i + 3, i + 2);
    }
}

/// Outline of an annular sector as a closed polyline.
pub fn outline_points(center: Pos2, a0: f32, a1: f32, r0: f32, r1: f32) -> Vec<Pos2> {
    let span = (a1 - a0).max(0.0);
    let n = ((span * r1) / SEG_LEN).ceil().clamp(1.0, 720.0) as usize;
    let pt = |r: f32, ang: f32| {
        let (s, c) = ang.sin_cos();
        Pos2::new(center.x + r * s, center.y - r * c)
    };
    let mut pts = Vec::with_capacity(2 * n + 2);
    for k in 0..=n {
        pts.push(pt(r1, a0 + span * k as f32 / n as f32));
    }
    for k in (0..=n).rev() {
        pts.push(pt(r0, a0 + span * k as f32 / n as f32));
    }
    pts
}

/// Final fill color of an arc, with search dimming applied.
pub fn arc_color(a: &ArcInfo, bg: Color32) -> Color32 {
    marked(a.color, a.mark, bg)
}

/// `color` faded toward `bg` by a search mark.
fn marked(color: Color32, mark: Mark, bg: Color32) -> Color32 {
    match mark {
        Mark::None | Mark::Match => color,
        Mark::Contains => colors::mix(color, bg, 0.45),
        Mark::Dim => colors::mix(color, bg, 0.8),
    }
}

/// Fill style of an arc.
pub fn arc_fill(a: &ArcInfo, dark: bool) -> Fill {
    if a.is_rest() {
        Fill::Stripes(colors::rest_colors(dark).1)
    } else if a.flags & flags::FREE != 0 {
        Fill::Outlined(colors::free_colors(dark).1)
    } else {
        Fill::Solid
    }
}

/// Fill of the extension of arc `a` (an arc in [`ChartModel::extended`]):
/// for unaccounted and reserved space a faint tint of the arc's color with
/// hatch lines; for free space the free arc's own near-transparent fill on
/// the light theme and, on the dark one (where that is within a few levels
/// of the background and a large free wedge read as a void), an opaque
/// fill ~12 levels over the background.
pub fn extension_style(a: &ArcInfo, bg: Color32, dark: bool) -> (Color32, Fill) {
    let base = arc_color(a, bg);
    if a.flags & flags::FREE != 0 {
        if dark {
            let lifted = colors::mix(bg, Color32::WHITE, FREE_EXTENSION_LIFT);
            return (marked(lifted, a.mark, bg), Fill::Solid);
        }
        return (base, Fill::Solid);
    }
    let (fill, lines) = hatch_colors(base, bg, dark);
    (fill, Fill::Hatched(lines))
}

/// Fill and hatch line colors of an unaccounted / reserved extension over
/// an arc of color `base`.
fn hatch_colors(base: Color32, bg: Color32, dark: bool) -> (Color32, Color32) {
    // Share of the background in the fill and in the hatch lines.
    let (fill, lines) = if dark { (0.72, 0.08) } else { (0.88, 0.6) };
    (colors::mix(base, bg, fill), colors::mix(base, bg, lines))
}

/// Outer radius of the extension of arc `i`, if it has one: the outer edge
/// of the model's deepest ring.
pub fn extension_outer(model: &ChartModel, geo: &Geometry, i: usize) -> Option<f32> {
    model
        .extended
        .contains(&(i as u32))
        .then(|| geo.bound(i32::from(model.depth())))
}

/// Screen extensions of a model at rest (see [Extensions](self#extensions)).
pub fn static_extensions(model: &ChartModel, geo: &Geometry, pal: &Pal) -> Vec<DrawArc> {
    let outer = geo.bound(i32::from(model.depth()));
    model
        .extended
        .iter()
        .map(|&i| {
            let a = &model.arcs[i as usize];
            let (color, fill) = extension_style(a, pal.bg, pal.dark);
            DrawArc {
                a0: a.a0,
                a1: a.a1,
                r0: geo.bound(i32::from(a.depth)),
                r1: outer,
                color,
                fill,
            }
        })
        .collect()
}

/// Screen arcs of a model at rest (with room for its extensions).
pub fn static_arcs(model: &ChartModel, geo: &Geometry, pal: &Pal) -> Vec<DrawArc> {
    let mut out = Vec::with_capacity(model.arcs.len() + model.extended.len());
    out.extend(model.arcs.iter().map(|a| {
        let d = i32::from(a.depth);
        DrawArc {
            a0: a.a0,
            a1: a.a1,
            r0: geo.bound(d - 1),
            r1: geo.bound(d),
            color: arc_color(a, pal.bg),
            fill: arc_fill(a, pal.dark),
        }
    }));
    out
}

/// The mesh of a model at rest: its arcs plus their extensions.
pub fn static_mesh(model: &ChartModel, geo: &Geometry, pal: &Pal) -> (Mesh, Vec<Shape>) {
    let mut arcs = static_arcs(model, geo, pal);
    arcs.extend(static_extensions(model, geo, pal));
    build_mesh(&arcs, geo.center, 1.0)
}

/// A running zoom animation between an outer model (ancestor focus) and an
/// inner one (descendant focus).
pub struct Transition {
    pub outer: Arc<ChartModel>,
    pub inner: Arc<ChartModel>,
    /// Span and ring of the anchor arc in the outer model.
    pub anchor: (f32, f32, u8),
    /// Middle angle of the anchor's top-level arc in the outer model, for
    /// color continuity in Folders mode (None in other modes).
    pub anchor_top_mid: Option<f32>,
    /// true = zooming in (outer → inner).
    pub inward: bool,
    pub start: f64,
    /// The colors the inner arcs had in the outer chart (Folders mode,
    /// `None` for arcs that keep their own), for the theme in the tuple.
    /// They do not change during the animation, and computing them (an
    /// OKLCH conversion per arc) every frame dominated the frame time on
    /// big charts, so the first frame computes them once.
    outer_colors: OnceCell<(bool, Vec<Option<Color32>>)>,
}

/// easeInOutCubic
pub fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// The zoom map of a [`Transition`] at one moment (see the module docs).
struct ZoomMap<'a> {
    p: f32,
    a0: f32,
    a1: f32,
    w: f32,
    da: i32,
    go: &'a Geometry,
    gi: &'a Geometry,
}

impl ZoomMap<'_> {
    /// Screen angle of outer-model angle `a`.
    fn ang(&self, a: f32) -> f32 {
        let z = ((a - self.a0) / self.w * TAU).clamp(0.0, TAU);
        a + (z - a) * self.p
    }

    /// Screen radius of outer-model ring boundary `k`.
    fn rad(&self, k: i32) -> f32 {
        let ro = self.go.bound(k);
        let ri = self.gi.bound(k - self.da);
        ro + (ri - ro) * self.p
    }

    /// Outer-model angle of inner-model angle `a`.
    fn inner_to_outer(&self, a: f32) -> f32 {
        self.a0 + a / TAU * self.w
    }

    /// Is outer arc `a` in the anchor's subtree (drawn from the inner
    /// model instead)?
    fn in_anchor_subtree(&self, a: &ArcInfo) -> bool {
        let eps = 1e-4;
        i32::from(a.depth) > self.da && a.a0 >= self.a0 - eps && a.a1 <= self.a1 + eps
    }
}

impl Transition {
    pub fn new(
        outer: Arc<ChartModel>,
        inner: Arc<ChartModel>,
        anchor: (f32, f32, u8),
        anchor_top_mid: Option<f32>,
        inward: bool,
        start: f64,
    ) -> Self {
        Self {
            outer,
            inner,
            anchor,
            anchor_top_mid,
            inward,
            start,
            outer_colors: OnceCell::new(),
        }
    }

    /// The animation from `old` to `new`, the same chart laid out for a
    /// different number of rings (the automatic count followed a resize,
    /// or the setting changed): the rings of the model with more of them
    /// slide between the two geometries, the extra ones growing from (or
    /// shrinking to) a zero-width band at the outer edge. It is a zoom
    /// whose anchor is the whole circle at the focus (see the module docs),
    /// run inward when rings are added and outward when they go. None when
    /// either chart is empty.
    pub fn ring_count(old: Arc<ChartModel>, new: Arc<ChartModel>, start: f64) -> Option<Self> {
        if old.arcs.is_empty() || new.arcs.is_empty() {
            return None;
        }
        let more = new.key.params.max_depth > old.key.params.max_depth;
        let (outer, inner) = if more { (old, new) } else { (new, old) };
        Some(Self::new(outer, inner, (0.0, TAU, 0), None, more, start))
    }

    /// Outer-chart colors of the inner arcs, aligned with `inner.arcs`
    /// (see `outer_colors`); empty outside Folders mode.
    fn outer_colors(&self, dark: bool) -> Cow<'_, [Option<Color32>]> {
        let compute = || -> Vec<Option<Color32>> {
            let Some(top_mid) = self.anchor_top_mid else {
                return Vec::new();
            };
            let (a0, a1, da) = self.anchor;
            let w = (a1 - a0).max(1e-6);
            self.inner
                .arcs
                .iter()
                .map(|a| {
                    (a.mark == Mark::None
                        && !a.is_rest()
                        && a.flags & (flags::FREE | flags::UNACCOUNTED) == 0)
                        .then(|| {
                            let mid = a0 + (a.a0 + a.a1) * 0.5 / TAU * w;
                            let d = i32::from(a.depth) + i32::from(da);
                            colors::folder_color(
                                colors::folder_hue_at(top_mid, mid),
                                d.clamp(1, 255) as u8,
                                a.flags & flags::DIR == 0,
                                dark,
                            )
                        })
                })
                .collect()
        };
        let (cached_dark, cached) = self.outer_colors.get_or_init(|| (dark, compute()));
        if *cached_dark == dark {
            Cow::Borrowed(cached.as_slice())
        } else {
            // The theme changed mid-animation (rare): no caching.
            Cow::Owned(compute())
        }
    }

    fn map<'a>(&self, now: f64, geo_outer: &'a Geometry, geo_inner: &'a Geometry) -> ZoomMap<'a> {
        let (a0, a1, da) = self.anchor;
        ZoomMap {
            p: self.zoom(now),
            a0,
            a1,
            w: (a1 - a0).max(1e-6),
            da: i32::from(da),
            go: geo_outer,
            gi: geo_inner,
        }
    }

    /// Linear progress 0..1 at time `now`.
    pub fn progress(&self, now: f64) -> f32 {
        ((now - self.start) as f32 / ZOOM_SECS).clamp(0.0, 1.0)
    }

    pub fn done(&self, now: f64) -> bool {
        self.progress(now) >= 1.0
    }

    /// Zoom amount: 0 = outer view, 1 = inner view.
    pub fn zoom(&self, now: f64) -> f32 {
        let e = ease(self.progress(now));
        if self.inward { e } else { 1.0 - e }
    }

    /// The arcs to draw at time `now`, with screen geometries interpolated
    /// between `geo_outer` and `geo_inner`.
    pub fn arcs(
        &self,
        now: f64,
        geo_outer: &Geometry,
        geo_inner: &Geometry,
        pal: &Pal,
    ) -> Vec<DrawArc> {
        let m = self.map(now, geo_outer, geo_inner);
        let (p, da) = (m.p, m.da);
        let mut out = Vec::with_capacity(
            self.outer.arcs.len()
                + self.inner.arcs.len()
                + self.outer.extended.len()
                + self.inner.extended.len(),
        );
        for a in &self.outer.arcs {
            let d = i32::from(a.depth);
            // The anchor's subtree is drawn from the inner model.
            if m.in_anchor_subtree(a) {
                continue;
            }
            out.push(DrawArc {
                a0: m.ang(a.a0),
                a1: m.ang(a.a1),
                r0: m.rad(d - 1),
                r1: m.rad(d),
                color: arc_color(a, pal.bg),
                fill: arc_fill(a, pal.dark),
            });
        }
        let from = self.outer_colors(pal.dark);
        for (k, a) in self.inner.arcs.iter().enumerate() {
            let d = i32::from(a.depth) + da;
            let oa0 = m.inner_to_outer(a.a0);
            let oa1 = m.inner_to_outer(a.a1);
            let mut color = arc_color(a, pal.bg);
            // Folders mode: fade from the color this arc had in the outer
            // chart.
            if let Some(Some(from)) = from.get(k) {
                color = colors::mix(*from, color, p);
            }
            out.push(DrawArc {
                a0: m.ang(oa0),
                a1: m.ang(oa1),
                r0: m.rad(d - 1),
                r1: m.rad(d),
                color,
                fill: arc_fill(a, pal.dark),
            });
        }
        out
    }

    /// Everything to draw at time `now`: [`Self::arcs`] followed by
    /// [`Self::extensions`].
    pub fn draw_arcs(
        &self,
        now: f64,
        geo_outer: &Geometry,
        geo_inner: &Geometry,
        pal: &Pal,
    ) -> Vec<DrawArc> {
        let mut out = self.arcs(now, geo_outer, geo_inner, pal);
        out.extend(self.extensions(now, geo_outer, geo_inner, pal));
        out
    }

    /// The extensions to draw at time `now`: those of both models, mapped
    /// like their arcs, reaching out to the outer edge of the deepest ring
    /// interpolated between the two charts.
    pub fn extensions(
        &self,
        now: f64,
        geo_outer: &Geometry,
        geo_inner: &Geometry,
        pal: &Pal,
    ) -> Vec<DrawArc> {
        let m = self.map(now, geo_outer, geo_inner);
        let ro = geo_outer.bound(i32::from(self.outer.depth()));
        let ri = geo_inner.bound(i32::from(self.inner.depth()));
        let edge = ro + (ri - ro) * m.p;
        let mut out = Vec::new();
        for &i in &self.outer.extended {
            let a = &self.outer.arcs[i as usize];
            if m.in_anchor_subtree(a) {
                continue;
            }
            let (color, fill) = extension_style(a, pal.bg, pal.dark);
            out.push(DrawArc {
                a0: m.ang(a.a0),
                a1: m.ang(a.a1),
                r0: m.rad(i32::from(a.depth)),
                r1: edge,
                color,
                fill,
            });
        }
        for &i in &self.inner.extended {
            let a = &self.inner.arcs[i as usize];
            let (color, fill) = extension_style(a, pal.bg, pal.dark);
            out.push(DrawArc {
                a0: m.ang(m.inner_to_outer(a.a0)),
                a1: m.ang(m.inner_to_outer(a.a1)),
                r0: m.rad(i32::from(a.depth) + m.da),
                r1: edge,
                color,
                fill,
            });
        }
        out
    }
}

/// Builds the mesh (plus outline shapes for free-space arcs).
pub fn build_mesh(arcs: &[DrawArc], center: Pos2, gap: f32) -> (Mesh, Vec<Shape>) {
    let mut mesh = Mesh::default();
    let mut extra = Vec::new();
    for a in arcs {
        tessellate(&mut mesh, center, a, gap);
        if let Fill::Outlined(stroke) = a.fill
            && a.a1 - a.a0 > 0.0
            && a.r1 - a.r0 > 1.0
        {
            let rg = gap * 0.5;
            let pts = outline_points(
                center,
                a.a0 + 0.004,
                a.a1 - 0.004,
                a.r0 + rg + 0.5,
                a.r1 - rg - 0.5,
            );
            extra.push(Shape::closed_line(pts, Stroke::new(1.0, stroke)));
        }
    }
    (mesh, extra)
}

/// Placement of one in-arc label.
pub struct Label {
    pub pos: Pos2,
    pub galley: Arc<egui::Galley>,
    pub angle: f32,
}

/// Room a label keeps from the edges of its sector, points.
const LABEL_MARGIN: f32 = 2.0;

/// Is the rectangle `w`×`h` centered at `c`, rotated by `rot`, fully inside
/// the annular sector (with `margin` points to spare)?
///
/// The outer circle is checked at the corners (the farthest points of a
/// rectangle from any center), the inner one against the point of each
/// edge closest to the center (a straight edge across a curved ring dips
/// toward the center between its corners), the straight sides of the
/// sector at the corners and edge midpoints.
fn rect_fits(
    geo: &Geometry,
    c: Pos2,
    w: f32,
    h: f32,
    rot: f32,
    sector: (f32, f32, f32, f32),
    margin: f32,
) -> bool {
    let (a0, a1, r0, r1) = sector;
    let (s, co) = rot.sin_cos();
    let (hw, hh) = (w * 0.5, h * 0.5);
    // Corners and edge midpoints (the arc bulges between corners).
    let probes = [
        (-hw, -hh),
        (hw, -hh),
        (hw, hh),
        (-hw, hh),
        (0.0, -hh),
        (0.0, hh),
        (-hw, 0.0),
        (hw, 0.0),
    ];
    let at = |(x, y): (f32, f32)| Pos2::new(c.x + x * co - y * s, c.y + x * s + y * co);
    for k in 0..4 {
        let (p, q) = (at(probes[k]), at(probes[(k + 1) % 4]));
        if dist_to_segment(geo.center, p, q) < r0 + margin {
            return false;
        }
    }
    for probe in probes {
        let p = at(probe);
        let (r, ang) = geo.polar(p);
        if r < r0 + margin || r > r1 - margin {
            return false;
        }
        // Angular distance from the sector edges, as a length at radius r.
        let rel = (ang - a0).rem_euclid(TAU);
        let span = a1 - a0;
        if span < TAU - 1e-3 && (rel > span || rel * r < margin || (span - rel) * r < margin) {
            return false;
        }
    }
    true
}

/// Distance from `c` to the segment `p`–`q`.
fn dist_to_segment(c: Pos2, p: Pos2, q: Pos2) -> f32 {
    let d = q - p;
    let t = ((c - p).dot(d) / d.length_sq().max(1e-12)).clamp(0.0, 1.0);
    (p + d * t).distance(c)
}

/// Shortens `name` with an ellipsis to roughly `max_w` points.
fn elide(name: &str, char_w: f32, max_w: f32) -> Option<String> {
    let n = name.chars().count();
    let fit = ((max_w / char_w.max(1.0)).floor() as usize).saturating_sub(1);
    if fit >= n {
        return Some(name.to_string());
    }
    if fit < 3 {
        return None;
    }
    let mut s: String = name.chars().take(fit).collect();
    s.push('…');
    Some(s)
}

/// Labels for the arcs of a model at rest: horizontal if the text fits,
/// else tangential, else radial (each kept upright), shortened with "…"
/// when needed, never overflowing the arc. Biggest arcs first, at most
/// `max_labels`. An extended arc whose name does not fit in its ring may
/// carry it in its extension.
pub fn layout_labels(
    painter: &egui::Painter,
    model: &ChartModel,
    geo: &Geometry,
    (bg, dark): (Color32, bool),
    max_labels: usize,
) -> Vec<Label> {
    let mut cands: Vec<usize> = model
        .arcs
        .iter()
        .enumerate()
        .filter(|(_, a)| a.label.is_some() && a.mark != Mark::Dim)
        .map(|(i, _)| i)
        .collect();
    let area = |a: &ArcInfo| {
        let d = i32::from(a.depth);
        let (r0, r1) = (geo.bound(d - 1), geo.bound(d));
        (a.a1 - a.a0) * (r0 + r1) * 0.5 * (r1 - r0)
    };
    cands.sort_by(|&x, &y| area(&model.arcs[y]).total_cmp(&area(&model.arcs[x])));
    let mut out = Vec::new();
    let outer = geo.bound(i32::from(model.depth()));
    for i in cands {
        if out.len() >= max_labels {
            break;
        }
        let a = &model.arcs[i];
        let d = i32::from(a.depth);
        let size = if a.depth <= 1 { 12.5 } else { 11.5 };
        let font = FontId::proportional(size);
        let name = a.label.as_deref().unwrap_or("");
        // Its own ring, then the extension above it (if any). Translucent
        // fills (free space) are read against the background.
        let shown = colors::over(arc_color(a, bg), bg);
        let mut slots = vec![((a.a0, a.a1, geo.bound(d - 1), geo.bound(d)), shown)];
        if model.extended.contains(&(i as u32)) {
            let ext = colors::over(extension_style(a, bg, dark).0, bg);
            slots.push(((a.a0, a.a1, geo.bound(d), outer), ext));
        }
        if let Some((pos, galley, angle)) =
            place_label_in(painter, geo, (name, &font), &slots, a.mid())
        {
            out.push(Label { pos, galley, angle });
        }
    }
    out
}

/// An annular sector (`a0, a1, r0, r1`) a label may go into, with the
/// opaque fill its text is read against.
type LabelSlot = ((f32, f32, f32, f32), Color32);

/// Fits `name` into the first of `slots` that takes it in full, else into
/// the first that takes it shortened, so an arc's thin ring does not cut
/// a name its extension has room for (see [`layout_labels`]). Returns the
/// position of the galley's top-left corner, the galley and its rotation.
fn place_label_in(
    painter: &egui::Painter,
    geo: &Geometry,
    text: (&str, &FontId),
    slots: &[LabelSlot],
    mid: f32,
) -> Option<(Pos2, Arc<egui::Galley>, f32)> {
    [false, true].into_iter().find_map(|shorten| {
        slots
            .iter()
            .find_map(|&slot| place_label(painter, geo, text, slot, mid, shorten))
    })
}

/// Fits `name` into one sector: in full (horizontal, tangential or radial)
/// or, with `shorten`, elided in the orientation with the most room.
fn place_label(
    painter: &egui::Painter,
    geo: &Geometry,
    (name, font): (&str, &FontId),
    (sector, fill): LabelSlot,
    mid: f32,
    shorten: bool,
) -> Option<(Pos2, Arc<egui::Galley>, f32)> {
    let text_color = colors::text_on(fill);
    let (a0, a1, r0, r1) = sector;
    let line_h = font.size * 1.3;
    let thick = r1 - r0;
    let span = a1 - a0;
    if thick < line_h * 0.9 && span * r1 < line_h * 0.9 {
        return None;
    }
    let rm = (r0 + r1) * 0.5;
    let full = painter.layout_no_wrap(name.to_string(), font.clone(), text_color);
    let (fw, fh) = (full.size().x, full.size().y);
    let char_w = fw / name.chars().count().max(1) as f32;
    // Upright rotations: tangential (perpendicular to the radius) and
    // radial (along it).
    let tangential = {
        let mut t = mid;
        if mid > TAU * 0.25 && mid < TAU * 0.75 {
            t -= std::f32::consts::PI;
        }
        t
    };
    let radial = {
        let mut t = mid - TAU * 0.25;
        if mid > std::f32::consts::PI {
            t += std::f32::consts::PI;
        }
        t
    };
    // Where a `w`×`h` text rotated by `rot` can go: centered in the ring,
    // or, with `inward` (across the ring, i.e. not radial), moved inward
    // until its inner edge touches the margin. A straight line of text in
    // a curved ring meets the outer edge with its corners and the inner
    // edge with its middle, so the inward shift gives the corners the room
    // the curvature takes (~w²/8r): a 95-pt name at r = 100 needs ~11 pt
    // less ring.
    let center_for = |w: f32, h: f32, rot: f32, inward: bool| -> Option<Pos2> {
        let r = if inward {
            let inner = r0 + LABEL_MARGIN + h * 0.5 + 0.01;
            if rot == radial || inner >= rm {
                return None;
            }
            inner
        } else {
            rm
        };
        let c = geo.point(r, mid);
        rect_fits(geo, c, w, h, rot, sector, LABEL_MARGIN).then_some(c)
    };
    let mut placed: Option<(Arc<egui::Galley>, f32, Pos2)> = None;
    if !shorten {
        // Full name: horizontal or tangential, centered in the ring if
        // either fits there, else moved inward; else radial. A name across
        // the ring reads better than one along it, even off-center.
        placed = [
            (0.0, false),
            (tangential, false),
            (0.0, true),
            (tangential, true),
            (radial, false),
        ]
        .into_iter()
        .find_map(|(rot, inward)| center_for(fw, fh, rot, inward).map(|c| (full.clone(), rot, c)));
    } else {
        // Shortened, in the orientation with the most room.
        let along_tangent = span * r0 - 6.0;
        let along_radius = thick - 6.0;
        let (rot, room) = if along_tangent >= along_radius {
            (tangential, along_tangent)
        } else {
            (radial, along_radius)
        };
        if let Some(short) = elide(name, char_w, room) {
            let g = painter.layout_no_wrap(short, font.clone(), text_color);
            let (w, h) = (g.size().x, g.size().y);
            if let Some(c) = center_for(w, h, rot, false).or_else(|| center_for(w, h, rot, true)) {
                placed = Some((g, rot, c));
            }
        }
    }
    let (galley, rot, c) = placed?;
    // TextShape rotates around its top-left corner: offset so the text is
    // centered on `c`.
    let (s, co) = rot.sin_cos();
    let (hw, hh) = (galley.size().x * 0.5, galley.size().y * 0.5);
    let pos = Pos2::new(c.x - hw * co + hh * s, c.y - hw * s - hh * co);
    Some((pos, galley, rot))
}

pub fn label_shapes(labels: &[Label], alpha: f32) -> Vec<Shape> {
    labels
        .iter()
        .map(|l| {
            Shape::Text(
                TextShape::new(l.pos, l.galley.clone(), Color32::PLACEHOLDER)
                    .with_angle(l.angle)
                    .with_opacity_factor(alpha),
            )
        })
        .collect()
}

/// Draws a highlight over arc `a` (hover / selection / list sync). With
/// `ext` (the outer radius of the arc's extension) the outline goes around
/// the whole wedge and the extension gets a light overlay.
pub fn highlight_shapes(
    geo: &Geometry,
    a: &ArcInfo,
    pal: &Pal,
    strong: bool,
    ext: Option<f32>,
) -> Vec<Shape> {
    let d = i32::from(a.depth);
    let (r0, r1) = (geo.bound(d - 1), geo.bound(d));
    let mut mesh = Mesh::default();
    let lighter = if pal.dark {
        colors::mix(a.color, Color32::WHITE, 0.22)
    } else {
        colors::mix(a.color, Color32::WHITE, 0.32)
    };
    let fill = match arc_fill(a, pal.dark) {
        Fill::Stripes(_) | Fill::Outlined(_) => colors::mix(a.color, pal.text, 0.12),
        Fill::Solid | Fill::Hatched(_) => lighter,
    };
    tessellate(
        &mut mesh,
        geo.center,
        &DrawArc {
            a0: a.a0,
            a1: a.a1,
            r0,
            r1,
            color: fill,
            fill: Fill::Solid,
        },
        1.0,
    );
    let ext = ext.filter(|&e| e > r1 + 1.0);
    if let Some(e) = ext {
        tessellate(
            &mut mesh,
            geo.center,
            &DrawArc {
                a0: a.a0,
                a1: a.a1,
                r0: r1,
                r1: e,
                color: pal.text.gamma_multiply(if pal.dark { 0.07 } else { 0.05 }),
                fill: Fill::Solid,
            },
            1.0,
        );
    }
    let stroke_color = if strong { pal.accent } else { pal.text };
    let width = if strong { 2.0 } else { 1.25 };
    let pts = outline_points(geo.center, a.a0, a.a1, r0 + 0.5, ext.unwrap_or(r1) - 0.5);
    vec![
        Shape::mesh(mesh),
        Shape::closed_line(pts, Stroke::new(width, stroke_color)),
    ]
}

/// Paints the center disc: focus name, size and smaller lines under it
/// (file count, free space; empty lines are skipped).
#[allow(clippy::too_many_arguments)]
pub fn paint_center(
    painter: &egui::Painter,
    geo: &Geometry,
    pal: &Pal,
    name: &str,
    size_text: &str,
    sub_lines: &[&str],
    hovered: bool,
    alpha: f32,
    show_up: bool,
) {
    let r = geo.r0() - 1.0;
    let c = geo.center;
    let fill = if hovered { pal.disc_hover } else { pal.disc };
    painter.circle_filled(
        c + egui::vec2(0.0, 1.5),
        r + 0.5,
        pal.shadow.gamma_multiply(0.5),
    );
    painter.circle(c, r, fill, Stroke::new(1.0, pal.border));
    if alpha <= 0.01 {
        return;
    }
    let max_w = r * 1.7;
    let fit = |text: &str, font: FontId, color: Color32| {
        let g = painter.layout_no_wrap(text.to_string(), font.clone(), color);
        if g.size().x <= max_w {
            return g;
        }
        let cw = g.size().x / text.chars().count().max(1) as f32;
        let short = elide(text, cw, max_w).unwrap_or_else(|| "…".into());
        painter.layout_no_wrap(short, font, color)
    };
    let big = (r * 0.36).clamp(17.0, 34.0);
    let name_size = (r * 0.2).clamp(12.0, 16.0);
    let small = (r * 0.17).clamp(11.0, 13.0);
    let g_name = fit(
        name,
        crate::theme::semibold(name_size),
        pal.text.gamma_multiply(alpha),
    );
    let g_size = fit(
        size_text,
        crate::theme::semibold(big),
        pal.text.gamma_multiply(alpha),
    );
    let mut galleys = vec![g_name, g_size];
    for line in sub_lines.iter().filter(|l| !l.is_empty()) {
        galleys.push(fit(
            line,
            FontId::proportional(small),
            pal.text_dim.gamma_multiply(alpha),
        ));
    }
    let total_h: f32 = galleys.iter().map(|g| g.size().y).sum();
    let mut y = c.y - total_h * 0.5;
    for g in galleys {
        let pos = Pos2::new(c.x - g.size().x * 0.5, y);
        y += g.size().y;
        painter.galley(pos, g, Color32::PLACEHOLDER);
    }
    if hovered && show_up {
        let g = painter.layout_no_wrap(
            crate::theme::icon::UP.to_string(),
            crate::theme::icon_font(12.0),
            pal.accent.gamma_multiply(alpha),
        );
        let pos = Pos2::new(c.x - g.size().x * 0.5, c.y - r + 6.0);
        painter.galley(pos, g, Color32::PLACEHOLDER);
    }
}

/// Draws a small legend box anchored at `anchor` (bottom-left corner).
/// The legend's last row: what the hatched wedges above unaccounted and
/// reserved space mean (shown while one is on the chart).
pub struct LegendHatch {
    pub label: String,
    /// Color of the arc itself (drawn as the swatch's bottom band).
    pub arc: Color32,
    /// The extension's fill and hatch line colors.
    pub fill: Color32,
    pub lines: Color32,
}

impl LegendHatch {
    /// The row for the first unaccounted or reserved extension of `model`,
    /// if it has one.
    pub fn of(model: &ChartModel, pal: &Pal, label: &str) -> Option<LegendHatch> {
        let a = model
            .extended
            .iter()
            .map(|&i| &model.arcs[i as usize])
            .find(|a| a.flags & flags::UNACCOUNTED != 0)?;
        // Unmarked colors: a search does not dim the legend.
        let (fill, lines) = hatch_colors(a.color, pal.bg, pal.dark);
        Some(LegendHatch {
            label: label.to_string(),
            arc: a.color,
            fill,
            lines,
        })
    }
}

/// A 12×12 legend swatch of an extended arc: the arc as a band at the
/// bottom, its hatched extension above.
fn hatch_swatch(painter: &egui::Painter, sw: Rect, h: &LegendHatch) {
    painter.rect_filled(sw, 3.0, h.fill);
    let clip = painter.with_clip_rect(sw.intersect(painter.clip_rect()));
    let mut x = sw.left() - sw.height();
    while x < sw.right() {
        clip.line_segment(
            [
                Pos2::new(x, sw.bottom()),
                Pos2::new(x + sw.height(), sw.top()),
            ],
            Stroke::new(1.0, h.lines),
        );
        x += 4.0;
    }
    let band = Rect::from_min_max(Pos2::new(sw.left(), sw.bottom() - 4.0), sw.max);
    painter.rect_filled(
        band,
        egui::CornerRadius {
            nw: 0,
            ne: 0,
            sw: 3,
            se: 3,
        },
        h.arc,
    );
}

pub fn paint_legend(
    painter: &egui::Painter,
    anchor: Pos2,
    pal: &Pal,
    rows: &[(Color32, String)],
    gradient: Option<&[Color32]>,
    hatch: Option<&LegendHatch>,
) {
    let font = FontId::proportional(12.0);
    let galleys: Vec<_> = rows
        .iter()
        .map(|(_, t)| painter.layout_no_wrap(t.clone(), font.clone(), pal.text_dim))
        .collect();
    let hatch_galley =
        hatch.map(|h| painter.layout_no_wrap(h.label.clone(), font.clone(), pal.text_dim));
    let row_h = 18.0;
    let pad = 10.0;
    let text_w = galleys
        .iter()
        .chain(&hatch_galley)
        .map(|g| g.size().x)
        .fold(0.0, f32::max);
    // The hatch row: below the gradient bar, or one more swatch row.
    let hatch_h = match (hatch.is_some(), gradient.is_some()) {
        (false, _) => 0.0,
        (true, true) => 20.0,
        (true, false) => row_h,
    };
    let (w, h) = if gradient.is_some() {
        (
            pad * 2.0 + 14.0 + 8.0 + text_w,
            pad * 2.0 + row_h * (rows.len() as f32 - 1.0) + 4.0 + hatch_h,
        )
    } else {
        (
            pad * 2.0 + 12.0 + 8.0 + text_w,
            pad * 2.0 + row_h * rows.len() as f32 - 4.0 + hatch_h,
        )
    };
    let rect = Rect::from_min_size(Pos2::new(anchor.x, anchor.y - h), egui::vec2(w, h));
    painter.rect(
        rect,
        8.0,
        pal.card.gamma_multiply(0.92),
        Stroke::new(1.0, pal.border),
        egui::StrokeKind::Inside,
    );
    if let Some(stops) = gradient {
        // Vertical gradient bar with tick labels at the stops.
        let bar = Rect::from_min_size(
            Pos2::new(rect.left() + pad, rect.top() + pad + 2.0),
            egui::vec2(14.0, row_h * (rows.len() as f32 - 1.0)),
        );
        let mut mesh = Mesh::default();
        let n = stops.len().max(2) - 1;
        for (k, pair) in stops.windows(2).enumerate() {
            let y0 = bar.top() + bar.height() * k as f32 / n as f32;
            let y1 = bar.top() + bar.height() * (k + 1) as f32 / n as f32;
            let base = mesh.vertices.len() as u32;
            mesh.colored_vertex(Pos2::new(bar.left(), y0), pair[0]);
            mesh.colored_vertex(Pos2::new(bar.right(), y0), pair[0]);
            mesh.colored_vertex(Pos2::new(bar.left(), y1), pair[1]);
            mesh.colored_vertex(Pos2::new(bar.right(), y1), pair[1]);
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base + 1, base + 3, base + 2);
        }
        painter.add(Shape::mesh(mesh));
        for (k, g) in galleys.into_iter().enumerate() {
            let y = bar.top() + row_h * k as f32;
            let pos = Pos2::new(bar.right() + 8.0, y - g.size().y * 0.5);
            painter.galley(pos, g, Color32::PLACEHOLDER);
        }
        if let (Some(h), Some(g)) = (hatch, hatch_galley) {
            let sw = Rect::from_min_size(
                Pos2::new(bar.left() + 1.0, bar.bottom() + 12.0),
                egui::vec2(12.0, 12.0),
            );
            hatch_swatch(painter, sw, h);
            let pos = Pos2::new(bar.right() + 8.0, sw.center().y - g.size().y * 0.5);
            painter.galley(pos, g, Color32::PLACEHOLDER);
        }
    } else {
        for (k, (g, (color, _))) in galleys.into_iter().zip(rows).enumerate() {
            let y = rect.top() + pad + row_h * k as f32;
            let sw = Rect::from_min_size(
                Pos2::new(rect.left() + pad, y + 1.0),
                egui::vec2(12.0, 12.0),
            );
            painter.rect_filled(sw, 3.0, *color);
            let pos = Pos2::new(sw.right() + 8.0, y + 7.0 - g.size().y * 0.5);
            painter.galley(pos, g, Color32::PLACEHOLDER);
        }
        if let (Some(h), Some(g)) = (hatch, hatch_galley) {
            let y = rect.top() + pad + row_h * rows.len() as f32;
            let sw = Rect::from_min_size(
                Pos2::new(rect.left() + pad, y + 1.0),
                egui::vec2(12.0, 12.0),
            );
            hatch_swatch(painter, sw, h);
            let pos = Pos2::new(sw.right() + 8.0, y + 7.0 - g.size().y * 0.5);
            painter.galley(pos, g, Color32::PLACEHOLDER);
        }
    }
    let _ = Align2::LEFT_TOP;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelKey, VirtualNames};
    use crate::settings::ColorMode;
    use ringscan_core::layout::Params;
    use ringscan_core::synth;

    const NAMES: VirtualNames = VirtualNames {
        this_pc: "This PC",
        free: "Free",
        unaccounted: "Unaccounted",
        reserved: "Reserved",
        orphaned: "Orphaned",
    };

    fn key(focus: u32, generation: u64) -> ModelKey {
        ModelKey {
            scan: 1,
            generation,
            focus,
            params: Params::default(),
            color_mode: ColorMode::Folders,
            dark: false,
            lang: ringscan_core::Lang::En,
            search: 0,
            now: 1_767_225_600,
        }
    }

    #[test]
    fn geometry_rings_and_hits() {
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 600.0));
        let g = Geometry::new(rect, 12);
        assert_eq!(g.bounds.len(), 13);
        assert!((g.outer() - 286.0).abs() < 0.01, "outer {}", g.outer());
        assert!(g.bounds.windows(2).all(|w| w[1] > w[0]));
        // Rings shrink outward.
        let w1 = g.bounds[1] - g.bounds[0];
        let w12 = g.bounds[12] - g.bounds[11];
        assert!(w12 < w1 && w12 > w1 * 0.4);
        assert_eq!(g.ring_at(g.r0() * 0.5), Some(0));
        for d in 1..=12u8 {
            let mid = (g.bound(i32::from(d) - 1) + g.bound(i32::from(d))) / 2.0;
            assert_eq!(g.ring_at(mid), Some(d));
        }
        assert_eq!(g.ring_at(g.outer() + 1.0), None);
        // Polar coordinates: 12 o'clock is 0, clockwise.
        for ang in [0.0f32, 1.0, 3.0, 5.5] {
            let p = g.point(100.0, ang);
            let (r, a) = g.polar(p);
            assert!((r - 100.0).abs() < 1e-3 && (a - ang).abs() < 1e-4);
        }
        let right = g.polar(g.center + egui::vec2(50.0, 0.0)).1;
        assert!((right - TAU / 4.0).abs() < 1e-5);
        // Small windows still get a usable chart.
        let tiny = Geometry::new(Rect::from_min_size(Pos2::ZERO, egui::vec2(100.0, 80.0)), 3);
        assert!(tiny.r0() > 0.0 && tiny.outer() > tiny.r0());
    }

    #[test]
    fn tessellation_counts() {
        let mut mesh = Mesh::default();
        let arc = DrawArc {
            a0: 0.0,
            a1: 1.0,
            r0: 50.0,
            r1: 80.0,
            color: Color32::RED,
            fill: Fill::Solid,
        };
        tessellate(&mut mesh, Pos2::ZERO, &arc, 1.0);
        // Segment count follows the outer arc length (80 px / 4 px).
        assert_eq!(mesh.vertices.len(), 2 * (20 + 1));
        assert_eq!(mesh.indices.len(), 3 * 2 * 20);
        assert!(mesh.is_valid());
        // Empty spans and transparent arcs add nothing.
        let n = mesh.vertices.len();
        tessellate(&mut mesh, Pos2::ZERO, &DrawArc { a1: 0.0, ..arc }, 1.0);
        tessellate(
            &mut mesh,
            Pos2::ZERO,
            &DrawArc {
                color: Color32::TRANSPARENT,
                ..arc
            },
            1.0,
        );
        assert_eq!(mesh.vertices.len(), n);
        // Every vertex stays inside the annulus (gap insets included).
        for v in &mesh.vertices {
            let r = v.pos.to_vec2().length();
            assert!((50.0..=80.0).contains(&r));
        }
        // Stripes: one quad strip per stripe.
        let mut striped = Mesh::default();
        tessellate(
            &mut striped,
            Pos2::ZERO,
            &DrawArc {
                fill: Fill::Stripes(Color32::GRAY),
                ..arc
            },
            1.0,
        );
        assert!(striped.vertices.len() > mesh.vertices.len() / 2);
        assert!(striped.is_valid());
    }

    #[test]
    fn transition_endpoints_match_static_charts() {
        // Same ring count, and different ones (the automatic count changed
        // between the two layouts).
        for (n_outer, n_inner) in [(12, 12), (12, 6), (6, 12), (5, 16)] {
            transition_endpoints_with_rings(n_outer, n_inner);
        }
    }

    fn transition_endpoints_with_rings(n_outer: u8, n_inner: u8) {
        let tree = synth::tree(3, 4000);
        let with_rings = |focus, n| {
            let mut k = key(focus, tree.generation());
            k.params.max_depth = n;
            ChartModel::build(&tree, k, None, &NAMES)
        };
        let outer = with_rings(0, n_outer);
        // Zoom into the widest zoomable ring-1 arc.
        let a = outer
            .arcs
            .iter()
            .filter(|a| a.depth == 1 && a.zoomable)
            .max_by(|x, y| x.span().total_cmp(&y.span()))
            .expect("a zoomable arc")
            .clone();
        let target = a.node().unwrap();
        let inner = with_rings(target, n_inner);
        assert!(outer.depth() <= n_outer && inner.depth() <= n_inner);
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 800.0));
        let (go, gi) = (
            Geometry::new(rect, outer.depth()),
            Geometry::new(rect, inner.depth()),
        );
        let pal = crate::theme::pal(false);
        let t = Transition::new(
            Arc::new(outer),
            Arc::new(inner),
            (a.a0, a.a1, a.depth),
            None,
            true,
            0.0,
        );
        // p = 1: the inner arcs sit exactly where the static inner chart
        // draws them; outer arcs outside the anchor are squeezed to nothing.
        let end = t.arcs(10.0, &go, &gi, &pal);
        let n_outer = end.len() - t.inner.arcs.len();
        let inner_static = static_arcs(&t.inner, &gi, &pal);
        for (x, y) in end[n_outer..].iter().zip(&inner_static) {
            assert!((x.a0 - y.a0).abs() < 1e-4 && (x.a1 - y.a1).abs() < 1e-4);
            assert!((x.r0 - y.r0).abs() < 1e-3 && (x.r1 - y.r1).abs() < 1e-3);
        }
        for x in &end[..n_outer] {
            let visible_span = x.a1 - x.a0;
            let inside_disc = x.r1 <= gi.r0() + 1e-3;
            assert!(visible_span < 1e-4 || inside_disc || (x.a0 <= 1e-4 && x.a1 >= TAU - 1e-4));
        }
        // p = 0: the outer arcs are drawn where the static outer chart has
        // them, and the inner arcs inside the anchor's span.
        let start = t.arcs(0.0, &go, &gi, &pal);
        for x in &start[n_outer..] {
            assert!(x.a0 >= a.a0 - 1e-4 && x.a1 <= a.a1 + 1e-4);
            assert!(x.r0 >= go.bound(i32::from(a.depth)) - 1e-3);
            // Deeper than the outer chart's rings: a zero-width band at
            // its edge, growing from there.
            assert!(x.r1 <= go.outer() + 1e-3);
        }
        // In between, every arc stays a proper annular sector inside the
        // larger of the two charts.
        let edge = go.outer().max(gi.outer());
        for k in 1..10 {
            let now = f64::from(ZOOM_SECS) * f64::from(k) / 10.0;
            for x in t.draw_arcs(now, &go, &gi, &pal) {
                assert!(x.r0.is_finite() && x.r1.is_finite() && x.a0.is_finite());
                assert!(x.r0 <= x.r1 + 1e-3 && x.a0 <= x.a1 + 1e-4, "{x:?}");
                assert!(x.r1 <= edge + 1e-3);
            }
        }
    }

    #[test]
    fn ring_count_transition_runs_between_the_two_static_charts() {
        let tree = synth::tree(3, 4000);
        let with_rings = |n| {
            let mut k = key(0, tree.generation());
            k.params.max_depth = n;
            Arc::new(ChartModel::build(&tree, k, None, &NAMES))
        };
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(700.0, 560.0));
        let pal = crate::theme::pal(false);
        for (from, to) in [(8, 12), (12, 8), (3, 9), (9, 3)] {
            let (old, new) = (with_rings(from), with_rings(to));
            assert!(
                old.depth() == from && new.depth() == to,
                "a deep enough tree"
            );
            let t = Transition::ring_count(Arc::clone(&old), Arc::clone(&new), 0.0).unwrap();
            let (go, gi) = (Geometry::of(rect, &t.outer), Geometry::of(rect, &t.inner));
            // The model with more rings is drawn throughout, at the old
            // chart's geometry first and the new one's last; its rings past
            // the other count are a zero-width band at the outer edge.
            let more = if from < to { &new } else { &old };
            let check = |now: f64, m: &ChartModel, at: &Geometry| {
                let drawn = t.draw_arcs(now, &go, &gi, &pal);
                let want = static_arcs(more, &Geometry::of(rect, m), &pal);
                assert_eq!(drawn.len(), want.len(), "{from}->{to} at {now}");
                for (x, y) in drawn.iter().zip(&want) {
                    assert!((x.a0 - y.a0).abs() < 1e-4 && (x.a1 - y.a1).abs() < 1e-4);
                    if x.r0 < at.outer() - 1e-3 {
                        assert!((x.r0 - y.r0).abs() < 1e-3 && (x.r1 - y.r1).abs() < 1e-3);
                    } else {
                        assert!((x.r1 - at.outer()).abs() < 1e-3, "{from}->{to}: {x:?}");
                    }
                }
            };
            let (g_old, g_new) = (Geometry::of(rect, &old), Geometry::of(rect, &new));
            check(0.0, &old, &g_old);
            check(10.0, &new, &g_new);
            // In between, every arc stays inside the larger chart.
            let edge = g_old.outer().max(g_new.outer());
            for k in 1..10 {
                let now = f64::from(ZOOM_SECS) * f64::from(k) / 10.0;
                for x in t.draw_arcs(now, &go, &gi, &pal) {
                    assert!(x.r0 <= x.r1 + 1e-3 && x.r1 <= edge + 1e-3, "{x:?}");
                }
            }
        }
        // Nothing to animate on an empty chart.
        let empty = Arc::new(ChartModel::empty(key(0, 0)));
        assert!(Transition::ring_count(empty, with_rings(8), 0.0).is_none());
    }

    fn square(side: f32) -> Rect {
        Rect::from_min_size(Pos2::ZERO, egui::vec2(side, side))
    }

    fn rect(w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::ZERO, egui::vec2(w, h))
    }

    #[test]
    fn auto_ring_count_grows_with_the_chart_and_keeps_rings_readable() {
        for min_width in [AUTO_RING_MIN_WIDTH, AUTO_RING_MIN_WIDTH_COLORS] {
            let mut last = 0;
            for side in (80..=4000).step_by(7) {
                let rect = square(side as f32);
                let n = auto_rings(rect, min_width);
                assert!(AUTO_RINGS.contains(&n), "{side}: {n}");
                assert!(n >= last, "{side}: {n} after {last}");
                last = n;
                // The outermost ring is `min_width` wide (unless even the
                // fewest rings are thinner), and one more ring would not be.
                let w = outermost_ring(rect, n);
                assert!(w >= min_width || n == *AUTO_RINGS.start());
                if n < *AUTO_RINGS.end() {
                    assert!(outermost_ring(rect, n + 1) < min_width);
                }
            }
            assert_eq!(auto_rings(square(150.0), min_width), *AUTO_RINGS.start());
            assert_eq!(auto_rings(square(4000.0), min_width), *AUTO_RINGS.end());
            // The smaller side decides.
            assert_eq!(
                auto_rings(rect(3000.0, 567.0), min_width),
                auto_rings(square(567.0), min_width)
            );
        }
        // The chart rectangles of 1280×640, 1280×800 and 1920×1080 windows
        // with both side panels: Folders mode, then file types and age
        // (colors rather than names: more, thinner rings).
        let at = |w, h, mode| auto_rings(rect(w, h), auto_ring_min_width(mode));
        let sizes = [(660.0, 567.0), (660.0, 727.0), (1300.0, 1007.0)];
        let folders: Vec<u8> = sizes
            .iter()
            .map(|&(w, h)| at(w, h, ColorMode::Folders))
            .collect();
        assert_eq!(folders, [8, 9, 12]);
        for mode in [ColorMode::Types, ColorMode::Age] {
            let colors: Vec<u8> = sizes.iter().map(|&(w, h)| at(w, h, mode)).collect();
            assert_eq!(colors, [11, 12, 16], "{mode:?}");
        }
    }

    #[test]
    fn the_smallest_window_gets_fewer_rings_with_room_for_names() {
        // The chart rectangle of a 760×520 window (the minimum size) with
        // both side panels. Five rings (what the geometry used to reserve
        // at least) were 18 pt to 13 pt wide: too thin for any name.
        let r = rect(280.0, 447.0);
        assert!(Geometry::new(r, 5).bounds[1] - Geometry::new(r, 5).bounds[0] < 21.0);
        let n = auto_rings(r, AUTO_RING_MIN_WIDTH);
        assert_eq!(n, 3);
        // A model laid out for that many rings gets the whole radius, so
        // every ring takes a line of text: 17 pt at 12.5 pt on ring 1,
        // 15 pt at 11.5 pt further out, plus the margins.
        let tree = synth::tree(3, 4000);
        let mut k = key(0, tree.generation());
        k.params.max_depth = n;
        let m = ChartModel::build(&tree, k, None, &NAMES);
        assert_eq!(m.depth(), n);
        let g = Geometry::of(r, &m);
        assert_eq!(g.bounds.len(), usize::from(n) + 1);
        let widths: Vec<f32> = g.bounds.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(widths[0] >= 17.0 + 2.0 * LABEL_MARGIN, "{widths:?}");
        assert!(
            widths.iter().all(|&w| w >= AUTO_RING_MIN_WIDTH),
            "{widths:?}"
        );
        // Colors: more rings (as many as the geometry used to reserve).
        assert_eq!(auto_rings(r, AUTO_RING_MIN_WIDTH_COLORS), 5);
        // A shallow folder in a chart laid out for 12 rings still gets the
        // usual five rings' room.
        let mut k = key(0, tree.generation());
        k.params.max_depth = 12;
        let deep = ChartModel::build(&tree, k, None, &NAMES);
        assert_eq!(Geometry::of(r, &deep), Geometry::new(r, deep.depth()));
        assert!(Geometry::of(r, &deep).bounds.len() > usize::from(MIN_GEOMETRY_RINGS));
    }

    #[test]
    fn auto_ring_count_has_hysteresis() {
        let auto = |side: f32| auto_rings(square(side), AUTO_RING_MIN_WIDTH);
        let sticky =
            |side: f32, current| auto_rings_sticky(square(side), current, AUTO_RING_MIN_WIDTH);
        // The first size that gets one ring more than the size before.
        let side = (300..2000)
            .map(|s| s as f32)
            .find(|&s| auto(s + 1.0) > auto(s))
            .expect("a threshold");
        let (below, above) = (auto(side), auto(side + 1.0));
        // Just across the threshold either way: the current count stays.
        assert_eq!(sticky(side + 1.0, below), below);
        assert_eq!(sticky(side, above), above);
        // Well past it: the count follows.
        let far = 2.0 * AUTO_RINGS_SLACK + 4.0;
        assert_eq!(sticky(side + 1.0 + far, below), above);
        assert_eq!(sticky(side - far, above), below);
        // A count that never fits this size (a fixed one switched to Auto).
        assert_eq!(sticky(side, 24), below);
    }

    #[test]
    fn hatch_stays_inside_the_sector() {
        let center = Pos2::new(400.0, 300.0);
        // A thin wedge, a wide one (hatched in pieces) and a full ring.
        for (a0, a1) in [(0.3f32, 0.36f32), (1.0, 5.2), (0.0, TAU)] {
            let arc = DrawArc {
                a0,
                a1,
                r0: 90.0,
                r1: 260.0,
                color: Color32::GRAY,
                fill: Fill::Hatched(Color32::BLACK),
            };
            let mut plain = Mesh::default();
            tessellate(
                &mut plain,
                center,
                &DrawArc {
                    fill: Fill::Solid,
                    ..arc
                },
                1.0,
            );
            let mut mesh = Mesh::default();
            tessellate(&mut mesh, center, &arc, 1.0);
            assert!(mesh.is_valid());
            let lines = &mesh.vertices[plain.vertices.len()..];
            assert!(
                lines.len() >= 8,
                "{a0}..{a1}: {} hatch vertices",
                lines.len()
            );
            assert!(lines.iter().all(|v| v.color == Color32::BLACK));
            let geo = Geometry {
                center,
                bounds: vec![90.0, 260.0],
            };
            for v in lines {
                let (r, ang) = geo.polar(v.pos);
                // Inside the inset annulus (chords of the clip circles
                // stay within a hundredth of a point).
                assert!((90.5 - 0.01..=259.5 + 0.01).contains(&r), "r {r}");
                if a1 - a0 < TAU - 1e-3 {
                    // Inside the wedge, 0.5 pt from its straight edges.
                    let rel = (ang - a0).rem_euclid(TAU);
                    assert!(rel <= a1 - a0 + 1e-4, "angle {ang}");
                    let q = v.pos - center;
                    let start = q.dot(egui::vec2(a0.cos(), a0.sin()));
                    let end = -q.dot(egui::vec2(a1.cos(), a1.sin()));
                    if a1 - a0 <= std::f32::consts::PI {
                        assert!(start >= 0.5 - 1e-3 && end >= 0.5 - 1e-3, "{start} {end}");
                    }
                }
            }
        }
        // Too thin to hatch: the fill only.
        let mut mesh = Mesh::default();
        let thin = DrawArc {
            a0: 0.0,
            a1: 1.0,
            r0: 100.0,
            r1: 101.5,
            color: Color32::GRAY,
            fill: Fill::Hatched(Color32::BLACK),
        };
        tessellate(&mut mesh, center, &thin, 1.0);
        assert!(mesh.vertices.iter().all(|v| v.color == Color32::GRAY));
    }

    fn this_pc_models(focus_drive: bool) -> (ChartModel, ChartModel, ArcInfo) {
        let tree = synth::this_pc(9, 20_000);
        let outer = ChartModel::build(&tree, key(0, tree.generation()), None, &NAMES);
        let c = tree.children(0).next().unwrap();
        let target = if focus_drive {
            c
        } else {
            // The biggest folder on C:, three levels down.
            let mut f = c;
            for _ in 0..3 {
                f = tree
                    .children(f)
                    .filter(|&x| tree.node(x).is_dir() && tree.has_children(x))
                    .max_by_key(|&x| tree.size(x, ringscan_core::SizeMode::Allocated))
                    .unwrap();
            }
            f
        };
        let mut inner_key = key(target, tree.generation());
        inner_key.params.show_free = true;
        let inner = ChartModel::build(&tree, inner_key, None, &NAMES);
        let a = outer.arcs[outer.index_of(target).expect("visible")].clone();
        (outer, inner, a)
    }

    #[test]
    fn static_extensions_reach_the_deepest_ring() {
        let tree = synth::this_pc(9, 20_000);
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 700.0));
        for dark in [false, true] {
            let k = ModelKey {
                dark,
                ..key(0, tree.generation())
            };
            let m = ChartModel::build(&tree, k, None, &NAMES);
            let geo = Geometry::new(rect, m.depth());
            let pal = crate::theme::pal(dark);
            let ext = static_extensions(&m, &geo, &pal);
            assert_eq!(ext.len(), m.extended.len());
            // Unaccounted and free space of C: and D:, C:'s reserved space.
            assert_eq!(ext.len(), 5);
            for (e, &i) in ext.iter().zip(&m.extended) {
                let a = &m.arcs[i as usize];
                assert_eq!((e.a0, e.a1), (a.a0, a.a1));
                assert_eq!(e.r0, geo.bound(i32::from(a.depth)));
                assert_eq!(e.r1, geo.bound(i32::from(m.depth())));
                assert_eq!(extension_outer(&m, &geo, i as usize), Some(e.r1));
                if a.flags & flags::FREE != 0 {
                    // Free space, no hatch: light, its own near-transparent
                    // fill; dark, an opaque fill ~12 levels over the
                    // background (not a void).
                    assert_eq!(e.fill, Fill::Solid);
                    if dark {
                        assert_eq!(e.color.a(), 255);
                        for (c, b) in [
                            (e.color.r(), pal.bg.r()),
                            (e.color.g(), pal.bg.g()),
                            (e.color.b(), pal.bg.b()),
                        ] {
                            assert!((10..=14).contains(&(i32::from(c) - i32::from(b))));
                        }
                    } else {
                        assert_eq!(e.color, colors::free_colors(dark).0);
                    }
                } else {
                    // Unaccounted: a faint opaque tint, hatched a bit darker
                    // (light) or lighter (dark), both between the arc's
                    // color and the background.
                    let Fill::Hatched(lines) = e.fill else {
                        panic!("unaccounted extension is hatched")
                    };
                    let lum = colors::luminance;
                    let (u, bg) = (colors::unaccounted_color(dark), pal.bg);
                    assert_eq!(e.color.a(), 255);
                    let between = |c: Color32| {
                        let (lo, hi) = (lum(u).min(lum(bg)), lum(u).max(lum(bg)));
                        lum(c) > lo && lum(c) < hi
                    };
                    assert!(between(e.color) && between(lines));
                    assert!((lum(e.color) - lum(bg)).abs() < (lum(lines) - lum(bg)).abs());
                }
            }
            // Every other arc has none.
            assert_eq!(extension_outer(&m, &geo, 0), None);
        }
    }

    #[test]
    fn transition_fades_from_the_outer_folder_colors() {
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 800.0));
        let (outer, inner, a) = this_pc_models(false);
        let (go, gi) = (
            Geometry::new(rect, outer.depth()),
            Geometry::new(rect, inner.depth()),
        );
        let idx = outer.index_of(a.node().unwrap()).unwrap();
        let top_mid = outer.arcs[outer.top_of(idx)].mid();
        let (outer, inner) = (Arc::new(outer), Arc::new(inner));
        let anchor = (a.a0, a.a1, a.depth);
        let now = f64::from(ZOOM_SECS) * 0.4;
        let colors_at = |t: &Transition, pal: &Pal| -> Vec<Color32> {
            let arcs = t.arcs(now, &go, &gi, pal);
            arcs[arcs.len() - inner.arcs.len()..]
                .iter()
                .map(|d| d.color)
                .collect()
        };
        let close = |x: Color32, y: Color32| {
            x.to_array()
                .iter()
                .zip(y.to_array())
                .all(|(&p, q)| p.abs_diff(q) <= 1)
        };
        let t = Transition::new(
            Arc::clone(&outer),
            Arc::clone(&inner),
            anchor,
            Some(top_mid),
            true,
            0.0,
        );
        let p = t.zoom(now);
        assert!(p > 0.05 && p < 0.95);
        let w = a.a1 - a.a0;
        // Light (the cached colors), then dark (the theme switched
        // mid-animation: computed afresh), then light again (cache).
        for dark in [false, true, false] {
            let pal = crate::theme::pal(dark);
            let got = colors_at(&t, &pal);
            let mut faded = 0;
            for (b, c) in inner.arcs.iter().zip(&got) {
                let own = arc_color(b, pal.bg);
                let plain = b.is_rest() || b.flags & (flags::FREE | flags::UNACCOUNTED) != 0;
                let want = if plain {
                    own
                } else {
                    faded += 1;
                    let mid = a.a0 + b.mid() / TAU * w;
                    let from = colors::folder_color(
                        colors::folder_hue_at(top_mid, mid),
                        b.depth + a.depth,
                        b.flags & flags::DIR == 0,
                        dark,
                    );
                    colors::mix(from, own, p)
                };
                assert!(close(*c, want), "{c:?} vs {want:?}");
            }
            assert!(faded > 100);
        }
        // Outside Folders mode the inner arcs keep their own colors.
        let plain = Transition::new(outer, Arc::clone(&inner), anchor, None, true, 0.0);
        let pal = crate::theme::pal(false);
        let got = colors_at(&plain, &pal);
        for (b, c) in inner.arcs.iter().zip(&got) {
            assert_eq!(*c, arc_color(b, pal.bg));
        }
    }

    #[test]
    fn a_name_too_long_for_its_ring_goes_whole_into_the_extension() {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.painter().clone();
            let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 800.0));
            let geo = Geometry::new(rect, 6);
            let font = FontId::proportional(11.5);
            let name = "Free space on this drive";
            let (a0, a1) = (0.9, 1.5);
            let own: LabelSlot = ((a0, a1, geo.bound(1), geo.bound(2)), Color32::GRAY);
            let ext: LabelSlot = ((a0, a1, geo.bound(2), geo.bound(6)), Color32::GRAY);
            let mid = (a0 + a1) * 0.5;
            let text = (name, &font);
            // The own ring only takes a shortened name.
            assert!(place_label(&painter, &geo, text, own, mid, false).is_none());
            let short = place_label(&painter, &geo, text, own, mid, true).expect("elided");
            assert!(short.1.text().ends_with('…'));
            // With the extension: the whole name, placed in the extension.
            let (pos, galley, angle) =
                place_label_in(&painter, &geo, text, &[own, ext], mid).expect("placed");
            assert_eq!(galley.text(), name);
            let (s, c) = angle.sin_cos();
            let (hw, hh) = (galley.size().x * 0.5, galley.size().y * 0.5);
            let center = Pos2::new(pos.x + hw * c - hh * s, pos.y + hw * s + hh * c);
            let (r, _) = geo.polar(center);
            assert!(r > geo.bound(2) && r < geo.bound(6), "r {r}");
            // A name that fits its ring stays there.
            let (pos, galley, _) =
                place_label_in(&painter, &geo, ("C:", &font), &[own, ext], mid).expect("placed");
            assert_eq!(galley.text(), "C:");
            let (r, _) = geo.polar(pos + galley.size() * 0.5);
            assert!(r < geo.bound(2) + 4.0, "r {r}");
            // Neither fits the full name: shortened in the own ring first.
            let narrow: LabelSlot = ((a0, a0 + 0.12, geo.bound(2), geo.bound(3)), Color32::GRAY);
            let (_, galley, _) =
                place_label_in(&painter, &geo, text, &[own, narrow], mid).expect("placed");
            assert_eq!(galley.text(), short.1.text());
        });
        // Nothing renders: drop the texture uploads.
        out.textures_delta.clear();
    }

    #[test]
    fn legend_explains_the_hatch_while_one_is_shown() {
        let tree = synth::this_pc(9, 20_000);
        for dark in [false, true] {
            let pal = crate::theme::pal(dark);
            let k = ModelKey {
                dark,
                ..key(0, tree.generation())
            };
            let m = ChartModel::build(&tree, k, None, &NAMES);
            let h = LegendHatch::of(&m, &pal, "Unaccounted / reserved").expect("row");
            assert_eq!(h.label, "Unaccounted / reserved");
            assert_eq!(h.arc, colors::unaccounted_color(dark));
            // The swatch uses the colors of the (unmarked) extensions.
            let i = m
                .extended
                .iter()
                .find(|&&i| m.arcs[i as usize].flags & flags::UNACCOUNTED != 0)
                .unwrap();
            let (fill, style) = extension_style(&m.arcs[*i as usize], pal.bg, dark);
            assert_eq!((h.fill, style), (fill, Fill::Hatched(h.lines)));
        }
        // No unaccounted extension (a folder focus): no row.
        let (_, inner, _) = this_pc_models(false);
        assert!(LegendHatch::of(&inner, &crate::theme::pal(false), "U").is_none());
    }

    #[test]
    fn transition_extensions_match_static_ones() {
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 800.0));
        let pal = crate::theme::pal(false);
        // Into drive C: (its unaccounted and free arcs go from ring 2 to
        // ring 1), then into a deep folder (no virtual leaves visible).
        for focus_drive in [true, false] {
            for inward in [true, false] {
                let (outer, inner, a) = this_pc_models(focus_drive);
                let (go, gi) = (
                    Geometry::new(rect, outer.depth()),
                    Geometry::new(rect, inner.depth()),
                );
                let (outer, inner) = (Arc::new(outer), Arc::new(inner));
                let t = Transition::new(
                    Arc::clone(&outer),
                    Arc::clone(&inner),
                    (a.a0, a.a1, a.depth),
                    None,
                    inward,
                    0.0,
                );
                let (at_outer, at_inner) = if inward { (0.0, 10.0) } else { (10.0, 0.0) };
                // Inner view: the inner model's extensions where its static
                // chart has them; the outer ones squeezed to nothing.
                let end = t.extensions(at_inner, &go, &gi, &pal);
                let n_outer = end.len() - inner.extended.len();
                let inner_static = static_extensions(&inner, &gi, &pal);
                assert_eq!(end[n_outer..].len(), inner_static.len());
                if !focus_drive {
                    assert!(inner_static.is_empty());
                }
                for (x, y) in end[n_outer..].iter().zip(&inner_static) {
                    assert!((x.a0 - y.a0).abs() < 1e-4 && (x.a1 - y.a1).abs() < 1e-4);
                    assert!((x.r0 - y.r0).abs() < 1e-3 && (x.r1 - y.r1).abs() < 1e-3);
                }
                for x in &end[..n_outer] {
                    assert!(x.a1 - x.a0 < 1e-4, "outer extension still visible");
                }
                // Outer view: the drawn extensions equal the static outer
                // chart's (C:'s are drawn from the inner model, D:'s from
                // the outer one; both at the same place).
                let start = t.extensions(at_outer, &go, &gi, &pal);
                let outer_static = static_extensions(&outer, &go, &pal);
                let edge = go.bound(i32::from(outer.depth()));
                for x in &start {
                    assert!((x.r1 - edge).abs() < 1e-3);
                    let same = outer_static.iter().any(|y| {
                        (x.a0 - y.a0).abs() < 1e-3
                            && (x.a1 - y.a1).abs() < 1e-3
                            && (x.r0 - y.r0).abs() < 1e-3
                    });
                    // A leaf the outer model could not show (deeper than
                    // its rings) starts as a zero-thickness band.
                    assert!(same || x.r0 >= edge - 1e-3, "{x:?}");
                }
                for y in &outer_static {
                    assert!(
                        start
                            .iter()
                            .any(|x| (x.a0 - y.a0).abs() < 1e-3 && (x.a1 - y.a1).abs() < 1e-3)
                    );
                }
                // Midway the outer edge moves smoothly between the two.
                let mid = t.extensions(ZOOM_SECS as f64 * 0.5, &go, &gi, &pal);
                let gi_edge = gi.bound(i32::from(inner.depth()));
                for x in &mid {
                    let (lo, hi) = (edge.min(gi_edge), edge.max(gi_edge));
                    assert!(x.r1 >= lo - 1e-3 && x.r1 <= hi + 1e-3);
                }
            }
        }
    }

    #[test]
    fn rect_fits_checks_the_inner_circle_between_corners() {
        let g = Geometry {
            center: Pos2::ZERO,
            bounds: vec![100.0, 140.0],
        };
        let sector = (0.0, TAU, 100.0, 140.0);
        // A 120×10 box at 12 o'clock, tilted 0.3 rad: its corners and
        // edge midpoints clear r = 100, but its inner edge dips below it
        // between them.
        let rot = 0.3f32;
        let c = g.point(109.0, 0.0);
        let (s, co) = rot.sin_cos();
        let pt = |x: f32, y: f32| Pos2::new(c.x + x * co - y * s, c.y + x * s + y * co);
        let probes = [
            (-60.0, -5.0),
            (60.0, -5.0),
            (60.0, 5.0),
            (-60.0, 5.0),
            (0.0, -5.0),
            (0.0, 5.0),
            (-60.0, 0.0),
            (60.0, 0.0),
        ];
        assert!(probes.iter().all(|&(x, y)| g.polar(pt(x, y)).0 > 102.0));
        assert!(dist_to_segment(g.center, pt(-60.0, 5.0), pt(60.0, 5.0)) < 99.5);
        assert!(!rect_fits(&g, c, 120.0, 10.0, rot, sector, 0.0));
        // Upright and tangential it fits.
        assert!(rect_fits(&g, c, 60.0, 10.0, 0.0, sector, 0.0));
    }

    #[test]
    fn a_name_too_wide_for_the_middle_of_a_curved_ring_moves_inward() {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.painter().clone();
            let font = FontId::proportional(11.5);
            let name = "Update_data_node";
            let size = painter
                .layout_no_wrap(name.into(), font.clone(), Color32::BLACK)
                .size();
            let (w, h) = (size.x, size.y);
            // A ring like ring 2 of a 1280×640 window (24 pt at r = 80 for
            // a 95-pt name in Segoe UI), sized for this font: just thick
            // enough for the line moved inward.
            let r0 = 80.0;
            let r1 = ((r0 + LABEL_MARGIN + h + 0.5).powi(2) + (w * 0.5).powi(2)).sqrt()
                + LABEL_MARGIN
                + 0.5;
            let geo = Geometry {
                center: Pos2::new(300.0, 300.0),
                bounds: vec![56.0, r0, r1],
            };
            // Centered in the ring, the corners of the line poke out.
            let mid = 0.0;
            let sector = (-1.5, 1.5, r0, r1);
            let rm = (r0 + r1) * 0.5;
            assert!(!rect_fits(
                &geo,
                geo.point(rm, mid),
                w,
                h,
                0.0,
                sector,
                LABEL_MARGIN
            ));
            let slot: LabelSlot = (sector, Color32::GRAY);
            let (pos, galley, angle) =
                place_label(&painter, &geo, (name, &font), slot, mid, false).expect("placed whole");
            assert_eq!(galley.text(), name);
            // Its center is below the middle of the ring, and the whole
            // box is inside the sector.
            let (sn, cs) = angle.sin_cos();
            let (hw, hh) = (galley.size().x * 0.5, galley.size().y * 0.5);
            let c = Pos2::new(pos.x + hw * cs - hh * sn, pos.y + hw * sn + hh * cs);
            assert!(geo.polar(c).0 < rm - 1.0);
            let size = galley.size();
            assert!(rect_fits(
                &geo,
                c,
                size.x,
                size.y,
                angle,
                sector,
                LABEL_MARGIN
            ));
        });
        out.textures_delta.clear();
    }

    #[test]
    fn a_name_that_fits_the_middle_of_its_ring_stays_there() {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.painter().clone();
            let font = FontId::proportional(11.5);
            let geo = Geometry {
                center: Pos2::new(300.0, 300.0),
                bounds: vec![56.0, 80.0, 124.0],
            };
            let rm = 102.0;
            let center = |(pos, galley, angle): (Pos2, Arc<egui::Galley>, f32)| {
                let (sn, cs) = angle.sin_cos();
                let (hw, hh) = (galley.size().x * 0.5, galley.size().y * 0.5);
                (
                    Pos2::new(pos.x + hw * cs - hh * sn, pos.y + hw * sn + hh * cs),
                    angle,
                )
            };
            // A short name anywhere around a roomy ring: centered, in the
            // first orientation that fits there.
            for k in 0..16 {
                let mid = TAU * k as f32 / 16.0 + 0.05;
                let slot: LabelSlot = ((mid - 0.6, mid + 0.6, 80.0, 124.0), Color32::GRAY);
                let placed = place_label(&painter, &geo, ("web", &font), slot, mid, false);
                let (c, _) = center(placed.expect("placed"));
                assert!((geo.polar(c).0 - rm).abs() < 0.1, "at {mid}");
            }
        });
        out.textures_delta.clear();
    }

    #[test]
    fn label_fit_and_elide() {
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 600.0));
        let g = Geometry::new(rect, 6);
        let (r0, r1) = (g.bound(0), g.bound(1));
        let sector = (0.0, 1.0, r0, r1);
        let c = g.point((r0 + r1) / 2.0, 0.5);
        assert!(rect_fits(&g, c, 20.0, 10.0, 0.5, sector, 2.0));
        assert!(!rect_fits(&g, c, 400.0, 10.0, 0.5, sector, 2.0));
        assert!(!rect_fits(&g, c, 20.0, r1 - r0 + 5.0, 0.5, sector, 2.0));
        assert_eq!(elide("folder", 7.0, 100.0).as_deref(), Some("folder"));
        assert_eq!(
            elide("a-very-long-name", 7.0, 50.0).as_deref(),
            Some("a-very…")
        );
        assert_eq!(elide("abcdef", 7.0, 20.0), None);
    }
}
