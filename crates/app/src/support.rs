//! "Support RingScan": the donation channels and the dialog that lists
//! them.
//!
//! The dialog opens only on request (a small heart link at the bottom of
//! the sidebar, or the settings menu); there are no reminders. The app
//! makes no network requests for it: the Monobank button opens the jar in
//! the default browser through the shell thread (`platform::open_url`),
//! and the QR codes of the crypto addresses are drawn locally with
//! `qrcodegen` (black on white with the standard quiet zone, in both
//! themes, so phone cameras read them).

use eframe::egui::{
    self, Align, Align2, Color32, ColorImage, FontId, Id, Layout, Rect, RichText, Sense, Stroke,
    StrokeKind, TextFormat, TextureHandle, TextureOptions, Ui, pos2, text::LayoutJob, vec2,
};
use qrcodegen::{QrCode, QrCodeEcc};

use crate::actions::Action;
use crate::i18n::Tr;
use crate::theme::{self, Pal, icon};
use crate::widgets;

/// Monobank jar: card, Apple Pay or Google Pay from any country (in UAH).
pub const JAR_URL: &str = "https://send.monobank.ua/jar/4w6egVrfe2";
/// All channels on one page (the account-wide `DONATE.md`).
pub const DONATE_URL: &str = "https://github.com/dimanester/.github/blob/main/DONATE.md";
/// USDT on the TRON network (TRC-20) only.
pub const USDT_TRC20: &str = "TABWnxDWAZURgjxQS9FSzyAAzznAx3Nhc8";
/// Bitcoin on the Bitcoin network only (native SegWit).
pub const BTC: &str = "bc1qhvzfzvp7sqtk2dg3qmw75ugk2h89ulmw792wyf";

/// Quiet zone around a QR code, in modules (the standard asks for 4).
const QUIET: usize = 4;
/// Points per QR module on screen (148 pt for a version 3 code).
const MODULE_PT: f32 = 4.0;
/// Points per QR module in the compact layout (111 pt, still far above
/// what phone cameras need).
const MODULE_PT_COMPACT: f32 = 3.0;
/// Below this window height (pt) the dialog uses the compact layout:
/// smaller QR codes, tighter spacing, no introductory paragraph.
const COMPACT_BELOW: f32 = 640.0;
/// Inner margin of the dialog frame.
const MARGIN: f32 = 24.0;
/// Free space kept between the dialog and the window edges.
const OUTER_GAP: f32 = 8.0;
/// Height reserved for the bottom row ("All options" link, Close).
const BOTTOM_ROW: f32 = 16.0 + 32.0;
/// Monospace size of the addresses.
const ADDRESS_PT: f32 = 11.5;

/// The QR code of `text`, one pixel per module, black on white with the
/// quiet zone.
pub fn qr_image(text: &str) -> ColorImage {
    let qr = QrCode::encode_text(text, QrCodeEcc::Medium).expect("an address fits a QR code");
    let n = qr.size() as usize;
    let side = n + 2 * QUIET;
    let mut px = vec![Color32::WHITE; side * side];
    for y in 0..n {
        for x in 0..n {
            if qr.get_module(x as i32, y as i32) {
                px[(y + QUIET) * side + x + QUIET] = Color32::BLACK;
            }
        }
    }
    ColorImage::new([side, side], px)
}

