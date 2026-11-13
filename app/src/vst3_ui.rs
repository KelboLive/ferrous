//! VST3 in the filter UI: scanning installed plugins (in a helper process,
//! cached), the "Add VST3 plugin" menu, and each plugin's generic parameter
//! editor with a button for its own editor window (Windows).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, RichText};
use ferrous_core::filters::AudioFilter;
use ferrous_vst3::ClassInfo;
use parking_lot::Mutex;

use crate::theme;

#[derive(Default)]
struct Library {
    classes: Vec<ClassInfo>,
    failed: Vec<(PathBuf, String)>,
    scanning: bool,
    scanned: bool,
}

static LIB: LazyLock<Mutex<Library>> = LazyLock::new(Default::default);

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Cache {
    /// Bundle path -> (modified time in seconds, its effects).
    bundles: BTreeMap<PathBuf, (u64, Vec<ClassInfo>)>,
}

fn cache_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("Ferrous").join("vst3-cache.json"))
}

fn mtime(p: &Path) -> u64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

/// Folders searched: the standard ones plus any in `FERROUS_VST3_PATH`.
pub fn search_paths() -> Vec<PathBuf> {
    let mut v = ferrous_vst3::default_search_paths();
    if let Some(extra) = std::env::var_os("FERROUS_VST3_PATH") {
        v.extend(std::env::split_paths(&extra));
    }
    v
}

/// Scans in the background; bundles unchanged since the last scan come from the cache.
pub fn start_scan(force: bool) {
    {
        let mut l = LIB.lock();
        if l.scanning || (l.scanned && !force) {
            return;
        }
        l.scanning = true;
    }
    std::thread::spawn(move || {
        let mut cache: Cache = cache_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut classes = Vec::new();
        let mut failed = Vec::new();
        let exe = std::env::current_exe().ok();
        for b in ferrous_vst3::find_bundles(&search_paths()) {
            let m = mtime(&b);
            if !force {
                if let Some((t, c)) = cache.bundles.get(&b) {
                    if *t == m {
                        classes.extend(c.iter().cloned());
                        continue;
                    }
                }
            }
            match scan_isolated(exe.as_deref(), &b) {
                Ok(c) => {
                    cache.bundles.insert(b.clone(), (m, c.clone()));
                    classes.extend(c);
                }
                Err(e) => failed.push((b, e)),
            }
        }
        classes.sort_by_cached_key(|c| c.name.to_lowercase());
        if let Some(p) = cache_path() {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(p, serde_json::to_vec_pretty(&cache).unwrap_or_default());
        }
        let mut l = LIB.lock();
        l.classes = classes;
        l.failed = failed;
        l.scanning = false;
        l.scanned = true;
    });
}

/// Runs `ferrous --scan-vst3 <bundle>` with a timeout.
fn scan_isolated(exe: Option<&Path>, bundle: &Path) -> Result<Vec<ClassInfo>, String> {
    let Some(exe) = exe else { return ferrous_vst3::scan(bundle).map_err(|e| e.to_string()) };
    let mut child = std::process::Command::new(exe)
        .arg("--scan-vst3")
        .arg(bundle)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                return Err("timed out while loading".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() { format!("crashed while loading ({})", out.status) } else { err });
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

/// The "+ VST3" menu next to "+ Add filter".
pub fn add_menu(ui: &mut egui::Ui) -> Option<AudioFilter> {
    let mut picked = None;
    ui.menu_button("+ VST3", |ui| {
        start_scan(false);
        ui.set_min_width(280.0);
        let l = LIB.lock();
        if l.scanning {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Scanning plugins…");
            });
        } else if l.classes.is_empty() {
            ui.label(RichText::new("No VST3 effects found").color(theme::muted()));
            for p in search_paths() {
                ui.label(RichText::new(p.display().to_string()).small().color(theme::muted()));
            }
        }
        let filter_id = egui::Id::new("vst3_search");
        let mut q: String = ui.data(|d| d.get_temp(filter_id)).unwrap_or_default();
        if l.classes.len() > 8 {
            ui.add(egui::TextEdit::singleline(&mut q).hint_text("Search plugins"));
            ui.data_mut(|d| d.insert_temp(filter_id, q.clone()));
        }
        let ql = q.to_lowercase();
        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
            for c in l.classes.iter().filter(|c| ql.is_empty() || c.name.to_lowercase().contains(&ql) || c.vendor.to_lowercase().contains(&ql)) {
                let r = ui.button(format!("{}  ·  {}", c.name, c.vendor)).on_hover_text(format!("{}\n{}", c.category, c.path.display()));
                if r.clicked() {
                    picked = Some(AudioFilter::Vst3 { path: c.path.display().to_string(), cid: c.cid.clone(), name: c.name.clone(), state: String::new() });
                    ui.close();
                }
            }
        });
        if !l.failed.is_empty() {
            ui.collapsing(format!("{} couldn't be loaded", l.failed.len()), |ui| {
                for (p, e) in &l.failed {
                    ui.label(RichText::new(format!("{}: {e}", p.file_name().unwrap_or_default().to_string_lossy())).small().color(theme::warn()));
                }
            });
        }
        drop(l);
        ui.separator();
        if ui.button("Rescan").clicked() {
            start_scan(true);
        }
    });
    picked
}

