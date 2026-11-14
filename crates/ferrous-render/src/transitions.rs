//! Scene transitions: blend the outgoing and incoming canvases.

use ferrous_core::scene::{MatteLayout, TransitionDirection, TransitionKind};
use ferrous_core::VideoFrame;
use rayon::prelude::*;

/// Smoothstep easing so motion starts and ends gently.
pub fn ease(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    p * p * (3.0 - 2.0 * p)
}

fn lerp_u8(a: u8, b: u8, t: u32) -> u8 {
    ((a as u32 * (256 - t) + b as u32 * t) >> 8) as u8
}

fn blend(a: &VideoFrame, b: &VideoFrame, t: f32) -> VideoFrame {
    let t = (t.clamp(0.0, 1.0) * 256.0) as u32;
    let mut out = vec![0u8; a.data.len()];
    out.par_chunks_mut(4096).enumerate().for_each(|(i, chunk)| {
        let off = i * 4096;
        for (j, o) in chunk.iter_mut().enumerate() {
            *o = lerp_u8(a.data[off + j], b.data[off + j], t);
        }
    });
    VideoFrame::new(a.width, a.height, out)
}

/// Offset of the incoming (and for Slide, outgoing) canvas in pixels.
fn offsets(dir: TransitionDirection, w: i64, h: i64, p: f32) -> (i64, i64) {
    let q = 1.0 - p;
    match dir {
        TransitionDirection::Left => ((w as f32 * q) as i64, 0),
        TransitionDirection::Right => (-(w as f32 * q) as i64, 0),
        TransitionDirection::Up => (0, (h as f32 * q) as i64),
        TransitionDirection::Down => (0, -(h as f32 * q) as i64),
    }
}

/// Draws `b` shifted by (dx,dy) over `a` (and `a` shifted by `a_shift` if given).
fn shifted(a: &VideoFrame, b: &VideoFrame, dx: i64, dy: i64, a_shift: Option<(i64, i64)>) -> VideoFrame {
    let (w, h) = (a.width as i64, a.height as i64);
    let row = (w * 4) as usize;
    let mut out = vec![0u8; a.data.len()];
    out.par_chunks_exact_mut(row).enumerate().for_each(|(y, dst)| {
        let y = y as i64;
        for x in 0..w {
            let (bx, by) = (x - dx, y - dy);
            let d = (x * 4) as usize;
            if bx >= 0 && bx < w && by >= 0 && by < h {
                let s = ((by * w + bx) * 4) as usize;
                dst[d..d + 4].copy_from_slice(&b.data[s..s + 4]);
            } else {
                let (ax, ay) = match a_shift {
                    Some((sx, sy)) => (x - sx, y - sy),
                    None => (x, y),
                };
                if ax >= 0 && ax < w && ay >= 0 && ay < h {
                    let s = ((ay * w + ax) * 4) as usize;
                    dst[d..d + 4].copy_from_slice(&a.data[s..s + 4]);
                } else {
                    dst[d..d + 4].copy_from_slice(&[0, 0, 0, 255]);
                }
            }
        }
    });
    VideoFrame::new(a.width, a.height, out)
}

/// Mixes `from` → `to` at linear progress `p` (0..1).
pub fn apply(kind: &TransitionKind, from: &VideoFrame, to: &VideoFrame, p: f32) -> VideoFrame {
    if from.width != to.width || from.height != to.height {
        return to.clone();
    }
    let e = ease(p);
    match kind {
        TransitionKind::Cut => to.clone(),
        TransitionKind::Fade => blend(from, to, e),
        TransitionKind::FadeToColor { bgra } => {
            let c = VideoFrame::solid(from.width, from.height, *bgra);
            if e < 0.5 {
                blend(from, &c, e * 2.0)
            } else {
                blend(&c, to, (e - 0.5) * 2.0)
            }
        }
        TransitionKind::Swipe { direction } => {
            let (dx, dy) = offsets(*direction, from.width as i64, from.height as i64, e);
            shifted(from, to, dx, dy, None)
        }
        TransitionKind::Slide { direction } => {
            let (w, h) = (from.width as i64, from.height as i64);
            let (dx, dy) = offsets(*direction, w, h, e);
            // The outgoing scene is pushed out the opposite side.
            let a_shift = (dx - dx.signum() * w * (dx != 0) as i64, dy - dy.signum() * h * (dy != 0) as i64);
            shifted(from, to, dx, dy, Some(a_shift))
        }
        // Stingers are overlaid by the engine; underneath it's a cut.
        TransitionKind::Stinger { .. } | TransitionKind::Matte { .. } => to.clone(),
        TransitionKind::Luma { wipe, image, softness, invert } => {
            let map = luma_map(wipe, image.as_deref(), from.width, from.height);
            luma_mix(from, to, &map, p, *softness, *invert)
        }
    }
}

