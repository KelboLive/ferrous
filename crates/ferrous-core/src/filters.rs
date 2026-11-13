//! Filter settings (what's saved). Processing lives in `ferrous-audio` and
//! `ferrous-render`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AudioFilter {
    Gain { db: f32 },
    /// RNNoise-based suppression (same model OBS uses for "RNNoise").
    NoiseSuppression,
    NoiseGate { open_db: f32, close_db: f32, attack_ms: f32, hold_ms: f32, release_ms: f32 },
    Compressor { ratio: f32, threshold_db: f32, attack_ms: f32, release_ms: f32, makeup_db: f32 },
    Limiter { threshold_db: f32, release_ms: f32 },
    /// From a plugin DLL; `settings` is a JSON object.
    Plugin { plugin: String, #[serde(default)] settings: String },
    /// A VST3 effect: bundle path, class id (hex), display name, and the
    /// plugin's saved state (opaque, base64).
    Vst3 { path: String, cid: String, name: String, #[serde(default)] state: String },
}

impl AudioFilter {
    /// Same plugin instance (VST3 state edits don't count as a different filter).
    pub fn same_instance(&self, other: &AudioFilter) -> bool {
        match (self, other) {
            (Self::Vst3 { path: a, cid: b, .. }, Self::Vst3 { path: c, cid: d, .. }) => a == c && b == d,
            _ => self == other,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Gain { .. } => "Gain",
            Self::NoiseSuppression => "Noise Suppression",
            Self::NoiseGate { .. } => "Noise Gate",
            Self::Compressor { .. } => "Compressor",
            Self::Limiter { .. } => "Limiter",
            Self::Plugin { .. } => "Plugin Filter",
            Self::Vst3 { .. } => "VST3 Plugin",
        }
    }

    /// Defaults match OBS's.
    pub fn defaults() -> Vec<AudioFilter> {
        vec![
            Self::NoiseSuppression,
            Self::NoiseGate { open_db: -26.0, close_db: -32.0, attack_ms: 25.0, hold_ms: 200.0, release_ms: 150.0 },
            Self::Compressor { ratio: 10.0, threshold_db: -18.0, attack_ms: 6.0, release_ms: 60.0, makeup_db: 0.0 },
            Self::Limiter { threshold_db: -6.0, release_ms: 60.0 },
            Self::Gain { db: 0.0 },
        ]
    }
}

fn bg_threshold() -> f32 {
    0.5
}
fn bg_softness() -> f32 {
    0.12
}
fn bg_smoothing() -> f32 {
    0.5
}
fn bg_fps() -> u32 {
    20
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VideoFilter {
    /// Removes a key color. `similarity`/`smoothness` use OBS's 1–1000 scale.
    ChromaKey { key_rgb: [u8; 3], similarity: u32, smoothness: u32, spill: u32 },
    ColorCorrection {
        gamma: f32,
        contrast: f32,
        brightness: f32,
        saturation: f32,
        hue_shift: f32,
        opacity: f32,
    },
    /// Scrolls the source, wrapping around, in pixels per second.
    Scroll { speed_x: f32, speed_y: f32 },
    /// Applies a `.cube` 3D LUT.
    Lut { path: PathBuf, amount: f32 },
    /// Keeps the person, removes everything behind them (no green screen).
    /// `threshold` (0–1) is how sure the model must be, `softness` feathers
    /// the edge, `smoothing` steadies the mask over time, and `max_fps`
    /// caps how often the model runs (the mask follows between runs).
    BackgroundRemoval {
        #[serde(default = "bg_threshold")]
        threshold: f32,
        #[serde(default = "bg_softness")]
        softness: f32,
        #[serde(default = "bg_smoothing")]
        smoothing: f32,
        #[serde(default = "bg_fps")]
        max_fps: u32,
    },
    /// From a plugin DLL; `settings` is a JSON object.
    Plugin { plugin: String, #[serde(default)] settings: String },
}

impl VideoFilter {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ChromaKey { .. } => "Chroma Key",
            Self::ColorCorrection { .. } => "Color Correction",
            Self::Scroll { .. } => "Scroll",
            Self::Lut { .. } => "Apply LUT",
            Self::BackgroundRemoval { .. } => "Background Removal",
            Self::Plugin { .. } => "Plugin Filter",
        }
    }

    pub fn defaults() -> Vec<VideoFilter> {
        vec![
            Self::ChromaKey { key_rgb: [0, 255, 0], similarity: 400, smoothness: 80, spill: 100 },
            Self::ColorCorrection {
                gamma: 0.0,
                contrast: 0.0,
                brightness: 0.0,
                saturation: 0.0,
                hue_shift: 0.0,
                opacity: 1.0,
            },
            Self::Scroll { speed_x: 50.0, speed_y: 0.0 },
            Self::Lut { path: PathBuf::new(), amount: 1.0 },
            Self::BackgroundRemoval { threshold: bg_threshold(), softness: bg_softness(), smoothing: bg_smoothing(), max_fps: bg_fps() },
        ]
    }
}

/// A filter plus its on/off switch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Filter<T> {
    pub enabled: bool,
    #[serde(flatten)]
    pub filter: T,
}

impl<T> Filter<T> {
    pub fn new(filter: T) -> Self {
        Self { enabled: true, filter }
    }
}

pub type AudioFilters = Vec<Filter<AudioFilter>>;
pub type VideoFilters = Vec<Filter<VideoFilter>>;
