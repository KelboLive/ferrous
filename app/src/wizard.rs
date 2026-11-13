//! Auto-Configuration Wizard: goal, then preferences, then a reviewable recommendation.

use eframe::egui::{self, DragValue, RichText};
use ferrous_capture::MonitorInfo;
use ferrous_core::Settings;
use ferrous_encode::autoconfig::{self, Goal, Inputs};

use crate::settings_ui::ProbeState;
use crate::theme;

pub enum Outcome {
    Open,
    Cancel,
    /// Boxed: returned every frame, and Settings is large.
    Apply(Box<Settings>),
}

pub struct Wizard {
    base: Settings,
    goal: Goal,
    fps: u32,
    upload_mbps: f32,
    know_upload: bool,
    platform_max_kbps: u32,
}

impl Wizard {
    pub fn new(current: &Settings) -> Self {
        Self { base: current.clone(), goal: Goal::Streaming, fps: 60, upload_mbps: 10.0, know_upload: false, platform_max_kbps: 6000 }
    }

    pub fn show(&mut self, ctx: &egui::Context, probe: &ProbeState, monitors: &[MonitorInfo]) -> Outcome {
        let mut out = Outcome::Open;
        let mut open = true;
        egui::Window::new("Auto-Configuration Wizard").open(&mut open).collapsible(false).default_width(480.0).show(ctx, |ui| {
            crate::widgets::section_title(ui, "What do you want to optimise for?");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.goal, Goal::Streaming, "Streaming (and recording)");
                ui.selectable_value(&mut self.goal, Goal::Recording, "Recording only");
            });
            ui.horizontal(|ui| {
                ui.label("Frame rate");
                ui.selectable_value(&mut self.fps, 30, "30");
                ui.selectable_value(&mut self.fps, 60, "60");
            });
            if self.goal == Goal::Streaming {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.know_upload, "I know my upload speed:");
                    ui.add_enabled(self.know_upload, DragValue::new(&mut self.upload_mbps).range(1.0..=1000.0).speed(0.5).suffix(" Mbps"));
                });
                ui.label(RichText::new("Run a speed test (e.g. fast.com, then \"More info\") to find your upload speed.").small().color(theme::muted()));
                ui.horizontal(|ui| {
                    ui.label("Platform limit");
                    egui::ComboBox::from_id_salt("plat").selected_text(format!("{} kbps", self.platform_max_kbps)).show_ui(ui, |ui| {
                        for (label, kbps) in [("6000 kbps (Twitch and most platforms)", 6000), ("9000 kbps", 9000), ("12000 kbps", 12000), ("20000 kbps", 20000)] {
                            ui.selectable_value(&mut self.platform_max_kbps, kbps, label);
                        }
                    });
                });
            }
            ui.separator();
            let ProbeState::Done(p) = probe else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Testing your encoders…");
                });
                return;
            };
            let display = monitors.first().map(|m| (m.width, m.height)).filter(|(w, h)| *w > 0 && *h > 0)
                .unwrap_or((self.base.video.base_width, self.base.video.base_height));
            let rec = autoconfig::recommend(&Inputs {
                goal: self.goal,
                working: &p.working,
                display,
                prefer_fps: self.fps,
                upload_kbps: (self.goal == Goal::Streaming && self.know_upload).then_some((self.upload_mbps * 1000.0) as u32),
                platform_max_kbps: self.platform_max_kbps,
            });
            crate::widgets::section_title(ui, "Recommended");
            egui::Grid::new("rec").num_columns(2).show(ui, |ui| {
                ui.label("Canvas");
                ui.label(format!("{}×{}", rec.base.0, rec.base.1));
                ui.end_row();
                ui.label("Output");
                ui.label(format!("{}×{} @ {} fps", rec.output.0, rec.output.1, rec.fps));
                ui.end_row();
                ui.label("Encoder");
                ui.label(&rec.encoder.encoder);
                ui.end_row();
                ui.label("Rate control");
                ui.label(match rec.encoder.rate_control {
                    ferrous_core::RateControl::Cbr => format!("CBR {} kbps", rec.encoder.bitrate_kbps),
                    _ => format!("Constant quality {}", rec.encoder.quality),
                });
                ui.end_row();
            });
            for n in &rec.notes {
                ui.label(RichText::new(format!("• {n}")).small().color(theme::muted()));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Apply settings").strong()).clicked() {
                    out = Outcome::Apply(Box::new(autoconfig::apply(&self.base, &rec, self.goal)));
                }
                if ui.button("Cancel").clicked() {
                    out = Outcome::Cancel;
                }
            });
        });
        if !open {
            return Outcome::Cancel;
        }
        out
    }
}
