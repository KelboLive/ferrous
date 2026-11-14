//! Frame codecs. Video payloads start with
//! `[u64 timestamp_us][u32 width][u32 height]`, then codec data.

use ferrous_core::VideoFrame;
use rayon::prelude::*;

pub(crate) fn header(ts: u64, w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(16);
    v.extend_from_slice(&ts.to_be_bytes());
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v
}

/// Largest picture a link will carry (about 8K at 4 bytes a pixel, with room to spare).
pub const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// Byte size of a BGRA frame, or `None` for a zero side or one too big to
/// trust: sizes come off the network, so `w * h * 4` must not wrap or
/// ask for gigabytes.
pub fn frame_bytes(w: u32, h: u32) -> Option<usize> {
    let n = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    (n > 0 && n <= MAX_FRAME_BYTES).then_some(n)
}

pub fn parse_header(p: &[u8]) -> Option<(u64, u32, u32, &[u8])> {
    if p.len() < 16 {
        return None;
    }
    Some((
        u64::from_be_bytes(p[0..8].try_into().ok()?),
        u32::from_be_bytes(p[8..12].try_into().ok()?),
        u32::from_be_bytes(p[12..16].try_into().ok()?),
        &p[16..],
    ))
}

/// Downscales (bilinear-ish box) to `w x h` if needed.
pub fn scale(frame: &VideoFrame, w: u32, h: u32) -> VideoFrame {
    if (frame.width, frame.height) == (w, h) || w == 0 || h == 0 {
        return frame.clone();
    }
    let (sw, sh) = (frame.width as usize, frame.height as usize);
    let src = frame.data.as_slice();
    let mut out = vec![0u8; (w * h * 4) as usize];
    let (fx, fy) = (sw as f32 / w as f32, sh as f32 / h as f32);
    out.par_chunks_exact_mut(w as usize * 4).enumerate().for_each(|(y, row)| {
        let sy0 = (y as f32 * fy) as usize;
        let sy1 = (((y + 1) as f32 * fy) as usize).clamp(sy0 + 1, sh);
        for x in 0..w as usize {
            let sx0 = (x as f32 * fx) as usize;
            let sx1 = (((x + 1) as f32 * fx) as usize).clamp(sx0 + 1, sw);
            let mut acc = [0u32; 4];
            let mut n = 0u32;
            // Sample at most a 4x4 grid inside the box: cheap and smooth enough.
            let stepy = ((sy1 - sy0) / 4).max(1);
            let stepx = ((sx1 - sx0) / 4).max(1);
            for sy in (sy0..sy1).step_by(stepy) {
                for sx in (sx0..sx1).step_by(stepx) {
                    let i = (sy * sw + sx) * 4;
                    for c in 0..4 {
                        acc[c] += src[i + c] as u32;
                    }
                    n += 1;
                }
            }
            for c in 0..4 {
                row[x * 4 + c] = (acc[c] / n.max(1)) as u8;
            }
        }
    });
    VideoFrame::new(w, h, out)
}

/// JPEG frames, the way obs-teleport sends them: each frame is one complete
/// JPEG (libjpeg-turbo, SIMD) straight from BGRA with 4:2:0 chroma, as obs-teleport sends from OBS's
/// I420 output: about half the encode work and a third less data than full-resolution (4:4:4) chroma,
/// which is what lets 1440p60 keep up on a PC that is also running a game. Throughput comes from
/// encoding several frames at once on separate workers, not from splitting a frame.
pub struct JpegEncoder {
    c: turbojpeg::Compressor,
    quality: u8,
    /// Scratch for planar YCbCr (reused).
    planar: Vec<u8>,
}

