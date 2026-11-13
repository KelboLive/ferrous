//! Real sender ↔ receiver over loopback TCP.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferrous_core::{LinkCodec, VideoFrame};
use ferrous_link::sender::SenderConfig;
use ferrous_link::{LinkReceiver, LinkSender};
use parking_lot::Mutex;

fn cfg(name: &str, codec: LinkCodec, pin: &str) -> SenderConfig {
    SenderConfig { name: name.into(), codec, width: 320, height: 180, fps: 30, port: 0, pin: pin.into(), audio: true, ffmpeg: PathBuf::from("ffmpeg") }
}

fn frame(t: u8) -> VideoFrame {
    let mut d = vec![0u8; 320 * 180 * 4];
    for (i, px) in d.chunks_exact_mut(4).enumerate() {
        px.copy_from_slice(&[t, (i % 320) as u8, 200, if i % 320 < 20 { 64 } else { 255 }]);
    }
    VideoFrame::new(320, 180, d)
}

/// Pumps frames until the receiver has one (or times out).
fn pump(s: &LinkSender, r: &LinkReceiver, f: &VideoFrame, secs: u64) -> Option<VideoFrame> {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        s.push_frame(f);
        std::thread::sleep(Duration::from_millis(33));
        if let Some(got) = r.latest_frame() {
            return Some(got);
        }
    }
    None
}

#[test]
fn jpeg_link_with_pin_audio_and_stats() {
    let s = LinkSender::start(cfg("Gaming PC", LinkCodec::Jpeg { quality: 90, adaptive: true }, "4321")).unwrap();
    let heard: Arc<Mutex<Vec<f32>>> = Arc::default();
    let h2 = heard.clone();
    let r = LinkReceiver::start(&format!("127.0.0.1:{}", s.port), "4321", PathBuf::from("ffmpeg"), Some(Arc::new(move |a: &[f32]| h2.lock().extend_from_slice(a))));
    let f = frame(100);
    let got = pump(&s, &r, &f, 10).expect("no frame received");
    assert_eq!((got.width, got.height), (320, 180));
    assert!((got.data[0] as i32 - 100).abs() < 8, "blue channel ~100, got {}", got.data[0]);
    for _ in 0..30 {
        s.push_frame(&f);
        s.push_audio(&[0.5f32; 960]);
        std::thread::sleep(Duration::from_millis(40));
    }
    let a = heard.lock();
    assert!(!a.is_empty(), "audio arrived");
    assert!((a[0] - 0.5).abs() < 0.001);
    assert!(r.stats.connected.load(Ordering::Relaxed));
    assert_eq!(*r.stats.sender_name.lock(), "Gaming PC");
    assert!(r.stats.rtt_ms.load(Ordering::Relaxed) < 200);
    assert!(r.stats.latency_ms.load(Ordering::Relaxed) < 500, "latency {}", r.stats.latency_ms.load(Ordering::Relaxed));
    assert_eq!(s.stats.clients.load(Ordering::Relaxed), 1);
    assert!(s.stats.kbps.load(Ordering::Relaxed) > 0);
    println!("latency {} ms, rtt {} ms, {} kbps", r.stats.latency_ms.load(Ordering::Relaxed), r.stats.rtt_ms.load(Ordering::Relaxed), r.stats.kbps.load(Ordering::Relaxed));
}

#[test]
fn wrong_or_missing_pin_is_refused() {
    let s = LinkSender::start(cfg("Locked", LinkCodec::Jpeg { quality: 80, adaptive: false }, "1111")).unwrap();
    let addr = format!("127.0.0.1:{}", s.port);
    let wrong = LinkReceiver::start(&addr, "2222", PathBuf::from("ffmpeg"), None);
    let none = LinkReceiver::start(&addr, "", PathBuf::from("ffmpeg"), None);
    assert!(pump(&s, &wrong, &frame(1), 3).is_none());
    std::thread::sleep(Duration::from_millis(300));
    let e1 = wrong.stats.error.lock().clone().unwrap_or_default();
    let e2 = none.stats.error.lock().clone().unwrap_or_default();
    assert!(!e1.is_empty(), "wrong PIN reported");
    assert!(e2.contains("PIN"), "{e2}");
    assert!(!wrong.stats.connected.load(Ordering::Relaxed));
}

#[test]
fn lossless_link_is_exact_including_alpha() {
    let s = LinkSender::start(cfg("Overlay", LinkCodec::Lossless, "")).unwrap();
    let r = LinkReceiver::start(&format!("127.0.0.1:{}", s.port), "", PathBuf::from("ffmpeg"), None);
    let f = frame(77);
    let got = pump(&s, &r, &f, 10).expect("no frame");
    assert_eq!(got.data, f.data);
}

#[test]
fn hardware_codec_stream_decodes() {
    if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
        return;
    }
    let s = LinkSender::start(cfg("H264", LinkCodec::Hardware { encoder: "libx264".into(), bitrate_kbps: 2000 }, "")).unwrap();
    let r = LinkReceiver::start(&format!("127.0.0.1:{}", s.port), "", PathBuf::from("ffmpeg"), None);
    let f = frame(150);
    let got = pump(&s, &r, &f, 15).expect("no decoded frame from the H.264 link");
    assert_eq!((got.width, got.height), (320, 180));
    // Lossy, but colours should be close in the opaque area.
    let i = (90 * 320 + 200) * 4;
    assert!((got.data[i + 2] as i32 - 200).abs() < 20, "red ~200, got {}", got.data[i + 2]);
}

