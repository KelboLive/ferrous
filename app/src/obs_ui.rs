//! OBS plugin sources in the UI: the "OBS plugins" add menu (types listed
//! by a short-lived plugin host, in the background) and a settings form
//! built from each plugin's own property list.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};
use ferrous_obs::{Property, PropertyKind, TypeInfo};
use parking_lot::Mutex;

use crate::theme;

#[derive(Default)]
struct Types {
    list: Option<Result<Vec<TypeInfo>, String>>,
    loading: bool,
}

static TYPES: LazyLock<Mutex<Types>> = LazyLock::new(Default::default);

fn load_types() {
    let mut t = TYPES.lock();
    if t.loading || t.list.is_some() {
        return;
    }
    t.loading = true;
    std::thread::spawn(|| {
        let r = ferrous_obs::list_types().map(|(inputs, _)| inputs.into_iter().filter(|i| i.video && !i.id.starts_with("audio_line")).collect()).map_err(|e| format!("{e:#}"));
        let mut t = TYPES.lock();
        t.list = Some(r);
        t.loading = false;
    });
}

/// The OBS plugin source types as `(id, name)`; `None` while they load. Empty
/// when OBS isn't installed or the list couldn't be read.
pub fn types() -> Option<Vec<(String, String)>> {
    load_types();
    match &TYPES.lock().list {
        None => None,
        Some(Err(_)) => Some(Vec::new()),
        Some(Ok(l)) => Some(l.iter().map(|t| (t.id.clone(), t.name.clone())).collect()),
    }
}

struct Form {
    props: Vec<Property>,
    values: serde_json::Value,
    fetched: Instant,
}

static FORMS: LazyLock<Mutex<std::collections::HashMap<u64, Form>>> = LazyLock::new(Default::default);
static FETCHING: LazyLock<Mutex<std::collections::HashSet<u64>>> = LazyLock::new(Default::default);

fn fetch(item: u64) {
    if !FETCHING.lock().insert(item) {
        return;
    }
    std::thread::spawn(move || {
        let host = ferrous_obs::registry().lock().get(&item).and_then(|w| w.upgrade());
        if let Some(h) = host {
            if let Ok((props, values)) = h.properties() {
                FORMS.lock().insert(item, Form { props, values, fetched: Instant::now() });
            }
        }
        FETCHING.lock().remove(&item);
    });
}

/// The settings form for an OBS plugin source. Changes are written into
/// `settings` (JSON); the engine hands them to the running plugin.
pub fn properties(ui: &mut egui::Ui, item: u64, type_id: &str, settings: &mut String) {
    ui.label(RichText::new(format!("OBS plugin · {type_id}")).small().color(theme::muted()));
    let running = ferrous_obs::registry().lock().get(&item).and_then(|w| w.upgrade()).is_some();
    if !running {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new("Starting the plugin…").small());
        });
        ui.ctx().request_repaint_after(Duration::from_millis(500));
        return;
    }
    let stale = FORMS.lock().get(&item).is_none_or(|f| f.fetched.elapsed() > Duration::from_secs(5));
    if stale {
        fetch(item);
    }
    let mut forms = FORMS.lock();
    let Some(form) = forms.get_mut(&item) else {
        ui.spinner();
        ui.ctx().request_repaint_after(Duration::from_millis(200));
        return;
    };
    let mut changed = false;
    let mut click = None;
    egui::Grid::new(("obsprops", item)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        changed |= draw(ui, &form.props, &mut form.values, &mut click);
    });
    if changed {
        *settings = form.values.to_string();
        // The plugin may add or change properties in response; look again soon.
        form.fetched = Instant::now() - Duration::from_secs(4);
    }
    drop(forms);
    if let Some(name) = click {
        if let Some(h) = ferrous_obs::registry().lock().get(&item).and_then(|w| w.upgrade()) {
            std::thread::spawn(move || {
                let _ = h.click(&name);
            });
        }
    }
    ui.ctx().request_repaint_after(Duration::from_secs(1));
}

fn draw(ui: &mut egui::Ui, props: &[Property], values: &mut serde_json::Value, click: &mut Option<String>) -> bool {
    let mut changed = false;
    for p in props.iter().filter(|p| p.visible) {
        match &p.kind {
            PropertyKind::Group { properties } => {
                ui.label(RichText::new(&p.description).strong());
                ui.end_row();
                changed |= draw(ui, properties, values, click);
            }
            PropertyKind::Button => {
                ui.label("");
                if ui.add_enabled(p.enabled, egui::Button::new(&p.description)).clicked() {
                    *click = Some(p.name.clone());
                }
                ui.end_row();
            }
            kind => {
                ui.label(&p.description);
                // A disabled property still shows its value.
                let v = &mut values[&p.name];
                let r = ui.scope(|ui| {
                    if !p.enabled {
                        ui.disable();
                    }
                    field(ui, kind, v)
                });
                changed |= r.inner;
                ui.end_row();
            }
        }
    }
    changed
}

