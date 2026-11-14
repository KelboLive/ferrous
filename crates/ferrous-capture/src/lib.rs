//! Video sources.
//!
//! Every source exposes its most recent frame; the engine samples sources on
//! its own clock, so capture rate and output rate are independent (as in OBS).

mod basic;
pub mod ffsource;
pub mod text;
pub mod obs;
pub mod browser;
#[cfg(windows)]
mod appaudio;
#[cfg(windows)]
mod gpu_ring;
#[cfg(windows)]
mod hdr;
#[cfg(windows)]
mod wgc;

use std::path::PathBuf;
use std::sync::Arc;

use ferrous_core::{SourceConfig, VideoFrame};

pub use basic::{ColorSource, ImageSource, TestPatternSource};
pub use ffsource::{list_audio_devices, list_video_devices, FfmpegSource};

/// Video and audio from another Ferrous (Ferrous Link). Reconnects by itself.
pub struct LinkSource(ferrous_link::LinkReceiver);

impl Source for LinkSource {
    fn latest_frame(&mut self) -> Option<VideoFrame> {
        self.0.latest_frame()
    }
}

/// A video source implemented by a plugin DLL.
pub struct PluginSource(ferrous_plugins::Instance);

impl Source for PluginSource {
    fn latest_frame(&mut self) -> Option<VideoFrame> {
        self.0.render_source()
    }
}

/// Receives a source's own audio as interleaved 48 kHz stereo f32.
pub type AudioSink = Arc<dyn Fn(&[f32]) + Send + Sync>;

/// What a source may need from the outside world.
#[derive(Clone)]
pub struct SourceContext {
    pub ffmpeg: PathBuf,
    pub audio: Option<AudioSink>,
    /// The scene item this source is for (0 if none).
    pub item_id: u64,
    /// The canvas frame rate (0 if unknown): capture needn't run faster.
    pub fps: u32,
}

