//! The window chrome around the chart: left sidebar (drives, Recycle Bin,
//! installed apps), top toolbar (navigation, breadcrumb, search, view
//! modes, settings) and bottom status bar.
//!
//! Each function takes plain inputs and returns [`Action`]s; the settings
//! the toolbar edits are passed as `&mut` and compared by the caller.

use std::path::PathBuf;

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Layout, Rect, RichText, Sense, Stroke, StrokeKind,
    TextureHandle, Ui, pos2, vec2,
};
use ringscan_core::NodeId;

use crate::actions::Action;
use crate::i18n::Tr;
use crate::platform::DriveKind;
use crate::scan_source::{Method, ScanStatus};
use crate::settings::{self, ColorMode, LangPref, Settings, SizePref, ThemePref};
use crate::theme::{self, Pal, icon};
use crate::widgets;

/// One drive in the sidebar.
#[derive(Clone, Debug)]
pub struct DriveCard {
    pub letter: char,
    pub label: String,
    pub fs: String,
    pub kind: DriveKind,
    pub used: Option<u64>,
    pub total: Option<u64>,
    /// The focus is this drive or inside it.
    pub active: bool,
    /// Counted in the "This PC" summary (scanned by "This PC").
    pub this_pc: bool,
}

pub struct SidebarIn<'a> {
    pub tr: Tr,
    pub pal: Pal,
    pub drives: &'a [DriveCard],
    /// The current tree is a "This PC" scan and it is the focus.
    pub this_pc_active: bool,
    pub recycle: Option<(u64, u64)>,
    pub logo: Option<&'a TextureHandle>,
    /// System-changing actions (emptying the Recycle Bin) are allowed;
    /// false while showing synthetic demo data.
    pub system_actions: bool,
}

pub struct SidebarOut {
    pub actions: Vec<Action>,
    /// Where the "Support" link (`support`), "Choose folder…"
    /// (`choose-folder`) and the Recycle Bin card (`recycle-bin`) were
    /// drawn (harness probes; tests check they never overlap).
    pub probes: Vec<(&'static str, Rect)>,
}

pub fn sidebar(ui: &mut Ui, inp: &SidebarIn) -> SidebarOut {
    let mut out = Vec::new();
    let mut probes = Vec::new();
    let (tr, pal) = (inp.tr, inp.pal);
    let s = tr.s;
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        if let Some(logo) = inp.logo {
            ui.add(egui::Image::new(logo).fit_to_exact_size(vec2(22.0, 22.0)));
        }
        ui.label(
            RichText::new(s.app_name)
                .font(theme::semibold(16.0))
                .color(pal.text),
        );
    });
    ui.add_space(12.0);
    widgets::caption(ui, s.drives);

    // The bottom block goes first, in a child ui pinned to the bottom of
    // the panel, so its height is known before the drive list is laid out:
    // the list then scrolls in the height that is left instead of running
    // under the Recycle Bin card in a low window.
    let avail = ui.available_rect_before_wrap();
    let mut bottom = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(avail)
            .layout(Layout::bottom_up(Align::Min)),
    );
    bottom_block(&mut bottom, inp, &mut out, &mut probes);
    // "Choose folder…" stays under the list, never scrolled away.
    let pick_h = PICK_SPACE + PICK_H + 2.0 * ui.spacing().item_spacing.y;
    let list_h = avail.height() - bottom.min_rect().height() - SIDEBAR_GAP - pick_h;
    egui::ScrollArea::vertical()
        .id_salt("sidebar-drives")
        .max_height(list_h.max(0.0))
        .auto_shrink([false, true])
        .show(ui, |ui| drive_list(ui, inp, &mut out));
    ui.add_space(PICK_SPACE);
    let pick = ui
        .add_sized(
            [ui.available_width(), PICK_H],
            egui::Button::new((
                RichText::new(icon::FOLDER_OPEN).font(theme::icon_font(14.0)),
                s.choose_folder,
            )),
        )
        .on_hover_text(s.choose_folder_tip);
    probes.push(("choose-folder", pick.rect));
    if pick.clicked() {
        out.push(Action::PickFolder);
    }
    SidebarOut {
        actions: out,
        probes,
    }
}

/// Least free space between "Choose folder…" and the bottom block.
const SIDEBAR_GAP: f32 = 8.0;
/// Height of "Choose folder…" and the space above it.
const PICK_H: f32 = 32.0;
const PICK_SPACE: f32 = 8.0;

