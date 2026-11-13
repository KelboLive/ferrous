//! Imports OBS Studio scene collections (`%APPDATA%\obs-studio\basic\scenes\*.json`).
//!
//! Scenes, nested scenes and groups, positions, scale, crop, visibility and
//! locks, the common source types, the common filters, and the default
//! transition are carried over. Anything without an Ferrous equivalent is
//! listed in the report instead of being silently dropped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::filters::{AudioFilter, Filter, VideoFilter};
use crate::scene::{PendingLayout, Scene, SceneCollection, SceneItem, SourceConfig, TextAlign, Transform, Transition, TransitionKind};

#[derive(Debug, Default)]
pub struct ImportReport {
    pub scenes: usize,
    pub items: usize,
    pub filters: usize,
    /// Human-readable notes about things that couldn't be imported exactly.
    pub skipped: Vec<String>,
}

/// Where OBS keeps scene collections on this machine.
pub fn obs_collections_dir() -> Option<PathBuf> {
    let d = dirs::config_dir()?.join("obs-studio").join("basic").join("scenes");
    d.is_dir().then_some(d)
}

/// OBS stores colours as 0xAABBGGRR.
fn abgr_to_bgra(v: u64) -> [u8; 4] {
    let r = (v & 0xff) as u8;
    let g = ((v >> 8) & 0xff) as u8;
    let b = ((v >> 16) & 0xff) as u8;
    let a = ((v >> 24) & 0xff) as u8;
    [b, g, r, a]
}

fn f(v: &Value, k: &str, d: f64) -> f32 {
    v[k].as_f64().unwrap_or(d) as f32
}