// ------------------------------------------------------- track-matte stingers

/// A matte's brightness, 0 (outgoing scene) to 255 (incoming scene).
pub struct Matte {
    pub width: u32,
    pub height: u32,
    pub luma: Vec<u8>,
}

fn luma_of(px: &[u8]) -> u8 {
    ((px[0] as u32 * 29 + px[1] as u32 * 150 + px[2] as u32 * 77) >> 8) as u8
}

/// Copies a rectangle out of a frame.
fn crop(f: &VideoFrame, x: u32, y: u32, w: u32, h: u32) -> VideoFrame {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for row in y..y + h {
        let s = ((row * f.width + x) * 4) as usize;
        out.extend_from_slice(&f.data[s..s + (w * 4) as usize]);
    }
    VideoFrame::new(w, h, out)
}

/// Splits one video frame into its optional picture and its matte.
pub fn split_matte(f: &VideoFrame, layout: MatteLayout) -> (Option<VideoFrame>, Matte) {
    let to_matte = |m: &VideoFrame| Matte { width: m.width, height: m.height, luma: m.data.chunks_exact(4).map(luma_of).collect() };
    match layout {
        MatteLayout::Luma => (None, to_matte(f)),
        MatteLayout::Horizontal if f.width >= 2 => {
            let w = f.width / 2;
            (Some(crop(f, 0, 0, w, f.height)), to_matte(&crop(f, w, 0, w, f.height)))
        }
        MatteLayout::Vertical if f.height >= 2 => {
            let h = f.height / 2;
            (Some(crop(f, 0, 0, f.width, h)), to_matte(&crop(f, 0, h, f.width, h)))
        }
        _ => (None, to_matte(f)),
    }
}

/// Shows `b` (incoming) where the matte is bright and `a` (outgoing) where
/// it is dark, scaling the matte to the canvas with bilinear sampling.
pub fn matte_mix(a: &VideoFrame, b: &VideoFrame, matte: &Matte, invert: bool) -> VideoFrame {
    if a.width != b.width || a.height != b.height || matte.width == 0 || matte.height == 0 {
        return b.clone();
    }
    let (w, h) = (a.width as usize, a.height as usize);
    let (mw, mh) = (matte.width as usize, matte.height as usize);
    let row = w * 4;
    let mut out = vec![0u8; a.data.len()];
    out.par_chunks_exact_mut(row).enumerate().for_each(|(y, dst)| {
        let fy = ((y as f32 + 0.5) * mh as f32 / h as f32 - 0.5).clamp(0.0, (mh - 1) as f32);
        let (y0, wy) = (fy as usize, fy.fract());
        let y1 = (y0 + 1).min(mh - 1);
        for x in 0..w {
            let fx = ((x as f32 + 0.5) * mw as f32 / w as f32 - 0.5).clamp(0.0, (mw - 1) as f32);
            let (x0, wx) = (fx as usize, fx.fract());
            let x1 = (x0 + 1).min(mw - 1);
            let s = |xx: usize, yy: usize| matte.luma[yy * mw + xx] as f32;
            let top = s(x0, y0) + (s(x1, y0) - s(x0, y0)) * wx;
            let bot = s(x0, y1) + (s(x1, y1) - s(x0, y1)) * wx;
            let mut m = (top + (bot - top) * wy).round() as u32;
            if invert {
                m = 255 - m;
            }
            let t = m + (m >> 7); // 255 -> 256, so white is fully the incoming scene
            let i = y * row + x * 4;
            for c in 0..4 {
                dst[x * 4 + c] = lerp_u8(a.data[i + c], b.data[i + c], t);
            }
        }
    });
    VideoFrame::new(a.width, a.height, out)
}