/// "This PC" and one card per drive.
fn drive_list(ui: &mut Ui, inp: &SidebarIn, out: &mut Vec<Action>) {
    let (tr, pal) = (inp.tr, inp.pal);
    let s = tr.s;
    // "This PC" summary.
    let known: Vec<&DriveCard> = inp
        .drives
        .iter()
        .filter(|d| d.this_pc && d.total.is_some())
        .collect();
    let (used, total) = known.iter().fold((0u64, 0u64), |acc, d| {
        (acc.0 + d.used.unwrap_or(0), acc.1 + d.total.unwrap_or(0))
    });
    let pc_sub = if total > 0 {
        tr.free_of(total.saturating_sub(used), total)
    } else {
        tr.n_drives(inp.drives.iter().filter(|d| d.this_pc).count() as u64)
    };
    let frac = (total > 0).then(|| used as f32 / total as f32);
    if card(
        ui,
        &pal,
        icon::PC,
        s.this_pc,
        None,
        &pc_sub,
        frac,
        inp.this_pc_active,
    )
    .on_hover_text(s.this_pc)
    .clicked()
    {
        out.push(Action::ScanThisPc);
    }
    for d in inp.drives {
        let title = format!(
            "{} ({}:)",
            if d.label.is_empty() {
                s.local_disk
            } else {
                &d.label
            },
            d.letter
        );
        let sub = match (d.used, d.total) {
            (Some(u), Some(t)) if t > 0 => tr.free_of(t.saturating_sub(u), t),
            (Some(u), None) => tr.used(u),
            _ => s.unknown.to_string(),
        };
        let frac = match (d.used, d.total) {
            (Some(u), Some(t)) if t > 0 => Some(u as f32 / t as f32),
            _ => None,
        };
        let glyph = match d.kind {
            DriveKind::Network => "\u{E8CE}",
            DriveKind::Removable => "\u{E88E}",
            DriveKind::CdRom => "\u{E958}",
            _ => icon::DRIVE,
        };
        let sub = if d.fs.is_empty() {
            sub
        } else {
            format!("{} · {sub}", d.fs)
        };
        if card(ui, &pal, glyph, &title, None, &sub, frac, d.active)
            .on_hover_text(s.scan_drive_tip)
            .clicked()
        {
            out.push(Action::ShowDrive(d.letter));
        }
    }
}

/// The bottom of the sidebar, drawn bottom-up: Recycle Bin, installed apps
/// and, smallest and last, the "Support" link.
fn bottom_block(
    ui: &mut Ui,
    inp: &SidebarIn,
    out: &mut Vec<Action>,
    probes: &mut Vec<(&'static str, Rect)>,
) {
    let (tr, pal) = (inp.tr, inp.pal);
    let s = tr.s;
    ui.add_space(6.0);
    let link = support_link_ui(ui, &pal, s.support);
    probes.push(("support", link.rect));
    if link.clicked() {
        out.push(Action::ShowSupport);
    }
    ui.add_space(4.0);
    let apps = ui
        .add_sized(
            [ui.available_width(), 32.0],
            egui::Button::new((
                RichText::new(icon::APPS).font(theme::icon_font(14.0)),
                s.installed_apps,
            )),
        )
        .on_hover_text(s.installed_apps_tip);
    if apps.clicked() {
        out.push(Action::InstalledApps);
    }
    ui.add_space(4.0);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 52.0), Sense::hover());
    probes.push(("recycle-bin", rect));
    ui.painter().rect(
        rect,
        6.0,
        pal.card,
        Stroke::new(1.0, pal.divider),
        StrokeKind::Inside,
    );
    ui.painter().text(
        pos2(rect.left() + 20.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon::DELETE,
        theme::icon_font(16.0),
        pal.text_dim,
    );
    let btn_w = {
        let g = ui.painter().layout_no_wrap(
            s.empty_bin.to_string(),
            FontId::proportional(13.0),
            pal.text,
        );
        g.size().x + 22.0
    };
    let text_room = rect.width() - 46.0 - btn_w - 12.0;
    let title = widgets::fit_text(ui, s.recycle_bin, &theme::semibold(13.5), text_room, false);
    ui.painter().text(
        pos2(rect.left() + 38.0, rect.top() + 17.0),
        Align2::LEFT_CENTER,
        title,
        theme::semibold(13.5),
        pal.text,
    );
    let bin_sub = match inp.recycle {
        Some((bytes, items)) => format!("{} · {}", tr.size(bytes), tr.n_items(items)),
        None => s.unknown.to_string(),
    };
    let bin_sub = widgets::fit_text(ui, &bin_sub, &FontId::proportional(12.0), text_room, false);
    ui.painter().text(
        pos2(rect.left() + 38.0, rect.top() + 35.0),
        Align2::LEFT_CENTER,
        bin_sub,
        FontId::proportional(12.0),
        pal.text_dim,
    );
    let btn_rect = Rect::from_min_size(
        pos2(rect.right() - 8.0 - btn_w, rect.center().y - 14.0),
        vec2(btn_w, 28.0),
    );
    let btn = ui.add_enabled_ui(inp.system_actions, |ui| {
        ui.put(
            btn_rect,
            egui::Button::new(RichText::new(s.empty_bin).size(13.0))
                .wrap_mode(egui::TextWrapMode::Extend),
        )
    });
    if btn.inner.clicked() {
        out.push(Action::AskEmptyBin);
    }
}

