//! Setup benchmark: measures this PC and connection, then recommends
//! separate streaming and recording settings.
//!
//! - **Encoders**: a real timed FFmpeg encode per encoder and candidate
//!   resolution/frame rate; reports the achieved frames per second.
//! - **Compositor**: renders a busy 3-layer scene at each resolution.
//! - **Upload**: a timed HTTP upload (Cloudflare's public speed-test
//!   endpoint by default).
//! - **Ingest servers**: TCP connect latency and jitter to each candidate
//!   (Twitch's published list, YouTube primary/backup, or custom URLs), and
//!   optionally Twitch's bandwidth-test mode, which streams without going live.

pub mod compare;
pub mod ingest;

use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use ferrous_core::{EncoderSettings, RateControl, Transform, VideoFrame};
use ferrous_encode::autoconfig::{self, Goal};
use ferrous_encode::{ffmpeg, EncoderInfo};
use ferrous_render::{Layer, Renderer};

pub const DEFAULT_UPLOAD_URL: &str = "https://speed.cloudflare.com/__up";

#[derive(Clone, Debug)]
pub struct EncoderResult {
    pub encoder: &'static EncoderInfo,
    pub width: u32,
    pub height: u32,
    pub target_fps: u32,
    /// Frames per second the encoder actually sustained.
    pub achieved_fps: f32,
}

impl EncoderResult {
    pub fn realtime(&self) -> bool {
        // 10% headroom for the rest of the app.
        self.achieved_fps >= self.target_fps as f32 * 1.1
    }
}

/// Parses the last `speed=` from FFmpeg's progress output.
pub fn parse_speed(stderr: &str) -> Option<f32> {
    stderr
        .split(['\r', '\n'])
        .filter_map(|l| l.split("speed=").nth(1))
        .filter_map(|v| v.split_whitespace().next())
        .filter_map(|v| v.trim_end_matches('x').parse::<f32>().ok())
        .next_back()
}

/// Encodes `secs` of a moving test pattern as fast as possible.
pub fn bench_encoder(ffmpeg_path: &Path, enc: &'static EncoderInfo, w: u32, h: u32, fps: u32, secs: f32) -> anyhow::Result<EncoderResult> {
    let es = EncoderSettings {
        encoder: enc.name.to_string(),
        rate_control: RateControl::Cbr,
        bitrate_kbps: 8000,
        preset: enc.default_preset.to_string(),
        ..EncoderSettings::streaming_default()
    };
    let out = ffmpeg::command(ffmpeg_path)
        .args(["-hide_banner", "-nostdin", "-stats", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=s={w}x{h}:r={fps}"))
        .args(["-t", &format!("{secs}")])
        .args(["-vf", &format!("format={}", ferrous_encode::args::encoder_pix_fmt(enc.name))])
        .args(ferrous_encode::args::video_args(&es, fps))
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    let text = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        anyhow::bail!("{} failed: {}", enc.name, text.lines().last().unwrap_or_default());
    }
    let speed = parse_speed(&text).ok_or_else(|| anyhow::anyhow!("no speed reported"))?;
    Ok(EncoderResult { encoder: enc, width: w, height: h, target_fps: fps, achieved_fps: speed * fps as f32 })
}

/// Milliseconds per frame to compose a busy 3-layer scene at `w x h`.
pub fn bench_compositor(w: u32, h: u32, gpu: bool, frames: u32) -> (String, f32) {
    let (mut r, _) = Renderer::new(w, h, gpu);
    let full = VideoFrame::solid(w, h, [40, 80, 120, 255]);
    let cam = VideoFrame::solid(w / 3, h / 3, [200, 100, 50, 230]);
    let overlay = VideoFrame::solid(w, h / 6, [255, 255, 255, 128]);
    // Changing data each frame defeats texture caching, like live capture.
    let mut total = Duration::ZERO;
    for i in 0..frames {
        let mut live = full.data.as_ref().clone();
        let n = live.len();
        live[(i as usize * 4) % n] = i as u8;
        let live = VideoFrame::new(w, h, live);
        let layers = [
            Layer { frame: &live, transform: Transform::default(), filters: &[] },
            Layer { frame: &cam, transform: Transform { x: w as f32 * 0.6, y: h as f32 * 0.6, scale_x: 1.1, scale_y: 1.1, ..Default::default() }, filters: &[] },
            Layer { frame: &overlay, transform: Transform { y: h as f32 * 0.8, opacity: 0.8, ..Default::default() }, filters: &[] },
        ];
        let t = Instant::now();
        let _ = r.compose(&layers);
        total += t.elapsed();
    }
    (r.describe(), total.as_secs_f32() * 1000.0 / frames.max(1) as f32)
}

/// Uploads generated data for `secs` and returns kbps. Uses chunked
/// transfer, so the server sees one request.
pub fn bench_upload(url: &str, secs: f32) -> anyhow::Result<u32> {
    struct Timed {
        until: Instant,
        sent: u64,
        buf: Vec<u8>,
    }
    impl Read for Timed {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if Instant::now() >= self.until {
                return Ok(0);
            }
            let n = out.len().min(self.buf.len());
            out[..n].copy_from_slice(&self.buf[..n]);
            self.sent += n as u64;
            Ok(n)
        }
    }
    // Incompressible-ish payload so nothing on the path can shrink it.
    let mut seed = 0x9E37_79B9u32;
    let buf: Vec<u8> = (0..256 * 1024)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect();
    let start = Instant::now();
    let mut body = Timed { until: start + Duration::from_secs_f32(secs), sent: 0, buf };
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs_f32(secs + 15.0))).build().into();
    let res = agent.post(url).header("Content-Type", "application/octet-stream").send(ureq::SendBody::from_reader(&mut body));
    let elapsed = start.elapsed().as_secs_f32();
    res.map_err(|e| anyhow::anyhow!("upload test failed: {e}"))?;
    Ok((body.sent as f32 * 8.0 / 1000.0 / elapsed.max(0.1)) as u32)
}

