//! The View → Docks menu, the Stats dock and the Performance Monitor dock.
//! The layout itself (drag, snap, float, persist) lives in `layout.rs`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, RichText, Sense, Shape, Stroke, Vec2};
use ferrous_core::{SnapGuides, UiSettings};
use ferrous_engine::{EngineStatus, SourcePerf};
use ferrous_sysmon::Snapshot;

pub use crate::layout::Dock;
use crate::layout::{self, Layout, Preset, PRESETS};
use crate::theme;

/// The View → Docks menu: show/hide and float every dock, lock the layout,
/// choose snap guides, apply a preset, save and recall named layouts.
/// "Enable preview" and its frame-rate cap (View menu and the preview's
/// right-click menu).
pub fn preview_menu(ui: &mut egui::Ui, settings: &mut UiSettings) {
    ui.checkbox(&mut settings.preview_enabled, "Enable preview")
        .on_hover_text("Turn off to save the CPU/GPU used to draw the preview. Streaming and recording are unaffected.");
    ui.add_enabled_ui(settings.preview_enabled, |ui| {
        ui.horizontal(|ui| {
            ui.label("Preview rate");
            for (fps, label) in [(0, "Match canvas"), (30, "30"), (15, "15")] {
                ui.selectable_value(&mut settings.preview_max_fps, fps, label);
            }
        });
    });
}

/// Returns true when "Custom Browser Docks…" was chosen.
pub fn view_menu(ui: &mut egui::Ui, l: &mut Layout, settings: &mut UiSettings, new_name: &mut String) -> bool {
    ui.set_min_width(260.0);
    ui.set_max_width(300.0);
    let custom: Vec<(layout::Dock, String)> = settings.custom_docks.iter().map(|c| (layout::Dock::Custom(c.id), c.name.clone())).collect();
    let all = layout::ALL.iter().map(|d| (*d, d.title().to_string())).chain(custom);
    for (d, title) in all {
        let open = l.is_open(d);
        ui.horizontal(|ui| {
            let mut shown = open;
            let r = ui.add_enabled(d.closeable(), egui::Checkbox::new(&mut shown, title));
            if r.changed() {
                if shown { l.open(d) } else { l.close(d) }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if open {
                    let mut floating = l.is_floating(d);
                    if ui.toggle_value(&mut floating, "float").on_hover_text("Show in its own window (or drag its tab out of the main window)").changed() {
                        if floating {
                            let at = ui.ctx().content_rect().center() - egui::vec2(170.0, 190.0);
                            l.float(d, at);
                        } else {
                            l.dock_back(d);
                        }
                    }
                }
            });
        });
    }
    ui.separator();
    preview_menu(ui, settings);
    ui.separator();
    let edit_custom = ui.button("🌐 Custom Browser Docks…").on_hover_text("Add docks that show a web page: chat, alerts, activity feeds").clicked();
    if edit_custom {
        ui.close();
    }
    ui.separator();
    ui.checkbox(&mut settings.lock_layout, "🔒 Lock layout").on_hover_text("Prevent docks from being dragged or closed by accident");
    ui.horizontal(|ui| {
        ui.label("Snap guides");
        ui.selectable_value(&mut settings.snap_guides, SnapGuides::Zones, "Zones").on_hover_text("The part of the panel the dock will take lights up as you approach an edge");
        ui.selectable_value(&mut settings.snap_guides, SnapGuides::Compass, "Compass").on_hover_text("Drop buttons appear in the middle of the panel under the pointer");
    });
    ui.separator();
    ui.menu_button("Layout presets", |ui| {
        for p in PRESETS {
            if ui.button(p.label()).clicked() {
                *l = Layout::from_preset(p);
                ui.close();
            }
        }
    });
    ui.menu_button("Saved layouts", |ui| {
        if settings.saved_layouts.is_empty() {
            ui.label(RichText::new("No saved layouts yet").small().color(theme::muted()));
        }
        let mut remove = None;
        for (name, data) in &settings.saved_layouts {
            ui.horizontal(|ui| {
                if ui.button(name).clicked() {
                    if let Some(tree) = layout::parse(data) {
                        l.tree = tree;
                        l.ensure_preview();
                    }
                    ui.close();
                }
                if ui.small_button("🗑").on_hover_text("Delete this layout").clicked() {
                    remove = Some(name.clone());
                }
            });
        }
        if let Some(n) = remove {
            settings.saved_layouts.remove(&n);
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(new_name).hint_text("Layout name").desired_width(140.0));
            if ui.add_enabled(!new_name.trim().is_empty(), egui::Button::new("Save current")).clicked() {
                settings.saved_layouts.insert(new_name.trim().to_string(), layout::serialize(&l.tree));
                new_name.clear();
            }
        });
    });
    if ui.button("Reset to default layout").clicked() {
        *l = Layout::from_preset(Preset::Ferrous);
        settings.lock_layout = false;
        ui.close();
    }
    edit_custom
}

