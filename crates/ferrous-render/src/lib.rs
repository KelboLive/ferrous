//! CPU scene compositor.
//!
//! Layers are cropped, bilinearly scaled, alpha blended and drawn bottom-up
//! onto a BGRA canvas, parallelised across rows with rayon. This is the
//! portable reference path; a wgpu (D3D12/Vulkan) compositor is the planned
//! fast path and must produce the same output.

pub mod filters;
pub mod matte;
mod gpu;
mod gpufx;
pub mod transitions;

use ferrous_core::{Transform, VideoFrame};
use rayon::prelude::*;

pub use ferrous_core::frame::FramePool;
pub use gpu::{prefer_dx12, GpuCompositor};
pub use ferrous_nvenc;

pub struct Composed {
    pub frame: VideoFrame,
    /// Per layer, same order as the input.
    pub cpu_ms: Vec<f32>,
    /// Per layer; empty when GPU timing isn't available.
    pub gpu_ms: Vec<f32>,
    pub gpu_total_ms: Option<f32>,
}

/// Whichever compositor is in use. GPU when available, CPU otherwise.
///
/// Compositions are memoised: when every layer is the same frame buffer
/// with the same transform as a recent composition, that result is
/// returned as-is. Static scenes (images, text, colour, a paused browser)
/// then cost nothing per tick: no redraw, no GPU readback, and the output
/// keeps its buffer, so downstream consumers can skip work too.
pub struct Renderer {
    backend: Backend,
    memo: Memo,
    pool: FramePool,
    /// Read the GPU result a frame late instead of waiting for it.
    pipelined: bool,
    /// Have the GPU also produce the NV12 picture for encoders.
    nv12: bool,
    /// See [`Renderer::set_bgra`].
    bgra: bool,
    /// See [`Renderer::set_nv12_jfif`].
    nv12_jfif: bool,
    /// See [`Renderer::set_preview_width`].
    preview: Option<u32>,
    /// Inputs of the frame in flight on the GPU, so its result can be
    /// memoised under the right key when it arrives.
    in_flight: Option<Inputs>,
}

type Inputs = Vec<(std::sync::Arc<Vec<u8>>, u32, u32, Transform, Vec<filters::GpuFilter>)>;

