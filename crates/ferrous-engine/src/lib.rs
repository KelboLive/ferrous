//! The engine: a fixed-rate render clock that samples sources, composes the
//! current scene, feeds the preview and every active output, and supervises
//! outputs (stream auto-reconnect, error reporting).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use ferrous_audio::{AudioEngine, Mixer};
use ferrous_capture::Source;
use ferrous_core::{SceneCollection, Settings, SourceConfig, VideoFrame};
use ferrous_output::{Output, OutputConfig, Target};
use ferrous_render::{Layer, Renderer};
use parking_lot::Mutex;

pub use ferrous_output::OutputStats;

/// Per-source cost, smoothed. CPU is measured; GPU is the compositor pass's
/// measured GPU time shared by covered area (estimate).
#[derive(Clone, Debug, Default)]
pub struct SourcePerf {
    /// Grabbing/decoding the frame plus filters, ms per output frame.
    pub source_ms: f32,
    /// Compositing (scaling, blending), ms per output frame.
    pub composite_ms: f32,
    pub gpu_ms: Option<f32>,
    /// Helper processes (FFmpeg, browser) whose CPU/GPU belongs to this source.
    pub pids: Vec<u32>,
}

/// What the UI wants from the preview: whether to make one at all, how
/// often (every rendered frame, or a lower cap), and how wide (the size it
/// is shown at, so a small dock costs little and a large one stays sharp).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PreviewConfig {
    pub enabled: bool,
    /// 0 = every rendered frame.
    pub max_fps: u32,
    /// Pixel width for the program/studio preview, and for the vertical one.
    pub width: u32,
    pub vertical_width: u32,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self { enabled: true, max_fps: 0, width: 1280, vertical_width: 720 }
    }
}

/// Timing of one rendered frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameSample {
    /// Time since the previous frame started (ideal: the frame period).
    pub interval_ms: f32,
    /// Time spent rendering this frame.
    pub work_ms: f32,
    /// Where it went: see [`FRAME_STAGES`].
    pub stages_ms: [f32; 6],
}

/// Names of [`FrameSample::stages_ms`].
pub const FRAME_STAGES: [&str; 6] = ["sources + compose", "vertical canvas", "outputs", "Ferrous Link", "previews", "housekeeping"];

/// Stopwatch splitting a frame into stages.
struct Laps {
    t: Instant,
    ms: [f32; 6],
}

impl Laps {
    fn new() -> Self {
        Self { t: Instant::now(), ms: [0.0; 6] }
    }
    fn lap(&mut self, stage: usize) {
        let now = Instant::now();
        self.ms[stage] += (now - self.t).as_secs_f32() * 1000.0;
        self.t = now;
    }
}

pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub serial: u64,
}

#[derive(Clone, Debug)]
pub enum EngineEvent {
    Info(String),
    Error(String),
}

#[derive(Clone, Debug, Default)]
pub struct OutputStatus {
    pub active: bool,
    pub reconnecting: bool,
    pub elapsed: Duration,
    pub bitrate_kbps: f32,
    pub speed: f32,
    pub frames_written: u64,
    pub frames_skipped: u64,
}

#[derive(Clone, Debug, Default)]
pub struct EngineStatus {
    pub render_ms: f32,
    pub fps: f32,
    /// Frames the compositor itself missed its deadline on.
    pub lagged_frames: u64,
    pub recording: OutputStatus,
    pub streaming: OutputStatus,
    pub recording_path: Option<PathBuf>,
    /// "GPU: <adapter>" or "CPU".
    pub renderer: String,
    /// GPU time of the main compositing pass (GPU compositor with timestamp support).
    pub gpu_ms: Option<f32>,
    /// Worst frame time in the last second.
    pub max_frame_ms: f32,
}

struct CachedSource {
    config: SourceConfig,
    source: Option<Box<dyn Source>>,
    last_attempt: Instant,
    error: Option<String>,
    size: (u32, u32),
    /// The source's own mixer channel, kept alive with the source.
    audio: Option<Arc<ferrous_audio::AudioChannel>>,
    filters: ferrous_render::filters::VideoFilterChain,
    filter_error: Option<String>,
    perf: SourcePerf,
    /// This frame's accumulated cost (source, composite, gpu).
    acc: (f32, f32, f32, bool),
    /// Since when the source hasn't been needed (not on screen).
    idle_since: Option<Instant>,
}

/// How long an unseen source keeps running, so switching back and forth
/// between scenes (or toggling an item) stays instant.
const IDLE_GRACE: Duration = Duration::from_secs(10);

/// Items whose sources should be running: the visible items of every scene
/// on screen (program, studio preview, a transition's outgoing scene, and
/// whatever Ferrous Link outputs send), through nested scenes.
fn shown_items(scenes: &SceneCollection, program: usize, studio: Option<usize>, from: Option<&ferrous_core::Scene>, links: &[ferrous_core::LinkSourceTarget]) -> HashSet<u64> {
    fn walk<'a>(sc: &'a ferrous_core::Scene, all: &'a SceneCollection, seen: &mut Vec<&'a str>, out: &mut HashSet<u64>) {
        if seen.contains(&sc.name.as_str()) {
            return;
        }
        seen.push(&sc.name);
        for item in sc.items.iter().filter(|i| i.visible) {
            out.insert(item.id);
            match &item.source {
                SourceConfig::Scene { name } => {
                    if let Some(inner) = all.scenes.iter().find(|s| s.name == *name) {
                        walk(inner, all, seen, out);
                    }
                }
                // A clone keeps its original running.
                SourceConfig::Clone { source } => {
                    out.insert(*source);
                }
                _ => {}
            }
        }
    }
    let mut out = HashSet::new();
    let mut roots: Vec<&ferrous_core::Scene> = Vec::new();
    roots.extend(scenes.scenes.get(program));
    roots.extend(studio.and_then(|i| scenes.scenes.get(i)));
    roots.extend(from);
    for l in links {
        match l {
            ferrous_core::LinkSourceTarget::Scene { name } => roots.extend(scenes.scenes.iter().find(|s| s.name == *name)),
            ferrous_core::LinkSourceTarget::Source { name } => {
                out.extend(scenes.scenes.iter().flat_map(|s| &s.items).filter(|i| i.name == *name).map(|i| i.id));
            }
            ferrous_core::LinkSourceTarget::Program => {}
        }
    }
    for sc in roots {
        walk(sc, scenes, &mut Vec::new(), &mut out);
    }
    out
}

struct StreamRetry {
    config: OutputConfig,
    attempts: u32,
    at: Instant,
}

struct ExtraStream {
    name: String,
    vertical: bool,
    cfg: OutputConfig,
    output: Option<Output>,
    attempts: u32,
    retry_at: Option<Instant>,
}

/// The virtual camera while it's on: frames go to a worker that converts
/// them to the camera's NV12 format and writes the shared region.
struct VirtualCamera {
    tx: crossbeam_channel::Sender<VideoFrame>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    status: Arc<VcamStats>,
    last_sent: Instant,
    #[cfg(windows)]
    camera: Option<ferrous_vcam::win::Camera>,
}

/// Virtual camera state for the UI.
#[derive(Default)]
pub struct VcamStats {
    /// A camera app has the camera open (the shared region is connected).
    pub connected: std::sync::atomic::AtomicBool,
    pub frames: std::sync::atomic::AtomicU64,
}

impl Drop for VirtualCamera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        #[cfg(windows)]
        if let Some(c) = self.camera.take() {
            c.stop();
        }
    }
}

#[derive(Default)]
struct Outputs {
    vcam: Option<VirtualCamera>,
    extras: Vec<ExtraStream>,
    replay_vertical: Option<(Output, PathBuf)>,
    replay: Option<(Output, PathBuf)>,
    record: Option<Output>,
    record_path: Option<PathBuf>,
    stream: Option<Output>,
    stream_retry: Option<StreamRetry>,
}

/// A source's (size, last error) for the UI.
type SourceInfo = ((u32, u32), Option<String>);

struct Shared {
    /// Replaced (not mutated) on change, so the render loop takes a
    /// pointer copy each frame instead of deep-cloning every scene.
    scenes: Mutex<Arc<SceneCollection>>,
    preview: Mutex<Option<Arc<PreviewFrame>>>,
    source_info: Mutex<HashMap<u64, SourceInfo>>,
    outputs: Mutex<Outputs>,
    status: Mutex<EngineStatus>,
    /// Last started stream, for auto-reconnect. `None` once the user stops.
    stream_config: Mutex<Option<OutputConfig>>,
    /// Ferrous Link outputs.
    links: Mutex<Vec<ActiveLink>>,
    perf: Mutex<HashMap<u64, SourcePerf>>,
    sysmon: ferrous_sysmon::SystemMonitor,
    /// Studio mode preview scene index, and its rendered frame.
    studio_scene: Mutex<Option<usize>>,
    studio_preview: Mutex<Option<Arc<PreviewFrame>>>,
    preview_cfg: Mutex<PreviewConfig>,
    /// The last few seconds of frame timings.
    frame_times: Mutex<std::collections::VecDeque<FrameSample>>,
    /// Called after a new preview is published (the UI wakes on it rather
    /// than polling at the preview rate).
    frame_notify: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    vertical_preview: Mutex<Option<Arc<PreviewFrame>>>,
    /// Multiview: while on, every scene is rendered small, one every 100 ms.
    multiview: AtomicBool,
    multiview_thumbs: Mutex<Vec<Option<Arc<PreviewFrame>>>>,
    stop: AtomicBool,
    /// Outputs waiting for the render thread to start their GPU encoder (it owns the compositor).
    encoder_requests: Mutex<Vec<EncoderRequest>>,
    events: Sender<EngineEvent>,
}

pub struct Engine {
    shared: Arc<Shared>,
    audio: Arc<AudioEngine>,
    render: Option<JoinHandle<()>>,
    events: Receiver<EngineEvent>,
    settings: Settings,
}

const MAX_RECONNECTS: u32 = 10;
const REPLAY_SEGMENT_SECS: u32 = 2;

/// A GPU encoder for the render thread to start on its compositor.
struct EncoderRequest {
    config: ferrous_render::ferrous_nvenc::Config,
    sink: ferrous_render::ferrous_nvenc::Sink,
    reply: Sender<Result<(), String>>,
}

/// Where the GPU encoder's packets go before and after the FFmpeg muxer is up.
enum SinkState {
    /// Muxer not started yet: keep packets (the first is the keyframe everything depends on).
    Waiting(Vec<ferrous_output::EncodedFrame>),
    Ready(ferrous_output::PacketSink),
    Cancelled,
}

/// The zero-copy (in-process NVENC) settings for an output, if it can use that path: an NVIDIA
/// encoder, the setting on, the GPU renderer, no HDR and no custom FFmpeg encoder options.
fn zero_copy_config(cfg: &OutputConfig) -> Option<(ferrous_render::ferrous_nvenc::Config, &'static str)> {
    use ferrous_render::ferrous_nvenc::{Codec, Config, Rate};
    let v = &cfg.video;
    let e = &cfg.encoder;
    if !v.zero_copy_encoding || v.renderer == ferrous_core::RendererMode::Cpu || ferrous_encode::args::hdr_active(e) || !e.custom_args.trim().is_empty() {
        return None;
    }
    let (codec, format) = match e.encoder.as_str() {
        "h264_nvenc" => (Codec::H264, "h264"),
        "hevc_nvenc" => (Codec::Hevc, "hevc"),
        "av1_nvenc" => (Codec::Av1, "obu"),
        _ => return None,
    };
    let fps = v.fps.max(1);
    let q = e.quality.clamp(1, 51) as u8;
    let rate = match e.rate_control {
        ferrous_core::RateControl::Cbr => Rate::Cbr(e.bitrate_kbps.max(100)),
        ferrous_core::RateControl::Vbr => Rate::Vbr(e.bitrate_kbps.max(100)),
        ferrous_core::RateControl::Quality => Rate::Quality(q),
        ferrous_core::RateControl::Cqp => Rate::Cqp(q),
    };
    let mut keyint = e.keyframe_interval_secs.max(1);
    // The replay buffer cuts on segment boundaries, so it needs a keyframe at every one.
    if let Target::Replay { segment_secs, .. } = &cfg.target {
        keyint = keyint.min((*segment_secs).max(1));
    }
    let preset = e.preset.strip_prefix('p').and_then(|p| p.parse::<u8>().ok()).filter(|p| (1..=7).contains(p)).unwrap_or(5);
    // Look-ahead spends latency for quality: recordings yes, live streams no.
    let live = matches!(cfg.target, Target::Stream { .. });
    let config = Config {
        codec,
        input: (v.base_width, v.base_height),
        output: ferrous_output::encoder_input_size(v),
        fps,
        rate,
        preset,
        keyframe_interval_frames: keyint * fps,
        lookahead: !live && matches!(rate, Rate::Quality(_) | Rate::Vbr(_)),
        multipass: true,
        bframes: e.b_frames.min(4),
    };
    Some((config, format))
}