/// Body of a VST3 filter: its parameters (live) and the editor button.
/// Edits made here or in the plugin's editor are saved into `f`'s state
/// (debounced) so they persist.
pub fn filter_body(ui: &mut egui::Ui, salt: impl std::hash::Hash + std::fmt::Debug, index: usize, f: &mut AudioFilter, live: Option<&Mutex<ferrous_audio::filters::FilterChain>>) {
    if !matches!(f, AudioFilter::Vst3 { .. }) {
        return;
    }
    let Some(live) = live else {
        ui.label(RichText::new("Starts when the source is active.").small().color(theme::muted()));
        return;
    };
    let mut chain = live.lock();
    // The running chain may be a frame behind the edited list; only touch it when it matches.
    let matches = chain.config().get(index).is_some_and(|c| c.filter.same_instance(f));
    let AudioFilter::Vst3 { name, state, .. } = f else { return };
    let Some(p) = chain.vst3_mut(index).filter(|_| matches) else {
        ui.label(RichText::new(format!("{name} isn't running (it failed to load, or is starting)")).small().color(theme::warn()));
        return;
    };
    let id = egui::Id::new(salt);
    ui.horizontal(|ui| {
        let lat = p.latency_samples();
        if lat > 0 {
            ui.label(RichText::new(format!("latency {:.1} ms", lat as f32 / 48.0)).small().color(theme::muted()));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let can = p.has_editor();
            let r = ui.add_enabled(can, egui::Button::new("Open plugin editor"));
            let r = if can { r } else { r.on_disabled_hover_text("Plugin editors open on Windows") };
            if r.clicked() {
                if let Err(e) = p.open_editor(&format!("{name} · Ferrous")) {
                    log::warn!("VST3 editor: {e:#}");
                }
            }
        });
    });
    let params = p.params();
    let search_id = id.with("q");
    let mut q: String = ui.data(|d| d.get_temp(search_id)).unwrap_or_default();
    if params.len() > 12 {
        ui.add(egui::TextEdit::singleline(&mut q).hint_text(format!("Filter {} parameters", params.len())));
        ui.data_mut(|d| d.insert_temp(search_id, q.clone()));
    }
    let ql = q.to_lowercase();
    egui::ScrollArea::vertical().id_salt(id.with("params")).max_height(260.0).show(ui, |ui| {
        egui::Grid::new(id.with("grid")).num_columns(3).spacing([8.0, 4.0]).show(ui, |ui| {
            for prm in params.iter().filter(|p| ql.is_empty() || p.title.to_lowercase().contains(&ql)) {
                ui.label(RichText::new(&prm.title).small());
                let mut v = prm.value;
                let changed = ui
                    .add_enabled_ui(!prm.read_only, |ui| {
                        if prm.steps == 1 {
                            let mut on = v >= 0.5;
                            let c = ui.checkbox(&mut on, "").changed();
                            v = if on { 1.0 } else { 0.0 };
                            c
                        } else if prm.steps > 1 {
                            let mut k = (v * prm.steps as f64).round() as i32;
                            let c = ui.add(egui::Slider::new(&mut k, 0..=prm.steps).show_value(false)).changed();
                            v = k as f64 / prm.steps as f64;
                            c
                        } else {
                            let r = ui.add(egui::Slider::new(&mut v, 0.0..=1.0).show_value(false));
                            if r.double_clicked() {
                                v = prm.default;
                                return true;
                            }
                            r.changed()
                        }
                    })
                    .inner;
                if changed {
                    p.set_param(prm.id, v);
                }
                ui.label(RichText::new(format!("{} {}", prm.display, prm.units).trim().to_string()).small().monospace().color(theme::muted()));
                ui.end_row();
            }
        });
    });
    // Save the plugin's state shortly after the last edit.
    let count = p.edit_count();
    let key = id.with("saved");
    let (saved, since): (u64, f64) = ui.data(|d| d.get_temp(key)).unwrap_or((0, 0.0));
    let now = ui.input(|i| i.time);
    if count != saved {
        if since == 0.0 {
            ui.data_mut(|d| d.insert_temp(key, (saved, now)));
            ui.ctx().request_repaint_after(Duration::from_millis(350));
        } else if now - since > 0.3 {
            *state = p.save_state();
            ui.data_mut(|d| d.insert_temp(key, (count, 0.0)));
        } else {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }
    // The editor window can change parameters while this panel is idle.
    if p.has_editor() {
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }
}