/// A quiet "♥ Support" text link, centered in the sidebar.
fn support_link_ui(ui: &mut Ui, pal: &Pal, text: &str) -> egui::Response {
    let font = FontId::proportional(12.5);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), font.clone(), pal.text_dim);
    let w = galley.size().x + 22.0;
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
    let rect = Rect::from_center_size(row.center(), vec2(w + 12.0, 24.0));
    let resp = ui
        .interact(rect, egui::Id::unique("support-link"), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 4.0, pal.card_hover);
    }
    let color = if resp.hovered() {
        pal.text
    } else {
        pal.text_dim
    };
    let x = rect.left() + 6.0;
    p.text(
        pos2(x + 7.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon::HEART,
        theme::icon_font(12.0),
        crate::support::heart_color(pal),
    );
    p.text(
        pos2(x + 20.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        font,
        color,
    );
    resp
}

/// One clickable drive card with icon, title, usage bar and subtitle.
#[allow(clippy::too_many_arguments)]
fn card(
    ui: &mut Ui,
    pal: &Pal,
    glyph: &str,
    title: &str,
    badge: Option<&str>,
    sub: &str,
    frac: Option<f32>,
    active: bool,
) -> egui::Response {
    let w = ui.available_width();
    let h = if frac.is_some() { 58.0 } else { 46.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    let p = ui.painter();
    let fill = if active {
        pal.nav_selected
    } else if resp.hovered() {
        pal.card_hover
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, 6.0, fill);
    if active {
        let bar = Rect::from_min_size(rect.left_top() + vec2(0.0, 14.0), vec2(3.0, h - 28.0));
        p.rect_filled(bar, 1.5, pal.accent);
    }
    p.text(
        pos2(rect.left() + 20.0, rect.top() + 18.0),
        Align2::CENTER_CENTER,
        glyph,
        theme::icon_font(18.0),
        if active { pal.accent } else { pal.text_dim },
    );
    let x = rect.left() + 40.0;
    let title_font = theme::semibold(13.5);
    let max_title = rect.right() - x - 8.0 - if badge.is_some() { 44.0 } else { 0.0 };
    let shown = widgets::fit_text(ui, title, &title_font, max_title, false);
    let g = p.layout_no_wrap(shown, title_font, pal.text);
    let title_w = g.size().x;
    p.galley(pos2(x, rect.top() + 9.0), g, pal.text);
    if let Some(b) = badge {
        p.text(
            pos2(x + title_w + 6.0, rect.top() + 18.0),
            Align2::LEFT_CENTER,
            b,
            FontId::proportional(11.0),
            pal.text_faint,
        );
    }
    let mut y = rect.top() + 30.0;
    if let Some(f) = frac {
        let bar = Rect::from_min_size(pos2(x, y), vec2(rect.right() - x - 10.0, 5.0));
        let color = if f > 0.9 { pal.bar_warn } else { pal.bar_fill };
        widgets::bar(p, bar, f, color, pal.bar_track);
        y += 9.0;
    }
    p.text(
        pos2(x, y),
        Align2::LEFT_TOP,
        sub,
        FontId::proportional(12.0),
        pal.text_dim,
    );
    resp
}

/// A breadcrumb segment.
#[derive(Clone, Debug)]
pub struct Crumb {
    pub label: String,
    pub target: CrumbTarget,
    /// The virtual "This PC" root (shown with a PC icon).
    pub is_this_pc: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CrumbTarget {
    /// A node of the current tree.
    Node(NodeId),
    /// A folder above the scanned root (clicking scans it).
    Path(PathBuf),
    /// The current focus (not clickable).
    Current,
}

pub struct ToolbarIn<'a> {
    pub tr: Tr,
    pub pal: Pal,
    pub can_back: bool,
    pub can_up: bool,
    pub can_rescan: bool,
    pub scanning: bool,
    pub crumbs: &'a [Crumb],
    pub elevated: bool,
    pub explorer_menu: bool,
    /// Ask the search box to take keyboard focus.
    pub focus_search: bool,
}

pub struct ToolbarOut {
    pub actions: Vec<Action>,
    pub search_changed: bool,
    pub search_has_focus: bool,
}

pub fn toolbar(ui: &mut Ui, inp: &ToolbarIn, search: &mut String, st: &mut Settings) -> ToolbarOut {
    let s = inp.tr.s;
    let pal = inp.pal;
    let mut out = ToolbarOut {
        actions: Vec::new(),
        search_changed: false,
        search_has_focus: false,
    };
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if widgets::icon_button(ui, icon::PANEL_LEFT, s.panel_toggle_tip, true).clicked() {
            st.left_panel = !st.left_panel;
        }
        if widgets::icon_button(ui, icon::BACK, s.back_tip, inp.can_back).clicked() {
            out.actions.push(Action::Back);
        }
        if widgets::icon_button(ui, icon::UP, s.up_tip, inp.can_up).clicked() {
            out.actions.push(Action::Up);
        }
        if inp.scanning {
            if widgets::icon_button(ui, icon::STOP, s.cancel_scan_tip, true).clicked() {
                out.actions.push(Action::CancelScan);
            }
        } else if widgets::icon_button(ui, icon::REFRESH, s.rescan_tip, inp.can_rescan).clicked() {
            out.actions.push(Action::Rescan(ringscan_core::NONE));
        }
        ui.add_space(6.0);

        // Right-aligned controls first, so the breadcrumb gets the rest.
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if widgets::icon_button(ui, icon::PANEL_RIGHT, s.panel_toggle_tip, true).clicked() {
                st.right_panel = !st.right_panel;
            }
            settings_menu(ui, inp, st, &mut out);
            let mut cm = match st.color_mode {
                ColorMode::Folders => 0,
                ColorMode::Types => 1,
                ColorMode::Age => 2,
            };
            if widgets::segmented(
                ui,
                "colors",
                &[s.colors_folders, s.colors_types, s.colors_age],
                &mut cm,
                s.color_mode_tip,
            ) {
                st.color_mode = [ColorMode::Folders, ColorMode::Types, ColorMode::Age][cm];
            }
            let mut sm = usize::from(st.size_mode == SizePref::Logical);
            if widgets::segmented(
                ui,
                "size",
                &[s.on_disk, s.logical],
                &mut sm,
                s.size_mode_tip,
            ) {
                st.size_mode = if sm == 1 {
                    SizePref::Logical
                } else {
                    SizePref::Allocated
                };
            }
            ui.add_space(4.0);
            // Search box.
            let w = (ui.available_width() * 0.35).clamp(140.0, 240.0);
            let edit = egui::TextEdit::singleline(search)
                .hint_text(RichText::new(s.search_hint).color(pal.text_faint))
                .desired_width(w - 30.0)
                .margin(vec2(30.0, 6.0))
                .id(egui::Id::unique("search-box"));
            let resp = ui.add(edit).on_hover_text(s.search_tip);
            ui.painter().text(
                resp.rect.left_center() + vec2(15.0, 0.0),
                Align2::CENTER_CENTER,
                icon::SEARCH,
                theme::icon_font(13.0),
                pal.text_dim,
            );
            if !search.is_empty() {
                let r = Rect::from_center_size(
                    resp.rect.right_center() - vec2(15.0, 0.0),
                    vec2(22.0, 22.0),
                );
                let x = ui.interact(r, egui::Id::unique("search-clear"), Sense::click());
                if x.hovered() {
                    ui.painter().rect_filled(r, 4.0, pal.card_hover);
                }
                ui.painter().text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    icon::CANCEL,
                    theme::icon_font(10.0),
                    pal.text_dim,
                );
                if x.on_hover_text(s.clear_search_tip).clicked() {
                    search.clear();
                    out.search_changed = true;
                }
            }
            if inp.focus_search {
                resp.request_focus();
            }
            if resp.changed() {
                out.search_changed = true;
            }
            if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                search.clear();
                out.search_changed = true;
                resp.surrender_focus();
            }
            out.search_has_focus = resp.has_focus();
            ui.add_space(6.0);
            // Breadcrumb in the remaining space (left to right).
            // Breadcrumb in an address-bar box over the remaining width.
            let rest = ui.available_rect_before_wrap();
            let bar = Rect::from_min_max(
                pos2(rest.left(), rest.center().y - 15.0),
                pos2(rest.right(), rest.center().y + 15.0),
            );
            ui.painter().rect(
                bar,
                4.0,
                if pal.dark {
                    Color32::from_rgb(0x2B, 0x2B, 0x2B)
                } else {
                    Color32::WHITE
                },
                Stroke::new(1.0, pal.border),
                StrokeKind::Inside,
            );
            let glyph = if inp.crumbs.first().is_some_and(|c| c.is_this_pc) {
                icon::PC
            } else {
                icon::FOLDER
            };
            ui.painter().text(
                pos2(bar.left() + 16.0, bar.center().y),
                Align2::CENTER_CENTER,
                glyph,
                theme::icon_font(14.0),
                pal.text_dim,
            );
            let inner = Rect::from_min_max(
                pos2(bar.left() + 30.0, bar.top()),
                pos2(bar.right() - 6.0, bar.bottom()),
            );
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(inner)
                    .layout(Layout::left_to_right(Align::Center)),
            );
            child.set_clip_rect(inner);
            breadcrumb(&mut child, inp, &mut out);
        });
    });
    out
}

