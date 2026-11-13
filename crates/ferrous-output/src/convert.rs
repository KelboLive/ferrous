//! Encoder input conversion: composed BGRA canvas -> the encoder's YUV
//! layout (NV12 for hardware encoders, I420 for software ones) at the
//! output resolution, BT.709 limited range.
//!
//! Done here, in the output's own thread and spread over all cores, instead
//! of inside FFmpeg: the pipe then carries 1.5 bytes per pixel instead of 4
//! (a 4K60 canvas drops from 2 GB/s to 750 MB/s), and FFmpeg's
//! single-threaded input scaler is out of the loop.

use rayon::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YuvLayout {
    /// Y plane, then interleaved UV at half resolution.
    Nv12,
    /// Y plane, U plane, V plane (U and V at half resolution).
    I420,
    /// HDR10: 10-bit NV12 (16-bit little-endian words, value in the high
    /// 10 bits), BT.2020 primaries, PQ transfer. Hardware encoders.
    P010,
    /// HDR10: planar 10-bit (value in the low 10 bits). Software encoders.
    I420P10,
}

impl YuvLayout {
    pub fn ffmpeg_pix_fmt(self) -> &'static str {
        match self {
            YuvLayout::Nv12 => "nv12",
            YuvLayout::I420 => "yuv420p",
            YuvLayout::P010 => "p010le",
            YuvLayout::I420P10 => "yuv420p10le",
        }
    }

    /// HDR10 layouts: 10 bits per sample, BT.2020 + PQ.
    pub fn is_hdr(self) -> bool {
        matches!(self, YuvLayout::P010 | YuvLayout::I420P10)
    }
}

pub fn frame_bytes(w: u32, h: u32) -> usize {
    (w * h + 2 * (w / 2) * (h / 2)) as usize
}

/// SDR reference white, in nits, that HDR10 output maps sRGB white to
/// (ITU-R BT.2408 recommends 203).
pub const DEFAULT_SDR_WHITE_NITS: f32 = 203.0;

/// SDR -> HDR10 lookup tables: sRGB code -> linear light, linear light ->
/// PQ code. Built once per converter.
struct HdrTables {
    /// sRGB byte -> linear (0..1).
    linear: [f32; 256],
    /// Linear light as a fraction of the SDR white -> PQ signal, sampled.
    pq: Vec<f32>,
    /// How many `pq` steps span linear 0..1.
    pq_scale: f32,
}

/// Linear values above SDR white (negative-free, but wide-gamut matrix
/// output can exceed 1.0 a little) are looked up up to this multiple.
const PQ_HEADROOM: f32 = 1.25;
const PQ_STEPS: usize = 16384;

/// SMPTE ST 2084 inverse EOTF: absolute luminance / 10 000 -> signal.
pub fn pq_encode(l: f32) -> f32 {
    const M1: f32 = 0.159_301_76;
    const M2: f32 = 78.84375;
    const C1: f32 = 0.8359375;
    const C2: f32 = 18.851_563;
    const C3: f32 = 18.6875;
    let p = l.max(0.0).powf(M1);
    ((C1 + C2 * p) / (1.0 + C3 * p)).powf(M2)
}

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

impl HdrTables {
    fn new(white_nits: f32) -> Self {
        let white = white_nits.clamp(80.0, 1000.0) / 10000.0;
        let linear = std::array::from_fn(|i| srgb_to_linear(i as f32 / 255.0));
        let pq_scale = (PQ_STEPS - 1) as f32 / PQ_HEADROOM;
        let pq = (0..PQ_STEPS).map(|i| pq_encode(i as f32 / pq_scale * white)).collect();
        Self { linear, pq, pq_scale }
    }

