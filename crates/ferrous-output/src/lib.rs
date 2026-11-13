//! Recording and streaming outputs.
//!
//! Each output is one FFmpeg process: composed BGRA frames go in on stdin,
//! mixed 48 kHz float audio goes in over a loopback TCP socket, and FFmpeg
//! scales, encodes and muxes to a file or a live ingest URL.

pub mod convert;
mod stats;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use ferrous_core::{AudioCodec, ConversionMode, EncoderSettings, RecordingFormat, VideoFrame, VideoSettings};
use ferrous_encode::{args, ffmpeg};
use parking_lot::Mutex;

pub use stats::OutputStats;

pub type AudioChunk = Arc<Vec<f32>>;

#[derive(Clone, Debug)]
pub enum Target {
    File { path: PathBuf, format: RecordingFormat },
    Stream { url: String },
    /// Rolling MPEG-TS segments in `dir`, `segment_secs` long, `count` kept.
    Replay { dir: PathBuf, segment_secs: u32, count: u32 },
}

#[derive(Clone, Debug)]
pub struct OutputConfig {
    pub ffmpeg: PathBuf,
    pub target: Target,
    pub video: VideoSettings,
    pub encoder: EncoderSettings,
    pub audio_codec: AudioCodec,
    pub audio_bitrate_kbps: u32,
    /// Mixer tracks (0-based) to include, one audio stream each. Empty = track 1.
    pub audio_tracks: Vec<usize>,
}

impl OutputConfig {
    pub fn tracks(&self) -> Vec<usize> {
        if self.audio_tracks.is_empty() {
            vec![0]
        } else {
            self.audio_tracks.clone()
        }
    }
}

/// Track indices set in a bitmask (bit 0 = track 1).
pub fn tracks_from_mask(mask: u32) -> Vec<usize> {
    (0..6).filter(|t| mask & (1 << t) != 0).collect()
}

/// Picks the container for a live URL.
pub fn stream_muxer(url: &str) -> &'static str {
    let u = url.to_ascii_lowercase();
    if u.starts_with("srt://") || u.starts_with("udp://") {
        "mpegts"
    } else if u.starts_with("http://") || u.starts_with("https://") {
        "whip"
    } else {
        "flv"
    }
}

/// Joins a server and stream key the way services expect.
pub fn stream_url(server: &str, key: &str) -> String {
    let server = server.trim();
    let key = key.trim();
    if key.is_empty() {
        server.to_string()
    } else if server.to_ascii_lowercase().starts_with("srt://") {
        let sep = if server.contains('?') { '&' } else { '?' };
        format!("{server}{sep}streamid={key}")
    } else {
        format!("{}/{}", server.trim_end_matches('/'), key)
    }
}

pub fn recording_path(folder: &Path, filename_format: &str, format: RecordingFormat) -> PathBuf {
    let stem = chrono::Local::now().format(filename_format).to_string();
    let mut path = folder.join(format!("{stem}.{}", format.extension()));
    let mut n = 2;
    while path.exists() {
        path = folder.join(format!("{stem} ({n}).{}", format.extension()));
        n += 1;
    }
    path
}

/// YUV layout an encoder takes: NV12 for hardware, I420 for software.
pub fn encoder_layout(es: &ferrous_core::EncoderSettings) -> convert::YuvLayout {
    use convert::YuvLayout as L;
    match args::encoder_pix_fmt_for(es) {
        "nv12" => L::Nv12,
        "p010le" => L::P010,
        "yuv420p10le" => L::I420P10,
        _ => L::I420,
    }
}

