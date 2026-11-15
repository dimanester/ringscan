//! Confirmation dialogs for destructive actions, and toast notifications.
//!
//! The dialogs list the affected paths (up to five, then "…and N more")
//! and the total size. Keyboard focus starts on **Cancel**, so Enter
//! never destroys anything by accident; Esc and a click on the backdrop
//! cancel too. "Delete permanently" uses a red button and a warning line.

use std::path::PathBuf;

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Id, Layout, Rect, RichText, Stroke, StrokeKind, pos2,
    vec2,
};
use ringscan_core::NodeId;

use crate::i18n::Tr;
use crate::theme::{self, Pal, icon};
use crate::widgets;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmKind {
    Recycle,
    Delete,
    EmptyBin,
}

/// A pending confirmation.
#[derive(Clone, Debug)]
pub struct Confirm {
    pub kind: ConfirmKind,
    pub ids: Vec<NodeId>,
    pub paths: Vec<PathBuf>,
    pub total: u64,
    /// Frames shown so far (focus goes to Cancel on the first one).
    pub frames: u32,
    /// Screen rects of the confirm and cancel buttons as last drawn (test
    /// harness probes `@ok` / `@cancel`).
    pub ok_rect: Option<Rect>,
    pub cancel_rect: Option<Rect>,
}

impl Confirm {
    pub fn new(kind: ConfirmKind, ids: Vec<NodeId>, paths: Vec<PathBuf>, total: u64) -> Confirm {
        Confirm {
            kind,
            ids,
            paths,
            total,
            frames: 0,
            ok_rect: None,
            cancel_rect: None,
        }
    }
}

/// Result of showing a dialog this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Pending,
    Cancel,
    Confirm,
}

pub fn show_confirm(ctx: &egui::Context, c: &mut Confirm, tr: Tr, pal: &Pal) -> Answer {
    let s = tr.s;
    let mut answer = Answer::Pending;
    let first = c.frames == 0;
    c.frames += 1;
    let (title, glyph, accent) = match c.kind {
        ConfirmKind::Recycle => (s.dlg_recycle_title, icon::DELETE, pal.accent),
        ConfirmKind::Delete => (s.dlg_delete_title, icon::WARNING, pal.danger),
        ConfirmKind::EmptyBin => (s.dlg_empty_title, icon::DELETE, pal.danger),
    };
    let modal = egui::Modal::new(Id::unique("confirm-dialog"))
        .backdrop_color(Color32::from_black_alpha(if pal.dark { 120 } else { 70 }))
        .frame(
            egui::Frame::new()
                .fill(pal.card)
                .stroke(Stroke::new(1.0, pal.border))
                .corner_radius(8.0)
                .inner_margin(egui::Margin::same(24))
                .shadow(egui::Shadow {
                    offset: [0, 8],
                    blur: 32,
                    spread: 0,
                    color: Color32::from_black_alpha(if pal.dark { 120 } else { 60 }),
                }),
        );
    let resp = modal.show(ctx, |ui| {
        ui.set_width(460.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(glyph)
                    .font(theme::icon_font(20.0))
                    .color(accent),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(title)
                    .font(theme::semibold(20.0))
                    .color(pal.text),
            );
        });
        ui.add_space(12.0);
        match c.kind {
            ConfirmKind::EmptyBin => {
                ui.label(RichText::new(s.dlg_empty_text).color(pal.text));
                if c.total > 0 {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(format!("{}: {}", s.total, tr.size(c.total)))
                            .color(pal.text_dim),
                    );
                }
            }
            _ => {
                // Path list in a sunken box.
                let shown = c.paths.len().min(5);
                egui::Frame::new()
                    .fill(if pal.dark {
                        Color32::from_rgb(0x24, 0x24, 0x24)
                    } else {
                        Color32::from_rgb(0xF5, 0xF5, 0xF5)
                    })
                    .corner_radius(6.0)
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        for p in &c.paths[..shown] {
                            let text = p.display().to_string();
                            let font = FontId::proportional(13.0);
                            let fitted =
                                widgets::fit_text(ui, &text, &font, ui.available_width(), true);
                            ui.label(RichText::new(fitted).font(font).color(pal.text));
                        }
                        if c.paths.len() > shown {
                            ui.label(
                                RichText::new(tr.and_more((c.paths.len() - shown) as u64))
                                    .color(pal.text_dim),
                            );
                        }
                    });
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("{}: {}", s.total, tr.size(c.total)))
                        .font(theme::semibold(14.0)),
                );
                if c.kind == ConfirmKind::Delete {
                    ui.add_space(6.0);
                    ui.label(RichText::new(s.dlg_delete_warning).color(pal.danger));
                }
            }
        }
        ui.add_space(20.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let cancel = widgets::plain_button(ui, s.cancel);
            let ok = match c.kind {
                ConfirmKind::Recycle => widgets::filled_button(
                    ui,
                    s.btn_recycle,
                    pal.accent,
                    pal.accent_hover,
                    pal.on_accent,
                ),
                ConfirmKind::Delete => widgets::filled_button(
                    ui,
                    s.btn_delete,
                    pal.danger,
                    pal.danger_hover,
                    pal.on_danger,
                ),
                ConfirmKind::EmptyBin => widgets::filled_button(
                    ui,
                    s.btn_empty,
                    pal.danger,
                    pal.danger_hover,
                    pal.on_danger,
                ),
            };
            c.ok_rect = Some(ok.rect);
            c.cancel_rect = Some(cancel.rect);
            if first {
                cancel.request_focus();
            }
            if cancel.clicked() {
                answer = Answer::Cancel;
            }
            if ok.clicked() {
                answer = Answer::Confirm;
            }
        });
    });
    if resp.should_close() && answer == Answer::Pending {
        answer = Answer::Cancel;
    }
    answer
}