/// Maps one OBS source to an Ferrous source config, or explains why not.
fn map_source(kind: &str, s: &Value, name: &str) -> Result<SourceConfig, String> {
    Ok(match kind {
        "monitor_capture" => SourceConfig::DisplayCapture {
            monitor_index: s["monitor"].as_u64().unwrap_or(0) as usize,
            capture_cursor: s["capture_cursor"].as_bool().unwrap_or(true),
        },
        "game_capture" => {
            // "Title:WindowClass:process.exe"; "any_fullscreen" follows whatever game is up.
            let exe = s["window"].as_str().unwrap_or_default().rsplit(':').next().unwrap_or_default().to_string();
            let program = if s["capture_mode"].as_str() == Some("window") { exe } else { String::new() };
            SourceConfig::GameCapture { program, capture_cursor: s["capture_cursor"].as_bool().unwrap_or(true) }
        }
        "window_capture" => {
            // "Title:WindowClass:process.exe"
            let w = s["window"].as_str().unwrap_or_default();
            let title = w.split(':').next().unwrap_or_default().replace("#3A", ":");
            SourceConfig::WindowCapture { title_contains: title, capture_cursor: s["cursor"].as_bool().unwrap_or(true) }
        }
        "image_source" => SourceConfig::Image { path: PathBuf::from(s["file"].as_str().unwrap_or_default()) },
        "color_source" | "color_source_v2" | "color_source_v3" => SourceConfig::Color {
            bgra: abgr_to_bgra(s["color"].as_u64().unwrap_or(0xffffffff)),
            width: s["width"].as_u64().unwrap_or(1920) as u32,
            height: s["height"].as_u64().unwrap_or(1080) as u32,
        },
        "ffmpeg_source" => {
            let local = s["is_local_file"].as_bool().unwrap_or(true);
            let input = if local { s["local_file"].as_str() } else { s["input"].as_str() };
            SourceConfig::Media {
                input: input.unwrap_or_default().to_string(),
                looping: s["looping"].as_bool().unwrap_or(false),
                hw_decode: s["hw_decode"].as_bool().unwrap_or(true),
            }
        }
        "dshow_input" => {
            // "Device Name:\\?\usb#…"
            let id = s["video_device_id"].as_str().unwrap_or_default();
            let device = id.split(":\\\\").next().unwrap_or(id).trim_end_matches(':').to_string();
            let resolution = (s["res_type"].as_i64() == Some(1))
                .then(|| s["resolution"].as_str().and_then(|r| r.split_once('x')))
                .flatten()
                .and_then(|(w, h)| Some([w.parse().ok()?, h.parse().ok()?]));
            SourceConfig::VideoDevice { device, resolution, fps: None, audio_device: None }
        }
        "text_gdiplus" | "text_gdiplus_v2" | "text_gdiplus_v3" | "text_ft2_source" | "text_ft2_source_v2" => {
            let from_file = (s["read_from_file"].as_bool() == Some(true) || s["from_file"].as_bool() == Some(true))
                .then(|| s["file"].as_str().or(s["text_file"].as_str()).map(PathBuf::from))
                .flatten();
            let face = s["font"]["face"].as_str().unwrap_or_default();
            let gdi = kind.starts_with("text_gdiplus");
            // GDI+ colours have no alpha in the low byte order the same way; opacity is separate.
            let mut bgra = abgr_to_bgra(s[if gdi { "color" } else { "color1" }].as_u64().unwrap_or(0xffffffff));
            if gdi {
                bgra[3] = (s["opacity"].as_u64().unwrap_or(100) * 255 / 100) as u8;
            }
            let outline = if s["outline"].as_bool() == Some(true) {
                let mut c = abgr_to_bgra(s["outline_color"].as_u64().unwrap_or(0xff000000));
                if gdi {
                    c[3] = (s["outline_opacity"].as_u64().unwrap_or(100) * 255 / 100) as u8;
                }
                (c, s["outline_size"].as_u64().unwrap_or(2) as u32)
            } else {
                ([0, 0, 0, 255], 0)
            };
            SourceConfig::Text {
                text: s["text"].as_str().unwrap_or_default().to_string(),
                from_file,
                // A face name maps to a font file only on the user's PC; the
                // text source falls back to the default font if it can't be found.
                font: face_to_file(face),
                size: f(&s["font"], "size", 72.0),
                bgra,
                outline,
                background: if gdi && s["bk_opacity"].as_u64().unwrap_or(0) > 0 {
                    let mut c = abgr_to_bgra(s["bk_color"].as_u64().unwrap_or(0));
                    c[3] = (s["bk_opacity"].as_u64().unwrap_or(0) * 255 / 100) as u8;
                    c
                } else {
                    [0, 0, 0, 0]
                },
                align: match s["align"].as_str() {
                    Some("center") => TextAlign::Center,
                    Some("right") => TextAlign::Right,
                    _ => TextAlign::Left,
                },
                wrap_width: if s["extents"].as_bool() == Some(true) && s["extents_wrap"].as_bool() == Some(true) {
                    s["extents_cx"].as_u64().unwrap_or(0) as u32
                } else {
                    0
                },
            }
        }
        "browser_source" => {
            let url = if s["is_local_file"].as_bool() == Some(true) {
                format!("file:///{}", s["local_file"].as_str().unwrap_or_default().replace('\\', "/"))
            } else {
                s["url"].as_str().unwrap_or("https://obsproject.com/browser-source").to_string()
            };
            SourceConfig::Browser {
                url,
                width: s["width"].as_u64().unwrap_or(800) as u32,
                height: s["height"].as_u64().unwrap_or(600) as u32,
                fps: s["fps"].as_u64().unwrap_or(30) as u32,
                css: s["css"].as_str().unwrap_or_default().to_string(),
            }
        }
        "wasapi_process_output_capture" => {
            let w = s["window"].as_str().unwrap_or_default();
            SourceConfig::ApplicationAudio { process: w.rsplit(':').next().unwrap_or_default().to_string() }
        }
        // Anything else comes from an OBS plugin: run it through the OBS
        // plugin bridge with its original settings.
        other => SourceConfig::ObsPlugin { type_id: other.to_string(), name: name.to_string(), settings: s.to_string() },
    })
}

/// "Arial" -> "arial.ttf" (Windows font file names for common faces).
fn face_to_file(face: &str) -> String {
    let known: &[(&str, &str)] = &[
        ("Arial", "arial.ttf"),
        ("Segoe UI", "segoeui.ttf"),
        ("Verdana", "verdana.ttf"),
        ("Tahoma", "tahoma.ttf"),
        ("Impact", "impact.ttf"),
        ("Calibri", "calibri.ttf"),
        ("Consolas", "consola.ttf"),
        ("Times New Roman", "times.ttf"),
        ("Courier New", "cour.ttf"),
        ("Comic Sans MS", "comic.ttf"),
    ];
    known.iter().find(|(f, _)| f.eq_ignore_ascii_case(face)).map(|(_, file)| file.to_string()).unwrap_or_default()
}

