//! Right panel: "Contents" (children of the focus), "Largest files" and
//! "File types" tabs, plus the search results list.
//!
//! All lists are virtualized with `ScrollArea::show_rows`: only the visible
//! rows are laid out, so a folder with a million entries scrolls as fast as
//! one with ten. The row data itself is computed by background [`jobs`]
//! (sorting a million children by name takes a while) and only ids plus
//! sort keys are kept; names and paths of the ~30 visible rows are read
//! under one short read lock per frame.
//!
//! Hovering a row reports the node back to the app, which highlights its
//! arc in the chart; the chart's hovered node is highlighted here in turn.

use std::sync::Arc;

use eframe::egui::{self, Align2, Color32, FontId, Rect, Sense, Stroke, Ui, pos2, vec2};
use ringscan_core::parking_lot::RwLock;
use ringscan_core::{NodeId, SizeMode, Tree, flags, stats};

use crate::actions::{self, Action, MenuTarget};
use crate::colors::{self, Group};
use crate::i18n::Tr;
use crate::jobs::{self, Slot};
use crate::model::{ChartModel, VirtualNames, display_name};
use crate::settings::SortColumn;
use crate::theme::{self, Pal, icon};
use crate::widgets;

const ROW_H: f32 = 26.0;

/// One child of the focus (sort keys only; the name is read when drawn).
#[derive(Clone, Debug)]
pub struct Row {
    pub id: NodeId,
    pub size: u64,
    pub files: u64,
    pub mtime: i64,
    pub flags: u16,
    pub zoomable: bool,
}