impl JpegEncoder {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_chroma(false)
    }

    /// `full_chroma`: 4:4:4 instead of 4:2:0.
    pub fn with_chroma(full_chroma: bool) -> anyhow::Result<Self> {
        let mut c = turbojpeg::Compressor::new()?;
        c.set_subsamp(if full_chroma { turbojpeg::Subsamp::None } else { turbojpeg::Subsamp::Sub2x2 })?;
        c.set_quality(90)?;
        Ok(Self { c, quality: 90, planar: Vec::new() })
    }

    /// Payload: header, then the JPEG.
    pub fn encode(&mut self, frame: &VideoFrame, quality: u8, ts: u64) -> anyhow::Result<Vec<u8>> {
        let q = quality.clamp(10, 100);
        if q != self.quality {
            self.c.set_quality(i32::from(q))?;
            self.quality = q;
        }
        let image = turbojpeg::Image {
            pixels: frame.data.as_slice(),
            width: frame.width as usize,
            pitch: frame.width as usize * 4,
            height: frame.height as usize,
            format: turbojpeg::PixelFormat::BGRA,
        };
        let jpeg = self.c.compress_to_vec(image)?;
        let mut v = header(ts, frame.width, frame.height);
        v.extend_from_slice(&jpeg);
        Ok(v)
    }
}

impl JpegEncoder {
    /// Compresses JPEG-ready YCbCr (BT.601 full range, NV12 layout: a Y plane, then Cb/Cr pairs at
    /// half size) without any colour conversion: the planes are only split into the planar form
    /// libjpeg-turbo takes. 4:2:0, like obs-teleport.
    pub fn encode_jfif_nv12(&mut self, w: u32, h: u32, nv12: &[u8], quality: u8, ts: u64) -> anyhow::Result<Vec<u8>> {
        let (wu, hu) = (w as usize, h as usize);
        anyhow::ensure!(w.is_multiple_of(2) && h.is_multiple_of(2) && nv12.len() >= wu * hu * 3 / 2, "bad YCbCr frame");
        let q = quality.clamp(10, 100);
        if q != self.quality {
            self.c.set_quality(i32::from(q))?;
            self.quality = q;
        }
        let (luma, chroma) = nv12.split_at(wu * hu);
        let quarter = wu * hu / 4;
        self.planar.clear();
        self.planar.reserve(wu * hu * 3 / 2);
        self.planar.extend_from_slice(luma);
        self.planar.extend(chroma[..quarter * 2].iter().step_by(2));
        self.planar.extend(chroma[..quarter * 2].iter().skip(1).step_by(2));
        let image = turbojpeg::YuvImage { pixels: self.planar.as_slice(), width: wu, align: 1, height: hu, subsamp: turbojpeg::Subsamp::Sub2x2 };
        let jpeg = self.c.compress_yuv_to_vec(image)?;
        let mut v = header(ts, w, h);
        v.extend_from_slice(&jpeg);
        Ok(v)
    }
}

/// Decodes whole-frame JPEGs into recycled BGRA buffers.
pub struct JpegDecoder {
    d: turbojpeg::Decompressor,
    pool: ferrous_core::FramePool,
}

impl JpegDecoder {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self { d: turbojpeg::Decompressor::new()?, pool: ferrous_core::FramePool::default() })
    }

    pub fn decode(&mut self, jpeg: &[u8], w: u32, h: u32) -> Option<VideoFrame> {
        let size = frame_bytes(w, h)?;
        let head = self.d.read_header(jpeg).ok()?;
        if (head.width, head.height) != (w as usize, h as usize) {
            return None;
        }
        let (mut buf, _) = self.pool.take(size);
        let image = turbojpeg::Image {
            pixels: buf.as_mut_slice(),
            width: w as usize,
            pitch: w as usize * 4,
            height: h as usize,
            format: turbojpeg::PixelFormat::BGRA,
        };
        self.d.decompress(jpeg, image).ok()?;
        // The decoder writes B, G, R and padding; make the padding opaque.
        buf.par_chunks_exact_mut(4 * 1024).for_each(|c| c.chunks_exact_mut(4).for_each(|p| p[3] = 255));
        let f = VideoFrame::new(w, h, buf);
        self.pool.track(&f);
        Some(f)
    }
}

