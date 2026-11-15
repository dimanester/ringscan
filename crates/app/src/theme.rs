//! Look and feel: Windows 11 fonts, icon glyphs, colors and egui styles.
//!
//! * **Fonts.** On Windows the UI uses the system's own *Segoe UI Variable*
//!   (`%WINDIR%\Fonts\SegUIVar.ttf`, weight axis at 400 for text and 600 for
//!   headings) and falls back to *Segoe UI*; the files are read at start-up,
//!   nothing is bundled. Icons come from *Segoe Fluent Icons* (Windows 11)
//!   or *Segoe MDL2 Assets* (Windows 10), which share code points. egui's
//!   bundled fonts stay in the fallback chain, and eframe's system font
//!   provider covers any other script on demand.
//! * **Styles.** One [`egui::Style`] per theme, set with `set_style_of`, so
//!   following the Windows light/dark setting is egui's job; [`Pal`] holds
//!   the matching colors for custom-painted widgets. The palette follows
//!   the WinUI 3 defaults (layer fills, 1-px strokes, accent `#005FB8` /
//!   `#60CDFF`, 4 px control and 8 px overlay corners).

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow,
    Stroke, Style, TextStyle, Theme, Visuals, epaint::text::FontTweak,
    epaint::text::VariationCoords,
};

use crate::settings::ThemePref;

/// Font family with the semibold weight (headings, emphasis).
pub const SEMIBOLD: &str = "semibold";
/// Font family of the icon glyphs.
pub const ICONS: &str = "icons";

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICONS.into()))
}

/// Segoe Fluent Icons / MDL2 Assets code points.
pub mod icon {
    pub const BACK: &str = "\u{E72B}";
    pub const UP: &str = "\u{E74A}";
    pub const REFRESH: &str = "\u{E72C}";
    pub const STOP: &str = "\u{E71A}";
    pub const SEARCH: &str = "\u{E721}";
    pub const CANCEL: &str = "\u{E711}";
    pub const SETTINGS: &str = "\u{E713}";
    pub const FOLDER: &str = "\u{E8B7}";
    pub const FOLDER_OPEN: &str = "\u{E838}";
    pub const DOCUMENT: &str = "\u{E7C3}";
    pub const DRIVE: &str = "\u{EDA2}";
    pub const PC: &str = "\u{E7F8}";
    pub const DELETE: &str = "\u{E74D}";
    pub const APPS: &str = "\u{E71D}";
    pub const SHIELD: &str = "\u{EA18}";
    pub const CHEVRON_RIGHT: &str = "\u{E76C}";
    pub const ZOOM_IN: &str = "\u{E8A3}";
    pub const OPEN: &str = "\u{E8A7}";
    pub const COPY: &str = "\u{E8C8}";
    pub const INFO: &str = "\u{E946}";
    pub const PANEL_LEFT: &str = "\u{E8A0}";
    pub const PANEL_RIGHT: &str = "\u{E89F}";
    pub const SORT_UP: &str = "\u{E70E}";
    pub const SORT_DOWN: &str = "\u{E70D}";
    pub const WARNING: &str = "\u{E7BA}";
    pub const EXPLORER: &str = "\u{EC50}";
    pub const PIE: &str = "\u{EB05}";
}

/// Colors for custom-painted widgets, per theme.
#[derive(Clone, Copy, Debug)]
pub struct Pal {
    pub dark: bool,
    /// Content layer (behind the chart and the details panel).
    pub bg: Color32,
    /// Window background ("Mica"): toolbar, sidebar, status bar.
    pub side: Color32,
    /// Hairline on the top and left edges of the content layer.
    pub layer_border: Color32,
    /// Cards and list backgrounds.
    pub card: Color32,
    pub card_hover: Color32,
    pub card_pressed: Color32,
    pub border: Color32,
    pub divider: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub text_faint: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub on_accent: Color32,
    /// Selected row background.
    pub selection: Color32,
    /// Selected navigation item (sidebar) background.
    pub nav_selected: Color32,
    pub danger: Color32,
    pub danger_hover: Color32,
    pub on_danger: Color32,
    pub bar_track: Color32,
    pub bar_fill: Color32,
    pub bar_warn: Color32,
    /// Center disc of the chart.
    pub disc: Color32,
    pub disc_hover: Color32,
    pub shadow: Color32,
}

