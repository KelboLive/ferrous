//! The virtual camera path end to end, with the file-backed region standing
//! in for the Windows camera source: the camera "opens" after Ferrous starts
//! (so Ferrous must keep retrying), frames arrive at 1080p NV12, and colours
//! survive the conversion.
#![allow(clippy::field_reassign_with_default)] // tests tweak a few settings fields

#![cfg(unix)]

use std::time::Duration;

use ferrous_core::{RendererMode, SceneCollection, Settings, SourceConfig, VideoSettings};
use ferrous_engine::Engine;

#[test]
fn program_reaches_the_camera() {
    let path = ferrous_vcam::unix_path();
    let _ = std::fs::remove_file(&path);
    let mut settings = Settings::default();
    settings.video = VideoSettings { base_width: 640, base_height: 360, output_width: 640, output_height: 360, fps: 30, renderer: RendererMode::Cpu, vertical: None, conversion: Default::default(), pipelined_readback: true, gpu_priority: false, gpu_capture: false };
    settings.audio.desktop_enabled = false;
    settings.audio.mic_enabled = false;
    let mut scenes = SceneCollection::default();
    scenes.current_scene_mut().items.clear();
    // Pure red (BGRA).
    scenes.add_item("Red", SourceConfig::Color { bgra: [0, 0, 255, 255], width: 640, height: 360 });
    let engine = Engine::start(settings, scenes);
    engine.start_virtual_camera().unwrap();
    assert!(engine.is_virtual_camera_active());
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(engine.virtual_camera_status().map(|s| s.0), Some(false), "no camera app yet");

    // A camera app opens the camera.
    let cam = ferrous_vcam::FileConsumer::create(&path).unwrap();
    let mut frame = vec![0u8; ferrous_vcam::FRAME_BYTES];
    let mut got = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(100));
        if cam.region.read(&mut frame) {
            got = true;
            break;
        }
    }
    assert!(got, "no frame reached the camera");
    let (connected, frames) = engine.virtual_camera_status().unwrap();
    assert!(connected && frames > 0);
    // BT.709 limited-range red: Y 63, U 102, V 240.
    let y = frame[1920 * 540 + 960];
    let uv = (1920 * 1080) + 1920 * 270 + 960;
    assert!((y as i32 - 63).abs() <= 2, "Y {y}");
    assert!((frame[uv] as i32 - 102).abs() <= 2 && (frame[uv + 1] as i32 - 240).abs() <= 2, "UV {} {}", frame[uv], frame[uv + 1]);

    engine.stop_virtual_camera();
    assert!(!engine.is_virtual_camera_active());
    let _ = std::fs::remove_file(path);
}
