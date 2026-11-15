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
//! [`MIN_GEOMETRY_RINGS`]), so a shallow folder is not drawn as a thin
//! band around a huge empty disc.
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
//! animation backwards. Labels are hidden while the animation runs.

use std::f32::consts::TAU;
use std::sync::Arc;

use eframe::egui::{
    self, Align2, Color32, FontId, Mesh, Pos2, Rect, Shape, Stroke, epaint::TextShape,
};
use ringscan_core::flags;

use crate::colors;
use crate::model::{ArcInfo, ChartModel, Mark};
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
    pub fn new(rect: Rect, rings: u8) -> Geometry {
        let rings = usize::from(rings.max(MIN_GEOMETRY_RINGS));
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

/// How an arc is filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    Solid,
    /// Two-tone stripes along the arc ("N smaller items").
    Stripes(Color32),
    /// Nearly transparent fill plus an outline (free space).
    Outlined(Color32),
}

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
    match a.mark {
        Mark::None | Mark::Match => a.color,
        Mark::Contains => colors::mix(a.color, bg, 0.45),
        Mark::Dim => colors::mix(a.color, bg, 0.8),
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

/// Screen arcs of a model at rest.
pub fn static_arcs(model: &ChartModel, geo: &Geometry, pal: &Pal) -> Vec<DrawArc> {
    model
        .arcs
        .iter()
        .map(|a| {
            let d = i32::from(a.depth);
            DrawArc {
                a0: a.a0,
                a1: a.a1,
                r0: geo.bound(d - 1),
                r1: geo.bound(d),
                color: arc_color(a, pal.bg),
                fill: arc_fill(a, pal.dark),
            }
        })
        .collect()
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

impl Transition {
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
        let p = self.zoom(now);
        let (a0, a1, da) = self.anchor;
        let da = i32::from(da);
        let w = (a1 - a0).max(1e-6);
        let ang = |a: f32| -> f32 {
            let z = ((a - a0) / w * TAU).clamp(0.0, TAU);
            a + (z - a) * p
        };
        let rad = |k: i32| -> f32 {
            let ro = geo_outer.bound(k);
            let ri = geo_inner.bound(k - da);
            ro + (ri - ro) * p
        };
        let mut out = Vec::with_capacity(self.outer.arcs.len() + self.inner.arcs.len());
        let eps = 1e-4;
        for a in &self.outer.arcs {
            let d = i32::from(a.depth);
            // The anchor's subtree is drawn from the inner model.
            if d > da && a.a0 >= a0 - eps && a.a1 <= a1 + eps {
                continue;
            }
            out.push(DrawArc {
                a0: ang(a.a0),
                a1: ang(a.a1),
                r0: rad(d - 1),
                r1: rad(d),
                color: arc_color(a, pal.bg),
                fill: arc_fill(a, pal.dark),
            });
        }
        for a in &self.inner.arcs {
            let d = i32::from(a.depth) + da;
            let oa0 = a0 + a.a0 / TAU * w;
            let oa1 = a0 + a.a1 / TAU * w;
            let mut color = arc_color(a, pal.bg);
            if let Some(top_mid) = self.anchor_top_mid
                && a.mark == Mark::None
                && !a.is_rest()
                && a.flags & (flags::FREE | flags::UNACCOUNTED) == 0
            {
                // The color this arc had in the outer chart.
                let hue = colors::folder_hue_at(top_mid, (oa0 + oa1) * 0.5);
                let from = colors::folder_color(
                    hue,
                    d.clamp(1, 255) as u8,
                    a.flags & flags::DIR == 0,
                    pal.dark,
                );
                color = colors::mix(from, color, p);
            }
            out.push(DrawArc {
                a0: ang(oa0),
                a1: ang(oa1),
                r0: rad(d - 1),
                r1: rad(d),
                color,
                fill: arc_fill(a, pal.dark),
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

/// Is the rectangle `w`×`h` centered at `c`, rotated by `rot`, fully inside
/// the annular sector (with `margin` points to spare)?
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
    for (x, y) in probes {
        let p = Pos2::new(c.x + x * co - y * s, c.y + x * s + y * co);
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
/// `max_labels`.
pub fn layout_labels(
    painter: &egui::Painter,
    model: &ChartModel,
    geo: &Geometry,
    bg: Color32,
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
    for i in cands {
        if out.len() >= max_labels {
            break;
        }
        let a = &model.arcs[i];
        let d = i32::from(a.depth);
        let (r0, r1) = (geo.bound(d - 1), geo.bound(d));
        let thick = r1 - r0;
        let span = a.a1 - a.a0;
        let rm = (r0 + r1) * 0.5;
        let size = if a.depth <= 1 { 12.5 } else { 11.5 };
        let font = FontId::proportional(size);
        let line_h = size * 1.3;
        if thick < line_h * 0.9 && span * r1 < line_h * 0.9 {
            continue;
        }
        let name = a.label.as_deref().unwrap_or("");
        let sector = (a.a0, a.a1, r0, r1);
        let mid = a.mid();
        // Translucent fills (free space) are read against the background.
        let shown = colors::over(arc_color(a, bg), bg);
        let text_color = colors::text_on(shown);
        let full = painter.layout_no_wrap(name.to_string(), font.clone(), text_color);
        let (fw, fh) = (full.size().x, full.size().y);
        let char_w = fw / name.chars().count().max(1) as f32;
        let c = geo.point(rm, mid);
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
        let mut placed: Option<(Arc<egui::Galley>, f32)> = None;
        // 1. Full name, horizontal / tangential / radial.
        for rot in [0.0, tangential, radial] {
            if rect_fits(geo, c, fw, fh, rot, sector, 2.0) {
                placed = Some((full.clone(), rot));
                break;
            }
        }
        // 2. Shortened, in the orientation with the most room.
        if placed.is_none() {
            let along_tangent = span * r0 - 6.0;
            let along_radius = thick - 6.0;
            let (rot, room) = if along_tangent >= along_radius {
                (tangential, along_tangent)
            } else {
                (radial, along_radius)
            };
            if let Some(short) = elide(name, char_w, room) {
                let g = painter.layout_no_wrap(short, font.clone(), text_color);
                if rect_fits(geo, c, g.size().x, g.size().y, rot, sector, 2.0) {
                    placed = Some((g, rot));
                }
            }
        }
        if let Some((galley, rot)) = placed {
            // TextShape rotates around its top-left corner: offset so the
            // text is centered on `c`.
            let (s, co) = rot.sin_cos();
            let (hw, hh) = (galley.size().x * 0.5, galley.size().y * 0.5);
            let pos = Pos2::new(c.x - hw * co + hh * s, c.y - hw * s - hh * co);
            out.push(Label {
                pos,
                galley,
                angle: rot,
            });
        }
    }
    out
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

/// Draws a highlight over arc `a` (hover / selection / list sync).
pub fn highlight_shapes(geo: &Geometry, a: &ArcInfo, pal: &Pal, strong: bool) -> Vec<Shape> {
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
        Fill::Solid => lighter,
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
    let stroke_color = if strong { pal.accent } else { pal.text };
    let width = if strong { 2.0 } else { 1.25 };
    let pts = outline_points(geo.center, a.a0, a.a1, r0 + 0.5, r1 - 0.5);
    vec![
        Shape::mesh(mesh),
        Shape::closed_line(pts, Stroke::new(width, stroke_color)),
    ]
}

/// Paints the center disc: focus name, size and file count.
#[allow(clippy::too_many_arguments)]
pub fn paint_center(
    painter: &egui::Painter,
    geo: &Geometry,
    pal: &Pal,
    name: &str,
    size_text: &str,
    sub_text: &str,
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
    let g_sub = fit(
        sub_text,
        FontId::proportional(small),
        pal.text_dim.gamma_multiply(alpha),
    );
    let total_h = g_name.size().y + g_size.size().y + g_sub.size().y;
    let mut y = c.y - total_h * 0.5;
    for g in [g_name, g_size, g_sub] {
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
pub fn paint_legend(
    painter: &egui::Painter,
    anchor: Pos2,
    pal: &Pal,
    rows: &[(Color32, String)],
    gradient: Option<&[Color32]>,
) {
    let font = FontId::proportional(12.0);
    let galleys: Vec<_> = rows
        .iter()
        .map(|(_, t)| painter.layout_no_wrap(t.clone(), font.clone(), pal.text_dim))
        .collect();
    let row_h = 18.0;
    let pad = 10.0;
    let text_w = galleys.iter().map(|g| g.size().x).fold(0.0, f32::max);
    let (w, h) = if gradient.is_some() {
        (
            pad * 2.0 + 14.0 + 8.0 + text_w,
            pad * 2.0 + row_h * (rows.len() as f32 - 1.0) + 4.0,
        )
    } else {
        (
            pad * 2.0 + 12.0 + 8.0 + text_w,
            pad * 2.0 + row_h * rows.len() as f32 - 4.0,
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
        let tree = synth::tree(3, 4000);
        let outer = ChartModel::build(&tree, key(0, tree.generation()), None, &NAMES);
        // Zoom into the widest zoomable ring-1 arc.
        let a = outer
            .arcs
            .iter()
            .filter(|a| a.depth == 1 && a.zoomable)
            .max_by(|x, y| x.span().total_cmp(&y.span()))
            .expect("a zoomable arc")
            .clone();
        let target = a.node().unwrap();
        let inner = ChartModel::build(&tree, key(target, tree.generation()), None, &NAMES);
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 800.0));
        let (go, gi) = (
            Geometry::new(rect, outer.depth()),
            Geometry::new(rect, inner.depth()),
        );
        let pal = crate::theme::pal(false);
        let t = Transition {
            outer: Arc::new(outer),
            inner: Arc::new(inner),
            anchor: (a.a0, a.a1, a.depth),
            anchor_top_mid: None,
            inward: true,
            start: 0.0,
        };
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
        }
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