// ------------------------------------------------------------- luma wipes

/// OBS's luma wipe: a pixel switches when the sweep passes its luma value,
/// with a `softness`-wide blend band.
pub fn luma_mix(a: &VideoFrame, b: &VideoFrame, map: &[u8], p: f32, softness: f32, invert: bool) -> VideoFrame {
    let soft = softness.clamp(0.0, 1.0);
    let time = p.clamp(0.0, 1.0) * (1.0 + soft);
    let row = a.width as usize * 4;
    let mut out = vec![0u8; a.data.len()];
    out.par_chunks_exact_mut(row).enumerate().for_each(|(y, dst)| {
        let w = a.width as usize;
        for x in 0..w {
            let mut l = map[y * w + x] as f32 / 255.0;
            if invert {
                l = 1.0 - l;
            }
            let i = y * row + x * 4;
            let t = if l <= time - soft {
                256
            } else if l >= time {
                0
            } else {
                (((time - l) / soft.max(1e-6)) * 256.0) as u32
            };
            for c in 0..4 {
                dst[x * 4 + c] = lerp_u8(a.data[i + c], b.data[i + c], t.min(256));
            }
        }
    });
    VideoFrame::new(a.width, a.height, out)
}

type MapKey = (String, Option<std::path::PathBuf>, u32, u32);
static MAPS: std::sync::LazyLock<parking_lot_free::Cache> = std::sync::LazyLock::new(Default::default);

mod parking_lot_free {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct Cache(pub Mutex<HashMap<super::MapKey, Arc<Vec<u8>>>>);
}

/// The wipe's luma map at `w x h`, generated (or loaded and scaled) once and cached.
pub fn luma_map(wipe: &str, image: Option<&std::path::Path>, w: u32, h: u32) -> std::sync::Arc<Vec<u8>> {
    let key: MapKey = (wipe.to_string(), image.map(|p| p.to_path_buf()), w, h);
    if let Some(m) = MAPS.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&key) {
        return m.clone();
    }
    let map = image
        .and_then(|p| match image::open(p) {
            Ok(img) => Some(img.resize_exact(w, h, image::imageops::FilterType::Triangle).to_luma8().into_raw()),
            Err(e) => {
                log::warn!("luma wipe image {}: {e}", p.display());
                None
            }
        })
        .unwrap_or_else(|| generate(wipe, w, h));
    let map = std::sync::Arc::new(map);
    let mut cache = MAPS.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.len() > 16 {
        cache.clear();
    }
    cache.insert(key, map.clone());
    map
}

/// Smooth value noise in 0..1 (for "cloud").
fn value_noise(x: f32, y: f32, seed: u32) -> f32 {
    fn hash(x: i32, y: i32, s: u32) -> f32 {
        let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (y as u32).wrapping_mul(0xd816_3841) ^ s.wrapping_mul(0xcb1a_b31f);
        h ^= h >> 13;
        h = h.wrapping_mul(0x5bd1_e995);
        h ^= h >> 15;
        (h & 0xffff) as f32 / 65535.0
    }
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (x - xi as f32, y - yi as f32);
    let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let a = hash(xi, yi, seed) + (hash(xi + 1, yi, seed) - hash(xi, yi, seed)) * sx;
    let b = hash(xi, yi + 1, seed) + (hash(xi + 1, yi + 1, seed) - hash(xi, yi + 1, seed)) * sx;
    a + (b - a) * sy
}