/// Set by the engine while the GPU compositor is in use: it can then
/// deliver NV12 itself, which makes converting in Ferrous the cheap choice.
static GPU_NV12: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_gpu_nv12(on: bool) {
    GPU_NV12.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// [`resolved_conversion`] for a particular encoder: Ferrous also wins on
/// "Auto" when the GPU hands over finished NV12 (no CPU work left to save
/// by letting FFmpeg do it).
pub fn effective_conversion(v: &VideoSettings, es: &ferrous_core::EncoderSettings) -> ConversionMode {
    // FFmpeg's own scaler can't do the SDR-to-HDR mapping.
    if args::hdr_active(es) {
        return ConversionMode::Ferrous;
    }
    match resolved_conversion(v) {
        ConversionMode::Ffmpeg if v.conversion == ConversionMode::Auto && GPU_NV12.load(std::sync::atomic::Ordering::Relaxed) && gpu_nv12_applies(v, es) => {
            ConversionMode::Ferrous
        }
        m => m,
    }
}

/// Where conversion happens for these settings on this machine.
pub fn resolved_conversion(v: &VideoSettings) -> ConversionMode {
    match v.conversion {
        ConversionMode::Auto => {
            let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
            let bgra_rate = v.base_width as u64 * v.base_height as u64 * v.fps as u64;
            if bgra_rate > 1920 * 1080 * 60 || cores >= 8 { ConversionMode::Ferrous } else { ConversionMode::Ffmpeg }
        }
        m => m,
    }
}

/// Whether the GPU compositor can hand this encoder its input directly:
/// NV12 at the canvas size, converted in Ferrous.
pub fn gpu_nv12_applies(v: &VideoSettings, es: &ferrous_core::EncoderSettings) -> bool {
    v.renderer != ferrous_core::RendererMode::Cpu
        && v.conversion != ConversionMode::Ffmpeg
        && encoder_layout(es) == convert::YuvLayout::Nv12
        && encoder_input_size(v) == (v.base_width, v.base_height)
        && v.base_width.is_multiple_of(2)
        && v.base_height.is_multiple_of(2)
}

/// Size of the frames Ferrous converts for the encoder (4:2:0 needs even sizes).
pub fn encoder_input_size(v: &VideoSettings) -> (u32, u32) {
    ((v.output_width & !1).max(2), (v.output_height & !1).max(2))
}

/// Full FFmpeg argument list (without the program name).
pub fn build_args(cfg: &OutputConfig, audio_ports: &[u16]) -> Vec<String> {
    let v = &cfg.video;
    let mut a: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));

    push(&["-hide_banner", "-loglevel", "warning", "-stats", "-stats_period", "1", "-nostdin", "-y"]);
    let hw = args::hw_upload(&cfg.encoder.encoder);
    if let Some((global, _)) = &hw {
        a.extend(global.iter().cloned());
    }
    let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
    // Input 0: raw frames, either the BGRA canvas or, converted by Ferrous,
    // YUV at the output size.
    let fps = v.fps.to_string();
    let converted = effective_conversion(v, &cfg.encoder) == ConversionMode::Ferrous;
    if converted {
        let (w, h) = encoder_input_size(v);
        let size = format!("{w}x{h}");
        push(&["-thread_queue_size", "1024", "-f", "rawvideo", "-pix_fmt", encoder_layout(&cfg.encoder).ffmpeg_pix_fmt()]);
        if args::hdr_active(&cfg.encoder) {
            push(&["-color_range", "tv", "-colorspace", "bt2020nc", "-color_primaries", "bt2020", "-color_trc", "smpte2084"]);
        } else {
            push(&["-color_range", "tv", "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709"]);
        }
        push(&["-video_size", &size, "-framerate", &fps, "-i", "pipe:0"]);
    } else {
        let size = format!("{}x{}", v.base_width, v.base_height);
        push(&["-thread_queue_size", "1024", "-f", "rawvideo", "-pix_fmt", "bgra"]);
        push(&["-video_size", &size, "-framerate", &fps, "-i", "pipe:0"]);
    }
    // Inputs 1..: one mixed audio track each.
    for port in audio_ports {
        let audio_url = format!("tcp://127.0.0.1:{port}?listen=1");
        push(&["-thread_queue_size", "1024", "-f", "f32le", "-ar", "48000", "-ac", "2", "-i", &audio_url]);
    }
    push(&["-map", "0:v"]);
    for i in 0..audio_ports.len() {
        push(&["-map", &format!("{}:a", i + 1)]);
    }

    let upload = hw.as_ref().map(|(_, f)| *f);
    if converted {
        if let Some(f) = upload {
            push(&["-vf", f]);
        }
    } else {
        let downscale = v.output_width < v.base_width || v.output_height < v.base_height;
        let mut vf = format!(
            "scale={}:{}:flags={}:out_color_matrix=bt709:out_range=tv,format={}",
            v.output_width,
            v.output_height,
            if downscale { "lanczos" } else { "bicubic" },
            args::encoder_pix_fmt(&cfg.encoder.encoder)
        );
        if let Some(f) = upload {
            vf.push(',');
            vf.push_str(f);
        }
        push(&["-vf", &vf]);
    }
    push(&["-fps_mode", "cfr"]);
    a.extend(args::video_args(&cfg.encoder, v.fps));

    match &cfg.target {
        Target::File { path, format } => {
            a.extend(args::audio_args(cfg.audio_codec, cfg.audio_bitrate_kbps));
            let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
            push(&["-max_muxing_queue_size", "4096"]);
            match format {
                RecordingFormat::Mp4 | RecordingFormat::Mov => push(&["-movflags", "+faststart"]),
                RecordingFormat::FragmentedMp4 => {
                    push(&["-movflags", "+frag_keyframe+empty_moov+default_base_moof"])
                }
                _ => {}
            }
            for (i, t) in cfg.tracks().iter().enumerate() {
                push(&[&format!("-metadata:s:a:{i}"), &format!("title=Track {}", t + 1)]);
            }
            push(&["-f", format.ffmpeg_muxer()]);
            a.push(path.to_string_lossy().into_owned());
        }
        Target::Replay { dir, segment_secs, count } => {
            a.extend(args::audio_args(AudioCodec::Aac, cfg.audio_bitrate_kbps));
            let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
            // Keyframe on every segment boundary so clips can start anywhere.
            let kf = format!("expr:gte(t,n_forced*{segment_secs})");
            push(&["-force_key_frames", &kf, "-max_muxing_queue_size", "4096"]);
            push(&["-f", "segment", "-segment_time", &segment_secs.to_string()]);
            push(&["-segment_wrap", &count.to_string(), "-reset_timestamps", "1", "-segment_format", "mpegts"]);
            a.push(dir.join("seg%03d.ts").to_string_lossy().into_owned());
        }
        Target::Stream { url } => {
            let mux = stream_muxer(url);
            // Live protocols: FLV/RTMP needs AAC, WebRTC (WHIP) needs Opus.
            let codec = if mux == "whip" { AudioCodec::Opus } else { AudioCodec::Aac };
            a.extend(args::audio_args(codec, cfg.audio_bitrate_kbps));
            let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
            push(&["-max_muxing_queue_size", "4096"]);
            if mux == "flv" {
                push(&["-flvflags", "no_duration_filesize"]);
            }
            push(&["-f", mux, url]);
        }
    }
    a
}

