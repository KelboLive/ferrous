use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateControl {
    /// Constant bitrate: what streaming services expect.
    Cbr,
    /// Variable bitrate with a peak cap.
    Vbr,
    /// Constant quality (CRF for x264/x265/SVT-AV1, CQ for NVENC, ICQ for QSV).
    Quality,
    /// Constant quantizer.
    Cqp,
}

impl RateControl {
    pub const ALL: [RateControl; 4] = [Self::Cbr, Self::Vbr, Self::Quality, Self::Cqp];
    pub fn label(self) -> &'static str {
        match self {
            Self::Cbr => "CBR",
            Self::Vbr => "VBR",
            Self::Quality => "Constant quality (CRF/CQ)",
            Self::Cqp => "CQP",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EncoderSettings {
    /// FFmpeg encoder name, e.g. `h264_nvenc`, `libx264`, `av1_qsv`.
    pub encoder: String,
    pub rate_control: RateControl,
    pub bitrate_kbps: u32,
    /// CRF / CQ / QP value, depending on the rate control.
    pub quality: u32,
    /// Encoder-specific preset name. Empty = encoder default.
    pub preset: String,
    pub keyframe_interval_secs: u32,
    pub b_frames: u32,
    /// Extra raw FFmpeg arguments appended after the generated ones.
    #[serde(default)]
    pub custom_args: String,
    /// Output HDR10 (10-bit, BT.2020, PQ). Only takes effect with HEVC or
    /// AV1 encoders; SDR sources are mapped with SDR white at
    /// `hdr_white_nits`.
    #[serde(default)]
    pub hdr: bool,
    /// Where SDR white lands in HDR output (203 is the broadcast reference).
    #[serde(default = "default_white")]
    pub hdr_white_nits: u32,
}

fn default_white() -> u32 {
    203
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self::streaming_default()
    }
}

impl EncoderSettings {
    pub fn streaming_default() -> Self {
        Self {
            encoder: "libx264".into(),
            rate_control: RateControl::Cbr,
            bitrate_kbps: 6000,
            quality: 23,
            preset: "veryfast".into(),
            keyframe_interval_secs: 2,
            b_frames: 2,
            custom_args: String::new(),
            hdr: false,
            hdr_white_nits: 203,
        }
    }