/// Dialog state.
#[derive(Default)]
pub struct Support {
    pub open: bool,
    /// QR textures of [`USDT_TRC20`] and [`BTC`], made on first show.
    qr: Option<[TextureHandle; 2]>,
    /// Screen rects of the dialog's buttons as last drawn (harness probes
    /// `@jar`, `@copy-usdt`, `@copy-btc`, `@donate-page`, `@close`).
    pub probes: Vec<(&'static str, Rect)>,
    /// The dialog's frame as last drawn (`None` while closed).
    pub rect: Option<Rect>,
}

/// "Caution" text color (WinUI's caution foreground).
fn caution(pal: &Pal) -> Color32 {
    if pal.dark {
        Color32::from_rgb(0xFC, 0xE1, 0x00)
    } else {
        Color32::from_rgb(0x9D, 0x5D, 0x00)
    }
}

/// Color of the heart glyph.
pub fn heart_color(pal: &Pal) -> Color32 {
    if pal.dark {
        Color32::from_rgb(0xFF, 0x99, 0xA4)
    } else {
        Color32::from_rgb(0xD1, 0x34, 0x38)
    }
}

impl Support {
    /// Shows the dialog while it is open; returns what was clicked.
    pub fn show(&mut self, ctx: &egui::Context, tr: Tr, pal: &Pal) -> Vec<Action> {
        let mut out = Vec::new();
        if !self.open {
            return out;
        }
        let s = tr.s;
        let qr = self
            .qr
            .get_or_insert_with(|| {
                [USDT_TRC20, BTC].map(|text| {
                    ctx.load_texture(
                        format!("qr-{text}"),
                        qr_image(text),
                        TextureOptions::NEAREST,
                    )
                })
            })
            .clone();
        let mut probes = Vec::new();
        let mut close = false;
        let screen_h = ctx.content_rect().height();
        let compact = screen_h < COMPACT_BELOW;
        let module_pt = if compact {
            MODULE_PT_COMPACT
        } else {
            MODULE_PT
        };
        let gap_v = if compact { 8.0 } else { 14.0 };
        let modal = egui::Modal::new(Id::unique("support-dialog"))
            .backdrop_color(Color32::from_black_alpha(if pal.dark { 120 } else { 70 }))
            .frame(
                egui::Frame::new()
                    .fill(pal.card)
                    .stroke(Stroke::new(1.0, pal.border))
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::same(MARGIN as i8))
                    .shadow(egui::Shadow {
                        offset: [0, 8],
                        blur: 32,
                        spread: 0,
                        color: Color32::from_black_alpha(if pal.dark { 120 } else { 60 }),
                    }),
            );
        let resp = modal.show(ctx, |ui| {
            ui.set_width(500.0);
            let top = ui.min_rect().top();
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(icon::HEART)
                        .font(theme::icon_font(20.0))
                        .color(heart_color(pal)),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(s.support_title)
                        .font(theme::semibold(20.0))
                        .color(pal.text),
                );
            });
            ui.add_space(8.0);
            // The title and the bottom row always show; what lies between
            // scrolls when even the compact layout is taller than the
            // window (the minimum window is 520 pt high).
            let title_h = ui.min_rect().bottom() - top;
            let max_h =
                (screen_h - 2.0 * (MARGIN + OUTER_GAP) - title_h - BOTTOM_ROW).max(MARGIN * 4.0);
            egui::ScrollArea::vertical()
                .id_salt("support-body")
                .max_height(max_h)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if !compact {
                        ui.label(RichText::new(s.support_text).color(pal.text_dim));
                        ui.add_space(gap_v);
                    }

                    // The main channel: the Monobank jar.
                    let jar = jar_button(ui, pal, s.support_jar, s.support_jar_sub);
                    probes.push(("jar", jar.rect));
                    if jar.clicked() {
                        out.push(Action::OpenUrl(JAR_URL));
                    }
                    ui.add_space(gap_v);

