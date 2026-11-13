//! The "Add Source" window: a list of source types on the left, and on the
//! right a tile to create a new one plus the sources of that type already in
//! your scenes (added again as a live clone, so nothing is captured twice).

use eframe::egui::{self, Align2, Color32, FontId, RichText, Sense, Vec2};
use ferrous_core::SourceConfig;

use crate::theme;

/// What can be added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    AppAudio,
    Browser,
    Color,
    Display,
    Game,
    Link,
    Image,
    Media,
    Scene,
    Pattern,
    Text,
    Camera,
    Window,
    /// A Ferrous plugin source: (id, name).
    Plugin(String, String),
    /// An OBS plugin source: (type id, name).
    Obs(String, String),
}

impl Kind {
    /// The built-in kinds, in the order the list shows them.
    pub const BUILTIN: [Kind; 13] = [
        Kind::AppAudio,
        Kind::Browser,
        Kind::Color,
        Kind::Display,
        Kind::Game,
        Kind::Link,
        Kind::Image,
        Kind::Media,
        Kind::Scene,
        Kind::Pattern,
        Kind::Text,
        Kind::Camera,
        Kind::Window,
    ];

    pub fn title(&self) -> String {
        match self {
            Kind::AppAudio => "Application Audio".into(),
            Kind::Browser => "Browser".into(),
            Kind::Color => "Color".into(),
            Kind::Display => "Display Capture".into(),
            Kind::Game => "Game Capture".into(),
            Kind::Link => "Ferrous Link".into(),
            Kind::Image => "Image".into(),
            Kind::Media => "Media".into(),
            Kind::Scene => "Scene".into(),
            Kind::Pattern => "Test Pattern".into(),
            Kind::Text => "Text".into(),
            Kind::Camera => "Video Capture Device".into(),
            Kind::Window => "Window Capture".into(),
            Kind::Plugin(_, n) | Kind::Obs(_, n) => n.clone(),
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            Kind::AppAudio => "🔈",
            Kind::Browser => "🌐",
            Kind::Color => "■",
            Kind::Display => "🖥",
            Kind::Game => "🎮",
            Kind::Link => "🔗",
            Kind::Image => "🖼",
            Kind::Media => "🎞",
            Kind::Scene => "🎬",
            Kind::Pattern => "▤",
            Kind::Text => "🅰",
            Kind::Camera => "📷",
            Kind::Window => "🗗",
            Kind::Plugin(..) => "🔌",
            Kind::Obs(..) => "📦",
        }
    }

    fn blurb(&self) -> &'static str {
        match self {
            Kind::AppAudio => "Sound from one app only, not the whole desktop.",
            Kind::Browser => "A web page, with transparency: chat, alerts, overlays.",
            Kind::Color => "A solid colour block.",
            Kind::Display => "Everything on one monitor.",
            Kind::Game => "Finds and follows your game. Light on the GPU, safe with anti-cheat.",
            Kind::Link => "Another PC's picture over your network.",
            Kind::Image => "A picture file (PNG, JPG, GIF, WebP).",
            Kind::Media => "A video, audio file, or stream URL.",
            Kind::Scene => "Show another scene inside this one.",
            Kind::Pattern => "Colour bars for testing your setup.",
            Kind::Text => "Text, or the contents of a file.",
            Kind::Camera => "A webcam or capture card.",
            Kind::Window => "One app's window.",
            Kind::Plugin(..) => "A source from a Ferrous plugin.",
            Kind::Obs(..) => "A source from your OBS Studio install.",
        }
    }

    /// Stable id for the "recently created" list.
    pub fn key(&self) -> String {
        match self {
            Kind::Plugin(id, _) => format!("plugin:{id}"),
            Kind::Obs(id, _) => format!("obs:{id}"),
            other => other.title(),
        }
    }

    /// The kind of an existing source (Source Clones and nothing else are left out).
    pub fn of(cfg: &SourceConfig) -> Option<Kind> {
        Some(match cfg {
            SourceConfig::DisplayCapture { .. } => Kind::Display,
            SourceConfig::GameCapture { .. } => Kind::Game,
            SourceConfig::WindowCapture { .. } => Kind::Window,
            SourceConfig::Image { .. } => Kind::Image,
            SourceConfig::VideoDevice { .. } => Kind::Camera,
            SourceConfig::Media { .. } => Kind::Media,
            SourceConfig::ApplicationAudio { .. } => Kind::AppAudio,
            SourceConfig::Text { .. } => Kind::Text,
            SourceConfig::Browser { .. } => Kind::Browser,
            SourceConfig::Scene { .. } => Kind::Scene,
            SourceConfig::LinkReceiver { .. } => Kind::Link,
            SourceConfig::Color { .. } => Kind::Color,
            SourceConfig::TestPattern { .. } => Kind::Pattern,
            SourceConfig::Plugin { plugin, .. } => Kind::Plugin(plugin.clone(), plugin.clone()),
            SourceConfig::ObsPlugin { type_id, name, .. } => Kind::Obs(type_id.clone(), name.clone()),
            SourceConfig::Clone { .. } => return None,
        })
    }
}