/// Starts an output, encoding on the GPU inside Ferrous when it can (see [`zero_copy_config`]) and
/// through FFmpeg otherwise; any failure on the GPU path falls back to FFmpeg.
fn start_output(shared: &Shared, audio: &AudioEngine, cfg: OutputConfig) -> anyhow::Result<Output> {
    if let Some((config, format)) = zero_copy_config(&cfg) {
        match start_zero_copy(shared, audio, &cfg, config, format) {
            Ok(out) => return Ok(out),
            Err(e) => log::warn!("GPU encoder not used ({e}); encoding through FFmpeg"),
        }
    }
    Output::start(cfg.clone(), subscribe_all(audio, &cfg))
}

fn start_zero_copy(shared: &Shared, audio: &AudioEngine, cfg: &OutputConfig, config: ferrous_render::ferrous_nvenc::Config, format: &str) -> anyhow::Result<Output> {
    let state = Arc::new(Mutex::new(SinkState::Waiting(Vec::new())));
    let sink: ferrous_render::ferrous_nvenc::Sink = {
        let state = state.clone();
        Box::new(move |p: ferrous_render::ferrous_nvenc::Packet<'_>| {
            let owned = || ferrous_output::EncodedFrame { data: p.data.to_vec(), pts: p.pts, dts: p.dts, keyframe: p.keyframe };
            let mut st = state.lock();
            match &mut *st {
                SinkState::Waiting(early) => {
                    early.push(owned());
                    // A muxer that never comes: give up after ~10 s of video.
                    early.len() < 600
                }
                SinkState::Ready(out) => out.send(owned()),
                SinkState::Cancelled => false,
            }
        })
    };
    let (reply, rx) = crossbeam_channel::bounded(1);
    shared.encoder_requests.lock().push(EncoderRequest { config, sink, reply });
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => anyhow::bail!(e),
        Err(_) => {
            *state.lock() = SinkState::Cancelled;
            anyhow::bail!("the render thread didn't start it in time")
        }
    }
    match Output::start_encoded(cfg.clone(), subscribe_all(audio, cfg), format) {
        Ok((out, packets)) => {
            let mut st = state.lock();
            if let SinkState::Waiting(early) = std::mem::replace(&mut *st, SinkState::Cancelled) {
                for p in early {
                    if !packets.send(p) {
                        break;
                    }
                }
            }
            *st = SinkState::Ready(packets);
            log::info!("{}: encoding on the GPU inside Ferrous (zero copy)", cfg.encoder.encoder);
            Ok(out)
        }
        Err(e) => {
            *state.lock() = SinkState::Cancelled;
            Err(e)
        }
    }
}

fn subscribe_all(audio: &AudioEngine, cfg: &OutputConfig) -> Vec<crossbeam_channel::Receiver<ferrous_output::AudioChunk>> {
    cfg.tracks().into_iter().map(|t| audio.subscribe_track(t)).collect()
}

fn chrono_like_now() -> String {
    // Seconds since the epoch keep names unique without another dependency.
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    secs.to_string()
}
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