fn map_video_filter(id: &str, s: &Value) -> Option<VideoFilter> {
    Some(match id {
        "chroma_key_filter" | "chroma_key_filter_v2" => VideoFilter::ChromaKey {
            key_rgb: match s["key_color_type"].as_str().unwrap_or("green") {
                "blue" => [0, 0, 255],
                "magenta" => [255, 0, 255],
                "custom" => {
                    let c = abgr_to_bgra(s["key_color"].as_u64().unwrap_or(0xff00ff00));
                    [c[2], c[1], c[0]]
                }
                _ => [0, 255, 0],
            },
            similarity: s["similarity"].as_u64().unwrap_or(400) as u32,
            smoothness: s["smoothness"].as_u64().unwrap_or(80) as u32,
            spill: s["spill"].as_u64().unwrap_or(100) as u32,
        },
        "color_filter" | "color_filter_v2" => VideoFilter::ColorCorrection {
            gamma: f(s, "gamma", 0.0),
            contrast: f(s, "contrast", 0.0),
            brightness: f(s, "brightness", 0.0),
            saturation: f(s, "saturation", 0.0),
            hue_shift: f(s, "hue_shift", 0.0),
            opacity: f(s, "opacity", 1.0).min(if s["opacity"].as_f64().unwrap_or(1.0) > 1.0 { 100.0 } else { 1.0 })
                / if s["opacity"].as_f64().unwrap_or(1.0) > 1.0 { 100.0 } else { 1.0 },
        },
        "scroll_filter" => VideoFilter::Scroll { speed_x: f(s, "speed_x", 0.0), speed_y: f(s, "speed_y", 0.0) },
        "clut_filter" => VideoFilter::Lut { path: PathBuf::from(s["image_path"].as_str().unwrap_or_default()), amount: f(s, "clut_amount", 1.0) },
        _ => return None,
    })
}

fn map_audio_filter(id: &str, s: &Value) -> Option<AudioFilter> {
    Some(match id {
        "noise_suppress_filter" | "noise_suppress_filter_v2" => AudioFilter::NoiseSuppression,
        "noise_gate_filter" => AudioFilter::NoiseGate {
            open_db: f(s, "open_threshold", -26.0),
            close_db: f(s, "close_threshold", -32.0),
            attack_ms: f(s, "attack_time", 25.0),
            hold_ms: f(s, "hold_time", 200.0),
            release_ms: f(s, "release_time", 150.0),
        },
        "compressor_filter" => AudioFilter::Compressor {
            ratio: f(s, "ratio", 10.0),
            threshold_db: f(s, "threshold", -18.0),
            attack_ms: f(s, "attack_time", 6.0),
            release_ms: f(s, "release_time", 60.0),
            makeup_db: f(s, "output_gain", 0.0),
        },
        "limiter_filter" => AudioFilter::Limiter { threshold_db: f(s, "threshold", -6.0), release_ms: f(s, "release_time", 60.0) },
        "gain_filter" => AudioFilter::Gain { db: f(s, "db", 0.0) },
        _ => return None,
    })
}

pub fn import_file(path: &Path) -> anyhow::Result<(SceneCollection, ImportReport)> {
    import_json(&std::fs::read_to_string(path)?)
}

/// The built-in luma wipe names (OBS's set, generated rather than shipped as images).
pub const LUMA_WIPES: &[&str] = &[
    "linear-h", "linear-v", "linear-topleft", "linear-topright", "barndoor-h", "barndoor-v", "barndoor-topleft",
    "barndoor-botleft", "blinds-h", "blinds-v", "box-topleft", "box-topright", "box-botleft", "box-botright", "iris",
    "square", "circles", "clock", "fan", "burst", "spiral", "checkerboard-small", "zigzag-h", "zigzag-v", "sinus9",
    "cloud", "dissolve",
];

