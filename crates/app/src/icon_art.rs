//! The RingScan logo, rasterized from code.
//!
//! One source of truth for the artwork: `build.rs` includes this file (via
//! `#[path]`) to write the multi-size `.ico` that is embedded into the exe,
//! and the app calls [`render`] at run time for the window and taskbar icon.
//! It must therefore stay dependency-free and use only `core`/`std`.
//!
//! The logo is a small sunburst: a slate disc in the middle, a ring of four
//! colored segments around it and a partial second and third ring, drawn
//! clockwise from 12 o'clock like the chart itself. Small sizes drop the
//! outer ring and use thicker gaps so the shape stays readable at 16 px.
//! Every pixel is supersampled (8×8) for clean anti-aliased edges.

#![allow(dead_code)]

/// One ring segment: ring index (0 = innermost ring), start and end angle in
/// degrees (clockwise from 12 o'clock) and an sRGB color.
struct Segment {
    ring: usize,
    a0: f32,
    a1: f32,
    rgb: [u8; 3],
}

const SEGMENTS: &[Segment] = &[
    // Ring 0: the top-level "folders".
    Segment {
        ring: 0,
        a0: 0.0,
        a1: 150.0,
        rgb: [0x2F, 0x7C, 0xF6],
    },
    Segment {
        ring: 0,
        a0: 150.0,
        a1: 238.0,
        rgb: [0x12, 0xB5, 0x9E],
    },
    Segment {
        ring: 0,
        a0: 238.0,
        a1: 305.0,
        rgb: [0xF5, 0x9E, 0x0B],
    },
    Segment {
        ring: 0,
        a0: 305.0,
        a1: 360.0,
        rgb: [0xE8, 0x4C, 0x8B],
    },
    // Ring 1: their children (lighter, partial).
    Segment {
        ring: 1,
        a0: 0.0,
        a1: 92.0,
        rgb: [0x5E, 0x9B, 0xF8],
    },
    Segment {
        ring: 1,
        a0: 92.0,
        a1: 138.0,
        rgb: [0x5E, 0x9B, 0xF8],
    },
    Segment {
        ring: 1,
        a0: 150.0,
        a1: 212.0,
        rgb: [0x3F, 0xCB, 0xB5],
    },
    Segment {
        ring: 1,
        a0: 238.0,
        a1: 290.0,
        rgb: [0xF8, 0xB7, 0x43],
    },
    Segment {
        ring: 1,
        a0: 305.0,
        a1: 343.0,
        rgb: [0xEE, 0x77, 0xA6],
    },
    // Ring 2: a few deep folders.
    Segment {
        ring: 2,
        a0: 0.0,
        a1: 58.0,
        rgb: [0x8D, 0xB9, 0xFA],
    },
    Segment {
        ring: 2,
        a0: 150.0,
        a1: 186.0,
        rgb: [0x74, 0xDB, 0xCA],
    },
    Segment {
        ring: 2,
        a0: 238.0,
        a1: 268.0,
        rgb: [0xFA, 0xCB, 0x75],
    },
];

/// Center disc color (slate blue).
const CENTER: [u8; 3] = [0x3A, 0x48, 0x6B];

/// Renders the logo as `size`×`size` RGBA8 pixels (straight alpha, rows top
/// to bottom).
pub fn render(size: u32) -> Vec<u8> {
    let n = size.max(1) as usize;
    // Radii as fractions of the half size. Small icons show two rings only.
    let small = size <= 24;
    let (center_r, rings): (f32, &[(f32, f32)]) = if small {
        (0.36, &[(0.40, 0.70), (0.74, 1.0)])
    } else {
        (0.30, &[(0.34, 0.58), (0.62, 0.81), (0.85, 1.0)])
    };
    let half = n as f32 / 2.0;
    // Gap between segments, in pixels of the final image.
    let gap_px = if small {
        1.0
    } else {
        (n as f32 / 96.0).max(0.9)
    };
    const SS: usize = 8;
    let mut out = vec![0u8; n * n * 4];
    for py in 0..n {
        for px in 0..n {
            // Premultiplied accumulation over the subsamples.
            let mut acc = [0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32 - half;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32 - half;
                    if let Some(rgb) = sample(x, y, half, center_r, rings, gap_px) {
                        acc[0] += f32::from(rgb[0]);
                        acc[1] += f32::from(rgb[1]);
                        acc[2] += f32::from(rgb[2]);
                        acc[3] += 1.0;
                    }
                }
            }
            let i = (py * n + px) * 4;
            if acc[3] > 0.0 {
                out[i] = (acc[0] / acc[3]).round() as u8;
                out[i + 1] = (acc[1] / acc[3]).round() as u8;
                out[i + 2] = (acc[2] / acc[3]).round() as u8;
                out[i + 3] = (acc[3] / (SS * SS) as f32 * 255.0).round() as u8;
            }
        }
    }
    out
}

/// Color of the point `(x, y)` (pixels from the icon center), if covered.
fn sample(
    x: f32,
    y: f32,
    half: f32,
    center_r: f32,
    rings: &[(f32, f32)],
    gap_px: f32,
) -> Option<[u8; 3]> {
    let r = (x * x + y * y).sqrt();
    // A hair inside the edge so the outer ring is not clipped by the bitmap.
    let rn = r / (half - 0.25);
    if rn <= center_r {
        return Some(CENTER);
    }
    // Clockwise from 12 o'clock, in degrees 0..360.
    let mut deg = x.atan2(-y).to_degrees();
    if deg < 0.0 {
        deg += 360.0;
    }
    for (ring, &(ri, ro)) in rings.iter().enumerate() {
        if rn < ri || rn > ro {
            continue;
        }
        // Gaps are a constant pixel width: convert to an angle at radius r.
        let gap_deg = (gap_px / 2.0 / r.max(1.0)).to_degrees();
        for s in SEGMENTS.iter().filter(|s| s.ring == ring) {
            if deg >= s.a0 + gap_deg && deg <= s.a1 - gap_deg {
                return Some(s.rgb);
            }
        }
        return None;
    }
    None
}