fn breadcrumb(ui: &mut Ui, inp: &ToolbarIn, out: &mut ToolbarOut) {
    let pal = inp.pal;
    ui.spacing_mut().item_spacing.x = 0.0;
    let font = FontId::proportional(14.0);
    let sep_w = 18.0;
    let avail = ui.available_width() - 4.0;
    let widths: Vec<f32> = inp
        .crumbs
        .iter()
        .map(|c| {
            ui.painter()
                .layout_no_wrap(c.label.clone(), font.clone(), pal.text)
                .size()
                .x
                + 12.0
        })
        .collect();
    // Drop crumbs after the first until the rest fits; mark the gap "…".
    let mut first_shown = 1usize.min(inp.crumbs.len());
    let total = |from: usize| -> f32 {
        widths.first().copied().unwrap_or(0.0)
            + widths[from.min(widths.len())..]
                .iter()
                .map(|w| w + sep_w)
                .sum::<f32>()
            + if from > 1 { 24.0 + sep_w } else { 0.0 }
    };
    while first_shown + 1 < inp.crumbs.len() && total(first_shown) > avail {
        first_shown += 1;
    }
    let mut show = |ui: &mut Ui, i: usize, max_w: f32| {
        let c = &inp.crumbs[i];
        let is_last = i + 1 == inp.crumbs.len();
        let label = widgets::fit_text(ui, &c.label, &font, (max_w - 12.0).max(30.0), false);
        let galley = ui.painter().layout_no_wrap(
            label,
            if is_last {
                theme::semibold(14.0)
            } else {
                font.clone()
            },
            pal.text,
        );
        let size = vec2(galley.size().x + 12.0, 24.0);
        let clickable = !matches!(c.target, CrumbTarget::Current);
        let (rect, resp) = ui.allocate_exact_size(
            size,
            if clickable {
                Sense::click()
            } else {
                Sense::hover()
            },
        );
        if clickable && resp.hovered() {
            ui.painter().rect_filled(rect, 4.0, pal.card_hover);
        }
        let color = match c.target {
            CrumbTarget::Path(_) => pal.text_faint,
            CrumbTarget::Current => pal.text,
            CrumbTarget::Node(_) => pal.text_dim,
        };
        ui.painter()
            .galley(rect.center() - galley.size() * 0.5, galley, color);
        let resp = match &c.target {
            CrumbTarget::Path(_) => resp.on_hover_text(inp.tr.s.scan_parent_tip),
            _ => resp,
        };
        if resp.clicked() {
            match &c.target {
                CrumbTarget::Node(id) => out.actions.push(Action::Zoom(*id)),
                CrumbTarget::Path(p) => out.actions.push(Action::ScanPath(p.clone())),
                CrumbTarget::Current => {}
            }
        }
    };
    let sep = |ui: &mut Ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(sep_w, 28.0), Sense::hover());
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            icon::CHEVRON_RIGHT,
            theme::icon_font(9.0),
            pal.text_faint,
        );
    };
    if inp.crumbs.is_empty() {
        return;
    }
    show(ui, 0, avail * 0.5);
    if first_shown > 1 {
        sep(ui);
        let (rect, _) = ui.allocate_exact_size(vec2(24.0, 28.0), Sense::hover());
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            "…",
            font.clone(),
            pal.text_faint,
        );
    }
    for i in first_shown..inp.crumbs.len() {
        sep(ui);
        let room = ui.available_width();
        show(ui, i, room);
    }
}