#[derive(Clone, Debug, Default)]
pub struct BenchReport {
    pub encoders: Vec<EncoderResult>,
    pub encoder_errors: Vec<String>,
    /// (resolution, describe, ms per frame)
    pub compositor: Vec<((u32, u32), String, f32)>,
    pub upload_kbps: Option<u32>,
    pub upload_error: Option<String>,
    pub ingests: Vec<ingest::IngestResult>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Recommendation {
    pub output: (u32, u32),
    pub fps: u32,
    pub encoder: EncoderSettings,
    pub notes: Vec<String>,
}

/// Candidate outputs, best first, keeping 16:9.
pub const CANDIDATES: &[(u32, u32, u32)] = &[(2560, 1440, 60), (1920, 1080, 60), (1920, 1080, 30), (1280, 720, 60), (1280, 720, 30)];

/// Best encoder result that runs in real time at this size and rate.
fn capable<'a>(r: &'a BenchReport, enc: &str, w: u32, h: u32, fps: u32) -> Option<&'a EncoderResult> {
    r.encoders.iter().find(|e| e.encoder.name == enc && e.width == w && e.height == h && e.target_fps == fps && e.realtime())
}

fn compositor_ok(r: &BenchReport, w: u32, h: u32, fps: u32) -> bool {
    // Unknown sizes are assumed fine; otherwise keep compositing under 60% of the frame budget.
    r.compositor.iter().find(|(s, _, _)| *s == (w, h)).is_none_or(|(_, _, ms)| *ms <= 0.6 * 1000.0 / fps as f32)
}