/// The luma value (0 = switches first) of a built-in wipe at normalised
/// position (u, v), with `aspect` = width / height.
pub fn wipe_value(wipe: &str, u: f32, v: f32, aspect: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    // Centred, aspect-corrected coordinates for round shapes.
    let (cx, cy) = ((u - 0.5) * aspect, v - 0.5);
    let r_max = (0.25 * aspect * aspect + 0.25).sqrt();
    let r = (cx * cx + cy * cy).sqrt() / r_max;
    let ang = (cy.atan2(cx) + PI / 2.0).rem_euclid(TAU) / TAU; // 0 at 12 o'clock, clockwise
    let v = match wipe {
        "linear-v" => v,
        "linear-topleft" => (u + v) / 2.0,
        "linear-topright" => ((1.0 - u) + v) / 2.0,
        "barndoor-h" => 1.0 - (u - 0.5).abs() * 2.0,
        "barndoor-v" => 1.0 - (v - 0.5).abs() * 2.0,
        "barndoor-topleft" => 1.0 - ((u - v).abs()),
        "barndoor-botleft" => 1.0 - ((u + v - 1.0).abs()),
        "blinds-h" => (v * 8.0).fract(),
        "blinds-v" => (u * 8.0).fract(),
        "box-topleft" => u.max(v),
        "box-topright" => (1.0 - u).max(v),
        "box-botleft" => u.max(1.0 - v),
        "box-botright" => (1.0 - u).max(1.0 - v),
        "iris" => r,
        "square" => ((u - 0.5).abs()).max((v - 0.5).abs()) * 2.0,
        "circles" => {
            // A grid of circles opening together.
            let n = 6.0;
            let (gx, gy) = ((u * n * aspect).fract() - 0.5, (v * n).fract() - 0.5);
            ((gx * gx + gy * gy).sqrt() / std::f32::consts::FRAC_1_SQRT_2).min(1.0)
        }
        "clock" => ang,
        "fan" => {
            // Sweeps out from 12 o'clock both ways.
            let a = ang;
            if a <= 0.5 { a * 2.0 } else { (1.0 - a) * 2.0 }
        }
        "burst" => {
            // Eight rays opening.
            let a = (ang * 8.0).fract();
            (a - 0.5).abs() * 2.0
        }
        "spiral" => (ang + r * 2.0).fract() * 0.5 + r * 0.5,
        "checkerboard-small" => {
            let n = 8.0;
            let (ix, iy) = ((u * n * aspect) as i32, (v * n) as i32);
            let cell = ((u * n * aspect).fract() + (v * n).fract()) / 2.0;
            if (ix + iy) % 2 == 0 { cell * 0.5 } else { 0.5 + cell * 0.5 }
        }
        "zigzag-h" => {
            let tri = ((u * 10.0).fract() - 0.5).abs() * 2.0;
            (v * 0.85 + tri * 0.15).clamp(0.0, 1.0)
        }
        "zigzag-v" => {
            let tri = ((v * 10.0).fract() - 0.5).abs() * 2.0;
            (u * 0.85 + tri * 0.15).clamp(0.0, 1.0)
        }
        "sinus9" => (u * 0.85 + 0.15 * (0.5 + 0.5 * (v * 9.0 * TAU).sin())).clamp(0.0, 1.0),
        "cloud" => {
            let s = 4.0;
            value_noise(u * s * aspect, v * s, 1) * 0.5 + value_noise(u * s * 2.0 * aspect, v * s * 2.0, 2) * 0.3 + value_noise(u * s * 4.0 * aspect, v * s * 4.0, 3) * 0.2
        }
        "dissolve" => value_noise(u * 400.0 * aspect, v * 400.0, 7),
        _ => u, // "linear-h" and unknown names
    };
    v.clamp(0.0, 1.0)
}