fn settings_menu(ui: &mut Ui, inp: &ToolbarIn, st: &mut Settings, out: &mut ToolbarOut) {
    let s = inp.tr.s;
    let pal = inp.pal;
    let button = egui::Button::new(RichText::new(icon::SETTINGS).font(theme::icon_font(16.0)))
        .frame_when_inactive(false)
        .min_size(vec2(34.0, 30.0));
    let resp = ui.add(button).on_hover_text(s.settings);
    egui::Popup::menu(&resp)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_min_width(300.0);
            ui.add_space(2.0);
            ui.label(RichText::new(s.settings).font(theme::semibold(15.0)));
            ui.add_space(4.0);
            let row = |ui: &mut Ui, label: &str| {
                ui.label(RichText::new(label).color(pal.text_dim).size(12.5));
            };
            row(ui, s.language);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut st.language, LangPref::Auto, s.lang_auto);
                ui.selectable_value(&mut st.language, LangPref::Uk, "Українська");
                ui.selectable_value(&mut st.language, LangPref::En, "English");
            });
            ui.add_space(4.0);
            row(ui, s.theme);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut st.theme, ThemePref::System, s.theme_system);
                ui.selectable_value(&mut st.theme, ThemePref::Light, s.theme_light);
                ui.selectable_value(&mut st.theme, ThemePref::Dark, s.theme_dark);
            });
            ui.separator();
            widgets::toggle(ui, &mut st.show_free, s.show_free);
            ui.add_space(4.0);
            row(ui, s.max_depth);
            ui.add(egui::Slider::new(
                &mut st.max_depth,
                settings::MAX_DEPTH_RANGE,
            ));
            row(ui, s.min_arc);
            ui.add(
                egui::Slider::new(&mut st.min_arc_deg, settings::MIN_ARC_RANGE)
                    .logarithmic(true)
                    .max_decimals(2)
                    // "0,30°" in Ukrainian, like every other number.
                    .custom_formatter(move |v, _| inp.tr.degrees(v))
                    .custom_parser(Tr::parse_decimal),
            );
            ui.separator();
            let mut menu = inp.explorer_menu;
            if widgets::toggle(ui, &mut menu, s.explorer_menu)
                .on_hover_text(s.explorer_menu_tip)
                .changed()
            {
                out.actions.push(Action::SetExplorerMenu(menu));
            }
            if !inp.elevated {
                ui.add_space(4.0);
                let b = egui::Button::new((
                    RichText::new(icon::SHIELD)
                        .font(theme::icon_font(14.0))
                        .color(pal.accent),
                    s.run_as_admin,
                ));
                if ui.add(b).on_hover_text(s.run_as_admin_tip).clicked() {
                    out.actions.push(Action::RunAsAdmin);
                    ui.close();
                }
            }
            ui.separator();
            let b = egui::Button::new((
                RichText::new(icon::HEART)
                    .font(theme::icon_font(13.0))
                    .color(crate::support::heart_color(&pal)),
                s.support_menu,
            ))
            .frame_when_inactive(false);
            if ui.add(b).clicked() {
                out.actions.push(Action::ShowSupport);
                ui.close();
            }
        });
}

