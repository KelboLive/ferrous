//! Maps obs-websocket requests onto Ferrous. Runs on the UI thread so
//! it sees exactly the state the user sees.

use std::sync::Arc;

use ferrous_core::{HotkeyAction, SourceConfig};
use ferrous_remote::{intent, status, RemoteCall, RemoteServer, Reply};
use serde_json::{json, Value};

use crate::app::FerrousApp;

type Res = Reply;

fn missing(field: &str) -> (u16, String) {
    (status::MISSING_REQUEST_FIELD, format!("missing field `{field}`"))
}

fn not_found(what: &str) -> (u16, String) {
    (status::RESOURCE_NOT_FOUND, format!("{what} not found"))
}

fn str_field<'a>(d: &'a Value, f: &str) -> Result<&'a str, (u16, String)> {
    d[f].as_str().ok_or_else(|| missing(f))
}

/// Request types handled below (reported by GetVersion).
pub const REQUESTS: &[&str] = &[
    "GetVersion", "GetStats", "GetSceneList", "GetCurrentProgramScene", "SetCurrentProgramScene",
    "GetCurrentPreviewScene", "SetCurrentPreviewScene", "GetStudioModeEnabled", "SetStudioModeEnabled",
    "TriggerStudioModeTransition", "GetSceneItemList", "GetSceneItemId", "GetSceneItemEnabled",
    "SetSceneItemEnabled", "GetInputList", "GetInputSettings", "SetInputSettings", "GetInputMute",
    "SetInputMute", "ToggleInputMute", "GetInputVolume", "SetInputVolume", "GetRecordStatus", "StartRecord",
    "StopRecord", "ToggleRecord", "GetStreamStatus", "StartStream", "StopStream", "ToggleStream",
    "GetReplayBufferStatus", "StartReplayBuffer", "StopReplayBuffer", "ToggleReplayBuffer", "SaveReplayBuffer",
    "BroadcastCustomEvent",
];

pub struct Remote {
    pub server: RemoteServer,
    pub calls: crossbeam_channel::Receiver<RemoteCall>,
    pub config: ferrous_core::RemoteSettings,
}

pub fn start(cfg: &ferrous_core::RemoteSettings, ctx: &eframe::egui::Context) -> anyhow::Result<Remote> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let ctx = ctx.clone();
    let server = RemoteServer::start(cfg.port, Some(cfg.password.clone()), tx, Arc::new(move || ctx.request_repaint()))?;
    Ok(Remote { server, calls: rx, config: cfg.clone() })
}

fn input_kind(src: &SourceConfig) -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Borrowed(match src {
        // OBS plugin sources report their real OBS kind.
        SourceConfig::ObsPlugin { type_id, .. } => return std::borrow::Cow::Owned(type_id.clone()),
        SourceConfig::DisplayCapture { .. } => "monitor_capture",
        SourceConfig::WindowCapture { .. } => "window_capture",
        SourceConfig::GameCapture { .. } => "game_capture",
        SourceConfig::Image { .. } => "image_source",
        SourceConfig::Color { .. } => "color_source_v3",
        SourceConfig::TestPattern { .. } => "ferrous_test_pattern",
        SourceConfig::VideoDevice { .. } => "dshow_input",
        SourceConfig::Media { .. } => "ffmpeg_source",
        SourceConfig::ApplicationAudio { .. } => "wasapi_process_output_capture",
        SourceConfig::Text { .. } => "text_gdiplus_v3",
        SourceConfig::Scene { .. } => "scene",
        SourceConfig::Clone { .. } => "source_clone",
        SourceConfig::Browser { .. } => "browser_source",
        SourceConfig::Plugin { .. } => "ferrous_plugin_source",
        SourceConfig::LinkReceiver { .. } => "teleport-source",
    })
}