    /// 8-bit sRGB RGB -> PQ-encoded BT.2020 R'G'B' (each 0..1).
    #[inline(always)]
    fn rgb(&self, r: i32, g: i32, b: i32) -> [f32; 3] {
        let (r, g, b) = (self.linear[r.clamp(0, 255) as usize], self.linear[g.clamp(0, 255) as usize], self.linear[b.clamp(0, 255) as usize]);
        // BT.709 -> BT.2020 primaries (ITU-R BT.2087).
        let r2 = 0.6274 * r + 0.3293 * g + 0.0433 * b;
        let g2 = 0.0691 * r + 0.9195 * g + 0.0114 * b;
        let b2 = 0.0164 * r + 0.0880 * g + 0.8956 * b;
        let look = |l: f32| self.pq[((l * self.pq_scale) as usize).min(PQ_STEPS - 1)];
        [look(r2), look(g2), look(b2)]
    }
}

/// BT.2020 non-constant-luminance, limited range, 10-bit code values.
#[inline(always)]
fn ycc10(p: [f32; 3]) -> u16 {
    ((64.0 + 876.0 * (0.2627 * p[0] + 0.678 * p[1] + 0.0593 * p[2])) + 0.5) as u16
}
#[inline(always)]
fn cbcr10(p: [f32; 3]) -> (u16, u16) {
    let y = 0.2627 * p[0] + 0.678 * p[1] + 0.0593 * p[2];
    let cb = (p[2] - y) / 1.8814;
    let cr = (p[0] - y) / 1.4746;
    (((512.0 + 896.0 * cb) + 0.5).clamp(64.0, 960.0) as u16, ((512.0 + 896.0 * cr) + 0.5).clamp(64.0, 960.0) as u16)
}

/// Separable resampling weights for one axis (a triangle filter widened by
/// the scale factor when shrinking, so downscales are properly averaged).
struct Axis {
    /// For each output position: first source index and its weights (Q14).
    taps: Vec<(usize, Vec<i32>)>,
}

impl Axis {
    fn new(src: usize, dst: usize) -> Self {
        let scale = src as f32 / dst as f32;
        let support = scale.max(1.0);
        let taps = (0..dst)
            .map(|o| {
                let center = (o as f32 + 0.5) * scale - 0.5;
                let lo = ((center - support).floor() as isize + 1).max(0) as usize;
                let hi = ((center + support).ceil() as isize - 1).min(src as isize - 1).max(lo as isize) as usize;
                let mut w: Vec<f32> = (lo..=hi).map(|i| (1.0 - (i as f32 - center).abs() / support).max(0.0)).collect();
                let sum: f32 = w.iter().sum();
                if sum <= 0.0 {
                    w.iter_mut().for_each(|x| *x = 0.0);
                    w[0] = 1.0;
                } else {
                    w.iter_mut().for_each(|x| *x /= sum);
                }
                // Quantise so each row sums to exactly 1 << 14.
                let mut q: Vec<i32> = w.iter().map(|x| (x * 16384.0).round() as i32).collect();
                let err = 16384 - q.iter().sum::<i32>();
                let big = q.iter().enumerate().max_by_key(|(_, v)| **v).map(|(i, _)| i).unwrap_or(0);
                q[big] += err;
                (lo, q)
            })
            .collect();
        Self { taps }
    }
}

/// BT.709 limited range, Q16 coefficients.
#[inline(always)]
fn y_of(r: i32, g: i32, b: i32) -> u8 {
    // 219/255 * (0.2126, 0.7152, 0.0722)
    ((11966 * r + 40254 * g + 4064 * b + (16 << 16) + 32768) >> 16).clamp(0, 255) as u8
}
#[inline(always)]
fn u_of(r: i32, g: i32, b: i32) -> u8 {
    // 224/255 * (-0.1146, -0.3854, 0.5)
    ((-6596 * r - 22189 * g + 28785 * b + (128 << 16) + 32768) >> 16).clamp(0, 255) as u8
}
#[inline(always)]
fn v_of(r: i32, g: i32, b: i32) -> u8 {
    // 224/255 * (0.5, -0.4542, -0.0458)
    ((28785 * r - 26145 * g - 2640 * b + (128 << 16) + 32768) >> 16).clamp(0, 255) as u8
}