pub struct StatusIn<'a> {
    pub tr: Tr,
    pub pal: Pal,
    pub status: &'a ScanStatus,
    pub method: Option<Method>,
    pub has_scan: bool,
    /// The process runs elevated (no "needs administrator" hint).
    pub elevated: bool,
    pub focus_size: u64,
    pub focus_files: u64,
    pub time: f64,
    /// Scans in memory but not shown that are still running: what they
    /// scan and the files found so far.
    pub background: &'a [(String, u64)],
}

/// Returns [`Action::CancelBackgroundScans`] when its stop button is
/// clicked.
pub fn status_bar(ui: &mut Ui, inp: &StatusIn) -> Vec<Action> {
    let (tr, pal) = (inp.tr, inp.pal);
    let s = tr.s;
    let st = inp.status;
    let mut actions = Vec::new();
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let font = FontId::proportional(12.5);
        let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
        if st.running {
            widgets::spinner(ui.painter(), rect.center(), 6.0, pal.accent, inp.time);
        } else if inp.has_scan {
            let color = if st.cancelled {
                pal.text_faint
            } else {
                Color32::from_rgb(0x2E, 0xA0, 0x43)
            };
            ui.painter().circle_filled(rect.center(), 4.0, color);
        }
        let state = if !inp.has_scan {
            s.st_idle
        } else if st.running {
            s.st_scanning
        } else if st.cancelled {
            s.st_cancelled
        } else {
            s.st_done
        };
        ui.label(
            RichText::new(state)
                .font(theme::semibold(12.5))
                .color(pal.text),
        );
        if inp.has_scan {
            let secs = st.elapsed.as_secs_f64();
            let rate = if secs > 0.05 {
                (st.files as f64 / secs) as u64
            } else {
                0
            };
            let mut parts = vec![
                tr.n_files(st.files),
                tr.n_folders(st.dirs),
                tr.size(st.bytes),
                tr.duration(st.elapsed.as_secs()),
            ];
            if let Some((done, total)) = st.mft {
                parts.push(tr.mft_progress(done, total));
            }
            if st.running && rate > 0 {
                parts.push(tr.files_per_sec(rate));
            }
            if st.errors > 0 {
                parts.push(tr.n_errors(st.errors));
            }
            ui.label(
                RichText::new(parts.join("  ·  "))
                    .font(font.clone())
                    .color(pal.text_dim),
            );
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if !inp.background.is_empty() {
                if small_icon_button(ui, icon::STOP, s.bg_stop_tip).clicked() {
                    actions.push(Action::CancelBackgroundScans);
                }
                let tip = inp
                    .background
                    .iter()
                    .map(|(name, files)| format!("{name} — {}", tr.n_files(*files)))
                    .collect::<Vec<_>>()
                    .join("\n");
                let n = inp.background.len() as u64;
                ui.label(
                    RichText::new(tr.n_background(n))
                        .font(font.clone())
                        .color(pal.text_dim),
                )
                .on_hover_text(tip);
                let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                widgets::spinner(ui.painter(), r.center(), 4.5, pal.text_dim, inp.time);
                let (r, _) = ui.allocate_exact_size(vec2(1.0, 14.0), Sense::hover());
                ui.painter()
                    .vline(r.center().x, r.y_range(), Stroke::new(1.0, pal.divider));
            }
            if inp.has_scan {
                let focus = format!(
                    "{}: {} · {}",
                    s.focus,
                    tr.size(inp.focus_size),
                    tr.n_files(inp.focus_files)
                );
                ui.label(RichText::new(focus).font(font.clone()).color(pal.text_dim));
                if let Some(m) = inp.method {
                    let text = match m {
                        Method::Walk { threads } => {
                            format!("{} · {}", s.method_walk, tr.threads(threads))
                        }
                        Method::Mixed { threads } => {
                            format!("{} · {}", s.method_mixed, tr.threads(threads))
                        }
                        Method::Mft => s.method_mft.to_string(),
                        Method::Pending => s.method_pending.to_string(),
                        Method::Demo => s.method_demo.to_string(),
                    };
                    let (r, _) = ui.allocate_exact_size(vec2(1.0, 14.0), Sense::hover());
                    ui.painter()
                        .vline(r.center().x, r.y_range(), Stroke::new(1.0, pal.divider));
                    let label =
                        ui.label(RichText::new(text).font(font.clone()).color(pal.text_dim));
                    let tip = method_tip(inp);
                    if !tip.is_empty() {
                        label.on_hover_text(tip);
                    }
                }
            }
            if st.running && !st.current.is_empty() {
                let (r, _) = ui.allocate_exact_size(vec2(1.0, 14.0), Sense::hover());
                ui.painter()
                    .vline(r.center().x, r.y_range(), Stroke::new(1.0, pal.divider));
                let room = ui.available_width() - 8.0;
                let cur = widgets::fit_text(ui, &st.current, &font, room.max(40.0), true);
                ui.label(RichText::new(cur).font(font.clone()).color(pal.text_faint));
            }
        });
    });
    actions
}