impl Engine {
    pub fn start(settings: Settings, scenes: SceneCollection) -> Self {
        // Bounded: events are tiny and drained every UI frame; if nobody reads them, the newest are dropped.
        let (tx, rx) = crossbeam_channel::bounded(1024);
        // Load plugin DLLs once per process.
        static PLUGINS: std::sync::Once = std::sync::Once::new();
        PLUGINS.call_once(|| {
            for problem in ferrous_plugins::load_dirs(&ferrous_plugins::default_dirs()) {
                let _ = tx.try_send(EngineEvent::Error(format!("Plugin not loaded: {problem}")));
            }
        });
        let shared = Arc::new(Shared {
            scenes: Mutex::new(Arc::new(scenes)),
            preview: Mutex::new(None),
            source_info: Mutex::new(HashMap::new()),
            outputs: Mutex::new(Outputs::default()),
            status: Mutex::new(EngineStatus::default()),
            stream_config: Mutex::new(None),
            links: Mutex::new(Vec::new()),
            perf: Mutex::new(HashMap::new()),
            sysmon: ferrous_sysmon::SystemMonitor::start(),
            studio_scene: Mutex::new(None),
            studio_preview: Mutex::new(None),
            preview_cfg: Mutex::new(PreviewConfig::default()),
            frame_times: Mutex::new(std::collections::VecDeque::with_capacity(FRAME_HISTORY)),
            frame_notify: Mutex::new(None),
            vertical_preview: Mutex::new(None),
            multiview: AtomicBool::new(false),
            multiview_thumbs: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            encoder_requests: Mutex::new(Vec::new()),
            events: tx,
        });
        let audio = Arc::new(AudioEngine::start(&settings.audio));
        apply_device_audio(&audio, &settings);
        sync_links(&shared, &audio, &settings);
        shared.sysmon.set_disk_path(&settings.record.folder);
        let render = Some(spawn_render(shared.clone(), audio.clone(), &settings));
        Self { shared, audio, render, events: rx, settings }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Applies new settings. Video/audio pipeline changes restart the render
    /// thread and audio devices, so they're refused while outputs are live.
    pub fn apply_settings(&mut self, settings: Settings) -> anyhow::Result<()> {
        let pipeline_changed =
            settings.video != self.settings.video || audio_devices_changed(&settings, &self.settings);
        if pipeline_changed {
            if self.is_recording() || self.is_streaming() {
                anyhow::bail!("stop recording and streaming before changing video or audio devices");
            }
            self.shared.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.render.take() {
                let _ = h.join();
            }
            self.shared.stop.store(false, Ordering::Relaxed);
            if audio_devices_changed(&settings, &self.settings) {
                self.audio = Arc::new(AudioEngine::start(&settings.audio));
            }
            self.render = Some(spawn_render(self.shared.clone(), self.audio.clone(), &settings));
        }
        apply_device_audio(&self.audio, &settings);
        if settings.links != self.settings.links || pipeline_changed {
            sync_links(&self.shared, &self.audio, &settings);
        }
        self.settings = settings;
        Ok(())
    }

    pub fn set_scenes(&self, scenes: &SceneCollection) {
        let mut s = self.shared.scenes.lock();
        if **s != *scenes {
            *s = Arc::new(scenes.clone());
        }
    }

    /// Called from the render thread whenever a new preview frame is ready.
    pub fn set_frame_notify(&self, f: Arc<dyn Fn() + Send + Sync>) {
        *self.shared.frame_notify.lock() = Some(f);
    }

    /// Timings of the most recent frames, oldest first.
    pub fn frame_times(&self) -> Vec<FrameSample> {
        self.shared.frame_times.lock().iter().copied().collect()
    }

    /// Turns the UI previews on or off and sets their rate and size.
    pub fn set_preview_config(&self, cfg: PreviewConfig) {
        *self.shared.preview_cfg.lock() = cfg;
    }

    /// Studio mode: `Some(scene)` renders that scene as the editable preview.
    pub fn set_studio_scene(&self, scene: Option<usize>) {
        *self.shared.studio_scene.lock() = scene;
    }

    /// Turns the multiview renderer on or off (every scene, small, a few
    /// times a second; it starts the sources of scenes that aren't live).
    pub fn set_multiview(&self, on: bool) {
        self.shared.multiview.store(on, Ordering::Relaxed);
    }

    /// The newest small picture of each scene, by scene index.
    pub fn multiview_thumbs(&self) -> Vec<Option<Arc<PreviewFrame>>> {
        self.shared.multiview_thumbs.lock().clone()
    }

    pub fn studio_preview(&self) -> Option<Arc<PreviewFrame>> {
        self.shared.studio_preview.lock().clone()
    }

    pub fn preview(&self) -> Option<Arc<PreviewFrame>> {
        self.shared.preview.lock().clone()
    }

    /// Native size and last error of each source, by scene item id.
    pub fn source_info(&self, id: u64) -> Option<((u32, u32), Option<String>)> {
        self.shared.source_info.lock().get(&id).cloned()
    }

    /// Running Ferrous Link outputs: name, port, live stats.
    pub fn link_stats(&self) -> Vec<(String, u16, Arc<ferrous_link::SenderStats>)> {
        self.shared.links.lock().iter().map(|l| (l.cfg.name.clone(), l.sender.port, l.sender.stats.clone())).collect()
    }

    /// Smoothed per-source cost by scene item id.
    pub fn perf(&self) -> HashMap<u64, SourcePerf> {
        self.shared.perf.lock().clone()
    }

    /// System, Ferrous and helper-process resource usage (1 Hz).
    pub fn system(&self) -> ferrous_sysmon::Snapshot {
        self.shared.sysmon.snapshot()
    }

    /// Helper process ids of running outputs: (label, pid).
    pub fn output_pids(&self) -> Vec<(String, u32)> {
        let o = self.shared.outputs.lock();
        let mut v = Vec::new();
        if let Some(s) = &o.stream {
            v.push(("Stream encoder".to_string(), s.pid()));
        }
        if let Some(r) = &o.record {
            v.push(("Recording encoder".to_string(), r.pid()));
        }
        if let Some((r, _)) = &o.replay {
            v.push(("Replay buffer encoder".to_string(), r.pid()));
        }
        if let Some((r, _)) = &o.replay_vertical {
            v.push(("Vertical replay encoder".to_string(), r.pid()));
        }
        for e in &o.extras {
            if let Some(out) = &e.output {
                v.push((format!("{} encoder", e.name), out.pid()));
            }
        }
        v
    }

    pub fn mixer(&self) -> Arc<Mixer> {
        self.audio.mixer.clone()
    }

    pub fn status(&self) -> EngineStatus {
        let mut st = self.shared.status.lock().clone();
        let outs = self.shared.outputs.lock();
        st.recording = output_status(outs.record.as_ref(), false);
        st.recording_path = outs.record_path.clone();
        st.streaming = output_status(outs.stream.as_ref(), outs.stream_retry.is_some());
        st
    }

    pub fn drain_events(&self) -> Vec<EngineEvent> {
        self.events.try_iter().collect()
    }

    pub fn is_recording(&self) -> bool {
        self.shared.outputs.lock().record.is_some()
    }

    pub fn is_streaming(&self) -> bool {
        let o = self.shared.outputs.lock();
        o.stream.is_some() || o.stream_retry.is_some() || !o.extras.is_empty()
    }

    /// Extra destinations: (name, live, reconnecting, kbps, skipped).
    pub fn destination_status(&self) -> Vec<(String, bool, bool, f32, u64)> {
        self.shared.outputs.lock().extras.iter().map(|e| {
            let live = e.output.as_ref().is_some_and(|o| o.is_running());
            let (kbps, skipped) = e.output.as_ref().map(|o| (o.stats.bitrate_kbps(), o.stats.frames_skipped.load(Ordering::Relaxed))).unwrap_or((0.0, 0));
            (e.name.clone(), live, e.retry_at.is_some(), kbps, skipped)
        }).collect()
    }

    pub fn vertical_preview(&self) -> Option<Arc<PreviewFrame>> {
        self.shared.vertical_preview.lock().clone()
    }

    fn ffmpeg(&self) -> PathBuf {
        ferrous_encode::ffmpeg::locate(&self.settings.ffmpeg_path)
    }

    pub fn start_recording(&self) -> anyhow::Result<PathBuf> {
        let s = &self.settings;
        self.shared.sysmon.set_disk_path(&s.record.folder);
        std::fs::create_dir_all(&s.record.folder)?;
        let path = ferrous_output::recording_path(&s.record.folder, &s.record.filename_format, s.record.format);
        let cfg = OutputConfig {
            ffmpeg: self.ffmpeg(),
            target: Target::File { path: path.clone(), format: s.record.format },
            video: match s.record.output_size {
                Some((w, h)) => ferrous_core::VideoSettings { output_width: w & !1, output_height: h & !1, ..s.video.clone() },
                None => s.video.clone(),
            },
            encoder: s.effective_record_encoder().clone(),
            audio_codec: s.record.audio_codec,
            audio_bitrate_kbps: s.record.audio_bitrate_kbps,
            audio_tracks: ferrous_output::tracks_from_mask(s.record.audio_tracks),
        };
        let out = start_output(&self.shared, &self.audio, cfg)?;
        let mut o = self.shared.outputs.lock();
        o.record = Some(out);
        o.record_path = Some(path.clone());
        Ok(path)
    }

    pub fn stop_recording(&self) {
        let (out, path) = {
            let mut o = self.shared.outputs.lock();
            (o.record.take(), o.record_path.take())
        };
        if let Some(out) = out {
            let events = self.shared.events.clone();
            // Finalising can take a few seconds; never block the UI on it.
            thread::spawn(move || match out.stop() {
                Ok(()) => {
                    let p = path.map(|p| p.display().to_string()).unwrap_or_default();
                    let _ = events.try_send(EngineEvent::Info(format!("Recording saved: {p}")));
                }
                Err(e) => {
                    let _ = events.try_send(EngineEvent::Error(format!("Recording failed: {e}")));
                }
            });
        }
    }

    pub fn start_streaming(&self) -> anyhow::Result<()> {
        self.start_streaming_with(&HashMap::new())
    }

    /// Starts the main stream (if a server is set) and every enabled extra
    /// destination. `session_keys` supplies server/key, by destination name,
    /// for destinations that ask each time.
    pub fn start_streaming_with(&self, session_keys: &HashMap<String, (String, String)>) -> anyhow::Result<()> {
        let s = &self.settings;
        let enabled: Vec<&ferrous_core::StreamDestination> = s.destinations.iter().filter(|d| d.enabled).collect();
        if s.stream.server.trim().is_empty() && enabled.is_empty() {
            anyhow::bail!("set a stream server in Settings > Stream first");
        }
        let mut errors = Vec::new();
        let mut extras = Vec::new();
        for d in enabled {
            let (server, key) = match session_keys.get(&d.name) {
                Some((sv, k)) => (sv.clone(), k.clone()),
                None if d.ask_each_time => {
                    errors.push(format!("{}: needs this session's server URL and stream key", d.name));
                    continue;
                }
                None => (d.server.clone(), d.key.clone()),
            };
            if server.trim().is_empty() {
                errors.push(format!("{}: no server URL", d.name));
                continue;
            }
            let vertical = d.canvas == ferrous_core::Canvas::Vertical;
            let (bw, bh) = match (vertical, s.video.vertical) {
                (true, Some(v)) => v,
                (true, None) => {
                    errors.push(format!("{}: turn on the vertical canvas in Settings > Video", d.name));
                    continue;
                }
                (false, _) => (s.video.base_width, s.video.base_height),
            };
            let (ow, oh) = if d.output_width == 0 || d.output_height == 0 { (bw, bh) } else { (d.output_width & !1, d.output_height & !1) };
            let cfg = OutputConfig {
                ffmpeg: self.ffmpeg(),
                target: Target::Stream { url: ferrous_output::stream_url(&server, &key) },
                video: ferrous_core::VideoSettings { base_width: bw, base_height: bh, output_width: ow, output_height: oh, ..s.video.clone() },
                encoder: d.encoder.clone(),
                audio_codec: ferrous_core::AudioCodec::Aac,
                audio_bitrate_kbps: d.audio_bitrate_kbps,
                audio_tracks: vec![d.audio_track],
            };
            let started = if vertical { Output::start(cfg.clone(), subscribe_all(&self.audio, &cfg)) } else { start_output(&self.shared, &self.audio, cfg.clone()) };
            match started {
                Ok(out) => extras.push(ExtraStream { name: d.name.clone(), vertical, cfg, output: Some(out), attempts: 0, retry_at: None }),
                Err(e) => errors.push(format!("{}: {e}", d.name)),
            }
        }
        if !extras.is_empty() {
            self.shared.outputs.lock().extras = extras;
        }
        for e in &errors {
            let _ = self.shared.events.try_send(EngineEvent::Error(format!("Destination not started: {e}")));
        }
        if s.stream.server.trim().is_empty() {
            return if self.is_streaming() { Ok(()) } else { Err(anyhow::anyhow!(errors.join("; "))) };
        }
        let cfg = OutputConfig {
            ffmpeg: self.ffmpeg(),
            target: Target::Stream { url: ferrous_output::stream_url(&s.stream.server, &s.stream.key) },
            video: s.video.clone(),
            encoder: s.stream.encoder.clone(),
            audio_codec: ferrous_core::AudioCodec::Aac,
            audio_bitrate_kbps: s.stream.audio_bitrate_kbps,
            audio_tracks: vec![s.stream.audio_track],
        };
        let out = start_output(&self.shared, &self.audio, cfg.clone())?;
        let mut o = self.shared.outputs.lock();
        o.stream = Some(out);
        o.stream_retry = None;
        drop(o);
        *self.shared.stream_config.lock() = Some(cfg);
        Ok(())
    }

    /// Starts the virtual camera ("Ferrous Virtual Camera", 1080p30).
    /// On Windows this registers the camera with the system (Windows 11,
    /// camera source installed); frames flow once an app opens it.
    pub fn start_virtual_camera(&self) -> anyhow::Result<()> {
        if self.shared.outputs.lock().vcam.is_some() {
            return Ok(());
        }
        #[cfg(windows)]
        let camera = Some(ferrous_vcam::win::Camera::start()?);
        let (tx, rx) = crossbeam_channel::bounded::<VideoFrame>(1);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let status = Arc::new(VcamStats::default());
        let thread = {
            let (stop, status) = (stop.clone(), status.clone());
            thread::Builder::new().name("virtual-camera".into()).spawn(move || vcam_worker(rx, stop, status))?
        };
        self.shared.outputs.lock().vcam = Some(VirtualCamera {
            tx,
            stop,
            thread: Some(thread),
            status,
            last_sent: Instant::now() - Duration::from_secs(1),
            #[cfg(windows)]
            camera,
        });
        let _ = self.shared.events.try_send(EngineEvent::Info("Virtual camera started".into()));
        Ok(())
    }

    pub fn stop_virtual_camera(&self) {
        let v = self.shared.outputs.lock().vcam.take();
        drop(v);
    }

    pub fn is_virtual_camera_active(&self) -> bool {
        self.shared.outputs.lock().vcam.is_some()
    }

    /// (connected to a camera app, frames sent) while the camera is on.
    pub fn virtual_camera_status(&self) -> Option<(bool, u64)> {
        self.shared.outputs.lock().vcam.as_ref().map(|v| (v.status.connected.load(Ordering::Relaxed), v.status.frames.load(Ordering::Relaxed)))
    }

    pub fn is_replay_active(&self) -> bool {
        self.shared.outputs.lock().replay.is_some()
    }

    /// Starts keeping the last `settings.replay.seconds` in rolling segments.
    pub fn start_replay(&self) -> anyhow::Result<()> {
        let s = &self.settings;
        let dir = ferrous_core::paths::config_dir().join(format!("replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let seg = REPLAY_SEGMENT_SECS;
        let cfg = OutputConfig {
            ffmpeg: self.ffmpeg(),
            target: Target::Replay { dir: dir.clone(), segment_secs: seg, count: s.replay.seconds.div_ceil(seg) + 3 },
            video: s.video.clone(),
            encoder: s.effective_record_encoder().clone(),
            audio_codec: ferrous_core::AudioCodec::Aac,
            audio_bitrate_kbps: s.record.audio_bitrate_kbps,
            audio_tracks: ferrous_output::tracks_from_mask(s.record.audio_tracks),
        };
        let out = start_output(&self.shared, &self.audio, cfg.clone())?;
        self.shared.outputs.lock().replay = Some((out, dir.clone()));
        if let (true, Some((vw, vh))) = (s.replay.vertical_too, s.video.vertical) {
            let vdir = dir.with_extension("vertical");
            let _ = std::fs::remove_dir_all(&vdir);
            std::fs::create_dir_all(&vdir)?;
            let vcfg = OutputConfig {
                target: Target::Replay { dir: vdir.clone(), segment_secs: seg, count: s.replay.seconds.div_ceil(seg) + 3 },
                video: ferrous_core::VideoSettings { base_width: vw, base_height: vh, output_width: vw, output_height: vh, ..s.video.clone() },
                ..cfg
            };
            let vout = Output::start(vcfg.clone(), subscribe_all(&self.audio, &vcfg))?;
            self.shared.outputs.lock().replay_vertical = Some((vout, vdir));
        }
        Ok(())
    }

    pub fn stop_replay(&self) {
        let rv = self.shared.outputs.lock().replay_vertical.take();
        if let Some((out, dir)) = rv {
            thread::spawn(move || {
                let _ = out.stop();
                let _ = std::fs::remove_dir_all(dir);
            });
        }
        let r = self.shared.outputs.lock().replay.take();
        if let Some((out, dir)) = r {
            thread::spawn(move || {
                let _ = out.stop();
                let _ = std::fs::remove_dir_all(dir);
            });
        }
    }

    /// Saves the buffered history to the recordings folder (in the background).
    pub fn save_replay(&self) -> anyhow::Result<PathBuf> {
        let dir = match &self.shared.outputs.lock().replay {
            Some((_, d)) => d.clone(),
            None => anyhow::bail!("the replay buffer isn't running"),
        };
        let s = &self.settings;
        std::fs::create_dir_all(&s.record.folder)?;
        let stem = format!("Replay {}", chrono_like_now());
        let out = s.record.folder.join(format!("{stem}.{}", s.replay.format.extension()));
        let (ffmpeg, secs, fmt, events, path) = (self.ffmpeg(), s.replay.seconds, s.replay.format, self.shared.events.clone(), out.clone());
        if let Some((_, vdir)) = &self.shared.outputs.lock().replay_vertical {
            let vpath = s.record.folder.join(format!("{stem} (vertical).{}", s.replay.format.extension()));
            let (ffmpeg, vdir, events) = (ffmpeg.clone(), vdir.clone(), events.clone());
            thread::spawn(move || match ferrous_output::save_replay(&ffmpeg, &vdir, secs, REPLAY_SEGMENT_SECS, &vpath, fmt) {
                Ok(()) => {
                    let _ = events.try_send(EngineEvent::Info(format!("Vertical clip saved: {}", vpath.display())));
                }
                Err(e) => {
                    let _ = events.try_send(EngineEvent::Error(format!("Vertical clip failed: {e}")));
                }
            });
        }
        thread::spawn(move || {
            match ferrous_output::save_replay(&ffmpeg, &dir, secs, REPLAY_SEGMENT_SECS, &path, fmt) {
                Ok(()) => {
                    let _ = events.try_send(EngineEvent::Info(format!("Replay saved: {}", path.display())));
                }
                Err(e) => {
                    let _ = events.try_send(EngineEvent::Error(format!("Replay save failed: {e}")));
                }
            }
        });
        Ok(out)
    }

    pub fn stop_streaming(&self) {
        *self.shared.stream_config.lock() = None;
        let extras = std::mem::take(&mut self.shared.outputs.lock().extras);
        if !extras.is_empty() {
            thread::spawn(move || {
                for e in extras {
                    if let Some(o) = e.output {
                        let _ = o.stop();
                    }
                }
            });
        }
        let out = {
            let mut o = self.shared.outputs.lock();
            o.stream_retry = None;
            o.stream.take()
        };
        if let Some(out) = out {
            let events = self.shared.events.clone();
            thread::spawn(move || {
                if let Err(e) = out.stop() {
                    log::warn!("stream stop: {e}");
                }
                let _ = events.try_send(EngineEvent::Info("Stream ended".into()));
            });
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop_streaming();
        if let Some((r, dir)) = self.shared.outputs.lock().replay_vertical.take() {
            let _ = r.stop();
            let _ = std::fs::remove_dir_all(dir);
        }
        if let Some((r, dir)) = self.shared.outputs.lock().replay.take() {
            let _ = r.stop();
            let _ = std::fs::remove_dir_all(dir);
        }
        let rec = self.shared.outputs.lock().record.take();
        if let Some(rec) = rec {
            // On exit, finish the file synchronously so it isn't truncated.
            let _ = rec.stop();
        }
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.render.take() {
            let _ = h.join();
        }
    }
}

fn apply_device_audio(audio: &AudioEngine, settings: &Settings) {
    let a = &settings.audio;
    if let Some(c) = audio.mixer.channel("desktop") {
        c.set_volume(a.desktop_volume);
        c.set_muted(a.desktop_muted);
        c.filters.lock().set(&a.desktop_filters);
        c.set_tracks(a.desktop_tracks);
        c.set_monitor(monitor_mode(a.desktop_monitor));
        c.set_delay_ms(a.desktop_delay_ms);
    }
    if let Some(c) = audio.mixer.channel("mic") {
        c.set_volume(a.mic_volume);
        c.set_muted(a.mic_muted);
        c.filters.lock().set(&a.mic_filters);
        c.set_tracks(a.mic_tracks);
        c.set_monitor(monitor_mode(a.mic_monitor));
        c.set_delay_ms(a.mic_delay_ms);
    }
}

fn monitor_mode(v: u8) -> ferrous_audio::MonitorMode {
    match v {
        1 => ferrous_audio::MonitorMode::MonitorOnly,
        2 => ferrous_audio::MonitorMode::MonitorAndOutput,
        _ => ferrous_audio::MonitorMode::Off,
    }
}

fn audio_devices_changed(a: &Settings, b: &Settings) -> bool {
    a.audio.desktop_device != b.audio.desktop_device
        || a.audio.mic_device != b.audio.mic_device
        || a.audio.desktop_enabled != b.audio.desktop_enabled
        || a.audio.mic_enabled != b.audio.mic_enabled
        || a.audio.monitor_device != b.audio.monitor_device
}

fn output_status(o: Option<&Output>, reconnecting: bool) -> OutputStatus {
    match o {
        Some(o) => OutputStatus {
            active: true,
            reconnecting,
            elapsed: o.elapsed(),
            bitrate_kbps: o.stats.bitrate_kbps(),
            speed: o.stats.speed(),
            frames_written: o.stats.frames_written.load(Ordering::Relaxed),
            frames_skipped: o.stats.frames_skipped.load(Ordering::Relaxed),
        },
        None => OutputStatus { reconnecting, ..Default::default() },
    }
}

fn spawn_render(shared: Arc<Shared>, audio: Arc<AudioEngine>, settings: &Settings) -> JoinHandle<()> {
    let video = settings.video.clone();
    let ffmpeg = ferrous_encode::ffmpeg::locate(&settings.ffmpeg_path);
    thread::Builder::new()
        .name("render".into())
        .spawn(move || {
            // The compositor's parallel work gets a pool of its own, which
            // this thread joins. On the shared global pool, Ferrous Link's JPEG
            // encoder, filters and previews queue alongside it, and a frame
            // could wait mid-composite for their work to finish.
            let threads = thread::available_parallelism().map_or(4, |n| n.get());
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .use_current_thread()
                .thread_name(|i| format!("compose-{i}"))
                .start_handler(|_| raise_thread_priority())
                .build()
                .map_err(|e| log::warn!("render pool: {e}; using the shared pool"))
                .ok();
            raise_thread_priority();
            render_loop(shared, audio, video, ffmpeg);
            drop(pool);
        })
        .expect("spawn render thread")
}

/// Converts program frames to 1080p NV12 and writes them for the camera.
fn vcam_worker(rx: crossbeam_channel::Receiver<VideoFrame>, stop: Arc<std::sync::atomic::AtomicBool>, status: Arc<VcamStats>) {
    let mut producer: Option<ferrous_vcam::Producer> = None;
    let mut last_try = Instant::now() - Duration::from_secs(5);
    let mut conv: Option<((u32, u32), ferrous_output::convert::Converter)> = None;
    let mut nv12 = Vec::new();
    let mut last_src: Option<Arc<Vec<u8>>> = None;
    while !stop.load(Ordering::Relaxed) {
        let Ok(frame) = rx.recv_timeout(Duration::from_millis(200)) else { continue };
        if producer.is_none() && last_try.elapsed() > Duration::from_secs(1) {
            last_try = Instant::now();
            producer = ferrous_vcam::Producer::open().ok();
            status.connected.store(producer.is_some(), Ordering::Relaxed);
        }
        let Some(p) = &producer else { continue };
        // The same buffer as last time (a static scene): the NV12 we
        // already have is still right, so just publish it again.
        if same_buffer(&last_src, &frame) && !nv12.is_empty() {
            p.write(&nv12);
            status.frames.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        last_src = Some(frame.data.clone());
        let size = (frame.width, frame.height);
        if conv.as_ref().map(|c| c.0) != Some(size) {
            conv = Some((size, ferrous_output::convert::Converter::new(size, (ferrous_vcam::WIDTH, ferrous_vcam::HEIGHT), ferrous_output::convert::YuvLayout::Nv12)));
        }
        conv.as_ref().unwrap().1.convert(&frame.data, &mut nv12);
        p.write(&nv12);
        status.frames.fetch_add(1, Ordering::Relaxed);
    }
}

/// True when `frame` is the very buffer last used (nothing new to show).
fn same_buffer(last: &Option<Arc<Vec<u8>>>, frame: &VideoFrame) -> bool {
    last.as_ref().is_some_and(|l| Arc::ptr_eq(l, &frame.data))
}

/// Which preview a frame is for.
#[derive(Clone, Copy)]
enum PreviewKind {
    Program = 0,
    Vertical = 1,
    Studio = 2,
}

/// Per preview kind: the frame to show, the width to make it, its serial.
type PreviewSlots = [Option<(VideoFrame, u32, u64)>; 3];

/// Makes the UI previews (downscale + RGBA) on its own thread, so the render
/// thread only hands over a frame pointer. Each kind has a one-frame slot:
/// if the worker falls behind, the newest frame wins.
struct PreviewWorker {
    jobs: Arc<Mutex<PreviewSlots>>,
    wake: crossbeam_channel::Sender<()>,
}

impl PreviewWorker {
    fn start(shared: Arc<Shared>) -> Self {
        let jobs: Arc<Mutex<PreviewSlots>> = Arc::default();
        let (wake, woken) = crossbeam_channel::bounded::<()>(1);
        let j = jobs.clone();
        let _ = thread::Builder::new().name("preview".into()).spawn(move || {
            while woken.recv().is_ok() {
                let pending = std::mem::take(&mut *j.lock());
                for (kind, job) in pending.into_iter().enumerate() {
                    let Some((frame, width, serial)) = job else { continue };
                    let (w, h, rgba) = ferrous_render::preview_rgba(&frame, width);
                    let p = Some(Arc::new(PreviewFrame { width: w, height: h, rgba, serial }));
                    match kind {
                        0 => *shared.preview.lock() = p,
                        1 => *shared.vertical_preview.lock() = p,
                        _ => *shared.studio_preview.lock() = p,
                    }
                }
                if let Some(f) = shared.frame_notify.lock().clone() {
                    f();
                }
            }
        });
        Self { jobs, wake }
    }

    fn submit(&self, kind: PreviewKind, frame: &VideoFrame, width: u32, serial: u64) {
        self.jobs.lock()[kind as usize] = Some((frame.clone(), width, serial));
        let _ = self.wake.try_send(());
    }
}

/// Rendering ahead of encoders, previews and background work keeps frame
/// times steady when the CPU is busy (a game, encoders, Ferrous Link).
/// Windows: above-normal priority. Elsewhere raising needs privileges, so
/// threads stay at the default.
fn raise_thread_priority() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
}

/// Asks Windows to schedule this process's GPU work ahead of other
/// programs' (the same call OBS makes): rendering lag shrinks when a game
/// uses the whole GPU. "Realtime" needs administrator rights, so "high" is
/// the fallback. No-op elsewhere.
fn raise_gpu_priority() {
    #[cfg(windows)]
    unsafe {
        #[link(name = "gdi32")]
        extern "system" {
            fn D3DKMTSetProcessSchedulingPriorityClass(process: isize, class: i32) -> i32;
        }
        let me = windows::Win32::System::Threading::GetCurrentProcess().0 as isize;
        for (class, name) in [(5, "realtime"), (4, "high")] {
            if D3DKMTSetProcessSchedulingPriorityClass(me, class) >= 0 {
                log::info!("GPU scheduling priority: {name}");
                return;
            }
        }
        log::warn!("couldn't raise the GPU scheduling priority");
    }
}

/// Frames of timing history kept (10 s at 60 fps).
const FRAME_HISTORY: usize = 600;

fn render_loop(shared: Arc<Shared>, audio: Arc<AudioEngine>, video: ferrous_core::VideoSettings, ffmpeg_path: PathBuf) {
    let fps = video.fps.clamp(1, 240);
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    let gpu_capture = video.gpu_capture && video.renderer != ferrous_core::RendererMode::Cpu;
    ferrous_capture::set_gpu_capture(gpu_capture);
    ferrous_render::prefer_dx12(gpu_capture);
    let (mut compositor, gpu_error) = Renderer::new(
        video.base_width,
        video.base_height,
        video.renderer != ferrous_core::RendererMode::Cpu,
    );
    if let Some(e) = gpu_error {
        let _ = shared.events.try_send(EngineEvent::Error(format!("GPU compositor unavailable, using CPU: {e}")));
    }
    shared.status.lock().renderer = compositor.describe();
    // GPU path: have it convert to NV12 for encoders (when one will use it)
    // and read results back a frame late instead of waiting on the GPU.
    if video.gpu_priority {
        raise_gpu_priority();
    }
    let on_gpu = compositor.is_gpu();
    ferrous_output::set_gpu_nv12(on_gpu);
    compositor.set_nv12(on_gpu);
    let pipelined_readback = on_gpu && video.pipelined_readback;
    let mut vertical: Option<((u32, u32), Renderer)> = video.vertical.map(|(w, h)| {
        let (w, h) = (w & !1, h & !1);
        ((w, h), Renderer::new(w, h, video.renderer != ferrous_core::RendererMode::Cpu).0)
    });
    let mut cache: HashMap<u64, CachedSource> = HashMap::new();
    let env = SourceEnv { fps, ffmpeg: ffmpeg_path, mixer: audio.mixer.clone() };
    let mut next = Instant::now();
    let mut serial = 0u64;
    // Previews go out every `divisor`-th rendered frame. Counting frames
    // (not timing them) keeps the gaps even: a ">= 33 ms" timer against a
    // 33.3 ms frame period skipped whole frames whenever one came in early.
    let mut frame_no: u64 = 0;
    let mut last_supervise = Instant::now();
    let mut fps_window = (Instant::now(), 0u32);
    let mut lagged = 0u64;
    let mut render_ms_avg = 0f32;
    let mut max_frame_ms = 0f32;
    // The program index and the collection it was taken from (a pointer
    // copy), so a transition can start from the scene as it was.
    let mut last_program: Option<(usize, Arc<SceneCollection>)> = None;
    let mut transition: Option<ActiveTransition> = None;
    let mut last_synced: Option<Arc<SceneCollection>> = None;
    let mut last_sync = Instant::now();
    let (mut last_multiview, mut multiview_next) = (Instant::now(), 0usize);
    // Buffers the previews were last made from: an unchanged canvas (a
    // memoised static scene) needs no new preview and no texture upload.
    let mut preview_src: Option<Arc<Vec<u8>>> = None;
    let mut vpreview_src: Option<Arc<Vec<u8>>> = None;
    let mut studio_src: Option<Arc<Vec<u8>>> = None;
    let mut last_start: Option<Instant> = None;
    let previews = PreviewWorker::start(shared.clone());

    while !shared.stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else if now - next > period * 2 {
            // Missed deadlines: skip ahead rather than burst, and count it.
            let missed = ((now - next).as_secs_f64() / period.as_secs_f64()) as u64;
            lagged += missed;
            next = now;
        }
        next += period;
        let t0 = Instant::now();
        let interval_ms = last_start.map_or(0.0, |l: Instant| (t0 - l).as_secs_f32() * 1000.0);
        last_start = Some(t0);
        let mut laps = Laps::new();
        frame_no += 1;
        // GPU encoders for outputs that just started (the compositor lives on this thread).
        let requests = std::mem::take(&mut *shared.encoder_requests.lock());
        for r in requests {
            let res = compositor.add_gpu_encoder(r.config, r.sink).map(|_| ()).map_err(|e| format!("{e:#}"));
            let _ = r.reply.send(res);
        }
        let pcfg = *shared.preview_cfg.lock();
        let divisor = if pcfg.max_fps == 0 { 1 } else { u64::from(fps.div_ceil(pcfg.max_fps.max(1))) };
        let preview_due = pcfg.enabled && frame_no.is_multiple_of(divisor);
        if !pcfg.enabled {
            // Re-enabling must publish at once, even for a static scene.
            preview_src = None;
            vpreview_src = None;
        }

        let scenes = shared.scenes.lock().clone();
        // Diff sources against the scenes only when they changed, plus a
        // periodic pass to restart dead sources and retry failed ones.
        if !last_synced.as_ref().is_some_and(|p| Arc::ptr_eq(p, &scenes)) || last_sync.elapsed() >= Duration::from_millis(250) {
            let program_idx = scenes.current.min(scenes.scenes.len().saturating_sub(1));
            let mut link_targets: Vec<_> = shared.links.lock().iter().map(|l| l.cfg.target.clone()).collect();
            if shared.multiview.load(Ordering::Relaxed) {
                link_targets.extend(scenes.scenes.iter().map(|s| ferrous_core::LinkSourceTarget::Scene { name: s.name.clone() }));
            }
            let studio_idx = *shared.studio_scene.lock();
            let shown = shown_items(&scenes, program_idx, studio_idx, transition.as_ref().map(|t| &t.from), &link_targets);
            sync_sources(&mut cache, &scenes, &env, &shown);
            last_synced = Some(scenes.clone());
            last_sync = Instant::now();
        }

        let program = scenes.current.min(scenes.scenes.len().saturating_sub(1));
        if last_program.as_ref().is_some_and(|(idx, _)| *idx != program) {
            let (idx, prev) = last_program.take().unwrap();
            let from = prev.scenes[idx.min(prev.scenes.len().saturating_sub(1))].clone();
            // The scene going live may bring its own transition.
            let cfg = scenes.scenes[program].transition.clone().unwrap_or_else(|| scenes.transition.clone());
            if cfg.kind != ferrous_core::TransitionKind::Cut && cfg.duration_ms > 0 {
                let stinger = match &cfg.kind {
                    ferrous_core::TransitionKind::Stinger { path, .. } | ferrous_core::TransitionKind::Matte { path, .. } => ferrous_capture::create_source(
                        &SourceConfig::Media { input: path.display().to_string(), looping: false, hw_decode: true },
                        &ferrous_capture::SourceContext { ffmpeg: env.ffmpeg.clone(), audio: None, item_id: 0, fps: video.fps },
                    )
                    .map_err(|e| log::warn!("stinger: {e}"))
                    .ok(),
                    _ => None,
                };
                transition = Some(ActiveTransition { from, cfg, start: Instant::now(), stinger });
            }
        }
        last_program = Some((program, scenes.clone()));

        // The program picture is the one steady stream of compositions, so it
        // is the one that can be read back a frame late; a transition or
        // anything else composed meanwhile goes through the waiting path.
        // The GPU's NV12 conversion (and its extra readback) only pays off when
        // something will encode this picture: skip it for a plain preview.
        if on_gpu {
            // What the program picture has to come back as, this frame:
            // - video NV12 (BT.709) for outputs that take the GPU's NV12, and Hardware links fed
            //   frames at canvas size;
            // - JPEG's YCbCr for JPEG links at canvas size, when nothing needs video NV12 (the GPU
            //   converts once, as OBS does for obs-teleport; with both wanted, the JPEG link uses BGRA);
            // - BGRA (4 bytes a pixel, the GPU path's biggest per-frame cost) only when something on
            //   the CPU looks at the picture itself.
            let canvas_size = |l: &ActiveLink| l.cfg.width == 0 || l.cfg.height == 0 || ((l.cfg.width & !1), (l.cfg.height & !1)) == (video.base_width, video.base_height);
            let (outputs_nv12, outputs_bgra) = {
                let o = shared.outputs.lock();
                let all: Vec<&Output> = o.record.iter().chain(o.stream.iter()).chain(o.replay.iter().map(|(x, _)| x)).chain(o.extras.iter().filter(|e| !e.vertical).filter_map(|e| e.output.as_ref())).collect();
                (all.iter().any(|x| x.takes_nv12()), o.vcam.is_some() || all.iter().any(|x| x.needs_bgra()))
            };
            let (links_nv12, jpeg_yuv_links, links_bgra) = {
                let ls = shared.links.lock();
                let program = |l: &&ActiveLink| matches!(l.cfg.target, ferrous_core::LinkSourceTarget::Program);
                let hw_frames = |l: &&ActiveLink| matches!(l.cfg.codec, ferrous_core::LinkCodec::Hardware { .. }) && l.sender.stream_input().is_none();
                let jpeg_420 = |l: &&ActiveLink| matches!(l.cfg.codec, ferrous_core::LinkCodec::Jpeg { full_chroma: false, .. }) && canvas_size(l);
                (
                    ls.iter().filter(program).any(|l| hw_frames(&l) && canvas_size(l)),
                    ls.iter().filter(program).any(|l| jpeg_420(&l)),
                    ls.iter().filter(program).any(|l| !(matches!(l.cfg.codec, ferrous_core::LinkCodec::Hardware { .. }) && (l.sender.stream_input().is_some() || canvas_size(l))) && !jpeg_420(&l))
                        || ls.iter().filter(program).any(|l| hw_frames(&l) && !canvas_size(l)),
                )
            };
            let video_nv12 = outputs_nv12 || links_nv12;
            let jfif = jpeg_yuv_links && !video_nv12;
            compositor.set_nv12(video_nv12 || jfif);
            compositor.set_nv12_jfif(jfif);
            // With pipelined read-back the frame returned now was drawn last time, so a preview due
            // next frame counts too.
            let preview_soon = pcfg.enabled && (preview_due || (pipelined_readback && (frame_no + 1).is_multiple_of(divisor)));
            let jpeg_needs_bgra = jpeg_yuv_links && !jfif;
            // The preview is drawn at its own size on the GPU and read back on its own (a quarter of
            // the bytes or less, no CPU downscale), so it doesn't need the full picture.
            compositor.set_preview_width(preview_soon.then_some(pcfg.width));
            compositor.set_bgra(outputs_bgra || links_bgra || jpeg_needs_bgra || transition.is_some() || shared.studio_scene.lock().is_some());
        }
        compositor.set_pipelined(pipelined_readback && transition.is_none() && shared.studio_scene.lock().is_none() && !shared.links.lock().iter().any(|l| matches!(l.cfg.target, ferrous_core::LinkSourceTarget::Scene { .. })));
        // GPU encoders copy the program picture as it is composed; during a transition the blend is
        // made on the CPU, so they get that instead (below).
        let in_transition = transition.is_some();
        let encoding = compositor.gpu_encoder_count() > 0;
        if encoding && !in_transition {
            compositor.set_encode_pts(Some(frame_no));
        }
        let to = compose_nested(&scenes.scenes[program], Some(&scenes), &mut cache, &mut compositor, 0);
        compositor.set_encode_pts(None);
        // Everything composed after the program picture (studio preview, multiview, scenes for links)
        // is read back as plain BGRA.
        compositor.set_bgra(true);
        compositor.set_nv12(false);
        compositor.set_nv12_jfif(false);
        compositor.set_preview_width(None);
        let gpu_preview = compositor.take_preview();
        compositor.set_pipelined(false);
        let canvas = match &mut transition {
            Some(t) => {
                let elapsed = t.start.elapsed().as_secs_f32() * 1000.0;
                let p = elapsed / t.cfg.duration_ms as f32;
                if p >= 1.0 {
                    transition = None;
                    to
                } else {
                    let from = compose_nested(&t.from, Some(&scenes), &mut cache, &mut compositor, 0);
                    match &t.cfg.kind {
                        ferrous_core::TransitionKind::Stinger { cut_point_ms, .. } => {
                            let under = if elapsed < *cut_point_ms as f32 { from } else { to };
                            match t.stinger.as_mut().and_then(|s| s.latest_frame()) {
                                Some(sf) => {
                                    let tf = ferrous_core::Transform::fit_to(sf.width, sf.height, video.base_width, video.base_height);
                                    let mut layers = vec![Layer { frame: &under, transform: ferrous_core::Transform::default(), filters: &[] }];
                                    layers.push(Layer { frame: &sf, transform: tf, filters: &[] });
                                    compositor.compose(&layers)
                                }
                                None => under,
                            }
                        }
                        ferrous_core::TransitionKind::Matte { layout, invert, .. } => match t.stinger.as_mut().and_then(|s| s.latest_frame()) {
                            Some(sf) => {
                                let (picture, matte) = ferrous_render::transitions::split_matte(&sf, *layout);
                                let mixed = ferrous_render::transitions::matte_mix(&from, &to, &matte, *invert);
                                match picture {
                                    Some(pic) => {
                                        let tf = ferrous_core::Transform::fit_to(pic.width, pic.height, video.base_width, video.base_height);
                                        compositor.compose(&[Layer { frame: &mixed, transform: ferrous_core::Transform::default(), filters: &[] }, Layer { frame: &pic, transform: tf, filters: &[] }])
                                    }
                                    None => mixed,
                                }
                            }
                            // Until the video's first frame arrives, hold the outgoing scene.
                            None => from,
                        },
                        kind => ferrous_render::transitions::apply(kind, &from, &to, p),
                    }
                }
            }
            None => to,
        };
        if encoding && in_transition {
            compositor.set_encode_pts(Some(frame_no));
            let _ = compositor.compose(&[Layer { frame: &canvas, transform: Default::default(), filters: &[] }]);
            compositor.set_encode_pts(None);
        }
        if encoding {
            compositor.encoders_tick(frame_no);
        }

        laps.lap(0);
        // Vertical canvas: same program scene, its own layout.
        let vcanvas = vertical.as_mut().map(|(size, vr)| {
            // Like the program picture: read the vertical canvas back a frame late instead of waiting for the
            // GPU on every changed frame (a transition needs both pictures current, so it waits).
            vr.set_pipelined(pipelined_readback && transition.is_none());
            let to = compose_vertical(&scenes.scenes[program], &scenes, &mut cache, vr, *size, (video.base_width, video.base_height));
            match &transition {
                Some(t) if !matches!(t.cfg.kind, ferrous_core::TransitionKind::Stinger { .. } | ferrous_core::TransitionKind::Matte { .. }) => {
                    let p = t.start.elapsed().as_secs_f32() * 1000.0 / t.cfg.duration_ms.max(1) as f32;
                    let from = compose_vertical(&t.from, &scenes, &mut cache, vr, *size, (video.base_width, video.base_height));
                    ferrous_render::transitions::apply(&t.cfg.kind, &from, &to, p.min(1.0))
                }
                _ => to,
            }
        });
        laps.lap(1);
        if let Some(v) = &vcanvas {
            if preview_due && !same_buffer(&vpreview_src, v) {
                vpreview_src = Some(v.data.clone());
                serial += 1;
                previews.submit(PreviewKind::Vertical, v, pcfg.vertical_width, serial);
            }
        }

        laps.lap(4);
        {
            let mut o = shared.outputs.lock();
            for e in &o.extras {
                if let Some(out) = &e.output {
                    match (&vcanvas, e.vertical) {
                        (Some(v), true) => out.push_frame(v),
                        (_, false) => out.push_frame(&canvas),
                        _ => {}
                    }
                }
            }
            if let (Some((r, _)), Some(v)) = (&o.replay_vertical, &vcanvas) {
                r.push_frame(v);
            }
            if let Some(r) = &o.record {
                r.push_frame(&canvas);
            }
            if let Some(v) = &mut o.vcam {
                if v.last_sent.elapsed() >= Duration::from_millis(1000 / ferrous_vcam::FPS as u64 - 2) {
                    v.last_sent = Instant::now();
                    let _ = v.tx.try_send(canvas.clone());
                }
            }
            if let Some(s) = &o.stream {
                s.push_frame(&canvas);
            }
            if let Some((r, _)) = &o.replay {
                r.push_frame(&canvas);
            }
        }

        laps.lap(2);
        // Ferrous Link outputs.
        {
            let mut links = shared.links.lock();
            for l in links.iter_mut() {
                if l.last_push.elapsed() + Duration::from_millis(2) < l.interval {
                    continue;
                }
                l.last_push = Instant::now();
                match &l.cfg.target {
                    ferrous_core::LinkSourceTarget::Program => l.sender.push_frame(&canvas),
                    ferrous_core::LinkSourceTarget::Scene { name } => {
                        if let Some(sc) = scenes.scenes.iter().find(|s| s.name == *name) {
                            let f = compose_nested(sc, Some(&scenes), &mut cache, &mut compositor, 0);
                            l.sender.push_frame(&f);
                        }
                    }
                    ferrous_core::LinkSourceTarget::Source { name } => {
                        let id = scenes.scenes.iter().flat_map(|s| &s.items).find(|i| i.name == *name).map(|i| i.id);
                        if let Some(f) = id.and_then(|id| cache.get_mut(&id)).and_then(|c| c.source.as_mut()).and_then(|s| s.latest_frame()) {
                            l.sender.push_frame(&f);
                        }
                    }
                }
            }
        }

        laps.lap(3);
        // Program preview: every rendered frame unless capped.
        if let (true, Some((w, h, rgba)), false) = (preview_due, gpu_preview, in_transition) {
            serial += 1;
            *shared.preview.lock() = Some(Arc::new(PreviewFrame { width: w, height: h, rgba, serial }));
            preview_src = None;
            if let Some(f) = shared.frame_notify.lock().clone() {
                f();
            }
        } else if preview_due && !canvas.data.is_empty() && !same_buffer(&preview_src, &canvas) {
            preview_src = Some(canvas.data.clone());
            serial += 1;
            previews.submit(PreviewKind::Program, &canvas, pcfg.width, serial);
        }

        // Studio mode: also render the preview scene for the UI.
        let studio = *shared.studio_scene.lock();
        match studio {
            Some(idx) if idx < scenes.scenes.len() && preview_due => {
                let f = compose_nested(&scenes.scenes[idx], Some(&scenes), &mut cache, &mut compositor, 0);
                if !same_buffer(&studio_src, &f) {
                    studio_src = Some(f.data.clone());
                    serial += 1;
                    previews.submit(PreviewKind::Studio, &f, pcfg.width, serial);
                }
            }
            None if studio_src.is_some() => {
                studio_src = None;
                *shared.studio_preview.lock() = None;
            }
            _ => {}
        }

        // Multiview: one scene at a time, so the cost stays at a few small renders a second.
        if shared.multiview.load(Ordering::Relaxed) {
            if last_multiview.elapsed() >= Duration::from_millis(100) && !scenes.scenes.is_empty() {
                last_multiview = Instant::now();
                let (n, idx) = (scenes.scenes.len(), multiview_next % scenes.scenes.len());
                multiview_next += 1;
                let f = compose_nested(&scenes.scenes[idx], Some(&scenes), &mut cache, &mut compositor, 0);
                let (w, h, rgba) = ferrous_render::preview_rgba(&f, 360);
                serial += 1;
                {
                    let mut t = shared.multiview_thumbs.lock();
                    t.resize(n, None);
                    t[idx] = Some(Arc::new(PreviewFrame { width: w, height: h, rgba, serial }));
                }
                if let Some(f) = shared.frame_notify.lock().clone() {
                    f();
                }
            }
        } else if !shared.multiview_thumbs.lock().is_empty() {
            shared.multiview_thumbs.lock().clear();
        }

        laps.lap(4);

        if last_supervise.elapsed() >= Duration::from_millis(500) {
            last_supervise = Instant::now();
            supervise(&shared, &audio);
            {
                let mut perf = shared.perf.lock();
                perf.clear();
                let mut pids: Vec<u32> = Vec::new();
                for (id, c) in cache.iter() {
                    let mut p = c.perf.clone();
                    p.pids = c.source.as_ref().map(|s| s.pids()).unwrap_or_default();
                    pids.extend(&p.pids);
                    perf.insert(*id, p);
                }
                {
                    let o = shared.outputs.lock();
                    pids.extend(o.stream.iter().chain(o.record.iter()).map(|x| x.pid()));
                    pids.extend(o.replay.iter().chain(o.replay_vertical.iter()).map(|(x, _)| x.pid()));
                    pids.extend(o.extras.iter().filter_map(|e| e.output.as_ref()).map(|x| x.pid()));
                }
                shared.sysmon.set_watched(pids);
            }
            let mut info = shared.source_info.lock();
            info.clear();
            for (id, c) in &cache {
                info.insert(*id, (c.size, c.error.clone().or_else(|| c.filter_error.clone())));
            }
        }

        // Fold this frame's per-source costs into smoothed readouts.
        for c in cache.values_mut() {
            let (s, comp, g, seen) = std::mem::take(&mut c.acc);
            let a = 0.08;
            c.perf.source_ms += (s - c.perf.source_ms) * a;
            c.perf.composite_ms += (comp - c.perf.composite_ms) * a;
            if g > 0.0 || c.perf.gpu_ms.is_some() {
                let prev = c.perf.gpu_ms.unwrap_or(g);
                c.perf.gpu_ms = Some(prev + (g - prev) * a);
            }
            if !seen {
                c.perf.source_ms *= 0.9;
                c.perf.composite_ms *= 0.9;
            }
        }
        laps.lap(5);
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        {
            let mut ft = shared.frame_times.lock();
            if ft.len() == FRAME_HISTORY {
                ft.pop_front();
            }
            ft.push_back(FrameSample { interval_ms, work_ms: ms, stages_ms: laps.ms });
        }
        max_frame_ms = max_frame_ms.max(ms);
        render_ms_avg = render_ms_avg * 0.95 + ms * 0.05;
        fps_window.1 += 1;
        if fps_window.0.elapsed() >= Duration::from_secs(1) {
            let mut st = shared.status.lock();
            st.fps = fps_window.1 as f32 / fps_window.0.elapsed().as_secs_f32();
            st.render_ms = render_ms_avg;
            st.lagged_frames = lagged;
            st.max_frame_ms = max_frame_ms;
            st.gpu_ms = compositor.last_gpu_ms();
            max_frame_ms = 0.0;
            fps_window = (Instant::now(), 0);
        }
    }
}

struct ActiveLink {
    cfg: ferrous_core::LinkOutputSettings,
    sender: Arc<ferrous_link::LinkSender>,
    interval: Duration,
    last_push: Instant,
    audio_stop: Arc<AtomicBool>,
}

impl Drop for ActiveLink {
    fn drop(&mut self) {
        self.audio_stop.store(true, Ordering::Relaxed);
    }
}

/// Starts/stops Ferrous Link outputs to match the settings.
fn sync_links(shared: &Shared, audio: &AudioEngine, settings: &Settings) {
    let old: Vec<ActiveLink> = std::mem::take(&mut *shared.links.lock());
    let mut links: Vec<ActiveLink> = Vec::new();
    // Shut the old senders down completely before starting new ones, so a restarted link gets its own
    // port back. Each link's audio thread holds a reference to its sender and lets go within ~100 ms of
    // being told to stop; starting the new sender before that found the port still taken and moved to
    // the next one, so receivers had to be pointed at a new port after every settings change.
    let deadline = Instant::now() + Duration::from_millis(500);
    for l in old {
        let sender = l.sender.clone();
        drop(l); // signals its audio thread to stop
        while Arc::strong_count(&sender) > 1 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        drop(sender); // the last reference: closes the listener and records the port as just released
    }
    // (The new links are built without holding the lock: starting a GPU encoder waits for the render
    // thread, which takes that lock every frame.)
    let v = &settings.video;
    for cfg in settings.links.iter().filter(|l| l.enabled) {
        let fps = if cfg.fps == 0 { v.fps } else { cfg.fps.min(v.fps) };
        let (w, h) = if cfg.width == 0 || cfg.height == 0 { (v.base_width, v.base_height) } else { (cfg.width & !1, cfg.height & !1) };
        let sc = ferrous_link::sender::SenderConfig {
            name: cfg.name.clone(),
            codec: cfg.codec.clone(),
            width: w,
            height: h,
            fps,
            port: cfg.port,
            pin: cfg.pin.clone(),
            audio: cfg.audio,
            ffmpeg: ferrous_encode::ffmpeg::locate(&settings.ffmpeg_path),
            external_stream: false,
        };
        match start_link_sender(shared, sc, cfg, settings) {
            Ok(sender) => {
                let sender = Arc::new(sender);
                let audio_stop = Arc::new(AtomicBool::new(false));
                if cfg.audio {
                    let rx = audio.subscribe_track(cfg.audio_track);
                    let (s, stop) = (sender.clone(), audio_stop.clone());
                    thread::spawn(move || {
                        while !stop.load(Ordering::Relaxed) {
                            if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
                                s.push_audio(&chunk);
                            }
                        }
                    });
                }
                let _ = shared.events.try_send(EngineEvent::Info(format!("Ferrous Link \"{}\" ready on port {}", cfg.name, sender.port)));
                links.push(ActiveLink {
                    cfg: cfg.clone(),
                    sender,
                    interval: Duration::from_secs_f64(1.0 / fps.max(1) as f64),
                    last_push: Instant::now() - Duration::from_secs(1),
                    audio_stop,
                });
            }
            Err(e) => {
                let _ = shared.events.try_send(EngineEvent::Error(format!("Ferrous Link \"{}\" failed: {e}", cfg.name)));
            }
        }
    }
    *shared.links.lock() = links;
}

/// Starts a link's sender. A Hardware link of the program picture with an NVIDIA encoder is fed by an
/// in-process GPU encoder straight from the canvas (no read-back, no FFmpeg on this PC), the way
/// recording is; anything else, or any failure starting that, uses the FFmpeg encoder.
fn start_link_sender(shared: &Shared, mut sc: ferrous_link::sender::SenderConfig, cfg: &ferrous_core::LinkOutputSettings, settings: &Settings) -> anyhow::Result<ferrous_link::LinkSender> {
    use ferrous_render::ferrous_nvenc::{Codec, Config, Rate};
    let v = &settings.video;
    let gpu = match &cfg.codec {
        ferrous_core::LinkCodec::Hardware { encoder, bitrate_kbps }
            if v.zero_copy_encoding && v.renderer != ferrous_core::RendererMode::Cpu && matches!(cfg.target, ferrous_core::LinkSourceTarget::Program) && sc.fps == v.fps =>
        {
            let codec = match encoder.as_str() {
                "h264_nvenc" => Some(Codec::H264),
                "hevc_nvenc" => Some(Codec::Hevc),
                _ => None,
            };
            codec.map(|codec| Config {
                codec,
                input: (v.base_width, v.base_height),
                output: (sc.width & !1, sc.height & !1),
                fps: sc.fps.max(1),
                // Variable bitrate around the target; no look-ahead, no B-frames: each frame leaves as
                // soon as it is encoded.
                rate: Rate::Vbr((*bitrate_kbps).max(1000)),
                preset: 4,
                keyframe_interval_frames: sc.fps.max(1),
                lookahead: false,
                multipass: false,
                bframes: 0,
            })
        }
        _ => None,
    };
    if let Some(config) = gpu {
        sc.external_stream = true;
        let sender = ferrous_link::LinkSender::start(sc.clone())?;
        let input = sender.stream_input().expect("external stream requested");
        let kind = if config.codec == Codec::Hevc { ferrous_output::es_mux::EsKind::Hevc } else { ferrous_output::es_mux::EsKind::H264 };
        let mut mux = ferrous_output::es_mux::EsMux::new(kind, config.fps, config.output);
        let sink: ferrous_render::ferrous_nvenc::Sink = Box::new(move |p| {
            let mut buf = Vec::with_capacity(p.data.len() + p.data.len() / 180 * 8 + 1024);
            mux.frame(p.data, p.pts, p.dts, p.keyframe, &mut buf);
            match input.send_timeout(buf, Duration::from_millis(500)) {
                Ok(()) => true,
                Err(crossbeam_channel::SendTimeoutError::Timeout(_)) => true, // a stalled receiver; it rejoins at a keyframe
                Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => false,
            }
        });
        let (reply, rx) = crossbeam_channel::bounded(1);
        shared.encoder_requests.lock().push(EncoderRequest { config, sink, reply });
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => {
                log::info!("Ferrous Link \"{}\": encoding on the GPU inside Ferrous (zero copy)", cfg.name);
                return Ok(sender);
            }
            Ok(Err(e)) => log::warn!("Ferrous Link \"{}\": GPU encoder not used ({e}); using FFmpeg", cfg.name),
            Err(_) => log::warn!("Ferrous Link \"{}\": GPU encoder didn't start in time; using FFmpeg", cfg.name),
        }
        // Free the port before starting again on the FFmpeg path.
        drop(sender);
        sc.external_stream = false;
    }
    ferrous_link::LinkSender::start(sc)
}

struct ActiveTransition {
    from: ferrous_core::Scene,
    cfg: ferrous_core::Transition,
    start: Instant,
    stinger: Option<Box<dyn Source>>,
}

/// A frame waiting for its source's video filters.
struct FilterJob<'a> {
    slot: usize,
    id: u64,
    frame: VideoFrame,
    filters: &'a ferrous_core::filters::VideoFilters,
}