// ---------------------------------------------------------------- graphs

/// A rolling history drawn as a filled sparkline.
#[derive(Default, Clone)]
pub struct History(VecDeque<f32>);

pub const HISTORY_LEN: usize = 240; // 60 s at 4 Hz

impl History {
    pub fn push(&mut self, v: f32) {
        self.0.push_back(v);
        while self.0.len() > HISTORY_LEN {
            self.0.pop_front();
        }
    }
    pub fn last(&self) -> f32 {
        self.0.back().copied().unwrap_or(0.0)
    }
}

pub fn sparkline(ui: &mut egui::Ui, h: &History, max: f32, color: Color32, size: Vec2) {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 3, theme::bg());
    if h.0.len() < 2 {
        return;
    }
    let max = h.0.iter().cloned().fold(max, f32::max).max(1e-3);
    let n = HISTORY_LEN as f32;
    let start = HISTORY_LEN - h.0.len();
    let pts: Vec<Pos2> = h
        .0
        .iter()
        .enumerate()
        .map(|(i, v)| Pos2::new(rect.left() + (start + i) as f32 / (n - 1.0) * rect.width(), rect.bottom() - (v / max).clamp(0.0, 1.0) * (rect.height() - 2.0)))
        .collect();
    let mut fill = pts.clone();
    fill.push(Pos2::new(rect.right(), rect.bottom()));
    fill.push(Pos2::new(pts[0].x, rect.bottom()));
    // Convex fill isn't guaranteed; draw thin vertical bars for the area instead.
    for w in pts.windows(2) {
        p.add(Shape::convex_polygon(
            vec![w[0], w[1], Pos2::new(w[1].x, rect.bottom()), Pos2::new(w[0].x, rect.bottom())],
            color.linear_multiply(0.18),
            Stroke::NONE,
        ));
    }
    p.add(Shape::line(pts, Stroke::new(1.5, color)));
    resp.on_hover_text(format!("now {:.1} · peak {:.1} (last 60 s)", h.last(), h.0.iter().cloned().fold(0.0, f32::max)));
}

/// Eases displayed numbers toward their targets so readouts don't jitter.
#[derive(Default)]
pub struct Smooth(HashMap<String, f32>);

impl Smooth {
    pub fn get(&mut self, key: &str, target: f32) -> f32 {
        let v = self.0.entry(key.to_string()).or_insert(target);
        *v += (target - *v) * 0.15;
        *v
    }
}

fn fmt_bytes(b: u64) -> String {
    let gb = b as f64 / 1_073_741_824.0;
    if gb >= 1.0 { format!("{gb:.1} GB") } else { format!("{:.0} MB", b as f64 / 1_048_576.0) }
}

// ---------------------------------------------------------------- stats

pub struct StatsInput<'a> {
    pub status: &'a EngineStatus,
    pub sys: &'a Snapshot,
    pub destinations: &'a [(String, bool, bool, f32, u64)],
    pub record_kbps_setting: u32,
    pub replay_active: bool,
    pub channels: Vec<(String, f32, bool)>,
    pub links: Vec<(String, u16, std::sync::Arc<ferrous_link::SenderStats>)>,
    pub remote_clients: Option<usize>,
    pub fps_target: u32,
}

const STAT_GROUPS: &[(&str, &[(&str, &str)])] = &[
    ("Rendering", &[("fps", "Frame rate"), ("frame_time", "Average frame time"), ("max_frame", "Worst frame (1 s)"), ("lagged", "Frames missed by rendering"), ("renderer", "Compositor"), ("gpu_pass", "GPU compositing time")]),
    ("System", &[("ember_cpu", "Ferrous CPU"), ("ember_mem", "Ferrous memory"), ("sys_cpu", "System CPU"), ("sys_mem", "System memory"), ("gpu", "GPU usage"), ("vram", "GPU memory (VRAM)")]),
    ("Storage", &[("disk_free", "Free space (recordings drive)"), ("rec_left", "Recording time left")]),
    ("Outputs", &[("outputs", "Stream, destinations, recording")]),
    ("Audio", &[("audio_buffers", "Audio buffer per channel")]),
    ("Network", &[("links", "Ferrous Link outputs"), ("remote", "Remote-control clients")]),
];

