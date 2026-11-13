//! Per-frame cost of the JPEG codec (libjpeg-turbo, one whole frame on one
//! core) at 1080p and 1440p:
//! `cargo run --release -p ferrous-link --example codec_stages`.
use std::time::Instant;
use ferrous_core::VideoFrame;
use ferrous_link::codec::{self, JpegDecoder, JpegEncoder};

fn frame(w: u32, h: u32) -> VideoFrame {
    let mut s = 0x9e3779b9u32;
    let mut d = vec![0u8; (w * h * 4) as usize];
    for (i, p) in d.chunks_exact_mut(4).enumerate() {
        s ^= s << 13; s ^= s >> 17; s ^= s << 5;
        let (x, y) = (i as u32 % w, i as u32 / w);
        let t = (s & 0x1f) as u8;
        p.copy_from_slice(&[(x * 127 / w) as u8 + t, (y * 127 / h) as u8 + t, 60 + t, 255]);
    }
    VideoFrame::new(w, h, d)
}

fn time<T>(name: &str, n: u32, mut f: impl FnMut() -> T) -> T {
    let mut out = f();
    let t = Instant::now();
    for _ in 0..n { out = f(); }
    println!("{name:<34} {:7.2} ms", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
    out
}

fn main() {
    let mut enc = JpegEncoder::new().unwrap();
    let mut dec = JpegDecoder::new().unwrap();
    for (w, h) in [(1920u32, 1080u32), (2560, 1440)] {
        let f = frame(w, h);
        let p = time(&format!("{w}x{h} q90 encode (1 core)"), 10, || enc.encode(&f, 90, 0).unwrap());
        println!("{:<34} {:7.0} KB", format!("{w}x{h} q90 size"), p.len() as f64 / 1024.0);
        let (_, w2, h2, data) = codec::parse_header(&p).unwrap();
        let data = data.to_vec();
        time(&format!("{w}x{h} q90 decode (1 core)"), 10, || dec.decode(&data, w2, h2).unwrap());
    }
}
