//! Records a few seconds of a test pattern through the full pipeline and
//! checks the file with ffprobe. Skipped when FFmpeg isn't installed.

#![allow(clippy::field_reassign_with_default)] // tests tweak a few settings fields
use std::process::Command;
use std::time::Duration;

use ferrous_core::{RecordingFormat, RendererMode, SceneCollection, Settings, SourceConfig, Transform, VideoSettings};
use ferrous_engine::Engine;

fn have_ffmpeg() -> bool {
    Command::new("ffprobe").arg("-version").output().is_ok()
        && Command::new("ffmpeg").arg("-version").output().is_ok()
}

fn probe(path: &std::path::Path) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,width,height:format=duration", "-of", "default=nw=1"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn record(format: RecordingFormat, encoder: &str) -> String {
    record_with(format, encoder, RendererMode::Cpu)
}

fn record_with(format: RecordingFormat, encoder: &str, renderer: RendererMode) -> String {
    let dir = std::env::temp_dir().join(format!("ferrous-e2e-{encoder}-{}-{renderer:?}", format.extension()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut settings = Settings::default();
    settings.video = VideoSettings { base_width: 640, base_height: 360, output_width: 320, output_height: 180, fps: 30, renderer, vertical: None, conversion: Default::default(), pipelined_readback: true, gpu_priority: false, gpu_capture: false, zero_copy_encoding: false };
    settings.audio.desktop_enabled = false;
    settings.audio.mic_enabled = false;
    settings.record.folder = dir.clone();
    settings.record.format = format;
    settings.record.encoder.encoder = encoder.into();
    settings.record.encoder.preset = String::new();

    let mut scenes = SceneCollection::default();
    scenes.current_scene_mut().items.clear();
    scenes.add_item("Bars", SourceConfig::TestPattern { width: 640, height: 360 });
    let id = scenes.add_item("Box", SourceConfig::Color { bgra: [0, 128, 255, 200], width: 100, height: 100 });
    scenes.current_scene_mut().items.iter_mut().find(|i| i.id == id).unwrap().transform =
        Transform { x: 50.0, y: 50.0, ..Default::default() };

    let engine = Engine::start(settings, scenes);
    std::thread::sleep(Duration::from_millis(300));
    let path = engine.start_recording().expect("start recording");
    std::thread::sleep(Duration::from_secs(3));
    let st = engine.status();
    assert!(st.recording.active);
    engine.stop_recording();
    // Stop runs in the background; wait for the "saved" event.
    let mut saved = false;
    for _ in 0..100 {
        for ev in engine.drain_events() {
            match ev {
                ferrous_engine::EngineEvent::Info(m) if m.contains("saved") => saved = true,
                ferrous_engine::EngineEvent::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
        if saved {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(saved, "recording never finalised");
    println!("render fps {:.1}, {:.2} ms/frame", st.fps, st.render_ms);
    probe(&path)
}

#[test]
fn records_mkv_x264() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not found; skipping");
        return;
    }
    let info = record(RecordingFormat::Mkv, "libx264");
    println!("{info}");
    assert!(info.contains("codec_name=h264"));
    assert!(info.contains("width=320"));
    assert!(info.contains("codec_name=aac"));
    let dur: f32 = info.lines().find_map(|l| l.strip_prefix("duration=")).unwrap().parse().unwrap();
    assert!((2.5..4.0).contains(&dur), "duration {dur}");
}

#[test]
fn records_fragmented_mp4() {
    if !have_ffmpeg() {
        return;
    }
    let info = record(RecordingFormat::FragmentedMp4, "libx264");
    assert!(info.contains("codec_name=h264"), "{info}");
}

#[test]
fn records_with_gpu_compositor() {
    if !have_ffmpeg() {
        return;
    }
    // This host only has a software Vulkan driver; force it so the GPU path is exercised.
    std::env::set_var("FERROUS_FORCE_GPU", "1");
    let info = record_with(RecordingFormat::Mkv, "libx264", RendererMode::Auto);
    assert!(info.contains("codec_name=h264"), "{info}");
}

/// A media file's video and audio both make it into the recording.
#[test]
fn media_source_video_and_audio_reach_recording() {
    if !have_ffmpeg() {
        return;
    }
    let dir = std::env::temp_dir().join("ferrous-e2e-media");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clip = dir.join("clip.mp4");
    let ok = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=320x240:r=30:d=2", "-f", "lavfi", "-i", "sine=f=440:d=2"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
        .arg(&clip)
        .status()
        .unwrap()
        .success();
    assert!(ok);

    let mut settings = Settings::default();
    settings.video = VideoSettings { base_width: 640, base_height: 360, output_width: 640, output_height: 360, fps: 30, renderer: RendererMode::Cpu, vertical: None, conversion: Default::default(), pipelined_readback: true, gpu_priority: false, gpu_capture: false, zero_copy_encoding: false };
    settings.audio.desktop_enabled = false;
    settings.audio.mic_enabled = false;
    settings.record.folder = dir.clone();
    settings.ffmpeg_path = String::new();
    let mut scenes = SceneCollection::default();
    scenes.current_scene_mut().items.clear();
    scenes.add_item("Clip", SourceConfig::Media { input: clip.display().to_string(), looping: true, hw_decode: false });

    let engine = Engine::start(settings, scenes);
    // Wait until the media source has produced a frame.
    let mut seen = false;
    for _ in 0..100 {
        if engine.source_info(2).is_some_and(|((w, _), _)| w == 320) {
            seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(seen, "media source never produced video");
    let path = engine.start_recording().unwrap();
    std::thread::sleep(Duration::from_secs(3));
    engine.stop_recording();
    std::thread::sleep(Duration::from_secs(3));
    drop(engine);

    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-i"])
        .arg(&path)
        .args(["-af", "volumedetect", "-f", "null", "-"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    let mean: f32 = log
        .lines()
        .find_map(|l| l.split("mean_volume:").nth(1))
        .and_then(|v| v.trim().trim_end_matches(" dB").parse().ok())
        .expect("volumedetect output");
    println!("recorded mean volume {mean} dB");
    assert!(mean > -40.0, "recording is silent ({mean} dB)");

    // The clip's moving test pattern should be visible top-left.
    let frame = dir.join("f.png");
    Command::new("ffmpeg").args(["-v", "error", "-y", "-ss", "1.5", "-i"]).arg(&path).args(["-frames:v", "1"]).arg(&frame).status().unwrap();
    assert!(frame.exists());
}

/// Video filters run in the engine: a keyed-out green layer reveals red below.
#[test]
fn chroma_key_filter_applies_in_engine() {
    use ferrous_core::filters::{Filter, VideoFilter};
    let mut settings = Settings::default();
    settings.video = VideoSettings { base_width: 64, base_height: 64, output_width: 64, output_height: 64, fps: 30, renderer: RendererMode::Cpu, vertical: None, conversion: Default::default(), pipelined_readback: true, gpu_priority: false, gpu_capture: false, zero_copy_encoding: false };
    settings.audio.desktop_enabled = false;
    settings.audio.mic_enabled = false;
    let mut scenes = SceneCollection::default();
    scenes.current_scene_mut().items.clear();
    scenes.add_item("Red", SourceConfig::Color { bgra: [0, 0, 255, 255], width: 64, height: 64 });
    let green = scenes.add_item("Green", SourceConfig::Color { bgra: [0, 255, 0, 255], width: 64, height: 64 });
    let engine = Engine::start(settings, scenes.clone());
    std::thread::sleep(Duration::from_millis(500));
    let p = engine.preview().unwrap();
    assert_eq!(&p.rgba[..3], &[0, 255, 0], "green on top before keying");

    scenes.current_scene_mut().items.iter_mut().find(|i| i.id == green).unwrap().video_filters =
        vec![Filter::new(VideoFilter::ChromaKey { key_rgb: [0, 255, 0], similarity: 400, smoothness: 80, spill: 100 })];
    engine.set_scenes(&scenes);
    std::thread::sleep(Duration::from_millis(500));
    let p = engine.preview().unwrap();
    assert_eq!(&p.rgba[..3], &[255, 0, 0], "red shows through after keying");
}

/// Changing the program scene runs the configured transition; studio mode
/// renders a separate preview scene.
#[test]
fn fade_transition_and_studio_preview() {
    let mut settings = Settings::default();
    settings.video = VideoSettings { base_width: 32, base_height: 32, output_width: 32, output_height: 32, fps: 60, renderer: RendererMode::Cpu, vertical: None, conversion: Default::default(), pipelined_readback: true, gpu_priority: false, gpu_capture: false, zero_copy_encoding: false };
    settings.audio.desktop_enabled = false;
    settings.audio.mic_enabled = false;
    let mut scenes = SceneCollection::default();
    scenes.current_scene_mut().items.clear();
    scenes.add_item("Black", SourceConfig::Color { bgra: [0, 0, 0, 255], width: 32, height: 32 });
    scenes.current = scenes.add_scene("White");
    scenes.add_item("White", SourceConfig::Color { bgra: [255, 255, 255, 255], width: 32, height: 32 });
    scenes.current = 0;
    scenes.transition = ferrous_core::Transition { kind: ferrous_core::TransitionKind::Fade, duration_ms: 1200 };
    let engine = Engine::start(settings, scenes.clone());
    engine.set_studio_scene(Some(1));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(engine.preview().unwrap().rgba[0], 0);
    assert_eq!(engine.studio_preview().unwrap().rgba[0], 255, "studio preview shows scene 2");

    scenes.current = 1;
    engine.set_scenes(&scenes);
    std::thread::sleep(Duration::from_millis(600));
    let mid = engine.preview().unwrap().rgba[0];
    assert!((40..=215).contains(&mid), "mid-transition value {mid}");
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(engine.preview().unwrap().rgba[0], 255);
}

/// A stinger video is overlaid during the transition.
#[test]
fn stinger_transition_overlays_video() {
    if !have_ffmpeg() {
        return;
    }
    let dir = std::env::temp_dir().join("ferrous-e2e-stinger");
    std::fs::create_dir_all(&dir).unwrap();
    let clip = dir.join("stinger.mp4");
    assert!(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=red:s=64x64:r=30:d=3", "-pix_fmt", "yuv420p"])
        .arg(&clip)
        .status()
        .unwrap()
        .success());
    let mut settings = Settings::default();
    settings.video = VideoSettings { base_width: 64, base_height: 64, output_width: 64, output_height: 64, fps: 30, renderer: RendererMode::Cpu, vertical: None, conversion: Default::default(), pipelined_readback: true, gpu_priority: false, gpu_capture: false, zero_copy_encoding: false };
    settings.audio.desktop_enabled = false;
    settings.audio.mic_enabled = false;
    let mut scenes = SceneCollection::default();
    scenes.current_scene_mut().items.clear();
    scenes.add_item("Black", SourceConfig::Color { bgra: [0, 0, 0, 255], width: 64, height: 64 });
    scenes.add_scene("Other");
    scenes.transition = ferrous_core::Transition {
        kind: ferrous_core::TransitionKind::Stinger { path: clip, cut_point_ms: 1000 },
        duration_ms: 2500,
    };
    let engine = Engine::start(settings, scenes.clone());
    std::thread::sleep(Duration::from_millis(300));
    scenes.current = 1;
    engine.set_scenes(&scenes);
    let mut saw_red = false;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        let p = engine.preview().unwrap();
        if p.rgba[0] > 200 && p.rgba[1] < 50 {
            saw_red = true;
            break;
        }
    }
    assert!(saw_red, "stinger never appeared");
}

fn tone_clip(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let clip = dir.join("tone.mp4");
    assert!(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=blue:s=64x64:r=30:d=3", "-f", "lavfi", "-i", "sine=f=440:d=3"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
        .arg(&clip)
        .status()
        .unwrap()
        .success());
    clip
}

fn mean_volume(path: &std::path::Path, stream: &str) -> f32 {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-i"])
        .arg(path)
        .args(["-map", stream, "-af", "volumedetect", "-f", "null", "-"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    log.lines()
        .find_map(|l| l.split("mean_volume:").nth(1))
        .and_then(|v| v.trim().trim_end_matches(" dB").parse().ok())
        .unwrap_or(-200.0)
}

fn small_settings(dir: &std::path::Path) -> Settings {
    let mut settings = Settings::default();
    settings.video = VideoSettings { base_width: 64, base_height: 64, output_width: 64, output_height: 64, fps: 30, renderer: RendererMode::Cpu, vertical: None, conversion: Default::default(), pipelined_readback: true, gpu_priority: false, gpu_capture: false, zero_copy_encoding: false };
    settings.audio.desktop_enabled = false;
    settings.audio.mic_enabled = false;
    settings.record.folder = dir.to_path_buf();
    settings
}

/// Sources can be routed to specific tracks; recordings carry each track.
#[test]
fn multitrack_recording_routes_sources() {
    if !have_ffmpeg() {
        return;
    }
    let dir = std::env::temp_dir().join("ferrous-e2e-multitrack");
    let _ = std::fs::remove_dir_all(&dir);
    let clip = tone_clip(&dir);
    let mut settings = small_settings(&dir);
    settings.record.audio_tracks = 0b11; // tracks 1 and 2
    let mut scenes = SceneCollection::default();
    scenes.current_scene_mut().items.clear();
    let id = scenes.add_item("Tone", SourceConfig::Media { input: clip.display().to_string(), looping: true, hw_decode: false });
    scenes.current_scene_mut().items.iter_mut().find(|i| i.id == id).unwrap().audio_tracks = 0b10; // track 2 only
    let engine = Engine::start(settings, scenes);
    std::thread::sleep(Duration::from_secs(1));
    let path = engine.start_recording().unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let st = engine.status();
    println!("rec status: active={} written={}", st.recording.active, st.recording.frames_written);
    engine.stop_recording();
    std::thread::sleep(Duration::from_secs(3));
    for ev in engine.drain_events() {
        println!("event: {ev:?}");
    }
    drop(engine);
    let t1 = mean_volume(&path, "0:a:0");
    let t2 = mean_volume(&path, "0:a:1");
    println!("track1 {t1} dB, track2 {t2} dB");
    assert!(t1 < -80.0, "track 1 should be silent, got {t1}");
    assert!(t2 > -40.0, "track 2 should carry the tone, got {t2}");
}

/// The replay buffer saves the recent past as a playable file.
#[test]
fn replay_buffer_saves_clip() {
    if !have_ffmpeg() {
        return;
    }
    let dir = std::env::temp_dir().join("ferrous-e2e-replay");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut settings = small_settings(&dir);
    settings.replay.seconds = 4;
    let mut scenes = SceneCollection::default();
    scenes.current_scene_mut().items.clear();
    scenes.add_item("Bars", SourceConfig::TestPattern { width: 64, height: 64 });
    let engine = Engine::start(settings, scenes);
    engine.start_replay().unwrap();
    std::thread::sleep(Duration::from_secs(9));
    let path = engine.save_replay().unwrap();
    let mut saved = false;
    for _ in 0..50 {
        for ev in engine.drain_events() {
            match ev {
                ferrous_engine::EngineEvent::Info(m) if m.contains("Replay saved") => saved = true,
                ferrous_engine::EngineEvent::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
        if saved {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(saved);
    engine.stop_replay();
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(&path)
        .output()
        .unwrap();
    let dur: f32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    println!("replay duration {dur}");
    // 4 s requested, kept in 2 s segments: expect roughly 4-8 s, never the full 9 s.
    assert!((3.5..8.5).contains(&dur), "{dur}");
}

/// A scene can be used as a source in another scene (scaled), and cycles are safe.
#[test]
fn nested_scene_source() {
    let mut settings = Settings::default();
    settings.video = VideoSettings { base_width: 100, base_height: 100, output_width: 100, output_height: 100, fps: 30, renderer: RendererMode::Cpu, vertical: None, conversion: Default::default(), pipelined_readback: true, gpu_priority: false, gpu_capture: false, zero_copy_encoding: false };
    settings.audio.desktop_enabled = false;
    settings.audio.mic_enabled = false;
    let mut scenes = SceneCollection::default();
    scenes.scenes[0].name = "Main".into();
    scenes.scenes[0].items.clear();
    let inner = scenes.add_scene("Camera Box");
    scenes.add_item_to(inner, "Green", SourceConfig::Color { bgra: [0, 255, 0, 255], width: 100, height: 100 });
    // Inner scene also references Main: a cycle that must not hang.
    scenes.add_item_to(inner, "Back to main", SourceConfig::Scene { name: "Main".into() });
    let id = scenes.add_item_to(0, "Nested", SourceConfig::Scene { name: "Camera Box".into() });
    scenes.scenes[0].items.iter_mut().find(|i| i.id == id).unwrap().transform =
        Transform { x: 50.0, y: 50.0, scale_x: 0.5, scale_y: 0.5, ..Default::default() };
    let engine = Engine::start(settings, scenes);
    std::thread::sleep(Duration::from_millis(600));
    let p = engine.preview().unwrap();
    let px = |x: usize, y: usize| &p.rgba[(y * 100 + x) * 4..(y * 100 + x) * 4 + 3];
    assert_eq!(px(10, 10), &[0, 0, 0], "outside the nested scene");
    assert_eq!(px(75, 75), &[0, 255, 0], "nested scene, scaled to the bottom-right quarter");
    assert_eq!(engine.source_info(id).unwrap().0, (100, 100));
}

/// Text renders through the engine.
#[test]
fn text_source_renders() {
    let mut settings = Settings::default();
    settings.video = VideoSettings { base_width: 320, base_height: 120, output_width: 320, output_height: 120, fps: 30, renderer: RendererMode::Cpu, vertical: None, conversion: Default::default(), pipelined_readback: true, gpu_priority: false, gpu_capture: false, zero_copy_encoding: false };
    settings.audio.desktop_enabled = false;
    settings.audio.mic_enabled = false;
    let mut scenes = SceneCollection::default();
    scenes.scenes[0].items.clear();
    let id = scenes.add_item("Title", SourceConfig::Text {
        text: "LIVE".into(), from_file: None, font: String::new(), size: 64.0, bgra: [255, 255, 255, 255],
        outline: ([0, 0, 0, 255], 2), background: [0, 0, 0, 0], align: ferrous_core::TextAlign::Left, wrap_width: 0,
    });
    let engine = Engine::start(settings, scenes);
    std::thread::sleep(Duration::from_millis(600));
    let ((w, h), err) = engine.source_info(id).unwrap();
    if err.as_deref().is_some_and(|e| e.contains("font")) {
        eprintln!("no font on this host; skipping");
        return;
    }
    assert!(w > 100 && h > 50, "{w}x{h} {err:?}");
    let p = engine.preview().unwrap();
    let white = p.rgba.chunks_exact(4).filter(|c| c[0] > 230 && c[1] > 230 && c[2] > 230).count();
    assert!(white > 300, "{white} white pixels");
}

/// Two Ferrous instances: A sends its program over Ferrous Link, B shows it as a source.
#[test]
fn ferrous_link_between_two_engines() {
    let mk = |w: u32| {
        let mut s = Settings::default();
        s.video = VideoSettings { base_width: w, base_height: w / 2, output_width: w, output_height: w / 2, fps: 30, renderer: RendererMode::Cpu, vertical: None, conversion: Default::default(), pipelined_readback: true, gpu_priority: false, gpu_capture: false, zero_copy_encoding: false };
        s.audio.desktop_enabled = false;
        s.audio.mic_enabled = false;
        s
    };
    let mut a_settings = mk(128);
    a_settings.links = vec![ferrous_core::LinkOutputSettings {
        enabled: true,
        name: "Engine A".into(),
        codec: ferrous_core::LinkCodec::Lossless,
        port: 47901,
        pin: "2468".into(),
        ..Default::default()
    }];
    let mut a_scenes = SceneCollection::default();
    a_scenes.scenes[0].items.clear();
    a_scenes.add_item("Magenta", SourceConfig::Color { bgra: [200, 0, 200, 255], width: 128, height: 64 });
    let a = Engine::start(a_settings, a_scenes);
    assert_eq!(a.link_stats().len(), 1, "{:?}", a.drain_events());

    let mut b_scenes = SceneCollection::default();
    b_scenes.scenes[0].items.clear();
    let id = b_scenes.add_item("From A", SourceConfig::LinkReceiver { sender: "127.0.0.1:47901".into(), pin: "2468".into() });
    let b = Engine::start(mk(128), b_scenes);
    let mut ok = false;
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(p) = b.preview() {
            if p.rgba[..3] == [200, 0, 200] {
                ok = true;
                break;
            }
        }
    }
    assert!(ok, "B never showed A's program; source info {:?}", b.source_info(id));
    let (_, _, stats) = &a.link_stats()[0];
    assert_eq!(stats.clients.load(std::sync::atomic::Ordering::Relaxed), 1);
}

/// The Teleport way: A composes on the GPU, converts to JPEG's YCbCr there and sends JPEGs made
/// straight from those planes (no BGRA read-back, A's preview off); B must show A's colour.
#[test]
fn jpeg_link_from_gpu_ycbcr() {
    std::env::set_var("FERROUS_FORCE_GPU", "1");
    let mk = |renderer| {
        let mut s = Settings::default();
        s.video = VideoSettings { base_width: 128, base_height: 64, output_width: 128, output_height: 64, fps: 30, renderer, vertical: None, conversion: Default::default(), pipelined_readback: true, gpu_priority: false, gpu_capture: false, zero_copy_encoding: false };
        s.audio.desktop_enabled = false;
        s.audio.mic_enabled = false;
        s
    };
    let mut a_settings = mk(RendererMode::Auto);
    a_settings.links = vec![ferrous_core::LinkOutputSettings {
        enabled: true,
        name: "GPU JPEG".into(),
        codec: ferrous_core::LinkCodec::Jpeg { quality: 95, adaptive: false, full_chroma: false },
        port: 47911,
        pin: "1357".into(),
        ..Default::default()
    }];
    let mut a_scenes = SceneCollection::default();
    a_scenes.scenes[0].items.clear();
    a_scenes.add_item("Teal", SourceConfig::Color { bgra: [180, 140, 20, 255], width: 128, height: 64 });
    let a = Engine::start(a_settings, a_scenes);
    a.set_preview_config(ferrous_engine::PreviewConfig { enabled: false, ..Default::default() });
    let mut b_scenes = SceneCollection::default();
    b_scenes.scenes[0].items.clear();
    let id = b_scenes.add_item("From A", SourceConfig::LinkReceiver { sender: "127.0.0.1:47911".into(), pin: "1357".into() });
    let b = Engine::start(mk(RendererMode::Cpu), b_scenes);
    let mut last = None;
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(p) = b.preview() {
            let px = [p.rgba[0], p.rgba[1], p.rgba[2]];
            last = Some(px);
            // RGBA preview of BGRA (180,140,20): R 20, G 140, B 180, within JPEG's rounding.
            if (i32::from(px[0]) - 20).abs() <= 10 && (i32::from(px[1]) - 140).abs() <= 10 && (i32::from(px[2]) - 180).abs() <= 10 {
                let (_, _, stats) = &a.link_stats()[0];
                let yuv = stats.frames_from_gpu_yuv.load(std::sync::atomic::Ordering::Relaxed);
                if std::env::var_os("FERROUS_FORCE_GPU").is_some() && yuv == 0 {
                    eprintln!("note: no GPU here, so the JPEGs came from BGRA");
                }
                eprintln!("JPEGs from GPU YCbCr: {yuv}");
                return;
            }
        }
    }
    panic!("B never showed A's colour; last {last:?}, source info {:?}", b.source_info(id));
}

/// TikTok-style: a vertical canvas streamed to a destination whose key is
/// given per session, while the main canvas stays horizontal. A local FFmpeg
/// RTMP listener plays the platform.
#[test]
fn vertical_destination_with_session_key() {
    if !have_ffmpeg() {
        return;
    }
    let dir = std::env::temp_dir().join("ferrous-e2e-vertical");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let received = dir.join("received.flv");
    let mut server = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-listen", "1", "-timeout", "20", "-i", "rtmp://127.0.0.1:19355/live/secret-session-key", "-c", "copy"])
        .arg(&received)
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));

    let mut settings = Settings::default();
    settings.video = VideoSettings { base_width: 640, base_height: 360, output_width: 640, output_height: 360, fps: 30, renderer: RendererMode::Cpu, vertical: Some((360, 640)), conversion: Default::default(), pipelined_readback: true, gpu_priority: false, gpu_capture: false, zero_copy_encoding: false };
    settings.audio.desktop_enabled = false;
    settings.audio.mic_enabled = false;
    settings.stream.server = String::new(); // only the destination
    let mut dest = ferrous_core::StreamDestination::tiktok("libx264");
    dest.output_width = 360;
    dest.output_height = 640;
    settings.destinations = vec![dest];

    let mut scenes = SceneCollection::default();
    scenes.scenes[0].items.clear();
    scenes.add_item("Bars", SourceConfig::TestPattern { width: 640, height: 360 });
    let engine = Engine::start(settings, scenes);
    std::thread::sleep(Duration::from_millis(500));
    // Without the session key it refuses.
    assert!(engine.start_streaming().is_err());
    let mut keys = std::collections::HashMap::new();
    keys.insert("TikTok LIVE".to_string(), ("rtmp://127.0.0.1:19355/live".to_string(), "secret-session-key".to_string()));
    engine.start_streaming_with(&keys).unwrap();
    assert!(engine.is_streaming());
    // Vertical preview: the 16:9 source fitted to the 9:16 width, black above and below.
    let v = engine.vertical_preview().expect("vertical preview");
    assert!(v.height > v.width);
    assert_eq!(&v.rgba[..3], &[0, 0, 0], "top of vertical canvas is empty");
    std::thread::sleep(Duration::from_secs(4));
    let live = engine.destination_status();
    assert!(live[0].1, "destination live: {live:?}");
    engine.stop_streaming();
    std::thread::sleep(Duration::from_secs(2));
    let _ = server.wait();
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(&received)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "360,640");
}

/// Per-source CPU cost and helper-process accounting for the performance dock.
#[test]
fn per_source_performance_is_measured() {
    if !have_ffmpeg() {
        return;
    }
    let dir = std::env::temp_dir().join("ferrous-e2e-perf");
    let clip = tone_clip(&dir);
    let mut settings = small_settings(&dir);
    settings.video.base_width = 1280;
    settings.video.base_height = 720;
    let mut scenes = SceneCollection::default();
    scenes.scenes[0].items.clear();
    let bars = scenes.add_item("Bars", SourceConfig::TestPattern { width: 1280, height: 720 });
    let media = scenes.add_item("Clip", SourceConfig::Media { input: clip.display().to_string(), looping: true, hw_decode: false });
    let engine = Engine::start(settings, scenes);
    std::thread::sleep(Duration::from_secs(4));
    let perf = engine.perf();
    let b = &perf[&bars];
    println!("bars: source {:.3} ms, composite {:.3} ms", b.source_ms, b.composite_ms);
    assert!(b.source_ms > 0.01 && b.composite_ms > 0.01);
    let m = &perf[&media];
    assert_eq!(m.pids.len(), 1, "media source reports its FFmpeg process");
    // On Windows the system monitor takes its first sample only ~5 s after start (it waits out the
    // GPU's start-up), so wait for it rather than assuming it's there.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let sys = loop {
        let sys = engine.system();
        if sys.per_pid.contains_key(&m.pids[0]) && sys.own.cpu > 0.0 || std::time::Instant::now() > deadline {
            break sys;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    let p = sys.per_pid.get(&m.pids[0]).expect("helper process sampled");
    println!("ffmpeg decoder: {:.1}% CPU, {} MB", p.cpu, p.mem_bytes / 1_000_000);
    assert!(p.mem_bytes > 0);
    assert!(sys.own.cpu > 0.0);
}

/// A stream whose connection drops reconnects by itself (off the render thread), and the picture keeps
/// rendering meanwhile.
#[test]
fn dropped_stream_reconnects() {
    if !have_ffmpeg() {
        return;
    }
    use std::io::Read;
    let dir = std::env::temp_dir().join("ferrous-e2e-reconnect");
    let _ = std::fs::remove_dir_all(&dir);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut settings = small_settings(&dir);
    settings.stream.server = format!("tcp://127.0.0.1:{port}");
    settings.stream.key = String::new();
    settings.stream.encoder.encoder = "libx264".into();
    let mut scenes = SceneCollection::default();
    scenes.add_item("Bars", SourceConfig::TestPattern { width: 64, height: 64 });
    let engine = Engine::start(settings, scenes);
    engine.start_streaming().expect("stream starts");
    listener.set_nonblocking(false).unwrap();
    // First connection: read a little, then cut it.
    let (mut first, _) = listener.accept().expect("FFmpeg connects");
    let mut buf = [0u8; 4096];
    let got = first.read(&mut buf).unwrap();
    assert!(got > 0, "stream data arrives");
    drop(first);
    // The engine notices, waits its reconnect delay and connects again.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(listener.accept().map(|(mut s, _)| {
            let mut b = [0u8; 4096];
            s.read(&mut b).unwrap_or(0)
        }));
    });
    let again = rx.recv_timeout(Duration::from_secs(30)).expect("reconnected within 30 s").expect("accept");
    assert!(again > 0, "data after reconnecting");
    let events = engine.drain_events();
    assert!(events.iter().any(|e| format!("{e:?}").contains("Reconnecting")), "{events:?}");
    assert!(engine.status().fps > 10.0, "rendering went on");
    engine.stop_streaming();
}

/// Outputs can be started from another thread (the window does this so it never freezes while
/// FFmpeg comes up), and the result is a normal recording.
#[test]
fn recording_starts_from_another_thread() {
    if !have_ffmpeg() {
        return;
    }
    let dir = std::env::temp_dir().join("ferrous-e2e-control");
    let _ = std::fs::remove_dir_all(&dir);
    let mut settings = small_settings(&dir);
    settings.record.encoder.encoder = "libx264".into();
    let mut scenes = SceneCollection::default();
    scenes.add_item("Bars", SourceConfig::TestPattern { width: 64, height: 64 });
    let engine = Engine::start(settings, scenes);
    let control = engine.control();
    let path = std::thread::spawn(move || control.start_recording()).join().unwrap().expect("starts");
    assert!(engine.is_recording());
    std::thread::sleep(Duration::from_secs(2));
    engine.stop_recording();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(2));
    assert!(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 0, "a file was written: {}", path.display());
}