fn map_transition(id: &str, s: &Value, report: &mut ImportReport) -> TransitionKind {
    use crate::TransitionDirection as D;
    let dir = |v: &Value| match v.as_str().unwrap_or("left") {
        "right" => D::Right,
        "up" => D::Up,
        "down" => D::Down,
        _ => D::Left,
    };
    match id {
        "cut_transition" => TransitionKind::Cut,
        "fade_transition" => TransitionKind::Fade,
        "fade_to_color_transition" => {
            // OBS stores 0xAABBGGRR.
            let c = s["color"].as_u64().unwrap_or(0xff000000) as u32;
            TransitionKind::FadeToColor { bgra: [(c >> 16) as u8, (c >> 8) as u8, c as u8, 255] }
        }
        "swipe_transition" => TransitionKind::Swipe { direction: dir(&s["direction"]) },
        "slide_transition" => TransitionKind::Slide { direction: dir(&s["direction"]) },
        "stinger_transition" if s["track_matte_enabled"].as_bool().unwrap_or(false) => TransitionKind::Matte {
            path: s["path"].as_str().unwrap_or_default().into(),
            layout: match s["track_matte_layout"].as_str().unwrap_or_default() {
                l if l.contains("horizontal") => crate::scene::MatteLayout::Horizontal,
                l if l.contains("vertical") => crate::scene::MatteLayout::Vertical,
                _ => crate::scene::MatteLayout::Luma,
            },
            invert: s["invert_matte"].as_bool().unwrap_or(false),
        },
        "stinger_transition" => TransitionKind::Stinger {
            path: s["path"].as_str().unwrap_or_default().into(),
            cut_point_ms: s["transition_point"].as_u64().unwrap_or(500) as u32,
        },
        "wipe_transition" => {
            let img = s["luma_image"].as_str().unwrap_or("linear-h.png");
            let stem = img.rsplit(['/', '\\']).next().unwrap_or(img).trim_end_matches(".png");
            let (wipe, image) = if LUMA_WIPES.contains(&stem) {
                (stem.to_string(), None)
            } else if std::path::Path::new(img).is_absolute() {
                ("linear-h".to_string(), Some(img.into()))
            } else {
                report.skipped.push(format!("luma wipe \"{img}\" isn't built in; using linear-h"));
                ("linear-h".to_string(), None)
            };
            TransitionKind::Luma {
                wipe,
                image,
                softness: s["luma_softness"].as_f64().unwrap_or(0.03) as f32,
                invert: s["luma_invert"].as_bool().unwrap_or(false),
            }
        }
        other => {
            report.skipped.push(format!("transition \"{other}\" has no equivalent; using Fade"));
            TransitionKind::Fade
        }
    }
}