/// Runs each job's filter chain. With several filtered sources in a scene the
/// chains run side by side (each source has its own state), instead of one
/// after another on the render thread. Returns `(slot, filtered frame)`.
fn run_filters(cache: &mut HashMap<u64, CachedSource>, jobs: Vec<FilterJob<'_>>) -> Vec<(usize, VideoFrame)> {
    use rayon::prelude::*;
    fn one(c: &mut CachedSource, frame: VideoFrame, filters: &ferrous_core::filters::VideoFilters) -> VideoFrame {
        let t = Instant::now();
        let (out, err) = c.filters.apply(frame, filters);
        c.filter_error = err;
        c.acc.0 += t.elapsed().as_secs_f32() * 1000.0;
        out
    }
    if jobs.len() < 2 {
        return jobs
            .into_iter()
            .filter_map(|j| cache.get_mut(&j.id).map(|c| (j.slot, one(c, j.frame, j.filters))))
            .collect();
    }
    let mut refs: HashMap<u64, &mut CachedSource> = cache.iter_mut().filter(|(id, _)| jobs.iter().any(|j| j.id == **id)).map(|(k, v)| (*k, v)).collect();
    let work: Vec<_> = jobs.into_iter().filter_map(|j| refs.remove(&j.id).map(|c| (j.slot, c, j.frame, j.filters))).collect();
    work.into_par_iter().map(|(slot, c, frame, filters)| (slot, one(c, frame, filters))).collect()
}

