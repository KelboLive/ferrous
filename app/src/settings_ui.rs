use std::sync::Arc;

use eframe::egui::{self, ComboBox, DragValue, RichText};
use ferrous_core::{
    AudioCodec, EncoderSettings, RateControl, RecordingFormat, RendererMode, Settings, STREAM_SERVICES,
};
use ferrous_encode::{catalog, ProbeResult};
use parking_lot::Mutex;

use crate::theme;

#[derive(Clone)]
pub enum ProbeState {
    Running,
    Done(Arc<ProbeResult>),
    Failed(String),
}

pub type SharedProbe = Arc<Mutex<ProbeState>>;

/// Runs the encoder probe on a background thread.
pub fn start_probe(ffmpeg_setting: &str, ctx: egui::Context) -> SharedProbe {
    let state = Arc::new(Mutex::new(ProbeState::Running));
    let path = ferrous_encode::ffmpeg::locate(ffmpeg_setting);
    let out = state.clone();
    std::thread::spawn(move || {
        let result = match ferrous_encode::probe(&path) {
            Ok(r) => ProbeState::Done(Arc::new(r)),
            Err(e) => ProbeState::Failed(format!("FFmpeg not found or not runnable ({}): {e}", path.display())),
        };
        *out.lock() = result;
        ctx.request_repaint();
    });
    state
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Stream,
    Recording,
    Video,
    Audio,
    Hotkeys,
    Link,
    Advanced,
}

pub struct SettingsWindow {
    pub open: bool,
    draft: Settings,
    tab: Tab,
    show_key: bool,
    outputs: Vec<String>,
    inputs: Vec<String>,
    scenes: Vec<(String, Vec<String>)>,
    /// Running Ferrous Link outputs as (name, listening port), set by the app.
    pub link_ports: Vec<(String, u16)>,
    presets: crate::presets_ui::PresetsUi,
}

pub enum SettingsAction {
    None,
    /// Boxed: returned every frame, and Settings is large.
    Apply(Box<Settings>),
    Reprobe,
}

impl SettingsWindow {
    pub fn new() -> Self {
        Self {
            open: false,
            draft: Settings::default(),
            tab: Tab::Stream,
            show_key: false,
            outputs: Vec::new(),
            inputs: Vec::new(),
            scenes: Vec::new(),
            link_ports: Vec::new(),
            presets: Default::default(),
        }
    }

    pub fn open_with(&mut self, current: &Settings, scenes: &ferrous_core::SceneCollection) {
        self.scenes = scenes.scenes.iter().map(|s| (s.name.clone(), s.items.iter().map(|i| i.name.clone()).collect())).collect();
        self.draft = current.clone();
        self.outputs = ferrous_audio::list_output_devices();
        self.inputs = ferrous_audio::list_input_devices();
        self.open = true;
    }