/// A source already in one of the scenes.
pub struct Existing {
    pub id: u64,
    pub name: String,
    pub scene: String,
    pub kind: Kind,
}

/// What the window wants done.
pub enum Outcome {
    None,
    /// Create a new source (with this name if one was typed).
    New(Kind, Option<String>),
    /// Add an existing source again, as a live clone.
    Existing(u64),
}

#[derive(Clone, PartialEq, Eq)]
enum Page {
    Recent,
    Kind(String),
}

pub struct State {
    pub open: bool,
    page: Page,
    picked: Option<u64>,
    pub name: String,
    pub visible: bool,
}

impl Default for State {
    fn default() -> Self {
        Self { open: false, page: Page::Recent, picked: None, name: String::new(), visible: true }
    }
}

impl State {
    pub fn open(&mut self) {
        self.open = true;
        self.picked = None;
        self.name.clear();
    }
}

/// What the window can offer right now.
pub struct Catalog<'a> {
    pub recent: &'a [String],
    pub existing: &'a [Existing],
    pub plugins: &'a [(String, String)],
    /// OBS plugin types; `None` while they load (or if OBS isn't installed).
    pub obs: Option<&'a [(String, String)]>,
    pub other_scenes: bool,
}

const TILE: Vec2 = Vec2::new(168.0, 126.0);

pub fn show(ctx: &egui::Context, st: &mut State, cat: &Catalog) -> Outcome {
    if !st.open {
        return Outcome::None;
    }
    let mut out = Outcome::None;
    let mut open = true;
    // Every kind the list offers, built-ins first.
    let mut kinds: Vec<Kind> = Kind::BUILTIN.iter().filter(|k| cfg!(windows) || **k != Kind::AppAudio).cloned().collect();
    kinds.sort_by_key(|k| k.title());
    let plugin_kinds: Vec<Kind> = cat.plugins.iter().map(|(id, n)| Kind::Plugin(id.clone(), n.clone())).collect();
    let obs_kinds: Vec<Kind> = cat.obs.unwrap_or_default().iter().map(|(id, n)| Kind::Obs(id.clone(), n.clone())).collect();
    let all: Vec<&Kind> = kinds.iter().chain(&plugin_kinds).chain(&obs_kinds).collect();

    egui::Window::new("Add Source")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .fixed_size([780.0, 470.0])
        .show(ctx, |ui| {
            ui.horizontal_top(|ui| {
                // ---- left: the list of types
                ui.allocate_ui(Vec2::new(215.0, 400.0), |ui| {
                    egui::ScrollArea::vertical().id_salt("add_types").show(ui, |ui| {
                        ui.set_width(200.0);
                        row(ui, st, Page::Recent, "🕒", "Recently Created");
                        ui.add_space(4.0);
                        for k in &kinds {
                            row(ui, st, Page::Kind(k.key()), k.icon(), &k.title());
                        }
                        if !plugin_kinds.is_empty() {
                            ui.add_space(6.0);
                            ui.label(RichText::new("Plugins").small().color(theme::muted()));
                            for k in &plugin_kinds {
                                row(ui, st, Page::Kind(k.key()), k.icon(), &k.title());
                            }
                        }
                        ui.add_space(6.0);
                        ui.label(RichText::new("OBS plugins").small().color(theme::muted()));
                        match cat.obs {
                            None => {
                                ui.label(RichText::new("Looking for OBS…").small().color(theme::muted()));
                                ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
                            }
                            Some([]) => {
                                ui.label(RichText::new("None found").small().color(theme::muted()));
                            }
                            Some(_) => {
                                for k in &obs_kinds {
                                    row(ui, st, Page::Kind(k.key()), k.icon(), &k.title());
                                }
                            }
                        }
                    });
                });
                ui.separator();
                // ---- right: tiles
                ui.vertical(|ui| {
                    let (title, sub) = match &st.page {
                        Page::Recent => ("Recently Created".to_string(), "Pick one you've made before, or choose a type on the left."),
                        Page::Kind(key) => (
                            all.iter().find(|k| k.key() == *key).map(|k| k.title()).unwrap_or_default(),
                            "Create a new one, or add one that's already in a scene.",
                        ),
                    };
                    ui.label(RichText::new(title).strong().size(17.0));
                    ui.label(RichText::new(sub).small().color(theme::muted()));
                    ui.add_space(6.0);
                    let area = Vec2::new(ui.available_width(), 300.0);
                    ui.allocate_ui(area, |ui| {
                        egui::ScrollArea::vertical().id_salt("add_tiles").auto_shrink([false, false]).show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = Vec2::splat(10.0);
                                match st.page.clone() {
                                    Page::Recent => {
                                        let mut any = false;
                                        for key in cat.recent {
                                            if let Some(k) = all.iter().find(|k| k.key() == *key) {
                                                any = true;
                                                if tile(ui, k.icon(), &k.title(), "New", false, !disabled(k, cat)).clicked() {
                                                    out = Outcome::New((*k).clone(), None);
                                                }
                                            }
                                        }
                                        if !any {
                                            ui.label(RichText::new("Nothing yet. Sources you add will show up here.").color(theme::muted()));
                                        }
                                    }
                                    Page::Kind(key) => {
                                        let Some(kind) = all.iter().find(|k| k.key() == key).map(|k| (*k).clone()) else { return };
                                        let ok = !disabled(&kind, cat);
                                        let typed = (!st.name.trim().is_empty()).then(|| st.name.trim().to_string());
                                        let r = tile(ui, "＋", "Create new", kind.blurb(), false, ok);
                                        if r.clicked() {
                                            out = Outcome::New(kind.clone(), typed);
                                        }
                                        for e in cat.existing.iter().filter(|e| e.kind.key() == kind.key()) {
                                            let sel = st.picked == Some(e.id);
                                            let r = tile(ui, kind.icon(), &e.name, &format!("In {}", e.scene), sel, true);
                                            if r.clicked() {
                                                st.picked = Some(e.id);
                                            }
                                            if r.double_clicked() {
                                                out = Outcome::Existing(e.id);
                                            }
                                        }
                                    }
                                }
                            });
                        });
                    });
                    ui.separator();
                    let on_existing = matches!(st.page, Page::Kind(_)) && st.picked.is_some();
                    ui.horizontal(|ui| {
                        if ui.add_enabled(on_existing, egui::Button::new("Add existing").min_size(Vec2::new(120.0, 28.0))).on_hover_text("Adds the selected source again. It shares the original's live picture, so nothing is captured twice.").clicked() {
                            if let Some(id) = st.picked {
                                out = Outcome::Existing(id);
                            }
                        }
                        ui.label(RichText::new("Name").small().color(theme::muted()));
                        ui.add(egui::TextEdit::singleline(&mut st.name).hint_text("Optional").desired_width(150.0));
                    });
                });
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.checkbox(&mut st.visible, "Make source visible");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new("Close").min_size(Vec2::new(90.0, 28.0))).clicked() {
                        st.open = false;
                    }
                });
            });
        });
    if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        st.open = false;
    }
    if !matches!(out, Outcome::None) {
        st.picked = None;
        st.name.clear();
    }
    out
}