fn inputs_of(layers: &[Layer<'_>]) -> Inputs {
    layers.iter().map(|l| (l.frame.data.clone(), l.frame.width, l.frame.height, l.transform, l.filters.to_vec())).collect()
}

/// Each filtered layer's frame with its filters run on the CPU (`None` for an unfiltered layer), for
/// compositors that can't run them as GPU passes.
fn bake_filters(layers: &[Layer<'_>]) -> Vec<Option<VideoFrame>> {
    layers.iter().map(|l| (!l.filters.is_empty()).then(|| filters::apply_plan_cpu(l.frame, l.filters))).collect()
}

/// `layers` drawing the baked frames in place of the originals, with no filters left to run.
fn baked_layers<'a>(layers: &[Layer<'a>], baked: &'a [Option<VideoFrame>]) -> Vec<Layer<'a>> {
    layers.iter().zip(baked).map(|(l, b)| Layer { frame: b.as_ref().unwrap_or(l.frame), transform: l.transform, filters: &[] }).collect()
}

pub enum Backend {
    Cpu(Compositor),
    Gpu(Box<GpuCompositor>),
}

impl Renderer {
    /// `prefer_gpu = false` forces the CPU path.
    pub fn new(width: u32, height: u32, prefer_gpu: bool) -> (Self, Option<String>) {
        let (backend, err) = Self::backend(width, height, prefer_gpu);
        (Self { backend, memo: Memo::default(), pool: FramePool::default(), pipelined: false, nv12: false, bgra: true, nv12_jfif: false, preview: None, in_flight: None }, err)
    }

    fn backend(width: u32, height: u32, prefer_gpu: bool) -> (Backend, Option<String>) {
        if prefer_gpu {
            return match GpuCompositor::new(width, height) {
                Ok(g) if g.is_software() && std::env::var_os("FERROUS_FORCE_GPU").is_none() => (
                    Backend::Cpu(Compositor::new(width, height)),
                    Some(format!("only a software GPU ({}) is available", g.adapter_name())),
                ),
                Ok(g) => (Backend::Gpu(Box::new(g)), None),
                Err(e) => (Backend::Cpu(Compositor::new(width, height)), Some(e.to_string())),
            };
        }
        (Backend::Cpu(Compositor::new(width, height)), None)
    }

    pub fn compose(&mut self, layers: &[Layer<'_>]) -> VideoFrame {
        self.compose_timed(layers).frame
    }

    /// True when compositing on the GPU.
    pub fn is_gpu(&self) -> bool {
        matches!(self.backend, Backend::Gpu(_))
    }

    /// True when frames that live only in a shared GPU texture can be drawn: the GPU compositor on
    /// the same adapter as the capture. Turns false for good if opening one fails.
    /// Marks the next composition as the program picture for output frame `pts` (GPU encoders copy it).
    pub fn set_encode_pts(&mut self, pts: Option<u64>) {
        if let Backend::Gpu(g) = &mut self.backend {
            g.set_encode_pts(pts);
        }
    }

    /// Starts an in-process hardware encoder fed straight from the canvas texture (see
    /// [`GpuCompositor::add_encoder`]).
    pub fn add_gpu_encoder(&mut self, config: ferrous_nvenc::Config, sink: ferrous_nvenc::Sink) -> anyhow::Result<std::sync::Arc<ferrous_nvenc::Stats>> {
        let r = match &mut self.backend {
            Backend::Gpu(g) => g.add_encoder(config, sink),
            Backend::Cpu(_) => anyhow::bail!("the CPU compositor can't feed a GPU encoder"),
        };
        // A memoised (static) scene would never be drawn again, so the new encoder would never get
        // its first picture and FFmpeg would wait for video forever: draw the next frame for real.
        self.memo.entries.clear();
        r
    }

    /// Once per output frame, after the program picture (see [`GpuCompositor::encoders_tick`]).
    pub fn encoders_tick(&mut self, pts: u64) {
        if let Backend::Gpu(g) = &mut self.backend {
            g.encoders_tick(pts);
        }
    }

    pub fn gpu_encoder_count(&self) -> usize {
        match &self.backend {
            Backend::Gpu(g) => g.encoder_count(),
            Backend::Cpu(_) => 0,
        }
    }

    pub fn takes_gpu_frames(&self) -> bool {
        matches!(&self.backend, Backend::Gpu(g) if g.shared_ok())
    }

    /// Opens a shared GPU frame ahead of drawing it. False means: use the source's CPU frame instead.
    pub fn accepts_gpu_frame(&mut self, frame: &VideoFrame) -> bool {
        match &mut self.backend {
            Backend::Gpu(g) => g.accepts_shared(frame),
            Backend::Cpu(_) => frame.gpu.is_none(),
        }
    }

    /// Reads GPU results a frame late, so the CPU never waits on the GPU.
    /// For one steady stream of compositions only (the program output): any
    /// other composition in between would be mistaken for it, so callers
    /// turn this off around those. No effect on the CPU path.
    pub fn set_pipelined(&mut self, on: bool) {
        self.pipelined = on;
    }

    /// Whether the GPU path should also produce NV12 for the encoders.
    pub fn set_nv12(&mut self, on: bool) {
        self.nv12 = on;
    }

    /// Whether the GPU path copies the picture back as BGRA (default on). Turn it off for frames
    /// where nothing on the CPU looks at the picture (no preview due, no virtual camera, outputs on
    /// NV12 or the GPU encoders): the returned frame then has empty `data`. No effect on the CPU path.
    pub fn set_bgra(&mut self, on: bool) {
        self.bgra = on;
    }

    /// Make the GPU's NV12 in JPEG's YCbCr (BT.601 full range) for the JPEG Link, instead of video's
    /// (BT.709 limited); frames say which in `nv12_jfif`. No effect on the CPU path.
    pub fn set_nv12_jfif(&mut self, on: bool) {
        self.nv12_jfif = on;
    }

    /// GPU path: with the next compositions, also draw an RGBA copy at most `width` wide for the UI
    /// preview and read back only that (see [`Renderer::take_preview`]). `None` stops it.
    pub fn set_preview_width(&mut self, width: Option<u32>) {
        self.preview = width;
    }

    /// The GPU-made preview of the frame last returned, if one was drawn: (width, height, RGBA).
    pub fn take_preview(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        match &mut self.backend {
            Backend::Gpu(g) => g.take_preview(),
            Backend::Cpu(_) => None,
        }
    }

    /// Composes and reports per-layer cost: measured CPU milliseconds on the
    /// CPU path; on the GPU path, the pass's measured GPU time split across
    /// layers by covered area (an estimate, flagged as such). A memo hit
    /// reports zero cost, because nothing was drawn.
    pub fn compose_timed(&mut self, layers: &[Layer<'_>]) -> Composed {
        // A memoised frame only counts if it has what this frame needs.
        let gpu = matches!(self.backend, Backend::Gpu(_));
        // (A preview from the GPU needs a fresh drawing, or a memoised frame with BGRA to scale.)
        let (need_bgra, need_nv12) = (self.bgra || !gpu || self.preview.is_some(), self.nv12 && gpu);
        if let Some(frame) = self.memo.get(false, layers).filter(|f| (!need_bgra || !f.data.is_empty()) && (!need_nv12 || (f.nv12.is_some() && f.nv12_jfif == self.nv12_jfif))) {
            return Composed { frame, cpu_ms: vec![0.0; layers.len()], gpu_ms: Vec::new(), gpu_total_ms: None };
        }
        let out = match &mut self.backend {
            Backend::Cpu(c) => {
                let baked = bake_filters(layers);
                let plain = baked_layers(layers, &baked);
                let (frame, cpu_ms) = c.compose_timed_with(&plain, &mut self.pool);
                Composed { frame, cpu_ms, gpu_ms: Vec::new(), gpu_total_ms: None }
            }
            Backend::Gpu(g) => {
                let t = std::time::Instant::now();
                let (w, h) = g.size();
                g.set_want_bgra(self.bgra);
                g.set_nv12_jfif(self.nv12_jfif);
                g.set_preview_width(self.preview);
                let (buf, _) = if self.bgra { self.pool.take((w * h * 4) as usize) } else { (Vec::new(), false) };
                let frame = if self.pipelined {
                    let (frame, is_current) = g.compose_pipelined(layers, buf, self.nv12);
                    // The frame that came back belongs to the inputs it was
                    // submitted with; remember it under those.
                    let owner = if is_current { Some(inputs_of(layers)) } else { self.in_flight.take() };
                    if let Some(inputs) = owner {
                        self.memo.put_inputs(false, inputs, &frame);
                    }
                    self.in_flight = Some(inputs_of(layers));
                    frame
                } else {
                    g.discard_pending();
                    self.in_flight = None;
                    g.compose_into(layers, buf, self.nv12)
                };
                let wall = t.elapsed().as_secs_f32() * 1000.0;
                let areas: Vec<f32> = layers
                    .iter()
                    .map(|l| {
                        let (w, h) = l.transform.output_size(l.frame.width, l.frame.height);
                        (w * h).max(0.0)
                    })
                    .collect();
                let total_area: f32 = areas.iter().sum::<f32>().max(1.0);
                let gpu_total = g.last_gpu_ms();
                let share = |a: &f32| a / total_area;
                Composed {
                    frame,
                    // Upload + submit + readback CPU time, shared the same way.
                    cpu_ms: areas.iter().map(|a| wall * share(a)).collect(),
                    gpu_ms: gpu_total.map(|g| areas.iter().map(|a| g * share(a)).collect()).unwrap_or_default(),
                    gpu_total_ms: gpu_total,
                }
            }
        };
        if !self.pipelined || matches!(self.backend, Backend::Cpu(_)) {
            self.memo.put(false, layers, &out.frame);
        }
        // A passed-through source frame isn't ours to recycle.
        if !layers.iter().any(|l| std::sync::Arc::ptr_eq(&l.frame.data, &out.frame.data)) {
            self.pool.track(&out.frame);
        }
        out
    }

    /// Composes onto a transparent canvas (for nested scenes, which must
    /// let what's beneath show through). Always on the CPU path.
    pub fn compose_transparent(&mut self, layers: &[Layer<'_>]) -> VideoFrame {
        if let Some(frame) = self.memo.get(true, layers) {
            return frame;
        }
        let (w, h) = self.size();
        let mut c = Compositor::new(w, h);
        c.background = [0, 0, 0, 0];
        let baked = bake_filters(layers);
        let (frame, _) = c.compose_timed_with(&baked_layers(layers, &baked), &mut self.pool);
        self.memo.put(true, layers, &frame);
        if !layers.iter().any(|l| std::sync::Arc::ptr_eq(&l.frame.data, &frame.data)) {
            self.pool.track(&frame);
        }
        frame
    }

    pub fn size(&self) -> (u32, u32) {
        match &self.backend {
            Backend::Cpu(c) => c.size(),
            Backend::Gpu(g) => g.size(),
        }
    }

    /// The last GPU pass time, when on the GPU path with timestamp queries.
    pub fn last_gpu_ms(&self) -> Option<f32> {
        match &self.backend {
            Backend::Gpu(g) => g.last_gpu_ms(),
            Backend::Cpu(_) => None,
        }
    }

    pub fn describe(&self) -> String {
        match &self.backend {
            Backend::Cpu(_) => "CPU".into(),
            Backend::Gpu(g) => format!("GPU: {}", g.adapter_name()),
        }
    }
}

/// Recent compositions keyed by their inputs. Entries hold the input
/// buffers, so a key's pointers can't be reused by new frames while it
/// lives; the oldest entry goes first once full.
#[derive(Default)]
struct Memo {
    entries: Vec<MemoEntry>,
    tick: u64,
}

struct MemoEntry {
    transparent: bool,
    inputs: Inputs,
    output: VideoFrame,
    used: u64,
}

const MEMO_ENTRIES: usize = 8;

impl Memo {
    fn matches(e: &MemoEntry, transparent: bool, layers: &[Layer<'_>]) -> bool {
        e.transparent == transparent
            && e.inputs.len() == layers.len()
            && e.inputs.iter().zip(layers).all(|((d, w, h, t, f), l)| {
                std::sync::Arc::ptr_eq(d, &l.frame.data) && *w == l.frame.width && *h == l.frame.height && *t == l.transform && f.as_slice() == l.filters
            })
    }

    fn get(&mut self, transparent: bool, layers: &[Layer<'_>]) -> Option<VideoFrame> {
        self.tick += 1;
        let tick = self.tick;
        let e = self.entries.iter_mut().find(|e| Self::matches(e, transparent, layers))?;
        e.used = tick;
        Some(e.output.clone())
    }

    fn put(&mut self, transparent: bool, layers: &[Layer<'_>], output: &VideoFrame) {
        self.put_inputs(transparent, inputs_of(layers), output);
    }

    fn put_inputs(&mut self, transparent: bool, inputs: Inputs, output: &VideoFrame) {
        let entry = MemoEntry { transparent, inputs, output: output.clone(), used: self.tick };
        // Same layout with fresher buffers (a live source moved on):
        // replace that entry, so live scenes hold one slot, not eight.
        let same_layout = |e: &MemoEntry| {
            e.transparent == transparent
                && e.inputs.len() == entry.inputs.len()
                && e.inputs.iter().zip(&entry.inputs).all(|((_, w, h, t, f), (_, w2, h2, t2, f2))| (w, h, t) == (w2, h2, t2) && f.len() == f2.len())
        };
        if let Some(e) = self.entries.iter_mut().find(|e| same_layout(e)) {
            *e = entry;
            return;
        }
        if self.entries.len() >= MEMO_ENTRIES {
            if let Some(i) = self.entries.iter().enumerate().min_by_key(|(_, e)| e.used).map(|(i, _)| i) {
                self.entries.swap_remove(i);
            }
        }
        self.entries.push(entry);
    }
}

pub struct Layer<'a> {
    pub frame: &'a VideoFrame,
    pub transform: Transform,
    /// Video filters to run on `frame` before it is drawn: on the GPU as passes over the texture, on
    /// the CPU through the matching CPU filters. Empty for an unfiltered layer.
    pub filters: &'a [filters::GpuFilter],
}

pub struct Compositor {
    width: u32,
    height: u32,
    background: [u8; 4],
}

impl Compositor {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, background: [0, 0, 0, 255] }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn compose(&self, layers: &[Layer<'_>]) -> VideoFrame {
        self.compose_timed(layers).0
    }

    /// Also returns the milliseconds spent drawing each layer.
    pub fn compose_timed(&self, layers: &[Layer<'_>]) -> (VideoFrame, Vec<f32>) {
        self.compose_timed_with(layers, &mut FramePool::default())
    }

    /// [`Compositor::compose_timed`], drawing into a buffer from `pool`.
    pub fn compose_timed_with(&self, layers: &[Layer<'_>], pool: &mut FramePool) -> (VideoFrame, Vec<f32>) {
        let (cw, ch) = (self.width as usize, self.height as usize);
        // An opaque, unscaled, full-canvas bottom layer (a camera or display
        // capture at canvas size) hides the background completely: start
        // from it instead of painting and then overdrawing every pixel, and
        // when it's the only layer, pass its buffer through untouched.
        if let Some(first) = layers.first() {
            let t0 = std::time::Instant::now();
            if self.covers_opaquely(first) {
                let mut times = vec![0.0; layers.len()];
                if layers.len() == 1 {
                    times[0] = t0.elapsed().as_secs_f32() * 1000.0;
                    return (first.frame.clone(), times);
                }
                let (mut canvas, _) = pool.take(cw * ch * 4);
                canvas.copy_from_slice(&first.frame.data);
                times[0] = t0.elapsed().as_secs_f32() * 1000.0;
                for (layer, ms) in layers.iter().zip(times.iter_mut()).skip(1) {
                    let t = std::time::Instant::now();
                    self.draw(&mut canvas, layer);
                    *ms = t.elapsed().as_secs_f32() * 1000.0;
                }
                return (VideoFrame::new(self.width, self.height, canvas), times);
            }
        }
        // A fresh buffer is already zeroed (transparent); a recycled one, or
        // any other background colour, needs painting.
        let (mut canvas, fresh) = pool.take(cw * ch * 4);
        if !(fresh && self.background == [0, 0, 0, 0]) {
            canvas.par_chunks_exact_mut(cw * 4).for_each(|row| {
                for px in row.chunks_exact_mut(4) {
                    px.copy_from_slice(&self.background);
                }
            });
        }
        let mut times = Vec::with_capacity(layers.len());
        for layer in layers {
            let t = std::time::Instant::now();
            self.draw(&mut canvas, layer);
            times.push(t.elapsed().as_secs_f32() * 1000.0);
        }
        (VideoFrame::new(self.width, self.height, canvas), times)
    }

    /// True when `layer` paints every canvas pixel at full opacity with
    /// its pixels unchanged: same size, no crop/scale/offset/rotation,
    /// and every source pixel opaque.
    fn covers_opaquely(&self, layer: &Layer<'_>) -> bool {
        let (f, t) = (layer.frame, &layer.transform);
        (f.width, f.height) == (self.width, self.height)
            && t.crop == [0; 4]
            && t.x == 0.0
            && t.y == 0.0
            && t.scale_x == 1.0
            && t.scale_y == 1.0
            && t.opacity >= 1.0
            && !t.is_rotated()
            && f.data.par_chunks(1 << 16).all(|c| c.chunks_exact(4).all(|p| p[3] == 255))
    }

    fn draw(&self, canvas: &mut [u8], layer: &Layer<'_>) {
        let f = layer.frame;
        let t = &layer.transform;
        let [cl, ct, cr, cb] = t.crop;
        let src_w = f.width.saturating_sub(cl + cr);
        let src_h = f.height.saturating_sub(ct + cb);
        if src_w == 0 || src_h == 0 || t.scale_x <= 0.0 || t.scale_y <= 0.0 || t.opacity <= 0.0 {
            return;
        }
        if t.is_rotated() {
            return self.draw_rotated(canvas, layer);
        }
        let dst_w = src_w as f32 * t.scale_x;
        let dst_h = src_h as f32 * t.scale_y;
        let x0 = t.x.max(0.0).floor() as i64;
        let y0 = t.y.max(0.0).floor() as i64;
        let x1 = ((t.x + dst_w).ceil() as i64).min(self.width as i64);
        let y1 = ((t.y + dst_h).ceil() as i64).min(self.height as i64);
        if x1 <= x0 || y1 <= y0 {
            return;
        }

        let cw = self.width as usize;
        let sw = f.width as usize;
        let src = f.data.as_slice();
        let opacity = (t.opacity.clamp(0.0, 1.0) * 256.0) as u32;
        let identity = (t.scale_x - 1.0).abs() < 1e-6
            && (t.scale_y - 1.0).abs() < 1e-6
            && t.x.fract() == 0.0
            && t.y.fract() == 0.0;

        // Per-column source coordinates in 16.8 fixed point, shared by all rows.
        let inv_sx = 1.0 / t.scale_x;
        let inv_sy = 1.0 / t.scale_y;
        let cols: Vec<(usize, usize, u32)> = (x0..x1)
            .map(|x| {
                let sx = ((x as f32 + 0.5 - t.x) * inv_sx - 0.5).clamp(0.0, (src_w - 1) as f32);
                let i = sx.floor() as usize;
                let frac = ((sx - i as f32) * 256.0) as u32;
                (cl as usize + i, cl as usize + (i + 1).min(src_w as usize - 1), frac)
            })
            .collect();

        // Shrunk layers average an n x n grid of bilinear taps over each pixel's footprint (as the GPU
        // path does), instead of skipping texels.
        let shrink_n: Option<usize> = {
            let r = inv_sx.max(inv_sy);
            (r > 1.25).then(|| (r.ceil() as usize).min(8))
        };

        canvas
            .par_chunks_exact_mut(cw * 4)
            .enumerate()
            .skip(y0 as usize)
            .take((y1 - y0) as usize)
            .for_each(|(y, row)| {
                let sy = ((y as f32 + 0.5 - t.y) * inv_sy - 0.5).clamp(0.0, (src_h - 1) as f32);
                let iy = sy.floor() as usize;
                let fy = ((sy - iy as f32) * 256.0) as u32;
                let r0 = (ct as usize + iy) * sw * 4;
                let r1 = (ct as usize + (iy + 1).min(src_h as usize - 1)) * sw * 4;
                // Unscaled, fully opaque row: a straight copy gives the same
                // pixels as the blend below, an order of magnitude faster.
                if identity && opacity >= 256 && !cols.is_empty() {
                    let (a0, n) = (cols[0].0, cols.len());
                    let s = &src[r0 + a0 * 4..r0 + (a0 + n) * 4];
                    if s.chunks_exact(4).all(|p| p[3] == 255) {
                        row[x0 as usize * 4..(x0 as usize + n) * 4].copy_from_slice(s);
                        return;
                    }
                }
                let (s0, s1) = (&src[r0..r0 + sw * 4], &src[r1..r1 + sw * 4]);
                let pixel = |s: &[u8], i: usize| -> [u8; 4] { s[i * 4..i * 4 + 4].try_into().unwrap() };
                for (k, &(a, b, fx)) in cols.iter().enumerate() {
                    let d = (x0 as usize + k) * 4;
                    let mut px = [0u32; 4];
                    if let Some(n) = shrink_n {
                        let x = x0 as usize + k;
                        let step = 1.0 / n as f32;
                        let mut acc = [0f32; 4];
                        for j in 0..n {
                            let ty = (y as f32 + 0.5 - t.y) * inv_sy - 0.5 + ((j as f32 + 0.5) * step - 0.5) * inv_sy;
                            for i in 0..n {
                                let tx = (x as f32 + 0.5 - t.x) * inv_sx - 0.5 + ((i as f32 + 0.5) * step - 0.5) * inv_sx;
                                let s = bilerp(src, sw, (cl as usize, ct as usize), (src_w as usize, src_h as usize), tx, ty);
                                for c in 0..3 {
                                    acc[c] += s[c] * s[3];
                                }
                                acc[3] += s[3];
                            }
                        }
                        let a = acc[3] * step * step;
                        if acc[3] > 0.0 {
                            for c in 0..3 {
                                px[c] = (acc[c] / acc[3]).round().clamp(0.0, 255.0) as u32;
                            }
                        }
                        px[3] = a.round().clamp(0.0, 255.0) as u32;
                    } else if identity {
                        let p = pixel(s0, a);
                        for c in 0..4 {
                            px[c] = p[c] as u32;
                        }
                    } else {
                        // One bounds check per source pixel, not per channel.
                        let (p00, p01, p10, p11) = (pixel(s0, a), pixel(s0, b), pixel(s1, a), pixel(s1, b));
                        for c in 0..4 {
                            let top = p00[c] as u32 * (256 - fx) + p01[c] as u32 * fx;
                            let bot = p10[c] as u32 * (256 - fx) + p11[c] as u32 * fx;
                            px[c] = (top * (256 - fy) + bot * fy) >> 16;
                        }
                    }
                    let alpha = (px[3] * opacity) >> 8;
                    let out = &mut row[d..d + 4];
                    if alpha >= 255 {
                        out[0] = px[0] as u8;
                        out[1] = px[1] as u8;
                        out[2] = px[2] as u8;
                        out[3] = 255;
                    } else if alpha > 0 && out[3] == 255 {
                        let inv = 255 - alpha;
                        for c in 0..3 {
                            out[c] = ((px[c] * alpha + out[c] as u32 * inv) / 255) as u8;
                        }
                    } else if alpha > 0 {
                        // Straight-alpha "over" onto a translucent canvas.
                        let da = out[3] as u32;
                        let oa = alpha * 255 + da * (255 - alpha); // scaled by 255
                        for c in 0..3 {
                            out[c] = ((px[c] * alpha * 255 + out[c] as u32 * da * (255 - alpha)) / oa.max(1)) as u8;
                        }
                        out[3] = (oa / 255) as u8;
                    }
                }
            });
    }
}

/// Bilinear sample of the cropped source region at (`sx`, `sy`) in region pixels, clamped to its edge.
fn bilerp(src: &[u8], stride_px: usize, origin: (usize, usize), size: (usize, usize), sx: f32, sy: f32) -> [f32; 4] {
    let sx = sx.clamp(0.0, (size.0 - 1) as f32);
    let sy = sy.clamp(0.0, (size.1 - 1) as f32);
    let (ix, iy) = (sx.floor() as usize, sy.floor() as usize);
    let (fx, fy) = (sx - ix as f32, sy - iy as f32);
    let (jx, jy) = ((ix + 1).min(size.0 - 1), (iy + 1).min(size.1 - 1));
    let at = |x: usize, y: usize| -> &[u8] {
        let i = ((origin.1 + y) * stride_px + origin.0 + x) * 4;
        &src[i..i + 4]
    };
    let (p00, p01, p10, p11) = (at(ix, iy), at(jx, iy), at(ix, jy), at(jx, jy));
    let mut out = [0f32; 4];
    for c in 0..4 {
        let top = p00[c] as f32 * (1.0 - fx) + p01[c] as f32 * fx;
        let bot = p10[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
        out[c] = top * (1.0 - fy) + bot * fy;
    }
    out
}

impl Compositor {
    /// Rotated layers: every canvas pixel in the rotated box's bounds is
    /// mapped back into the box, sampled bilinearly, and its alpha scaled by
    /// how much of the pixel the box covers, so edges are antialiased.
    fn draw_rotated(&self, canvas: &mut [u8], layer: &Layer<'_>) {
        let f = layer.frame;
        let t = &layer.transform;
        let [cl, ct, cr, cb] = t.crop;
        let src_w = f.width.saturating_sub(cl + cr) as usize;
        let src_h = f.height.saturating_sub(ct + cb) as usize;
        let (w, h) = t.output_size(f.width, f.height);
        let (bx0, by0, bx1, by1) = t.bounds(f.width, f.height);
        let x0 = (bx0.floor() as i64 - 1).max(0) as usize;
        let y0 = (by0.floor() as i64 - 1).max(0) as usize;
        let x1 = ((bx1.ceil() as i64 + 1).min(self.width as i64)).max(0) as usize;
        let y1 = ((by1.ceil() as i64 + 1).min(self.height as i64)).max(0) as usize;
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let (cx, cy) = (t.x + w / 2.0, t.y + h / 2.0);
        let (c, s) = t.rotation_cs();
        let (hw, hh) = (w / 2.0, h / 2.0);
        let (inv_sx, inv_sy) = (1.0 / t.scale_x, 1.0 / t.scale_y);
        let cw = self.width as usize;
        let sw = f.width as usize;
        let src = f.data.as_slice();
        let opacity = t.opacity.clamp(0.0, 1.0);
        canvas.par_chunks_exact_mut(cw * 4).enumerate().skip(y0).take(y1 - y0).for_each(|(y, row)| {
            let dy = y as f32 + 0.5 - cy;
            for x in x0..x1 {
                let dx = x as f32 + 0.5 - cx;
                // Inverse rotation into the box's frame (origin at its centre).
                let lx = dx * c + dy * s;
                let ly = -dx * s + dy * c;
                let cover = ((hw - lx.abs()).min(hh - ly.abs()) + 0.5).clamp(0.0, 1.0);
                if cover <= 0.0 {
                    continue;
                }
                let sx = ((lx + hw) * inv_sx - 0.5).clamp(0.0, (src_w - 1) as f32);
                let sy = ((ly + hh) * inv_sy - 0.5).clamp(0.0, (src_h - 1) as f32);
                let (ix, iy) = (sx.floor() as usize, sy.floor() as usize);
                let (fx, fy) = (((sx - ix as f32) * 256.0) as u32, ((sy - iy as f32) * 256.0) as u32);
                let (a, b) = (cl as usize + ix, cl as usize + (ix + 1).min(src_w - 1));
                let r0 = (ct as usize + iy) * sw * 4;
                let r1 = (ct as usize + (iy + 1).min(src_h - 1)) * sw * 4;
                let mut px = [0u32; 4];
                for ch in 0..4 {
                    let top = src[r0 + a * 4 + ch] as u32 * (256 - fx) + src[r0 + b * 4 + ch] as u32 * fx;
                    let bot = src[r1 + a * 4 + ch] as u32 * (256 - fx) + src[r1 + b * 4 + ch] as u32 * fx;
                    px[ch] = (top * (256 - fy) + bot * fy) >> 16;
                }
                let alpha = ((px[3] as f32) * opacity * cover).round() as u32;
                blend(&mut row[x * 4..x * 4 + 4], px, alpha.min(255));
            }
        });
    }
}

/// Straight-alpha "over" of `px` (BGRA, alpha given separately) onto `out`.
fn blend(out: &mut [u8], px: [u32; 4], alpha: u32) {
    if alpha >= 255 {
        out[0] = px[0] as u8;
        out[1] = px[1] as u8;
        out[2] = px[2] as u8;
        out[3] = 255;
    } else if alpha > 0 && out[3] == 255 {
        let inv = 255 - alpha;
        for c in 0..3 {
            out[c] = ((px[c] * alpha + out[c] as u32 * inv) / 255) as u8;
        }
    } else if alpha > 0 {
        let da = out[3] as u32;
        let oa = alpha * 255 + da * (255 - alpha);
        for c in 0..3 {
            out[c] = ((px[c] * alpha * 255 + out[c] as u32 * da * (255 - alpha)) / oa.max(1)) as u8;
        }
        out[3] = (oa / 255) as u8;
    }
}

/// Downscales a BGRA frame for the UI preview and converts it to RGBA.
/// Returns `(width, height, rgba)`.
///
/// Each output pixel averages the source pixels under it (up to 4x4
/// samples). Picking one source pixel instead made fine detail shimmer and
/// crawl as it moved, which reads as a choppy preview.
pub fn preview_rgba(frame: &VideoFrame, max_width: u32) -> (u32, u32, Vec<u8>) {
    let scale = (max_width.max(1) as f32 / frame.width as f32).min(1.0);
    let w = ((frame.width as f32 * scale).round() as u32).max(1);
    let h = ((frame.height as f32 * scale).round() as u32).max(1);
    let (sw, sh) = (frame.width as usize, frame.height as usize);
    let src = frame.data.as_slice();
    let mut out = vec![0u8; (w * h * 4) as usize];
    let ratio = 1.0 / scale;
    let taps = (ratio.ceil() as usize).clamp(1, 4);
    // Sample positions inside each output pixel's footprint, per axis.
    let positions = |n: usize, limit: usize| -> Vec<[usize; 4]> {
        (0..n)
            .map(|o| {
                let mut p = [0usize; 4];
                for (t, slot) in p.iter_mut().enumerate().take(taps) {
                    let f = (o as f32 + (t as f32 + 0.5) / taps as f32) * ratio;
                    *slot = (f as usize).min(limit - 1);
                }
                p
            })
            .collect()
    };
    let xs = positions(w as usize, sw);
    let ys = positions(h as usize, sh);
    let n = (taps * taps) as u32;
    out.par_chunks_exact_mut(w as usize * 4).zip(ys.par_iter()).for_each(|(row, sy)| {
        for (x, sx) in xs.iter().enumerate() {
            let mut acc = [0u32; 3];
            for &yy in &sy[..taps] {
                let base = yy * sw;
                for &xx in &sx[..taps] {
                    let i = (base + xx) * 4;
                    acc[0] += src[i + 2] as u32;
                    acc[1] += src[i + 1] as u32;
                    acc[2] += src[i] as u32;
                }
            }
            let d = &mut row[x * 4..x * 4 + 4];
            d[0] = ((acc[0] + n / 2) / n) as u8;
            d[1] = ((acc[1] + n / 2) / n) as u8;
            d[2] = ((acc[2] + n / 2) / n) as u8;
            d[3] = 255;
        }
    });
    (w, h, out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn pipelined_renderer_settles_on_the_current_scene_and_memoises_it() {
        std::env::set_var("FERROUS_FORCE_GPU", "1");
        let (mut r, err) = Renderer::new(32, 32, true);
        if err.is_some() || !matches!(r.backend, Backend::Gpu(_)) {
            eprintln!("no GPU; skipping");
            return;
        }
        r.set_pipelined(true);
        let (a, b) = (VideoFrame::solid(32, 32, [255, 0, 0, 255]), VideoFrame::solid(32, 32, [0, 0, 255, 255]));
        fn l(f: &VideoFrame) -> [Layer<'_>; 1] {
            [Layer { frame: f, transform: Transform::default(), filters: &[] }]
        }
        for _ in 0..3 {
            assert_eq!(&r.compose(&l(&a)).data[..4], &[255, 0, 0, 255]);
        }
        // Switching scenes shows at most one stale frame, then the new one
        // for good, even though identical layers are memoised.
        let outs: Vec<[u8; 4]> = (0..4).map(|_| r.compose(&l(&b)).data[..4].try_into().unwrap()).collect();
        assert!(outs[0] == [255, 0, 0, 255] || outs[0] == [0, 0, 255, 255]);
        assert_eq!(outs[2], [0, 0, 255, 255], "{outs:?}");
        assert_eq!(outs[3], [0, 0, 255, 255], "{outs:?}");
        // Back to synchronous: exact and immediate.
        r.set_pipelined(false);
        assert_eq!(&r.compose(&l(&a)).data[..4], &[255, 0, 0, 255]);
    }

    /// Without the BGRA read-back the frame carries only NV12, and a memoised frame without BGRA is
    /// never handed out when BGRA is wanted again.
    #[test]
    fn gpu_skips_the_bgra_readback_when_asked() {
        std::env::set_var("FERROUS_FORCE_GPU", "1");
        let (mut r, err) = Renderer::new(32, 32, true);
        if err.is_some() || !matches!(r.backend, Backend::Gpu(_)) {
            eprintln!("no GPU; skipping");
            return;
        }
        let a = VideoFrame::solid(32, 32, [255, 0, 0, 255]);
        let l = [Layer { frame: &a, transform: Transform::default(), filters: &[] }];
        r.set_nv12(true);
        r.set_bgra(false);
        let f = r.compose(&l);
        assert!(f.data.is_empty(), "no BGRA");
        let nv12 = f.nv12.as_ref().expect("NV12 still produced");
        assert_eq!(nv12.len(), 32 * 32 * 3 / 2);
        // Same layers, BGRA wanted again: must not come back empty from the memo.
        r.set_bgra(true);
        let g = r.compose(&l);
        assert_eq!(&g.data[..4], &[255, 0, 0, 255]);
        // JPEG's YCbCr: the test colour (BGRA 255,0,0 = pure blue) is Y 29, Cb 255, Cr 107 in JFIF,
        // not video's 32/240/118.
        r.set_nv12_jfif(true);
        r.set_bgra(false);
        let j = r.compose(&l);
        let nv = j.nv12.as_ref().expect("JFIF planes");
        assert!(j.nv12_jfif);
        let (y, cb, cr) = (nv[0], nv[32 * 32], nv[32 * 32 + 1]);
        assert!((i32::from(y) - 29).abs() <= 2 && (i32::from(cb) - 255).abs() <= 2 && (i32::from(cr) - 107).abs() <= 2, "{y} {cb} {cr}");
        r.set_nv12_jfif(false);
        let v = r.compose(&l);
        assert!(!v.nv12_jfif, "a video consumer never gets JPEG's YCbCr from the memo");
        r.set_bgra(true);
        // A GPU-made preview: half size, RGBA order, the frame's colour (BGRA 255,0,0 = blue).
        r.set_bgra(false);
        r.set_preview_width(Some(16));
        let _ = r.compose(&[Layer { frame: &VideoFrame::solid(32, 32, [255, 0, 0, 255]), transform: Transform::default(), filters: &[] }]);
        let (pw, ph, rgba) = r.take_preview().expect("a preview copy");
        assert_eq!((pw, ph, rgba.len()), (16, 16, 16 * 16 * 4));
        assert_eq!(&rgba[..4], &[0, 0, 255, 255]);
        r.set_preview_width(None);
        r.set_bgra(true);
        // Pipelined, alternating (a preview every other frame): every BGRA frame is real.
        r.set_pipelined(true);
        for i in 0..6 {
            r.set_bgra(i % 2 == 0);
            let f = r.compose(&l);
            if !f.data.is_empty() {
                assert_eq!(&f.data[..4], &[255, 0, 0, 255]);
            }
        }
    }

    use super::*;

    fn px(f: &VideoFrame, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * f.width + x) * 4) as usize;
        f.data[i..i + 4].try_into().unwrap()
    }

    /// A layer shrunk 4:1 averages its source: 1-pixel lines every 4th pixel come out ~25% grey on
    /// both compositors, and they agree with each other.
    #[test]
    fn shrunk_layers_average_on_cpu_and_gpu() {
        let mut src = vec![0u8; 256 * 256 * 4];
        for (i, px) in src.chunks_exact_mut(4).enumerate() {
            let v = if (i % 256) % 4 == 0 { 255 } else { 0 };
            px.copy_from_slice(&[v, v, v, 255]);
        }
        let f = ferrous_core::VideoFrame::new(256, 256, src);
        let tf = ferrous_core::Transform { scale_x: 0.25, scale_y: 0.25, ..Default::default() };
        let layers = [Layer { frame: &f, transform: tf, filters: &[] }];
        let cpu = Compositor::new(64, 64).compose(&layers);
        let inner = |v: &ferrous_core::VideoFrame| -> Vec<f32> {
            v.data.chunks_exact(4).enumerate().filter(|(i, _)| (4..60).contains(&(i % 64)) && (4..60).contains(&(i / 64))).map(|(_, p)| p[0] as f32).collect()
        };
        let c = inner(&cpu);
        let mean = c.iter().sum::<f32>() / c.len() as f32;
        assert!((mean - 64.0).abs() < 8.0, "cpu mean {mean}");
        assert!(c.iter().all(|v| (v - 64.0).abs() < 24.0), "cpu skipped lines");
        if let Ok(mut g) = gpu::GpuCompositor::new(64, 64) {
            let gp = inner(&g.compose_into(&layers, Vec::new(), false));
            let worst = c.iter().zip(&gp).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
            assert!(worst < 24.0, "cpu and gpu differ by {worst}");
        }
    }

    /// On the CPU path the same filter plan is baked in first, and the memo sees filter changes.
    #[test]
    fn cpu_renderer_runs_filter_plans_and_memo_notices_changes() {
        use crate::filters::GpuFilter;
        let (mut r, _) = Renderer::new(8, 1, false);
        let mut px = vec![0u8; 8 * 4];
        px[0] = 200; // pixel 0 is blue
        px[3] = 255;
        let f = ferrous_core::VideoFrame::new(8, 1, px);
        let at = |dx: f32| [GpuFilter::Scroll { dx, dy: 0.0 }];
        let one = at(1.0);
        let a = r.compose(&[Layer { frame: &f, transform: Default::default(), filters: &one }]);
        assert_eq!((a.data[0], a.data[4]), (0, 200), "scrolled one pixel right");
        // Same frame buffer, different offset: must not come back from the memo.
        let three = at(3.0);
        let b = r.compose(&[Layer { frame: &f, transform: Default::default(), filters: &three }]);
        assert_eq!(b.data[12], 200, "scrolled three pixels right");
        // Same plan again is the memoised result.
        let again = r.compose(&[Layer { frame: &f, transform: Default::default(), filters: &three }]);
        assert!(std::sync::Arc::ptr_eq(&b.data, &again.data));
    }

    #[test]
    fn draws_scaled_layer_with_position() {
        let red = VideoFrame::solid(10, 10, [0, 0, 255, 255]);
        let c = Compositor::new(100, 50);
        let out = c.compose(&[Layer {
            frame: &red,
            transform: Transform { x: 20.0, y: 10.0, scale_x: 2.0, scale_y: 2.0, ..Default::default() },
            filters: &[],
        }]);
        assert_eq!(px(&out, 25, 15), [0, 0, 255, 255]);
        assert_eq!(px(&out, 39, 29), [0, 0, 255, 255]);
        assert_eq!(px(&out, 41, 15), [0, 0, 0, 255]);
        assert_eq!(px(&out, 10, 15), [0, 0, 0, 255]);
    }

    #[test]
    fn blends_opacity_and_crops() {
        let white = VideoFrame::solid(4, 4, [255, 255, 255, 255]);
        let c = Compositor::new(4, 4);
        let out = c.compose(&[Layer {
            frame: &white,
            transform: Transform { opacity: 0.5, crop: [0, 0, 2, 0], ..Default::default() },
            filters: &[],
        }]);
        let p = px(&out, 0, 0);
        assert!((120..=135).contains(&p[0]), "{p:?}");
        assert_eq!(px(&out, 3, 0), [0, 0, 0, 255]);
    }

    #[test]
    fn offscreen_layer_is_safe() {
        let f = VideoFrame::solid(8, 8, [1, 2, 3, 255]);
        let c = Compositor::new(16, 16);
        for (x, y) in [(-100.0, 0.0), (100.0, 0.0), (-4.0, -4.0), (12.0, 12.0)] {
            c.compose(&[Layer { frame: &f, transform: Transform { x, y, ..Default::default() }, filters: &[] }]);
        }
    }

    /// The GPU path must match the CPU reference within rounding.
    #[test]
    fn gpu_matches_cpu() {
        let Ok(mut gpu) = GpuCompositor::new(320, 180) else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let mut data = vec![0u8; 64 * 48 * 4];
        for (i, px) in data.chunks_exact_mut(4).enumerate() {
            let (x, y) = (i % 64, i / 64);
            px.copy_from_slice(&[(x * 4) as u8, (y * 5) as u8, ((x + y) * 2) as u8, 255]);
        }
        let grad = VideoFrame::new(64, 48, data);
        let red = VideoFrame::solid(10, 10, [0, 0, 255, 255]);
        let half = VideoFrame::solid(40, 40, [255, 255, 255, 128]);
        let layers = [
            Layer { frame: &grad, transform: Transform { x: 10.0, y: 5.0, scale_x: 3.0, scale_y: 2.5, ..Default::default() }, filters: &[] },
            Layer { frame: &red, transform: Transform { x: 200.0, y: 100.0, ..Default::default() }, filters: &[] },
            Layer { frame: &half, transform: Transform { x: 150.0, y: 60.0, opacity: 0.8, ..Default::default() }, filters: &[] },
        ];
        let a = Compositor::new(320, 180).compose(&layers);
        let b = gpu.compose(&layers);
        let diffs: Vec<i32> = a.data.iter().zip(b.data.iter()).map(|(x, y)| (*x as i32 - *y as i32).abs()).collect();
        let mean = diffs.iter().sum::<i32>() as f32 / diffs.len() as f32;
        let max = *diffs.iter().max().unwrap();
        assert!(mean < 0.5, "mean diff {mean}");
        assert!(max <= 8, "max diff {max}");
        // Static frames are uploaded once and reused.
        let _ = gpu.compose(&layers);
        assert_eq!(gpu.cached_textures(), 3);
        match gpu.last_gpu_ms() {
            Some(ms) => {
                println!("GPU pass time {ms:.3} ms");
                assert!(ms > 0.0 && ms < 1000.0);
            }
            None => println!("adapter has no timestamp queries"),
        }
    }

    #[test]
    fn rotated_layer_lands_rotated_with_soft_edges() {
        // A 40x10 white bar rotated 90° about its centre (50,50) becomes a
        // 10x40 bar.
        let bar = VideoFrame::solid(40, 10, [255, 255, 255, 255]);
        let c = Compositor::new(100, 100);
        let out = c.compose(&[Layer { frame: &bar, transform: Transform { x: 30.0, y: 45.0, rotation: 90.0, ..Default::default() }, filters: &[] }]);
        assert_eq!(px(&out, 50, 32), [255, 255, 255, 255], "inside, vertical extent");
        assert_eq!(px(&out, 35, 50), [0, 0, 0, 255], "outside, old horizontal extent");
        // 45°: an edge pixel is partly covered.
        let out = c.compose(&[Layer { frame: &bar, transform: Transform { x: 30.0, y: 45.0, rotation: 45.0, ..Default::default() }, filters: &[] }]);
        let partial = (0..100).flat_map(|y| (0..100).map(move |x| (x, y))).filter(|&(x, y)| { let v = px(&out, x, y)[0]; v > 20 && v < 235 }).count();
        assert!(partial > 20, "antialiased edge pixels: {partial}");
        assert_eq!(px(&out, 50, 50), [255, 255, 255, 255]);
    }

    #[test]
    fn rotated_gpu_matches_cpu_inside_edges() {
        let Ok(mut gpu) = GpuCompositor::new(160, 120) else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let mut data = vec![0u8; 32 * 24 * 4];
        for (i, p) in data.chunks_exact_mut(4).enumerate() {
            let (x, y) = (i % 32, i / 32);
            p.copy_from_slice(&[(x * 8) as u8, (y * 10) as u8, 128, 255]);
        }
        let grad = VideoFrame::new(32, 24, data);
        let layers = [Layer { frame: &grad, transform: Transform { x: 40.0, y: 30.0, scale_x: 2.0, scale_y: 2.0, rotation: 30.0, ..Default::default() }, filters: &[] }];
        let a = Compositor::new(160, 120).compose(&layers);
        let b = gpu.compose(&layers);
        // Compare pixels well inside the box (edges differ only by AA rounding).
        let t = layers[0].transform;
        let mut n = 0;
        let mut worst = 0;
        for y in 0..120u32 {
            for x in 0..160u32 {
                let (lx, ly) = t.to_local(32, 24, x as f32 + 0.5, y as f32 + 0.5);
                if lx > 2.0 && ly > 2.0 && lx < 62.0 && ly < 46.0 {
                    n += 1;
                    for ch in 0..3 {
                        worst = worst.max((px(&a, x, y)[ch] as i32 - px(&b, x, y)[ch] as i32).abs());
                    }
                }
            }
        }
        assert!(n > 2000);
        assert!(worst <= 8, "max interior diff {worst}");
    }

    #[test]
    fn preview_swizzles() {
        let f = VideoFrame::solid(200, 100, [10, 20, 30, 255]);
        let (w, h, rgba) = preview_rgba(&f, 100);
        assert_eq!((w, h), (100, 50));
        assert_eq!(&rgba[..4], &[30, 20, 10, 255]);
    }

    #[test]
    fn recycled_buffers_never_show_the_previous_frame() {
        let (w, h) = (64u32, 32u32);
        let red = VideoFrame::solid(32, 32, [0, 0, 255, 255]);
        let blue = VideoFrame::solid(32, 32, [255, 0, 0, 255]);
        let left = Transform::default();
        let right = Transform { x: 32.0, ..Default::default() };
        let (mut r, _) = Renderer::new(w, h, false);
        for transparent in [false, true] {
            for round in 0..4 {
                // Alternate sides so a stale buffer would show the old half.
                let (f, t) = if round % 2 == 0 { (&red, left) } else { (&blue, right) };
                let layers = [Layer { frame: f, transform: t, filters: &[] }];
                let out = if transparent { r.compose_transparent(&layers) } else { r.compose(&layers) };
                let other_x = if round % 2 == 0 { 48 } else { 16 };
                let px = &out.data[((16 * w + other_x) * 4) as usize..][..4];
                let expect: [u8; 4] = if transparent { [0, 0, 0, 0] } else { [0, 0, 0, 255] };
                assert_eq!(px, expect, "transparent={transparent} round {round}: the uncovered half must be background");
                drop(out); // let the pool have it back
            }
        }
    }
}