fn row(ui: &mut egui::Ui, label: &str, value: impl Into<RichText>) {
    ui.label(RichText::new(label).color(theme::muted()));
    ui.label(value.into());
    ui.end_row();
}

fn level(v: f32, warn: f32, bad: f32) -> Color32 {
    if v >= bad { theme::live() } else if v >= warn { theme::warn() } else { theme::text() }
}

pub fn stats_dock(ui: &mut egui::Ui, s: &StatsInput<'_>, cfg: &mut UiSettings, smooth: &mut Smooth) {
    ui.horizontal(|ui| {
        crate::widgets::dock_title(ui, "Stats");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.menu_button("⚙ Choose stats", |ui| {
                for (group, items) in STAT_GROUPS {
                    ui.label(RichText::new(*group).strong());
                    for (id, label) in *items {
                        let mut on = !cfg.hidden_stats.iter().any(|h| h == id);
                        if ui.checkbox(&mut on, *label).changed() {
                            if on {
                                cfg.hidden_stats.retain(|h| h != id);
                            } else {
                                cfg.hidden_stats.push(id.to_string());
                            }
                        }
                    }
                }
                ui.separator();
                if ui.button("Show all").clicked() {
                    cfg.hidden_stats.clear();
                }
            });
        });
    });
    let on = |id: &str| !cfg.hidden_stats.iter().any(|h| h == id);
    let st = s.status;
    let sys = s.sys;
    egui::ScrollArea::vertical().id_salt("stats_scroll").show(ui, |ui| {
        egui::Grid::new("stats_grid").num_columns(2).striped(true).spacing([18.0, 4.0]).show(ui, |ui| {
            if on("fps") {
                let fps = smooth.get("fps", st.fps);
                row(ui, "Frame rate", RichText::new(format!("{fps:.1} / {} fps", s.fps_target)).color(level(s.fps_target as f32 - fps, 1.0, 5.0)));
            }
            if on("frame_time") {
                let budget = 1000.0 / s.fps_target.max(1) as f32;
                row(ui, "Average frame time", RichText::new(format!("{:.2} ms (budget {budget:.1} ms)", smooth.get("ft", st.render_ms))).color(level(st.render_ms / budget, 0.7, 1.0)));
            }
            if on("max_frame") {
                row(ui, "Worst frame (1 s)", format!("{:.2} ms", st.max_frame_ms));
            }
            if on("lagged") {
                row(ui, "Frames missed by rendering", RichText::new(st.lagged_frames.to_string()).color(if st.lagged_frames > 0 { theme::warn() } else { theme::text() }));
            }
            if on("renderer") {
                row(ui, "Compositor", st.renderer.clone());
            }
            if on("gpu_pass") {
                row(ui, "GPU compositing time", st.gpu_ms.map(|g| format!("{:.3} ms", smooth.get("gpu_ms", g))).unwrap_or_else(|| "n/a (CPU compositor or no timestamps)".into()));
            }
            if on("ember_cpu") {
                let v = smooth.get("own_cpu", sys.own.cpu / sys.cores as f32);
                row(ui, "Ferrous CPU", RichText::new(format!("{v:.1}% of all cores ({:.0}% of one core)", sys.own.cpu)).color(level(v, 50.0, 80.0)));
            }
            if on("ember_mem") {
                row(ui, "Ferrous memory", fmt_bytes(sys.own.mem_bytes));
            }
            if on("sys_cpu") {
                let v = smooth.get("sys_cpu", sys.sys_cpu);
                row(ui, "System CPU", RichText::new(format!("{v:.1}% ({} threads)", sys.cores)).color(level(v, 70.0, 90.0)));
            }
            if on("sys_mem") {
                let pct = sys.sys_mem_used as f32 / sys.sys_mem_total.max(1) as f32 * 100.0;
                row(ui, "System memory", RichText::new(format!("{} / {} ({pct:.0}%)", fmt_bytes(sys.sys_mem_used), fmt_bytes(sys.sys_mem_total))).color(level(pct, 80.0, 92.0)));
            }
            if on("gpu") {
                row(ui, "GPU usage", match sys.gpu {
                    Some(g) => RichText::new(format!("{:.0}% (busiest engine)", smooth.get("gpu", g))).color(level(g, 80.0, 95.0)),
                    None => RichText::new("n/a on this system"),
                });
            }
            if on("vram") {
                row(ui, "GPU memory (VRAM)", sys.vram_used.map(fmt_bytes).unwrap_or_else(|| "n/a on this system".into()));
            }
            if on("disk_free") {
                row(ui, "Free space (recordings drive)", sys.disk_free.map(fmt_bytes).unwrap_or_else(|| "unknown".into()));
            }
            if on("rec_left") {
                let kbps = if st.recording.active && st.recording.bitrate_kbps > 0.0 { st.recording.bitrate_kbps } else { s.record_kbps_setting as f32 };
                let left = sys.disk_free.map(|b| b as f32 * 8.0 / 1000.0 / kbps.max(1.0));
                row(ui, "Recording time left", match left {
                    Some(secs) => {
                        let h = (secs / 3600.0) as u32;
                        RichText::new(format!("~{h} h {:02} min at {:.1} Mbps", (secs / 60.0) as u32 % 60, kbps / 1000.0)).color(if secs < 1800.0 { theme::warn() } else { theme::text() })
                    }
                    None => RichText::new("unknown"),
                });
            }
        });
        if on("outputs") {
            ui.add_space(6.0);
            crate::widgets::section_title(ui, "Outputs");
            egui::Grid::new("outputs_grid").num_columns(6).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                for h in ["Output", "Status", "Time", "Bitrate", "Skipped (encoder)", "Speed"] {
                    ui.label(RichText::new(h).small().strong());
                }
                ui.end_row();
                let mut out = |name: &str, o: &ferrous_engine::OutputStatus| {
                    ui.label(name);
                    let (txt, col) = if o.reconnecting && !o.active { ("Reconnecting", theme::warn()) } else if o.active { ("Live", theme::ok()) } else { ("Off", theme::muted()) };
                    ui.colored_label(col, txt);
                    ui.label(if o.active { crate::widgets::fmt_duration(o.elapsed) } else { "-".into() });
                    ui.label(if o.active { format!("{:.0} kbps", o.bitrate_kbps) } else { "-".into() });
                    let total = (o.frames_written + o.frames_skipped).max(1);
                    let pct = o.frames_skipped as f32 * 100.0 / total as f32;
                    ui.colored_label(level(pct, 0.5, 2.0), if o.active { format!("{} ({pct:.1}%)", o.frames_skipped) } else { "-".into() });
                    ui.colored_label(if o.active && o.speed > 0.0 && o.speed < 0.97 { theme::warn() } else { theme::text() }, if o.active && o.speed > 0.0 { format!("{:.2}x", o.speed) } else { "-".into() });
                    ui.end_row();
                };
                out("Stream", &st.streaming);
                out("Recording", &st.recording);
                for (name, live, retry, kbps, skipped) in s.destinations {
                    ui.label(name);
                    ui.colored_label(if *live { theme::ok() } else { theme::warn() }, if *live { "Live" } else if *retry { "Reconnecting" } else { "Down" });
                    ui.label("-");
                    ui.label(format!("{kbps:.0} kbps"));
                    ui.label(skipped.to_string());
                    ui.label("-");
                    ui.end_row();
                }
                ui.label("Replay buffer");
                ui.colored_label(if s.replay_active { theme::ok() } else { theme::muted() }, if s.replay_active { "Buffering" } else { "Off" });
                for _ in 0..4 {
                    ui.label("");
                }
                ui.end_row();
            });
        }
        if on("audio_buffers") && !s.channels.is_empty() {
            ui.add_space(6.0);
            crate::widgets::section_title(ui, "Audio buffers");
            egui::Grid::new("audio_grid").num_columns(2).striped(true).show(ui, |ui| {
                for (name, ms, active) in &s.channels {
                    ui.label(name);
                    if *active {
                        ui.colored_label(if *ms > 50.0 { theme::warn() } else { theme::text() }, format!("{ms:.0} ms queued"));
                    } else {
                        ui.colored_label(theme::muted(), "no input");
                    }
                    ui.end_row();
                }
            });
        }
        if on("links") && !s.links.is_empty() {
            ui.add_space(6.0);
            crate::widgets::section_title(ui, "Ferrous Link outputs");
            egui::Grid::new("links_grid").num_columns(5).striped(true).show(ui, |ui| {
                for h in ["Link", "Receivers", "Bandwidth", "JPEG quality", "Dropped / skipped"] {
                    ui.label(RichText::new(h).small().strong());
                }
                ui.end_row();
                for (name, port, st) in &s.links {
                    ui.label(format!("{name} :{port}"));
                    ui.label(st.clients.load(Relaxed).to_string());
                    ui.label(format!("{:.1} Mbps", st.kbps.load(Relaxed) as f32 / 1000.0));
                    let q = st.quality.load(Relaxed);
                    ui.label(if q > 0 { q.to_string() } else { "-".into() });
                    ui.label(format!("{} / {}", st.frames_dropped.load(Relaxed), st.frames_skipped.load(Relaxed)));
                    ui.end_row();
                }
            });
        }
        if on("remote") {
            ui.add_space(6.0);
            ui.label(RichText::new(match s.remote_clients {
                Some(n) => format!("Remote control: {n} client(s) connected"),
                None => "Remote control: off".into(),
            }).color(theme::muted()));
        }
    });
}