/// Samples, filters and composes one scene's visible items.
/// Max depth for scenes nested in scenes (also stops cycles).
const MAX_NESTING: usize = 4;

fn compose_nested(
    scene: &ferrous_core::Scene,
    all: Option<&SceneCollection>,
    cache: &mut HashMap<u64, CachedSource>,
    compositor: &mut Renderer,
    depth: usize,
) -> VideoFrame {
    // One slot per drawn item, in order; filtered frames are dropped into
    // their slots once every chain has run.
    let mut slots: Vec<(VideoFrame, ferrous_core::Transform, Vec<ferrous_render::filters::GpuFilter>)> = Vec::new();
    let mut ids: Vec<u64> = Vec::new();
    let mut jobs: Vec<FilterJob<'_>> = Vec::new();
    for item in &scene.items {
        if !item.visible {
            continue;
        }
        if let (SourceConfig::Scene { name }, Some(all)) = (&item.source, all) {
            if depth < MAX_NESTING && *name != scene.name {
                if let Some(inner) = all.scenes.iter().find(|s| s.name == *name) {
                    let f = compose_nested(inner, Some(all), cache, compositor, depth + 1);
                    if let Some(c) = cache.get_mut(&item.id) {
                        c.size = (f.width, f.height);
                    }
                    slots.push((f, item.transform, Vec::new()));
                    ids.push(item.id);
                }
            }
            continue;
        }
        let t = Instant::now();
        // Nested scenes compose on the CPU, so only the top level may take GPU-resident frames.
        let gpu = depth == 0 && compositor.takes_gpu_frames() && gpu_resident_ok(&item.video_filters);
        let mut latest = item_frame_with(cache, item, gpu);
        if latest.as_ref().is_some_and(|f| f.gpu.is_some() && !compositor.accepts_gpu_frame(f)) {
            latest = item_frame_with(cache, item, false);
        }
        if let Some(c) = cache.get_mut(&item.id) {
            if let Some(f) = latest {
                c.size = (f.width, f.height);
                let mut plan = Vec::new();
                if item.video_filters.iter().any(|f| f.enabled) {
                    // On the GPU the filters run as passes over the layer's texture; a chain with a
                    // plugin (CPU only), or the CPU compositor, filters the frame here instead.
                    match compositor.is_gpu().then(|| c.filters.plan(&f, &item.video_filters)).flatten() {
                        Some((p, err)) => {
                            c.filter_error = err;
                            plan = p;
                        }
                        None => jobs.push(FilterJob { slot: slots.len(), id: item.id, frame: f.clone(), filters: &item.video_filters }),
                    }
                } else {
                    c.filter_error = None;
                }
                slots.push((f, item.transform, plan));
                ids.push(item.id);
            }
            c.acc.0 += t.elapsed().as_secs_f32() * 1000.0;
            c.acc.3 = true;
        }
    }
    for (slot, out) in run_filters(cache, jobs) {
        slots[slot].0 = out;
    }
    let frames = slots;
    let layers: Vec<Layer<'_>> = frames.iter().map(|(frame, transform, plan)| Layer { frame, transform: *transform, filters: plan }).collect();
    if depth > 0 {
        compositor.compose_transparent(&layers)
    } else {
        let out = compositor.compose_timed(&layers);
        attribute(cache, &ids, &out);
        out.frame
    }
}

