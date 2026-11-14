//! An OBS plugin source through the bridge, on the real libobs (when OBS is
//! installed and a display is available): OBS's own Color Source renders,
//! reaches Ferrous's canvas, and takes new settings without restarting.

#![allow(clippy::field_reassign_with_default)] // tests tweak a few settings fields

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use ferrous_core::{RendererMode, SceneCollection, Settings, SourceConfig, VideoSettings};
use ferrous_engine::Engine;

fn host() -> Option<PathBuf> {
    if ferrous_obs::obs_dir().is_none() || (cfg!(unix) && std::env::var_os("DISPLAY").is_none()) {
        eprintln!("no OBS install or display; skipping");
        return None;
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    assert!(Command::new(env!("CARGO")).current_dir(&root).args(["build", "-q", "-p", "ferrous-obs-host"]).status().unwrap().success());
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or(root.join("target"));
    Some(target.join("debug").join(if cfg!(windows) { "ferrous-obs-host.exe" } else { "ferrous-obs-host" }))
}

fn wait_for(engine: &Engine, what: &str, f: impl Fn(&[u8]) -> bool) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(60) {
        if let Some(p) = engine.preview() {
            if f(&p.rgba[..4]) {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}: {:?}", engine.preview().map(|p| p.rgba[..4].to_vec()));
}

#[test]
fn obs_color_source_runs_and_updates_live() {
    let Some(host) = host() else { return };
    std::env::set_var("FERROUS_OBS_HOST", &host);
    let mut settings = Settings::default();
    settings.video = VideoSettings { base_width: 320, base_height: 240, output_width: 320, output_height: 240, fps: 30, renderer: RendererMode::Cpu, vertical: None, conversion: Default::default(), pipelined_readback: true, gpu_priority: false, gpu_capture: false };
    settings.audio.desktop_enabled = false;
    settings.audio.mic_enabled = false;
    let mut scenes = SceneCollection::default();
    scenes.current_scene_mut().items.clear();
    // OBS colours are 0xAABBGGRR: opaque red.
    let red = r#"{"color": 4278190335, "width": 320, "height": 240}"#;
    let id = scenes.add_item("OBS Color", SourceConfig::ObsPlugin { type_id: "color_source_v3".into(), name: "Color Source".into(), settings: red.into() });
    let engine = Engine::start(settings, scenes.clone());
    wait_for(&engine, "red from OBS", |p| p[0] > 200 && p[1] < 40 && p[2] < 40);

    let pid = ferrous_obs::registry().lock().get(&id).and_then(|w| w.upgrade()).map(|h| h.pid()).expect("host registered");
    let (props, values) = ferrous_obs::registry().lock().get(&id).and_then(|w| w.upgrade()).unwrap().properties().unwrap();
    assert!(props.iter().any(|p| p.name == "color"), "{props:?}");
    assert_eq!(values["width"], 320);

    // New settings (blue) apply to the running plugin: same host process.
    let blue = r#"{"color": 4294901760, "width": 320, "height": 240}"#;
    if let SourceConfig::ObsPlugin { settings, .. } = &mut scenes.current_scene_mut().items[0].source {
        *settings = blue.into();
    }
    engine.set_scenes(&scenes);
    wait_for(&engine, "blue after update", |p| p[2] > 200 && p[0] < 40);
    let pid2 = ferrous_obs::registry().lock().get(&id).and_then(|w| w.upgrade()).map(|h| h.pid());
    assert_eq!(pid2, Some(pid), "updated in place, not restarted");
}
