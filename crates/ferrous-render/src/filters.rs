//! Video filters, applied to a source's frame before compositing.

use std::path::{Path, PathBuf};
use std::time::Instant;

use ferrous_core::filters::{VideoFilter, VideoFilters};
use ferrous_core::VideoFrame;
use rayon::prelude::*;

/// A parsed `.cube` 3D LUT (RGB in, RGB out, 0..1).
pub struct Lut3d {
    size: usize,
    data: Vec<[f32; 3]>,
}

impl Lut3d {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let mut size = 0usize;
        let mut data = Vec::new();
        for line in text.lines() {
            let l = line.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            if let Some(n) = l.strip_prefix("LUT_3D_SIZE") {
                size = n.trim().parse()?;
                continue;
            }
            if l.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                continue; // TITLE, DOMAIN_MIN/MAX, LUT_1D_SIZE…
            }
            let v: Vec<f32> = l.split_whitespace().filter_map(|t| t.parse().ok()).collect();
            if v.len() == 3 {
                data.push([v[0], v[1], v[2]]);
            }
        }
        if size < 2 || data.len() != size * size * size {
            anyhow::bail!("not a valid 3D .cube LUT ({} entries for size {size})", data.len());
        }
        Ok(Self { size, data })
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

    /// Trilinear lookup. `.cube` order: red changes fastest.
    /// Edge length of the cube.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Entries with red varying fastest, then green, then blue.
    pub fn entries(&self) -> &[[f32; 3]] {
        &self.data
    }

    fn sample(&self, r: f32, g: f32, b: f32) -> [f32; 3] {
        let n = self.size - 1;
        let f = |v: f32| {
            let x = v.clamp(0.0, 1.0) * n as f32;
            let i = (x.floor() as usize).min(n - 1);
            (i, x - i as f32)
        };
        let (ri, rf) = f(r);
        let (gi, gf) = f(g);
        let (bi, bf) = f(b);
        let at = |r: usize, g: usize, b: usize| self.data[r + g * self.size + b * self.size * self.size];
        let mut out = [0f32; 3];
        for (c, o) in out.iter_mut().enumerate() {
            let c00 = at(ri, gi, bi)[c] * (1.0 - rf) + at(ri + 1, gi, bi)[c] * rf;
            let c10 = at(ri, gi + 1, bi)[c] * (1.0 - rf) + at(ri + 1, gi + 1, bi)[c] * rf;
            let c01 = at(ri, gi, bi + 1)[c] * (1.0 - rf) + at(ri + 1, gi, bi + 1)[c] * rf;
            let c11 = at(ri, gi + 1, bi + 1)[c] * (1.0 - rf) + at(ri + 1, gi + 1, bi + 1)[c] * rf;
            let c0 = c00 * (1.0 - gf) + c10 * gf;
            let c1 = c01 * (1.0 - gf) + c11 * gf;
            *o = c0 * (1.0 - bf) + c1 * bf;
        }
        out
    }
}

/// BT.709 chroma (Cb, Cr) of an RGB colour, each roughly -0.5..0.5.
fn chroma(r: f32, g: f32, b: f32) -> (f32, f32) {
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    ((b - y) / 1.8556, (r - y) / 1.5748)
}

/// Per-pixel operation on BGRA bytes.
fn map_pixels(frame: &VideoFrame, f: impl Fn(&mut [u8]) + Sync) -> VideoFrame {
    let mut data = frame.data.as_ref().clone();
    data.par_chunks_exact_mut(4 * 256).for_each(|chunk| {
        for px in chunk.chunks_exact_mut(4) {
            f(px)
        }
    });
    let rem = data.len() % (4 * 256);
    let len = data.len();
    for px in data[len - rem..].chunks_exact_mut(4) {
        f(px)
    }
    VideoFrame::new(frame.width, frame.height, data)
}

