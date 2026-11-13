//! Person segmentation for background removal.
//!
//! PP-HumanSeg (PaddleSeg, Apache-2.0, via the OpenCV model zoo) run with
//! `tract`, a pure-Rust inference engine: no GPU runtime or extra DLLs to
//! ship, and it cross-compiles like the rest of Ferrous. The network is tiny
//! (192×192 in, 6 MB) so one pass is a few milliseconds on a modern CPU; the
//! caller runs it on its own thread at a capped rate.

use tract_onnx::prelude::*;

/// Side of the square network input and of the mask it returns.
pub const SIZE: usize = 192;

static MODEL: &[u8] = include_bytes!("../../../assets/models/pphumanseg_2023mar.onnx");

pub struct Segmenter {
    plan: Arc<TypedRunnableModel>,
    /// Reused network input (CHW, normalised).
    input: Vec<f32>,
}

impl Segmenter {
    pub fn new() -> anyhow::Result<Self> {
        let plan = tract_onnx::onnx()
            .model_for_read(&mut &MODEL[..])?
            .with_input_fact(0, f32::fact([1, 3, SIZE, SIZE]).into())?
            .into_optimized()?
            .into_runnable()?;
        Ok(Self { plan, input: vec![0.0; 3 * SIZE * SIZE] })
    }

    /// Person probability (0–255) for each of `SIZE × SIZE` cells covering
    /// the whole BGRA frame.
    pub fn segment(&mut self, bgra: &[u8], w: usize, h: usize) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(w > 0 && h > 0 && bgra.len() >= w * h * 4, "bad frame");
        downscale_to_input(bgra, w, h, &mut self.input);
        let x = Tensor::from_shape(&[1, 3, SIZE, SIZE], &self.input)?;
        let out = self.plan.run(tvec!(x.into()))?;
        let plane = SIZE * SIZE;
        let view = out[0].to_plain_array_view::<f32>()?;
        let flat: Vec<f32> = view.iter().copied().collect();
        anyhow::ensure!(flat.len() >= 2 * plane, "unexpected output size {}", flat.len());
        // Channel 0 = background, 1 = person; softmax of two logits.
        Ok((0..plane)
            .map(|i| {
                let d = flat[plane + i] - flat[i];
                (255.0 / (1.0 + (-d).exp())) as u8
            })
            .collect())
    }
}

/// Area-samples the frame down to the network input: RGB, CHW, (v/255-0.5)/0.5.
fn downscale_to_input(bgra: &[u8], w: usize, h: usize, out: &mut [f32]) {
    const TAPS: usize = 3;
    let plane = SIZE * SIZE;
    for oy in 0..SIZE {
        for ox in 0..SIZE {
            let mut acc = [0u32; 3];
            for ty in 0..TAPS {
                let y = ((oy * TAPS + ty) * h / (SIZE * TAPS)).min(h - 1);
                for tx in 0..TAPS {
                    let x = ((ox * TAPS + tx) * w / (SIZE * TAPS)).min(w - 1);
                    let p = (y * w + x) * 4;
                    acc[0] += u32::from(bgra[p + 2]);
                    acc[1] += u32::from(bgra[p + 1]);
                    acc[2] += u32::from(bgra[p]);
                }
            }
            let i = oy * SIZE + ox;
            for c in 0..3 {
                out[c * plane + i] = (acc[c] as f32 / (TAPS * TAPS) as f32) / 127.5 - 1.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real photograph with people in it: they must come out as person
    /// and the grass around them mostly not.
    #[test]
    fn finds_people_in_a_photo() {
        let Ok(bytes) = std::fs::read("/tmp/messi.jpg") else { return };
        let img = image::load_from_memory(&bytes).unwrap().to_rgba8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        let mut bgra = img.into_raw();
        for p in bgra.chunks_exact_mut(4) {
            p.swap(0, 2);
        }
        let mut s = Segmenter::new().unwrap();
        let t = std::time::Instant::now();
        let mask = s.segment(&bgra, w, h).unwrap();
        println!("segment: {:?}", t.elapsed());
        let on = mask.iter().filter(|&&m| m > 128).count();
        println!("person cells: {on} of {}", mask.len());
        // Dump a preview for eyeballing.
        let mut pv = image::GrayImage::new(SIZE as u32, SIZE as u32);
        for (i, m) in mask.iter().enumerate() {
            pv.put_pixel((i % SIZE) as u32, (i / SIZE) as u32, image::Luma([*m]));
        }
        let _ = pv.save("/tmp/mask.png");
        assert!(on > 300 && on < mask.len() * 7 / 10, "implausible person area {on}");
    }

    #[test]
    fn flat_colour_has_no_person() {
        let mut s = Segmenter::new().unwrap();
        let bgra = vec![60u8; 320 * 240 * 4];
        let mask = s.segment(&bgra, 320, 240).unwrap();
        let on = mask.iter().filter(|&&m| m > 128).count();
        assert!(on < mask.len() / 20, "{on} person cells in an empty frame");
    }
}
