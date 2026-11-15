//! Chart colors: palettes for the three color modes and the special arcs.
//!
//! Colors are designed in OKLCH (perceptual lightness, chroma, hue) and
//! converted to sRGB, so hues spaced evenly around the wheel look equally
//! bright: a yellow folder does not glare next to a dark blue one, which a
//! plain HSV rainbow cannot avoid. Out-of-gamut colors keep their lightness
//! and hue and lose chroma until they fit.
//!
//! * **Folders**: hue = angular position of the top-level folder (ring 1)
//!   the arc belongs to, starting with blue at 12 o'clock and running
//!   clockwise around the wheel, like the original Scanner. Deeper rings get
//!   lighter and less saturated; files are a bit more muted than folders.
//! * **File types**: one fixed color per [`Group`] (video, audio, ...);
//!   folders take the color of the group that dominates their visible
//!   content, washed out toward gray the more mixed it is.
//! * **Age**: a warm-to-cool gradient over a log time scale from "today" to
//!   "10 years"; folders show the size-weighted average age of their visible
//!   content.
//!
//! Special arcs: "N smaller items" (Rest) neutral gray with stripes, free
//! space almost transparent, unaccounted space dark gray.

use std::sync::OnceLock;

use eframe::egui::Color32;

/// sRGB color from OKLCH (`l` 0..1, chroma ~0..0.37, hue in degrees),
/// reducing chroma until it fits the sRGB gamut.
pub fn oklch(l: f32, c: f32, h_deg: f32) -> Color32 {
    let h = h_deg.to_radians();
    let (sin, cos) = h.sin_cos();
    let mut lo = 0.0f32;
    let mut hi = c.max(0.0);
    if let Some(rgb) = oklab_to_srgb(l, hi * cos, hi * sin) {
        return to_color(rgb);
    }
    // Binary search for the largest chroma inside the gamut.
    for _ in 0..12 {
        let mid = (lo + hi) / 2.0;
        if oklab_to_srgb(l, mid * cos, mid * sin).is_some() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    to_color(oklab_to_srgb(l, lo * cos, lo * sin).unwrap_or([l, l, l]))
}

/// OKLab → gamma-encoded sRGB in 0..1, or None if outside the gamut.
fn oklab_to_srgb(l: f32, a: f32, b: f32) -> Option<[f32; 3]> {
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    let lin = [
        4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_94 * s3,
        -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_38 * s3,
        -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3,
    ];
    const EPS: f32 = 1e-4;
    if lin.iter().any(|&v| !(-EPS..=1.0 + EPS).contains(&v)) {
        return None;
    }
    Some(lin.map(|v| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.003_130_8 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    }))
}

fn to_color(rgb: [f32; 3]) -> Color32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgb(q(rgb[0]), q(rgb[1]), q(rgb[2]))
}

/// Linear interpolation in gamma space (good enough for UI tints).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        f(a.r(), b.r()),
        f(a.g(), b.g()),
        f(a.b(), b.b()),
        f(a.a(), b.a()),
    )
}

/// `top` (premultiplied, possibly translucent) composited over opaque `bg`.
pub fn over(top: Color32, bg: Color32) -> Color32 {
    let a = f32::from(top.a()) / 255.0;
    let f = |t: u8, b: u8| (f32::from(t) + f32::from(b) * (1.0 - a)).round().min(255.0) as u8;
    Color32::from_rgb(f(top.r(), bg.r()), f(top.g(), bg.g()), f(top.b(), bg.b()))
}

