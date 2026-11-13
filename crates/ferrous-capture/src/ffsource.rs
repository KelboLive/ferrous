//! Sources decoded by FFmpeg: media files/URLs and video capture devices
//! (DirectShow webcams and capture cards on Windows, V4L2 elsewhere).
//!
//! FFmpeg writes BGRA frames to stdout and, when the input has audio,
//! 48 kHz stereo f32 to a loopback TCP socket we listen on.

use std::io::{BufReader, Read};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ferrous_core::VideoFrame;
use ferrous_encode::ffmpeg;
use parking_lot::Mutex;

use crate::{AudioSink, Source};

pub enum Input<'a> {
    Media { input: &'a str, looping: bool, hw_decode: bool },
    Device { device: &'a str, resolution: Option<[u32; 2]>, fps: Option<u32>, audio_device: Option<&'a str> },
}

/// Whether a media input has an audio stream, via `ffmpeg -i` stream listing.
fn media_has_audio(ffmpeg_path: &Path, input: &str) -> bool {
    let out = ffmpeg::command(ffmpeg_path)
        .args(["-hide_banner", "-nostdin", "-i", input])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stderr).lines().any(|l| l.contains("Stream #") && l.contains("Audio:")),
        Err(_) => false,
    }
}

/// Arguments for the input side.
pub fn input_args(input: &Input<'_>) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    match input {
        Input::Media { input, looping, hw_decode } => {
            // Read at native speed, like playing it.
            a.push("-re".into());
            if *looping {
                a.extend(["-stream_loop".into(), "-1".into()]);
            }
            if *hw_decode {
                a.extend(["-hwaccel".into(), "auto".into()]);
            }
            a.extend(["-i".into(), input.to_string()]);
        }
        Input::Device { device, resolution, fps, audio_device } => {
            #[cfg(windows)]
            a.extend(["-f".into(), "dshow".into(), "-rtbufsize".into(), "256M".into()]);
            #[cfg(not(windows))]
            a.extend(["-f".into(), "v4l2".into()]);
            if let Some([w, h]) = resolution {
                a.extend(["-video_size".into(), format!("{w}x{h}")]);
            }
            if let Some(f) = fps {
                a.extend(["-framerate".into(), f.to_string()]);
            }
            #[cfg(windows)]
            {
                let mut spec = format!("video={device}");
                if let Some(ad) = audio_device {
                    spec.push_str(&format!(":audio={ad}"));
                }
                a.extend(["-i".into(), spec]);
            }
            #[cfg(not(windows))]
            {
                let _ = audio_device;
                a.extend(["-i".into(), device.to_string()]);
            }
        }
    }
    a
}

/// Finds "WxH" in the rawvideo output stream line FFmpeg prints at startup.
pub fn parse_output_size(line: &str) -> Option<(u32, u32)> {
    if !(line.contains("Video: rawvideo") && line.contains("bgra")) {
        return None;
    }
    line.split(|c: char| c == ',' || c.is_whitespace()).find_map(|tok| {
        let (w, h) = tok.split_once('x')?;
        let w: u32 = w.parse().ok()?;
        let h: u32 = h.parse().ok()?;
        (w > 0 && h > 0).then_some((w, h))
    })
}

pub struct FfmpegSource {
    latest: Arc<Mutex<Option<VideoFrame>>>,
    alive: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Child>>,
    threads: Vec<JoinHandle<()>>,
    /// Devices are restarted by the engine when they die; finished files aren't.
    restart_on_exit: bool,
}

impl FfmpegSource {
    pub fn start(ffmpeg_path: &Path, input: Input<'_>, audio: Option<AudioSink>) -> anyhow::Result<Self> {
        let want_audio = audio.is_some()
            && match &input {
                Input::Media { input, .. } => media_has_audio(ffmpeg_path, input),
                Input::Device { audio_device, .. } => audio_device.is_some(),
            };
        let restart_on_exit = matches!(input, Input::Device { .. });

        let listener = if want_audio { Some(TcpListener::bind("127.0.0.1:0")?) } else { None };
        let mut args: Vec<String> =
            ["-hide_banner", "-loglevel", "info", "-nostdin"].iter().map(|s| s.to_string()).collect();
        args.extend(input_args(&input));
        args.extend(["-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", "bgra", "pipe:1"].iter().map(|s| s.to_string()));
        if let Some(l) = &listener {
            let port = l.local_addr()?.port();
            args.extend(
                ["-map", "0:a:0", "-f", "f32le", "-ar", "48000", "-ac", "2"].iter().map(|s| s.to_string()),
            );
            args.push(format!("tcp://127.0.0.1:{port}"));
        }
        log::info!("source: ffmpeg {}", args.join(" "));

        let mut child = ffmpeg::command(ffmpeg_path)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("couldn't launch FFmpeg: {e}"))?;
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let child = Arc::new(Mutex::new(child));