/// Reusable converter for one (input size, output size, layout).
pub struct Converter {
    src: (u32, u32),
    dst: (u32, u32),
    layout: YuvLayout,
    x: Option<Axis>,
    y: Option<Axis>,
    hdr: Option<HdrTables>,
}

impl Converter {
    /// `dst` is rounded down to even dimensions (4:2:0 needs them).
    pub fn new(src: (u32, u32), dst: (u32, u32), layout: YuvLayout) -> Self {
        Self::with_white(src, dst, layout, DEFAULT_SDR_WHITE_NITS)
    }

    /// Like [`Self::new`]; `white_nits` is where SDR white lands in the
    /// HDR layouts (ignored for SDR ones).
    pub fn with_white(src: (u32, u32), dst: (u32, u32), layout: YuvLayout, white_nits: f32) -> Self {
        let dst = ((dst.0 & !1).max(2), (dst.1 & !1).max(2));
        let same = src == dst;
        Self {
            src,
            dst,
            layout,
            x: (!same).then(|| Axis::new(src.0 as usize, dst.0 as usize)),
            y: (!same).then(|| Axis::new(src.1 as usize, dst.1 as usize)),
            hdr: layout.is_hdr().then(|| HdrTables::new(white_nits)),
        }
    }

    pub fn output_size(&self) -> (u32, u32) {
        self.dst
    }

    pub fn input_size(&self) -> (u32, u32) {
        self.src
    }

