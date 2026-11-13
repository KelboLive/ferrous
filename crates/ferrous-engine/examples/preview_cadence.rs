//! How smoothly the UI preview updates: runs the engine on a moving 1080p60
//! test pattern, polls for new preview frames like the UI does, and reports
//! the rate and how even the gaps between frames are.
//! `cargo run --release -p ferrous-engine --example preview_cadence -- [secs] [fps]`
#![allow(clippy::field_reassign_with_default)] // a few settings fields tweaked
use std::time::{Duration, Instant};

use ferrous_core::{RendererMode, SceneCollection, Settings, SourceConfig, VideoSettings};
use ferrous_engine::Engine;

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(6);
    let fps: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(60);
    let mut settings = Settings::default();
    settings.video = VideoSettings { base_width: 1920, base_height: 1080, output_width: 1920, output_height: 1080, fps, renderer: RendererMode::Cpu, vertical: None, conversion: Default::default(), pipelined_readback: true, gpu_priority: false };
    settings.audio.desktop_enabled = false;
    settings.audio.mic_enabled = false;
    let mut scenes = SceneCollection::default();
    scenes.current_scene_mut().items.clear();
    scenes.add_item("Bars", SourceConfig::TestPattern { width: 1920, height: 1080 });
    let engine = Engine::start(settings, scenes);
    std::thread::sleep(Duration::from_secs(2));

    let mut last = engine.preview().map(|p| p.serial);
    let mut arrivals = Vec::new();
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(1));
        let s = engine.preview().map(|p| p.serial);
        if s != last {
            last = s;
            arrivals.push(Instant::now());
        }
    }
    let gaps: Vec<f64> = arrivals.windows(2).map(|w| (w[1] - w[0]).as_secs_f64() * 1000.0).collect();
    let n = gaps.len().max(1) as f64;
    let mean = gaps.iter().sum::<f64>() / n;
    let sd = (gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / n).sqrt();
    let long = gaps.iter().filter(|g| **g > mean * 1.4).count();
    let mut sorted = gaps.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let st = engine.status();
    println!(
        "canvas {fps} fps (rendering {:.1}): preview {:.1} fps | gap mean {mean:.1} ms, sd {sd:.1} ms, max {:.1} ms | uneven gaps {:.0}% | render {:.1} ms/frame",
        st.fps,
        gaps.len() as f64 / secs as f64,
        sorted.last().copied().unwrap_or(0.0),
        long as f64 * 100.0 / n,
        st.render_ms,
    );
}