/// An item's newest raw frame. A clone reads its original's frame; cloning a
/// clone (or yourself) shows nothing, which also rules out cycles.
#[cfg(test)]
fn item_frame(cache: &mut HashMap<u64, CachedSource>, item: &ferrous_core::SceneItem) -> Option<VideoFrame> {
    item_frame_with(cache, item, false)
}

/// Whether the GPU compositor can run all of an item's filters without the picture on the CPU
/// (plugins and background removal read pixels, so they need CPU frames).
fn gpu_resident_ok(filters: &ferrous_core::filters::VideoFilters) -> bool {
    use ferrous_core::filters::VideoFilter;
    !filters.iter().any(|f| f.enabled && matches!(f.filter, VideoFilter::Plugin { .. } | VideoFilter::BackgroundRemoval { .. }))
}

/// [`item_frame`], preferring a frame that lives only on the GPU when `gpu` is set and the source has one.
fn item_frame_with(cache: &mut HashMap<u64, CachedSource>, item: &ferrous_core::SceneItem, gpu: bool) -> Option<VideoFrame> {
    let id = match &item.source {
        SourceConfig::Clone { source } => {
            if *source == item.id || matches!(cache.get(source)?.config, SourceConfig::Clone { .. }) {
                return None;
            }
            *source
        }
        _ => item.id,
    };
    let src = cache.get_mut(&id)?.source.as_mut()?;
    if gpu {
        if let Some(f) = src.latest_gpu_frame() {
            return Some(f);
        }
    }
    src.latest_frame()
}