    /// The picture of `frame` this converter would produce, when the GPU
    /// compositor already made it: NV12 at the frame's own size, which is
    /// the output size (nothing to scale).
    pub fn ready_made<'a>(&self, frame: &'a ferrous_core::VideoFrame) -> Option<&'a [u8]> {
        let nv12 = frame.nv12.as_ref()?;
        (self.layout == YuvLayout::Nv12 && self.src == self.dst && (frame.width, frame.height) == self.src && nv12.len() == self.output_bytes())
            .then_some(nv12.as_slice())
    }

    pub fn output_bytes(&self) -> usize {
        frame_bytes(self.dst.0, self.dst.1) * if self.layout.is_hdr() { 2 } else { 1 }
    }

    /// Converts a BGRA frame into `out` (resized to [`Self::output_bytes`]).
    pub fn convert(&self, bgra: &[u8], out: &mut Vec<u8>) {
        let (sw, sh) = (self.src.0 as usize, self.src.1 as usize);
        let (dw, dh) = (self.dst.0 as usize, self.dst.1 as usize);
        assert_eq!(bgra.len(), sw * sh * 4, "input is not {sw}x{sh} BGRA");
        out.resize(self.output_bytes(), 0);
        if self.hdr.is_some() {
            self.convert_hdr(bgra, out);
            return;
        }
        let (y_plane, chroma) = out.split_at_mut(dw * dh);
        let (cw, ch) = (dw / 2, dh / 2);


        if self.x.is_none() {
            // Same size: straight from BGRA bytes, two rows at a time.
            let row = sw * 4;
            match self.layout {
                YuvLayout::Nv12 => y_plane
                    .par_chunks_exact_mut(dw * 2)
                    .zip(chroma.par_chunks_exact_mut(cw * 2))
                    .enumerate()
                    .for_each(|(pair, (yr, uv))| {
                        let src = &bgra[pair * 2 * row..(pair * 2 + 2) * row];
                        direct_pair(src, row, yr, dw, |cx, u, v| {
                            uv[cx * 2] = u;
                            uv[cx * 2 + 1] = v;
                        });
                    }),
                YuvLayout::I420 => {
                    let (u_plane, v_plane) = chroma.split_at_mut(cw * ch);
                    y_plane
                        .par_chunks_exact_mut(dw * 2)
                        .zip(u_plane.par_chunks_exact_mut(cw).zip(v_plane.par_chunks_exact_mut(cw)))
                        .enumerate()
                        .for_each(|(pair, (yr, (ur, vr)))| {
                            let src = &bgra[pair * 2 * row..(pair * 2 + 2) * row];
                            direct_pair(src, row, yr, dw, |cx, u, v| {
                                ur[cx] = u;
                                vr[cx] = v;
                            });
                        });
                }
                YuvLayout::P010 | YuvLayout::I420P10 => unreachable!("HDR layouts go through convert_hdr"),
            }
            return;
        }
        let init = || ([vec![0i32; dw * 3], vec![0i32; dw * 3]], vec![0i32; sw * 3]);
        match self.layout {
            YuvLayout::Nv12 => {
                y_plane.par_chunks_exact_mut(dw * 2).zip(chroma.par_chunks_exact_mut(cw * 2)).enumerate().for_each_init(init, |(rgb, tmp), (pair, (yr, uv))| {
                    self.luma_pair(bgra, sw, pair, yr, rgb, tmp);
                    for cx in 0..cw {
                        let (r, g, b) = avg4(rgb, cx);
                        uv[cx * 2] = u_of(r, g, b);
                        uv[cx * 2 + 1] = v_of(r, g, b);
                    }
                });
            }
            YuvLayout::I420 => {
                let (u_plane, v_plane) = chroma.split_at_mut(cw * ch);
                y_plane
                    .par_chunks_exact_mut(dw * 2)
                    .zip(u_plane.par_chunks_exact_mut(cw).zip(v_plane.par_chunks_exact_mut(cw)))
                    .enumerate()
                    .for_each_init(init, |(rgb, tmp), (pair, (yr, (ur, vr)))| {
                        self.luma_pair(bgra, sw, pair, yr, rgb, tmp);
                        for cx in 0..cw {
                            let (r, g, b) = avg4(rgb, cx);
                            ur[cx] = u_of(r, g, b);
                            vr[cx] = v_of(r, g, b);
                        }
                    });
            }
            YuvLayout::P010 | YuvLayout::I420P10 => unreachable!("HDR layouts go through convert_hdr"),
        }
    }

    /// SDR BGRA -> 10-bit HDR10 (BT.2020, PQ, limited range).
    fn convert_hdr(&self, bgra: &[u8], out: &mut [u8]) {
        let t = self.hdr.as_ref().expect("HDR tables");
        let sw = self.src.0 as usize;
        let (dw, dh) = (self.dst.0 as usize, self.dst.1 as usize);
        let (cw, ch) = (dw / 2, dh / 2);
        let semi = self.layout == YuvLayout::P010;
        // P010 keeps its 10 bits in the high end of each word.
        let put = move |dst: &mut [u8], v: u16| {
            let w = if semi { v << 6 } else { v };
            dst.copy_from_slice(&w.to_le_bytes());
        };
        let (y_plane, chroma) = out.split_at_mut(dw * dh * 2);
        let init = || ([vec![0i32; dw * 3], vec![0i32; dw * 3]], vec![0i32; sw * 3], vec![[0f32; 3]; dw * 2]);
        let work = |(rgb, tmp, pix): &mut ([Vec<i32>; 2], Vec<i32>, Vec<[f32; 3]>), pair: usize, yr: &mut [u8], mut chroma_out: Box<dyn FnMut(usize, u16, u16) + '_>| {
            for k in 0..2 {
                self.resample_row(bgra, sw, pair * 2 + k, &mut rgb[k], tmp);
                for x in 0..dw {
                    let p = t.rgb(rgb[k][x * 3], rgb[k][x * 3 + 1], rgb[k][x * 3 + 2]);
                    pix[k * dw + x] = p;
                    let o = (k * dw + x) * 2;
                    put(&mut yr[o..o + 2], ycc10(p));
                }
            }
            for cx in 0..cw {
                let mut a = [0f32; 3];
                for (k, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                    let p = pix[k * dw + cx * 2 + dx];
                    a.iter_mut().zip(p).for_each(|(s, v)| *s += v * 0.25);
                }
                let (u, v) = cbcr10(a);
                chroma_out(cx, u, v);
            }
        };
        if semi {
            y_plane.par_chunks_exact_mut(dw * 4).zip(chroma.par_chunks_exact_mut(cw * 4)).enumerate().for_each_init(init, |st, (pair, (yr, uv))| {
                work(st, pair, yr, Box::new(|cx, u, v| {
                    put(&mut uv[cx * 4..cx * 4 + 2], u);
                    put(&mut uv[cx * 4 + 2..cx * 4 + 4], v);
                }));
            });
        } else {
            let (u_plane, v_plane) = chroma.split_at_mut(cw * ch * 2);
            y_plane
                .par_chunks_exact_mut(dw * 4)
                .zip(u_plane.par_chunks_exact_mut(cw * 2).zip(v_plane.par_chunks_exact_mut(cw * 2)))
                .enumerate()
                .for_each_init(init, |st, (pair, (yr, (ur, vr)))| {
                    work(st, pair, yr, Box::new(|cx, u, v| {
                        put(&mut ur[cx * 2..cx * 2 + 2], u);
                        put(&mut vr[cx * 2..cx * 2 + 2], v);
                    }));
                });
        }
    }

    /// Resamples output rows 2*pair and 2*pair+1 into `rgb` and writes their luma.
    fn luma_pair(&self, bgra: &[u8], sw: usize, pair: usize, yr: &mut [u8], rgb: &mut [Vec<i32>; 2], tmp: &mut [i32]) {
        let dw = self.dst.0 as usize;
        for k in 0..2 {
            self.resample_row(bgra, sw, pair * 2 + k, &mut rgb[k], tmp);
            let row = &mut yr[k * dw..(k + 1) * dw];
            for (x, y) in row.iter_mut().enumerate() {
                let p = &rgb[k][x * 3..x * 3 + 3];
                *y = y_of(p[0], p[1], p[2]);
            }
        }
    }

    /// One output row as R,G,B (0..255) per pixel.
    fn resample_row(&self, bgra: &[u8], sw: usize, oy: usize, out: &mut [i32], tmp: &mut [i32]) {
        let dw = self.dst.0 as usize;
        match (&self.x, &self.y) {
            (Some(ax), Some(ay)) => {
                // Vertical pass into a full-width row (8 fractional bits), then horizontal.
                let (y0, wy) = &ay.taps[oy];
                tmp.iter_mut().for_each(|t| *t = 0);
                for (j, w) in wy.iter().enumerate() {
                    let src = &bgra[(y0 + j) * sw * 4..(y0 + j + 1) * sw * 4];
                    for (t, px) in tmp.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
                        t[0] += px[2] as i32 * w;
                        t[1] += px[1] as i32 * w;
                        t[2] += px[0] as i32 * w;
                    }
                }
                tmp.iter_mut().for_each(|t| *t = (*t + 32) >> 6);
                for (ox, (x0, wx)) in ax.taps.iter().enumerate() {
                    let mut s = [0i32; 3];
                    for (p, w) in tmp[x0 * 3..(x0 + wx.len()) * 3].chunks_exact(3).zip(wx) {
                        s[0] += p[0] * w;
                        s[1] += p[1] * w;
                        s[2] += p[2] * w;
                    }
                    for c in 0..3 {
                        out[ox * 3 + c] = (s[c] + (1 << 21)) >> 22;
                    }
                }
            }
            _ => {
                let src = &bgra[oy * sw * 4..(oy + 1) * sw * 4];
                for x in 0..dw {
                    out[x * 3] = src[x * 4 + 2] as i32;
                    out[x * 3 + 1] = src[x * 4 + 1] as i32;
                    out[x * 3 + 2] = src[x * 4] as i32;
                }
            }
        }
    }
}