pub fn pal(dark: bool) -> Pal {
    if dark {
        Pal {
            dark,
            bg: Color32::from_rgb(0x28, 0x28, 0x28),
            side: Color32::from_rgb(0x20, 0x20, 0x20),
            layer_border: Color32::from_rgb(0x1A, 0x1A, 0x1A),
            card: Color32::from_rgb(0x2B, 0x2B, 0x2B),
            card_hover: Color32::from_rgb(0x32, 0x32, 0x32),
            card_pressed: Color32::from_rgb(0x27, 0x27, 0x27),
            border: Color32::from_rgb(0x3A, 0x3A, 0x3A),
            divider: Color32::from_rgb(0x2F, 0x2F, 0x2F),
            text: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            text_dim: Color32::from_rgb(0xC5, 0xC5, 0xC5),
            text_faint: Color32::from_rgb(0x8A, 0x8A, 0x8A),
            accent: Color32::from_rgb(0x60, 0xCD, 0xFF),
            accent_hover: Color32::from_rgb(0x5A, 0xBD, 0xEA),
            on_accent: Color32::from_rgb(0x00, 0x00, 0x00),
            selection: Color32::from_rgb(0x2E, 0x3B, 0x47),
            nav_selected: Color32::from_rgb(0x2D, 0x2D, 0x2D),
            danger: Color32::from_rgb(0xE8, 0x4A, 0x4A),
            danger_hover: Color32::from_rgb(0xF0, 0x5E, 0x5E),
            on_danger: Color32::WHITE,
            bar_track: Color32::from_rgb(0x3A, 0x3A, 0x3A),
            bar_fill: Color32::from_rgb(0x60, 0xCD, 0xFF),
            bar_warn: Color32::from_rgb(0xFF, 0x99, 0xA4),
            disc: Color32::from_rgb(0x2E, 0x2F, 0x33),
            disc_hover: Color32::from_rgb(0x38, 0x3A, 0x3F),
            shadow: Color32::from_black_alpha(90),
        }
    } else {
        Pal {
            dark,
            bg: Color32::from_rgb(0xF9, 0xF9, 0xF9),
            side: Color32::from_rgb(0xF3, 0xF3, 0xF3),
            layer_border: Color32::from_rgb(0xE5, 0xE5, 0xE5),
            card: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            card_hover: Color32::from_rgb(0xF5, 0xF5, 0xF5),
            card_pressed: Color32::from_rgb(0xEE, 0xEE, 0xEE),
            border: Color32::from_rgb(0xE1, 0xE1, 0xE1),
            divider: Color32::from_rgb(0xE8, 0xE8, 0xE8),
            text: Color32::from_rgb(0x1A, 0x1A, 0x1A),
            text_dim: Color32::from_rgb(0x5C, 0x5C, 0x5C),
            text_faint: Color32::from_rgb(0x8E, 0x8E, 0x8E),
            accent: Color32::from_rgb(0x00, 0x5F, 0xB8),
            accent_hover: Color32::from_rgb(0x19, 0x6E, 0xBF),
            on_accent: Color32::WHITE,
            selection: Color32::from_rgb(0xDD, 0xEB, 0xF8),
            nav_selected: Color32::from_rgb(0xE8, 0xE8, 0xE8),
            danger: Color32::from_rgb(0xC4, 0x2B, 0x1C),
            danger_hover: Color32::from_rgb(0xB0, 0x27, 0x19),
            on_danger: Color32::WHITE,
            bar_track: Color32::from_rgb(0xE3, 0xE3, 0xE3),
            bar_fill: Color32::from_rgb(0x00, 0x67, 0xC0),
            bar_warn: Color32::from_rgb(0xC4, 0x2B, 0x1C),
            disc: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            disc_hover: Color32::from_rgb(0xF3, 0xF6, 0xFA),
            shadow: Color32::from_black_alpha(40),
        }
    }
}