/// NV12 (BT.709, limited range, as the Hardware codec's encoder is fed) to
/// BGRA, in parallel. `out` must hold `w * h * 4` bytes.
pub fn nv12_to_bgra(nv12: &[u8], w: usize, h: usize, out: &mut [u8]) {
    let (y_plane, uv_plane) = nv12.split_at(w * h);
    out.par_chunks_exact_mut(w * 4 * 2).enumerate().for_each(|(pair, rows)| {
        let uv = &uv_plane[pair * w..pair * w + w];
        for (dy, row) in rows.chunks_exact_mut(w * 4).enumerate() {
            let y_row = &y_plane[(pair * 2 + dy) * w..][..w];
            for (x, px) in row.chunks_exact_mut(4).enumerate() {
                // Fixed point, Q13. BT.709 limited range.
                let c = (y_row[x] as i32 - 16) * 9539;
                let d = uv[x & !1] as i32 - 128;
                let e = uv[(x & !1) + 1] as i32 - 128;
                let r = (c + 14688 * e + 4096) >> 13;
                let g = (c - 1747 * d - 4366 * e + 4096) >> 13;
                let b = (c + 17307 * d + 4096) >> 13;
                px[0] = b.clamp(0, 255) as u8;
                px[1] = g.clamp(0, 255) as u8;
                px[2] = r.clamp(0, 255) as u8;
                px[3] = 255;
            }
        }
    });
}

/// Lossless chunks: compressed independently so every core works on one.
const LOSSLESS_CHUNKS: usize = 16;

/// Lossless: XOR against the previous frame (static areas become zeros),
/// then LZ4, both in parallel chunks. Payload: header, `[u8 keyframe]`,
/// `[u32 chunk_count]`, then `[u32 len][lz4 block, size-prepended]` per chunk.
#[derive(Default)]
pub struct LosslessEncoder {
    prev: Option<VideoFrame>,
    since_key: u32,
}

fn chunk_len(total: usize) -> usize {
    total.div_ceil(LOSSLESS_CHUNKS).div_ceil(4) * 4
}

impl LosslessEncoder {
    /// `force_key` when a new receiver joins or one missed a delta.
    /// Returns the payload and whether it is a keyframe.
    pub fn encode(&mut self, frame: &VideoFrame, ts: u64, force_key: bool) -> (Vec<u8>, bool) {
        let key = force_key
            || self.since_key >= 120
            || self.prev.as_ref().is_none_or(|p| (p.width, p.height) != (frame.width, frame.height));
        self.since_key = if key { 0 } else { self.since_key + 1 };
        let cur = frame.data.as_slice();
        let n = chunk_len(cur.len());
        let blocks: Vec<Vec<u8>> = match (&self.prev, key) {
            (Some(prev), false) => cur
                .par_chunks(n)
                .zip(prev.data.par_chunks(n))
                .map(|(a, b)| {
                    let diff: Vec<u8> = a.iter().zip(b).map(|(x, y)| x ^ y).collect();
                    lz4_flex::compress_prepend_size(&diff)
                })
                .collect(),
            _ => cur.par_chunks(n).map(lz4_flex::compress_prepend_size).collect(),
        };
        self.prev = Some(frame.clone());
        let mut v = header(ts, frame.width, frame.height);
        v.reserve(5 + blocks.iter().map(|b| b.len() + 4).sum::<usize>());
        v.push(key as u8);
        v.extend_from_slice(&(blocks.len() as u32).to_be_bytes());
        for b in blocks {
            v.extend_from_slice(&(b.len() as u32).to_be_bytes());
            v.extend_from_slice(&b);
        }
        (v, key)
    }
}

#[derive(Default)]
pub struct LosslessDecoder {
    prev: Option<VideoFrame>,
}