    pub fn show(&mut self, ctx: &egui::Context, probe: &ProbeState) -> SettingsAction {
        let mut action = SettingsAction::None;
        if !self.open {
            return action;
        }
        let mut open = self.open;
        egui::Window::new("Settings")
            .open(&mut open)
            .default_size([720.0, 560.0])
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (tab, label) in [
                        (Tab::Stream, "Stream"),
                        (Tab::Recording, "Recording"),
                        (Tab::Video, "Video"),
                        (Tab::Audio, "Audio"),
                        (Tab::Hotkeys, "Hotkeys"),
                        (Tab::Link, "Ferrous Link"),
                        (Tab::Advanced, "Advanced"),
                    ] {
                        ui.selectable_value(&mut self.tab, tab, label);
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical().max_height(440.0).show(ui, |ui| match self.tab {
                    Tab::Stream => self.stream_tab(ui, probe),
                    Tab::Recording => self.record_tab(ui, probe),
                    Tab::Video => self.video_tab(ui),
                    Tab::Audio => self.audio_tab(ui),
                    Tab::Hotkeys => self.hotkeys_tab(ui),
                    Tab::Link => self.link_tab(ui, probe),
                    Tab::Advanced => {
                        if self.advanced_tab(ui, probe) {
                            action = SettingsAction::Reprobe;
                        }
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        action = SettingsAction::Apply(Box::new(self.draft.clone()));
                    }
                    if ui.button(RichText::new("OK").strong()).clicked() {
                        action = SettingsAction::Apply(Box::new(self.draft.clone()));
                        self.open = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.open = false;
                    }
                });
            });
        if !open {
            self.open = false;
        }
        if self.open {
            self.presets.show(ctx, &mut self.draft, probe);
        } else {
            self.presets.open = false;
        }
        action
    }

    fn stream_tab(&mut self, ui: &mut egui::Ui, probe: &ProbeState) {
        let s = &mut self.draft.stream;
        egui::Grid::new("stream").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            crate::widgets::row_label(ui, "Service");
            ComboBox::from_id_salt("service").selected_text(&s.service).show_ui(ui, |ui| {
                for (name, url) in STREAM_SERVICES {
                    if ui.selectable_label(s.service == *name, *name).clicked() {
                        s.service = name.to_string();
                        if !url.is_empty() {
                            s.server = url.to_string();
                        }
                    }
                }
            });
            ui.end_row();
            crate::widgets::row_label(ui, "Server");
            ui.add(egui::TextEdit::singleline(&mut s.server).desired_width(420.0));
            ui.end_row();
            crate::widgets::row_label(ui, "Stream key");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut s.key).password(!self.show_key).desired_width(330.0));
                ui.toggle_value(&mut self.show_key, "Show");
            });
            ui.end_row();
            crate::widgets::row_label(ui, "Audio bitrate");
            ui.add(DragValue::new(&mut s.audio_bitrate_kbps).range(64..=320).suffix(" kbps"));
            ui.end_row();
            crate::widgets::row_label(ui, "Audio track");
            ComboBox::from_id_salt("strack").selected_text(format!("Track {}", s.audio_track + 1)).show_ui(ui, |ui| {
                for t in 0..6 {
                    ui.selectable_value(&mut s.audio_track, t, format!("Track {}", t + 1));
                }
            });
            ui.end_row();
        });
        ui.label(
            RichText::new("RTMP/RTMPS, SRT (srt://…) and WHIP (https://…) are supported.")
                .small()
                .color(theme::muted()),
        );
        ui.add_space(10.0);
        crate::widgets::section_title(ui, "Video encoder");
        encoder_editor(ui, "stream_enc", &mut s.encoder, probe);

        ui.add_space(14.0);
        crate::widgets::section_title(ui, "Additional destinations (multistream)");
        ui.label(RichText::new("Each destination starts with Start Streaming and has its own canvas, encoder and bitrate. Leave the main server empty to stream only to these.").small().color(theme::muted()));
        let has_vertical = self.draft.video.vertical.is_some();
        let mut remove = None;
        for (i, d) in self.draft.destinations.iter_mut().enumerate() {
            ui.push_id(("dest", i), |ui| {
                theme::card().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut d.enabled, "");
                        ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(160.0));
                        ComboBox::from_id_salt("canvas").selected_text(match d.canvas { ferrous_core::Canvas::Main => "Main canvas", ferrous_core::Canvas::Vertical => "Vertical canvas" }).show_ui(ui, |ui| {
                            ui.selectable_value(&mut d.canvas, ferrous_core::Canvas::Main, "Main canvas");
                            ui.selectable_value(&mut d.canvas, ferrous_core::Canvas::Vertical, "Vertical canvas");
                        });
                        if ui.small_button("✖").clicked() {
                            remove = Some(i);
                        }
                    });
                    if d.canvas == ferrous_core::Canvas::Vertical && !has_vertical {
                        ui.colored_label(theme::warn(), "Turn on the vertical canvas in Settings > Video.");
                    }
                    ui.checkbox(&mut d.ask_each_time, "Ask for server and key each time I go live (TikTok issues new ones per session)");
                    if !d.ask_each_time {
                        ui.add(egui::TextEdit::singleline(&mut d.server).hint_text("Server URL").desired_width(f32::INFINITY));
                        ui.add(egui::TextEdit::singleline(&mut d.key).hint_text("Stream key").password(true).desired_width(f32::INFINITY));
                    }
                    ui.horizontal(|ui| {
                        crate::widgets::row_label(ui, "Output");
                        ui.add(DragValue::new(&mut d.output_width).range(0..=7680));
                        ui.label("×");
                        ui.add(DragValue::new(&mut d.output_height).range(0..=7680));
                        ui.label(RichText::new("0 = canvas size").small().color(theme::muted()));
                        crate::widgets::row_label(ui, "Audio");
                        ui.add(DragValue::new(&mut d.audio_bitrate_kbps).range(64..=320).suffix(" kbps"));
                    });
                    egui::CollapsingHeader::new("Encoder").id_salt("enc").show(ui, |ui| {
                        encoder_editor(ui, &format!("dest_enc{i}"), &mut d.encoder, probe);
                    });
                });
            });
        }
        if let Some(i) = remove {
            self.draft.destinations.remove(i);
        }
        ui.horizontal(|ui| {
            if ui.button("+ Add destination").clicked() {
                self.draft.destinations.push(ferrous_core::StreamDestination { name: format!("Destination {}", self.draft.destinations.len() + 1), ..Default::default() });
            }
            if ui.button("+ TikTok LIVE (vertical)").clicked() {
                let enc = match probe {
                    ProbeState::Done(p) => ferrous_encode::autoconfig::pick_encoder(&p.working, ferrous_encode::autoconfig::Goal::Streaming).map(|e| e.name).unwrap_or("libx264"),
                    _ => "libx264",
                };
                self.draft.destinations.push(ferrous_core::StreamDestination::tiktok(enc));
                if self.draft.video.vertical.is_none() {
                    self.draft.video.vertical = Some((1080, 1920));
                }
            }
        });
    }

    fn record_tab(&mut self, ui: &mut egui::Ui, probe: &ProbeState) {
        let r = &mut self.draft.record;
        egui::Grid::new("rec").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            crate::widgets::row_label(ui, "Folder");
            ui.horizontal(|ui| {
                let mut folder = r.folder.display().to_string();
                if ui.add(egui::TextEdit::singleline(&mut folder).desired_width(330.0)).changed() {
                    r.folder = folder.into();
                }
                if ui.button("Browse…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().set_directory(&r.folder).pick_folder() {
                        r.folder = dir;
                    }
                }
            });
            ui.end_row();
            crate::widgets::row_label(ui, "File name");
            ui.add(egui::TextEdit::singleline(&mut r.filename_format).desired_width(220.0))
                .on_hover_text("strftime format, e.g. %Y-%m-%d %H-%M-%S");
            ui.end_row();
            crate::widgets::row_label(ui, "Format");
            ComboBox::from_id_salt("fmt").selected_text(r.format.label()).show_ui(ui, |ui| {
                for f in RecordingFormat::ALL {
                    ui.selectable_value(&mut r.format, f, f.label());
                }
            });
            ui.end_row();
            ui.label("Audio codec");
            let opus_ok = matches!(probe, ProbeState::Done(p) if p.audio.opus);
            ComboBox::from_id_salt("acodec").selected_text(r.audio_codec.label()).show_ui(ui, |ui| {
                for c in AudioCodec::ALL {
                    let enabled = c != AudioCodec::Opus || opus_ok || !matches!(probe, ProbeState::Done(_));
                    ui.add_enabled_ui(enabled, |ui| {
                        ui.selectable_value(&mut r.audio_codec, c, c.label());
                    });
                }
            });
            ui.end_row();
            if matches!(r.audio_codec, AudioCodec::Aac | AudioCodec::Opus) {
                crate::widgets::row_label(ui, "Audio bitrate");
                ui.add(DragValue::new(&mut r.audio_bitrate_kbps).range(64..=512).suffix(" kbps"));
                ui.end_row();
            }
        });
        if matches!(r.format, RecordingFormat::Mp4 | RecordingFormat::Mov) {
            ui.label(
                RichText::new("⚠ MP4/MOV files are unreadable if the app crashes mid-recording. MKV or fragmented MP4 are safer.")
                    .small()
                    .color(theme::warn()),
            );
        }
        ui.horizontal(|ui| {
            crate::widgets::row_label(ui, "Audio tracks");
            for t in 0..6 {
                let mut on = r.audio_tracks & (1 << t) != 0;
                if ui.checkbox(&mut on, format!("{}", t + 1)).changed() {
                    r.audio_tracks ^= 1 << t;
                    if r.audio_tracks == 0 {
                        r.audio_tracks = 1;
                    }
                }
            }
        });
        ui.add_space(10.0);
        crate::widgets::section_title(ui, "Replay buffer");
        let has_vertical = self.draft.video.vertical.is_some();
        let rp = &mut self.draft.replay;
        if has_vertical {
            ui.checkbox(&mut rp.vertical_too, "Also save a vertical (9:16) clip, ready to post");
        }
        ui.horizontal(|ui| {
            crate::widgets::row_label(ui, "Keep the last");
            ui.add(DragValue::new(&mut rp.seconds).range(5..=600).suffix(" s"));
            ComboBox::from_id_salt("rpfmt").selected_text(rp.format.label()).show_ui(ui, |ui| {
                for f in [RecordingFormat::Mkv, RecordingFormat::Mp4, RecordingFormat::Mov] {
                    ui.selectable_value(&mut rp.format, f, f.label());
                }
            });
        });
        ui.add_space(10.0);
        crate::widgets::section_title(ui, "Video encoder");
        crate::presets_ui::quick_row(ui, &mut self.presets, &mut self.draft, probe);
        let r = &mut self.draft.record;
        ui.checkbox(&mut r.same_as_stream, "Use the streaming encoder settings");
        if !r.same_as_stream {
            encoder_editor(ui, "rec_enc", &mut r.encoder, probe);
        }
    }

    fn video_tab(&mut self, ui: &mut egui::Ui) {
        let v = &mut self.draft.video;
        const PRESETS: &[(u32, u32)] =
            &[(3840, 2160), (2560, 1440), (1920, 1080), (1600, 900), (1280, 720), (854, 480)];
        egui::Grid::new("video").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            crate::widgets::row_label(ui, "Base (canvas) resolution");
            res_picker(ui, "base", &mut v.base_width, &mut v.base_height, PRESETS);
            ui.end_row();
            crate::widgets::row_label(ui, "Output (scaled) resolution");
            res_picker(ui, "out", &mut v.output_width, &mut v.output_height, PRESETS);
            ui.end_row();
            crate::widgets::row_label(ui, "Frame rate");
            ComboBox::from_id_salt("fps").selected_text(format!("{} fps", v.fps)).show_ui(ui, |ui| {
                for f in [24, 25, 30, 48, 50, 60, 120, 144] {
                    ui.selectable_value(&mut v.fps, f, format!("{f} fps"));
                }
            });
            ui.end_row();
            crate::widgets::row_label(ui, "Vertical canvas");
            ui.horizontal(|ui| {
                let mut on = v.vertical.is_some();
                if ui.checkbox(&mut on, "For TikTok / Shorts / Reels").changed() {
                    v.vertical = on.then_some((1080, 1920));
                }
                if let Some((w, h)) = &mut v.vertical {
                    ui.add(DragValue::new(w).range(240..=2160));
                    ui.label("×");
                    ui.add(DragValue::new(h).range(426..=3840));
                    for (pw, ph) in [(1080, 1920), (720, 1280)] {
                        if ui.small_button(format!("{pw}×{ph}")).clicked() {
                            *w = pw;
                            *h = ph;
                        }
                    }
                }
            });
            ui.end_row();
            crate::widgets::row_label(ui, "Compositor");
            ComboBox::from_id_salt("renderer")
                .selected_text(match v.renderer {
                    RendererMode::Auto => "GPU (fall back to CPU)",
                    RendererMode::Cpu => "CPU only",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut v.renderer, RendererMode::Auto, "GPU (fall back to CPU)");
                    ui.selectable_value(&mut v.renderer, RendererMode::Cpu, "CPU only");
                });
            ui.end_row();
            crate::widgets::row_label(ui, "Encoder input");
            {
                use ferrous_core::ConversionMode as C;
                let label = |c: C| match c {
                    C::Auto => "Auto",
                    C::Ferrous => "Convert in Ferrous (multi-threaded, 62% less pipe traffic)",
                    C::Ffmpeg => "Convert in FFmpeg (Lanczos scaler)",
                };
                ComboBox::from_id_salt("conversion").selected_text(label(v.conversion)).width(330.0).show_ui(ui, |ui| {
                    for c in [C::Auto, C::Ferrous, C::Ffmpeg] {
                        ui.selectable_value(&mut v.conversion, c, label(c));
                    }
                })
                .response
                .on_hover_text("Where the canvas is scaled to the output size and converted to the encoder's YUV format. Auto converts in Ferrous above 1080p60 (where the pipe to FFmpeg gets busy) or on 8+ core CPUs, and whenever the GPU renderer can hand over finished NV12.");
            }
            ui.end_row();
            crate::widgets::row_label(ui, "GPU readback");
            ui.checkbox(&mut v.pipelined_readback, "Pipelined (smoother frames, one frame more latency)")
                .on_hover_text("Reads each GPU frame back one frame later, so the program never waits for the GPU (it matters most when a game is using the same GPU). It adds one frame of delay: 16 ms at 60 fps, 7 ms at 144 fps. Turn it off for the lowest latency. Studio mode and transitions always read back immediately. Takes effect on restart.");
            ui.end_row();
            crate::widgets::row_label(ui, "GPU priority");
            ui.checkbox(&mut v.gpu_priority, "Schedule Ferrous's GPU work ahead of other programs")
                .on_hover_text("Helps when a game uses the whole GPU and Ferrous reports rendering lag (frames missed in the Stats dock). Works best when Ferrous runs as administrator, and with Windows Game Mode on. Takes effect on restart.");
            ui.end_row();
        });
        // Encoders want even dimensions for 4:2:0 chroma.
        v.output_width &= !1;
        v.output_height &= !1;
    }

    fn audio_tab(&mut self, ui: &mut egui::Ui) {
        let a = &mut self.draft.audio;
        egui::Grid::new("audio").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            crate::widgets::row_label(ui, "Desktop audio");
            ui.horizontal(|ui| {
                ui.checkbox(&mut a.desktop_enabled, "");
                device_picker(ui, "desk", &mut a.desktop_device, &self.outputs);
            });
            ui.end_row();
            crate::widgets::row_label(ui, "Microphone");
            ui.horizontal(|ui| {
                ui.checkbox(&mut a.mic_enabled, "");
                device_picker(ui, "mic", &mut a.mic_device, &self.inputs);
            });
            ui.end_row();
            crate::widgets::row_label(ui, "Monitoring device");
            device_picker(ui, "mondev", &mut a.monitor_device, &self.outputs);
            ui.end_row();
        });
        ui.label(
            RichText::new("Desktop audio is captured with WASAPI loopback from the chosen output device.")
                .small()
                .color(theme::muted()),
        );
    }

    fn link_tab(&mut self, ui: &mut egui::Ui, probe: &ProbeState) {
        use ferrous_core::{LinkCodec, LinkSourceTarget};
        let link_ports = self.link_ports.clone();
        ui.label(RichText::new("Send this PC's video and audio to another Ferrous on your network (e.g. gaming PC → streaming PC). Receivers add an \"Ferrous Link\" source.").small().color(theme::muted()));
        let scenes = self.scenes.clone();
        let hw: Vec<String> = match probe {
            ProbeState::Done(p) => p.working.iter().filter(|e| e.codec != ferrous_encode::Codec::Av1).map(|e| e.name.to_string()).collect(),
            _ => vec!["libx264".into()],
        };
        let mut remove = None;
        for (i, l) in self.draft.links.iter_mut().enumerate() {
            ui.push_id(("link", i), |ui| {
                crate::theme::card().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut l.enabled, "");
                        ui.add(egui::TextEdit::singleline(&mut l.name).desired_width(180.0));
                        if ui.small_button("✖").clicked() {
                            remove = Some(i);
                        }
                    });
                    egui::Grid::new("lg").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                        crate::widgets::row_label(ui, "Send");
                        let label = match &l.target {
                            LinkSourceTarget::Program => "Program output".to_string(),
                            LinkSourceTarget::Scene { name } => format!("Scene: {name}"),
                            LinkSourceTarget::Source { name } => format!("Source: {name}"),
                        };
                        ComboBox::from_id_salt("tgt").selected_text(label).width(240.0).show_ui(ui, |ui| {
                            ui.selectable_value(&mut l.target, LinkSourceTarget::Program, "Program output");
                            for (n, items) in &scenes {
                                ui.selectable_value(&mut l.target, LinkSourceTarget::Scene { name: n.clone() }, format!("Scene: {n}"));
                                for it in items {
                                    ui.selectable_value(&mut l.target, LinkSourceTarget::Source { name: it.clone() }, format!("    Source: {it}"));
                                }
                            }
                        });
                        ui.end_row();
                        crate::widgets::row_label(ui, "Codec");
                        ui.horizontal(|ui| {
                            let is = |c: &LinkCodec, k: u8| matches!((c, k), (LinkCodec::Jpeg { .. }, 0) | (LinkCodec::Lossless, 1) | (LinkCodec::Hardware { .. }, 2));
                            if ui.selectable_label(is(&l.codec, 0), "JPEG (lowest latency)").clicked() && !is(&l.codec, 0) {
                                l.codec = LinkCodec::Jpeg { quality: 92, adaptive: true };
                            }
                            if ui.selectable_label(is(&l.codec, 1), "Lossless + alpha").clicked() {
                                l.codec = LinkCodec::Lossless;
                            }
                            if ui.selectable_label(is(&l.codec, 2), "Hardware (lowest bandwidth)").clicked() && !is(&l.codec, 2) {
                                l.codec = LinkCodec::Hardware { encoder: hw.first().cloned().unwrap_or("libx264".into()), bitrate_kbps: 20_000 };
                            }
                        });
                        ui.end_row();
                        match &mut l.codec {
                            LinkCodec::Jpeg { quality, adaptive } => {
                                crate::widgets::row_label(ui, "Quality");
                                ui.horizontal(|ui| {
                                    ui.add(egui::Slider::new(quality, 30..=100));
                                    ui.checkbox(adaptive, "Adapt to network");
                                });
                                ui.end_row();
                                ui.label("");
                                ui.label(
                                    RichText::new("Recommended on a wired network: works like obs-teleport (whole-frame JPEG on several cores, paced playback). Needs roughly 150–600 Mbps at 1080p–1440p60. On Wi-Fi, use Hardware.")
                                        .small()
                                        .color(theme::muted()),
                                );
                                ui.end_row();
                            }
                            LinkCodec::Hardware { encoder, bitrate_kbps } => {
                                crate::widgets::row_label(ui, "Encoder");
                                ui.horizontal(|ui| {
                                    ComboBox::from_id_salt("lenc").selected_text(encoder.clone()).show_ui(ui, |ui| {
                                        for e in &hw {
                                            ui.selectable_value(encoder, e.clone(), e);
                                        }
                                    });
                                    ui.add(DragValue::new(bitrate_kbps).range(1000..=200_000).speed(100).suffix(" kbps"));
                                });
                                ui.end_row();
                                ui.label("");
                                ui.label(
                                    RichText::new("For Wi-Fi or slower networks: GPU encode on this PC, GPU decode on the receiver. 20,000–50,000 kbps looks near-lossless at 1080p60; adds a few frames of latency.")
                                        .small()
                                        .color(theme::muted()),
                                );
                                ui.end_row();
                            }
                            LinkCodec::Lossless => {
                                ui.label("");
                                ui.label(RichText::new("Exact pixels and transparency, for overlays and mostly-static content. Full-screen motion at 1080p60 needs multiple Gbps.").small().color(theme::muted()));
                                ui.end_row();
                            }
                        }
                        crate::widgets::row_label(ui, "Size / FPS");
                        ui.horizontal(|ui| {
                            ui.add(DragValue::new(&mut l.width).range(0..=7680).prefix("w "));
                            ui.add(DragValue::new(&mut l.height).range(0..=4320).prefix("h "));
                            ui.add(DragValue::new(&mut l.fps).range(0..=240).prefix("fps "));
                            ui.label(RichText::new("0 = same as canvas").small().color(theme::muted()));
                        });
                        ui.end_row();
                        crate::widgets::row_label(ui, "Audio");
                        ui.horizontal(|ui| {
                            crate::widgets::tip(ui.checkbox(&mut l.audio, "Send audio from"), "Send audio from");
                            ComboBox::from_id_salt("ltrack").selected_text(format!("Track {}", l.audio_track + 1)).show_ui(ui, |ui| {
                                for t in 0..6 {
                                    ui.selectable_value(&mut l.audio_track, t, format!("Track {}", t + 1));
                                }
                            });
                        });
                        ui.end_row();
                        crate::widgets::row_label(ui, "Pairing PIN");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut l.pin).desired_width(90.0).hint_text("none"));
                            ui.label(RichText::new(if l.pin.is_empty() { "Open link (anyone on the network can watch)" } else { "Encrypted; receivers need this PIN" }).small().color(theme::muted()));
                        });
                        ui.end_row();
                        crate::widgets::row_label(ui, "Port");
                        ui.add(DragValue::new(&mut l.port).range(0..=65535)).on_hover_text("0 = automatic (47778 or next free)");
                        ui.end_row();
                        crate::widgets::row_label(ui, "Receivers use");
                        ui.vertical(|ui| {
                            // The port actually bound, once this output is running.
                            let live = link_ports.iter().find(|(n, _)| *n == l.name).map(|(_, p)| *p);
                            match live {
                                Some(port) => {
                                    let addrs = ferrous_link::discovery::local_addresses();
                                    if addrs.is_empty() {
                                        ui.label(RichText::new("No network adapter is connected.").small().color(theme::warn()));
                                    }
                                    for (adapter, ip, virt) in addrs {
                                        ui.horizontal(|ui| {
                                            let text = format!("{ip}:{port}");
                                            let r = ui.add(egui::Label::new(RichText::new(&text).monospace().color(if virt { theme::muted() } else { theme::text() })).sense(egui::Sense::click()));
                                            if r.on_hover_text("Click to copy").clicked() {
                                                ui.ctx().copy_text(text);
                                            }
                                            ui.label(RichText::new(if virt { format!("{adapter} (virtual — usually not this one)") } else { adapter }).small().color(theme::muted()));
                                        });
                                    }
                                    ui.label(RichText::new("Or pick it by name in the receiver's Ferrous Link source.").small().color(theme::muted()));
                                }
                                None => {
                                    ui.label(RichText::new("Not running: enable it and click Apply.").small().color(theme::warn()));
                                }
                            }
                        });
                        ui.end_row();
                    });
                });
            });
            ui.add_space(6.0);
        }
        if let Some(i) = remove {
            self.draft.links.remove(i);
        }
        if ui.button("+ Add Ferrous Link output").clicked() {
            let n = self.draft.links.len() + 1;
            self.draft.links.push(ferrous_core::LinkOutputSettings { enabled: true, name: format!("Ferrous Link {n}"), ..Default::default() });
        }
    }

    fn hotkeys_tab(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Global hotkeys work even while a game has focus. Examples: Ctrl+Shift+R, F9, Alt+1.").small().color(theme::muted()));
        let scenes = self.scenes.clone();
        let mut remove = None;
        for (i, hk) in self.draft.hotkeys.iter_mut().enumerate() {
            ui.push_id(i, |ui| {
                ui.horizontal(|ui| {
                    let valid = crate::hotkeys::parse(&hk.keys).is_ok();
                    ui.add(egui::TextEdit::singleline(&mut hk.keys).desired_width(130.0).hint_text("Ctrl+Shift+R"))
                        .on_hover_text(if valid { "OK" } else { "Not a valid key combination" });
                    ui.label(if valid { RichText::new("✔").color(theme::ok()) } else { RichText::new("✖").color(theme::live()) });
                    ComboBox::from_id_salt("act").selected_text(hk.action.label()).width(190.0).show_ui(ui, |ui| {
                        for a in ferrous_core::HotkeyAction::all() {
                            let same = std::mem::discriminant(&a) == std::mem::discriminant(&hk.action);
                            if ui.selectable_label(same, a.label()).clicked() && !same {
                                hk.action = a;
                            }
                        }
                    });
                    match &mut hk.action {
                        ferrous_core::HotkeyAction::SwitchScene { scene } => {
                            ComboBox::from_id_salt("sc").selected_text(scene.clone()).show_ui(ui, |ui| {
                                for (n, _) in &scenes {
                                    ui.selectable_value(scene, n.clone(), n);
                                }
                            });
                        }
                        ferrous_core::HotkeyAction::ToggleSourceVisibility { scene, source } => {
                            ComboBox::from_id_salt("sc").selected_text(scene.clone()).show_ui(ui, |ui| {
                                for (n, _) in &scenes {
                                    ui.selectable_value(scene, n.clone(), n);
                                }
                            });
                            let items = scenes.iter().find(|(n, _)| n == scene).map(|(_, i)| i.clone()).unwrap_or_default();
                            ComboBox::from_id_salt("src").selected_text(source.clone()).show_ui(ui, |ui| {
                                for n in &items {
                                    ui.selectable_value(source, n.clone(), n);
                                }
                            });
                        }
                        ferrous_core::HotkeyAction::ToggleMute { channel } => {
                            ui.add(egui::TextEdit::singleline(channel).desired_width(110.0));
                        }
                        _ => {}
                    }
                    if ui.small_button("✖").clicked() {
                        remove = Some(i);
                    }
                });
            });
        }
        if let Some(i) = remove {
            self.draft.hotkeys.remove(i);
        }
        if ui.button("+ Add hotkey").clicked() {
            self.draft.hotkeys.push(ferrous_core::Hotkey { keys: String::new(), action: ferrous_core::HotkeyAction::ToggleRecording });
        }
    }

    /// Returns true if the user asked to re-run the encoder probe.
    /// The theme being picked while the window is open (previewed live).
    pub fn theme_preview(&self) -> Option<(&str, u8, u8)> {
        self.open.then_some((self.draft.ui.theme.as_str(), self.draft.ui.glass, self.draft.ui.motion))
    }

    fn appearance(&mut self, ui: &mut egui::Ui) {
        crate::widgets::section_title(ui, "Appearance");
        ui.label(RichText::new("Pick a theme; it previews as you click and is kept when you press OK.").small().color(theme::muted()));
        ui.add_space(4.0);
        let cell = egui::vec2(168.0, 58.0);
        let per_row = ((ui.available_width() + 8.0) / (cell.x + 8.0)).floor().max(1.0) as usize;
        for row in theme::PALETTES.chunks(per_row) {
            ui.horizontal(|ui| {
                for t in row {
                    let (rect, resp) = ui.allocate_exact_size(cell, egui::Sense::click());
                    let chosen = self.draft.ui.theme == t.id;
                    let p = ui.painter();
                    p.rect_filled(rect, 10.0, t.bg);
                    let strip = egui::Rect::from_min_max(egui::pos2(rect.left() + 10.0, rect.bottom() - 18.0), egui::pos2(rect.right() - 10.0, rect.bottom() - 10.0));
                    theme::gradient_rect(p, strip, t.accent, t.accent2, 4.0);
                    p.text(rect.left_top() + egui::vec2(10.0, 8.0), egui::Align2::LEFT_TOP, t.name, egui::FontId::proportional(13.0), t.text);
                    p.rect_stroke(rect, 10.0, egui::Stroke::new(if chosen { 2.0 } else { 1.0 }, if chosen { t.accent } else { t.border }), egui::StrokeKind::Inside);
                    if resp.clicked() {
                        self.draft.ui.theme = t.id.to_string();
                    }
                    resp.on_hover_text(t.tagline);
                }
            });
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            crate::widgets::row_label(ui, "Glass effects");
            let name = ["Off", "Light", "Full"][usize::from(self.draft.ui.glass.min(2))];
            egui::ComboBox::from_id_salt("glass").selected_text(name).show_ui(ui, |ui| {
                for (i, n) in ["Off", "Light", "Full"].iter().enumerate() {
                    ui.selectable_value(&mut self.draft.ui.glass, i as u8, *n);
                }
            });
            ui.label(RichText::new("Translucent panels over the theme's backdrop. Light skips the sheen; Off is flat.").small().color(theme::muted()));
        });
        ui.horizontal(|ui| {
            crate::widgets::row_label(ui, "Animated backgrounds");
            let names = ["Off", "Reduced (30 fps)", "Full"];
            egui::ComboBox::from_id_salt("motion").selected_text(names[usize::from(self.draft.ui.motion.min(2))]).show_ui(ui, |ui| {
                for (i, n) in names.iter().enumerate() {
                    ui.selectable_value(&mut self.draft.ui.motion, i as u8, *n);
                }
            });
            ui.label(RichText::new("Rain, drifting glows, twinkling stars. Costs a little GPU; Off is fully static.").small().color(theme::muted()));
        });
        ui.add_space(10.0);
    }

    fn advanced_tab(&mut self, ui: &mut egui::Ui, probe: &ProbeState) -> bool {
        let mut reprobe = false;
        self.appearance(ui);
        egui::Grid::new("adv").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            crate::widgets::row_label(ui, "FFmpeg path");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.ffmpeg_path)
                        .hint_text("auto: next to ferrous.exe, then PATH")
                        .desired_width(330.0),
                );
                if ui.button("Browse…").clicked() {
                    if let Some(f) = rfd::FileDialog::new().add_filter("ffmpeg", &["exe"]).pick_file() {
                        self.draft.ffmpeg_path = f.display().to_string();
                    }
                }
            });
            ui.end_row();
        });
        ui.add_space(8.0);
        crate::widgets::section_title(ui, "Remote control (obs-websocket compatible)");
        let r = &mut self.draft.remote;
        crate::widgets::tip(ui.checkbox(&mut r.enabled, "Enable remote control server"), "Enable remote control server");
        ui.horizontal(|ui| {
            crate::widgets::row_label(ui, "Port");
            ui.add(DragValue::new(&mut r.port).range(1024..=65535));
            crate::widgets::row_label(ui, "Password");
            ui.add(egui::TextEdit::singleline(&mut r.password).password(!self.show_key).desired_width(160.0));
            ui.toggle_value(&mut self.show_key, "Show");
        });
        if r.enabled && r.password.is_empty() {
            ui.colored_label(theme::warn(), "Without a password anyone on your network can control Ferrous.");
        }
        ui.label(
            RichText::new("Works with tools built for OBS WebSocket 5: Stream Deck, Touch Portal, Streamer.bot, TikFinity and others.")
                .small()
                .color(theme::muted()),
        );
        ui.add_space(8.0);
        crate::widgets::section_title(ui, "Plugins");
        let plugins = ferrous_plugins::list();
        if plugins.is_empty() {
            ui.label(RichText::new("No plugins installed. Put plugin DLLs in %APPDATA%\\Ferrous\\plugins.").small().color(theme::muted()));
        }
        for (id, name, kind) in plugins {
            ui.label(format!("{name}  ({id}, {kind:?})"));
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            crate::widgets::section_title(ui, "Encoders on this PC");
            if ui.small_button("Re-scan").clicked() {
                reprobe = true;
            }
        });
        match probe {
            ProbeState::Running => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Testing encoders…");
                });
            }
            ProbeState::Failed(e) => {
                ui.colored_label(theme::live(), e);
            }
            ProbeState::Done(p) => {
                ui.label(RichText::new(&p.ffmpeg_version).small().color(theme::muted()));
                for e in &p.working {
                    ui.label(RichText::new(format!("✔ {}", e.display_name())).color(theme::ok()));
                }
                for (e, why) in &p.failed {
                    ui.label(RichText::new(format!("✖ {}", e.display_name())).color(theme::muted()))
                        .on_hover_text(why);
                }
                let missing: Vec<_> = catalog::ENCODERS
                    .iter()
                    .filter(|e| !p.working.iter().any(|w| w.name == e.name) && !p.failed.iter().any(|(f, _)| f.name == e.name))
                    .map(|e| e.name)
                    .collect();
                if !missing.is_empty() {
                    ui.label(
                        RichText::new(format!("Not in this FFmpeg build: {}", missing.join(", ")))
                            .small()
                            .color(theme::muted()),
                    );
                }
            }
        }
        reprobe
    }
}