/// A running output. Dropping it stops it cleanly.
pub struct Output {
    video_tx: Option<Sender<VideoFrame>>,
    owed_frames: Arc<AtomicU64>,
    pub stats: Arc<OutputStats>,
    child: Arc<Mutex<Child>>,
    audio_stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub target: Target,
    started: Instant,
}

impl Output {
    /// `audio` holds one receiver per entry in `cfg.tracks()`.
    pub fn start(cfg: OutputConfig, audio: Vec<Receiver<AudioChunk>>) -> anyhow::Result<Self> {
        let mut ports = Vec::new();
        for _ in 0..audio.len() {
            let l = TcpListener::bind("127.0.0.1:0")?;
            ports.push(l.local_addr()?.port());
        }
        let argv = build_args(&cfg, &ports);
        log::info!("ffmpeg {}", redact(&argv).join(" "));

        let mut child = ffmpeg::command(&cfg.ffmpeg)
            .args(&argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("couldn't launch FFmpeg ({}): {e}", cfg.ffmpeg.display()))?;
        let mut stdin = child.stdin.take().expect("stdin piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let child = Arc::new(Mutex::new(child));

        let stats = Arc::new(OutputStats::default());
        stats.running.store(true, Ordering::Relaxed);
        let owed = Arc::new(AtomicU64::new(0));
        let audio_stop = Arc::new(AtomicBool::new(false));
        let frame_bytes = (cfg.video.base_width * cfg.video.base_height * 4) as usize;
        let converter = (effective_conversion(&cfg.video, &cfg.encoder) == ConversionMode::Ferrous).then(|| {
            convert::Converter::with_white(
                (cfg.video.base_width, cfg.video.base_height),
                encoder_input_size(&cfg.video),
                encoder_layout(&cfg.encoder),
                cfg.encoder.hdr_white_nits as f32,
            )
        });
        // A few frames of buffer; more just adds latency and memory.
        let (video_tx, video_rx) = crossbeam_channel::bounded::<VideoFrame>(8);

        let video_thread = {
            let (stats, owed) = (stats.clone(), owed.clone());
            thread::Builder::new().name("output-video".into()).spawn(move || {
                let mut yuv = Vec::new();
                let mut converted: Option<Arc<Vec<u8>>> = None;
                while let Ok(frame) = video_rx.recv() {
                    if frame.byte_len() != frame_bytes {
                        log::error!("frame size mismatch: {frame:?}");
                        continue;
                    }
                    let bytes: &[u8] = match &converter {
                        Some(c) if c.ready_made(&frame).is_some() => c.ready_made(&frame).unwrap_or_default(),
                        Some(c) => {
                            // A static scene hands over the same buffer each
                            // tick; its YUV from last time is still valid.
                            if !converted.as_ref().is_some_and(|p| Arc::ptr_eq(p, &frame.data)) {
                                c.convert(&frame.data, &mut yuv);
                                converted = Some(frame.data.clone());
                            }
                            &yuv
                        }
                        None => &frame.data,
                    };
                    if stdin.write_all(bytes).is_err() {
                        stats.fail("FFmpeg stopped accepting video");
                        break;
                    }
                    stats.frames_written.fetch_add(1, Ordering::Relaxed);
                    stats.bytes_piped.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    // Repay frames skipped while the encoder was backed up, so
                    // video duration keeps tracking wall-clock (and audio).
                    while video_rx.is_empty() && owed.load(Ordering::Relaxed) > 0 {
                        owed.fetch_sub(1, Ordering::Relaxed);
                        if stdin.write_all(bytes).is_err() {
                            break;
                        }
                    }
                }
                drop(stdin); // EOF -> FFmpeg finalises the file
            })?
        };

        // One writer per track: FFmpeg probes input N (reading data) before it
        // listens on input N+1, so tracks must connect and send independently.
        let mut audio_threads = Vec::new();
        for (port, rx) in ports.iter().copied().zip(audio) {
            let (stop, stats) = (audio_stop.clone(), stats.clone());
            audio_threads.push(thread::Builder::new().name("output-audio".into()).spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut sock = loop {
                    match TcpStream::connect(("127.0.0.1", port)) {
                        Ok(s) => break s,
                        Err(_) if Instant::now() < deadline && !stop.load(Ordering::Relaxed) => {
                            thread::sleep(Duration::from_millis(20))
                        }
                        Err(e) => {
                            if !stop.load(Ordering::Relaxed) {
                                stats.fail(&format!("FFmpeg audio input never opened: {e}"));
                            }
                            return;
                        }
                    }
                };
                let _ = sock.set_nodelay(true);
                let mut bytes = Vec::with_capacity(480 * 2 * 4);
                while !stop.load(Ordering::Relaxed) {
                    let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) else { continue };
                    bytes.clear();
                    for s in chunk.iter() {
                        bytes.extend_from_slice(&s.to_le_bytes());
                    }
                    if sock.write_all(&bytes).is_err() {
                        break;
                    }
                }
                let _ = sock.shutdown(Shutdown::Both);
            })?);
        }

        let log_thread = {
            let stats = stats.clone();
            thread::Builder::new().name("output-log".into()).spawn(move || {
                // FFmpeg separates progress updates with '\r', log lines with '\n'.
                let mut reader = BufReader::new(stderr);
                let mut buf = Vec::new();
                let mut byte = [0u8; 1];
                loop {
                    match reader.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    if byte[0] == b'\r' || byte[0] == b'\n' {
                        if !buf.is_empty() {
                            stats.ingest_line(&String::from_utf8_lossy(&buf));
                            buf.clear();
                        }
                    } else {
                        buf.push(byte[0]);
                    }
                }
                let _ = reader.fill_buf();
            })?
        };

        Ok(Self {
            video_tx: Some(video_tx),
            owed_frames: owed,
            stats,
            child,
            audio_stop,
            threads: { let mut t = vec![video_thread, log_thread]; t.extend(audio_threads); t },
            target: cfg.target,
            started: Instant::now(),
        })
    }

    /// Hands a composed frame to the encoder without blocking the render loop.
    pub fn push_frame(&self, frame: &VideoFrame) {
        let Some(tx) = &self.video_tx else { return };
        match tx.try_send(frame.clone()) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
                self.owed_frames.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// False once FFmpeg has exited or failed.
    pub fn is_running(&self) -> bool {
        if !self.stats.running.load(Ordering::Relaxed) {
            return false;
        }
        match self.child.lock().try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    self.stats.fail(&format!("FFmpeg exited with {status}"));
                } else {
                    self.stats.running.store(false, Ordering::Relaxed);
                }
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.lock().id()
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Stops gracefully: closes both inputs so FFmpeg flushes and finalises,
    /// then waits up to 15 s before killing it.
    pub fn stop(mut self) -> anyhow::Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> anyhow::Result<()> {
        self.video_tx.take();
        self.audio_stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let mut child = self.child.lock();
            match child.try_wait()? {
                Some(status) => {
                    self.stats.running.store(false, Ordering::Relaxed);
                    if status.success() {
                        return Ok(());
                    }
                    anyhow::bail!("FFmpeg exited with {status}: {}", self.stats.last_error());
                }
                None if Instant::now() > deadline => {
                    let _ = child.kill();
                    self.stats.running.store(false, Ordering::Relaxed);
                    anyhow::bail!("FFmpeg didn't finish in time and was killed");
                }
                None => {}
            }
            drop(child);
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        if !self.threads.is_empty() {
            if let Err(e) = self.shutdown() {
                log::warn!("output stop: {e}");
            }
        }
    }
}

