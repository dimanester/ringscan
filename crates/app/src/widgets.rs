//! Small custom widgets in the WinUI 3 style: icon buttons, segmented
//! controls, a tab strip and progress bars. egui's stock widgets are used
//! where they already look right (text edit, checkbox, slider, menus).

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Rect, Response, Sense, Stroke, StrokeKind, Ui,
    pos2, vec2,
};

use crate::theme::{self, Pal};

/// A square, frameless button with one icon glyph; hover and press fills.
pub fn icon_button(ui: &mut Ui, icon: &str, tooltip: &str, enabled: bool) -> Response {
    let size = vec2(34.0, 30.0);
    let (rect, resp) = ui.allocate_exact_size(
        size,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let pal = theme::current(ui.ctx());
    if ui.is_rect_visible(rect) {
        let fill = if !enabled {
            Color32::TRANSPARENT
        } else if resp.is_pointer_button_down_on() {
            pal.card_pressed
        } else if resp.hovered() {
            pal.card_hover
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, 4.0, fill);
        let color = if enabled {
            pal.text
        } else {
            pal.text_faint.gamma_multiply(0.7)
        };
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            icon,
            theme::icon_font(16.0),
            color,
        );
    }
    if enabled {
        resp.on_hover_text(tooltip)
    } else {
        resp.on_disabled_hover_text(tooltip)
    }
}

/// Segmented control: one row of options, the selected one on a raised
/// thumb. Returns true when the selection changed.
pub fn segmented(
    ui: &mut Ui,
    id: &str,
    options: &[&str],
    selected: &mut usize,
    tooltip: &str,
) -> bool {
    let pal = theme::current(ui.ctx());
    let font = FontId::proportional(13.0);
    let pad = 11.0;
    let widths: Vec<f32> = options
        .iter()
        .map(|o| {
            ui.painter()
                .layout_no_wrap(o.to_string(), font.clone(), pal.text)
                .size()
                .x
                + 2.0 * pad
        })
        .collect();
    let total = widths.iter().sum::<f32>() + 4.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(total, 30.0), Sense::hover());
    resp.on_hover_text(tooltip);
    let painter = ui.painter_at(rect.expand(2.0));
    painter.rect(
        rect,
        CornerRadius::same(6),
        if pal.dark {
            Color32::from_rgb(0x1F, 0x1F, 0x1F)
        } else {
            Color32::from_rgb(0xEC, 0xEC, 0xEC)
        },
        Stroke::new(1.0, pal.divider),
        StrokeKind::Inside,
    );
    let mut x = rect.left() + 2.0;
    let mut changed = false;
    // Animated thumb position.
    let target_x: f32 =
        rect.left() + 2.0 + widths[..(*selected).min(widths.len())].iter().sum::<f32>();
    let thumb_x =
        ui.ctx()
            .animate_value_with_time(ui.scope_id().with(id).with("thumb"), target_x, 0.12);
    let thumb_w = widths.get(*selected).copied().unwrap_or(0.0);
    let thumb = Rect::from_min_size(
        pos2(thumb_x, rect.top() + 2.0),
        vec2(thumb_w, rect.height() - 4.0),
    );
    painter.rect(
        thumb,
        CornerRadius::same(4),
        pal.card,
        Stroke::new(
            1.0,
            if pal.dark {
                Color32::from_rgb(0x45, 0x45, 0x45)
            } else {
                Color32::from_rgb(0xD6, 0xD6, 0xD6)
            },
        ),
        StrokeKind::Inside,
    );
    for (i, (opt, w)) in options.iter().zip(&widths).enumerate() {
        let r = Rect::from_min_size(pos2(x, rect.top()), vec2(*w, rect.height()));
        let item = ui.interact(r, ui.scope_id().with(id).with(i), Sense::click());
        if item.clicked() && *selected != i {
            *selected = i;
            changed = true;
        }
        let color = if i == *selected || item.hovered() {
            pal.text
        } else {
            pal.text_dim
        };
        let f = if i == *selected {
            theme::semibold(13.0)
        } else {
            font.clone()
        };
        painter.text(r.center(), Align2::CENTER_CENTER, *opt, f, color);
        x += w;
    }
    changed
}