/// Two BGRA rows (no scaling) to two luma rows and one chroma row.
#[inline(always)]
fn direct_pair(src: &[u8], row: usize, yr: &mut [u8], dw: usize, mut put: impl FnMut(usize, u8, u8)) {
    let (r0, r1) = src.split_at(row);
    let (y0, y1) = yr.split_at_mut(dw);
    for (cx, ((a, b), (ya, yb))) in r0.chunks_exact(8).zip(r1.chunks_exact(8)).zip(y0.chunks_exact_mut(2).zip(y1.chunks_exact_mut(2))).enumerate() {
        let (b0, g0, r0_) = (a[0] as i32, a[1] as i32, a[2] as i32);
        let (b1, g1, r1_) = (a[4] as i32, a[5] as i32, a[6] as i32);
        let (b2, g2, r2) = (b[0] as i32, b[1] as i32, b[2] as i32);
        let (b3, g3, r3) = (b[4] as i32, b[5] as i32, b[6] as i32);
        ya[0] = y_of(r0_, g0, b0);
        ya[1] = y_of(r1_, g1, b1);
        yb[0] = y_of(r2, g2, b2);
        yb[1] = y_of(r3, g3, b3);
        let (r, g, bl) = ((r0_ + r1_ + r2 + r3 + 2) >> 2, (g0 + g1 + g2 + g3 + 2) >> 2, (b0 + b1 + b2 + b3 + 2) >> 2);
        put(cx, u_of(r, g, bl), v_of(r, g, bl));
    }
}