/// A short message in the bottom-right corner.
pub struct Toast {
    pub text: String,
    pub error: bool,
    pub born: f64,
}

#[derive(Default)]
pub struct Toasts {
    pub items: Vec<Toast>,
}

const TOAST_SECS: f64 = 4.0;

impl Toasts {
    pub fn info(&mut self, text: impl Into<String>, now: f64) {
        self.push(text.into(), false, now);
    }

    pub fn error(&mut self, text: impl Into<String>, now: f64) {
        self.push(text.into(), true, now);
    }

    fn push(&mut self, text: String, error: bool, now: f64) {
        self.items.retain(|t| t.text != text);
        self.items.push(Toast {
            text,
            error,
            born: now,
        });
        if self.items.len() > 4 {
            self.items.remove(0);
        }
    }

    /// Paints the toasts above `area`'s bottom-right corner.
    pub fn show(&mut self, ctx: &egui::Context, area: Rect, pal: &Pal, now: f64) {
        self.items.retain(|t| now - t.born < TOAST_SECS);
        if self.items.is_empty() {
            return;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Tooltip,
            Id::unique("toasts"),
        ));
        let mut y = area.bottom() - 16.0;
        for t in self.items.iter().rev() {
            let age = now - t.born;
            let alpha = (((TOAST_SECS - age) / 0.3).min(age / 0.15)).clamp(0.0, 1.0) as f32;
            let font = FontId::proportional(13.5);
            let galley = painter.layout(t.text.clone(), font, pal.text, 360.0);
            let size = galley.size() + vec2(48.0, 22.0);
            let rect = Rect::from_min_size(pos2(area.right() - 16.0 - size.x, y - size.y), size);
            painter.rect(
                rect.translate(vec2(0.0, 3.0)),
                8.0,
                pal.shadow.gamma_multiply(alpha * 0.6),
                Stroke::NONE,
                StrokeKind::Inside,
            );
            painter.rect(
                rect,
                8.0,
                pal.card.gamma_multiply(alpha),
                Stroke::new(1.0, pal.border.gamma_multiply(alpha)),
                StrokeKind::Inside,
            );
            let (glyph, color) = if t.error {
                (icon::WARNING, pal.danger)
            } else {
                (icon::INFO, pal.accent)
            };
            painter.text(
                pos2(rect.left() + 20.0, rect.center().y),
                Align2::CENTER_CENTER,
                glyph,
                theme::icon_font(15.0),
                color.gamma_multiply(alpha),
            );
            painter.galley(
                pos2(rect.left() + 36.0, rect.top() + 11.0),
                galley,
                pal.text.gamma_multiply(alpha),
            );
            y = rect.top() - 8.0;
        }
    }
}