/// Relative luminance (0..1) of an opaque color, for picking label colors.
pub fn luminance(c: Color32) -> f32 {
    let lin = |v: u8| {
        let v = f32::from(v) / 255.0;
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// Black or white text, whichever reads better on `bg`.
pub fn text_on(bg: Color32) -> Color32 {
    if luminance(bg) > 0.36 {
        Color32::from_rgb(0x1B, 0x1B, 0x1F)
    } else {
        Color32::WHITE
    }
}

// ---------------------------------------------------------------------------
// Folders mode
// ---------------------------------------------------------------------------

/// Hue (degrees) of a top-level arc whose middle sits at `mid_angle`
/// radians clockwise from 12 o'clock: blue at the top, then purple, red,
/// orange, yellow, green and teal around the clock.
pub fn folder_hue(mid_angle: f32) -> f32 {
    (255.0 + mid_angle.to_degrees()).rem_euclid(360.0)
}

/// How much an arc's own angular offset from its top-level folder shifts
/// its hue (degrees per degree): siblings deep inside one big folder stay
/// in the same color family but remain distinguishable.
const HUE_SPREAD: f32 = 0.3;

/// Hue of an arc whose middle is at `mid` inside a top-level arc whose
/// middle is at `top_mid` (radians).
pub fn folder_hue_at(top_mid: f32, mid: f32) -> f32 {
    (folder_hue(top_mid) + HUE_SPREAD * (mid - top_mid).to_degrees()).rem_euclid(360.0)
}

/// Folders-mode color of an arc on ring `depth` (1 = innermost). Deeper
/// rings fade toward the background: lighter and less saturated on the
/// light theme, darker and less saturated on the dark one.
pub fn folder_color(hue: f32, depth: u8, is_file: bool, dark: bool) -> Color32 {
    let d = f32::from(depth.max(1) - 1);
    let (mut l, mut c) = if dark {
        ((0.66 - 0.015 * d).max(0.46), 0.135 * 0.93f32.powf(d))
    } else {
        ((0.70 + 0.017 * d).min(0.87), 0.14 * 0.94f32.powf(d))
    };
    if is_file {
        c *= 0.62;
        l += if dark { -0.02 } else { 0.03 };
    }
    oklch(l, c, hue)
}

// ---------------------------------------------------------------------------
// File types mode
// ---------------------------------------------------------------------------

/// File type groups of the "File types" color mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
    Video,
    Audio,
    Images,
    Archives,
    Programs,
    Documents,
    Code,
    System,
    Other,
}

impl Group {
    pub const ALL: [Group; 9] = [
        Group::Video,
        Group::Audio,
        Group::Images,
        Group::Archives,
        Group::Programs,
        Group::Documents,
        Group::Code,
        Group::System,
        Group::Other,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn color(self, dark: bool) -> Color32 {
        let dl = if dark { -0.04 } else { 0.0 };
        match self {
            Group::Video => oklch(0.60 + dl, 0.17, 305.0),
            Group::Audio => oklch(0.70 + dl, 0.15, 352.0),
            Group::Images => oklch(0.72 + dl, 0.16, 145.0),
            Group::Archives => oklch(0.80 + dl, 0.14, 78.0),
            Group::Programs => oklch(0.64 + dl, 0.17, 28.0),
            Group::Documents => oklch(0.62 + dl, 0.14, 252.0),
            Group::Code => oklch(0.74 + dl, 0.11, 198.0),
            Group::System => oklch(0.56 + dl, 0.05, 270.0),
            Group::Other => oklch(0.78 + dl, 0.0, 0.0),
        }
    }

    /// Group of a lowercase extension without the dot ("" = none).
    pub fn of_ext(ext: &str) -> Group {
        match ext {
            "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" | "mpg" | "mpeg" | "m2ts"
            | "mts" | "vob" | "flv" | "3gp" | "ogv" | "hevc" => Group::Video,
            "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" | "wma" | "opus" | "aiff" | "aif"
            | "alac" | "mid" | "midi" | "ape" | "wv" => Group::Audio,
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "heic" | "heif"
            | "avif" | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "psd"
            | "svg" | "ico" | "jxl" | "xcf" | "kra" | "exr" | "hdr" | "tga" | "dds" => {
                Group::Images
            }
            "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "lz" | "lzma"
            | "cab" | "iso" | "img" | "vhd" | "vhdx" | "vmdk" | "qcow2" | "wim" | "esd" | "dmg"
            | "apk" | "appx" | "msix" | "nupkg" | "jar" | "whl" => Group::Archives,
            "exe" | "dll" | "msi" | "msp" | "sys" | "drv" | "ocx" | "cpl" | "scr" | "com"
            | "bat" | "cmd" | "ps1" | "efi" | "mui" | "node" | "so" | "dylib" | "lib" | "a"
            | "pdb" | "winmd" => Group::Programs,
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp"
            | "rtf" | "txt" | "md" | "epub" | "djvu" | "fb2" | "mobi" | "csv" | "pages" | "key"
            | "numbers" | "one" | "pst" | "ost" | "eml" | "msg" | "chm" | "xps" => Group::Documents,
            "rs" | "c" | "h" | "cpp" | "hpp" | "cc" | "cs" | "java" | "kt" | "py" | "pyc"
            | "js" | "mjs" | "ts" | "tsx" | "jsx" | "go" | "rb" | "php" | "swift" | "lua"
            | "sh" | "html" | "htm" | "css" | "scss" | "json" | "xml" | "yaml" | "yml" | "toml"
            | "ini" | "cfg" | "conf" | "sql" | "db" | "sqlite" | "map" | "wasm" | "class" | "o"
            | "obj" | "rlib" | "rmeta" | "d" | "pack" | "idx" | "ipynb" | "proto" => Group::Code,
            "log" | "tmp" | "dat" | "bin" | "etl" | "evtx" | "cat" | "manifest" | "mum"
            | "regtrans-ms" | "blf" | "pf" | "dmp" | "hiberfil" | "pagefile" | "swp" | "cache"
            | "lnk" | "ttf" | "otf" | "ttc" | "fon" | "nls" | "edb" | "jrs" | "chk" | "old"
            | "bak" | "inf" | "pnf" | "msu" | "ldf" | "mdf" => Group::System,
            _ => Group::Other,
        }
    }
}

/// Folder color in types mode: the dominant group's color, washed out
/// toward neutral gray the smaller its share of the content.
pub fn types_folder_color(dominant: Group, share: f32, depth: u8, dark: bool) -> Color32 {
    let neutral = neutral(dark, depth);
    let strength = ((share - 0.25) / 0.55).clamp(0.0, 1.0) * 0.85;
    mix(neutral, dominant.color(dark), strength)
}

/// Neutral color of folders whose content is not visible.
pub fn neutral(dark: bool, depth: u8) -> Color32 {
    let d = f32::from(depth.max(1) - 1);
    if dark {
        oklch((0.50 + 0.02 * d).min(0.7), 0.0, 0.0)
    } else {
        oklch((0.84 + 0.01 * d).min(0.92), 0.0, 0.0)
    }
}

// ---------------------------------------------------------------------------
// Age mode
// ---------------------------------------------------------------------------

/// Gradient stops: (position 0..1, OKLCH).
const AGE_STOPS: [(f32, [f32; 3]); 5] = [
    (0.0, [0.64, 0.19, 28.0]),
    (0.33, [0.79, 0.15, 78.0]),
    (0.58, [0.76, 0.12, 155.0]),
    (0.82, [0.63, 0.12, 238.0]),
    (1.0, [0.50, 0.07, 275.0]),
];

/// Days at the end of the scale ("10 years").
pub const AGE_MAX_DAYS: f32 = 3652.0;

/// Gradient position (0 = today, 1 = 10+ years) of an age in days.
pub fn age_t(days: f32) -> f32 {
    ((1.0 + days.max(0.0)).ln() / (1.0 + AGE_MAX_DAYS).ln()).clamp(0.0, 1.0)
}

/// Age-mode color for gradient position `t` (see [`age_t`]).
pub fn age_color(t: f32, dark: bool) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let i = AGE_STOPS.iter().rposition(|s| s.0 <= t).unwrap_or(0);
    let (a, b) = (AGE_STOPS[i], AGE_STOPS[(i + 1).min(AGE_STOPS.len() - 1)]);
    let f = if b.0 > a.0 {
        (t - a.0) / (b.0 - a.0)
    } else {
        0.0
    };
    // Interpolate in OKLab (via a/b components) to avoid hue detours.
    let lab = |c: [f32; 3]| {
        let h = c[2].to_radians();
        [c[0], c[1] * h.cos(), c[1] * h.sin()]
    };
    let (la, lb) = (lab(a.1), lab(b.1));
    let l = la[0] + (lb[0] - la[0]) * f - if dark { 0.03 } else { 0.0 };
    let aa = la[1] + (lb[1] - la[1]) * f;
    let bb = la[2] + (lb[2] - la[2]) * f;
    let c = (aa * aa + bb * bb).sqrt();
    oklch(l, c, bb.atan2(aa).to_degrees())
}

// ---------------------------------------------------------------------------
// Special arcs
// ---------------------------------------------------------------------------

/// The two stripe colors of a Rest ("N smaller items") arc.
///
/// Computed once: the chart asks for them per Rest arc on every mesh build
/// and every animation frame, and the OKLCH conversion (`powf`, slow in the
/// Windows C runtime) cost ~5 ms per 10 000 Rest arcs there.
pub fn rest_colors(dark: bool) -> (Color32, Color32) {
    static COLORS: OnceLock<[(Color32, Color32); 2]> = OnceLock::new();
    COLORS.get_or_init(|| {
        [
            (oklch(0.845, 0.0, 0.0), oklch(0.875, 0.0, 0.0)),
            (oklch(0.43, 0.0, 0.0), oklch(0.46, 0.0, 0.0)),
        ]
    })[usize::from(dark)]
}

/// Fill and outline of a free-space arc: nearly the chart background.
pub fn free_colors(dark: bool) -> (Color32, Color32) {
    if dark {
        (
            Color32::from_rgba_unmultiplied(255, 255, 255, 10),
            Color32::from_rgba_unmultiplied(255, 255, 255, 46),
        )
    } else {
        (
            Color32::from_rgba_unmultiplied(0, 0, 0, 8),
            Color32::from_rgba_unmultiplied(0, 0, 0, 40),
        )
    }
}

pub fn unaccounted_color(dark: bool) -> Color32 {
    if dark {
        oklch(0.40, 0.008, 260.0)
    } else {
        oklch(0.50, 0.008, 260.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oklch_known_values() {
        // White and black.
        assert_eq!(oklch(1.0, 0.0, 0.0), Color32::from_rgb(255, 255, 255));
        assert_eq!(oklch(0.0, 0.0, 0.0), Color32::from_rgb(0, 0, 0));
        // Out-of-gamut chroma is reduced, never panics.
        let c = oklch(0.9, 0.4, 260.0);
        assert!(c.b() >= c.r());
    }

    #[test]
    fn rest_colors_are_cached_per_theme() {
        assert_eq!(
            rest_colors(false),
            (oklch(0.845, 0.0, 0.0), oklch(0.875, 0.0, 0.0))
        );
        assert_eq!(
            rest_colors(true),
            (oklch(0.43, 0.0, 0.0), oklch(0.46, 0.0, 0.0))
        );
        assert_eq!(rest_colors(true), rest_colors(true));
    }

    #[test]
    fn groups() {
        assert_eq!(Group::of_ext("mp4"), Group::Video);
        assert_eq!(Group::of_ext("dll"), Group::Programs);
        assert_eq!(Group::of_ext(""), Group::Other);
        assert!(age_t(0.0) == 0.0 && age_t(1e9) == 1.0);
    }
}