        let latest: Arc<Mutex<Option<VideoFrame>>> = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let (size_tx, size_rx) = crossbeam_channel::bounded::<(u32, u32)>(1);
        let errors: Arc<Mutex<Vec<String>>> = Arc::default();

        let log_thread = {
            let errors = errors.clone();
            thread::spawn(move || {
                let mut r = BufReader::new(stderr);
                let mut line = Vec::new();
                let mut in_output = false;
                let mut sent = false;
                let mut byte = [0u8; 1];
                while let Ok(1) = r.read(&mut byte) {
                    if byte[0] != b'\n' && byte[0] != b'\r' {
                        line.push(byte[0]);
                        continue;
                    }
                    let l = String::from_utf8_lossy(&line).to_string();
                    line.clear();
                    if l.starts_with("Output #0") {
                        in_output = true;
                    }
                    if in_output && !sent {
                        if let Some(sz) = parse_output_size(&l) {
                            let _ = size_tx.send(sz);
                            sent = true;
                        }
                    }
                    let lower = l.to_ascii_lowercase();
                    if lower.contains("error") || lower.contains("could not") || lower.contains("no such") {
                        let mut e = errors.lock();
                        e.push(l.trim().to_string());
                        if e.len() > 20 {
                            e.remove(0);
                        }
                    }
                }
            })
        };

        let video_thread = {
            let (latest, alive, stop, errors) = (latest.clone(), alive.clone(), stop.clone(), errors.clone());
            thread::spawn(move || {
                let mut out = stdout;
                let Ok((w, h)) = size_rx.recv_timeout(Duration::from_secs(20)) else {
                    log::warn!("source never produced video: {}", errors.lock().last().cloned().unwrap_or_default());
                    alive.store(false, Ordering::Relaxed);
                    return;
                };
                let len = (w * h * 4) as usize;
                loop {
                    let mut buf = vec![0u8; len];
                    if out.read_exact(&mut buf).is_err() || stop.load(Ordering::Relaxed) {
                        break;
                    }
                    *latest.lock() = Some(VideoFrame::new(w, h, buf));
                }
                alive.store(false, Ordering::Relaxed);
            })
        };

        let mut threads = vec![log_thread, video_thread];
        if let (Some(listener), Some(sink)) = (listener, audio.filter(|_| want_audio)) {
            let stop = stop.clone();
            threads.push(thread::spawn(move || {
                let _ = listener.set_nonblocking(true);
                let deadline = Instant::now() + Duration::from_secs(20);
                let mut sock = loop {
                    match listener.accept() {
                        Ok((s, _)) => break s,
                        Err(_) if Instant::now() < deadline && !stop.load(Ordering::Relaxed) => {
                            thread::sleep(Duration::from_millis(20))
                        }
                        Err(_) => return,
                    }
                };
                let _ = sock.set_nonblocking(false);
                let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
                let mut bytes = vec![0u8; 480 * 2 * 4];
                let mut pending: Vec<u8> = Vec::new();
                let mut samples: Vec<f32> = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    match sock.read(&mut bytes) {
                        Ok(0) => break,
                        Ok(n) => {
                            pending.extend_from_slice(&bytes[..n]);
                            let whole = pending.len() / 8 * 8; // complete stereo frames
                            samples.clear();
                            samples.extend(
                                pending[..whole].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                            );
                            pending.drain(..whole);
                            sink(&samples);
                        }
                        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                        Err(_) => break,
                    }
                }
            }));
        }

        Ok(Self { latest, alive, stop, child, threads, restart_on_exit })
    }
}

impl Source for FfmpegSource {
    fn latest_frame(&mut self) -> Option<VideoFrame> {
        self.latest.lock().clone()
    }

