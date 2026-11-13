use std::path::Path;
use std::time::Instant;

use ferrous_core::VideoFrame;

use crate::Source;

pub struct ColorSource(VideoFrame);

impl ColorSource {
    pub fn new(width: u32, height: u32, bgra: [u8; 4]) -> Self {
        Self(VideoFrame::solid(width.max(1), height.max(1), bgra))
    }
}

impl Source for ColorSource {
    fn latest_frame(&mut self) -> Option<VideoFrame> {
        Some(self.0.clone())
    }
}

pub struct ImageSource(VideoFrame);

impl ImageSource {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let img = image::open(path)
            .map_err(|e| anyhow::anyhow!("can't open image {}: {e}", path.display()))?
            .to_rgba8();
        let (w, h) = img.dimensions();
        let mut data = img.into_raw();
        for px in data.chunks_exact_mut(4) {
            px.swap(0, 2); // RGBA -> BGRA
        }
        Ok(Self(VideoFrame::new(w, h, data)))
    }
}

impl Source for ImageSource {
    fn latest_frame(&mut self) -> Option<VideoFrame> {
        Some(self.0.clone())
    }
}

/// SMPTE-style colour bars with a moving sweep, so dropped or frozen
/// frames are obvious in recordings.
pub struct TestPatternSource {
    width: u32,
    height: u32,
    start: Instant,
    bars: Vec<u8>,
}

const BARS: [[u8; 4]; 7] = [
    [192, 192, 192, 255],
    [0, 192, 192, 255],
    [192, 192, 0, 255],
    [0, 192, 0, 255],
    [192, 0, 192, 255],
    [0, 0, 192, 255],
    [192, 0, 0, 255],
];

impl TestPatternSource {
    pub fn new(width: u32, height: u32) -> Self {
        let (width, height) = (width.max(16), height.max(16));
        let mut bars = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let c = BARS[(x * 7 / width) as usize];
                let i = ((y * width + x) * 4) as usize;
                bars[i..i + 4].copy_from_slice(&c);
            }
        }
        Self { width, height, start: Instant::now(), bars }
    }
}

impl Source for TestPatternSource {
    fn latest_frame(&mut self) -> Option<VideoFrame> {
        let mut data = self.bars.clone();
        let t = self.start.elapsed().as_secs_f32();
        let bar_w = (self.width / 40).max(4);
        let pos = ((t * 0.25).fract() * (self.width - bar_w) as f32) as u32;
        for y in 0..self.height {
            for x in pos..pos + bar_w {
                let i = ((y * self.width + x) * 4) as usize;
                data[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        Some(VideoFrame::new(self.width, self.height, data))
    }
}