#[inline(always)]
fn avg4(rgb: &[Vec<i32>; 2], cx: usize) -> (i32, i32, i32) {
    let mut s = [0i32; 3];
    for row in rgb {
        for dx in 0..2 {
            let p = &row[(cx * 2 + dx) * 3..(cx * 2 + dx) * 3 + 3];
            s[0] += p[0];
            s[1] += p[1];
            s[2] += p[2];
        }
    }
    ((s[0] + 2) >> 2, (s[1] + 2) >> 2, (s[2] + 2) >> 2)
}

#[cfg(test)]
mod tests {
    fn word(b: &[u8], i: usize) -> u16 {
        u16::from_le_bytes([b[i * 2], b[i * 2 + 1]])
    }

    #[test]
    fn pq_matches_the_standard_at_known_points() {
        assert!(super::pq_encode(0.0).abs() < 1e-6);
        assert!((super::pq_encode(1.0) - 1.0).abs() < 1e-4);
        // 100 nits is 0.508 on the PQ curve; 203 nits is about 0.581.
        assert!((super::pq_encode(0.01) - 0.508).abs() < 2e-3);
        assert!((super::pq_encode(0.0203) - 0.581).abs() < 3e-3);
    }

    #[test]
    fn hdr_white_and_black_land_on_the_right_codes() {
        use super::{Converter, YuvLayout};
        for layout in [YuvLayout::P010, YuvLayout::I420P10] {
            let c = Converter::new((4, 4), (4, 4), layout);
            assert_eq!(c.output_bytes(), 48);
            let mut out = Vec::new();
            c.convert(&[255u8; 4 * 4 * 4], &mut out);
            assert_eq!(out.len(), c.output_bytes());
            let shift = if layout == YuvLayout::P010 { 6 } else { 0 };
            let y = word(&out, 0) >> shift;
            // 203 nits in PQ is about 0.581 -> 64 + 876 * 0.581 = 573.
            assert!((565..=581).contains(&y), "white Y {y}");
            let u = word(&out, 16) >> shift;
            assert!((510..=514).contains(&u), "neutral chroma {u}");
            let mut out = Vec::new();
            c.convert(&[0u8; 4 * 4 * 4], &mut out);
            assert_eq!(word(&out, 0) >> shift, 64);
        }
    }

    #[test]
    fn hdr_conversion_scales_like_the_sdr_one() {
        use super::{Converter, YuvLayout};
        let c = Converter::new((8, 8), (4, 4), YuvLayout::P010);
        let mut out = Vec::new();
        c.convert(&[128u8; 8 * 8 * 4], &mut out);
        assert_eq!(out.len(), c.output_bytes());
        let y = word(&out, 0) >> 6;
        assert!(y > 64 && y < 573, "{y}");
    }

