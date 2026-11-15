//! The chart area: mesh caching, the zoom animation, hit testing, clicks,
//! the context menu, the center disc, legends and the tooltip.

use std::sync::Arc;
use std::time::Instant;

use eframe::egui::{
    self, Align2, Color32, Context, Frame, Id, Pos2, RichText, Sense, Stroke, Ui, pos2, vec2,
};
use ringscan_core::flags;

use crate::actions::{self, Action, MenuTarget};
use crate::chart::{self, Geometry};
use crate::colors::{self, Group};
use crate::details::{self};
use crate::i18n::Tr;
use crate::model::{ChartModel, Mark};
use crate::settings::ColorMode;
use crate::theme::{self, Pal};

use super::{App, virtual_names};

/// Cached chart mesh for a model at rest.
pub(super) struct MeshCache {
    pub(super) serial: u64,
    pub(super) geo: Geometry,
    pub(super) dark: bool,
    pub(super) mesh: Arc<egui::Mesh>,
    pub(super) extra: Vec<egui::Shape>,
}

/// What the pointer is over in the chart.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Hit {
    Center,
    Arc(usize),
}

impl App {
    // ----- chart --------------------------------------------------------

    pub(super) fn chart_ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let pal = theme::current(&ctx);
        let tr = self.tr();
        let rect = ui.max_rect();
        self.chart_rect = rect;
        let resp = ui.interact(rect, Id::unique("chart"), Sense::click());
        let painter = ui.painter_at(rect);
        let now = self.now();
        let time = ctx.input(|i| i.time);
        if self.scan.is_none() {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                tr.s.nothing_scanned,
                egui::FontId::proportional(15.0),
                pal.text_dim,
            );
            return;
        }
        if self.transition.as_ref().is_some_and(|t| t.done(now)) {
            self.transition = None;
            self.transition_end = now;
        }
        // The automatic ring count follows this rectangle: re-layout now if
        // it changed the count, so the new count shows in this frame. The
        // check is memoized and takes no lock; `update` already checked
        // the model's other inputs this frame.
        if self.wanted_rings() != self.model.key.params.max_depth {
            self.ensure_model(&ctx, false);
        }
        let model = Arc::clone(&self.model);
        let geo = Geometry::of(rect, &model);

        // Hit test (not while animating).
        let hit = if self.transition.is_none() {
            resp.hover_pos().and_then(|p| {
                let (r, ang) = geo.polar(p);
                match geo.ring_at(r)? {
                    0 => (model.center.display > 0 || model.center.is_root).then_some(Hit::Center),
                    // Extensions of virtual leaves answer for their arc.
                    d => model.hit_any(ang, d).map(Hit::Arc),
                }
            })
        } else {
            None
        };
        self.chart_hit = hit;
        self.chart_hover = match hit {
            Some(Hit::Arc(i)) => model.arcs[i].node(),
            _ => None,
        };
        if hit.is_some() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        // Arcs.
        self.drawn_serial = model.serial;
        if let Some(t) = &self.transition {
            let geo_o = Geometry::of(rect, &t.outer);
            let geo_i = Geometry::of(rect, &t.inner);
            let arcs = t.draw_arcs(now, &geo_o, &geo_i, &pal);
            // mesh_ms is the mesh build alone (the arc mapping is not timed).
            let started = Instant::now();
            let (mesh, extra) = chart::build_mesh(&arcs, geo.center, 1.0);
            self.mesh_ms = started.elapsed().as_secs_f32() * 1000.0;
            self.frames.mesh_ms.push(self.mesh_ms);
            self.anim_vertices = mesh.vertices.len();
            painter.add(egui::Shape::mesh(mesh));
            painter.extend(extra);
            ctx.request_repaint();
        } else {
            let dark = pal.dark;
            let fresh = self
                .mesh
                .as_ref()
                .is_some_and(|c| c.serial == model.serial && c.geo == geo && c.dark == dark);
            if !fresh {
                let started = Instant::now();
                let (mesh, extra) = chart::static_mesh(&model, &geo, &pal);
                self.mesh_ms = started.elapsed().as_secs_f32() * 1000.0;
                self.frames.mesh_ms.push(self.mesh_ms);
                self.mesh = Some(MeshCache {
                    serial: model.serial,
                    geo: geo.clone(),
                    dark,
                    mesh: Arc::new(mesh),
                    extra,
                });
            }
            if let Some(c) = &self.mesh {
                painter.add(egui::Shape::Mesh(Arc::clone(&c.mesh)));
                painter.extend(c.extra.iter().cloned());
            }
            // Search matches get an outline.
            if self.highlight().is_some() {
                for a in model.arcs.iter().filter(|a| a.mark == Mark::Match) {
                    let d = i32::from(a.depth);
                    let pts = chart::outline_points(
                        geo.center,
                        a.a0,
                        a.a1,
                        geo.bound(d - 1) + 0.5,
                        geo.bound(d) - 0.5,
                    );
                    painter.add(egui::Shape::closed_line(pts, Stroke::new(1.5, pal.text)));
                }
            }
            // Selection, list hover and chart hover highlights, then the
            // labels on top so a highlighted arc keeps its name.
            let ext = |i: usize| chart::extension_outer(&model, &geo, i);
            if let Some(i) = self.selected.and_then(|s| model.index_of(s)) {
                painter.extend(chart::highlight_shapes(
                    &geo,
                    &model.arcs[i],
                    &pal,
                    true,
                    ext(i),
                ));
            }
            let hover_idx = match hit {
                Some(Hit::Arc(i)) => Some(i),
                _ => self.details.hovered.and_then(|id| model.index_of(id)),
            };
            if let Some(i) = hover_idx
                && Some(i) != self.selected.and_then(|s| model.index_of(s))
            {
                painter.extend(chart::highlight_shapes(
                    &geo,
                    &model.arcs[i],
                    &pal,
                    false,
                    ext(i),
                ));
            }
            // Labels fade in after a zoom.
            let since = (now - self.transition_end).max(0.0);
            let alpha = (since / 0.18).clamp(0.0, 1.0) as f32;
            let labels = chart::layout_labels(&painter, &model, &geo, (pal.bg, pal.dark), 240);
            self.label_count = labels.len();
            painter.extend(chart::label_shapes(&labels, alpha));
            if alpha < 1.0 {
                ctx.request_repaint();
            }
        }

        // Center disc.
        let (center_model, alpha) = match &self.transition {
            Some(t) => {
                let p = t.progress(now);
                let target = if t.inward { &t.inner } else { &t.outer };
                (Arc::clone(target), ((p - 0.35) / 0.65).clamp(0.0, 1.0))
            }
            None => (Arc::clone(&model), 1.0),
        };
        let c = &center_model.center;
        let sub = if c.flags & flags::DIR != 0 && c.files + c.dirs > 0 {
            tr.n_files(c.files)
        } else if model.arcs.is_empty() && !self.status.running {
            tr.s.empty.to_string()
        } else {
            String::new()
        };
        // Free space as text: the chart may not show it (the default).
        let free = if c.free > 0 {
            tr.free(c.free)
        } else {
            String::new()
        };
        let center_hovered = hit == Some(Hit::Center);
        chart::paint_center(
            &painter,
            &geo,
            &pal,
            &c.name,
            &tr.size(c.size),
            &[sub.as_str(), free.as_str()],
            center_hovered,
            alpha,
            !c.is_root,
        );
        if self.status.running {
            // Progress ring just inside the disc edge.
            let r = geo.r0() - 5.0;
            let start = (time * 2.2) as f32;
            let pts: Vec<Pos2> = (0..=40)
                .map(|k| geo.point(r, start + 1.4 * k as f32 / 40.0))
                .collect();
            painter.add(egui::Shape::line(pts, Stroke::new(2.5, pal.accent)));
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }

        // Legend.
        let legend_anchor = pos2(rect.left() + 14.0, rect.bottom() - 14.0);
        // Explains the hatched wedges while one is on the chart (Folders
        // mode has no legend: there the tooltip does).
        let hatch = (self.settings.color_mode != ColorMode::Folders)
            .then(|| chart::LegendHatch::of(&model, &pal, tr.s.legend_unaccounted))
            .flatten();
        match self.settings.color_mode {
            ColorMode::Types => {
                let rows: Vec<(Color32, String)> = Group::ALL
                    .iter()
                    .map(|&g| (g.color(pal.dark), details::group_name(tr, g).to_string()))
                    .collect();
                chart::paint_legend(&painter, legend_anchor, &pal, &rows, None, hatch.as_ref());
            }
            ColorMode::Age => {
                let s = tr.s;
                let ticks = [
                    (0.0, s.age_day),
                    (7.0, s.age_week),
                    (30.0, s.age_month),
                    (365.0, s.age_year),
                    (1096.0, s.age_3y),
                    (colors::AGE_MAX_DAYS, s.age_10y),
                ];
                let rows: Vec<(Color32, String)> = ticks
                    .iter()
                    .map(|&(d, l)| (colors::age_color(colors::age_t(d), pal.dark), l.to_string()))
                    .collect();
                let stops: Vec<Color32> = rows.iter().map(|r| r.0).collect();
                chart::paint_legend(
                    &painter,
                    legend_anchor,
                    &pal,
                    &rows,
                    Some(&stops),
                    hatch.as_ref(),
                );
            }
            ColorMode::Folders => {}
        }

        // Clicks.
        if resp.clicked() && !self.popup_was_open {
            match hit {
                Some(Hit::Center) => self.actions.push(Action::Up),
                Some(Hit::Arc(i)) => {
                    let a = &model.arcs[i];
                    match a.kind {
                        ringscan_core::layout::ArcKind::Node(id) if a.zoomable => {
                            self.actions.push(Action::Zoom(id))
                        }
                        ringscan_core::layout::ArcKind::Node(id) => {
                            self.actions.push(Action::Select(Some(id)))
                        }
                        ringscan_core::layout::ArcKind::Rest { parent, .. }
                            if parent != model.focus() =>
                        {
                            self.actions.push(Action::Zoom(parent))
                        }
                        _ => {}
                    }
                }
                None => self.actions.push(Action::Select(None)),
            }
        }
        if resp.middle_clicked() && !self.popup_was_open {
            self.actions.push(Action::Back);
        }
        if resp.secondary_clicked() {
            self.context_target = match hit {
                Some(Hit::Arc(i)) => model.arcs[i].node().map(|id| {
                    let a = &model.arcs[i];
                    MenuTarget {
                        id,
                        zoomable: a.zoomable,
                        is_dir: a.flags & flags::DIR != 0,
                        is_virtual: a.flags & flags::VIRTUAL != 0,
                        is_focus: false,
                    }
                }),
                Some(Hit::Center) => Some(MenuTarget {
                    id: model.focus(),
                    zoomable: false,
                    is_dir: model.center.flags & flags::DIR != 0,
                    is_virtual: model.center.flags & flags::VIRTUAL != 0,
                    is_focus: true,
                }),
                None => None,
            };
            if let Some(t) = &self.context_target {
                self.selected = Some(t.id);
            }
        }
        if let Some(a) =
            actions::attach_menu(&resp, tr, self.context_target.as_ref(), self.menu_caps())
        {
            self.actions.push(a);
        }

        // Tooltip.
        if let (Some(h), Some(pos)) = (hit, resp.hover_pos())
            && !egui::Popup::is_any_open(&ctx)
            && !self.modal_open()
        {
            self.tooltip(&ctx, pos, h, &model, &pal, tr);
        }
    }

    pub(super) fn tooltip(
        &self,
        ctx: &Context,
        pos: Pos2,
        hit: Hit,
        model: &ChartModel,
        pal: &Pal,
        tr: Tr,
    ) {
        let Some(scan) = &self.scan else { return };
        let s = tr.s;
        let mode: ringscan_core::SizeMode = self.settings.size_mode.into();
        struct Info {
            title: String,
            glyph: &'static str,
            size: u64,
            lines: Vec<String>,
            notes: Vec<String>,
            path: Option<String>,
        }
        let info = {
            let t = scan.tree.read_recursive();
            match hit {
                Hit::Center => {
                    let id = model.focus();
                    if !t.is_live(id) {
                        return;
                    }
                    let mut lines = Vec::new();
                    if t.node(id).is_dir() {
                        lines.push(format!(
                            "{} · {}",
                            tr.n_files(t.files(id)),
                            tr.n_folders(t.dirs(id))
                        ));
                    }
                    if model.center.free > 0 {
                        lines.push(format!("{}: {}", s.free_space, tr.size(model.center.free)));
                    }
                    if !model.center.is_root {
                        lines.push(s.click_up_tip.to_string());
                    }
                    Info {
                        title: model.center.name.clone(),
                        glyph: theme::icon::FOLDER,
                        size: model.center.size,
                        lines,
                        notes: Vec::new(),
                        path: (!t.node(id).is_virtual()).then(|| t.path(id)),
                    }
                }
                Hit::Arc(i) => {
                    let a = &model.arcs[i];
                    let parent_size = if a.parent == crate::model::NO_PARENT {
                        model.center.display
                    } else {
                        model.arcs[a.parent as usize].size
                    }
                    .max(1);
                    let focus_size = model.center.display.max(1);
                    // Used bytes (the arc of a drive also spans its free space).
                    let used = match a.kind {
                        ringscan_core::layout::ArcKind::Node(id) if t.is_live(id) => {
                            t.size(id, mode)
                        }
                        _ => a.size,
                    };
                    let share = format!(
                        "{} {} · {} {}",
                        tr.percent(used as f64 / parent_size as f64),
                        s.of_parent,
                        tr.percent(used as f64 / focus_size as f64),
                        s.of_focus
                    );
                    match a.kind {
                        ringscan_core::layout::ArcKind::Rest { count, .. } => Info {
                            title: tr.n_smaller_items(u64::from(count)),
                            glyph: theme::icon::PIE,
                            size: a.size,
                            lines: vec![share],
                            notes: Vec::new(),
                            path: None,
                        },
                        ringscan_core::layout::ArcKind::Node(id) => {
                            if !t.is_live(id) {
                                return;
                            }
                            let n = t.node(id);
                            let mut lines = vec![share];
                            // A drive (under "This PC"), whether its free
                            // space is drawn or not.
                            let free = if n.is_dir() {
                                crate::model::free_below(&t, id, mode)
                            } else {
                                0
                            };
                            if free > 0 {
                                lines.push(format!("{}: {}", s.free_space, tr.size(free)));
                            }
                            if n.is_dir() {
                                lines.push(format!(
                                    "{} · {}",
                                    tr.n_files(t.files(id)),
                                    tr.n_folders(t.dirs(id))
                                ));
                            }
                            if n.mtime() > 0 && !n.is_virtual() {
                                lines.push(format!(
                                    "{}: {}",
                                    s.modified,
                                    tr.date(self.local_time_fn()(n.mtime()))
                                ));
                            }
                            if mode == ringscan_core::SizeMode::Allocated
                                && !n.is_dir()
                                && n.logical() != n.alloc()
                                && !n.is_virtual()
                            {
                                lines.push(format!("{}: {}", s.logical, tr.size(n.logical())));
                            }
                            let mut notes = Vec::new();
                            if n.has(flags::UNACCOUNTED) {
                                notes.push(if t.name(id) == ringscan_engine::RESERVED_NAME {
                                    s.reserved_note.to_string()
                                } else {
                                    s.unaccounted_note.to_string()
                                });
                            }
                            if n.has(flags::REPARSE) {
                                notes.push(s.reparse_note.to_string());
                            }
                            if n.has(flags::ERROR) {
                                notes.push(s.error_note.to_string());
                            }
                            if n.has(flags::CLOUD) {
                                notes.push(s.cloud_note.to_string());
                            }
                            Info {
                                title: crate::model::display_name(&t, id, &virtual_names(tr)),
                                glyph: if n.is_virtual() {
                                    theme::icon::PIE
                                } else if n.is_dir() {
                                    theme::icon::FOLDER
                                } else {
                                    theme::icon::DOCUMENT
                                },
                                size: used,
                                lines,
                                notes,
                                path: (!n.is_virtual()).then(|| t.path(id)),
                            }
                        }
                    }
                }
            }
        };
        let screen = ctx.content_rect();
        let right = pos.x > screen.center().x + screen.width() * 0.15;
        let below = pos.y < screen.center().y + screen.height() * 0.2;
        let (pivot, offset) = match (right, below) {
            (false, true) => (Align2::LEFT_TOP, vec2(18.0, 20.0)),
            (true, true) => (Align2::RIGHT_TOP, vec2(-14.0, 20.0)),
            (false, false) => (Align2::LEFT_BOTTOM, vec2(18.0, -12.0)),
            (true, false) => (Align2::RIGHT_BOTTOM, vec2(-14.0, -12.0)),
        };
        egui::Area::new(Id::unique("chart-tooltip"))
            .order(egui::Order::Tooltip)
            .fixed_pos(pos + offset)
            .pivot(pivot)
            .interactable(false)
            .constrain(true)
            .show(ctx, |ui| {
                Frame::new()
                    .fill(pal.card)
                    .stroke(Stroke::new(1.0, pal.border))
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .shadow(egui::Shadow {
                        offset: [0, 4],
                        blur: 14,
                        spread: 0,
                        color: pal.shadow,
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(380.0);
                        ui.spacing_mut().item_spacing.y = 3.0;
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(info.glyph)
                                    .font(theme::icon_font(14.0))
                                    .color(pal.text_dim),
                            );
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&info.title)
                                        .font(theme::semibold(14.0))
                                        .color(pal.text),
                                )
                                .truncate(),
                            );
                        });
                        ui.label(
                            RichText::new(tr.size(info.size))
                                .font(theme::semibold(18.0))
                                .color(pal.text),
                        );
                        for l in &info.lines {
                            ui.label(RichText::new(l).size(12.5).color(pal.text_dim));
                        }
                        for n in &info.notes {
                            ui.label(RichText::new(n).size(12.0).color(pal.text_faint).italics());
                        }
                        if let Some(p) = &info.path {
                            ui.add_space(2.0);
                            ui.add(
                                egui::Label::new(RichText::new(p).size(11.5).color(pal.text_faint))
                                    .wrap(),
                            );
                        }
                    });
            });
    }
}
