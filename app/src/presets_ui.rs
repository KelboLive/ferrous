//! Recording presets and the speed/quality benchmark.
//!
//! Lives inside the Settings window: it edits the Settings draft (current
//! recording settings and the saved presets), so nothing changes until the
//! person presses Apply or OK there.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use eframe::egui::{self, RichText};
use ferrous_bench::compare::{self, Event, Source, Spec, Trial, TrialResult};
use ferrous_core::{EncoderSettings, RecordingPreset, Settings};
use ferrous_encode::presets;
use parking_lot::Mutex;

use crate::settings_ui::ProbeState;
use crate::theme;

#[derive(Default)]
struct Run {
    total: usize,
    step: usize,
    current: String,
    preparing: bool,
    results: Vec<TrialResult>,
    log: Vec<String>,
    done: bool,
    cancelled: bool,
    error: Option<String>,
    /// Settings as the person has them, by trial label (the tested copy may
    /// be forced to a common bitrate).
    originals: HashMap<String, EncoderSettings>,
    /// What the run was measured with, for the results header.
    vmaf_note: String,
}

pub struct PresetsUi {
    pub open: bool,
    save_name: String,
    /// Names left out of the benchmark (everything is in by default).
    skipped: HashSet<String>,
    include_current: bool,
    sweep: bool,
    equal_bitrate: bool,
    bitrate_kbps: u32,
    secs: f32,
    use_clip: bool,
    clip: String,
    run: Option<Arc<Mutex<Run>>>,
    cancel: Arc<AtomicBool>,
}