/// Turns measurements into a recommendation for one goal.
pub fn recommend(r: &BenchReport, goal: Goal, display: (u32, u32), platform_max_kbps: u32) -> Recommendation {
    let working: Vec<&'static EncoderInfo> = {
        let mut v: Vec<&'static EncoderInfo> = r.encoders.iter().map(|e| e.encoder).collect();
        v.dedup_by_key(|e| e.name);
        v
    };
    let base = autoconfig::recommend(&autoconfig::Inputs {
        goal,
        working: &working,
        display,
        prefer_fps: 60,
        upload_kbps: r.upload_kbps,
        platform_max_kbps,
    });
    // Keep the encoder/bitrate reasoning; resolution is decided below from measurements.
    let mut notes: Vec<String> = base.notes.iter().filter(|n| !n.starts_with("Streaming at") && !n.starts_with("Recording at") && !n.starts_with("Frame rate lowered")).cloned().collect();
    let enc_name = base.encoder.encoder.clone();
    // Walk down from the autoconfig choice until the measured encoder and
    // compositor both keep up.
    let start = CANDIDATES.iter().position(|(_, h, f)| *h <= base.output.1 && *f <= base.fps).unwrap_or(0);
    let mut chosen = None;
    for &(w, h, f) in &CANDIDATES[start..] {
        if h > display.1 {
            continue;
        }
        if let Some(e) = capable(r, &enc_name, w, h, f) {
            if compositor_ok(r, w, h, f) {
                notes.push(format!("{} sustained {:.0} fps at {w}×{h} (needs {f})", e.encoder.name, e.achieved_fps));
                chosen = Some((w, h, f));
                break;
            }
            notes.push(format!("Compositing {w}×{h} is too slow for {f} fps on this PC"));
        }
    }
    let (w, h, fps) = chosen.unwrap_or_else(|| {
        if !r.encoders.is_empty() {
            notes.push("No tested setting ran in real time; using the safest one".into());
        }
        (base.output.0.min(1280), base.output.1.min(720), 30)
    });
    notes.push(format!("Result: {w}×{h} at {fps} fps"));
    let mut encoder = base.encoder.clone();
    if goal == Goal::Streaming && (w, h, fps) != (base.output.0, base.output.1, base.fps) {
        // Bitrate stays within the upload budget computed by autoconfig.
        encoder.bitrate_kbps = encoder.bitrate_kbps.min(base.encoder.bitrate_kbps);
    }
    Recommendation { output: (w, h), fps, encoder, notes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrous_encode::catalog::find;

    #[test]
    fn parses_ffmpeg_speed() {
        let s = "frame=  10 fps=0.0 q=0.0 size=0kB time=00:00:00.33 bitrate=N/A speed=0.66x\rframe= 120 fps=118 q=-1.0 Lsize=N/A time=00:00:02.00 bitrate=N/A speed=1.97x\n";
        assert_eq!(parse_speed(s), Some(1.97));
    }

    fn result(enc: &str, w: u32, h: u32, f: u32, got: f32) -> EncoderResult {
        EncoderResult { encoder: find(enc).unwrap(), width: w, height: h, target_fps: f, achieved_fps: got }
    }

    #[test]
    fn recommendation_respects_measured_capacity() {
        // NVENC handles 1080p60 but not 1440p60; compositing is fast enough.
        let r = BenchReport {
            encoders: vec![
                result("h264_nvenc", 2560, 1440, 60, 50.0),
                result("h264_nvenc", 1920, 1080, 60, 140.0),
                result("hevc_nvenc", 2560, 1440, 60, 90.0),
                result("hevc_nvenc", 1920, 1080, 60, 150.0),
            ],
            compositor: vec![((2560, 1440), "CPU".into(), 6.0), ((1920, 1080), "CPU".into(), 3.0)],
            upload_kbps: Some(30_000),
            ..Default::default()
        };
        let s = recommend(&r, Goal::Streaming, (2560, 1440), 8000);
        assert_eq!(s.encoder.encoder, "h264_nvenc");
        assert_eq!((s.output, s.fps), ((1920, 1080), 60));
        let rec = recommend(&r, Goal::Recording, (2560, 1440), 0);
        assert_eq!(rec.encoder.encoder, "hevc_nvenc");
        assert_eq!((rec.output, rec.fps), ((2560, 1440), 60), "{:?}", rec.notes);
    }

    #[test]
    fn slow_compositor_lowers_resolution() {
        let r = BenchReport {
            encoders: vec![result("libx264", 1920, 1080, 60, 80.0), result("libx264", 1920, 1080, 30, 90.0), result("libx264", 1280, 720, 60, 150.0)],
            compositor: vec![((1920, 1080), "CPU".into(), 14.0), ((1280, 720), "CPU".into(), 5.0)],
            upload_kbps: Some(50_000),
            ..Default::default()
        };
        // 14 ms > 60% of 16.7 ms, so 1080p60 is out; 1080p30 (budget 20 ms) fits.
        let s = recommend(&r, Goal::Streaming, (1920, 1080), 8000);
        assert_eq!((s.output, s.fps), ((1920, 1080), 30), "{:?}", s.notes);
    }

    #[test]
    fn real_encoder_and_compositor_bench() {
        let Some(x264) = find("libx264") else { return };
        if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let r = bench_encoder(Path::new("ffmpeg"), x264, 640, 360, 30, 1.0).unwrap();
        println!("x264 640x360: {:.0} fps", r.achieved_fps);
        assert!(r.achieved_fps > 30.0);
        let (desc, ms) = bench_compositor(640, 360, false, 10);
        println!("{desc}: {ms:.2} ms/frame");
        assert!(ms > 0.0 && ms < 200.0);
    }

    /// Upload measurement against a local HTTP sink.
    #[test]
    fn upload_bench_against_local_server() {
        use std::io::{BufRead, BufReader, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            // Headers, then drain the chunked body until the 0-length chunk.
            let mut line = String::new();
            loop {
                line.clear();
                r.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            loop {
                line.clear();
                if r.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let n = usize::from_str_radix(line.trim(), 16).unwrap_or(0);
                let mut skip = vec![0u8; n + 2];
                r.read_exact(&mut skip).unwrap();
                if n == 0 {
                    break;
                }
            }
            let mut s = s;
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").unwrap();
        });
        let kbps = bench_upload(&format!("http://127.0.0.1:{port}/up"), 1.0).unwrap();
        println!("loopback upload {kbps} kbps");
        assert!(kbps > 10_000);
    }
}