// ---------------------------------------------------------------- performance

#[derive(Default)]
pub struct PerfState {
    cpu: HashMap<u64, History>,
    gpu: HashMap<u64, History>,
    proc_cpu: HashMap<String, History>,
    total_cpu: History,
    total_gpu: History,
    last_sample: Option<Instant>,
    sort_by_cost: bool,
}

pub struct PerfRow {
    pub id: u64,
    pub scene: String,
    pub name: String,
    pub kind: &'static str,
    pub perf: SourcePerf,
}

/// CPU % of one core implied by `ms` of work per frame at `fps`.
fn ms_to_core_pct(ms: f32, fps: f32) -> f32 {
    ms * fps / 10.0
}

#[allow(clippy::too_many_arguments)]
pub fn perf_dock(
    ui: &mut egui::Ui,
    st: &mut PerfState,
    rows: &[PerfRow],
    outputs: &[(String, u32)],
    sys: &Snapshot,
    fps: f32,
    cfg: &mut UiSettings,
    smooth: &mut Smooth,
) {
    // Sample histories at 4 Hz.
    if st.last_sample.is_none_or(|t| t.elapsed() >= Duration::from_millis(250)) {
        st.last_sample = Some(Instant::now());
        for r in rows {
            let helper: f32 = r.perf.pids.iter().filter_map(|p| sys.per_pid.get(p)).map(|p| p.cpu).sum();
            st.cpu.entry(r.id).or_default().push(ms_to_core_pct(r.perf.source_ms + r.perf.composite_ms, fps) + helper);
            let helper_gpu: f32 = r.perf.pids.iter().filter_map(|p| sys.per_pid.get(p)).filter_map(|p| p.gpu).sum();
            st.gpu.entry(r.id).or_default().push(r.perf.gpu_ms.map(|g| g * fps / 10.0).unwrap_or(0.0) + helper_gpu);
        }
        for (label, pid) in outputs {
            st.proc_cpu.entry(label.clone()).or_default().push(sys.per_pid.get(pid).map(|p| p.cpu).unwrap_or(0.0));
        }
        st.total_cpu.push(sys.own.cpu);
        st.total_gpu.push(sys.gpu.unwrap_or(0.0));
    }
    ui.horizontal(|ui| {
        crate::widgets::dock_title(ui, "Performance Monitor");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.toggle_value(&mut st.sort_by_cost, "Sort by cost");
            crate::widgets::tip(ui.checkbox(&mut cfg.perf_current_scene_only, "Current scene only"), "Current scene only");
        });
    });
    ui.horizontal(|ui| {
        let own = smooth.get("perf_own", sys.own.cpu);
        ui.label(RichText::new(format!("Ferrous total: {:.1}% of all cores ({own:.0}% of one core)", own / sys.cores as f32)).strong())
            .on_hover_text("Task Manager and RTSS show the first figure. A program using several cores can pass 100% of one core: 200% means two cores' worth of work.");
        sparkline(ui, &st.total_cpu, 100.0, theme::accent(), Vec2::new(160.0, 26.0));
        match sys.gpu {
            Some(g) => {
                ui.label(RichText::new(format!("System GPU: {:.0}%", smooth.get("perf_gpu", g))).strong());
                sparkline(ui, &st.total_gpu, 100.0, theme::ok(), Vec2::new(160.0, 26.0));
            }
            None => {
                ui.label(RichText::new("System GPU: n/a").color(theme::muted()));
            }
        }
    });
    ui.label(
        RichText::new("CPU = time Ferrous spends on the source each frame (capture, decode, filters, compositing) + its helper process. GPU = share of the GPU compositing pass by area (est.) + its helper's GPU use. Screen capture's own GPU work happens inside Windows and isn't split per source.")
            .small()
            .color(theme::muted()),
    );
    let mut order: Vec<&PerfRow> = rows.iter().collect();
    if st.sort_by_cost {
        order.sort_by(|a, b| st.cpu.get(&b.id).map(|h| h.last()).unwrap_or(0.0).total_cmp(&st.cpu.get(&a.id).map(|h| h.last()).unwrap_or(0.0)));
    }
    egui::ScrollArea::vertical().id_salt("perf_scroll").show(ui, |ui| {
        egui::Grid::new("perf_grid").num_columns(7).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
            for h in ["Source", "Capture + filters", "Compositing", "Helper process", "CPU (60 s)", "GPU", "GPU (60 s)"] {
                ui.label(RichText::new(h).small().strong());
            }
            ui.end_row();
            for r in order {
                ui.vertical(|ui| {
                    ui.set_min_width(170.0);
                    ui.label(&r.name);
                    ui.label(RichText::new(format!("{} · {}", r.kind, r.scene)).small().color(theme::muted()));
                });
                let src = smooth.get(&format!("s{}", r.id), r.perf.source_ms);
                let comp = smooth.get(&format!("c{}", r.id), r.perf.composite_ms);
                ui.colored_label(level(src, 4.0, 10.0), format!("{src:.2} ms"));
                ui.colored_label(level(comp, 4.0, 10.0), format!("{comp:.2} ms"));
                let helpers: Vec<_> = r.perf.pids.iter().filter_map(|p| sys.per_pid.get(p)).collect();
                if helpers.is_empty() {
                    ui.label(RichText::new("-").color(theme::muted()));
                } else {
                    let c: f32 = helpers.iter().map(|p| p.cpu).sum();
                    let g: Option<f32> = helpers.iter().map(|p| p.gpu).sum();
                    let m: u64 = helpers.iter().map(|p| p.mem_bytes).sum();
                    ui.label(format!("{:.0}% of a core{} · {}", smooth.get(&format!("h{}", r.id), c), g.map(|g| format!(" · {g:.0}% GPU")).unwrap_or_default(), fmt_bytes(m)));
                }
                if let Some(h) = st.cpu.get(&r.id) {
                    sparkline(ui, h, 10.0, theme::accent(), Vec2::new(120.0, 24.0));
                } else {
                    ui.label("");
                }
                let gpu_now = st.gpu.get(&r.id).map(|h| h.last());
                match (r.perf.gpu_ms, gpu_now) {
                    (Some(ms), _) => {
                        ui.label(format!("{:.3} ms est.", smooth.get(&format!("g{}", r.id), ms)));
                    }
                    (None, Some(v)) if v > 0.0 => {
                        ui.label(format!("{v:.0}% (helper)"));
                    }
                    _ => {
                        ui.label(RichText::new("CPU path").small().color(theme::muted()));
                    }
                }
                if let Some(h) = st.gpu.get(&r.id) {
                    sparkline(ui, h, 10.0, theme::ok(), Vec2::new(120.0, 24.0));
                } else {
                    ui.label("");
                }
                ui.end_row();
            }
            for (label, pid) in outputs {
                ui.vertical(|ui| {
                    ui.label(label);
                    ui.label(RichText::new("output · FFmpeg").small().color(theme::muted()));
                });
                ui.label("-");
                ui.label("-");
                let p = sys.per_pid.get(pid).copied().unwrap_or_default();
                ui.label(format!("{:.0}% of a core{} · {}", smooth.get(label, p.cpu), p.gpu.map(|g| format!(" · {g:.0}% GPU")).unwrap_or_default(), fmt_bytes(p.mem_bytes)));
                if let Some(h) = st.proc_cpu.get(label) {
                    sparkline(ui, h, 100.0, theme::accent(), Vec2::new(120.0, 24.0));
                } else {
                    ui.label("");
                }
                ui.label(p.gpu.map(|g| format!("{g:.0}%")).unwrap_or_else(|| "-".into()));
                ui.label("");
                ui.end_row();
            }
        });
    });
    ui.ctx().request_repaint_after(Duration::from_millis(250));
}