impl FerrousApp {
    pub fn handle_remote(&mut self, rt: &str, d: &Value) -> Res {
        let n = self.scenes.scenes.len();
        let scene_idx = |app: &Self, name: &str| app.scenes.scenes.iter().position(|s| s.name == name);
        match rt {
            "GetVersion" => Ok(json!({
                "obsVersion": "30.2.0",
                "obsWebSocketVersion": "5.5.0",
                "rpcVersion": 1,
                "availableRequests": REQUESTS,
                "supportedImageFormats": [],
                "platform": if cfg!(windows) { "windows" } else { "linux" },
                "platformDescription": format!("Ferrous {}", env!("CARGO_PKG_VERSION")),
            })),
            "GetStats" => {
                let st = self.engine.status();
                Ok(json!({
                    "activeFps": st.fps, "averageFrameRenderTime": st.render_ms,
                    "renderSkippedFrames": st.lagged_frames, "renderTotalFrames": 0,
                    "outputSkippedFrames": st.recording.frames_skipped + st.streaming.frames_skipped,
                    "outputTotalFrames": st.recording.frames_written + st.streaming.frames_written,
                    "cpuUsage": 0.0, "memoryUsage": 0.0, "availableDiskSpace": 0.0,
                    "webSocketSessionIncomingMessages": 0, "webSocketSessionOutgoingMessages": 0,
                }))
            }
            "GetSceneList" => {
                // obs-websocket lists scenes bottom-to-top with sceneIndex.
                let scenes: Vec<Value> = self.scenes.scenes.iter().enumerate().rev()
                    .map(|(i, s)| json!({ "sceneName": s.name, "sceneIndex": n - 1 - i, "sceneUuid": format!("ferrous-scene-{i}") }))
                    .collect();
                let program = &self.scenes.scenes[self.scenes.current.min(n - 1)].name;
                let preview = self.studio_mode.then(|| self.scenes.scenes[self.studio_scene.min(n - 1)].name.clone());
                Ok(json!({ "currentProgramSceneName": program, "currentPreviewSceneName": preview, "scenes": scenes }))
            }
            "GetCurrentProgramScene" => {
                let name = &self.scenes.scenes[self.scenes.current.min(n - 1)].name;
                Ok(json!({ "sceneName": name, "currentProgramSceneName": name }))
            }
            "SetCurrentProgramScene" => {
                let name = str_field(d, "sceneName")?;
                let i = scene_idx(self, name).ok_or_else(|| not_found("scene"))?;
                self.scenes.current = i;
                Ok(Value::Null)
            }
            "GetCurrentPreviewScene" | "SetCurrentPreviewScene" | "TriggerStudioModeTransition" if !self.studio_mode => {
                Err((status::STUDIO_MODE_NOT_ACTIVE, "studio mode is off".into()))
            }
            "GetCurrentPreviewScene" => {
                let name = &self.scenes.scenes[self.studio_scene.min(n - 1)].name;
                Ok(json!({ "sceneName": name, "currentPreviewSceneName": name }))
            }
            "SetCurrentPreviewScene" => {
                let name = str_field(d, "sceneName")?;
                self.studio_scene = scene_idx(self, name).ok_or_else(|| not_found("scene"))?;
                Ok(Value::Null)
            }
            "TriggerStudioModeTransition" => {
                self.perform(HotkeyAction::Transition);
                Ok(Value::Null)
            }
            "GetStudioModeEnabled" => Ok(json!({ "studioModeEnabled": self.studio_mode })),
            "SetStudioModeEnabled" => {
                let on = d["studioModeEnabled"].as_bool().ok_or_else(|| missing("studioModeEnabled"))?;
                if on != self.studio_mode {
                    self.studio_mode = on;
                    self.studio_scene = self.scenes.current;
                }
                Ok(Value::Null)
            }
            "GetSceneItemList" => {
                let i = scene_idx(self, str_field(d, "sceneName")?).ok_or_else(|| not_found("scene"))?;
                let items: Vec<Value> = self.scenes.scenes[i].items.iter().enumerate().map(|(idx, it)| json!({
                    "sceneItemId": it.id, "sceneItemIndex": idx, "sourceName": it.name,
                    "sceneItemEnabled": it.visible, "sceneItemLocked": it.locked,
                    "inputKind": input_kind(&it.source), "isGroup": false,
                    "sourceType": if matches!(it.source, SourceConfig::Scene { .. }) { "OBS_SOURCE_TYPE_SCENE" } else { "OBS_SOURCE_TYPE_INPUT" },
                })).collect();
                Ok(json!({ "sceneItems": items }))
            }
            "GetSceneItemId" => {
                let i = scene_idx(self, str_field(d, "sceneName")?).ok_or_else(|| not_found("scene"))?;
                let src = str_field(d, "sourceName")?;
                let it = self.scenes.scenes[i].items.iter().find(|it| it.name == src).ok_or_else(|| not_found("source"))?;
                Ok(json!({ "sceneItemId": it.id }))
            }
            "GetSceneItemEnabled" | "SetSceneItemEnabled" => {
                let i = scene_idx(self, str_field(d, "sceneName")?).ok_or_else(|| not_found("scene"))?;
                let id = d["sceneItemId"].as_u64().ok_or_else(|| missing("sceneItemId"))?;
                let it = self.scenes.scenes[i].items.iter_mut().find(|it| it.id == id).ok_or_else(|| not_found("scene item"))?;
                if rt == "SetSceneItemEnabled" {
                    it.visible = d["sceneItemEnabled"].as_bool().ok_or_else(|| missing("sceneItemEnabled"))?;
                    Ok(Value::Null)
                } else {
                    Ok(json!({ "sceneItemEnabled": it.visible }))
                }
            }
            "GetInputList" => {
                let mut inputs: Vec<Value> = self.engine.mixer().channels().iter()
                    .filter(|c| c.id == "desktop" || c.id == "mic")
                    .map(|c| json!({ "inputName": *c.name.lock(), "inputKind": if c.id == "desktop" { "wasapi_output_capture" } else { "wasapi_input_capture" }, "unversionedInputKind": "wasapi" }))
                    .collect();
                for s in &self.scenes.scenes {
                    for it in &s.items {
                        if !matches!(it.source, SourceConfig::Scene { .. }) {
                            inputs.push(json!({ "inputName": it.name, "inputKind": input_kind(&it.source), "unversionedInputKind": input_kind(&it.source) }));
                        }
                    }
                }
                Ok(json!({ "inputs": inputs }))
            }
            "GetInputSettings" | "SetInputSettings" => {
                let name = str_field(d, "inputName")?.to_string();
                let item = self.scenes.scenes.iter_mut().flat_map(|s| s.items.iter_mut()).find(|i| i.name == name).ok_or_else(|| not_found("input"))?;
                if rt == "GetInputSettings" {
                    let mut settings = serde_json::to_value(&item.source).unwrap_or(json!({}));
                    if let SourceConfig::Browser { url, .. } = &item.source {
                        settings["url"] = json!(url);
                    }
                    return Ok(json!({ "inputSettings": settings, "inputKind": input_kind(&item.source) }));
                }
                let s = &d["inputSettings"];
                // The settings alert/overlay tools change most: text, URL, file.
                match &mut item.source {
                    SourceConfig::Text { text, .. } => {
                        if let Some(t) = s["text"].as_str() { *text = t.to_string(); }
                    }
                    SourceConfig::Browser { url, css, .. } => {
                        if let Some(u) = s["url"].as_str() { *url = u.to_string(); }
                        if let Some(c) = s["css"].as_str() { *css = c.to_string(); }
                    }
                    SourceConfig::Media { input, looping, .. } => {
                        if let Some(f) = s["local_file"].as_str().or(s["input"].as_str()) { *input = f.to_string(); }
                        if let Some(l) = s["looping"].as_bool() { *looping = l; }
                    }
                    SourceConfig::Image { path } => {
                        if let Some(f) = s["file"].as_str() { *path = f.into(); }
                    }
                    _ => return Err((status::INVALID_REQUEST_FIELD, "this input's settings can't be changed remotely".into())),
                }
                Ok(Value::Null)
            }
            "GetInputMute" | "SetInputMute" | "ToggleInputMute" | "GetInputVolume" | "SetInputVolume" => {
                let name = str_field(d, "inputName")?;
                let ch = self.engine.mixer().channels().into_iter().find(|c| *c.name.lock() == name).ok_or_else(|| not_found("audio input"))?;
                match rt {
                    "GetInputMute" => Ok(json!({ "inputMuted": ch.muted() })),
                    "SetInputMute" => {
                        ch.set_muted(d["inputMuted"].as_bool().ok_or_else(|| missing("inputMuted"))?);
                        Ok(Value::Null)
                    }
                    "ToggleInputMute" => {
                        ch.set_muted(!ch.muted());
                        Ok(json!({ "inputMuted": ch.muted() }))
                    }
                    "GetInputVolume" => {
                        let v = ch.volume();
                        Ok(json!({ "inputVolumeMul": v, "inputVolumeDb": if v > 0.0 { 20.0 * v.log10() } else { -100.0 } }))
                    }
                    _ => {
                        let v = if let Some(m) = d["inputVolumeMul"].as_f64() {
                            m as f32
                        } else if let Some(db) = d["inputVolumeDb"].as_f64() {
                            10f32.powf(db as f32 / 20.0)
                        } else {
                            return Err(missing("inputVolumeMul"));
                        };
                        ch.set_volume(v);
                        Ok(Value::Null)
                    }
                }
            }
            "GetRecordStatus" => {
                let st = self.engine.status().recording;
                Ok(json!({ "outputActive": st.active, "outputPaused": false, "outputTimecode": crate::widgets::fmt_duration(st.elapsed) + ".000", "outputDuration": st.elapsed.as_millis() as u64, "outputBytes": 0 }))
            }
            "GetStreamStatus" => {
                let st = self.engine.status().streaming;
                Ok(json!({ "outputActive": st.active, "outputReconnecting": st.reconnecting, "outputTimecode": crate::widgets::fmt_duration(st.elapsed) + ".000", "outputDuration": st.elapsed.as_millis() as u64, "outputSkippedFrames": st.frames_skipped, "outputTotalFrames": st.frames_written, "outputBytes": 0, "outputCongestion": 0.0 }))
            }
            "GetReplayBufferStatus" => Ok(json!({ "outputActive": self.engine.is_replay_active() })),
            "StartRecord" | "StopRecord" | "ToggleRecord" => {
                let on = self.engine.is_recording();
                match (rt, on) {
                    ("StartRecord", true) => return Err((status::OUTPUT_RUNNING, "already recording".into())),
                    ("StopRecord", false) => return Err((status::OUTPUT_NOT_RUNNING, "not recording".into())),
                    _ => {}
                }
                let path = self.engine.status().recording_path;
                self.perform(HotkeyAction::ToggleRecording);
                Ok(match rt {
                    "ToggleRecord" => json!({ "outputActive": !on }),
                    "StopRecord" => json!({ "outputPath": path.map(|p| p.display().to_string()) }),
                    _ => Value::Null,
                })
            }
            "StartStream" | "StopStream" | "ToggleStream" => {
                let on = self.engine.is_streaming();
                match (rt, on) {
                    ("StartStream", true) => return Err((status::OUTPUT_RUNNING, "already streaming".into())),
                    ("StopStream", false) => return Err((status::OUTPUT_NOT_RUNNING, "not streaming".into())),
                    _ => {}
                }
                self.perform(HotkeyAction::ToggleStreaming);
                Ok(if rt == "ToggleStream" { json!({ "outputActive": !on }) } else { Value::Null })
            }
            "StartReplayBuffer" | "StopReplayBuffer" | "ToggleReplayBuffer" => {
                let on = self.engine.is_replay_active();
                match (rt, on) {
                    ("StartReplayBuffer", true) => return Err((status::OUTPUT_RUNNING, "replay buffer already running".into())),
                    ("StopReplayBuffer", false) => return Err((status::OUTPUT_NOT_RUNNING, "replay buffer not running".into())),
                    _ => {}
                }
                self.perform(HotkeyAction::ToggleReplayBuffer);
                Ok(if rt == "ToggleReplayBuffer" { json!({ "outputActive": !on }) } else { Value::Null })
            }
            "SaveReplayBuffer" => self.engine.save_replay().map(|_| Value::Null).map_err(|e| (status::OUTPUT_NOT_RUNNING, e.to_string())),
            "BroadcastCustomEvent" => {
                if let Some(r) = &self.remote {
                    r.server.emit("CustomEvent", intent::GENERAL, d["eventData"].clone());
                }
                Ok(Value::Null)
            }
            _ => Err((status::UNKNOWN_REQUEST_TYPE, format!("{rt} isn't supported by Ferrous"))),
        }
    }
}