pub fn chroma_key(frame: &VideoFrame, key_rgb: [u8; 3], similarity: u32, smoothness: u32, spill: u32) -> VideoFrame {
    let k = key_rgb.map(|v| v as f32 / 255.0);
    let (kcb, kcr) = chroma(k[0], k[1], k[2]);
    let sim = similarity as f32 / 1000.0;
    let smooth = (smoothness as f32 / 1000.0).max(0.001);
    let spill = (spill as f32 / 1000.0).max(0.001);
    // Same maths as OBS's chroma_key shader.
    map_pixels(frame, |px| {
        let (b, g, r) = (px[0] as f32 / 255.0, px[1] as f32 / 255.0, px[2] as f32 / 255.0);
        let (cb, cr) = chroma(r, g, b);
        let base = ((cb - kcb).powi(2) + (cr - kcr).powi(2)).sqrt() - sim;
        let alpha = (base / smooth).clamp(0.0, 1.0).powf(1.5);
        px[3] = (px[3] as f32 * alpha) as u8;
        let spill_val = (base / spill).clamp(0.0, 1.0).powf(1.5);
        let grey = (0.2126 * r + 0.7152 * g + 0.0722 * b).clamp(0.0, 1.0);
        let mix = |c: f32| ((grey + (c - grey) * spill_val) * 255.0 + 0.5) as u8;
        px[2] = mix(r);
        px[1] = mix(g);
        px[0] = mix(b);
    })
}

/// OBS-style colour correction. Ranges: gamma -3..3, contrast -4..4,
/// brightness -1..1, saturation -1..5, hue -180..180°, opacity 0..1.
pub fn color_correction(
    frame: &VideoFrame,
    gamma: f32,
    contrast: f32,
    brightness: f32,
    saturation: f32,
    hue_shift: f32,
    opacity: f32,
) -> VideoFrame {
    // Tone curve as a 256-entry table: gamma, then contrast about mid-grey, then brightness.
    let g = if gamma < 0.0 { 1.0 - gamma } else { 1.0 / (1.0 + gamma) };
    let c = contrast + 1.0;
    let lut: Vec<f32> = (0..256)
        .map(|i| {
            let v = (i as f32 / 255.0).powf(g);
            ((v - 0.5) * c + 0.5 + brightness).clamp(0.0, 1.0)
        })
        .collect();
    let sat = saturation + 1.0;
    let (sin, cos) = hue_shift.to_radians().sin_cos();
    // Hue rotation about the grey axis (standard YIQ-free approximation).
    let k = 1.0 / 3.0;
    let sq = (1.0f32 / 3.0).sqrt();
    let m = [
        [cos + (1.0 - cos) * k, k * (1.0 - cos) - sq * sin, k * (1.0 - cos) + sq * sin],
        [k * (1.0 - cos) + sq * sin, cos + k * (1.0 - cos), k * (1.0 - cos) - sq * sin],
        [k * (1.0 - cos) - sq * sin, k * (1.0 - cos) + sq * sin, cos + k * (1.0 - cos)],
    ];
    let identity_hue = hue_shift.abs() < 0.01;
    map_pixels(frame, |px| {
        let mut rgb = [lut[px[2] as usize], lut[px[1] as usize], lut[px[0] as usize]];
        if !identity_hue {
            let o = rgb;
            for (i, row) in m.iter().enumerate() {
                rgb[i] = row[0] * o[0] + row[1] * o[1] + row[2] * o[2];
            }
        }
        let y = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
        for v in &mut rgb {
            *v = (y + (*v - y) * sat).clamp(0.0, 1.0);
        }
        px[2] = (rgb[0] * 255.0 + 0.5) as u8;
        px[1] = (rgb[1] * 255.0 + 0.5) as u8;
        px[0] = (rgb[2] * 255.0 + 0.5) as u8;
        px[3] = (px[3] as f32 * opacity.clamp(0.0, 1.0)) as u8;
    })
}