    /// The GPU compositor's NV12 must be what this converter would make.
    #[test]
    fn gpu_nv12_agrees_with_the_cpu_converter() {
        use ferrous_core::{Transform, VideoFrame};
        use ferrous_render::{GpuCompositor, Layer};
        let Ok(mut gpu) = GpuCompositor::new(96, 64) else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let mut px = vec![0u8; 96 * 64 * 4];
        for (i, p) in px.chunks_exact_mut(4).enumerate() {
            let (x, y) = (i % 96, i / 96);
            p.copy_from_slice(&[(x * 2) as u8, (y * 3) as u8, ((x ^ y) * 4) as u8, 255]);
        }
        let src = VideoFrame::new(96, 64, px);
        let out = gpu.compose_into(&[Layer { frame: &src, transform: Transform::default() }], Vec::new(), true);
        let conv = Converter::new((96, 64), (96, 64), YuvLayout::Nv12);
        let ready = conv.ready_made(&out).expect("same size, NV12 layout");
        let mut cpu = Vec::new();
        conv.convert(&out.data, &mut cpu);
        assert_eq!(ready.len(), cpu.len());
        let worst = ready.iter().zip(&cpu).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        assert!(worst <= 3, "GPU and CPU NV12 differ by {worst}");
        // A scaled output, or another layout, isn't served from the GPU frame.
        assert!(Converter::new((96, 64), (48, 32), YuvLayout::Nv12).ready_made(&out).is_none());
        assert!(Converter::new((96, 64), (96, 64), YuvLayout::I420).ready_made(&out).is_none());
    }

    use super::*;

    fn bgra(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let [r, g, b] = f(x, y);
                v.extend_from_slice(&[b, g, r, 255]);
            }
        }
        v
    }

    #[test]
    fn bt709_limited_range_reference_colours() {
        let c = Converter::new((2, 2), (2, 2), YuvLayout::I420);
        let mut out = Vec::new();
        for (rgb, yuv) in [
            ([0, 0, 0], [16, 128, 128]),
            ([255, 255, 255], [235, 128, 128]),
            ([255, 0, 0], [63, 102, 240]),
            ([0, 255, 0], [173, 42, 26]),
            ([0, 0, 255], [32, 240, 118]),
        ] {
            c.convert(&bgra(2, 2, |_, _| rgb), &mut out);
            let got = [out[0], out[4], out[5]];
            for i in 0..3 {
                assert!((got[i] as i32 - yuv[i]).abs() <= 1, "{rgb:?}: {got:?} vs {yuv:?}");
            }
        }
    }

    #[test]
    fn nv12_and_i420_carry_the_same_samples() {
        let src = bgra(64, 36, |x, y| [(x * 4) as u8, (y * 7) as u8, ((x + y) * 3) as u8]);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        Converter::new((64, 36), (64, 36), YuvLayout::Nv12).convert(&src, &mut a);
        Converter::new((64, 36), (64, 36), YuvLayout::I420).convert(&src, &mut b);
        assert_eq!(a.len(), frame_bytes(64, 36));
        assert_eq!(a[..64 * 36], b[..64 * 36]);
        let n = 32 * 18;
        for i in 0..n {
            assert_eq!(a[64 * 36 + i * 2], b[64 * 36 + i]);
            assert_eq!(a[64 * 36 + i * 2 + 1], b[64 * 36 + n + i]);
        }
    }

    #[test]
    fn downscale_averages_and_keeps_flat_areas_exact() {
        // A fine checkerboard averages to mid-grey when halved; flat stays flat.
        let src = bgra(128, 64, |x, y| if (x + y) % 2 == 0 { [255, 255, 255] } else { [0, 0, 0] });
        let c = Converter::new((128, 64), (64, 32), YuvLayout::I420);
        let mut out = Vec::new();
        c.convert(&src, &mut out);
        let mid = y_of(128, 128, 128) as i32;
        let worst = out[..64 * 32].iter().map(|v| (*v as i32 - mid).abs()).max().unwrap();
        assert!(worst <= 3, "checkerboard should blur to grey, off by {worst}");
        let flat = bgra(1920, 1080, |_, _| [200, 100, 50]);
        Converter::new((1920, 1080), (1280, 720), YuvLayout::Nv12).convert(&flat, &mut out);
        let y = y_of(200, 100, 50);
        assert!(out[..1280 * 720].iter().all(|v| *v == y));
    }
}
