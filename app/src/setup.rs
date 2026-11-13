//! First-run setup: benchmark the PC and connection, rank ingest servers,
//! and recommend streaming and recording settings the user can apply.

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, RichText};
use ferrous_bench::ingest;
use ferrous_bench::{BenchReport, Recommendation};
use ferrous_core::Settings;
use ferrous_encode::autoconfig::Goal;
use ferrous_encode::{Codec, EncoderInfo};
use parking_lot::Mutex;

use crate::settings_ui::ProbeState;
use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    Twitch,
    YouTube,
    TikTok,
    Custom,
    RecordingOnly,
}

impl Platform {
    fn label(self) -> &'static str {
        match self {
            Platform::Twitch => "Twitch",
            Platform::YouTube => "YouTube",
            Platform::TikTok => "TikTok LIVE",
            Platform::Custom => "Other (custom RTMP/SRT)",
            Platform::RecordingOnly => "Just recording",
        }
    }
    fn bitrate_cap(self) -> u32 {
        match self {
            Platform::YouTube => 12_000,
            Platform::Custom => 20_000,
            _ => 6_000,
        }
    }
}

#[derive(Default)]
struct Progress {
    log: Vec<String>,
    step: usize,
    total: usize,
    report: BenchReport,
    done: bool,
}

enum Page {
    Choose,
    Running(Arc<Mutex<Progress>>),
    Results(BenchReport),
}

pub enum Outcome {
    Open,
    Close { completed: bool },
    /// Boxed: returned every frame, and Settings is large.
    Apply(Box<Settings>),
}

pub struct Setup {
    page: Page,
    platform: Platform,
    test_upload: bool,
    test_ingests: bool,
    twitch_key: String,
    custom_servers: String,
    /// Index into the ranked ingests (Results page).
    chosen_ingest: usize,
    apply_stream: bool,
    apply_record: bool,
    base: Settings,
}