    pub fn recording_default() -> Self {
        Self {
            rate_control: RateControl::Quality,
            bitrate_kbps: 40000,
            quality: 20,
            keyframe_interval_secs: 2,
            ..Self::streaming_default()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Aac,
    Opus,
    Flac,
    Pcm,
}

impl AudioCodec {
    pub const ALL: [AudioCodec; 4] = [Self::Aac, Self::Opus, Self::Flac, Self::Pcm];
    pub fn label(self) -> &'static str {
        match self {
            Self::Aac => "AAC",
            Self::Opus => "Opus",
            Self::Flac => "FLAC (lossless)",
            Self::Pcm => "PCM (uncompressed)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordingFormat {
    Mkv,
    Mp4,
    FragmentedMp4,
    Mov,
    MpegTs,
    Flv,
}

impl RecordingFormat {
    pub const ALL: [RecordingFormat; 6] =
        [Self::Mkv, Self::Mp4, Self::FragmentedMp4, Self::Mov, Self::MpegTs, Self::Flv];
    pub fn label(self) -> &'static str {
        match self {
            Self::Mkv => "Matroska (.mkv), crash-safe",
            Self::Mp4 => "MPEG-4 (.mp4)",
            Self::FragmentedMp4 => "Fragmented MP4 (.mp4), crash-safe",
            Self::Mov => "QuickTime (.mov)",
            Self::MpegTs => "MPEG-TS (.ts)",
            Self::Flv => "FLV (.flv)",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mkv => "mkv",
            Self::Mp4 | Self::FragmentedMp4 => "mp4",
            Self::Mov => "mov",
            Self::MpegTs => "ts",
            Self::Flv => "flv",
        }
    }
    pub fn ffmpeg_muxer(self) -> &'static str {
        match self {
            Self::Mkv => "matroska",
            Self::Mp4 | Self::FragmentedMp4 => "mp4",
            Self::Mov => "mov",
            Self::MpegTs => "mpegts",
            Self::Flv => "flv",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RendererMode {
    /// GPU when an adapter is available, otherwise CPU.
    #[default]
    Auto,
    Cpu,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    /// Canvas the scene is composed on.
    pub base_width: u32,
    pub base_height: u32,
    /// Scaled resolution sent to encoders.
    pub output_width: u32,
    pub output_height: u32,
    pub fps: u32,
    #[serde(default)]
    pub renderer: RendererMode,
    /// Second canvas for vertical platforms (TikTok, Shorts, Reels). `None` = off.
    #[serde(default)]
    pub vertical: Option<(u32, u32)>,
    /// Where the canvas is scaled and converted to the encoder's YUV format.
    #[serde(default)]
    pub conversion: ConversionMode,
    /// Read GPU results a frame late instead of waiting for them: smoother
    /// frame times, one frame (16 ms at 60 fps) more latency.
    #[serde(default = "yes")]
    pub pipelined_readback: bool,
    /// Ask Windows to schedule Ferrous's GPU work ahead of other programs'
    /// (a game's), which cuts rendering lag when the game maxes the GPU.
    /// Works best when Ferrous runs as administrator.
    #[serde(default)]
    pub gpu_priority: bool,
}

fn yes() -> bool {
    true
}

/// Where the composed canvas becomes the encoder's input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ConversionMode {
    /// Ferrous when the frame rate would strain the pipe (above 1080p60) or
    /// there are cores to spare (8 or more), FFmpeg otherwise.
    #[default]
    Auto,
    /// In Ferrous, multi-threaded, before the pipe (1.5 bytes/pixel to FFmpeg).
    #[serde(alias = "Ember")]
    Ferrous,
    /// Inside FFmpeg (BGRA over the pipe, FFmpeg's Lanczos scaler).
    Ffmpeg,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            base_width: 1920,
            base_height: 1080,
            output_width: 1920,
            output_height: 1080,
            fps: 60,
            renderer: RendererMode::Auto,
            vertical: None,
            conversion: ConversionMode::Auto,
            pipelined_readback: true,
            gpu_priority: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamSettings {
    pub service: String,
    /// e.g. `rtmp://live.twitch.tv/app`, `srt://host:port`, or a WHIP URL.
    pub server: String,
    pub key: String,
    pub encoder: EncoderSettings,
    pub audio_bitrate_kbps: u32,
    /// Which mixer track (0-based) is streamed.
    #[serde(default)]
    pub audio_track: usize,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            service: "Twitch".into(),
            server: "rtmp://live.twitch.tv/app".into(),
            key: String::new(),
            encoder: EncoderSettings::streaming_default(),
            audio_bitrate_kbps: 160,
            audio_track: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordSettings {
    pub folder: PathBuf,
    pub format: RecordingFormat,
    /// Reuse the streaming encoder settings instead of `encoder`.
    pub same_as_stream: bool,
    pub encoder: EncoderSettings,
    pub audio_codec: AudioCodec,
    pub audio_bitrate_kbps: u32,
    /// chrono format string for the file name.
    pub filename_format: String,
    /// Audio tracks written to the file (bit 0 = track 1). MKV/MOV/MP4 support several.
    #[serde(default = "track1")]
    pub audio_tracks: u32,
    /// Record at this size instead of the stream's output size (e.g. stream
    /// 1080p, record at the full canvas resolution). `None` = same as output.
    #[serde(default)]
    pub output_size: Option<(u32, u32)>,
}

impl Default for RecordSettings {
    fn default() -> Self {
        Self {
            folder: crate::paths::default_recording_dir(),
            format: RecordingFormat::Mkv,
            same_as_stream: false,
            encoder: EncoderSettings::recording_default(),
            audio_codec: AudioCodec::Aac,
            audio_bitrate_kbps: 192,
            filename_format: "%Y-%m-%d %H-%M-%S".into(),
            audio_tracks: 1,
            output_size: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// Output device to capture via WASAPI loopback. `None` = system default.
    pub desktop_device: Option<String>,
    pub desktop_enabled: bool,
    /// Input device (microphone). `None` = system default.
    pub mic_device: Option<String>,
    pub mic_enabled: bool,
    /// Linear gain, 0.0 to 2.0.
    pub desktop_volume: f32,
    pub mic_volume: f32,
    #[serde(default)]
    pub desktop_muted: bool,
    #[serde(default)]
    pub mic_muted: bool,
    #[serde(default)]
    pub desktop_filters: crate::filters::AudioFilters,
    #[serde(default)]
    pub mic_filters: crate::filters::AudioFilters,
    #[serde(default = "track1")]
    pub desktop_tracks: u32,
    #[serde(default = "track1")]
    pub mic_tracks: u32,
    /// 0 off, 1 monitor only, 2 monitor and output.
    #[serde(default)]
    pub desktop_monitor: u8,
    #[serde(default)]
    pub mic_monitor: u8,
    /// Sync offsets in milliseconds (hold the channel back).
    #[serde(default)]
    pub desktop_delay_ms: u32,
    #[serde(default)]
    pub mic_delay_ms: u32,
    /// Output device for audio monitoring. `None` = system default.
    #[serde(default)]
    pub monitor_device: Option<String>,
}

fn track1() -> u32 {
    1
}

fn default_replay_secs() -> u32 {
    30
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplaySettings {
    /// How much history to keep.
    #[serde(default = "default_replay_secs")]
    pub seconds: u32,
    /// Saved clip container.
    pub format: RecordingFormat,
    /// Also keep a vertical-canvas buffer and save a ready-to-post vertical clip.
    #[serde(default)]
    pub vertical_too: bool,
}

impl Default for ReplaySettings {
    fn default() -> Self {
        Self { seconds: 30, format: RecordingFormat::Mp4, vertical_too: false }
    }
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            desktop_device: None,
            desktop_enabled: true,
            mic_device: None,
            mic_enabled: true,
            desktop_volume: 1.0,
            mic_volume: 1.0,
            desktop_muted: false,
            mic_muted: false,
            desktop_filters: Vec::new(),
            mic_filters: Vec::new(),
            desktop_tracks: 1,
            mic_tracks: 1,
            desktop_monitor: 0,
            mic_monitor: 0,
            desktop_delay_ms: 0,
            mic_delay_ms: 0,
            monitor_device: None,
        }
    }
}

/// What a global hotkey does.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HotkeyAction {
    ToggleRecording,
    ToggleStreaming,
    ToggleReplayBuffer,
    SaveReplay,
    /// Switch the program scene (or the preview scene in studio mode).
    SwitchScene { scene: String },
    /// Studio mode: send preview to program.
    Transition,
    ToggleSourceVisibility { scene: String, source: String },
    /// Mixer channel name, e.g. "Mic/Aux".
    ToggleMute { channel: String },
}

impl HotkeyAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ToggleRecording => "Start/Stop Recording",
            Self::ToggleStreaming => "Start/Stop Streaming",
            Self::ToggleReplayBuffer => "Start/Stop Replay Buffer",
            Self::SaveReplay => "Save Replay",
            Self::SwitchScene { .. } => "Switch to Scene",
            Self::Transition => "Studio Mode Transition",
            Self::ToggleSourceVisibility { .. } => "Show/Hide Source",
            Self::ToggleMute { .. } => "Mute/Unmute",
        }
    }

    pub fn all() -> Vec<HotkeyAction> {
        vec![
            Self::ToggleRecording,
            Self::ToggleStreaming,
            Self::ToggleReplayBuffer,
            Self::SaveReplay,
            Self::SwitchScene { scene: String::new() },
            Self::Transition,
            Self::ToggleSourceVisibility { scene: String::new(), source: String::new() },
            Self::ToggleMute { channel: "Mic/Aux".into() },
        ]
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hotkey {
    /// e.g. "Ctrl+Shift+R", "F9".
    pub keys: String,
    #[serde(flatten)]
    pub action: HotkeyAction,
}

/// A named set of recording settings: encoder, container, audio and
/// recording size. Applied to [`RecordSettings`] in one click.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordingPreset {
    pub name: String,
    pub encoder: EncoderSettings,
    pub format: RecordingFormat,
    pub audio_codec: AudioCodec,
    pub audio_bitrate_kbps: u32,
    #[serde(default)]
    pub output_size: Option<(u32, u32)>,
}

impl RecordingPreset {
    /// Snapshot of the current recording settings under `name`.
    pub fn from_record(name: impl Into<String>, r: &RecordSettings, fallback_encoder: &EncoderSettings) -> Self {
        let encoder = if r.same_as_stream { fallback_encoder.clone() } else { r.encoder.clone() };
        Self { name: name.into(), encoder, format: r.format, audio_codec: r.audio_codec, audio_bitrate_kbps: r.audio_bitrate_kbps, output_size: r.output_size }
    }

    /// Writes this preset into `r` (folder, file name and tracks are kept).
    pub fn apply_to(&self, r: &mut RecordSettings) {
        r.same_as_stream = false;
        r.encoder = self.encoder.clone();
        r.format = self.format;
        r.audio_codec = self.audio_codec;
        r.audio_bitrate_kbps = self.audio_bitrate_kbps;
        r.output_size = self.output_size;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub video: VideoSettings,
    pub stream: StreamSettings,
    pub record: RecordSettings,
    pub audio: AudioSettings,
    /// Explicit path to ffmpeg.exe. Empty = next to the app, then PATH.
    #[serde(default)]
    pub ffmpeg_path: String,
    #[serde(default)]
    pub replay: ReplaySettings,
    #[serde(default)]
    pub hotkeys: Vec<Hotkey>,
    #[serde(default)]
    pub remote: RemoteSettings,
    /// Extra stream destinations (multistreaming, vertical platforms).
    #[serde(default)]
    pub destinations: Vec<StreamDestination>,
    /// Ferrous Link outputs (send video/audio to other Ferrous instances).
    #[serde(default)]
    pub links: Vec<LinkOutputSettings>,
    #[serde(default)]
    pub ui: UiSettings,
    /// User-saved recording presets (the built-in ones are generated).
    #[serde(default)]
    pub recording_presets: Vec<RecordingPreset>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DockState {
    Docked,
    Floating,
    Hidden,
}

/// Which docks are shown and how, plus stats-dock preferences.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    /// Dock id -> state. Missing ids use their default.
    pub docks: std::collections::BTreeMap<String, DockState>,
    /// Stats the user turned off in the Stats dock.
    pub hidden_stats: Vec<String>,
    /// Source types added most recently (newest first), for the Add Source window.
    pub recent_sources: Vec<String>,
    /// Performance Monitor: only list sources in the current scene.
    pub perf_current_scene_only: bool,
    /// The first-run setup (benchmark) has been completed or skipped.
    pub setup_done: bool,
    /// The current dock layout (serialized dock tree; owned by the UI).
    pub layout: Option<String>,
    /// Layouts the user saved by name.
    pub saved_layouts: std::collections::BTreeMap<String, String>,
    /// How drop targets are shown while dragging a dock.
    pub snap_guides: SnapGuides,
    /// Docks can't be dragged, closed or resized by accident.
    pub lock_layout: bool,
    /// Audio Mixer in OBS's vertical layout (strips side by side).
    pub mixer_vertical: bool,
    /// User-defined docks that show a web page (chat, alerts, activity feeds).
    pub custom_docks: Vec<CustomDock>,
    /// Show the live preview. Off saves the CPU/GPU spent drawing it;
    /// streaming and recording are unaffected.
    pub preview_enabled: bool,
    /// Preview frame-rate cap; 0 = the canvas frame rate.
    pub preview_max_fps: u32,
    /// One-time layout updates already applied to `layout` (1: the
    /// Properties dock closed when properties moved to the right-click menu).
    pub layout_rev: u32,
    /// Colour theme id (see the app's palettes); unknown ids fall back to Ferrous.
    pub theme: String,
    /// Glass panels on themes that have a backdrop: 0 off, 1 light, 2 full.
    pub glass: u8,
    /// Animated theme backgrounds: 0 off, 1 reduced, 2 full.
    pub motion: u8,
}

/// A custom browser dock: a named dock showing a web page, like OBS's
/// "Custom Browser Docks". `id` is stable, so the layout keeps its place
/// when the dock is renamed or its URL changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomDock {
    pub id: u32,
    pub name: String,
    pub url: String,
}

/// How drop targets are shown while dragging a dock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SnapGuides {
    /// The half of the panel the dock would take lights up as you approach an edge.
    #[default]
    Zones,
    /// A compass of drop buttons appears in the panel under the pointer (Visual Studio style).
    Compass,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self { docks: Default::default(), hidden_stats: Vec::new(), recent_sources: Vec::new(), perf_current_scene_only: false, setup_done: false, layout: None, saved_layouts: Default::default(), snap_guides: SnapGuides::Zones, lock_layout: false, mixer_vertical: false, custom_docks: Vec::new(), preview_enabled: true, preview_max_fps: 0, layout_rev: 0, theme: "ember".into(), glass: 2, motion: 1 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "codec", rename_all = "snake_case")]
pub enum LinkCodec {
    /// Parallel JPEG: lowest latency, moderate bandwidth. `adaptive` lowers
    /// quality automatically when a receiver can't keep up.
    Jpeg { quality: u8, adaptive: bool },
    /// Exact pixels including alpha (LZ4 + frame deltas): best for overlays.
    Lossless,
    /// FFmpeg encoder (e.g. h264_nvenc): lowest bandwidth, a little more latency.
    Hardware { encoder: String, bitrate_kbps: u32 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LinkSourceTarget {
    Program,
    Scene { name: String },
    Source { name: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkOutputSettings {
    pub enabled: bool,
    /// Shown to receivers.
    pub name: String,
    pub target: LinkSourceTarget,
    pub codec: LinkCodec,
    /// 0 = canvas size.
    pub width: u32,
    pub height: u32,
    /// 0 = canvas frame rate.
    pub fps: u32,
    /// 0 = default (47778, or the next free port).
    pub port: u16,
    /// Empty = open link; otherwise receivers need this PIN and traffic is encrypted.
    pub pin: String,
    pub audio: bool,
    /// Mixer track (0-based) sent with the video.
    pub audio_track: usize,
}

impl Default for LinkOutputSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            name: "Ferrous Link".into(),
            target: LinkSourceTarget::Program,
            codec: LinkCodec::Jpeg { quality: 92, adaptive: true },
            width: 0,
            height: 0,
            fps: 0,
            port: 0,
            pin: String::new(),
            audio: true,
            audio_track: 0,
        }
    }
}

/// The obs-websocket-compatible remote control server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteSettings {
    pub enabled: bool,
    pub port: u16,
    /// Empty = no authentication (not recommended on shared networks).
    pub password: String,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self { enabled: false, port: 4455, password: String::new() }
    }
}

impl Settings {
    pub fn default_path() -> PathBuf {
        crate::paths::config_dir().join("settings.json")
    }