/// Adds a composition's per-layer costs to each item's frame accumulator.
fn attribute(cache: &mut HashMap<u64, CachedSource>, ids: &[u64], out: &ferrous_render::Composed) {
    for (i, id) in ids.iter().enumerate() {
        if let Some(c) = cache.get_mut(id) {
            c.acc.1 += out.cpu_ms.get(i).copied().unwrap_or(0.0);
            c.acc.2 += out.gpu_ms.get(i).copied().unwrap_or(0.0);
        }
    }
}

/// Composes a scene on the vertical canvas using its vertical layout.
fn compose_vertical(
    scene: &ferrous_core::Scene,
    all: &SceneCollection,
    cache: &mut HashMap<u64, CachedSource>,
    compositor: &mut Renderer,
    canvas: (u32, u32),
    main: (u32, u32),
) -> VideoFrame {
    let mut slots: Vec<(VideoFrame, ferrous_core::Transform, Vec<ferrous_render::filters::GpuFilter>)> = Vec::new();
    let mut ids: Vec<u64> = Vec::new();
    let mut jobs: Vec<FilterJob<'_>> = Vec::new();
    for item in &scene.items {
        let mut filtered = false;
        let frame = if let SourceConfig::Scene { name } = &item.source {
            all.scenes.iter().find(|s| s.name == *name && s.name != scene.name).map(|inner| compose_nested(inner, Some(all), cache, compositor, 1))
        } else {
            let gpu = compositor.takes_gpu_frames() && gpu_resident_ok(&item.video_filters);
            let mut f = item_frame_with(cache, item, gpu);
            if f.as_ref().is_some_and(|f| f.gpu.is_some() && !compositor.accepts_gpu_frame(f)) {
                f = item_frame_with(cache, item, false);
            }
            f.inspect(|_| filtered = item.video_filters.iter().any(|f| f.enabled))
        };
        let Some(f) = frame else { continue };
        let (t, visible) = scene.vertical_layout(item, canvas, main);
        if visible {
            let mut plan = Vec::new();
            if filtered {
                let planned = if compositor.is_gpu() { cache.get_mut(&item.id).and_then(|c| c.filters.plan(&f, &item.video_filters).map(|(p, err)| (c, p, err))) } else { None };
                match planned {
                    Some((c, p, err)) => {
                        c.filter_error = err;
                        plan = p;
                    }
                    None => jobs.push(FilterJob { slot: slots.len(), id: item.id, frame: f.clone(), filters: &item.video_filters }),
                }
            }
            slots.push((f, t, plan));
            ids.push(item.id);
        }
    }
    for (slot, out) in run_filters(cache, jobs) {
        slots[slot].0 = out;
    }
    let frames = slots;
    let layers: Vec<Layer<'_>> = frames.iter().map(|(frame, transform, plan)| Layer { frame, transform: *transform, filters: plan }).collect();
    let out = compositor.compose_timed(&layers);
    attribute(cache, &ids, &out);
    out.frame
}

struct SourceEnv {
    fps: u32,
    ffmpeg: PathBuf,
    mixer: Arc<Mixer>,
}

/// Creates, recreates and drops sources so the cache matches every item in
/// every scene. Sources stay warm across scene switches, like OBS.
fn sync_sources(cache: &mut HashMap<u64, CachedSource>, scenes: &SceneCollection, env: &SourceEnv, shown: &HashSet<u64>) {
    let mut wanted: HashMap<u64, &ferrous_core::SceneItem> = HashMap::new();
    for scene in &scenes.scenes {
        for item in &scene.items {
            wanted.insert(item.id, item);
        }
    }
    cache.retain(|id, _| wanted.contains_key(id));
    for (id, item) in wanted {
        let config = &item.source;
        let entry = cache.entry(id).or_insert_with(|| CachedSource {
            config: config.clone(),
            source: None,
            last_attempt: Instant::now() - Duration::from_secs(60),
            error: None,
            size: (0, 0),
            audio: None,
            filters: Default::default(),
            filter_error: None,
            perf: SourcePerf::default(),
            acc: (0.0, 0.0, 0.0, false),
            idle_since: None,
        });
        // Sources nobody can see stop after a grace period (and don't start).
        let needed = item.keep_running || shown.contains(&id) || matches!(config, SourceConfig::Scene { .. } | SourceConfig::Clone { .. });
        let sleeping = if needed {
            entry.idle_since = None;
            false
        } else {
            let since = *entry.idle_since.get_or_insert_with(Instant::now);
            since.elapsed() >= IDLE_GRACE
        };
        if sleeping {
            if entry.source.take().is_some() {
                log::debug!("source {id} ({}) idle, stopped", item.name);
            }
            entry.config = config.clone();
            continue;
        }
        if entry.config != *config {
            // Some sources (OBS plugins) take new settings without a restart.
            let updated = entry.source.as_mut().is_some_and(|s| s.update(config));
            entry.config = config.clone();
            if !updated {
                entry.source = None;
                entry.last_attempt = Instant::now() - Duration::from_secs(60);
            }
        }
        let dead = entry.source.as_ref().is_some_and(|s| !s.is_alive());
        if dead {
            entry.source = None;
        }
        if matches!(config, SourceConfig::Scene { .. } | SourceConfig::Clone { .. }) {
            continue; // composed by the engine itself
        }
        // Not yet seen: wait to be shown rather than starting it.
        if entry.source.is_none() && needed && entry.last_attempt.elapsed() >= Duration::from_secs(2) {
            entry.last_attempt = Instant::now();
            let has_audio = matches!(
                config,
                SourceConfig::Media { .. } | SourceConfig::VideoDevice { .. } | SourceConfig::ApplicationAudio { .. } | SourceConfig::LinkReceiver { .. } | SourceConfig::ObsPlugin { .. }
            );
            let channel = has_audio.then(|| env.mixer.source_channel(&format!("source-{id}"), &item.name));
            let sink: Option<ferrous_capture::AudioSink> = channel.clone().map(|c| {
                let f: ferrous_capture::AudioSink = Arc::new(move |samples: &[f32]| c.push(samples));
                f
            });
            let ctx = ferrous_capture::SourceContext { ffmpeg: env.ffmpeg.clone(), audio: sink, item_id: id, fps: env.fps };
            match ferrous_capture::create_source(config, &ctx) {
                Ok(s) => {
                    entry.source = Some(s);
                    entry.error = None;
                    if let Some(ch) = &channel {
                        ch.set_volume(item.audio_volume);
                        ch.set_muted(item.audio_muted);
                    }
                    entry.audio = channel;
                }
                Err(e) => {
                    if entry.error.as_deref() != Some(&e.to_string()) {
                        log::warn!("source {id}: {e}");
                    }
                    entry.error = Some(e.to_string());
                }
            }
        }
        if let Some(ch) = &entry.audio {
            ch.filters.lock().set(&item.audio_filters);
            ch.set_tracks(item.audio_tracks);
            ch.set_monitor(monitor_mode(item.audio_monitor));
            ch.set_delay_ms(item.audio_sync_ms);
            if *ch.name.lock() != item.name {
                *ch.name.lock() = item.name.clone();
            }
        }
    }
}