fn res_picker(ui: &mut egui::Ui, id: &str, w: &mut u32, h: &mut u32, presets: &[(u32, u32)]) {
    ui.horizontal(|ui| {
        ui.add(DragValue::new(w).range(16..=7680));
        ui.label("×");
        ui.add(DragValue::new(h).range(16..=4320));
        ComboBox::from_id_salt(id).selected_text("Presets").show_ui(ui, |ui| {
            for &(pw, ph) in presets {
                if ui.selectable_label(*w == pw && *h == ph, format!("{pw}×{ph}")).clicked() {
                    *w = pw;
                    *h = ph;
                }
            }
        });
    });
}

fn device_picker(ui: &mut egui::Ui, id: &str, value: &mut Option<String>, devices: &[String]) {
    let text = value.clone().unwrap_or_else(|| "Default".into());
    ComboBox::from_id_salt(id).selected_text(text).width(340.0).show_ui(ui, |ui| {
        ui.selectable_value(value, None, "Default");
        for d in devices {
            ui.selectable_value(value, Some(d.clone()), d);
        }
    });
}

/// Encoder picker plus the knobs that apply to the chosen encoder.
pub fn encoder_editor(ui: &mut egui::Ui, id: &str, e: &mut EncoderSettings, probe: &ProbeState) {
    let available: Vec<&'static catalog::EncoderInfo> = match probe {
        ProbeState::Done(p) => p.working.clone(),
        _ => catalog::ENCODERS.iter().collect(),
    };
    let current_label = catalog::find(&e.encoder).map(|i| i.display_name()).unwrap_or_else(|| e.encoder.clone());

    egui::Grid::new(id).num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
        crate::widgets::row_label(ui, "Encoder");
        ComboBox::from_id_salt((id, "enc")).selected_text(current_label).width(340.0).show_ui(ui, |ui| {
            let mut last_vendor = None;
            for info in &available {
                if last_vendor != Some(info.vendor) {
                    ui.label(RichText::new(info.vendor.label()).small().color(theme::muted()));
                    last_vendor = Some(info.vendor);
                }
                let label = format!("  {} ({})", info.codec.label(), info.name);
                if ui.selectable_label(e.encoder == info.name, label).clicked() && e.encoder != info.name {
                    e.encoder = info.name.to_string();
                    e.preset = info.default_preset.to_string();
                }
            }
        });
        ui.end_row();

        if let ProbeState::Done(_) = probe {
            if !available.iter().any(|i| i.name == e.encoder) {
                ui.label("");
                ui.colored_label(theme::warn(), "This encoder didn't pass the hardware test on this PC.");
                ui.end_row();
            }
        }

        crate::widgets::row_label(ui, "Rate control");
        ComboBox::from_id_salt((id, "rc")).selected_text(e.rate_control.label()).show_ui(ui, |ui| {
            for rc in RateControl::ALL {
                ui.selectable_value(&mut e.rate_control, rc, rc.label());
            }
        });
        ui.end_row();

        match e.rate_control {
            RateControl::Cbr | RateControl::Vbr => {
                crate::widgets::row_label(ui, "Bitrate");
                ui.add(DragValue::new(&mut e.bitrate_kbps).range(500..=150_000).speed(50).suffix(" kbps"));
            }
            RateControl::Quality | RateControl::Cqp => {
                ui.label(if e.rate_control == RateControl::Quality { "Quality (CRF/CQ)" } else { "QP" });
                ui.add(DragValue::new(&mut e.quality).range(0..=63))
                    .on_hover_text("Lower is better quality and bigger files. ~18–23 is typical for H.264.");
            }
        }
        ui.end_row();

        if let Some(info) = catalog::find(&e.encoder) {
            if !info.presets.is_empty() {
                crate::widgets::row_label(ui, "Preset");
                let shown = if e.preset.is_empty() { info.default_preset } else { e.preset.as_str() };
                ComboBox::from_id_salt((id, "preset")).selected_text(shown.to_string()).show_ui(ui, |ui| {
                    for p in info.presets {
                        ui.selectable_value(&mut e.preset, p.to_string(), *p);
                    }
                });
                ui.end_row();
            }
        }

        crate::widgets::row_label(ui, "Keyframe interval");
        ui.add(DragValue::new(&mut e.keyframe_interval_secs).range(1..=10).suffix(" s"));
        ui.end_row();
        crate::widgets::row_label(ui, "B-frames");
        ui.add(DragValue::new(&mut e.b_frames).range(0..=4));
        ui.end_row();
        crate::widgets::row_label(ui, "HDR10");
        let can = ferrous_encode::args::supports_hdr(&e.encoder);
        ui.horizontal(|ui| {
            ui.add_enabled(can, egui::Checkbox::new(&mut e.hdr, "Output HDR10 (10-bit, BT.2020, PQ)"))
                .on_hover_text("Encodes 10-bit HDR10. Your SDR sources are mapped into HDR with white at the level on the right, so they look as they do now on an HDR display; true HDR capture is not in yet. Needs an HEVC or AV1 encoder and a player that understands HDR; most live services don't accept it. Recordings only, in practice.")
                .on_disabled_hover_text("HDR10 needs an HEVC or AV1 encoder (H.264 is 8-bit).");
            if e.hdr && can {
                ui.add(DragValue::new(&mut e.hdr_white_nits).range(80..=400).suffix(" nits"))
                    .on_hover_text("Brightness of SDR white inside the HDR picture. 203 is the broadcast reference; raise it if the result looks dim.");
            }
        });
        ui.end_row();
        crate::widgets::row_label(ui, "Custom FFmpeg options");
        ui.add(egui::TextEdit::singleline(&mut e.custom_args).hint_text("-tune zerolatency").desired_width(340.0));
        ui.end_row();
    });
}