                    // Crypto: two cards side by side.
                    let gap = 12.0;
                    let card_w = (ui.available_width() - gap) * 0.5;
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        let cards = [
                            (
                                "USDT · TRC-20",
                                USDT_TRC20,
                                s.support_usdt_warn,
                                "copy-usdt",
                            ),
                            ("Bitcoin · BTC", BTC, s.support_btc_warn, "copy-btc"),
                        ];
                        for ((title, address, warn, probe), tex) in cards.into_iter().zip(&qr) {
                            ui.allocate_ui_with_layout(
                                vec2(card_w, 0.0),
                                Layout::top_down(Align::Center),
                                |ui| {
                                    let card = Card {
                                        width: card_w,
                                        title,
                                        address,
                                        warn,
                                        module_pt,
                                    };
                                    let copy = crypto_card(ui, pal, tr, &card, tex);
                                    probes.push((probe, copy.rect));
                                    if copy.clicked() {
                                        out.push(Action::CopyText(address));
                                    }
                                },
                            );
                        }
                    });
                });
            ui.add_space(if compact { 10.0 } else { 16.0 });
            ui.horizontal(|ui| {
                let link = ui
                    .add(
                        egui::Label::new(with_icon(
                            s.support_all,
                            icon::OPEN,
                            13.0,
                            pal.accent,
                            false,
                        ))
                        .sense(Sense::click()),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(DONATE_URL);
                probes.push(("donate-page", link.rect));
                if link.clicked() {
                    out.push(Action::OpenUrl(DONATE_URL));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let b = widgets::plain_button(ui, s.close);
                    probes.push(("close", b.rect));
                    if b.clicked() {
                        close = true;
                    }
                });
            });
        });
        if close || resp.should_close() {
            self.open = false;
        }
        self.rect = self.open.then_some(resp.response.rect);
        self.probes = if self.open { probes } else { Vec::new() };
        out
    }
}

/// `text` and an icon glyph before or after it (two fonts, one label).
fn with_icon(text: &str, glyph: &str, size: f32, color: Color32, glyph_first: bool) -> LayoutJob {
    let mut job = LayoutJob::default();
    let fmt = |font: FontId| TextFormat {
        font_id: font,
        color,
        valign: Align::Center,
        ..TextFormat::default()
    };
    if glyph_first {
        job.append(glyph, 0.0, fmt(theme::icon_font(size - 1.0)));
        job.append(text, 6.0, fmt(FontId::proportional(size)));
    } else {
        job.append(text, 0.0, fmt(FontId::proportional(size)));
        job.append(glyph, 6.0, fmt(theme::icon_font(size - 1.0)));
    }
    job
}