/// The palette of the theme egui currently shows.
pub fn current(ctx: &egui::Context) -> Pal {
    pal(ctx.theme() == Theme::Dark)
}

fn windows_font(file: &str) -> Option<Arc<Vec<u8>>> {
    let windir = std::env::var_os("WINDIR").or_else(|| std::env::var_os("SystemRoot"))?;
    let path = PathBuf::from(windir).join("Fonts").join(file);
    std::fs::read(path).ok().map(Arc::new)
}

/// Which UI font was found (reported by the harness log).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum UiFont {
    SegoeUiVariable,
    SegoeUi,
    EguiDefault,
}

/// Installs fonts and both theme styles. Returns the UI font in use.
pub fn install(ctx: &egui::Context) -> UiFont {
    let mut defs = FontDefinitions::default();
    let fallbacks = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut ui_font = UiFont::EguiDefault;
    let mut regular: Option<String> = None;
    let mut bold: Option<String> = None;

    if let Some(var) = windows_font("SegUIVar.ttf") {
        let blob: eframe::epaint::text::Blob = var;
        let tweak = |wght: f32| FontTweak {
            coords: VariationCoords::new([(b"wght", wght)]),
            ..FontTweak::default()
        };
        defs.font_data.insert(
            "segoe-var".into(),
            Arc::new(FontData::from_blob(blob.clone(), 0).tweak(tweak(400.0))),
        );
        defs.font_data.insert(
            "segoe-var-sb".into(),
            Arc::new(FontData::from_blob(blob, 0).tweak(tweak(600.0))),
        );
        regular = Some("segoe-var".into());
        bold = Some("segoe-var-sb".into());
        ui_font = UiFont::SegoeUiVariable;
    } else if let Some(ui) = windows_font("segoeui.ttf") {
        defs.font_data
            .insert("segoe".into(), Arc::new(FontData::from_blob(ui, 0)));
        regular = Some("segoe".into());
        if let Some(sb) = windows_font("seguisb.ttf") {
            defs.font_data
                .insert("segoe-sb".into(), Arc::new(FontData::from_blob(sb, 0)));
            bold = Some("segoe-sb".into());
        }
        ui_font = UiFont::SegoeUi;
    }
    let icons = windows_font("SegoeIcons.ttf").or_else(|| windows_font("segmdl2.ttf"));
    if let Some(icons) = icons {
        defs.font_data.insert(
            "fluent-icons".into(),
            Arc::new(FontData::from_blob(icons, 0).tweak(FontTweak {
                // The glyphs sit on the baseline; center them on the text.
                y_offset_factor: 0.08,
                ..FontTweak::default()
            })),
        );
    }

    let mut prop = fallbacks.clone();
    if let Some(r) = &regular {
        prop.insert(0, r.clone());
    }
    let mut sb = fallbacks.clone();
    if let Some(b) = bold.as_ref().or(regular.as_ref()) {
        sb.insert(0, b.clone());
    }
    let mut ic = prop.clone();
    if defs.font_data.contains_key("fluent-icons") {
        ic.insert(0, "fluent-icons".into());
    }
    defs.families.insert(FontFamily::Proportional, prop);
    defs.families.insert(FontFamily::Name(SEMIBOLD.into()), sb);
    defs.families.insert(FontFamily::Name(ICONS.into()), ic);
    ctx.set_fonts(defs);

    ctx.set_style_of(Theme::Light, style(false));
    ctx.set_style_of(Theme::Dark, style(true));
    ui_font
}

pub fn apply_theme_pref(ctx: &egui::Context, pref: ThemePref) {
    ctx.set_theme(match pref {
        ThemePref::System => egui::ThemePreference::System,
        ThemePref::Light => egui::ThemePreference::Light,
        ThemePref::Dark => egui::ThemePreference::Dark,
    });
}

