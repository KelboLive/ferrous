//! Ferrous Link throughput benchmark over loopback.
//!
//! `cargo run --release -p ferrous-link --example link_bench -- <codec> [secs] [WxH] [fps] [pin]`
//! codec: `jpeg85`, `jpeg95`, `lossless`, `x264:<kbps>`.
//!
//! Frames are textured, noisy and moving (a stand-in for game footage, which
//! compresses far worse than test patterns), pushed on a real-time schedule.
//! Prints the receiver's frame rate and the sender's drops per second.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ferrous_core::{LinkCodec, VideoFrame};
use ferrous_link::sender::SenderConfig;
use ferrous_link::{LinkReceiver, LinkSender};

fn frames(w: u32, h: u32, n: usize) -> Vec<VideoFrame> {
    // Deterministic noise.
    let mut seed = 0x1234_5678u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let tex: Vec<u8> = (0..(w * h) as usize).map(|_| (rnd() & 0x3f) as u8).collect();
    (0..n)
        .map(|k| {
            let mut d = vec![0u8; (w * h * 4) as usize];
            let shift = k * 7;
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let i = (y * w as usize + x) * 4;
                    let t = tex[(y * w as usize + (x + shift) % w as usize) % tex.len()];
                    // Gradient sky/ground plus texture plus a moving bright box.
                    let in_box = (x + shift * 3) % w as usize > 400 && (x + shift * 3) % (w as usize) < 700 && y > h as usize / 3 && y < h as usize / 2;
                    d[i] = (x * 255 / w as usize) as u8 / 2 + t;
                    d[i + 1] = (y * 255 / h as usize) as u8 / 2 + t;
                    d[i + 2] = if in_box { 240 } else { 60 + t };
                    d[i + 3] = 255;
                }
            }
            VideoFrame::new(w, h, d)
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let codec_arg = args.get(1).map(String::as_str).unwrap_or("jpeg85");
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let (w, h) = args
        .get(3)
        .and_then(|s| s.split_once('x'))
        .map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap()))
        .unwrap_or((1920u32, 1080u32));
    let fps: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(60);
    let pin = args.get(5).cloned().unwrap_or_default();
    let codec = match codec_arg {
        "jpeg85" => LinkCodec::Jpeg { quality: 85, adaptive: false, full_chroma: false },
        "jpeg95" => LinkCodec::Jpeg { quality: 95, adaptive: false, full_chroma: false },
        "lossless" => LinkCodec::Lossless,
        c if c.starts_with("x264:") => LinkCodec::Hardware { encoder: "libx264".into(), bitrate_kbps: c[5..].parse().unwrap() },
        c => panic!("unknown codec {c}"),
    };
    let ffmpeg = ferrous_encode::ffmpeg::locate("");
    // FERROUS_BENCH_STATIC=1: the same frame buffer every tick, like a static scene.
    let src = frames(w, h, if std::env::var_os("FERROUS_BENCH_STATIC").is_some() { 1 } else { 16 });
    let sender = LinkSender::start(SenderConfig {
        name: "bench".into(),
        codec,
        width: w,
        height: h,
        fps,
        port: 0,
        pin: pin.clone(),
        audio: false,
        ffmpeg: ffmpeg.clone(),
    })
    .unwrap();
    let rx = LinkReceiver::start(&format!("127.0.0.1:{}", sender.port), &pin, ffmpeg, None);
    // Wait for the connection.
    let t0 = Instant::now();
    while !rx.stats.connected.load(Ordering::Relaxed) && t0.elapsed() < Duration::from_secs(5) {
        sender.push_frame(&src[0]);
        std::thread::sleep(Duration::from_millis(20));
    }
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    // What a viewer sees: a 60 Hz "render loop" on the receiving side taking
    // the frame due at each tick. New picture every tick = smooth.
    let rx = std::sync::Arc::new(rx);
    let shown = {
        let rx = rx.clone();
        std::thread::spawn(move || {
            let (mut ticks, mut fresh, mut last): (u32, u32, Option<std::sync::Arc<Vec<u8>>>) = (0, 0, None);
            let t0 = Instant::now();
            let mut next = Instant::now();
            while t0.elapsed() < Duration::from_secs(secs) {
                next += Duration::from_secs_f64(1.0 / 60.0);
                if let Some(f) = rx.latest_frame() {
                    ticks += 1;
                    if !last.as_ref().is_some_and(|l| std::sync::Arc::ptr_eq(l, &f.data)) {
                        fresh += 1;
                        last = Some(f.data.clone());
                    }
                }
                if let Some(d) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
            }
            (ticks, fresh)
        })
    };
    let start = Instant::now();
    let base = (rx.stats.frames.load(Ordering::Relaxed), sender.stats.frames_dropped.load(Ordering::Relaxed), sender.stats.frames_skipped.load(Ordering::Relaxed), sender.stats.bytes_sent.load(Ordering::Relaxed));
    let mut next = Instant::now();
    let mut i = 0usize;
    let mut disconnects = 0;
    let mut was_connected = true;
    let mut lat_sum = 0u64;
    let mut lat_n = 0u64;
    let mut last_sample = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        sender.push_frame(&src[i % src.len()]);
        i += 1;
        next += period;
        let c = rx.stats.connected.load(Ordering::Relaxed);
        if was_connected && !c {
            disconnects += 1;
        }
        was_connected = c;
        if last_sample.elapsed() >= Duration::from_millis(250) {
            last_sample = Instant::now();
            lat_sum += rx.stats.latency_ms.load(Ordering::Relaxed) as u64;
            lat_n += 1;
        }
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
    }
    let el = start.elapsed().as_secs_f64();
    let got = rx.stats.frames.load(Ordering::Relaxed) - base.0;
    let dropped = sender.stats.frames_dropped.load(Ordering::Relaxed) - base.1;
    let skipped = sender.stats.frames_skipped.load(Ordering::Relaxed) - base.2;
    let bytes = sender.stats.bytes_sent.load(Ordering::Relaxed) - base.3;
    let errors = rx.stats.decode_errors.load(Ordering::Relaxed);
    let (ticks, fresh) = shown.join().unwrap();
    println!(
        "{codec_arg:<12} {w}x{h}@{fps}{}: received {:5.1} fps | new picture on {:5.1}% of 60 Hz ticks | sender skipped {:4.1}/s, dropped {:4.1}/s | {:6.1} Mbps | latency ~{:3} ms | decode errors/skips {errors} | disconnects {disconnects}",
        if pin.is_empty() { "" } else { " +PIN" },
        got as f64 / el,
        fresh as f64 * 100.0 / ticks.max(1) as f64,
        skipped as f64 / el,
        dropped as f64 / el,
        bytes as f64 * 8.0 / el / 1e6,
        lat_sum / lat_n.max(1),
    );
}
