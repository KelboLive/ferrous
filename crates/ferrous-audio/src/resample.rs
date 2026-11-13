/// Streaming linear-interpolation resampler for interleaved stereo.
///
/// Good enough for device rate conversion (44.1 -> 48 kHz); a windowed-sinc
/// resampler is a drop-in upgrade if anyone hears the difference.
pub struct LinearResampler {
    step: f64,
    pos: f64,
    last: [f32; 2],
    passthrough: bool,
}

impl LinearResampler {
    pub fn new(from_hz: u32, to_hz: u32) -> Self {
        Self {
            step: from_hz as f64 / to_hz as f64,
            pos: 0.0,
            last: [0.0; 2],
            passthrough: from_hz == to_hz,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        let frames = input.len() / 2;
        if frames == 0 {
            return;
        }
        // `pos` is relative to the frame before input[0] (stored in `last`).
        let get = |i: isize, c: usize| -> f32 {
            if i < 0 {
                self.last[c]
            } else {
                input[i as usize * 2 + c]
            }
        };
        while self.pos < frames as f64 {
            let i = self.pos.floor() as isize - 1;
            let frac = (self.pos - self.pos.floor()) as f32;
            for c in 0..2 {
                let a = get(i, c);
                let b = get(i + 1, c);
                out.push(a + (b - a) * frac);
            }
            self.pos += self.step;
        }
        self.pos -= frames as f64;
        self.last = [input[(frames - 1) * 2], input[(frames - 1) * 2 + 1]];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_is_right_over_time() {
        let mut r = LinearResampler::new(44_100, 48_000);
        let mut out = Vec::new();
        let block = vec![0.5f32; 441 * 2];
        for _ in 0..100 {
            r.process(&block, &mut out);
        }
        let frames = out.len() / 2;
        assert!((frames as i64 - 48_000).abs() <= 2, "{frames}");
        assert!(out[1000..].iter().all(|s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn passthrough() {
        let mut r = LinearResampler::new(48_000, 48_000);
        let mut out = Vec::new();
        r.process(&[1.0, 2.0], &mut out);
        assert_eq!(out, vec![1.0, 2.0]);
    }
}