/// Whether a kind can't be added right now.
fn disabled(k: &Kind, cat: &Catalog) -> bool {
    *k == Kind::Scene && !cat.other_scenes
}

/// One row of the type list.
fn row(ui: &mut egui::Ui, st: &mut State, page: Page, icon: &str, title: &str) {
    let selected = st.page == page;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 6.0, theme::accent().gamma_multiply(0.85));
    } else if resp.hovered() {
        p.rect_filled(rect, 6.0, theme::surface());
    }
    let col = if selected { theme::on(theme::accent()) } else { theme::text() };
    p.text(rect.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, icon, FontId::proportional(14.0), col);
    p.text(rect.left_center() + Vec2::new(34.0, 0.0), Align2::LEFT_CENTER, title, FontId::proportional(14.0), col);
    if resp.clicked() {
        st.page = page;
        st.picked = None;
    }
}

/// A card: a big icon on a darker panel, with a title and a small line under it.
fn tile(ui: &mut egui::Ui, icon: &str, title: &str, sub: &str, selected: bool, enabled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(TILE, if enabled { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    let fill = if resp.hovered() && enabled { theme::surface().lerp_to_gamma(Color32::WHITE, 0.08) } else { theme::surface() };
    p.rect_filled(rect, 8.0, fill);
    let stroke = if selected { egui::Stroke::new(2.0, theme::accent()) } else { egui::Stroke::new(1.0, theme::border()) };
    p.rect_stroke(rect, 8.0, stroke, egui::StrokeKind::Inside);
    let dim = |c: Color32| if enabled { c } else { c.gamma_multiply(0.4) };
    let art = egui::Rect::from_min_size(rect.min + Vec2::splat(6.0), Vec2::new(rect.width() - 12.0, 58.0));
    p.rect_filled(art, 6.0, theme::bg().lerp_to_gamma(Color32::BLACK, 0.25));
    p.text(art.center(), Align2::CENTER_CENTER, icon, FontId::proportional(30.0), dim(theme::text()));
    let short: String = if title.chars().count() > 23 { title.chars().take(22).collect::<String>() + "…" } else { title.to_string() };
    let name = p.layout_no_wrap(short, FontId::proportional(13.5), dim(theme::text()));
    p.galley(rect.min + Vec2::new(8.0, 68.0), name, theme::text());
    let sub = p.layout(sub.to_string(), FontId::proportional(11.0), dim(theme::muted()), rect.width() - 16.0);
    p.galley(rect.min + Vec2::new(8.0, 87.0), sub, theme::muted());
    if resp.hovered() && enabled {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}