impl LosslessDecoder {
    pub fn decode(&mut self, data: &[u8], w: u32, h: u32) -> Option<VideoFrame> {
        let total = frame_bytes(w, h)?;
        let n = chunk_len(total);
        let expected = total.div_ceil(n);
        let key = *data.first()? == 1;
        let count = u32::from_be_bytes(data.get(1..5)?.try_into().ok()?) as usize;
        // The count is the peer's word: check it before reserving anything.
        if count != expected {
            return None;
        }
        let mut off = 5usize;
        let mut blocks = Vec::with_capacity(count);
        for _ in 0..count {
            let len = u32::from_be_bytes(data.get(off..off.checked_add(4)?)?.try_into().ok()?) as usize;
            off += 4;
            blocks.push(data.get(off..off.checked_add(len)?)?);
            off = off.checked_add(len)?;
        }
        let prev = if key { None } else { Some(self.prev.as_ref().filter(|p| (p.width, p.height) == (w, h))?.clone()) };
        let mut out = vec![0u8; total];
        let ok = out.par_chunks_mut(n).zip(blocks.into_par_iter()).enumerate().all(|(i, (dst, block))| {
            // The size prefix is untrusted too: it must match the chunk before LZ4 allocates it.
            if block.get(..4).map(|p| u32::from_le_bytes([p[0], p[1], p[2], p[3]]) as usize) != Some(dst.len()) {
                return false;
            }
            let Ok(raw) = lz4_flex::decompress_size_prepended(block) else { return false };
            if raw.len() != dst.len() {
                return false;
            }
            match &prev {
                None => dst.copy_from_slice(&raw),
                Some(p) => {
                    let base = &p.data[i * n..i * n + dst.len()];
                    for ((d, r), b) in dst.iter_mut().zip(&raw).zip(base) {
                        *d = r ^ b;
                    }
                }
            }
            true
        });
        if !ok {
            return None;
        }
        let f = VideoFrame::new(w, h, out);
        self.prev = Some(f.clone());
        Some(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JPEG straight from JFIF YCbCr decodes to the colour it describes.
    #[test]
    fn jpeg_from_ycbcr_planes_decodes_to_the_right_colour() {
        let (w, h) = (64u32, 32u32);
        // Pure blue in JFIF: Y 29, Cb 255, Cr 107.
        let mut nv12 = vec![29u8; (w * h) as usize];
        for _ in 0..(w * h / 4) {
            nv12.extend_from_slice(&[255, 107]);
        }
        let mut enc = JpegEncoder::new().unwrap();
        let payload = enc.encode_jfif_nv12(w, h, &nv12, 95, 0).unwrap();
        let mut dec = JpegDecoder::new().unwrap();
        let f = dec.decode(&payload[16..], w, h).unwrap();
        let px = &f.data[(16 * w as usize + 30) * 4..][..4];
        assert!(px[0] > 240 && px[1] < 15 && px[2] < 15, "BGRA {px:?}");
    }

    fn pattern(w: u32, h: u32, shift: u32) -> VideoFrame {
        let mut d = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                d[i] = ((x + shift) * 3) as u8;
                d[i + 1] = (y * 5) as u8;
                d[i + 2] = 128;
                d[i + 3] = if x < 10 { 0 } else { 255 }; // alpha must survive lossless
            }
        }
        VideoFrame::new(w, h, d)
    }

    #[test]
    fn jpeg_roundtrip_is_close() {
        let f = pattern(320, 180, 0);
        let enc = JpegEncoder::new().unwrap().encode(&f, 90, 42).unwrap();
        let (ts, w, h, rest) = parse_header(&enc).unwrap();
        assert_eq!((ts, w, h), (42, 320, 180));
        let d = JpegDecoder::new().unwrap().decode(rest, w, h).unwrap();
        assert!(d.data.chunks_exact(4).all(|p| p[3] == 255), "decoded frames are opaque");
        let err: f64 = f.data.chunks_exact(4).zip(d.data.chunks_exact(4))
            .map(|(a, b)| (0..3).map(|c| (a[c] as f64 - b[c] as f64).abs()).sum::<f64>())
            .sum::<f64>() / (320.0 * 180.0 * 3.0);
        assert!(err < 4.0, "mean error {err}");
        assert!(enc.len() < f.byte_len() / 5, "compressed {} vs {}", enc.len(), f.byte_len());
    }