/// A 20 px icon button that fits the status bar.
fn small_icon_button(ui: &mut Ui, glyph: &str, tooltip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::click());
    let pal = theme::current(ui.ctx());
    let fill = if resp.is_pointer_button_down_on() {
        pal.card_pressed
    } else if resp.hovered() {
        pal.card_hover
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 4.0, fill);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        theme::icon_font(11.0),
        pal.text,
    );
    resp.on_hover_text(tooltip)
}

/// Hover text of the method label: the method per drive, the engine's note
/// on why the MFT was not used, and how to get the fast path.
fn method_tip(inp: &StatusIn) -> String {
    let s = inp.tr.s;
    let mut lines = Vec::new();
    let mut walked_drive = false;
    for u in &inp.status.units {
        let method = match u.method {
            Some(ringscan_engine::Method::Mft) => s.method_mft,
            Some(ringscan_engine::Method::Walk) => s.method_walk,
            _ => s.method_pending,
        };
        lines.push(format!("{} — {method}", u.name));
        if let Some(note) = &u.note {
            lines.push(format!("    {}", inp.tr.unit_note(note)));
            walked_drive |= u.method == Some(ringscan_engine::Method::Walk);
        }
    }
    if walked_drive && !inp.elevated {
        lines.push(String::new());
        lines.push(s.method_tip_admin.to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringscan_core::Lang;

    fn drive(letter: char) -> DriveCard {
        DriveCard {
            letter,
            label: String::new(),
            fs: "NTFS".into(),
            kind: DriveKind::Fixed,
            used: Some(300 << 30),
            total: Some(1 << 40),
            active: false,
            this_pc: true,
        }
    }

    /// The sidebar's probes after a few frames in a window of `h` points
    /// with the app's toolbar (44 pt) and status bar (30 pt).
    fn probes(drives: &[DriveCard], h: f32) -> Vec<(&'static str, Rect)> {
        let ctx = egui::Context::default();
        let inp = SidebarIn {
            tr: Tr::new(Lang::Uk),
            pal: theme::pal(false),
            drives,
            this_pc_active: true,
            recycle: Some((5 << 30, 1234)),
            logo: None,
            system_actions: true,
        };
        let mut probes = Vec::new();
        for _ in 0..3 {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(760.0, h))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                egui::Panel::top("toolbar")
                    .exact_size(44.0)
                    .show(ui, |_| {});
                egui::Panel::bottom("status")
                    .exact_size(30.0)
                    .show(ui, |_| {});
                probes = egui::Panel::left("sidebar")
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 4)))
                    .default_size(240.0)
                    .show(ui, |ui| sidebar(ui, &inp))
                    .inner
                    .probes;
            });
            out.textures_delta.clear();
        }
        probes
    }

    #[test]
    fn choose_folder_stays_clear_of_the_bottom_block() {
        // The minimum window height, with up to eight drive cards.
        for n in [0, 1, 2, 4, 8] {
            let drives: Vec<DriveCard> = "CDEFGHIJ".chars().take(n).map(drive).collect();
            for h in [520.0, 800.0] {
                let p = probes(&drives, h);
                let get = |name: &str| {
                    p.iter()
                        .find(|(k, _)| *k == name)
                        .map(|&(_, r)| r)
                        .unwrap_or_else(|| panic!("{n} drives, {h}: no {name}"))
                };
                let (pick, bin, link) = (get("choose-folder"), get("recycle-bin"), get("support"));
                assert!(
                    pick.bottom() + SIDEBAR_GAP <= bin.top() + 0.5,
                    "{n} drives, {h}: {pick:?} vs {bin:?}"
                );
                assert!(pick.height() >= PICK_H - 0.5, "{n} drives, {h}: {pick:?}");
                assert!(bin.bottom() <= link.top());
                assert!(link.bottom() <= h - 30.0, "{n} drives, {h}: {link:?}");
            }
        }
    }
}