#[derive(Clone, Debug)]
pub struct FileRow {
    pub id: NodeId,
    pub name: String,
    pub folder: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct TypeRow {
    pub ext: String,
    pub group: Group,
    pub files: u64,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct SearchResult {
    pub hits: Vec<FileRow>,
    pub capped: bool,
}

type ContentsKey = (NodeId, u64, SizeMode, SortColumn, bool, bool);
type StatKey = (NodeId, u64, SizeMode);

#[derive(Default)]
pub struct Details {
    /// Id of the scan the lists below were computed for (`Scan::id`).
    scan: u64,
    pub contents: Slot<ContentsKey, Vec<Row>>,
    pub largest: Slot<StatKey, Vec<FileRow>>,
    pub types: Slot<StatKey, Vec<TypeRow>>,
    /// Node hovered in a list this frame.
    pub hovered: Option<NodeId>,
}

/// Everything the panel reads from the app.
pub struct Inputs<'a> {
    /// Id of the scan `tree` belongs to (`Scan::id`).
    pub scan: u64,
    pub tree: &'a Arc<RwLock<Tree>>,
    pub model: &'a ChartModel,
    pub tr: Tr,
    pub pal: Pal,
    pub mode: SizeMode,
    pub show_free: bool,
    pub focus: NodeId,
    pub generation: u64,
    pub scanning: bool,
    /// What the row context menus may offer.
    pub menu_caps: actions::MenuCaps,
    pub now: f64,
    pub chart_hover: Option<NodeId>,
    pub selected: Option<NodeId>,
    pub names: VirtualNames,
    pub search: Option<&'a SearchResult>,
    pub search_busy: bool,
}

pub struct Out {
    pub actions: Vec<Action>,
    pub tab_changed: bool,
}

impl Details {
    /// Starts/refreshes the background queries needed by the visible tab.
    fn refresh(
        &mut self,
        ctx: &egui::Context,
        inp: &Inputs,
        tab: usize,
        sort: SortColumn,
        asc: bool,
    ) {
        let bucket_fast = jobs::gen_bucket(inp.generation, inp.scanning, inp.now, 0.5);
        let bucket_slow = jobs::gen_bucket(inp.generation, inp.scanning, inp.now, 1.5);
        let tree = Arc::clone(inp.tree);
        let (focus, mode, show_free) = (inp.focus, inp.mode, inp.show_free);
        self.contents.poll();
        self.largest.poll();
        self.types.poll();
        match tab {
            0 => self.contents.ensure(
                ctx,
                (focus, bucket_fast, mode, sort, asc, show_free),
                move || contents_rows(&tree.read(), focus, mode, sort, asc, show_free),
            ),
            1 => self
                .largest
                .ensure(ctx, (focus, bucket_slow, mode), move || {
                    let t = tree.read();
                    stats::largest_files(&t, focus, mode, 100)
                        .into_iter()
                        .map(|id| FileRow {
                            id,
                            name: t.name(id).to_string(),
                            folder: t.path(t.node(id).parent()),
                            size: t.size(id, mode),
                        })
                        .collect()
                }),
            _ => self.types.ensure(ctx, (focus, bucket_slow, mode), move || {
                let t = tree.read();
                let mut rows: Vec<TypeRow> = stats::ext_breakdown(&t, focus, mode)
                    .into_iter()
                    .map(|e| {
                        let ext = t.ext_name(e.ext).to_string();
                        TypeRow {
                            group: Group::of_ext(&ext),
                            ext,
                            files: e.files,
                            size: e.size,
                        }
                    })
                    .collect();
                rows.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.ext.cmp(&b.ext)));
                rows
            }),
        }
    }

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        inp: &Inputs,
        tab: &mut usize,
        sort: &mut SortColumn,
        asc: &mut bool,
    ) -> Out {
        if self.scan != inp.scan {
            // Rows of another tree: their ids mean nothing here.
            *self = Details {
                scan: inp.scan,
                ..Details::default()
            };
        }
        self.hovered = None;
        // A resizable panel shrinks to its content; claim the full width
        // so a short "Computing…" frame does not narrow it.
        ui.set_min_width(ui.available_width());
        let mut out = Out {
            actions: Vec::new(),
            tab_changed: false,
        };
        let s = inp.tr.s;
        if let Some(res) = inp.search {
            self.search_ui(ui, inp, res, &mut out);
            return out;
        }
        if inp.search_busy {
            ui.add_space(12.0);
            ui.label(egui::RichText::new(s.computing).color(inp.pal.text_dim));
            return out;
        }
        ui.add_space(2.0);
        out.tab_changed = widgets::tabs(ui, &[s.tab_contents, s.tab_largest, s.tab_types], tab);
        self.refresh(ui.ctx(), inp, *tab, *sort, *asc);
        ui.add_space(2.0);
        match *tab {
            0 => self.contents_ui(ui, inp, sort, asc, &mut out),
            1 => self.largest_ui(ui, inp, &mut out),
            _ => self.types_ui(ui, inp),
        }
        out
    }

    fn contents_ui(
        &mut self,
        ui: &mut Ui,
        inp: &Inputs,
        sort: &mut SortColumn,
        asc: &mut bool,
        out: &mut Out,
    ) {
        let s = inp.tr.s;
        let pal = inp.pal;
        let width = ui.available_width();
        // Column widths: name takes the rest; optional columns drop out on
        // narrow panels.
        let size_w = 76.0;
        let pct_w = if width > 330.0 { 92.0 } else { 0.0 };
        let files_w = if width > 430.0 { 70.0 } else { 0.0 };
        let date_w = if width > 520.0 { 88.0 } else { 0.0 };
        let name_w = (width - size_w - pct_w - files_w - date_w - 8.0).max(60.0);
        let cols = [
            (SortColumn::Name, s.col_name, name_w, Align2::LEFT_CENTER),
            (SortColumn::Size, s.col_size, size_w, Align2::RIGHT_CENTER),
            (SortColumn::Size, s.col_percent, pct_w, Align2::LEFT_CENTER),
            (
                SortColumn::Files,
                s.col_files,
                files_w,
                Align2::RIGHT_CENTER,
            ),
            (
                SortColumn::Modified,
                s.col_modified,
                date_w,
                Align2::RIGHT_CENTER,
            ),
        ];
        // Header.
        let (hrect, _) = ui.allocate_exact_size(vec2(width, 26.0), Sense::hover());
        let mut x = hrect.left();
        for (i, (col, title, w, align)) in cols.iter().enumerate() {
            if *w <= 0.0 {
                continue;
            }
            let r = Rect::from_min_size(pos2(x, hrect.top()), vec2(*w, hrect.height()));
            x += w;
            if i == 2 {
                // The share column sorts like size; no own header action.
                ui.painter().text(
                    r.left_center() + vec2(8.0, 0.0),
                    Align2::LEFT_CENTER,
                    *title,
                    FontId::proportional(12.0),
                    pal.text_dim,
                );
                continue;
            }
            let resp = ui.interact(r, ui.scope_id().with(("hdr", i)), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(r.shrink(1.0), 4.0, pal.card_hover);
            }
            if resp.clicked() {
                if *sort == *col {
                    *asc = !*asc;
                } else {
                    *sort = *col;
                    *asc = matches!(col, SortColumn::Name);
                }
            }
            let active = *sort == *col;
            let color = if active { pal.text } else { pal.text_dim };
            let font = if active {
                theme::semibold(12.0)
            } else {
                FontId::proportional(12.0)
            };
            let text_pos = match *align {
                Align2::RIGHT_CENTER => {
                    r.right_center() - vec2(if active { 22.0 } else { 8.0 }, 0.0)
                }
                _ => r.left_center() + vec2(8.0, 0.0),
            };
            let g = ui.painter().text(text_pos, *align, *title, font, color);
            if active {
                let arrow = if *asc { icon::SORT_UP } else { icon::SORT_DOWN };
                let ax = if matches!(*align, Align2::RIGHT_CENTER) {
                    r.right() - 14.0
                } else {
                    g.right() + 9.0
                };
                ui.painter().text(
                    pos2(ax, r.center().y),
                    Align2::CENTER_CENTER,
                    arrow,
                    theme::icon_font(9.0),
                    pal.text_dim,
                );
            }
        }
        ui.painter().hline(
            hrect.x_range(),
            hrect.bottom(),
            Stroke::new(1.0, pal.divider),
        );

        let rows = match self.contents.get() {
            Some(r) => Arc::clone(r),
            None => {
                ui.add_space(12.0);
                ui.label(egui::RichText::new(s.computing).color(pal.text_dim));
                return;
            }
        };
        if rows.is_empty() {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(s.no_files).color(pal.text_dim))
            });
            return;
        }
        let total = inp.model.center.display.max(1);
        let tr = inp.tr;
        let mut hovered = None;
        egui::ScrollArea::vertical()
            .id_salt(("contents", inp.scan, inp.focus))
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H, rows.len(), |ui, range| {
                let width = ui.available_width();
                // One short read lock for the visible rows.
                let visible: Vec<(String, u64, bool)> = {
                    let t = inp.tree.read_recursive();
                    rows[range.clone()]
                        .iter()
                        .map(|r| {
                            if t.is_live(r.id) {
                                (display_name(&t, r.id, &inp.names), t.dirs(r.id), true)
                            } else {
                                (t.name(r.id).to_string(), 0, false)
                            }
                        })
                        .collect()
                };
                for (row, (name, dirs, live)) in rows[range].iter().zip(visible) {
                    let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
                    let is_dir = row.flags & flags::DIR != 0;
                    let selected = inp.selected == Some(row.id);
                    let synced = inp.chart_hover == Some(row.id);
                    if resp.hovered() {
                        hovered = Some(row.id);
                    }
                    let bg = if selected {
                        Some(pal.selection)
                    } else if resp.hovered() || synced {
                        Some(pal.card_hover)
                    } else {
                        None
                    };
                    if let Some(bg) = bg {
                        ui.painter()
                            .rect_filled(rect.shrink2(vec2(2.0, 1.0)), 4.0, bg);
                    }
                    if selected {
                        let bar = Rect::from_min_size(
                            rect.left_top() + vec2(2.0, 7.0),
                            vec2(3.0, ROW_H - 14.0),
                        );
                        ui.painter().rect_filled(bar, 1.5, pal.accent);
                    }
                    let painter = ui.painter_at(rect);
                    let mut x = rect.left() + 8.0;
                    let cy = rect.center().y;
                    // Swatch: the arc's color (gray if merged into "smaller items").
                    let swatch = inp
                        .model
                        .index_of(row.id)
                        .map(|i| inp.model.arcs[i].color)
                        .unwrap_or_else(|| colors::rest_colors(pal.dark).0);
                    painter.rect_filled(
                        Rect::from_center_size(pos2(x + 3.0, cy), vec2(6.0, 14.0)),
                        2.0,
                        swatch,
                    );
                    x += 12.0;
                    let glyph = if is_dir { icon::FOLDER } else { icon::DOCUMENT };
                    let glyph_color = if is_dir {
                        Color32::from_rgb(0xE8, 0xB3, 0x39)
                    } else {
                        pal.text_dim
                    };
                    if row.flags & flags::VIRTUAL == 0 {
                        painter.text(
                            pos2(x + 8.0, cy),
                            Align2::CENTER_CENTER,
                            glyph,
                            theme::icon_font(14.0),
                            glyph_color,
                        );
                    } else {
                        painter.text(
                            pos2(x + 8.0, cy),
                            Align2::CENTER_CENTER,
                            icon::PIE,
                            theme::icon_font(13.0),
                            pal.text_dim,
                        );
                    }
                    x += 22.0;
                    let font = FontId::proportional(13.5);
                    let name_room = cols[0].2 - (x - rect.left()) - 4.0;
                    let shown = widgets::fit_text(ui, &name, &font, name_room, false);
                    let name_color = if live { pal.text } else { pal.text_faint };
                    painter.text(pos2(x, cy), Align2::LEFT_CENTER, shown, font, name_color);
                    let mut cx = rect.left() + cols[0].2;
                    painter.text(
                        pos2(cx + size_w - 8.0, cy),
                        Align2::RIGHT_CENTER,
                        tr.size(row.size),
                        FontId::proportional(13.0),
                        pal.text,
                    );
                    cx += size_w;
                    if pct_w > 0.0 {
                        let frac = row.size as f64 / total as f64;
                        let bar = Rect::from_min_size(pos2(cx + 8.0, cy - 3.0), vec2(36.0, 6.0));
                        widgets::bar(&painter, bar, frac as f32, swatch, pal.bar_track);
                        painter.text(
                            pos2(bar.right() + 6.0, cy),
                            Align2::LEFT_CENTER,
                            tr.percent(frac),
                            FontId::proportional(12.0),
                            pal.text_dim,
                        );
                        cx += pct_w;
                    }
                    if files_w > 0.0 {
                        let t = if is_dir {
                            tr.count(row.files)
                        } else {
                            String::new()
                        };
                        painter.text(
                            pos2(cx + files_w - 8.0, cy),
                            Align2::RIGHT_CENTER,
                            t,
                            FontId::proportional(12.5),
                            pal.text_dim,
                        );
                        cx += files_w;
                    }
                    if date_w > 0.0 {
                        painter.text(
                            pos2(cx + date_w - 8.0, cy),
                            Align2::RIGHT_CENTER,
                            tr.short_date(row.mtime),
                            FontId::proportional(12.5),
                            pal.text_dim,
                        );
                    }
                    let _ = dirs;
                    if resp.double_clicked() && row.zoomable {
                        out.actions.push(Action::Zoom(row.id));
                    } else if resp.clicked() {
                        out.actions.push(Action::Select(Some(row.id)));
                    }
                    let target = MenuTarget {
                        id: row.id,
                        zoomable: row.zoomable,
                        is_dir,
                        is_virtual: row.flags & flags::VIRTUAL != 0,
                        is_focus: false,
                    };
                    if let Some(a) = actions::attach_menu(&resp, tr, Some(&target), inp.menu_caps) {
                        out.actions.push(a);
                    }
                }
            });
        self.hovered = hovered;
    }

    fn largest_ui(&mut self, ui: &mut Ui, inp: &Inputs, out: &mut Out) {
        let pal = inp.pal;
        let s = inp.tr.s;
        let Some(rows) = self.largest.get().cloned() else {
            ui.add_space(12.0);
            ui.label(egui::RichText::new(s.computing).color(pal.text_dim));
            return;
        };
        self.file_list(ui, inp, &rows, "largest", out);
    }

    fn search_ui(&mut self, ui: &mut Ui, inp: &Inputs, res: &SearchResult, out: &mut Out) {
        let pal = inp.pal;
        let s = inp.tr.s;
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(s.search_results).font(theme::semibold(14.0)));
            ui.label(
                egui::RichText::new(inp.tr.n_results(res.hits.len() as u64, res.capped))
                    .color(pal.text_dim),
            );
        });
        ui.add_space(4.0);
        if res.hits.is_empty() {
            ui.add_space(12.0);
            ui.label(egui::RichText::new(s.nothing_found).color(pal.text_dim));
            return;
        }
        self.file_list(ui, inp, &res.hits, "search", out);
    }

    /// Name + size + folder rows (largest files, search results).
    fn file_list(
        &mut self,
        ui: &mut Ui,
        inp: &Inputs,
        rows: &[FileRow],
        salt: &str,
        out: &mut Out,
    ) {
        let pal = inp.pal;
        let tr = inp.tr;
        let row_h = 40.0;
        let mut hovered = None;
        egui::ScrollArea::vertical()
            .id_salt((salt, inp.scan, inp.focus))
            .auto_shrink([false, false])
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                let width = ui.available_width();
                let live: Vec<(bool, bool, bool)> = {
                    let t = inp.tree.read_recursive();
                    rows[range.clone()]
                        .iter()
                        .map(|r| {
                            let ok = t.is_live(r.id);
                            let n = t.node(r.id);
                            (ok, n.is_dir(), ok && n.is_dir() && t.has_children(r.id))
                        })
                        .collect()
                };
                for (k, (row, (ok, is_dir, zoomable))) in
                    rows[range.clone()].iter().zip(live).enumerate()
                {
                    let rank = range.start + k + 1;
                    let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::click());
                    if resp.hovered() {
                        hovered = Some(row.id);
                    }
                    let selected = inp.selected == Some(row.id);
                    if selected {
                        ui.painter()
                            .rect_filled(rect.shrink2(vec2(2.0, 1.0)), 4.0, pal.selection);
                    } else if resp.hovered() || inp.chart_hover == Some(row.id) {
                        ui.painter()
                            .rect_filled(rect.shrink2(vec2(2.0, 1.0)), 4.0, pal.card_hover);
                    }
                    let painter = ui.painter_at(rect);
                    let x0 = rect.left() + 8.0;
                    painter.text(
                        pos2(x0 + 14.0, rect.top() + 13.0),
                        Align2::RIGHT_CENTER,
                        rank.to_string(),
                        FontId::proportional(11.5),
                        pal.text_faint,
                    );
                    let glyph = if is_dir { icon::FOLDER } else { icon::DOCUMENT };
                    painter.text(
                        pos2(x0 + 28.0, rect.top() + 13.0),
                        Align2::CENTER_CENTER,
                        glyph,
                        theme::icon_font(13.0),
                        if is_dir {
                            Color32::from_rgb(0xE8, 0xB3, 0x39)
                        } else {
                            pal.text_dim
                        },
                    );
                    let size_text = tr.size(row.size);
                    let size_g = painter.layout_no_wrap(size_text, theme::semibold(13.0), pal.text);
                    let name_room = width - 48.0 - size_g.size().x - 20.0;
                    let font = FontId::proportional(13.5);
                    let name = widgets::fit_text(ui, &row.name, &font, name_room, false);
                    painter.text(
                        pos2(x0 + 40.0, rect.top() + 13.0),
                        Align2::LEFT_CENTER,
                        name,
                        font,
                        if ok { pal.text } else { pal.text_faint },
                    );
                    let sx = rect.right() - 10.0 - size_g.size().x;
                    painter.galley(
                        pos2(sx, rect.top() + 13.0 - size_g.size().y * 0.5),
                        size_g,
                        pal.text,
                    );
                    let small = FontId::proportional(11.5);
                    let folder = widgets::fit_text(ui, &row.folder, &small, width - 58.0, true);
                    painter.text(
                        pos2(x0 + 40.0, rect.top() + 29.0),
                        Align2::LEFT_CENTER,
                        folder,
                        small,
                        pal.text_faint,
                    );
                    if resp.double_clicked() {
                        out.actions.push(if zoomable {
                            Action::Zoom(row.id)
                        } else {
                            Action::Select(Some(row.id))
                        });
                    } else if resp.clicked() {
                        out.actions.push(Action::Select(Some(row.id)));
                    }
                    let target = MenuTarget {
                        id: row.id,
                        zoomable,
                        is_dir,
                        is_virtual: false,
                        is_focus: false,
                    };
                    if ok
                        && let Some(a) =
                            actions::attach_menu(&resp, tr, Some(&target), inp.menu_caps)
                    {
                        out.actions.push(a);
                    }
                }
            });
        self.hovered = hovered;
    }

    fn types_ui(&mut self, ui: &mut Ui, inp: &Inputs) {
        let pal = inp.pal;
        let tr = inp.tr;
        let s = tr.s;
        let Some(rows) = self.types.get().cloned() else {
            ui.add_space(12.0);
            ui.label(egui::RichText::new(s.computing).color(pal.text_dim));
            return;
        };
        if rows.is_empty() {
            ui.add_space(16.0);
            ui.label(egui::RichText::new(s.no_files).color(pal.text_dim));
            return;
        }
        let total: u64 = rows.iter().map(|r| r.size).sum::<u64>().max(1);
        let row_h = 40.0;
        egui::ScrollArea::vertical()
            .id_salt(("types", inp.scan, inp.focus))
            .auto_shrink([false, false])
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                let width = ui.available_width();
                for row in &rows[range] {
                    let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
                    if resp.hovered() {
                        ui.painter()
                            .rect_filled(rect.shrink2(vec2(2.0, 1.0)), 4.0, pal.card_hover);
                    }
                    let painter = ui.painter_at(rect);
                    let color = row.group.color(pal.dark);
                    let x0 = rect.left() + 10.0;
                    painter.rect_filled(
                        Rect::from_min_size(pos2(x0, rect.top() + 8.0), vec2(12.0, 12.0)),
                        3.0,
                        color,
                    );
                    let ext = if row.ext.is_empty() {
                        s.no_ext.to_string()
                    } else {
                        format!(".{}", row.ext)
                    };
                    painter.text(
                        pos2(x0 + 20.0, rect.top() + 14.0),
                        Align2::LEFT_CENTER,
                        ext,
                        theme::semibold(13.5),
                        pal.text,
                    );
                    let size_text = format!(
                        "{}  ·  {}",
                        tr.size(row.size),
                        tr.percent(row.size as f64 / total as f64)
                    );
                    painter.text(
                        pos2(rect.right() - 10.0, rect.top() + 14.0),
                        Align2::RIGHT_CENTER,
                        size_text,
                        FontId::proportional(13.0),
                        pal.text,
                    );
                    let sub = format!("{} · {}", group_name(tr, row.group), tr.n_files(row.files));
                    painter.text(
                        pos2(x0 + 20.0, rect.top() + 30.0),
                        Align2::LEFT_CENTER,
                        sub,
                        FontId::proportional(11.5),
                        pal.text_faint,
                    );
                    let bar = Rect::from_min_size(
                        pos2(rect.right() - 10.0 - 90.0, rect.top() + 27.0),
                        vec2(90.0, 5.0),
                    );
                    widgets::bar(
                        &painter,
                        bar,
                        (row.size as f64 / total as f64) as f32,
                        color,
                        pal.bar_track,
                    );
                }
            });
    }
}