/// Shifts the image by (dx, dy) pixels with wrap-around.
pub fn scroll(frame: &VideoFrame, dx: f32, dy: f32) -> VideoFrame {
    let (w, h) = (frame.width as i64, frame.height as i64);
    let ox = (dx.round() as i64).rem_euclid(w) as usize;
    let oy = (dy.round() as i64).rem_euclid(h) as usize;
    if ox == 0 && oy == 0 {
        return frame.clone();
    }
    let row = (w * 4) as usize;
    let src = frame.data.as_slice();
    let mut out = vec![0u8; src.len()];
    out.par_chunks_exact_mut(row).enumerate().for_each(|(y, dst)| {
        let sy = (y + h as usize - oy) % h as usize;
        let s = &src[sy * row..(sy + 1) * row];
        let split = (w as usize - ox) * 4;
        dst[ox * 4..].copy_from_slice(&s[..split]);
        dst[..ox * 4].copy_from_slice(&s[split..]);
    });
    VideoFrame::new(frame.width, frame.height, out)
}

pub fn apply_lut(frame: &VideoFrame, lut: &Lut3d, amount: f32) -> VideoFrame {
    let a = amount.clamp(0.0, 1.0);
    map_pixels(frame, |px| {
        let (r, g, b) = (px[2] as f32 / 255.0, px[1] as f32 / 255.0, px[0] as f32 / 255.0);
        let o = lut.sample(r, g, b);
        px[2] = ((r + (o[0] - r) * a) * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
        px[1] = ((g + (o[1] - g) * a) * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
        px[0] = ((b + (o[2] - b) * a) * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
    })
}

/// One filter step in a form the GPU compositor can run on a layer's texture. The same data drives
/// the CPU fallback ([`GpuFilter::apply_cpu`]), so both produce the same picture.
#[derive(Clone)]
pub enum GpuFilter {
    ChromaKey { key_rgb: [u8; 3], similarity: u32, smoothness: u32, spill: u32 },
    ColorCorrection { gamma: f32, contrast: f32, brightness: f32, saturation: f32, hue_shift: f32, opacity: f32 },
    /// Offset in pixels (wraps).
    Scroll { dx: f32, dy: f32 },
    Lut { lut: std::sync::Arc<Lut3d>, amount: f32 },
    Matte { mask: std::sync::Arc<crate::matte::Mask>, threshold: f32, softness: f32 },
}

impl PartialEq for GpuFilter {
    fn eq(&self, other: &Self) -> bool {
        use GpuFilter::*;
        match (self, other) {
            (ChromaKey { key_rgb: a, similarity: b, smoothness: c, spill: d }, ChromaKey { key_rgb: a2, similarity: b2, smoothness: c2, spill: d2 }) => (a, b, c, d) == (a2, b2, c2, d2),
            (ColorCorrection { gamma: a, contrast: b, brightness: c, saturation: d, hue_shift: e, opacity: f }, ColorCorrection { gamma: a2, contrast: b2, brightness: c2, saturation: d2, hue_shift: e2, opacity: f2 }) => {
                (a, b, c, d, e, f) == (a2, b2, c2, d2, e2, f2)
            }
            (Scroll { dx, dy }, Scroll { dx: dx2, dy: dy2 }) => (dx, dy) == (dx2, dy2),
            (Lut { lut, amount }, Lut { lut: l2, amount: a2 }) => std::sync::Arc::ptr_eq(lut, l2) && amount == a2,
            (Matte { mask, threshold, softness }, Matte { mask: m2, threshold: t2, softness: s2 }) => mask.version == m2.version && (threshold, softness) == (t2, s2),
            _ => false,
        }
    }
}

impl GpuFilter {
    /// The CPU version of this step (used when there is no GPU compositor).
    pub fn apply_cpu(&self, frame: &VideoFrame) -> VideoFrame {
        match self {
            GpuFilter::ChromaKey { key_rgb, similarity, smoothness, spill } => chroma_key(frame, *key_rgb, *similarity, *smoothness, *spill),
            GpuFilter::ColorCorrection { gamma, contrast, brightness, saturation, hue_shift, opacity } => {
                color_correction(frame, *gamma, *contrast, *brightness, *saturation, *hue_shift, *opacity)
            }
            GpuFilter::Scroll { dx, dy } => scroll(frame, *dx, *dy),
            GpuFilter::Lut { lut, amount } => apply_lut(frame, lut, *amount),
            GpuFilter::Matte { mask, threshold, softness } => crate::matte::apply_mask(frame, mask, *threshold, *softness),
        }
    }
}

/// Runs a plan on the CPU, step by step.
pub fn apply_plan_cpu(frame: &VideoFrame, plan: &[GpuFilter]) -> VideoFrame {
    plan.iter().fold(frame.clone(), |f, op| op.apply_cpu(&f))
}

/// Input buffer, settings, output and error of the last filter run.
type FilterMemo = (std::sync::Arc<Vec<u8>>, VideoFilters, VideoFrame, Option<String>);

/// A source's video filters plus the state they need (scroll clock, LUTs).
pub struct VideoFilterChain {
    start: Instant,
    lut: Option<(PathBuf, Result<std::sync::Arc<Lut3d>, String>)>,
    /// Plugin instances by (plugin id, settings), recreated when settings change.
    plugins: Vec<((String, String), Result<ferrous_plugins::Instance, String>)>,
    /// Last input buffer, filter settings and result, for chains that are a
    /// pure function of their input (no scroll clock, no plugins).
    memo: Option<FilterMemo>,
    /// Background removal: the model thread (alive only while the filter is
    /// on) and the last masked result, keyed by input pixels, mask and settings.
    matte: Option<crate::matte::MatteWorker>,
    matte_memo: Option<MatteMemo>,
}

/// (input pixels, mask version, threshold, softness) → masked frame.
type MatteMemo = (std::sync::Arc<Vec<u8>>, u64, f32, f32, VideoFrame);

impl Default for VideoFilterChain {
    fn default() -> Self {
        Self { start: Instant::now(), lut: None, plugins: Vec::new(), memo: None, matte: None, matte_memo: None }
    }
}

impl VideoFilterChain {
    /// Applies the enabled filters in order. Returns the first error (e.g.
    /// a LUT that won't load) alongside the best-effort frame.
    pub fn apply(&mut self, frame: VideoFrame, filters: &VideoFilters) -> (VideoFrame, Option<String>) {
        // A static source through stateless filters gives the same result
        // every tick; reuse it rather than re-filtering every pixel.
        let pure = filters.iter().filter(|f| f.enabled).all(|f| !matches!(f.filter, VideoFilter::Scroll { .. } | VideoFilter::Plugin { .. } | VideoFilter::BackgroundRemoval { .. }));
        if pure {
            if let Some((input, cfg, out, err)) = &self.memo {
                if std::sync::Arc::ptr_eq(input, &frame.data) && cfg == filters {
                    return (out.clone(), err.clone());
                }
            }
        }
        let input = frame.data.clone();
        let (out, err) = self.run(frame, filters);
        self.memo = pure.then(|| (input, filters.clone(), out.clone(), err.clone()));
        (out, err)
    }

    /// The enabled filters as GPU steps, in order, plus the first error. `None` when the chain has a
    /// step only the CPU can run (a plugin): use [`VideoFilterChain::apply`] for that one.
    pub fn plan(&mut self, frame: &VideoFrame, filters: &VideoFilters) -> Option<(Vec<GpuFilter>, Option<String>)> {
        if filters.iter().any(|f| f.enabled && matches!(f.filter, VideoFilter::Plugin { .. })) {
            return None;
        }
        let mut err = None;
        let mut plan = Vec::new();
        if !filters.iter().any(|fl| fl.enabled && matches!(fl.filter, VideoFilter::BackgroundRemoval { .. })) {
            self.matte = None;
        }
        for filter in filters.iter().filter(|f| f.enabled) {
            match &filter.filter {
                VideoFilter::BackgroundRemoval { threshold, softness, smoothing, max_fps } => {
                    let worker = self.matte.get_or_insert_with(crate::matte::MatteWorker::start);
                    worker.submit(frame, *max_fps, *smoothing);
                    if let Some(e) = worker.error() {
                        err.get_or_insert(e);
                    }
                    // No mask yet (model still loading): show the picture as it is.
                    if let Some(mask) = worker.mask() {
                        plan.push(GpuFilter::Matte { mask, threshold: *threshold, softness: *softness });
                    }
                }
                VideoFilter::ChromaKey { key_rgb, similarity, smoothness, spill } => {
                    plan.push(GpuFilter::ChromaKey { key_rgb: *key_rgb, similarity: *similarity, smoothness: *smoothness, spill: *spill })
                }
                VideoFilter::ColorCorrection { gamma, contrast, brightness, saturation, hue_shift, opacity } => plan.push(GpuFilter::ColorCorrection {
                    gamma: *gamma,
                    contrast: *contrast,
                    brightness: *brightness,
                    saturation: *saturation,
                    hue_shift: *hue_shift,
                    opacity: *opacity,
                }),
                VideoFilter::Scroll { speed_x, speed_y } => {
                    let t = self.start.elapsed().as_secs_f32();
                    plan.push(GpuFilter::Scroll { dx: speed_x * t, dy: speed_y * t });
                }
                VideoFilter::Lut { path, amount } => {
                    if self.lut.as_ref().map(|(p, _)| p != path).unwrap_or(true) {
                        self.lut = Some((path.clone(), Lut3d::load(path).map(std::sync::Arc::new).map_err(|e| e.to_string())));
                    }
                    match &self.lut.as_ref().unwrap().1 {
                        Ok(lut) => plan.push(GpuFilter::Lut { lut: lut.clone(), amount: *amount }),
                        Err(e) => {
                            err.get_or_insert_with(|| format!("LUT: {e}"));
                        }
                    }
                }
                VideoFilter::Plugin { .. } => unreachable!("plugin chains return early"),
            }
        }
        Some((plan, err))
    }

    fn run(&mut self, frame: VideoFrame, filters: &VideoFilters) -> (VideoFrame, Option<String>) {
        let mut f = frame;
        let mut err = None;
        // Drop plugin instances no longer in the chain.
        self.plugins.retain(|(k, _)| {
            filters.iter().any(|fl| fl.enabled && matches!(&fl.filter, VideoFilter::Plugin { plugin, settings } if (plugin, settings) == (&k.0, &k.1)))
        });
        // The model thread only lives while a Background Removal filter is on.
        if !filters.iter().any(|fl| fl.enabled && matches!(fl.filter, VideoFilter::BackgroundRemoval { .. })) {
            self.matte = None;
            self.matte_memo = None;
        }
        for filter in filters.iter().filter(|f| f.enabled) {
            f = match &filter.filter {
                VideoFilter::BackgroundRemoval { threshold, softness, smoothing, max_fps } => {
                    let worker = self.matte.get_or_insert_with(crate::matte::MatteWorker::start);
                    worker.submit(&f, *max_fps, *smoothing);
                    if let Some(e) = worker.error() {
                        err.get_or_insert(e);
                    }
                    match worker.mask() {
                        // No mask yet (model still loading): show the picture as it is.
                        None => f,
                        Some(mask) => match &self.matte_memo {
                            Some((input, v, t, s, out)) if std::sync::Arc::ptr_eq(input, &f.data) && *v == mask.version && t == threshold && s == softness => out.clone(),
                            _ => {
                                let out = crate::matte::apply_mask(&f, &mask, *threshold, *softness);
                                self.matte_memo = Some((f.data.clone(), mask.version, *threshold, *softness, out.clone()));
                                out
                            }
                        },
                    }
                }
                VideoFilter::ChromaKey { key_rgb, similarity, smoothness, spill } => {
                    chroma_key(&f, *key_rgb, *similarity, *smoothness, *spill)
                }
                VideoFilter::ColorCorrection { gamma, contrast, brightness, saturation, hue_shift, opacity } => {
                    color_correction(&f, *gamma, *contrast, *brightness, *saturation, *hue_shift, *opacity)
                }
                VideoFilter::Scroll { speed_x, speed_y } => {
                    let t = self.start.elapsed().as_secs_f32();
                    scroll(&f, speed_x * t, speed_y * t)
                }
                VideoFilter::Plugin { plugin, settings } => {
                    let key = (plugin.clone(), settings.clone());
                    if !self.plugins.iter().any(|(k, _)| *k == key) {
                        let inst = ferrous_plugins::Instance::new(plugin, settings, ferrous_plugins::Kind::VideoFilter).map_err(|e| e.to_string());
                        self.plugins.push((key.clone(), inst));
                    }
                    match &mut self.plugins.iter_mut().find(|(k, _)| *k == key).unwrap().1 {
                        Ok(inst) => inst.filter_video(&f),
                        Err(e) => {
                            err.get_or_insert_with(|| e.clone());
                            f
                        }
                    }
                }
                VideoFilter::Lut { path, amount } => {
                    if self.lut.as_ref().map(|(p, _)| p != path).unwrap_or(true) {
                        self.lut = Some((path.clone(), Lut3d::load(path).map(std::sync::Arc::new).map_err(|e| e.to_string())));
                    }
                    match &self.lut.as_ref().unwrap().1 {
                        Ok(lut) => apply_lut(&f, lut, *amount),
                        Err(e) => {
                            err.get_or_insert_with(|| format!("LUT: {e}"));
                            f
                        }
                    }
                }
            };
        }
        (f, err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(f: &VideoFrame, i: usize) -> [u8; 4] {
        f.data[i * 4..i * 4 + 4].try_into().unwrap()
    }

    #[test]
    fn chroma_key_removes_green_keeps_skin() {
        let mut data = Vec::new();
        data.extend_from_slice(&[30, 200, 40, 255]); // green screen (BGRA)
        data.extend_from_slice(&[120, 150, 220, 255]); // skin tone
        let f = VideoFrame::new(2, 1, data);
        let out = chroma_key(&f, [0, 255, 0], 400, 80, 100);
        assert!(px(&out, 0)[3] < 10, "green kept alpha {}", px(&out, 0)[3]);
        assert!(px(&out, 1)[3] > 245, "skin lost alpha {}", px(&out, 1)[3]);
    }

    #[test]
    fn color_correction_identity_and_brightness() {
        let f = VideoFrame::solid(4, 4, [10, 128, 240, 255]);
        let same = color_correction(&f, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0);
        for (a, b) in same.data.iter().zip(f.data.iter()) {
            assert!((*a as i32 - *b as i32).abs() <= 1);
        }
        let bright = color_correction(&f, 0.0, 0.0, 0.2, 0.0, 0.0, 1.0);
        assert!(px(&bright, 0)[1] > 170);
        let grey = color_correction(&f, 0.0, 0.0, 0.0, -1.0, 0.0, 1.0);
        let p = px(&grey, 0);
        assert!(p[0] == p[1] && p[1] == p[2], "{p:?}");
    }

    #[test]
    fn scroll_wraps() {
        let mut data = vec![0u8; 3 * 4];
        data[0] = 1; // pixel 0
        let f = VideoFrame::new(3, 1, data);
        let s = scroll(&f, 1.0, 0.0);
        assert_eq!(px(&s, 1)[0], 1);
        let s = scroll(&f, -1.0, 0.0);
        assert_eq!(px(&s, 2)[0], 1);
    }

    #[test]
    fn lut_identity_and_invert() {
        let mut ident = String::from("TITLE \"id\"\nLUT_3D_SIZE 2\n");
        let mut inv = String::from("LUT_3D_SIZE 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    ident.push_str(&format!("{r} {g} {b}\n"));
                    inv.push_str(&format!("{} {} {}\n", 1 - r, 1 - g, 1 - b));
                }
            }
        }
        let f = VideoFrame::solid(2, 2, [20, 100, 200, 255]);
        let same = apply_lut(&f, &Lut3d::parse(&ident).unwrap(), 1.0);
        assert_eq!(px(&same, 0), [20, 100, 200, 255]);
        let neg = apply_lut(&f, &Lut3d::parse(&inv).unwrap(), 1.0);
        assert_eq!(px(&neg, 0), [235, 155, 55, 255]);
        assert!(Lut3d::parse("LUT_3D_SIZE 2\n0 0 0\n").is_err());
    }
}