static GPU_CAPTURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turns GPU-resident capture on for sources created from now on (Windows display, window and game
/// capture; off by default). Harmless elsewhere.
pub fn set_gpu_capture(on: bool) {
    GPU_CAPTURE.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn gpu_capture() -> bool {
    GPU_CAPTURE.load(std::sync::atomic::Ordering::Relaxed)
}

pub trait Source: Send {
    /// Latest frame, or `None` if nothing has arrived yet.
    fn latest_frame(&mut self) -> Option<VideoFrame>;
    /// Latest frame as a shared GPU texture (see [`ferrous_core::SharedFrame`]), for sources that can
    /// hand one over. Asking for it instead of [`Source::latest_frame`] is what saves the CPU copy.
    fn latest_gpu_frame(&mut self) -> Option<VideoFrame> {
        None
    }
    /// False once the source has died (e.g. captured window closed); the
    /// engine will try to recreate it.
    fn is_alive(&self) -> bool {
        true
    }
    /// Helper processes this source runs (for per-source CPU/GPU accounting).
    fn pids(&self) -> Vec<u32> {
        Vec::new()
    }
    /// Applies changed settings in place (no restart). Returns false if the
    /// source must be recreated instead.
    fn update(&mut self, _config: &SourceConfig) -> bool {
        false
    }
}

pub fn create_source(config: &SourceConfig, ctx: &SourceContext) -> anyhow::Result<Box<dyn Source>> {
    Ok(match config {
        SourceConfig::Text { text, from_file, font, size, bgra, outline, background, align, wrap_width } => {
            Box::new(text::TextSource::new(
                text,
                from_file.clone(),
                font,
                text::TextStyleConfig {
                    size: *size,
                    bgra: *bgra,
                    outline: *outline,
                    background: *background,
                    align: *align,
                    wrap_width: *wrap_width,
                },
            )?)
        }
        SourceConfig::Browser { url, width, height, fps, css } => {
            Box::new(browser::BrowserSource::start(url, *width, *height, *fps, css)?)
        }
        SourceConfig::LinkReceiver { sender, pin } => {
            let sink: Option<ferrous_link::receiver::AudioSink> = ctx.audio.clone().map(|a| {
                let f: ferrous_link::receiver::AudioSink = a;
                f
            });
            Box::new(LinkSource(ferrous_link::LinkReceiver::start(sender, pin, ctx.ffmpeg.clone(), sink)))
        }
        SourceConfig::ObsPlugin { type_id, settings, .. } => Box::new(obs::ObsPluginSource::start(type_id, settings, ctx.audio.clone(), ctx.item_id)?),
        SourceConfig::Plugin { plugin, settings } => Box::new(PluginSource(ferrous_plugins::Instance::new(
            plugin,
            settings,
            ferrous_plugins::Kind::VideoSource,
        )?)),
        // Nested scenes are composed by the engine, not created here.
        SourceConfig::Scene { .. } | SourceConfig::Clone { .. } => anyhow::bail!("scene and clone sources are rendered by the engine"),
        SourceConfig::Media { input, looping, hw_decode } => {
            if input.trim().is_empty() {
                anyhow::bail!("no media file selected");
            }
            Box::new(FfmpegSource::start(
                &ctx.ffmpeg,
                ffsource::Input::Media { input, looping: *looping, hw_decode: *hw_decode },
                ctx.audio.clone(),
            )?)
        }
        #[cfg(windows)]
        SourceConfig::ApplicationAudio { process } => {
            let sink = ctx.audio.clone().ok_or_else(|| anyhow::anyhow!("no audio mixer"))?;
            Box::new(appaudio::AppAudioSource::start(process, sink)?)
        }
        #[cfg(not(windows))]
        SourceConfig::ApplicationAudio { .. } => anyhow::bail!("application audio capture is only available on Windows"),
        SourceConfig::VideoDevice { device, resolution, fps, audio_device } => {
            if device.trim().is_empty() {
                anyhow::bail!("no device selected");
            }
            Box::new(FfmpegSource::start(
                &ctx.ffmpeg,
                ffsource::Input::Device {
                    device,
                    resolution: *resolution,
                    fps: *fps,
                    audio_device: audio_device.as_deref(),
                },
                ctx.audio.clone(),
            )?)
        }
        SourceConfig::Color { bgra, width, height } => Box::new(ColorSource::new(*width, *height, *bgra)),
        SourceConfig::Image { path } => Box::new(ImageSource::open(path)?),
        SourceConfig::TestPattern { width, height } => Box::new(TestPatternSource::new(*width, *height)),
        #[cfg(windows)]
        SourceConfig::DisplayCapture { monitor_index, capture_cursor } => {
            Box::new(wgc::WgcSource::monitor(*monitor_index, *capture_cursor, ctx.fps)?)
        }
        #[cfg(windows)]
        SourceConfig::WindowCapture { title_contains, capture_cursor } => {
            Box::new(wgc::WgcSource::window(title_contains, *capture_cursor, ctx.fps)?)
        }
        #[cfg(windows)]
        SourceConfig::GameCapture { program, capture_cursor } => Box::new(wgc::GameSource::new(program, *capture_cursor, ctx.fps)),
        #[cfg(not(windows))]
        SourceConfig::DisplayCapture { .. } | SourceConfig::WindowCapture { .. } | SourceConfig::GameCapture { .. } => {
            anyhow::bail!("screen and window capture are only available on Windows")
        }
    })
}

#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub title: String,
    pub process: String,
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    #[cfg(windows)]
    {
        wgc::list_monitors()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Process names of applications with windows, for Application Audio.
pub fn list_audio_apps() -> Vec<String> {
    let mut v: Vec<String> = list_windows().into_iter().map(|w| w.process).filter(|p| !p.is_empty()).collect();
    v.sort();
    v.dedup();
    v
}

pub fn list_windows() -> Vec<WindowInfo> {
    #[cfg(windows)]
    {
        wgc::list_windows()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}