fn style(dark: bool) -> Style {
    let p = pal(dark);
    let mut s = Style {
        visuals: if dark {
            Visuals::dark()
        } else {
            Visuals::light()
        },
        ..Style::default()
    };
    s.text_styles = [
        (TextStyle::Small, FontId::proportional(12.0)),
        (TextStyle::Body, FontId::proportional(14.0)),
        (TextStyle::Button, FontId::proportional(14.0)),
        (TextStyle::Heading, semibold(20.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ]
    .into();

    let sp = &mut s.spacing;
    sp.item_spacing = egui::vec2(8.0, 6.0);
    sp.button_padding = egui::vec2(11.0, 5.0);
    sp.interact_size = egui::vec2(32.0, 30.0);
    sp.menu_margin = Margin::same(6);
    sp.window_margin = Margin::same(16);
    sp.icon_width = 18.0;
    sp.icon_width_inner = 10.0;
    sp.icon_spacing = 8.0;
    sp.combo_height = 300.0;
    sp.tooltip_width = 420.0;
    sp.menu_width = 260.0;
    sp.slider_width = 150.0;
    sp.scroll.bar_width = 8.0;
    sp.scroll.floating_width = 3.0;

    s.interaction.tooltip_delay = 0.35;
    s.interaction.selectable_labels = false;

    let v = &mut s.visuals;
    let r4 = CornerRadius::same(4);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_fill = p.card;
    v.panel_fill = p.bg;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.extreme_bg_color = if dark {
        Color32::from_rgb(0x1E, 0x1E, 0x1E)
    } else {
        Color32::WHITE
    };
    v.text_edit_bg_color = Some(if dark {
        Color32::from_rgb(0x2D, 0x2D, 0x2D)
    } else {
        Color32::WHITE
    });
    v.faint_bg_color = if dark {
        Color32::from_rgb(0x25, 0x25, 0x25)
    } else {
        Color32::from_rgb(0xF6, 0xF6, 0xF6)
    };
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = if dark {
        Color32::from_rgb(0x1E, 0x5A, 0x7A)
    } else {
        Color32::from_rgb(0xCC, 0xE4, 0xF7)
    };
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: p.shadow,
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: p.shadow,
    };
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.striped = false;
    v.indent_has_left_vline = false;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.card;
    w.noninteractive.weak_bg_fill = p.card;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.divider);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r4;

    let (inactive, hovered, active) = if dark {
        (
            Color32::from_rgb(0x2D, 0x2D, 0x2D),
            Color32::from_rgb(0x32, 0x32, 0x32),
            Color32::from_rgb(0x27, 0x27, 0x27),
        )
    } else {
        (
            Color32::from_rgb(0xFB, 0xFB, 0xFB),
            Color32::from_rgb(0xF4, 0xF4, 0xF4),
            Color32::from_rgb(0xEE, 0xEE, 0xEE),
        )
    };
    let stroke = if dark {
        Color32::from_rgb(0x3C, 0x3C, 0x3C)
    } else {
        Color32::from_rgb(0xDD, 0xDD, 0xDD)
    };
    w.inactive.bg_fill = inactive;
    w.inactive.weak_bg_fill = inactive;
    w.inactive.bg_stroke = Stroke::new(1.0, stroke);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r4;
    w.inactive.expansion = 0.0;
    w.hovered.bg_fill = hovered;
    w.hovered.weak_bg_fill = hovered;
    w.hovered.bg_stroke = Stroke::new(1.0, stroke);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.corner_radius = r4;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = active;
    w.active.weak_bg_fill = active;
    w.active.bg_stroke = Stroke::new(1.0, stroke);
    w.active.fg_stroke = Stroke::new(1.0, p.text_dim);
    w.active.corner_radius = r4;
    w.active.expansion = 0.0;
    w.open.bg_fill = active;
    w.open.weak_bg_fill = active;
    w.open.bg_stroke = Stroke::new(1.0, stroke);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = r4;
    s
}