/// Concatenates the newest replay segments covering `seconds` into `out`.
pub fn save_replay(ffmpeg_path: &Path, dir: &Path, seconds: u32, segment_secs: u32, out: &Path, format: RecordingFormat) -> anyhow::Result<()> {
    let mut segs: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "ts"))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    if segs.is_empty() {
        anyhow::bail!("the replay buffer is empty");
    }
    segs.sort();
    let want = (seconds.div_ceil(segment_secs.max(1)) + 1) as usize;
    let chosen = &segs[segs.len().saturating_sub(want)..];
    let list = dir.join("concat.txt");
    let mut text = String::new();
    for (_, p) in chosen {
        text.push_str(&format!("file '{}'\n", p.display().to_string().replace('\'', "'\\''")));
    }
    std::fs::write(&list, text)?;
    let mut cmd = ffmpeg::command(ffmpeg_path);
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"]).arg(&list);
    cmd.args(["-map", "0", "-c", "copy"]);
    if matches!(format, RecordingFormat::Mp4 | RecordingFormat::FragmentedMp4 | RecordingFormat::Mov) {
        cmd.args(["-bsf:a", "aac_adtstoasc", "-movflags", "+faststart"]);
    }
    cmd.args(["-f", format.ffmpeg_muxer()]).arg(out);
    let o = cmd.stdin(Stdio::null()).output()?;
    if !o.status.success() {
        anyhow::bail!("saving replay failed: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

/// Hides stream keys in logs.
fn redact(argv: &[String]) -> Vec<String> {
    argv.iter()
        .map(|a| {
            let lower = a.to_ascii_lowercase();
            if lower.starts_with("rtmp") || lower.starts_with("srt://") || lower.starts_with("http") {
                match a.rfind('/') {
                    Some(i) if !lower.starts_with("srt://") => format!("{}/****", &a[..i]),
                    _ => a.split('?').next().unwrap_or("").to_string() + "?****",
                }
            } else {
                a.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(stream_url("rtmp://live.twitch.tv/app/", "abc"), "rtmp://live.twitch.tv/app/abc");
        assert_eq!(stream_url("srt://host:9000", "k"), "srt://host:9000?streamid=k");
        assert_eq!(stream_muxer("rtmps://a.rtmps.youtube.com:443/live2/x"), "flv");
        assert_eq!(stream_muxer("srt://x:1"), "mpegts");
        assert_eq!(stream_muxer("https://whip.example/endpoint"), "whip");
    }

    fn cfg(encoder: &str, conversion: ConversionMode) -> OutputConfig {
        OutputConfig {
            ffmpeg: "ffmpeg".into(),
            target: Target::File { path: "out.mkv".into(), format: RecordingFormat::Mkv },
            video: VideoSettings { output_width: 1280, output_height: 720, conversion, ..Default::default() },
            encoder: EncoderSettings { encoder: encoder.into(), ..EncoderSettings::recording_default() },
            audio_codec: AudioCodec::Aac,
            audio_bitrate_kbps: 192,
            audio_tracks: vec![],
        }
    }

    #[test]
    fn args_ffmpeg_conversion_scales_bgra() {
        let j = build_args(&cfg("libx264", ConversionMode::Ffmpeg), &[5000]).join(" ");
        assert!(j.contains("-pix_fmt bgra -video_size 1920x1080 -framerate 60 -i pipe:0"));
        assert!(j.contains("tcp://127.0.0.1:5000?listen=1"));
        assert!(j.contains("scale=1280:720:flags=lanczos"));
        assert!(j.ends_with("-f matroska out.mkv"));
    }

    #[test]
    fn args_ferrous_conversion_pipes_yuv_at_output_size() {
        let j = build_args(&cfg("libx264", ConversionMode::Ferrous), &[5000]).join(" ");
        assert!(j.contains("-pix_fmt yuv420p -color_range tv -colorspace bt709"), "{j}");
        assert!(j.contains("-video_size 1280x720 -framerate 60 -i pipe:0"), "{j}");
        assert!(!j.contains("scale="), "{j}");
        let j = build_args(&cfg("h264_nvenc", ConversionMode::Ferrous), &[5000]).join(" ");
        assert!(j.contains("-pix_fmt nv12"), "{j}");
    }

    #[test]
    fn vendor_neutral_gpu_encoders_upload_to_the_device() {
        let j = build_args(&cfg("hevc_d3d12va", ConversionMode::Ferrous), &[5000]).join(" ");
        assert!(j.contains("-y -init_hw_device d3d12va=ember -filter_hw_device ember -thread_queue_size"), "{j}");
        assert!(j.contains("-vf hwupload"), "{j}");
        assert!(j.contains("-rc_mode") && j.contains("-c:v hevc_d3d12va"), "{j}");
        let j = build_args(&cfg("av1_vulkan", ConversionMode::Ffmpeg), &[5000]).join(" ");
        assert!(j.contains("-init_hw_device vulkan=ember"), "{j}");
        assert!(j.contains("format=nv12,hwupload"), "{j}");
    }

    /// End to end through real FFmpeg: an Ferrous-converted recording decodes
    /// to the same picture as one FFmpeg converted itself.
    #[test]
    fn ferrous_conversion_matches_ffmpeg_conversion() {
        let ffmpeg = std::path::PathBuf::from("ffmpeg");
        if std::process::Command::new(&ffmpeg).arg("-version").output().is_err() {
            eprintln!("no ffmpeg; skipping");
            return;
        }
        let dir = std::env::temp_dir().join("ferrous-convert-e2e");
        std::fs::create_dir_all(&dir).unwrap();
        // A colourful gradient with fine detail, 640x360 -> 320x180.
        let (w, h) = (640u32, 360u32);
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let r = (x * 255 / w) as u8;
                let g = (y * 255 / h) as u8;
                let b = if (x / 8 + y / 8) % 2 == 0 { 220 } else { 40 };
                data.extend_from_slice(&[b, g, r, 255]);
            }
        }
        let frame = VideoFrame::new(w, h, data);
        let decode = |mode: ConversionMode, name: &str| -> Vec<u8> {
            let path = dir.join(name);
            let mut c = cfg("libx264", mode);
            c.ffmpeg = ffmpeg.clone();
            c.video = VideoSettings { base_width: w, base_height: h, output_width: 320, output_height: 180, fps: 30, conversion: mode, ..Default::default() };
            c.encoder.rate_control = ferrous_core::RateControl::Quality;
            c.encoder.quality = 0; // lossless-ish
            c.target = Target::File { path: path.clone(), format: RecordingFormat::Mkv };
            // FFmpeg waits for audio before reading video, so feed silence.
            let (tx, rx) = crossbeam_channel::unbounded();
            let out = Output::start(c, vec![rx]).unwrap();
            for _ in 0..15 {
                for _ in 0..4 {
                    let _ = tx.send(Arc::new(vec![0f32; 480 * 2]));
                }
                out.push_frame(&frame);
                std::thread::sleep(std::time::Duration::from_millis(33));
            }
            out.stop().unwrap();
            let o = std::process::Command::new(&ffmpeg)
                .args(["-v", "error", "-i"]).arg(&path)
                .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
                .output().unwrap();
            assert_eq!(o.stdout.len(), 320 * 180 * 3, "{}", String::from_utf8_lossy(&o.stderr));
            o.stdout
        };
        let a = decode(ConversionMode::Ferrous, "ember.mkv");
        let b = decode(ConversionMode::Ffmpeg, "ffmpeg.mkv");
        let mse = a.iter().zip(&b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / a.len() as f64;
        let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
        println!("Ferrous vs FFmpeg conversion PSNR: {psnr:.1} dB");
        assert!(psnr > 30.0, "PSNR {psnr:.1} dB");
    }

    /// Throughput of the converter on a 1080p and a 4K canvas.
    #[test]
    fn converter_throughput() {
        for (w, h, ow, oh) in [(1920u32, 1080u32, 1920u32, 1080u32), (3840, 2160, 3840, 2160), (3840, 2160, 1920, 1080)] {
            let src = vec![128u8; (w * h * 4) as usize];
            let c = convert::Converter::new((w, h), (ow, oh), convert::YuvLayout::Nv12);
            let mut out = Vec::new();
            c.convert(&src, &mut out);
            let n = 10;
            let t = Instant::now();
            for _ in 0..n {
                c.convert(&src, &mut out);
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
            println!("{w}x{h} -> {ow}x{oh} NV12: {ms:.2} ms/frame, pipe {:.1} MB/frame (BGRA would be {:.1})",
                out.len() as f64 / 1e6, (w * h * 4) as f64 / 1e6);
        }
    }

    #[test]
    fn redacts_keys() {
        let r = redact(&["rtmp://live.twitch.tv/app/live_123_secret".into()]);
        assert_eq!(r[0], "rtmp://live.twitch.tv/app/****");
    }
}