    #[test]
    fn lossless_is_exact_and_deltas_are_small() {
        let mut e = LosslessEncoder::default();
        let mut d = LosslessDecoder::default();
        let a = pattern(256, 128, 0);
        let b = pattern(256, 128, 0); // identical: delta should be tiny
        let c = pattern(256, 128, 7);
        for (f, force) in [(&a, false), (&b, false), (&c, false)] {
            let (enc, _) = e.encode(f, 1, force);
            let (_, w, h, rest) = parse_header(&enc).unwrap();
            let out = d.decode(rest, w, h).unwrap();
            assert_eq!(out.data, f.data, "lossless roundtrip");
        }
        let key = e.encode(&a, 1, true).0.len();
        let same = e.encode(&a, 1, false).0.len();
        assert!(same * 20 < key, "static delta {same} vs keyframe {key}");
    }

    #[test]
    fn hostile_headers_are_refused_not_fatal() {
        // Zero, wrapping and absurd sizes; a chunk count of four billion; a lying LZ4 size prefix.
        let mut d = LosslessDecoder::default();
        let mut huge_count = vec![1u8];
        huge_count.extend_from_slice(&u32::MAX.to_be_bytes());
        for (w, h) in [(0, 0), (0, 10), (65535, 65535), (u32::MAX, u32::MAX), (100_000, 100_000)] {
            assert!(d.decode(&huge_count, w, h).is_none(), "{w}x{h}");
            assert!(JpegDecoder::new().unwrap().decode(&[0xFF, 0xD8], w, h).is_none(), "{w}x{h}");
        }
        assert!(d.decode(&huge_count, 64, 64).is_none(), "count must match the picture");
        let n = chunk_len(64 * 64 * 4);
        let mut lying = vec![1u8];
        lying.extend_from_slice(&((64usize * 64 * 4).div_ceil(n) as u32).to_be_bytes());
        for _ in 0..(64usize * 64 * 4).div_ceil(n) {
            let mut blk = u32::MAX.to_le_bytes().to_vec(); // "decompresses to 4 GB"
            blk.extend_from_slice(&[0; 8]);
            lying.extend_from_slice(&(blk.len() as u32).to_be_bytes());
            lying.extend_from_slice(&blk);
        }
        assert!(d.decode(&lying, 64, 64).is_none());
    }

    #[test]
    fn frame_bytes_checks_sizes() {
        assert_eq!(frame_bytes(1920, 1080), Some(1920 * 1080 * 4));
        assert_eq!(frame_bytes(7680, 4320), Some(7680 * 4320 * 4));
        assert_eq!(frame_bytes(0, 5), None);
        assert_eq!(frame_bytes(65535, 65535), None);
        assert_eq!(frame_bytes(u32::MAX, u32::MAX), None);
    }

    #[test]
    fn scale_down() {
        let f = VideoFrame::solid(400, 200, [10, 20, 30, 255]);
        let s = scale(&f, 100, 50);
        assert_eq!((s.width, s.height), (100, 50));
        assert_eq!(&s.data[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn nv12_round_trip_matches_the_encoder_converter() {
        let (w, h) = (64u32, 32u32);
        let f = pattern(w, h, 3);
        let conv = ferrous_output::convert::Converter::new((w, h), (w, h), ferrous_output::convert::YuvLayout::Nv12);
        let mut nv12 = Vec::new();
        conv.convert(&f.data, &mut nv12);
        let mut back = vec![0u8; (w * h * 4) as usize];
        nv12_to_bgra(&nv12, w as usize, h as usize, &mut back);
        // Chroma is shared by 2x2 pixels, so compare a smooth area's average.
        let err: f64 = f.data.chunks_exact(4).zip(back.chunks_exact(4)).skip(16 * w as usize)
            .map(|(a, b)| (0..3).map(|c| (a[c] as f64 - b[c] as f64).abs()).sum::<f64>())
            .sum::<f64>() / ((w * h / 2) as f64 * 3.0);
        assert!(err < 6.0, "mean error {err}");
        let grey = VideoFrame::solid(w, h, [128, 128, 128, 255]);
        conv.convert(&grey.data, &mut nv12);
        nv12_to_bgra(&nv12, w as usize, h as usize, &mut back);
        assert!(back.chunks_exact(4).all(|p| p[0].abs_diff(128) <= 1 && p[1].abs_diff(128) <= 1 && p[2].abs_diff(128) <= 1), "grey stays grey");
    }
}