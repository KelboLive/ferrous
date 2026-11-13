//! Background removal as a video filter.
//!
//! The segmentation model is the expensive part, so it never runs on the
//! render path: a worker thread takes the newest frame, runs the network on a
//! 192×192 downscale at no more than `max_fps`, smooths the mask over time and
//! publishes it. The filter itself only upsamples the latest mask over the
//! frame and multiplies the alpha, which costs about as much as a colour key.
//! A static source (same pixels) is not segmented again.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ferrous_core::VideoFrame;
use ferrous_matte::SIZE;
use parking_lot::{Condvar, Mutex};
use rayon::prelude::*;

/// The latest person mask (0–1 per cell, `SIZE × SIZE`).
pub struct Mask {
    pub cells: Vec<f32>,
    /// Bumps with every published mask, so callers can tell when it changed.
    pub version: u64,
}

#[derive(Default)]
struct Shared {
    /// Newest frame not yet segmented.
    input: Mutex<Option<VideoFrame>>,
    wake: Condvar,
    mask: Mutex<Option<Arc<Mask>>>,
    error: Mutex<Option<String>>,
    /// (max_fps, smoothing), updated as the sliders move.
    params: Mutex<(u32, f32)>,
}

pub struct MatteWorker {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Pixel buffer last handed over, so an unchanged source isn't re-sent.
    last_sent: Option<Arc<Vec<u8>>>,
}

impl MatteWorker {
    pub fn start() -> Self {
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (sh, st) = (shared.clone(), stop.clone());
        let thread = thread::Builder::new()
            .name("background-removal".into())
            .spawn(move || run(sh, st))
            .ok();
        Self { shared, stop, thread, last_sent: None }
    }

    /// Offers the current frame and settings; never blocks.
    pub fn submit(&mut self, frame: &VideoFrame, max_fps: u32, smoothing: f32) {
        *self.shared.params.lock() = (max_fps.max(1), smoothing.clamp(0.0, 0.95));
        if self.last_sent.as_ref().is_some_and(|l| Arc::ptr_eq(l, &frame.data)) {
            return;
        }
        self.last_sent = Some(frame.data.clone());
        *self.shared.input.lock() = Some(frame.clone());
        self.shared.wake.notify_one();
    }

    pub fn mask(&self) -> Option<Arc<Mask>> {
        self.shared.mask.lock().clone()
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().clone()
    }
}

impl Drop for MatteWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(sh: Arc<Shared>, stop: Arc<AtomicBool>) {
    let mut seg = match ferrous_matte::Segmenter::new() {
        Ok(s) => s,
        Err(e) => {
            *sh.error.lock() = Some(format!("Background removal: {e}"));
            return;
        }
    };
    let mut smoothed: Vec<f32> = Vec::new();
    let mut version = 0u64;
    let mut last_run = Instant::now() - Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        let frame = {
            let mut g = sh.input.lock();
            while g.is_none() && !stop.load(Ordering::Relaxed) {
                sh.wake.wait_for(&mut g, Duration::from_millis(200));
            }
            g.take()
        };
        let Some(frame) = frame else { continue };
        // Cap the model rate; the newest frame wins while waiting.
        let (fps, smoothing) = *sh.params.lock();
        let gap = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
        if let Some(wait) = gap.checked_sub(last_run.elapsed()) {
            thread::sleep(wait);
        }
        let frame = sh.input.lock().take().unwrap_or(frame);
        last_run = Instant::now();
        match seg.segment(&frame.data, frame.width as usize, frame.height as usize) {
            Ok(m) => {
                if smoothed.len() != m.len() {
                    smoothed = m.iter().map(|&v| f32::from(v) / 255.0).collect();
                } else {
                    for (s, &v) in smoothed.iter_mut().zip(&m) {
                        *s = *s * smoothing + (f32::from(v) / 255.0) * (1.0 - smoothing);
                    }
                }
                version += 1;
                *sh.mask.lock() = Some(Arc::new(Mask { cells: smoothed.clone(), version }));
                *sh.error.lock() = None;
            }
            Err(e) => *sh.error.lock() = Some(format!("Background removal: {e}")),
        }
    }
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Multiplies the frame's alpha by the person mask, scaled up smoothly
/// (bilinear) and sharpened around `threshold` over `softness`.
pub fn apply_mask(frame: &VideoFrame, mask: &Mask, threshold: f32, softness: f32) -> VideoFrame {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let soft = softness.max(0.01);
    let (lo, hi) = (threshold - soft, threshold + soft);
    // Horizontal sample positions once per frame, not per pixel.
    let xs: Vec<(usize, usize, f32)> = (0..w)
        .map(|x| {
            let f = ((x as f32 + 0.5) / w as f32 * SIZE as f32 - 0.5).clamp(0.0, (SIZE - 1) as f32);
            let i = f.floor() as usize;
            (i, (i + 1).min(SIZE - 1), f - i as f32)
        })
        .collect();
    let mut out = frame.data.as_ref().clone();
    out.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let f = ((y as f32 + 0.5) / h as f32 * SIZE as f32 - 0.5).clamp(0.0, (SIZE - 1) as f32);
        let (y0, fy) = (f.floor() as usize, f - f.floor());
        let y1 = (y0 + 1).min(SIZE - 1);
        let (r0, r1) = (&mask.cells[y0 * SIZE..(y0 + 1) * SIZE], &mask.cells[y1 * SIZE..(y1 + 1) * SIZE]);
        for (px, &(x0, x1, fx)) in row.chunks_exact_mut(4).zip(&xs) {
            let top = r0[x0] + (r0[x1] - r0[x0]) * fx;
            let bot = r1[x0] + (r1[x1] - r1[x0]) * fx;
            let m = smoothstep(lo, hi, top + (bot - top) * fy);
            px[3] = (f32::from(px[3]) * m + 0.5) as u8;
        }
    });
    VideoFrame::new(frame.width, frame.height, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask_of(v: f32) -> Mask {
        Mask { cells: vec![v; SIZE * SIZE], version: 1 }
    }

    #[test]
    fn mask_scales_alpha_and_edge_is_soft() {
        let f = VideoFrame::solid(64, 48, [10, 20, 30, 255]);
        assert_eq!(apply_mask(&f, &mask_of(1.0), 0.5, 0.1).data[3], 255);
        assert_eq!(apply_mask(&f, &mask_of(0.0), 0.5, 0.1).data[3], 0);
        let mid = apply_mask(&f, &mask_of(0.5), 0.5, 0.1).data[3];
        assert!((100..=155).contains(&mid), "mid {mid}");
        // Colour is untouched.
        assert_eq!(&apply_mask(&f, &mask_of(1.0), 0.5, 0.1).data[..3], &[10, 20, 30]);
    }

    #[test]
    fn worker_publishes_a_mask_and_stops() {
        let mut w = MatteWorker::start();
        let f = VideoFrame::solid(320, 240, [60, 60, 60, 255]);
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            w.submit(&f, 30, 0.0);
            if let Some(m) = w.mask() {
                assert!(m.cells.iter().filter(|&&c| c > 0.5).count() < SIZE * SIZE / 20, "an empty frame is not a person");
                break;
            }
            assert!(w.error().is_none(), "{:?}", w.error());
            assert!(Instant::now() < end, "no mask within 30 s");
            thread::sleep(Duration::from_millis(50));
        }
        drop(w); // joins the thread
    }
}