/// Underlined tab strip ("SelectorBar"). Returns true on change.
pub fn tabs(ui: &mut Ui, labels: &[&str], selected: &mut usize) -> bool {
    let pal = theme::current(ui.ctx());
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for (i, l) in labels.iter().enumerate() {
            let sel = i == *selected;
            let font = if sel {
                theme::semibold(13.5)
            } else {
                FontId::proportional(13.5)
            };
            let galley = ui.painter().layout_no_wrap(
                l.to_string(),
                font,
                if sel { pal.text } else { pal.text_dim },
            );
            let size = vec2(galley.size().x + 20.0, 34.0);
            let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
            if resp.hovered() && !sel {
                ui.painter()
                    .rect_filled(rect.shrink2(vec2(0.0, 3.0)), 4.0, pal.card_hover);
            }
            let pos = rect.center() - galley.size() * 0.5 - vec2(0.0, 1.0);
            let color = if sel || resp.hovered() {
                pal.text
            } else {
                pal.text_dim
            };
            ui.painter().galley(pos, galley, color);
            if sel {
                let w = (rect.width() - 24.0).clamp(12.0, 40.0);
                let bar = Rect::from_center_size(
                    pos2(rect.center().x, rect.bottom() - 2.5),
                    vec2(w, 3.0),
                );
                ui.painter().rect_filled(bar, 1.5, pal.accent);
            }
            if resp.clicked() && !sel {
                *selected = i;
                changed = true;
            }
        }
    });
    changed
}

/// WinUI-style toggle switch with a label on the right. Returns true when
/// toggled.
pub fn toggle(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let pal = theme::current(ui.ctx());
    let font = FontId::proportional(14.0);
    let galley = ui.painter().layout(
        label.to_string(),
        font,
        pal.text,
        ui.available_width() - 56.0,
    );
    let h = galley.size().y.max(22.0);
    let (rect, mut resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), h + 4.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.12);
    let track = Rect::from_min_size(pos2(rect.left(), rect.center().y - 10.0), vec2(40.0, 20.0));
    let p = ui.painter();
    if *on {
        let fill = if resp.hovered() {
            pal.accent_hover
        } else {
            pal.accent
        };
        p.rect_filled(track, 10.0, fill);
    } else {
        let stroke = if pal.dark {
            Color32::from_rgb(0x9E, 0x9E, 0x9E)
        } else {
            Color32::from_rgb(0x86, 0x86, 0x86)
        };
        let fill = if resp.hovered() {
            pal.card_hover
        } else {
            Color32::TRANSPARENT
        };
        p.rect(
            track,
            10.0,
            fill,
            Stroke::new(1.0, stroke),
            StrokeKind::Inside,
        );
    }
    let knob_x = egui::lerp(track.left() + 10.0..=track.right() - 10.0, t);
    let knob_color = if *on {
        pal.on_accent
    } else if pal.dark {
        Color32::from_rgb(0xCF, 0xCF, 0xCF)
    } else {
        Color32::from_rgb(0x5D, 0x5D, 0x5D)
    };
    let r = if resp.hovered() { 7.0 } else { 6.0 };
    p.circle_filled(pos2(knob_x, track.center().y), r, knob_color);
    p.galley(
        pos2(
            track.right() + 12.0,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        pal.text,
    );
    resp
}