    /// Loads settings, falling back to defaults (and logging) on any error so
    /// a corrupt file never prevents the app from starting.
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("settings file is invalid ({e}); using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Encoder settings that recording will actually use.
    pub fn effective_record_encoder(&self) -> &EncoderSettings {
        if self.record.same_as_stream {
            &self.stream.encoder
        } else {
            &self.record.encoder
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Canvas {
    #[default]
    Main,
    Vertical,
}

/// An additional stream destination (multistreaming). Each has its own
/// canvas, encoder and bitrate and starts together with the main stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamDestination {
    pub enabled: bool,
    pub name: String,
    pub server: String,
    pub key: String,
    /// Ask for server/key every time (platforms that issue per-session keys,
    /// such as TikTok LIVE). Nothing is saved when set.
    pub ask_each_time: bool,
    pub canvas: Canvas,
    /// 0 = canvas size.
    pub output_width: u32,
    pub output_height: u32,
    pub encoder: EncoderSettings,
    pub audio_bitrate_kbps: u32,
    pub audio_track: usize,
}

impl Default for StreamDestination {
    fn default() -> Self {
        Self {
            enabled: true,
            name: "Destination".into(),
            server: String::new(),
            key: String::new(),
            ask_each_time: false,
            canvas: Canvas::Main,
            output_width: 0,
            output_height: 0,
            encoder: EncoderSettings::streaming_default(),
            audio_bitrate_kbps: 160,
            audio_track: 0,
        }
    }
}

impl StreamDestination {
    /// TikTok LIVE preset: vertical canvas, per-session key, tuned H.264.
    /// TikTok issues a new server URL and key for each LIVE session; you
    /// paste them when you go live.
    pub fn tiktok(encoder: &str) -> Self {
        Self {
            name: "TikTok LIVE".into(),
            ask_each_time: true,
            canvas: Canvas::Vertical,
            output_width: 1080,
            output_height: 1920,
            encoder: EncoderSettings {
                encoder: encoder.to_string(),
                rate_control: RateControl::Cbr,
                bitrate_kbps: 6000,
                keyframe_interval_secs: 2,
                b_frames: 0,
                preset: String::new(),
                ..EncoderSettings::streaming_default()
            },
            audio_bitrate_kbps: 128,
            ..Self::default()
        }
    }
}

/// Common streaming destinations, pre-filled in the settings dialog.
pub const STREAM_SERVICES: &[(&str, &str)] = &[
    ("Twitch", "rtmp://live.twitch.tv/app"),
    ("YouTube (RTMPS)", "rtmps://a.rtmps.youtube.com:443/live2"),
    ("YouTube (RTMP)", "rtmp://a.rtmp.youtube.com/live2"),
    ("Facebook Live", "rtmps://live-api-s.facebook.com:443/rtmp"),
    // TikTok issues a server URL per LIVE session; paste it when going live.
    ("TikTok LIVE", ""),
    ("Custom", ""),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Old or hand-edited files with missing fields keep everything they do set.
    #[test]
    fn partial_settings_merge_with_defaults() {
        let s: Settings = serde_json::from_str(r#"{"remote":{"enabled":true,"port":4460},"video":{"fps":30}}"#).unwrap();
        assert!(s.remote.enabled);
        assert_eq!(s.remote.port, 4460);
        assert_eq!(s.video.fps, 30);
        assert_eq!(s.video.base_width, 1920);
        assert_eq!(s.stream.encoder.encoder, "libx264");
    }
}