    fn is_alive(&self) -> bool {
        !self.restart_on_exit || self.alive.load(Ordering::Relaxed)
    }

    fn pids(&self) -> Vec<u32> {
        vec![self.child.lock().id()]
    }
}

impl Drop for FfmpegSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.child.lock().kill();
        let _ = self.child.lock().wait();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Video capture devices: DirectShow names on Windows, `/dev/video*` elsewhere.
pub fn list_video_devices(ffmpeg_path: &Path) -> Vec<String> {
    list_devices(ffmpeg_path).0
}

/// DirectShow audio devices (for capture cards' audio). Empty off Windows.
pub fn list_audio_devices(ffmpeg_path: &Path) -> Vec<String> {
    list_devices(ffmpeg_path).1
}

fn list_devices(ffmpeg_path: &Path) -> (Vec<String>, Vec<String>) {
    #[cfg(windows)]
    {
        let out = ffmpeg::command(ffmpeg_path)
            .args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"])
            .stdin(Stdio::null())
            .output();
        match out {
            Ok(o) => parse_dshow_devices(&String::from_utf8_lossy(&o.stderr)),
            Err(_) => (Vec::new(), Vec::new()),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = ffmpeg_path;
        let mut v: Vec<String> = std::fs::read_dir("/dev")
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| e.path().display().to_string())
                    .filter(|p| p.starts_with("/dev/video"))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        (v, Vec::new())
    }
}

/// Parses `ffmpeg -list_devices true -f dshow -i dummy` output (both the
/// FFmpeg 4.x section style and the 5.x+ "(video)"/"(audio)" suffix style).
pub fn parse_dshow_devices(text: &str) -> (Vec<String>, Vec<String>) {
    let (mut video, mut audio) = (Vec::new(), Vec::new());
    let mut section = 0; // 0 unknown, 1 video, 2 audio
    for line in text.lines() {
        let body = line.split_once(']').map(|(_, b)| b).unwrap_or(line).trim();
        if body.starts_with("DirectShow video devices") {
            section = 1;
            continue;
        }
        if body.starts_with("DirectShow audio devices") {
            section = 2;
            continue;
        }
        if body.starts_with("Alternative name") || !body.starts_with('"') {
            continue;
        }
        let Some(end) = body[1..].find('"') else { continue };
        let name = body[1..1 + end].to_string();
        let rest = &body[2 + end..];
        let kind = if rest.contains("(video)") {
            1
        } else if rest.contains("(audio)") {
            2
        } else {
            section
        };
        match kind {
            1 => video.push(name),
            2 => audio.push(name),
            _ => {}
        }
    }
    (video, audio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dshow_new_and_old() {
        let new = r#"[dshow @ 000001f8] "HD Pro Webcam C920" (video)
[dshow @ 000001f8]   Alternative name "@device_pnp_\\?\usb#vid_046d"
[dshow @ 000001f8] "Elgato Game Capture HD" (video)
[dshow @ 000001f8] "Microphone (HD Pro Webcam C920)" (audio)
[dshow @ 000001f8]   Alternative name "@device_cm_{33D9A762}""#;
        let (v, a) = parse_dshow_devices(new);
        assert_eq!(v, vec!["HD Pro Webcam C920", "Elgato Game Capture HD"]);
        assert_eq!(a, vec!["Microphone (HD Pro Webcam C920)"]);

        let old = "[dshow @ 0x1] DirectShow video devices (some may be both video and audio devices)\n[dshow @ 0x1]  \"USB Camera\"\n[dshow @ 0x1] DirectShow audio devices\n[dshow @ 0x1]  \"Line In\"\n";
        let (v, a) = parse_dshow_devices(old);
        assert_eq!(v, vec!["USB Camera"]);
        assert_eq!(a, vec!["Line In"]);
    }

    #[test]
    fn parses_output_size() {
        let l = "  Stream #0:0: Video: rawvideo (BGRA / 0x41524742), bgra(pc, gbr/unknown/unknown, progressive), 1280x720 [SAR 1:1 DAR 16:9], q=2-31, 884736 kb/s, 30 fps";
        assert_eq!(parse_output_size(l), Some((1280, 720)));
        assert_eq!(parse_output_size("Stream #0:0: Video: h264, yuv420p, 1920x1080"), None);
    }
}