/// The wide accent button of the Monobank jar: title, subtitle, a "link
/// opens outside" glyph.
fn jar_button(ui: &mut Ui, pal: &Pal, title: &str, sub: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 54.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let fill = if resp.hovered() {
        pal.accent_hover
    } else {
        pal.accent
    };
    let p = ui.painter();
    p.rect_filled(rect, 6.0, fill);
    if resp.has_focus() {
        p.rect_stroke(
            rect.expand(2.0),
            8.0,
            Stroke::new(2.0, pal.text),
            StrokeKind::Outside,
        );
    }
    p.text(
        pos2(rect.left() + 26.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon::HEART,
        theme::icon_font(18.0),
        pal.on_accent,
    );
    p.text(
        pos2(rect.left() + 48.0, rect.top() + 18.0),
        Align2::LEFT_CENTER,
        title,
        theme::semibold(15.0),
        pal.on_accent,
    );
    p.text(
        pos2(rect.left() + 48.0, rect.top() + 37.0),
        Align2::LEFT_CENTER,
        sub,
        FontId::proportional(12.5),
        pal.on_accent.gamma_multiply(0.85),
    );
    p.text(
        pos2(rect.right() - 20.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon::OPEN,
        theme::icon_font(14.0),
        pal.on_accent,
    );
    resp
}

/// What one crypto card shows.
struct Card<'a> {
    width: f32,
    title: &'a str,
    address: &'a str,
    warn: &'a str,
    /// Points per QR module.
    module_pt: f32,
}

/// `address` laid out in two rows of (nearly) equal length: users compare
/// the first and last characters with their wallet, and a row that
/// wrapped wherever the card ended left a 3-character tail. Wrapping only
/// changes the layout: copying selected text gives the address verbatim.
fn address_galley(ui: &Ui, address: &str, color: Color32) -> std::sync::Arc<egui::Galley> {
    let font = FontId::monospace(ADDRESS_PT);
    let half = address.len().div_ceil(2);
    let row_w = ui.fonts_mut(|f| {
        f.layout_no_wrap(address[..half].to_string(), font.clone(), color)
            .size()
            .x
    });
    let mut job = LayoutJob::simple(address.to_string(), font, color, row_w + 0.5);
    job.wrap.break_anywhere = true;
    ui.fonts_mut(|f| f.layout_job(job))
}

/// One crypto channel: title, QR code, address, copy button and network
/// warning. Returns the copy button.
fn crypto_card(ui: &mut Ui, pal: &Pal, tr: Tr, card: &Card, qr: &TextureHandle) -> egui::Response {
    let Card {
        width,
        title,
        address,
        warn,
        module_pt,
    } = *card;
    let s = tr.s;
    let mut copy = None;
    egui::Frame::new()
        .fill(if pal.dark {
            Color32::from_rgb(0x24, 0x24, 0x24)
        } else {
            Color32::from_rgb(0xF7, 0xF7, 0xF7)
        })
        .stroke(Stroke::new(1.0, pal.divider))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(width - 26.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(title)
                        .font(theme::semibold(14.0))
                        .color(pal.text),
                );
                ui.add_space(4.0);
                let side = qr.size()[0] as f32 * module_pt;
                ui.add(
                    egui::Image::new(qr)
                        .fit_to_exact_size(vec2(side, side))
                        .corner_radius(4.0),
                )
                .on_hover_text(address);
                ui.add_space(4.0);
                ui.add(egui::Label::new(address_galley(ui, address, pal.text)).selectable(true));
                ui.add_space(4.0);
                let b = ui.add(egui::Button::new((
                    RichText::new(icon::COPY).font(theme::icon_font(13.0)),
                    RichText::new(s.support_copy).size(13.0),
                )));
                copy = Some(b);
                ui.add_space(4.0);
                ui.label(with_icon(warn, icon::WARNING, 12.5, caution(pal), true));
            });
        });
    copy.expect("drawn")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dark module at (x, y) of a [`qr_image`], quiet zone included.
    fn dark(img: &ColorImage, x: usize, y: usize) -> bool {
        img.pixels[y * img.size[0] + x] == Color32::BLACK
    }

    #[test]
    fn qr_codes_have_finder_patterns_and_quiet_zone() {
        for text in [USDT_TRC20, BTC, JAR_URL] {
            let img = qr_image(text);
            let side = img.size[0];
            assert_eq!(img.size[1], side);
            let n = side - 2 * QUIET;
            assert_eq!((n - 17) % 4, 0, "a QR size is 17 + 4 v");
            // Quiet zone all white.
            for i in 0..side {
                for j in 0..QUIET {
                    assert!(!dark(&img, i, j) && !dark(&img, j, i));
                    assert!(!dark(&img, i, side - 1 - j) && !dark(&img, side - 1 - j, i));
                }
            }
            // Finder patterns: 7x7 dark ring, white ring, 3x3 dark core.
            for (ox, oy) in [(0, 0), (n - 7, 0), (0, n - 7)] {
                let at = |x: usize, y: usize| dark(&img, QUIET + ox + x, QUIET + oy + y);
                for k in 0..7 {
                    assert!(at(k, 0) && at(k, 6) && at(0, k) && at(6, k));
                }
                for k in 1..6 {
                    assert!(!at(k, 1) && !at(k, 5) && !at(1, k) && !at(5, k));
                }
                for y in 2..5 {
                    for x in 2..5 {
                        assert!(at(x, y));
                    }
                }
            }
        }
        // Both addresses fit version 3 (29 modules): 148 pt on screen.
        assert_eq!(qr_image(USDT_TRC20).size[0], 29 + 2 * QUIET);
        assert_eq!(qr_image(BTC).size[0], 29 + 2 * QUIET);
    }

    #[test]
    fn addresses_show_in_two_balanced_rows() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut out = ctx.run_ui(Default::default(), |ui| {
            for a in [USDT_TRC20, BTC] {
                let g = address_galley(ui, a, Color32::WHITE);
                let rows: Vec<usize> = g
                    .rows
                    .iter()
                    .map(|r| r.char_count_excluding_newline().0)
                    .collect();
                assert_eq!(rows, [a.len().div_ceil(2), a.len() / 2], "{a}");
                // Selecting and copying gives the address verbatim.
                assert_eq!(g.text(), a);
                // Fits the narrowest card: (500 - 12) / 2 - 26 pt.
                assert!(g.size().x < 218.0, "{a}: {}", g.size().x);
            }
        });
        out.textures_delta.clear();
    }

    #[test]
    fn btc_address_checksum() {
        // BIP 173 bech32: hrp "bc", witness version 0, 20-byte program.
        const CHARSET: &str = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";
        let (hrp, data) = BTC.split_once('1').unwrap();
        assert_eq!(hrp, "bc");
        let values: Vec<u32> = data
            .chars()
            .map(|c| CHARSET.find(c).expect("bech32 character") as u32)
            .collect();
        let mut chk: u32 = 1;
        let mut feed = |v: u32| {
            const GEN: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
            let top = chk >> 25;
            chk = ((chk & 0x1ff_ffff) << 5) ^ v;
            for (i, g) in GEN.iter().enumerate() {
                if (top >> i) & 1 == 1 {
                    chk ^= g;
                }
            }
        };
        for c in hrp.bytes() {
            feed(u32::from(c) >> 5);
        }
        feed(0);
        for c in hrp.bytes() {
            feed(u32::from(c) & 31);
        }
        for &v in &values {
            feed(v);
        }
        assert_eq!(chk, 1, "bech32 checksum");
        assert_eq!(values[0], 0, "witness version 0");
        // 1 version + 32 program + 6 checksum characters = P2WPKH.
        assert_eq!(values.len(), 39);
    }

    #[test]
    fn tron_address_checksum() {
        // Base58Check: 0x41 + 20-byte hash + first 4 bytes of
        // sha256(sha256(payload)).
        const ALPHABET: &str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
        let mut bytes: Vec<u8> = Vec::new();
        for c in USDT_TRC20.chars() {
            let mut carry = ALPHABET.find(c).expect("base58 character") as u32;
            for b in bytes.iter_mut().rev() {
                carry += u32::from(*b) * 58;
                *b = (carry & 0xff) as u8;
                carry >>= 8;
            }
            while carry > 0 {
                bytes.insert(0, (carry & 0xff) as u8);
                carry >>= 8;
            }
        }
        assert_eq!(bytes.len(), 25);
        assert_eq!(bytes[0], 0x41, "TRON mainnet prefix");
        let digest = sha256(&sha256(&bytes[..21]));
        assert_eq!(&bytes[21..], &digest[..4], "base58check checksum");
    }

    #[test]
    fn sha256_known_answer() {
        let d = sha256(b"abc");
        assert_eq!(d[..4], [0xba, 0x78, 0x16, 0xbf]);
        assert_eq!(d[28..], [0xf2, 0x00, 0x15, 0xad]);
    }

    /// Minimal SHA-256 (FIPS 180-4), for the address checksum test only.
    fn sha256(data: &[u8]) -> [u8; 32] {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut msg = data.to_vec();
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
        for block in msg.chunks(64) {
            let mut w = [0u32; 64];
            for (i, word) in block.chunks(4).enumerate() {
                w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let mut v = h;
            for i in 0..64 {
                let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
                let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
                let t1 = v[7]
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
                let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
                let t2 = s0.wrapping_add(maj);
                v = [
                    t1.wrapping_add(t2),
                    v[0],
                    v[1],
                    v[2],
                    v[3].wrapping_add(t1),
                    v[4],
                    v[5],
                    v[6],
                ];
            }
            for (a, b) in h.iter_mut().zip(v) {
                *a = a.wrapping_add(b);
            }
        }
        let mut out = [0u8; 32];
        for (i, word) in h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}
