//! Editors for video and audio filter chains.

use eframe::egui::{self, DragValue, RichText, Slider};
use ferrous_core::filters::{AudioFilter, AudioFilters, Filter, VideoFilter, VideoFilters};

use crate::theme;

/// Shared list UI: enable toggle, reorder, remove, and a body per filter.
fn chain_ui<T: Clone>(
    ui: &mut egui::Ui,
    id: &str,
    list: &mut Vec<Filter<T>>,
    label: impl Fn(&T) -> String,
    defaults: Vec<T>,
    mut body: impl FnMut(&mut egui::Ui, usize, &mut T),
    extra_add: impl FnOnce(&mut egui::Ui) -> Option<T>,
) {
    let mut remove = None;
    let mut swap = None;
    let n = list.len();
    for (i, f) in list.iter_mut().enumerate() {
        ui.push_id((id, i), |ui| {
            egui::Frame::new()
                .fill(theme::bg())
                .corner_radius(6)
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut f.enabled, RichText::new(label(&f.filter)).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("✖").clicked() {
                                remove = Some(i);
                            }
                            if i + 1 < n && ui.small_button("⬇").clicked() {
                                swap = Some((i, i + 1));
                            }
                            if i > 0 && ui.small_button("⬆").clicked() {
                                swap = Some((i, i - 1));
                            }
                        });
                    });
                    if f.enabled {
                        body(ui, i, &mut f.filter);
                    }
                });
        });
        ui.add_space(4.0);
    }
    if let Some(i) = remove {
        list.remove(i);
    }
    if let Some((a, b)) = swap {
        list.swap(a, b);
    }
    ui.horizontal(|ui| {
        ui.menu_button("+ Add filter", |ui| {
            for d in defaults {
                if ui.button(label(&d)).clicked() {
                    list.push(Filter::new(d));
                    ui.close();
                }
            }
        });
        if let Some(f) = extra_add(ui) {
            list.push(Filter::new(f));
        }
    });
}

fn plugin_settings(ui: &mut egui::Ui, plugin: &str, settings: &mut String) {
    ui.label(RichText::new(plugin).small().color(theme::muted()));
    ui.add(egui::TextEdit::multiline(settings).desired_rows(2).desired_width(f32::INFINITY).hint_text("{ } settings JSON"));
    if !settings.trim().is_empty() && serde_json::from_str::<serde_json::Value>(settings).is_err() {
        ui.colored_label(theme::warn(), "Settings aren't valid JSON");
    }
}

fn with_plugins<T>(mut defaults: Vec<T>, kind: ferrous_plugins::Kind, make: impl Fn(String) -> T) -> Vec<T> {
    for (id, _, k) in ferrous_plugins::list() {
        if k == kind {
            defaults.push(make(id));
        }
    }
    defaults
}

pub fn video_filters(ui: &mut egui::Ui, id: &str, list: &mut VideoFilters) {
    let defaults = with_plugins(VideoFilter::defaults(), ferrous_plugins::Kind::VideoFilter, |plugin| VideoFilter::Plugin { plugin, settings: String::new() });
    chain_ui(ui, id, list, |f| f.label().to_string(), defaults, |ui, _, f| match f {
        VideoFilter::Plugin { plugin, settings } => plugin_settings(ui, plugin, settings),
        VideoFilter::ChromaKey { key_rgb, similarity, smoothness, spill } => {
            ui.horizontal(|ui| {
                crate::widgets::row_label(ui, "Key color");
                ui.color_edit_button_srgb(key_rgb);
                if ui.small_button("Green").clicked() {
                    *key_rgb = [0, 255, 0];
                }
                if ui.small_button("Blue").clicked() {
                    *key_rgb = [0, 0, 255];
                }
            });
            ui.add(Slider::new(similarity, 1..=1000).text("Similarity"));
            ui.add(Slider::new(smoothness, 1..=1000).text("Smoothness"));
            ui.add(Slider::new(spill, 1..=1000).text("Spill reduction"));
        }
        VideoFilter::ColorCorrection { gamma, contrast, brightness, saturation, hue_shift, opacity } => {
            ui.add(Slider::new(gamma, -3.0..=3.0).text("Gamma"));
            ui.add(Slider::new(contrast, -4.0..=4.0).text("Contrast"));
            ui.add(Slider::new(brightness, -1.0..=1.0).text("Brightness"));
            ui.add(Slider::new(saturation, -1.0..=5.0).text("Saturation"));
            ui.add(Slider::new(hue_shift, -180.0..=180.0).text("Hue shift"));
            ui.add(Slider::new(opacity, 0.0..=1.0).text("Opacity"));
        }
        VideoFilter::Scroll { speed_x, speed_y } => {
            ui.add(Slider::new(speed_x, -500.0..=500.0).text("Horizontal px/s"));
            ui.add(Slider::new(speed_y, -500.0..=500.0).text("Vertical px/s"));
        }
        VideoFilter::BackgroundRemoval { threshold, softness, smoothing, max_fps } => {
            ui.label(RichText::new("Keeps the person and removes what's behind them, with no green screen. Runs a small model on a background thread, so it costs very little; lower \"Model rate\" if your CPU is busy.").small().color(crate::theme::muted()));
            ui.add(Slider::new(threshold, 0.1..=0.9).text("Threshold")).on_hover_text("How sure the model must be that a pixel is you. Raise it to cut more of the background, lower it if parts of you disappear.");
            ui.add(Slider::new(softness, 0.0..=0.4).text("Edge softness")).on_hover_text("Feathers the cut-out edge.");
            ui.add(Slider::new(smoothing, 0.0..=0.9).text("Smoothing")).on_hover_text("Steadies the edge over time. Higher is calmer but lags fast movement.");
            ui.add(Slider::new(max_fps, 5..=60).text("Model rate (per second)")).on_hover_text("How often the model runs. The mask follows between runs, so 15 to 20 looks smooth and keeps CPU use low.");
        }
        VideoFilter::Lut { path, amount } => {
            ui.horizontal(|ui| {
                let shown = if path.as_os_str().is_empty() { "No LUT".to_string() } else { path.display().to_string() };
                ui.label(RichText::new(shown).small());
                if ui.small_button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("Cube LUT", &["cube"]).pick_file() {
                        *path = p;
                    }
                }
            });
            ui.add(Slider::new(amount, 0.0..=1.0).text("Amount"));
        }
    }, |_| None);
}