impl Default for PresetsUi {
    fn default() -> Self {
        Self {
            open: false,
            save_name: String::new(),
            skipped: HashSet::new(),
            include_current: true,
            sweep: false,
            equal_bitrate: false,
            bitrate_kbps: 20_000,
            secs: 6.0,
            use_clip: false,
            clip: String::new(),
            run: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct Entry {
    preset: RecordingPreset,
    builtin: bool,
}

fn entries(settings: &Settings, probe: &ProbeState) -> Vec<Entry> {
    let mut v: Vec<Entry> = match probe {
        ProbeState::Done(p) => presets::builtin(&p.working).into_iter().map(|preset| Entry { preset, builtin: true }).collect(),
        _ => Vec::new(),
    };
    v.extend(settings.recording_presets.iter().cloned().map(|preset| Entry { preset, builtin: false }));
    v
}

/// The one-line preset chooser on the Recording tab.
pub fn quick_row(ui: &mut egui::Ui, ui_state: &mut PresetsUi, settings: &mut Settings, probe: &ProbeState) {
    let list = entries(settings, probe);
    let active = list.iter().find(|e| presets::matches(&e.preset, &settings.record)).map(|e| e.preset.name.clone());
    ui.horizontal(|ui| {
        crate::widgets::row_label(ui, "Preset");
        egui::ComboBox::from_id_salt("rec_preset").width(260.0).selected_text(active.clone().unwrap_or_else(|| "Custom".into())).show_ui(ui, |ui| {
            for e in &list {
                if ui.selectable_label(active.as_deref() == Some(&e.preset.name), &e.preset.name).on_hover_text(presets::summary(&e.preset.encoder)).clicked() {
                    e.preset.apply_to(&mut settings.record);
                }
            }
            if list.is_empty() {
                ui.label(RichText::new("Built-in presets appear once the encoder test finishes.").small().color(theme::muted()));
            }
        });
        if ui.button("Presets & benchmark…").on_hover_text("Save your own presets and measure which settings give the best picture your PC can record in real time.").clicked() {
            ui_state.open = true;
        }
    });
}

impl PresetsUi {
    pub fn show(&mut self, ctx: &egui::Context, settings: &mut Settings, probe: &ProbeState) {
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new("Recording presets & benchmark").open(&mut open).default_size([760.0, 640.0]).collapsible(false).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.presets_section(ui, settings, probe);
                ui.add_space(12.0);
                ui.separator();
                self.bench_section(ui, ctx, settings, probe);
            });
        });
        self.open = open;
    }

    fn presets_section(&mut self, ui: &mut egui::Ui, settings: &mut Settings, probe: &ProbeState) {
        crate::widgets::section_title(ui, "Presets");
        let list = entries(settings, probe);
        let mut delete: Option<String> = None;
        egui::Grid::new("preset_list").num_columns(3).spacing([14.0, 6.0]).striped(true).show(ui, |ui| {
            for e in &list {
                let on = presets::matches(&e.preset, &settings.record);
                ui.label(RichText::new(format!("{}{}", if on { "● " } else { "" }, e.preset.name)).strong().color(if on { theme::accent() } else { theme::text() }));
                ui.label(RichText::new(presets::summary(&e.preset.encoder)).small().color(theme::muted()));
                ui.horizontal(|ui| {
                    if ui.add_enabled(!on, egui::Button::new("Use")).clicked() {
                        e.preset.apply_to(&mut settings.record);
                    }
                    if !e.builtin && ui.button("Delete").clicked() {
                        delete = Some(e.preset.name.clone());
                    }
                });
                ui.end_row();
            }
        });
        if list.iter().all(|e| !e.builtin) {
            ui.label(RichText::new("Built-in presets appear once the encoder test finishes.").small().color(theme::muted()));
        }
        if let Some(n) = delete {
            settings.recording_presets.retain(|p| p.name != n);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Save the current recording settings as");
            ui.add(egui::TextEdit::singleline(&mut self.save_name).desired_width(200.0).hint_text("e.g. Gameplay 4K"));
            let name = self.save_name.trim().to_string();
            if ui.add_enabled(!name.is_empty(), egui::Button::new("Save")).clicked() {
                let p = RecordingPreset::from_record(name.clone(), &settings.record, &settings.stream.encoder);
                match settings.recording_presets.iter_mut().find(|x| x.name == name) {
                    Some(old) => *old = p,
                    None => settings.recording_presets.push(p),
                }
                self.save_name.clear();
            }
        });
        ui.label(RichText::new("A preset holds the encoder, quality, container, audio and recording size. Press Apply or OK in Settings to keep changes.").small().color(theme::muted()));
    }

    fn bench_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, settings: &mut Settings, probe: &ProbeState) {
        crate::widgets::section_title(ui, "Benchmark");
        ui.label("Encodes the same footage with each setting, then measures how fast it ran and how close the result is to the original. Pick the best picture that still keeps up in real time.");
        let running = self.run.as_ref().is_some_and(|r| !r.lock().done);
        let list = entries(settings, probe);
        ui.add_enabled_ui(!running, |ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("Settings to test").small().color(theme::muted()));
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.include_current, "Current settings");
                for e in &list {
                    let mut on = !self.skipped.contains(&e.preset.name);
                    if ui.checkbox(&mut on, &e.preset.name).changed() {
                        if on { self.skipped.remove(&e.preset.name) } else { self.skipped.insert(e.preset.name.clone()) };
                    }
                }
            });
            ui.checkbox(&mut self.sweep, "Also try the slowest, middle and fastest preset of every working encoder")
                .on_hover_text("Adds three trials per encoder at your current quality setting. Takes longer, but shows where each encoder's sweet spot is.");
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.equal_bitrate, "Compare at equal bitrate");
                ui.add_enabled(self.equal_bitrate, egui::DragValue::new(&mut self.bitrate_kbps).range(1000..=200_000).suffix(" kbps"));
                ui.label(RichText::new("(measures encoder efficiency; leave off to compare your settings as they are)").small().color(theme::muted()));
            });
            ui.horizontal(|ui| {
                ui.label("Footage");
                ui.radio_value(&mut self.use_clip, false, "Built-in test scene");
                ui.radio_value(&mut self.use_clip, true, "My own clip");
                if self.use_clip {
                    ui.add(egui::TextEdit::singleline(&mut self.clip).desired_width(220.0).hint_text("a gameplay recording"));
                    if ui.button("Browse…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().add_filter("Video", &["mkv", "mp4", "mov", "webm", "avi", "ts"]).pick_file() {
                            self.clip = p.display().to_string();
                        }
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Length per test");
                ui.add(egui::Slider::new(&mut self.secs, 3.0..=20.0).suffix(" s"));
                let v = &settings.video;
                ui.label(RichText::new(format!("at {}×{} {} fps", settings.record.output_size.map_or(v.output_width, |s| s.0), settings.record.output_size.map_or(v.output_height, |s| s.1), v.fps)).small().color(theme::muted()));
            });
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let ready = matches!(probe, ProbeState::Done(_));
            if running {
                if ui.button("Cancel").clicked() {
                    self.cancel.store(true, Ordering::Relaxed);
                }
            } else if ui.add_enabled(ready, egui::Button::new(RichText::new("Run benchmark").strong()).fill(theme::accent().linear_multiply(0.6))).clicked() {
                self.start(ctx, settings, probe, &list);
            }
            if !ready {
                ui.spinner();
                ui.label("Finding encoders…");
            }
        });
        let Some(run) = self.run.clone() else { return };
        let r = run.lock();
        if !r.done {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
            ui.add(egui::ProgressBar::new(r.step as f32 / r.total.max(1) as f32).text(if r.preparing { "Preparing test footage…".to_string() } else { format!("{} ({}/{})", r.current, r.step + 1, r.total) }));
            ui.label(RichText::new("Keep other heavy programs closed while this runs: they would slow the encoders and skew the numbers.").small().color(theme::muted()));
        }
        if let Some(e) = &r.error {
            ui.label(RichText::new(e).color(theme::warn()));
        }
        for l in r.log.iter().filter(|l| l.starts_with('✖')) {
            ui.label(RichText::new(l).small().color(theme::warn()));
        }
        if r.results.is_empty() {
            return;
        }
        let picks = compare::recommend(&r.results);
        ui.add_space(8.0);
        crate::widgets::section_title(ui, if r.done { "Results" } else { "Results so far" });
        ui.label(RichText::new(&r.vmaf_note).small().color(theme::muted()));
        let mut order: Vec<usize> = (0..r.results.len()).collect();
        order.sort_by(|&a, &b| r.results[b].quality().partial_cmp(&r.results[a].quality()).unwrap_or(std::cmp::Ordering::Equal));
        let mut use_it: Option<usize> = None;
        let mut save_it: Option<usize> = None;
        egui::Grid::new("bench_results").num_columns(7).spacing([14.0, 5.0]).striped(true).show(ui, |ui| {
            for h in ["Setting", "Speed", "Bitrate", "Quality", "", "", ""] {
                ui.label(RichText::new(h).small().strong().color(theme::muted()));
            }
            ui.end_row();
            for &i in &order {
                let t = &r.results[i];
                ui.label(&t.label).on_hover_text(presets::summary(&t.encoder));
                let col = if t.realtime() { theme::ok() } else { theme::warn() };
                ui.label(RichText::new(format!("{:.0} fps ({:.1}×)", t.fps, t.speed())).color(col)).on_hover_text(if t.realtime() { "Keeps up in real time with headroom." } else { "Too slow to record live at this frame rate with headroom." });
                ui.label(format!("{:.1} Mbps", t.bitrate_kbps / 1000.0));
                let q = match t.vmaf {
                    Some(v) => format!("VMAF {v:.1}"),
                    None => format!("{:.1} dB", compare::ssim_db(t.ssim)),
                };
                ui.label(q).on_hover_text(format!("SSIM {:.4}   PSNR {:.1} dB{}", t.ssim, t.psnr, t.vmaf.map(|v| format!("   VMAF {v:.1}")).unwrap_or_default()));
                let mut tags = String::new();
                if picks.best_quality == Some(i) {
                    tags.push_str("★ best quality  ");
                }
                if picks.best_value == Some(i) {
                    tags.push_str("◆ best value  ");
                }
                if picks.lightest == Some(i) {
                    tags.push_str("⚡ lightest");
                }
                ui.label(RichText::new(tags).small().color(theme::accent()));
                if ui.button("Use").on_hover_text("Make these the recording settings").clicked() {
                    use_it = Some(i);
                }
                if ui.button("Save as preset").clicked() {
                    save_it = Some(i);
                }
                ui.end_row();
            }
        });
        ui.label(RichText::new("Best value: within a barely visible difference of the best quality, but the lightest on your PC. Quality is measured against the original footage; speed assumes nothing else is running, so leave headroom when recording a game.").small().color(theme::muted()));
        let original = |t: &TrialResult| r.originals.get(&t.label).cloned().unwrap_or_else(|| t.encoder.clone());
        if let Some(i) = use_it {
            let mut e = original(&r.results[i]);
            e.hdr = settings.record.encoder.hdr;
            e.hdr_white_nits = settings.record.encoder.hdr_white_nits;
            settings.record.encoder = e;
            settings.record.same_as_stream = false;
        }
        if let Some(i) = save_it {
            let t = &r.results[i];
            let mut p = RecordingPreset::from_record(t.label.clone(), &settings.record, &settings.stream.encoder);
            p.encoder = original(t);
            settings.recording_presets.retain(|x| x.name != p.name);
            settings.recording_presets.push(p);
        }
    }

    fn start(&mut self, ctx: &egui::Context, settings: &Settings, probe: &ProbeState, list: &[Entry]) {
        let ProbeState::Done(p) = probe else { return };
        let ffmpeg = ferrous_encode::ffmpeg::locate(&settings.ffmpeg_path);
        let v = &settings.video;
        let (w, h) = settings.record.output_size.unwrap_or((v.output_width, v.output_height));
        let spec = Spec {
            width: w & !1,
            height: h & !1,
            fps: v.fps.max(1),
            secs: self.secs,
            source: if self.use_clip && !self.clip.trim().is_empty() { Source::Clip(PathBuf::from(self.clip.trim())) } else { Source::Synthetic },
        };
        let mut raw: Vec<(String, EncoderSettings)> = Vec::new();
        if self.include_current {
            raw.push(("Current settings".into(), settings.effective_record_encoder().clone()));
        }
        for e in list.iter().filter(|e| !self.skipped.contains(&e.preset.name)) {
            raw.push((e.preset.name.clone(), e.preset.encoder.clone()));
        }
        if self.sweep {
            let base = EncoderSettings { rate_control: ferrous_core::RateControl::Quality, ..settings.record.encoder.clone() };
            for t in compare::sweep(&p.working, &base) {
                raw.push((t.label, t.encoder));
            }
        }
        // The same settings listed twice measure the same thing twice.
        let mut seen = Vec::new();
        raw.retain(|(_, e)| {
            let dup = seen.contains(e);
            seen.push(e.clone());
            !dup
        });
        let mut originals = HashMap::new();
        let trials: Vec<Trial> = raw
            .into_iter()
            .map(|(label, e)| {
                originals.insert(label.clone(), e.clone());
                Trial { label, encoder: if self.equal_bitrate { compare::at_bitrate(&e, self.bitrate_kbps) } else { e } }
            })
            .collect();
        let run = Arc::new(Mutex::new(Run { total: trials.len(), preparing: true, originals, ..Default::default() }));
        self.cancel = Arc::new(AtomicBool::new(false));
        self.run = Some(run.clone());
        let (cancel, ctx) = (self.cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            let vmaf = compare::has_vmaf(&ffmpeg);
            run.lock().vmaf_note = if vmaf { "Quality is VMAF (0–100, higher is better).".into() } else { "Quality is SSIM in decibels (higher is better; about 0.5 dB is the smallest difference worth caring about). This FFmpeg build has no VMAF.".into() };
            let out = compare::run_all(&ffmpeg, &spec, &trials, &cancel, |ev| {
                let mut r = run.lock();
                match ev {
                    Event::Preparing => r.preparing = true,
                    Event::Started { index, label, .. } => {
                        r.preparing = false;
                        r.step = index;
                        r.current = label;
                    }
                    Event::Finished { result, .. } => match result {
                        Ok(t) => r.results.push(t),
                        Err(e) => r.log.push(format!("✖ {e}")),
                    },
                }
                ctx.request_repaint();
            });
            let mut r = run.lock();
            if let Err(e) = out {
                r.error = Some(e.to_string());
            }
            r.cancelled = cancel.load(Ordering::Relaxed);
            r.done = true;
            ctx.request_repaint();
        });
    }
}