fn generate(wipe: &str, w: u32, h: u32) -> Vec<u8> {
    let aspect = w as f32 / h.max(1) as f32;
    let mut out = vec![0u8; (w * h) as usize];
    out.par_chunks_exact_mut(w as usize).enumerate().for_each(|(y, row)| {
        let v = (y as f32 + 0.5) / h as f32;
        for (x, o) in row.iter_mut().enumerate() {
            let u = (x as f32 + 0.5) / w as f32;
            *o = (wipe_value(wipe, u, v, aspect) * 255.0).round() as u8;
        }
    });
    // Normalise so every wipe uses the full range (starts at 0, ends at 255).
    let (lo, hi) = (*out.iter().min().unwrap_or(&0), *out.iter().max().unwrap_or(&255));
    if hi > lo && (lo > 0 || hi < 255) {
        let k = 255.0 / (hi - lo) as f32;
        out.par_iter_mut().for_each(|p| *p = ((*p - lo) as f32 * k).round() as u8);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(f: &VideoFrame, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * f.width + x) * 4) as usize;
        f.data[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn fade_midpoint_and_ends() {
        let a = VideoFrame::solid(8, 8, [0, 0, 0, 255]);
        let b = VideoFrame::solid(8, 8, [200, 200, 200, 255]);
        assert_eq!(px(&apply(&TransitionKind::Fade, &a, &b, 0.0), 0, 0), [0, 0, 0, 255]);
        assert_eq!(px(&apply(&TransitionKind::Fade, &a, &b, 1.0), 0, 0)[0], 200);
        let mid = px(&apply(&TransitionKind::Fade, &a, &b, 0.5), 0, 0)[0];
        assert!((95..=105).contains(&mid), "{mid}");
    }

    #[test]
    fn swipe_left_reveals_from_right() {
        let a = VideoFrame::solid(100, 10, [0, 0, 0, 255]);
        let b = VideoFrame::solid(100, 10, [255, 255, 255, 255]);
        let f = apply(&TransitionKind::Swipe { direction: TransitionDirection::Left }, &a, &b, 0.5);
        assert_eq!(px(&f, 99, 5)[0], 255, "incoming on the right");
        assert_eq!(px(&f, 0, 5)[0], 0, "outgoing still on the left");
    }

    #[test]
    fn slide_moves_both() {
        let mut ad = vec![0u8; 100 * 4];
        ad[99 * 4] = 77; // marker at the right edge of the outgoing scene
        let a = VideoFrame::new(100, 1, ad);
        let b = VideoFrame::solid(100, 1, [255, 255, 255, 255]);
        let f = apply(&TransitionKind::Slide { direction: TransitionDirection::Left }, &a, &b, 0.5);
        // Halfway, the outgoing right edge has moved left by ~half the width.
        assert_eq!(px(&f, 49, 0)[0], 77);
    }

    fn luma(wipe: &str) -> TransitionKind {
        TransitionKind::Luma { wipe: wipe.into(), image: None, softness: 0.1, invert: false }
    }

    #[test]
    fn luma_wipe_ends_match_and_sweeps_in_order() {
        let a = VideoFrame::solid(64, 36, [0, 0, 0, 255]);
        let b = VideoFrame::solid(64, 36, [255, 255, 255, 255]);
        for wipe in ferrous_core::import_obs::LUMA_WIPES {
            let k = luma(wipe);
            assert_eq!(apply(&k, &a, &b, 0.0).data, a.data, "{wipe} starts on the outgoing scene");
            assert_eq!(apply(&k, &a, &b, 1.0).data, b.data, "{wipe} ends on the incoming scene");
            let mid = apply(&k, &a, &b, 0.5);
            let lit = mid.data.chunks_exact(4).filter(|p| p[0] > 128).count() as f32 / (64.0 * 36.0);
            assert!((0.2..0.8).contains(&lit), "{wipe}: {lit} of the frame switched at the midpoint");
        }
        // linear-h goes left to right; inverted, right to left.
        let mid = apply(&luma("linear-h"), &a, &b, 0.4);
        assert!(px(&mid, 2, 10)[0] == 255 && px(&mid, 60, 10)[0] == 0);
        let inv = TransitionKind::Luma { wipe: "linear-h".into(), image: None, softness: 0.1, invert: true };
        let mid = apply(&inv, &a, &b, 0.4);
        assert!(px(&mid, 2, 10)[0] == 0 && px(&mid, 60, 10)[0] == 255);
        // Clock starts at 12 o'clock and runs clockwise.
        let q = apply(&luma("clock"), &a, &b, 0.3);
        assert_eq!(px(&q, 40, 4)[0], 255, "just right of 12 o'clock");
        assert_eq!(px(&q, 20, 4)[0], 0, "just left of 12 o'clock");
    }

    #[test]
    fn luma_softness_blends_a_band() {
        let a = VideoFrame::solid(200, 4, [0, 0, 0, 255]);
        let b = VideoFrame::solid(200, 4, [255, 255, 255, 255]);
        let hard = TransitionKind::Luma { wipe: "linear-h".into(), image: None, softness: 0.0, invert: false };
        let soft = TransitionKind::Luma { wipe: "linear-h".into(), image: None, softness: 0.5, invert: false };
        let partial = |k: &TransitionKind| apply(k, &a, &b, 0.5).data.chunks_exact(4).filter(|p| p[0] > 10 && p[0] < 245).count();
        assert!(partial(&hard) <= 8);
        assert!(partial(&soft) > 100);
    }

    #[test]
    fn luma_from_image_file() {
        let dir = std::env::temp_dir().join("ferrous-luma-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vgrad.png");
        // Bottom-to-top gradient: bright at the top switches last.
        let img = image::GrayImage::from_fn(16, 16, |_, y| image::Luma([((15 - y) * 17) as u8]));
        img.save(&path).unwrap();
        let a = VideoFrame::solid(32, 32, [0, 0, 0, 255]);
        let b = VideoFrame::solid(32, 32, [255, 255, 255, 255]);
        let k = TransitionKind::Luma { wipe: "linear-h".into(), image: Some(path), softness: 0.05, invert: false };
        let f = apply(&k, &a, &b, 0.5);
        assert_eq!(px(&f, 16, 30)[0], 255, "bottom (dark) already switched");
        assert_eq!(px(&f, 16, 1)[0], 0, "top (bright) not yet");
    }

    #[test]
    fn fade_to_color_hits_color_midway() {
        let a = VideoFrame::solid(4, 4, [255, 255, 255, 255]);
        let b = VideoFrame::solid(4, 4, [255, 255, 255, 255]);
        let f = apply(&TransitionKind::FadeToColor { bgra: [0, 0, 0, 255] }, &a, &b, 0.5);
        assert!(px(&f, 0, 0)[0] < 5);
    }

    #[test]
    fn matte_shows_incoming_where_white_and_outgoing_where_black() {
        let a = VideoFrame::solid(4, 2, [0, 0, 0, 255]);
        let b = VideoFrame::solid(4, 2, [200, 200, 200, 255]);
        // A 2x1 matte scaled up: black left, white right.
        let m = Matte { width: 2, height: 1, luma: vec![0, 255] };
        let f = matte_mix(&a, &b, &m, false);
        assert_eq!(px(&f, 0, 0)[0], 0);
        assert_eq!(px(&f, 3, 1)[0], 200);
        let g = matte_mix(&a, &b, &m, true);
        assert_eq!(px(&g, 0, 0)[0], 200);
        assert_eq!(px(&g, 3, 1)[0], 0);
    }

    #[test]
    fn matte_layouts_split_picture_from_matte() {
        // 4x2 frame: left half red picture, right half white matte.
        let mut d = Vec::new();
        for _ in 0..2 {
            d.extend_from_slice(&[0, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255]);
        }
        let f = VideoFrame::new(4, 2, d);
        let (pic, m) = split_matte(&f, MatteLayout::Horizontal);
        let pic = pic.unwrap();
        assert_eq!((pic.width, pic.height), (2, 2));
        assert_eq!(px(&pic, 0, 0)[2], 255);
        assert_eq!((m.width, m.height), (2, 2));
        assert!(m.luma.iter().all(|l| *l >= 254));
        let (none, whole) = split_matte(&f, MatteLayout::Luma);
        assert!(none.is_none());
        assert_eq!(whole.width, 4);
    }
}