/// Which encoders to benchmark: the best hardware H.264 and HEVC/AV1, plus x264.
fn pick_encoders(working: &[&'static EncoderInfo]) -> Vec<&'static EncoderInfo> {
    let mut v: Vec<&'static EncoderInfo> = Vec::new();
    let hw = |c: Codec| working.iter().copied().find(|e| e.vendor.is_hardware() && e.codec == c);
    for e in [hw(Codec::H264), hw(Codec::Hevc).or(hw(Codec::Av1)), working.iter().copied().find(|e| e.name == "libx264")].into_iter().flatten() {
        if !v.iter().any(|x| x.name == e.name) {
            v.push(e);
        }
    }
    v
}

impl Setup {
    pub fn new(current: &Settings) -> Self {
        Self {
            page: Page::Choose,
            platform: Platform::Twitch,
            test_upload: true,
            test_ingests: true,
            twitch_key: String::new(),
            custom_servers: String::new(),
            chosen_ingest: 0,
            apply_stream: true,
            apply_record: true,
            base: current.clone(),
        }
    }

    fn start(&mut self, probe: &ProbeState, display: (u32, u32), ffmpeg: PathBuf) {
        let ProbeState::Done(p) = probe else { return };
        let encoders = pick_encoders(&p.working);
        let candidates: Vec<(u32, u32, u32)> = ferrous_bench::CANDIDATES.iter().copied().filter(|(_, h, _)| *h <= display.1.max(720)).collect();
        let resolutions: Vec<(u32, u32)> = {
            let mut r: Vec<(u32, u32)> = candidates.iter().map(|(w, h, _)| (*w, *h)).collect();
            r.dedup();
            r
        };
        let prog = Arc::new(Mutex::new(Progress::default()));
        let platform = self.platform;
        let (test_upload, test_ingests) = (self.test_upload && platform != Platform::RecordingOnly, self.test_ingests && platform != Platform::RecordingOnly);
        let key = self.twitch_key.trim().to_string();
        let custom: Vec<String> = self.custom_servers.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        prog.lock().total = encoders.len() * candidates.len() + resolutions.len() + test_upload as usize + test_ingests as usize;
        let p2 = prog.clone();
        let gpu = self.base.video.renderer != ferrous_core::RendererMode::Cpu;
        std::thread::spawn(move || {
            let log = |m: String| {
                let mut p = p2.lock();
                p.log.push(m);
                p.step += 1;
            };
            for &(w, h) in &resolutions {
                let (desc, ms) = ferrous_bench::bench_compositor(w, h, gpu, 30);
                log(format!("Compositor {w}×{h}: {ms:.1} ms/frame ({desc})"));
                p2.lock().report.compositor.push(((w, h), desc, ms));
            }
            for enc in &encoders {
                for &(w, h, f) in &candidates {
                    match ferrous_bench::bench_encoder(&ffmpeg, enc, w, h, f, 2.5) {
                        Ok(r) => {
                            log(format!("{} {w}×{h}@{f}: {:.0} fps {}", enc.name, r.achieved_fps, if r.realtime() { "✔" } else { "✖ too slow" }));
                            p2.lock().report.encoders.push(r);
                        }
                        Err(e) => {
                            log(format!("{} {w}×{h}@{f}: failed ({e})", enc.name));
                            p2.lock().report.encoder_errors.push(e.to_string());
                        }
                    }
                }
            }
            if test_upload {
                match ferrous_bench::bench_upload(ferrous_bench::DEFAULT_UPLOAD_URL, 8.0) {
                    Ok(k) => {
                        log(format!("Upload: {:.1} Mbps", k as f32 / 1000.0));
                        p2.lock().report.upload_kbps = Some(k);
                    }
                    Err(e) => {
                        log(format!("Upload test failed: {e}"));
                        p2.lock().report.upload_error = Some(e.to_string());
                    }
                }
            }
            if test_ingests {
                let list = match platform {
                    Platform::Twitch => ingest::fetch_twitch().unwrap_or_else(|e| {
                        log(format!("Couldn't fetch Twitch's ingest list: {e}"));
                        Vec::new()
                    }),
                    Platform::YouTube => ingest::youtube(),
                    Platform::Custom => custom.iter().map(|s| ingest::Ingest { name: s.clone(), server: s.clone(), priority: 0, backup: false }).collect(),
                    _ => Vec::new(),
                };
                let mut ranked = ingest::rank(list, 5);
                // Twitch bandwidth test on the 3 lowest-latency servers (doesn't go live).
                if platform == Platform::Twitch && !key.is_empty() {
                    for r in ranked.iter_mut().filter(|r| r.rtt_ms.is_some()).take(3) {
                        match ingest::twitch_bandwidth_test(&ffmpeg, &r.ingest.server, &key, 8000, 8) {
                            Ok(k) => r.bandwidth_kbps = Some(k),
                            Err(e) => r.error = Some(e.to_string()),
                        }
                    }
                    ingest::sort(&mut ranked);
                }
                log(format!("Ranked {} ingest servers", ranked.len()));
                p2.lock().report.ingests = ranked;
            }
            p2.lock().done = true;
        });
        self.page = Page::Running(prog);
    }

    pub fn show(&mut self, ctx: &egui::Context, probe: &ProbeState, display: (u32, u32), ffmpeg: PathBuf) -> Outcome {
        let mut out = Outcome::Open;
        let mut open = true;
        let mut next: Option<Page> = None;
        enum View {
            Choose,
            Running(Arc<Mutex<Progress>>),
            Results(BenchReport),
        }
        let view = match &self.page {
            Page::Choose => View::Choose,
            Page::Running(p) => View::Running(p.clone()),
            Page::Results(r) => View::Results(r.clone()),
        };
        egui::Window::new("Setup & Benchmark").open(&mut open).collapsible(false).default_size([640.0, 560.0]).show(ctx, |ui| match view {
            View::Choose => {
                ui.label(RichText::new("Let's measure this PC and your connection, then pick the best settings.").strong());
                ui.label(RichText::new("Takes about a minute. Encoders are tested with real encodes; nothing goes live.").small().color(theme::muted()));
                ui.add_space(8.0);
                crate::widgets::section_title(ui, "Where will you stream?");
                ui.horizontal_wrapped(|ui| {
                    for p in [Platform::Twitch, Platform::YouTube, Platform::TikTok, Platform::Custom, Platform::RecordingOnly] {
                        ui.selectable_value(&mut self.platform, p, p.label());
                    }
                });
                if self.platform != Platform::RecordingOnly {
                    ui.checkbox(&mut self.test_upload, "Measure upload speed (uploads test data to speed.cloudflare.com for ~8 s)");
                    match self.platform {
                        Platform::Twitch => {
                            ui.checkbox(&mut self.test_ingests, "Rank Twitch ingest servers by latency and pick the fastest");
                            ui.horizontal(|ui| {
                                ui.label("Stream key (optional)");
                                ui.add(egui::TextEdit::singleline(&mut self.twitch_key).password(true).desired_width(240.0));
                            });
                            ui.label(RichText::new("With a key, the 3 closest servers also get a real bandwidth test in Twitch's test mode (nothing goes live).").small().color(theme::muted()));
                        }
                        Platform::YouTube => {
                            ui.checkbox(&mut self.test_ingests, "Test YouTube's primary and backup ingest servers");
                        }
                        Platform::TikTok => {
                            ui.label(RichText::new("TikTok gives you one server per LIVE session, so there are no alternatives to rank. Ferrous checks that server when you go live.").small().color(theme::muted()));
                        }
                        Platform::Custom => {
                            ui.checkbox(&mut self.test_ingests, "Rank these servers (one URL per line):");
                            ui.add(egui::TextEdit::multiline(&mut self.custom_servers).desired_rows(3).desired_width(f32::INFINITY).hint_text("rtmp://ingest-a.example.com/live\nsrt://ingest-b.example.com:9000"));
                        }
                        Platform::RecordingOnly => {}
                    }
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let ready = matches!(probe, ProbeState::Done(_));
                    if ui.add_enabled(ready, egui::Button::new(RichText::new("Run benchmark").strong()).fill(theme::accent().linear_multiply(0.6))).clicked() {
                        self.start(probe, display, ffmpeg.clone());
                    }
                    if !ready {
                        ui.spinner();
                        ui.label("Finding encoders…");
                    }
                    if ui.button("Skip").clicked() {
                        out = Outcome::Close { completed: true };
                    }
                });
            }
            View::Running(prog) => {
                let p = prog.lock();
                ui.add(egui::ProgressBar::new(p.step as f32 / p.total.max(1) as f32).show_percentage());
                egui::ScrollArea::vertical().stick_to_bottom(true).max_height(380.0).show(ui, |ui| {
                    for l in &p.log {
                        ui.label(RichText::new(l).monospace().small());
                    }
                });
                if p.done {
                    next = Some(Page::Results(p.report.clone()));
                } else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Benchmarking…");
                    });
                    ctx.request_repaint_after(std::time::Duration::from_millis(200));
                }
            }
            View::Results(report) => {
                let report = &report;
                let streaming = self.platform != Platform::RecordingOnly;
                let s = ferrous_bench::recommend(report, Goal::Streaming, display, self.platform.bitrate_cap());
                let r = ferrous_bench::recommend(report, Goal::Recording, display, 0);
                egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                    crate::widgets::section_title(ui, "Measured");
                    egui::Grid::new("bench").striped(true).num_columns(2).show(ui, |ui| {
                        for ((w, h), desc, ms) in &report.compositor {
                            ui.label(format!("Compositor {w}×{h}"));
                            ui.label(format!("{ms:.1} ms/frame ({desc})"));
                            ui.end_row();
                        }
                        for e in &report.encoders {
                            ui.label(format!("{} {}×{}@{}", e.encoder.name, e.width, e.height, e.target_fps));
                            ui.colored_label(if e.realtime() { theme::ok() } else { theme::warn() }, format!("{:.0} fps", e.achieved_fps));
                            ui.end_row();
                        }
                        if streaming {
                            ui.label("Upload");
                            match (&report.upload_kbps, &report.upload_error) {
                                (Some(k), _) => ui.label(format!("{:.1} Mbps", *k as f32 / 1000.0)),
                                (None, Some(e)) => ui.colored_label(theme::warn(), format!("not measured: {e}")),
                                _ => ui.label("not measured"),
                            };
                            ui.end_row();
                        }
                    });
                    if !report.ingests.is_empty() {
                        ui.add_space(6.0);
                        crate::widgets::section_title(ui, "Ingest servers (fastest first)");
                        egui::Grid::new("ingests").striped(true).num_columns(4).show(ui, |ui| {
                            for (i, ing) in report.ingests.iter().enumerate().take(20) {
                                ui.radio_value(&mut self.chosen_ingest, i, &ing.ingest.name);
                                ui.label(ing.rtt_ms.map(|r| format!("{r:.0} ms")).unwrap_or_else(|| "unreachable".into()));
                                ui.label(ing.jitter_ms.map(|j| format!("±{j:.0} ms")).unwrap_or_default());
                                ui.label(ing.bandwidth_kbps.map(|b| format!("{:.1} Mbps sustained", b as f32 / 1000.0)).or_else(|| ing.error.clone()).unwrap_or_default());
                                ui.end_row();
                            }
                        });
                        if ui.small_button("Use the fastest").clicked() {
                            self.chosen_ingest = report.ingests.iter().position(|r| ingest::fastest(std::slice::from_ref(r)).is_some()).unwrap_or(0);
                        }
                    }
                    ui.add_space(8.0);
                    ui.columns(if streaming { 2 } else { 1 }, |cols| {
                        let mut i = 0;
                        if streaming {
                            rec_card(&mut cols[0], "Streaming", &s, &mut self.apply_stream);
                            i = 1;
                        }
                        rec_card(&mut cols[i], "Recording", &r, &mut self.apply_record);
                    });
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(self.apply_stream || self.apply_record, egui::Button::new(RichText::new("Apply selected").strong()).fill(theme::accent().linear_multiply(0.6))).clicked() {
                        let ingest = report.ingests.get(self.chosen_ingest).filter(|r| r.rtt_ms.is_some()).map(|r| r.ingest.clone());
                        out = Outcome::Apply(Box::new(self.apply(display, streaming && self.apply_stream, self.apply_record, &s, &r, ingest)));
                    }
                    if ui.button("Run again").clicked() {
                        next = Some(Page::Choose);
                    }
                    if ui.button("Close without applying").clicked() {
                        out = Outcome::Close { completed: true };
                    }
                });
            }
        });
        if let Some(p) = next {
            self.page = p;
        }
        if !open {
            return Outcome::Close { completed: false };
        }
        out
    }

    fn apply(&self, display: (u32, u32), stream: bool, record: bool, s: &Recommendation, r: &Recommendation, ingest: Option<ingest::Ingest>) -> Settings {
        let mut st = self.base.clone();
        st.video.base_width = display.0;
        st.video.base_height = display.1;
        if stream {
            st.video.output_width = s.output.0;
            st.video.output_height = s.output.1;
            st.video.fps = s.fps;
            st.stream.encoder = s.encoder.clone();
            if let Some(i) = ingest {
                st.stream.service = self.platform.label().to_string();
                st.stream.server = i.server;
            }
            if self.platform == Platform::Twitch && !self.twitch_key.trim().is_empty() && st.stream.key.is_empty() {
                st.stream.key = self.twitch_key.trim().to_string();
            }
        }
        if record {
            st.record.same_as_stream = false;
            st.record.encoder = r.encoder.clone();
            // Record at full quality even if the stream is scaled down.
            st.record.output_size = (r.output != (st.video.output_width, st.video.output_height)).then_some(r.output);
            if !stream {
                st.video.fps = r.fps;
                st.video.output_width = r.output.0;
                st.video.output_height = r.output.1;
                st.record.output_size = None;
            }
        }
        st.ui.setup_done = true;
        st
    }
}

fn rec_card(ui: &mut egui::Ui, title: &str, r: &Recommendation, apply: &mut bool) {
    theme::card().show(ui, |ui| {
        ui.checkbox(apply, RichText::new(format!("Apply {title} settings")).strong());
        egui::Grid::new(title).num_columns(2).show(ui, |ui| {
            ui.label("Resolution");
            ui.label(format!("{}×{}", r.output.0, r.output.1));
            ui.end_row();
            ui.label("Frame rate");
            ui.label(format!("{} fps", r.fps));
            ui.end_row();
            ui.label("Encoder");
            ui.label(&r.encoder.encoder);
            ui.end_row();
            ui.label(match r.encoder.rate_control {
                ferrous_core::RateControl::Cbr | ferrous_core::RateControl::Vbr => "Bitrate",
                _ => "Quality",
            });
            ui.label(match r.encoder.rate_control {
                ferrous_core::RateControl::Cbr | ferrous_core::RateControl::Vbr => format!("{} kbps", r.encoder.bitrate_kbps),
                _ => format!("CQ {}", r.encoder.quality),
            });
            ui.end_row();
        });
        for n in &r.notes {
            ui.label(RichText::new(format!("• {n}")).small().color(theme::muted()));
        }
    });
}