pub fn import_json(text: &str) -> anyhow::Result<(SceneCollection, ImportReport)> {
    let root: Value = serde_json::from_str(text)?;
    let sources = root["sources"].as_array().cloned().unwrap_or_default();
    let groups = root["groups"].as_array().cloned().unwrap_or_default();
    let by_name: HashMap<String, &Value> = sources.iter().chain(groups.iter()).filter_map(|s| Some((s["name"].as_str()?.to_string(), s))).collect();
    let mut report = ImportReport::default();

    // Scenes in OBS's display order, falling back to source order.
    let mut scene_names: Vec<String> = root["scene_order"]
        .as_array()
        .map(|a| a.iter().filter_map(|o| o["name"].as_str().map(String::from)).collect())
        .unwrap_or_default();
    if scene_names.is_empty() {
        scene_names = sources.iter().filter(|s| s["id"] == "scene").filter_map(|s| s["name"].as_str().map(String::from)).collect();
    }
    // Groups become scenes too (used as nested scenes), appended after.
    let group_names: Vec<String> = groups.iter().filter_map(|g| g["name"].as_str().map(String::from)).collect();

    let mut coll = SceneCollection::default();
    coll.scenes.clear();
    for name in scene_names.iter().chain(group_names.iter()) {
        let Some(src) = by_name.get(name) else { continue };
        let mut scene = Scene::new(name.clone());
        // OBS lists items bottom-to-top, same as Ferrous.
        for it in src["settings"]["items"].as_array().cloned().unwrap_or_default() {
            let item_name = it["name"].as_str().unwrap_or_default().to_string();
            let Some(s) = by_name.get(&item_name) else {
                report.skipped.push(format!("\"{item_name}\": source missing from the file"));
                continue;
            };
            let kind = s["id"].as_str().unwrap_or_default();
            let cfg = if kind == "scene" || kind == "group" {
                Ok(SourceConfig::Scene { name: item_name.clone() })
            } else {
                map_source(kind, &s["settings"], &item_name)
            };
            let cfg = match cfg {
                Ok(c) => c,
                Err(why) => {
                    report.skipped.push(why);
                    continue;
                }
            };
            let transform = Transform {
                x: f(&it["pos"], "x", 0.0),
                y: f(&it["pos"], "y", 0.0),
                scale_x: f(&it["scale"], "x", 1.0),
                scale_y: f(&it["scale"], "y", 1.0),
                crop: [
                    it["crop_left"].as_u64().unwrap_or(0) as u32,
                    it["crop_top"].as_u64().unwrap_or(0) as u32,
                    it["crop_right"].as_u64().unwrap_or(0) as u32,
                    it["crop_bottom"].as_u64().unwrap_or(0) as u32,
                ],
                opacity: 1.0,
                rotation: it["rot"].as_f64().unwrap_or(0.0) as f32,
            };
            let bounds_type = it["bounds_type"].as_u64().unwrap_or(0) as u32;
            let pending = PendingLayout {
                align: it["align"].as_u64().unwrap_or(5) as u32,
                bounds: (bounds_type != 0).then(|| (f(&it["bounds"], "x", 0.0), f(&it["bounds"], "y", 0.0), bounds_type, it["bounds_align"].as_u64().unwrap_or(0) as u32)),
            };
            let mut video_filters = Vec::new();
            let mut audio_filters = Vec::new();
            for fl in s["filters"].as_array().cloned().unwrap_or_default() {
                let fid = fl["id"].as_str().unwrap_or_default();
                let enabled = fl["enabled"].as_bool().unwrap_or(true);
                if let Some(vf) = map_video_filter(fid, &fl["settings"]) {
                    video_filters.push(Filter { enabled, filter: vf });
                    report.filters += 1;
                } else if let Some(af) = map_audio_filter(fid, &fl["settings"]) {
                    audio_filters.push(Filter { enabled, filter: af });
                    report.filters += 1;
                } else {
                    report.skipped.push(format!("\"{item_name}\": filter `{fid}` isn't available"));
                }
            }
            let id = coll.alloc_id();
            scene.items.push(SceneItem {
                id,
                name: item_name,
                source: cfg,
                visible: it["visible"].as_bool().unwrap_or(true),
                locked: it["locked"].as_bool().unwrap_or(false),
                keep_running: false,
                transform,
                video_filters,
                audio_filters,
                audio_tracks: s["mixers"].as_u64().map(|m| m as u32 & 0x3f).filter(|m| *m != 0).unwrap_or(1),
                audio_monitor: s["monitoring_type"].as_u64().unwrap_or(0).min(2) as u8,
                audio_volume: s["volume"].as_f64().map(|v| v as f32).unwrap_or(1.0).clamp(0.0, 2.0),
                audio_muted: s["muted"].as_bool().unwrap_or(false),
                // OBS stores the sync offset in nanoseconds (negative = earlier, which we can't do).
                audio_sync_ms: s["sync"].as_i64().map(|ns| (ns / 1_000_000).clamp(0, 5000) as u32).unwrap_or(0),
                pending_layout: Some(pending),
            });
            report.items += 1;
        }
        coll.scenes.push(scene);
        report.scenes += 1;
    }
    if coll.scenes.is_empty() {
        anyhow::bail!("no scenes found; is this an OBS scene collection file?");
    }
    let current = root["current_program_scene"].as_str().or(root["current_scene"].as_str()).unwrap_or_default();
    coll.current = coll.scenes.iter().position(|s| s.name == current).unwrap_or(0);
    let current_tr = root["current_transition"].as_str().unwrap_or("Fade");
    let tr_def = root["transitions"].as_array().and_then(|a| a.iter().find(|t| t["name"].as_str() == Some(current_tr)).cloned());
    let kind = match (current_tr, tr_def.as_ref().and_then(|t| t["id"].as_str())) {
        ("Cut", None) => TransitionKind::Cut,
        (_, Some(id)) => map_transition(id, &tr_def.as_ref().unwrap()["settings"], &mut report),
        _ => TransitionKind::Fade,
    };
    coll.transition = Transition { kind, duration_ms: root["transition_duration"].as_u64().unwrap_or(300) as u32 };
    for name in ["DesktopAudioDevice1", "AuxAudioDevice1"] {
        if root[name].is_object() {
            report.skipped.push(format!("{name}: global audio device settings stay in your Ferrous profile"));
        }
    }
    Ok((coll, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "name": "Streaming",
      "current_scene": "Gaming", "current_program_scene": "Gaming",
      "current_transition": "Fade", "transition_duration": 450,
      "scene_order": [{"name": "Gaming"}, {"name": "BRB"}],
      "DesktopAudioDevice1": {"id": "wasapi_output_capture", "name": "Desktop Audio", "settings": {}},
      "sources": [
        {"id": "scene", "name": "Gaming", "settings": {"items": [
          {"name": "Display", "visible": true, "locked": true, "pos": {"x": 0, "y": 0}, "scale": {"x": 0.5, "y": 0.5},
           "align": 5, "bounds_type": 0, "crop_left": 10, "crop_top": 0, "crop_right": 0, "crop_bottom": 20, "rot": 0},
          {"name": "Cam", "visible": true, "pos": {"x": 1900, "y": 1060}, "scale": {"x": 1, "y": 1},
           "align": 10, "bounds_type": 2, "bounds": {"x": 480, "y": 270}, "bounds_align": 0, "rot": 0},
          {"name": "Ticker", "visible": false, "pos": {"x": 0, "y": 1000}, "scale": {"x": 1, "y": 1}, "align": 5, "rot": 15},
          {"name": "BRB", "visible": true, "pos": {"x": 0, "y": 0}, "scale": {"x": 1, "y": 1}, "align": 5},
          {"name": "Spectralizer", "visible": true, "pos": {"x": 0, "y": 0}, "scale": {"x": 1, "y": 1}, "align": 5}
        ]}},
        {"id": "scene", "name": "BRB", "settings": {"items": [
          {"name": "Bg", "visible": true, "pos": {"x": 0, "y": 0}, "scale": {"x": 1, "y": 1}, "align": 5}
        ]}},
        {"id": "monitor_capture", "name": "Display", "settings": {"monitor": 1, "capture_cursor": false}},
        {"id": "dshow_input", "name": "Cam", "settings": {"video_device_id": "HD Pro Webcam C920:\\\\?\\usb#vid_046d", "res_type": 1, "resolution": "1280x720"},
         "mixers": 3, "monitoring_type": 1, "volume": 0.5, "muted": true,
         "filters": [
           {"id": "chroma_key_filter_v2", "name": "Key", "enabled": true, "settings": {"key_color_type": "green", "similarity": 420}},
           {"id": "noise_suppress_filter_v2", "name": "NS", "settings": {}},
           {"id": "shader_filter", "name": "Fancy", "settings": {}}
         ]},
        {"id": "text_gdiplus_v3", "name": "Ticker", "settings": {"text": "Welcome!", "font": {"face": "Arial", "size": 48}, "color": 4278255615, "opacity": 100, "outline": true, "outline_size": 3, "outline_color": 4278190080}},
        {"id": "color_source_v3", "name": "Bg", "settings": {"color": 4282664004, "width": 1920, "height": 1080}},
        {"id": "spectralizer", "name": "Spectralizer", "settings": {}}
      ]
    }"#;

    #[test]
    fn imports_obs_collection() {
        let (c, r) = import_json(SAMPLE).unwrap();
        assert_eq!(r.scenes, 2);
        assert_eq!(c.scenes[0].name, "Gaming");
        assert_eq!(c.current, 0);
        assert_eq!(c.transition.duration_ms, 450);
        let g = &c.scenes[0];
        assert_eq!(g.items.len(), 5, "{:?}", r.skipped);
        // A plugin source becomes an OBS plugin source with its settings.
        assert!(matches!(&g.items[4].source, SourceConfig::ObsPlugin { type_id, .. } if type_id == "spectralizer"));
        assert!(r.skipped.iter().any(|s| s.contains("shader_filter")));

        let display = &g.items[0];
        assert_eq!(display.source, SourceConfig::DisplayCapture { monitor_index: 1, capture_cursor: false });
        assert!(display.locked);
        assert_eq!(display.transform.crop, [10, 0, 0, 20]);
        assert_eq!(display.transform.scale_x, 0.5);

        let cam = &g.items[1];
        assert!(matches!(&cam.source, SourceConfig::VideoDevice { device, resolution: Some([1280, 720]), .. } if device == "HD Pro Webcam C920"));
        assert_eq!(cam.video_filters.len(), 1);
        assert_eq!(cam.audio_filters.len(), 1);
        assert_eq!(cam.audio_tracks, 3);
        assert_eq!(cam.audio_monitor, 1);
        assert_eq!((cam.audio_volume, cam.audio_muted), (0.5, true));
        assert_eq!((display.audio_volume, display.audio_muted), (1.0, false));
        // Bottom-right aligned 480x270 box, scale-inner: a 1280x720 cam fills it exactly.
        let t = cam.pending_layout.as_ref().unwrap().resolve(&cam.transform, 1280, 720);
        assert!((t.scale_x - 0.375).abs() < 1e-4);
        assert!((t.x - 1420.0).abs() < 0.01 && (t.y - 790.0).abs() < 0.01, "{t:?}");

        let ticker = &g.items[2];
        assert!(!ticker.visible);
        match &ticker.source {
            SourceConfig::Text { text, font, size, bgra, outline, .. } => {
                assert_eq!(text, "Welcome!");
                assert_eq!(font, "arial.ttf");
                assert_eq!(*size, 48.0);
                assert_eq!(*bgra, [0, 255, 255, 255]); // 0xFF00FFFF = yellow (R=FF,G=FF,B=00)
                assert_eq!(outline.1, 3);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(g.items[3].source, SourceConfig::Scene { name: "BRB".into() });
        assert_eq!(c.scenes[1].items[0].source, SourceConfig::Color { bgra: [0x44, 0x44, 0x44, 0xff], width: 1920, height: 1080 });
    }

    #[test]
    fn top_left_align_without_bounds_is_unchanged() {
        let p = PendingLayout { align: 5, bounds: None };
        let t = Transform { x: 100.0, y: 50.0, ..Default::default() };
        let r = p.resolve(&t, 640, 480);
        assert_eq!((r.x, r.y), (100.0, 50.0));
        let centered = PendingLayout { align: 0, bounds: None }.resolve(&t, 640, 480);
        assert_eq!((centered.x, centered.y), (-220.0, -190.0));
    }
}

#[cfg(test)]
mod transition_import_tests {
    use super::*;

    fn with_transition(def: &str, current: &str) -> TransitionKind {
        let json = format!(
            r#"{{"current_scene": "A", "current_transition": "{current}", "transitions": [{def}],
                "sources": [{{"id": "scene", "name": "A", "settings": {{"items": []}}}}]}}"#
        );
        import_json(&json).unwrap().0.transition.kind
    }

    #[test]
    fn imports_luma_wipe_and_others() {
        let k = with_transition(r#"{"name": "Wipe", "id": "wipe_transition", "settings": {"luma_image": "clock.png", "luma_softness": 0.2, "luma_invert": true}}"#, "Wipe");
        assert_eq!(k, TransitionKind::Luma { wipe: "clock".into(), image: None, softness: 0.2, invert: true });
        let k = with_transition(r#"{"name": "S", "id": "swipe_transition", "settings": {"direction": "up"}}"#, "S");
        assert_eq!(k, TransitionKind::Swipe { direction: crate::TransitionDirection::Up });
        let k = with_transition(r#"{"name": "St", "id": "stinger_transition", "settings": {"path": "C:/x.webm", "transition_point": 900}}"#, "St");
        assert_eq!(k, TransitionKind::Stinger { path: "C:/x.webm".into(), cut_point_ms: 900 });
        assert_eq!(with_transition(r#"{"name": "Z", "id": "zoom_thing", "settings": {}}"#, "Z"), TransitionKind::Fade);
        assert_eq!(with_transition(r#"{"name": "Q", "id": "fade_transition", "settings": {}}"#, "Cut"), TransitionKind::Cut);
    }
}