fn supervise(shared: &Shared, audio: &AudioEngine) {
    let mut o = shared.outputs.lock();
    // Extra destinations: report drops and reconnect (up to MAX_RECONNECTS).
    for e in o.extras.iter_mut() {
        if e.output.as_ref().is_some_and(|out| !out.is_running()) {
            let out = e.output.take().unwrap();
            let err = out.stats.error().unwrap_or_else(|| "connection closed".into());
            let _ = shared.events.try_send(EngineEvent::Error(format!("{} disconnected: {err}", e.name)));
            e.retry_at = Some(Instant::now() + RECONNECT_DELAY);
        }
        if e.output.is_none() && e.retry_at.is_some_and(|t| Instant::now() >= t) {
            e.attempts += 1;
            match Output::start(e.cfg.clone(), subscribe_all(audio, &e.cfg)) {
                Ok(out) => {
                    e.output = Some(out);
                    e.retry_at = None;
                    let _ = shared.events.try_send(EngineEvent::Info(format!("{}: reconnecting (attempt {})…", e.name, e.attempts)));
                }
                Err(_) if e.attempts < MAX_RECONNECTS => e.retry_at = Some(Instant::now() + RECONNECT_DELAY),
                Err(err) => {
                    e.retry_at = None;
                    let _ = shared.events.try_send(EngineEvent::Error(format!("{}: gave up reconnecting: {err}", e.name)));
                }
            }
        }
    }
    o.extras.retain(|e| e.output.is_some() || e.retry_at.is_some());
    if o.replay.as_ref().is_some_and(|(r, _)| !r.is_running()) {
        let (r, _) = o.replay.take().unwrap();
        let err = r.stats.error().unwrap_or_else(|| "FFmpeg exited".into());
        let _ = shared.events.try_send(EngineEvent::Error(format!("Replay buffer stopped: {err}")));
    }
    if let Some(rec) = &o.record {
        if !rec.is_running() {
            let err = rec.stats.error().unwrap_or_else(|| "FFmpeg exited".into());
            let _ = shared.events.try_send(EngineEvent::Error(format!("Recording stopped: {err}")));
            o.record = None;
            o.record_path = None;
        }
    }
    if let Some(stream) = &o.stream {
        if !stream.is_running() {
            let err = stream.stats.error().unwrap_or_else(|| "connection closed".into());
            let _ = shared.events.try_send(EngineEvent::Error(format!("Stream disconnected: {err}")));
            o.stream = None;
            let cfg = shared.stream_config.lock().clone();
            match (cfg, o.stream_retry.as_mut()) {
                // Died again during a reconnect's probation: keep counting.
                (Some(_), Some(r)) => r.at = Instant::now() + RECONNECT_DELAY,
                (Some(config), None) => {
                    o.stream_retry =
                        Some(StreamRetry { config, attempts: 0, at: Instant::now() + RECONNECT_DELAY })
                }
                (None, _) => o.stream_retry = None,
            }
        }
    }
    if o.stream.is_none() {
        if let Some(retry) = o.stream_retry.as_mut() {
            if Instant::now() >= retry.at {
                retry.attempts += 1;
                let n = retry.attempts;
                match Output::start(retry.config.clone(), subscribe_all(audio, &retry.config)) {
                    Ok(out) => {
                        let _ = shared.events.try_send(EngineEvent::Info(format!("Reconnecting (attempt {n})…")));
                        o.stream = Some(out);
                        // Stay in retry mode until it has survived a while.
                        if let Some(r) = o.stream_retry.as_mut() {
                            r.at = Instant::now() + Duration::from_secs(30);
                        }
                    }
                    Err(e) => {
                        let _ = shared.events.try_send(EngineEvent::Error(format!("Reconnect failed: {e}")));
                        if let Some(r) = o.stream_retry.as_mut() {
                            r.at = Instant::now() + RECONNECT_DELAY;
                        }
                    }
                }
                if n >= MAX_RECONNECTS && o.stream.is_none() {
                    o.stream_retry = None;
                    let _ = shared.events.try_send(EngineEvent::Error("Gave up reconnecting".into()));
                }
            }
        }
    } else if let Some(r) = &o.stream_retry {
        // Reconnected stream has been stable past its probation window.
        if Instant::now() >= r.at {
            o.stream_retry = None;
            let _ = shared.events.try_send(EngineEvent::Info("Stream reconnected".into()));
        }
    }
}

#[cfg(test)]
mod idle_tests {
    use super::*;
    use ferrous_core::{SceneCollection, SourceConfig};

    fn color() -> SourceConfig {
        SourceConfig::Color { bgra: [1, 2, 3, 255], width: 16, height: 16 }
    }

    /// A: item (visible), hidden item, nested scene B.  B: one item.  C: one item.
    fn collection() -> (SceneCollection, [u64; 5]) {
        let mut c = SceneCollection::default();
        let a = 0;
        let b = c.add_scene("B");
        let cc = c.add_scene("C");
        c.scenes[a].name = "A".into();
        let shown = c.add_item_to(a, "shown", color());
        let hidden = c.add_item_to(a, "hidden", color());
        c.scenes[a].items.iter_mut().find(|i| i.id == hidden).unwrap().visible = false;
        let inb = c.add_item_to(b, "inb", color());
        let nest = c.add_item_to(a, "nest", SourceConfig::Scene { name: "B".into() });
        let inc = c.add_item_to(cc, "inc", color());
        let _ = inb;
        (c, [shown, hidden, nest, inb, inc])
    }

    #[test]
    fn only_what_is_on_screen_is_shown() {
        let (c, [shown, hidden, nest, inb, inc]) = collection();
        let s = shown_items(&c, 0, None, None, &[]);
        assert!(s.contains(&shown) && s.contains(&nest) && s.contains(&inb), "{s:?}");
        assert!(!s.contains(&hidden) && !s.contains(&inc));
        // The studio preview scene and a Link scene target count too.
        let s = shown_items(&c, 0, Some(2), None, &[]);
        assert!(s.contains(&inc));
        let s = shown_items(&c, 0, None, None, &[ferrous_core::LinkSourceTarget::Scene { name: "C".into() }]);
        assert!(s.contains(&inc));
        // A scene that nests itself can't loop.
        let mut c2 = c.clone();
        c2.add_item_to(1, "self", SourceConfig::Scene { name: "B".into() });
        let _ = shown_items(&c2, 1, None, None, &[]);
    }

    /// A filtered source composes to the same picture whether its filters run as GPU passes or on the
    /// CPU (the GPU path is only taken on a real or forced GPU; skipped when there is none).
    #[test]
    fn filtered_sources_compose_the_same_on_gpu_and_cpu() {
        use ferrous_core::filters::{Filter, VideoFilter};
        std::env::set_var("FERROUS_FORCE_GPU", "1");
        let (mut gpu, err) = Renderer::new(128, 72, true);
        if err.is_some() || !gpu.is_gpu() {
            eprintln!("no GPU; skipping");
            return;
        }
        let (mut cpu, _) = Renderer::new(128, 72, false);
        let mut c = SceneCollection::default();
        c.scenes = vec![ferrous_core::Scene::new("A")];
        // Static sources, so both renderers see exactly the same frames: a skin-toned card through colour
        // correction, with a green card keyed out on top of it.
        let skin = c.add_item_to(0, "skin", SourceConfig::Color { bgra: [120, 150, 220, 255], width: 128, height: 72 });
        let green = c.add_item_to(0, "green", SourceConfig::Color { bgra: [30, 200, 40, 255], width: 64, height: 72 });
        for it in c.scenes[0].items.iter_mut() {
            if it.id == skin {
                it.video_filters = vec![Filter { enabled: true, filter: VideoFilter::ColorCorrection { gamma: 0.2, contrast: 0.2, brightness: 0.05, saturation: 0.5, hue_shift: 30.0, opacity: 0.9 } }];
            } else if it.id == green {
                it.video_filters = vec![Filter { enabled: true, filter: VideoFilter::ChromaKey { key_rgb: [0, 255, 0], similarity: 400, smoothness: 80, spill: 100 } }];
            }
        }
        let env = SourceEnv { fps: 60, ffmpeg: PathBuf::from("ffmpeg"), mixer: Arc::new(Mixer::default()) };
        let mut cache = HashMap::new();
        sync_sources(&mut cache, &c, &env, &shown_items(&c, 0, None, None, &[]));
        for it in c.scenes[0].items.clone() {
            for _ in 0..100 {
                if item_frame(&mut cache, &it).is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let a = compose_nested(&c.scenes[0], Some(&c), &mut cache, &mut gpu, 0);
        let b = compose_nested(&c.scenes[0], Some(&c), &mut cache, &mut cpu, 0);
        let worst = a.data.iter().zip(b.data.iter()).map(|(x, y)| (*x as i32 - *y as i32).abs()).max().unwrap();
        assert!(worst <= 4, "GPU and CPU filtered scenes differ by {worst}");
    }

    #[test]
    fn a_clone_shares_its_originals_picture_and_keeps_it_running() {
        let mut c = SceneCollection::default();
        c.scenes = vec![ferrous_core::Scene::new("A"), ferrous_core::Scene::new("B")];
        let orig = c.add_item_to(0, "cam", SourceConfig::TestPattern { width: 64, height: 36 });
        let copy = c.add_item_to(1, "cam (clone)", SourceConfig::Clone { source: orig });
        let again = c.add_item_to(1, "clone of clone", SourceConfig::Clone { source: copy });
        let me = c.add_item_to(1, "self", SourceConfig::Clone { source: 0 });
        c.scenes[1].items.iter_mut().find(|i| i.id == me).unwrap().source = SourceConfig::Clone { source: me };

        // Only scene B is on screen, yet the original (in A) must run.
        let on_screen = shown_items(&c, 1, None, None, &[]);
        assert!(on_screen.contains(&orig));

        let env = SourceEnv { fps: 60, ffmpeg: PathBuf::from("ffmpeg"), mixer: Arc::new(Mixer::default()) };
        let mut cache = HashMap::new();
        sync_sources(&mut cache, &c, &env, &on_screen);
        assert!(cache[&orig].source.is_some());
        assert!(cache[&copy].source.is_none(), "a clone never captures");

        let item = |id: u64| c.scenes.iter().flat_map(|s| &s.items).find(|i| i.id == id).unwrap().clone();
        let mut frame = None;
        for _ in 0..100 {
            frame = item_frame(&mut cache, &item(copy));
            if frame.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let f = frame.expect("the clone shows the original's frame");
        assert_eq!((f.width, f.height), (64, 36));
        assert!(item_frame(&mut cache, &item(again)).is_none(), "a clone of a clone shows nothing");
        assert!(item_frame(&mut cache, &item(me)).is_none(), "a self-clone shows nothing");
    }

    #[test]
    fn unseen_sources_neither_start_nor_survive_the_grace_period() {
        let (c, [shown, hidden, ..]) = collection();
        let env = SourceEnv { fps: 60, ffmpeg: PathBuf::from("ffmpeg"), mixer: Arc::new(Mixer::default()) };
        let mut cache = HashMap::new();
        let on_screen = shown_items(&c, 0, None, None, &[]);
        sync_sources(&mut cache, &c, &env, &on_screen);
        assert!(cache[&shown].source.is_some(), "a shown source starts");
        assert!(cache[&hidden].source.is_none(), "a hidden one doesn't");

        // Showing it starts it; hiding it again keeps it through the grace
        // period, then stops it.
        let all: HashSet<u64> = cache.keys().copied().collect();
        sync_sources(&mut cache, &c, &env, &all);
        assert!(cache[&hidden].source.is_some());
        sync_sources(&mut cache, &c, &env, &on_screen);
        assert!(cache[&hidden].source.is_some(), "kept during the grace period");
        cache.get_mut(&hidden).unwrap().idle_since = Some(Instant::now() - IDLE_GRACE - Duration::from_secs(1));
        sync_sources(&mut cache, &c, &env, &on_screen);
        assert!(cache[&hidden].source.is_none(), "stopped once idle for long enough");

        // "Keep running" exempts an item.
        let mut c = c;
        c.scenes[0].items.iter_mut().find(|i| i.id == hidden).unwrap().keep_running = true;
        cache.get_mut(&hidden).unwrap().last_attempt = Instant::now() - Duration::from_secs(60); // past the retry throttle
        sync_sources(&mut cache, &c, &env, &on_screen);
        assert!(cache[&hidden].source.is_some());
    }
}