/// `live`: the channel's running filter chain, for VST3 parameters and editors.
pub fn audio_filters(ui: &mut egui::Ui, id: &str, list: &mut AudioFilters, live: Option<&parking_lot::Mutex<ferrous_audio::filters::FilterChain>>) {
    let defaults = with_plugins(AudioFilter::defaults(), ferrous_plugins::Kind::AudioFilter, |plugin| AudioFilter::Plugin { plugin, settings: String::new() });
    let label = |f: &AudioFilter| match f {
        AudioFilter::Vst3 { name, .. } => format!("VST3 · {name}"),
        other => other.label().to_string(),
    };
    chain_ui(ui, id, list, label, defaults, |ui, i, f| match f {
        AudioFilter::Plugin { plugin, settings } => plugin_settings(ui, plugin, settings),
        AudioFilter::Vst3 { .. } => crate::vst3_ui::filter_body(ui, (id, i), i, f, live),
        AudioFilter::Gain { db } => {
            ui.add(Slider::new(db, -30.0..=30.0).text("dB"));
        }
        AudioFilter::NoiseSuppression => {
            ui.label(RichText::new("RNNoise: removes fans, hum and keyboard noise.").small().color(theme::muted()));
        }
        AudioFilter::NoiseGate { open_db, close_db, attack_ms, hold_ms, release_ms } => {
            ui.add(Slider::new(close_db, -96.0..=0.0).text("Close threshold dB"));
            ui.add(Slider::new(open_db, -96.0..=0.0).text("Open threshold dB"));
            ui.horizontal(|ui| {
                ui.add(DragValue::new(attack_ms).range(0.0..=1000.0).suffix(" ms attack"));
                ui.add(DragValue::new(hold_ms).range(0.0..=5000.0).suffix(" ms hold"));
                ui.add(DragValue::new(release_ms).range(0.0..=5000.0).suffix(" ms release"));
            });
        }
        AudioFilter::Compressor { ratio, threshold_db, attack_ms, release_ms, makeup_db } => {
            ui.add(Slider::new(ratio, 1.0..=32.0).text("Ratio"));
            ui.add(Slider::new(threshold_db, -60.0..=0.0).text("Threshold dB"));
            ui.horizontal(|ui| {
                ui.add(DragValue::new(attack_ms).range(1.0..=500.0).suffix(" ms attack"));
                ui.add(DragValue::new(release_ms).range(1.0..=1000.0).suffix(" ms release"));
            });
            ui.add(Slider::new(makeup_db, 0.0..=32.0).text("Output gain dB"));
        }
        AudioFilter::Limiter { threshold_db, release_ms } => {
            ui.add(Slider::new(threshold_db, -60.0..=0.0).text("Threshold dB"));
            ui.add(DragValue::new(release_ms).range(1.0..=1000.0).suffix(" ms release"));
        }
    }, crate::vst3_ui::add_menu);
}