/// Children of `focus` sorted for the "Contents" tab.
fn contents_rows(
    t: &Tree,
    focus: NodeId,
    mode: SizeMode,
    sort: SortColumn,
    asc: bool,
    show_free: bool,
) -> Vec<Row> {
    if !t.is_live(focus) {
        return Vec::new();
    }
    let mut rows: Vec<Row> = t
        .children(focus)
        .filter(|&c| show_free || !t.node(c).has(flags::FREE))
        .map(|c| {
            let n = t.node(c);
            Row {
                id: c,
                size: n.size(mode),
                files: t.files(c),
                mtime: n.mtime(),
                flags: n.flags(),
                zoomable: n.is_dir() && t.has_children(c),
            }
        })
        .collect();
    match sort {
        SortColumn::Size => rows.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id))),
        SortColumn::Files => rows.sort_by(|a, b| b.files.cmp(&a.files).then(b.size.cmp(&a.size))),
        SortColumn::Modified => {
            rows.sort_by(|a, b| b.mtime.cmp(&a.mtime).then(b.size.cmp(&a.size)))
        }
        SortColumn::Name => {
            // Folders first, then case-insensitive name.
            let mut keyed: Vec<(bool, String, Row)> = rows
                .into_iter()
                .map(|r| (r.flags & flags::DIR == 0, t.name(r.id).to_lowercase(), r))
                .collect();
            keyed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            rows = keyed.into_iter().map(|k| k.2).collect();
            // Name sorts ascending by default: flip the meaning below.
            if !asc {
                rows.reverse();
            }
            return rows;
        }
    }
    if asc {
        rows.reverse();
    }
    rows
}

pub fn group_name(tr: Tr, g: Group) -> &'static str {
    let s = tr.s;
    match g {
        Group::Video => s.grp_video,
        Group::Audio => s.grp_audio,
        Group::Images => s.grp_images,
        Group::Archives => s.grp_archives,
        Group::Programs => s.grp_programs,
        Group::Documents => s.grp_documents,
        Group::Code => s.grp_code,
        Group::System => s.grp_system,
        Group::Other => s.grp_other,
    }
}