fn field(ui: &mut egui::Ui, kind: &PropertyKind, v: &mut serde_json::Value) -> bool {
    match kind {
        PropertyKind::Bool => {
            let mut b = v.as_bool().unwrap_or(false);
            let c = ui.checkbox(&mut b, "").changed();
            *v = b.into();
            c
        }
        PropertyKind::Int { min, max, step, suffix } => {
            let mut x = v.as_i64().unwrap_or(*min);
            let c = ui.add(egui::DragValue::new(&mut x).range(*min..=*max).speed(*step as f64).suffix(suffix.as_str())).changed();
            *v = x.into();
            c
        }
        PropertyKind::Float { min, max, step, suffix } => {
            let mut x = v.as_f64().unwrap_or(*min);
            let c = ui.add(egui::Slider::new(&mut x, *min..=*max).step_by(*step).suffix(suffix.as_str())).changed();
            *v = x.into();
            c
        }
        PropertyKind::Text { multiline, password, info } => {
            let mut s = v.as_str().unwrap_or("").to_string();
            if *info {
                ui.label(RichText::new(&s).small().color(theme::muted()));
                return false;
            }
            let c = if *multiline {
                ui.add(egui::TextEdit::multiline(&mut s).desired_rows(3)).changed()
            } else {
                ui.add(egui::TextEdit::singleline(&mut s).password(*password)).changed()
            };
            *v = s.into();
            c
        }
        PropertyKind::Path { directory, .. } => {
            let mut s = v.as_str().unwrap_or("").to_string();
            let mut c = false;
            ui.horizontal(|ui| {
                c |= ui.add(egui::TextEdit::singleline(&mut s).desired_width(160.0)).changed();
                if ui.small_button("Browse…").clicked() {
                    let p = if *directory { rfd::FileDialog::new().pick_folder() } else { rfd::FileDialog::new().pick_file() };
                    if let Some(p) = p {
                        s = p.display().to_string();
                        c = true;
                    }
                }
            });
            *v = s.into();
            c
        }
        PropertyKind::List { items } => {
            let current = items.iter().find(|(_, val)| val == v).map(|(n, _)| n.clone()).unwrap_or_else(|| v.to_string());
            let mut c = false;
            egui::ComboBox::from_id_salt(ui.next_auto_id()).selected_text(current).show_ui(ui, |ui| {
                for (name, val) in items {
                    if ui.selectable_label(v == val, name).clicked() {
                        *v = val.clone();
                        c = true;
                    }
                }
            });
            c
        }
        PropertyKind::Color { alpha } => {
            // OBS stores 0xAABBGGRR.
            let n = v.as_u64().unwrap_or(0xFFFF_FFFF) as u32;
            let mut rgba = [n as u8, (n >> 8) as u8, (n >> 16) as u8, (n >> 24) as u8];
            let c = if *alpha {
                ui.color_edit_button_srgba_unmultiplied(&mut rgba).changed()
            } else {
                let mut rgb = [rgba[0], rgba[1], rgba[2]];
                let c = ui.color_edit_button_srgb(&mut rgb).changed();
                rgba = [rgb[0], rgb[1], rgb[2], 255];
                c
            };
            *v = (u32::from_le_bytes(rgba) as u64).into();
            c
        }
        PropertyKind::Font => {
            let face = v["face"].as_str().unwrap_or("Sans").to_string();
            let mut size = v["size"].as_i64().unwrap_or(48);
            let mut c = false;
            ui.horizontal(|ui| {
                let mut f = face.clone();
                c |= ui.add(egui::TextEdit::singleline(&mut f).desired_width(110.0)).changed();
                c |= ui.add(egui::DragValue::new(&mut size).range(4..=512).suffix(" pt")).changed();
                if c {
                    v["face"] = f.into();
                    v["size"] = size.into();
                }
            });
            c
        }
        PropertyKind::Other { .. } | PropertyKind::Button | PropertyKind::Group { .. } => {
            ui.label(RichText::new("(not editable here)").small().color(theme::muted()));
            false
        }
    }
}
