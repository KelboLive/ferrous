//! Render-loop frame pacing and per-stage cost under a realistic load.
//! `cargo run --release -p ferrous-engine --example render_bench -- [secs] [cpu|gpu] [light|heavy]`
//!
//! light: 1080p60 canvas, a moving full-screen source (stand-in for game
//!        capture), a scaled overlay, text, preview on.
//! heavy: light + 1080x1920 vertical canvas + an Ferrous Link JPEG output
//!        with a receiver attached (shares the CPU, like a real setup).
#![allow(clippy::field_reassign_with_default)]
use std::time::Duration;

use ferrous_core::{LinkCodec, LinkOutputSettings, LinkSourceTarget, RendererMode, SceneCollection, Settings, SourceConfig, TextAlign, Transform, VideoSettings};
use ferrous_engine::{Engine, PreviewConfig, FRAME_STAGES};

fn pct(v: &mut [f32], p: f32) -> f32 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f32 * p) as usize]
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let secs: u64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    let gpu = a.get(2).is_some_and(|s| s == "gpu");
    let heavy = a.get(3).is_some_and(|s| s == "heavy");
    let mut settings = Settings::default();
    settings.video = VideoSettings {
        base_width: 1920, base_height: 1080, output_width: 1920, output_height: 1080, fps: 60,
        renderer: if gpu { RendererMode::Auto } else { RendererMode::Cpu },
        vertical: heavy.then_some((1080, 1920)), conversion: Default::default(), pipelined_readback: true, gpu_priority: false,
    };
    settings.audio.desktop_enabled = false;
    settings.audio.mic_enabled = false;
    if heavy {
        settings.links = vec![LinkOutputSettings {
            enabled: true, name: "bench".into(), target: LinkSourceTarget::Program,
            codec: LinkCodec::Jpeg { quality: 85, adaptive: false }, width: 1280, height: 720, fps: 0, port: 0,
            pin: String::new(), audio: false, ..Default::default()
        }];
    }
    let mut scenes = SceneCollection::default();
    scenes.current_scene_mut().items.clear();
    scenes.add_item("Game", SourceConfig::TestPattern { width: 1920, height: 1080 });
    let cam = scenes.add_item("Cam", SourceConfig::TestPattern { width: 640, height: 360 });
    scenes.add_item("Text", SourceConfig::Text {
        text: "Ferrous benchmark".into(), from_file: None, font: String::new(), size: 48.0,
        bgra: [255, 255, 255, 255], outline: ([0, 0, 0, 255], 2), background: [0, 0, 0, 0], align: TextAlign::Left, wrap_width: 0,
    });
    if let Some(i) = scenes.current_scene_mut().items.iter_mut().find(|i| i.id == cam) {
        i.transform = Transform { x: 1380.0, y: 760.0, scale_x: 0.8, scale_y: 0.8, ..Default::default() };
    }
    let engine = Engine::start(settings, scenes);
    engine.set_preview_config(PreviewConfig { enabled: true, max_fps: 0, width: 1280, vertical_width: 540 });
    let _rx = heavy.then(|| {
        std::thread::sleep(Duration::from_millis(500));
        let port = engine.link_stats().first().map(|l| l.1).unwrap_or(47778);
        ferrous_link::LinkReceiver::start(&format!("127.0.0.1:{port}"), "", ferrous_encode::ffmpeg::locate(""), None)
    });
    std::thread::sleep(Duration::from_secs(3)); // warm up
    let lag0 = engine.status().lagged_frames;
    std::thread::sleep(Duration::from_secs(secs));
    let st = engine.status();
    let ft = engine.frame_times();
    let n = (secs as usize * 60).min(ft.len());
    let ft = &ft[ft.len() - n..];
    let mut iv: Vec<f32> = ft.iter().map(|f| f.interval_ms).collect();
    let mut wk: Vec<f32> = ft.iter().map(|f| f.work_ms).collect();
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
    let (imean, wmean) = (mean(&iv), mean(&wk));
    let isd = (iv.iter().map(|x| (x - imean).powi(2)).sum::<f32>() / iv.len() as f32).sqrt();
    let late = iv.iter().filter(|x| **x > 16.67 * 1.5).count();
    println!(
        "{} {:<5} | {} | frame interval {imean:.2} ms (sd {isd:.2}, p99 {:.1}, max {:.1}), >25ms: {late} | work mean {wmean:.2} p99 {:.1} max {:.1} ms | lagged {}",
        if gpu { "GPU" } else { "CPU" },
        if heavy { "heavy" } else { "light" },
        st.renderer,
        pct(&mut iv, 0.99), iv.last().copied().unwrap_or(0.0),
        pct(&mut wk.clone(), 0.99), { wk.sort_by(|a, b| a.partial_cmp(b).unwrap()); wk.last().copied().unwrap_or(0.0) },
        st.lagged_frames - lag0,
    );
    for (i, name) in FRAME_STAGES.iter().enumerate() {
        let mut v: Vec<f32> = ft.iter().map(|f| f.stages_ms[i]).collect();
        let m = mean(&v);
        println!("    {name:<18} mean {m:6.2} ms  p99 {:6.2}  max {:6.2}", pct(&mut v, 0.99), v.last().copied().unwrap_or(0.0));
    }
}