/// Horizontal bar: track plus a filled fraction.
pub fn bar(painter: &egui::Painter, rect: Rect, frac: f32, fill: Color32, track: Color32) {
    let r = rect.height() * 0.5;
    painter.rect_filled(rect, r, track);
    let f = frac.clamp(0.0, 1.0);
    if f > 0.0 {
        let w = (rect.width() * f).max(rect.height().min(rect.width()));
        painter.rect_filled(
            Rect::from_min_size(rect.min, vec2(w, rect.height())),
            r,
            fill,
        );
    }
}

/// An indeterminate spinner (rotating arc).
pub fn spinner(
    painter: &egui::Painter,
    center: egui::Pos2,
    radius: f32,
    color: Color32,
    time: f64,
) {
    let start = (time * 4.0) as f32;
    let sweep = 1.6 + 0.9 * ((time * 2.1) as f32).sin();
    let n = 24;
    let pts: Vec<egui::Pos2> = (0..=n)
        .map(|k| {
            let a = start + sweep * k as f32 / n as f32;
            center + vec2(a.cos(), a.sin()) * radius
        })
        .collect();
    painter.add(egui::Shape::line(pts, Stroke::new(2.0, color)));
}

/// Primary (accent) or danger button with fixed min width.
pub fn filled_button(
    ui: &mut Ui,
    text: &str,
    fill: Color32,
    hover: Color32,
    fg: Color32,
) -> Response {
    let font = FontId::proportional(14.0);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, fg);
    let size = vec2((galley.size().x + 28.0).max(110.0), 32.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let fill = if resp.hovered() { hover } else { fill };
    ui.painter().rect_filled(rect, 4.0, fill);
    if resp.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            6.0,
            Stroke::new(2.0, fg_focus(ui)),
            StrokeKind::Outside,
        );
    }
    ui.painter()
        .galley(rect.center() - galley.size() * 0.5, galley, fg);
    resp
}

/// Standard (neutral) dialog button that shows a focus ring.
pub fn plain_button(ui: &mut Ui, text: &str) -> Response {
    let pal = theme::current(ui.ctx());
    let font = FontId::proportional(14.0);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), font, pal.text);
    let size = vec2((galley.size().x + 28.0).max(110.0), 32.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let fill = if resp.is_pointer_button_down_on() {
        pal.card_pressed
    } else if resp.hovered() {
        pal.card_hover
    } else {
        pal.card
    };
    ui.painter().rect(
        rect,
        4.0,
        fill,
        Stroke::new(1.0, pal.border),
        StrokeKind::Inside,
    );
    if resp.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            6.0,
            Stroke::new(2.0, fg_focus(ui)),
            StrokeKind::Outside,
        );
    }
    ui.painter()
        .galley(rect.center() - galley.size() * 0.5, galley, pal.text);
    resp
}

fn fg_focus(ui: &Ui) -> Color32 {
    theme::current(ui.ctx()).text
}

/// Small uppercase-ish section caption.
pub fn caption(ui: &mut Ui, text: &str) {
    let pal: Pal = theme::current(ui.ctx());
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(text)
            .font(theme::semibold(12.0))
            .color(pal.text_dim),
    );
    ui.add_space(2.0);
}

/// Truncates `text` with "…" to fit `max_w` points in `font` (keeps the end
/// when `keep_end`, e.g. for paths).
pub fn fit_text(ui: &Ui, text: &str, font: &FontId, max_w: f32, keep_end: bool) -> String {
    let painter = ui.painter();
    let w = |s: &str| {
        painter
            .layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE)
            .size()
            .x
    };
    if w(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    // Largest n such that the shortened text fits.
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let s = shorten(&chars, mid, keep_end);
        if w(&s) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    shorten(&chars, lo, keep_end)
}

fn shorten(chars: &[char], n: usize, keep_end: bool) -> String {
    if n >= chars.len() {
        return chars.iter().collect();
    }
    if keep_end {
        let mut s = String::from("…");
        s.extend(&chars[chars.len() - n..]);
        s
    } else {
        let mut s: String = chars[..n].iter().collect();
        s.push('…');
        s
    }
}