#[test]
fn discovered_by_name() {
    let s = LinkSender::start(cfg("Discoverable PC", LinkCodec::Jpeg { quality: 80, adaptive: false }, "")).unwrap();
    ferrous_link::discovery::start_listening();
    let mut found = false;
    for _ in 0..40 {
        if ferrous_link::discovered().iter().any(|a| a.name == "Discoverable PC" && a.port == s.port) {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(found, "beacon heard: {:?}", ferrous_link::discovered());
    let r = LinkReceiver::start("Discoverable PC", "", PathBuf::from("ffmpeg"), None);
    assert!(pump(&s, &r, &frame(5), 10).is_some(), "connected by name");
}

/// A receiver that stops reading makes the sender drop frames for it and
/// lower JPEG quality, without stalling anyone else.
#[test]
fn adaptive_quality_backs_off_for_slow_receiver() {
    use std::io::Write;
    let mut c = cfg("Busy", LinkCodec::Jpeg { quality: 95, adaptive: true }, "");
    c.width = 1280;
    c.height = 720;
    let s = LinkSender::start(c).unwrap();
    // Hand-rolled receiver that handshakes then never reads.
    let mut sock = std::net::TcpStream::connect(("127.0.0.1", s.port)).unwrap();
    let _hello = ferrous_link::protocol::read_raw(&mut sock).unwrap();
    ferrous_link::protocol::write_raw(&mut sock, &ferrous_link::protocol::frame(ferrous_link::protocol::kind::AUTH, b"{}")).unwrap();
    sock.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    // Noisy frames compress badly, so buffers fill quickly.
    let mut seed = 1u32;
    let mut noisy = vec![0u8; 1280 * 720 * 4];
    for b in noisy.iter_mut() {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        *b = seed as u8;
    }
    let end = Instant::now() + Duration::from_secs(6);
    while Instant::now() < end {
        // A new buffer each time, like live capture (an identical buffer is
        // sent as a tiny repeat and would never fill the network).
        s.push_frame(&VideoFrame::new(1280, 720, noisy.clone()));
        std::thread::sleep(Duration::from_millis(16));
    }
    let q = s.stats.quality.load(Ordering::Relaxed);
    let dropped = s.stats.frames_dropped.load(Ordering::Relaxed);
    println!("quality now {q}, dropped {dropped}");
    assert!(dropped > 0);
    assert!(q < 95, "quality should back off, still {q}");
    drop(sock);
}

/// A static scene sends the same buffer every tick, which goes out as tiny
/// repeats. Receivers joining at any point must still get the picture, for
/// both codecs, and lossless must stay exact across the repeats.
#[test]
fn static_scene_repeats_and_late_joiners_get_the_picture() {
    for codec in [LinkCodec::Jpeg { quality: 90, adaptive: false }, LinkCodec::Lossless] {
        let s = LinkSender::start(cfg("Static", codec.clone(), "")).unwrap();
        let f = frame(42);
        let first = LinkReceiver::start(&format!("127.0.0.1:{}", s.port), "", PathBuf::from("ffmpeg"), None);
        assert!(pump(&s, &first, &f, 5).is_some(), "{codec:?}: first receiver gets a frame");
        // Keep the picture static for a while, then a second receiver joins.
        for _ in 0..30 {
            s.push_frame(&f);
            std::thread::sleep(Duration::from_millis(10));
        }
        let late = LinkReceiver::start(&format!("127.0.0.1:{}", s.port), "", PathBuf::from("ffmpeg"), None);
        let got = pump(&s, &late, &f, 5).unwrap_or_else(|| panic!("{codec:?}: late joiner got no picture from a static scene"));
        if codec == LinkCodec::Lossless {
            assert_eq!(got.data, f.data, "lossless picture is exact");
        }
        let before = s.stats.bytes_sent.load(Ordering::Relaxed);
        for _ in 0..30 {
            s.push_frame(&f);
            std::thread::sleep(Duration::from_millis(10));
        }
        let per_frame = (s.stats.bytes_sent.load(Ordering::Relaxed) - before) / 30;
        assert!(per_frame < 200, "{codec:?}: static frames cost {per_frame} bytes each (should be tiny repeats)");
    }
}

/// Switching the encoder restarts the sender; a receiver that was watching
/// must come back with the new codec, on the same port, and show its picture.
#[test]
fn receiver_follows_encoder_switches() {
    if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
        return;
    }
    let hw = |bitrate| LinkCodec::Hardware { encoder: "libx264".into(), bitrate_kbps: bitrate };
    // A port of its own: the other tests start senders on the default range
    // in parallel and would take this one in the gap between two senders.
    let mut c0 = cfg("Switcher", hw(2000), "");
    c0.port = 47_930;
    let first = LinkSender::start(c0).unwrap();
    let port = first.port;
    let r = LinkReceiver::start(&format!("127.0.0.1:{port}"), "", PathBuf::from("ffmpeg"), None);
    assert!(pump(&first, &r, &frame(60), 15).is_some(), "no picture from the first encoder");
    drop(first);

    for (n, codec) in [LinkCodec::Jpeg { quality: 90, adaptive: false }, hw(3000), LinkCodec::Lossless, hw(2500)].into_iter().enumerate() {
        let mut c = cfg("Switcher", codec, "");
        c.port = port;
        let s = LinkSender::start(c).unwrap();
        assert_eq!(s.port, port, "switch {n}: the sender must take its old port back");
        // Only frames from the new sender count: forget the old picture.
        let marker = 120 + n as u8 * 20;
        let f = frame(marker);
        let end = Instant::now() + Duration::from_secs(20);
        let mut ok = false;
        while Instant::now() < end {
            s.push_frame(&f);
            std::thread::sleep(Duration::from_millis(33));
            if let Some(got) = r.latest_frame() {
                if (got.data[0] as i32 - marker as i32).abs() < 12 {
                    ok = true;
                    break;
                }
            }
        }
        assert!(ok, "switch {n}: the receiver never showed the new sender's picture");
        drop(s);
    }
}
