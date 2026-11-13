use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, ColorImage, CornerRadius, Layout, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, TextureHandle, TextureOptions, Vec2,
};
use ferrous_capture::{MonitorInfo, WindowInfo};
use ferrous_core::{HotkeyAction, SceneCollection, Settings, SourceConfig, TextAlign, Transform, TransitionDirection, TransitionKind};
use ferrous_engine::{Engine, EngineEvent};

use crate::settings_ui::{self, ProbeState, SettingsAction, SettingsWindow, SharedProbe};
use crate::{fx, theme};
use crate::widgets::{self, fmt_duration};

struct Toast {
    text: String,
    /// The raw message, when `text` is a plain-language version of it.
    detail: Option<String>,
    error: bool,
    action: Option<ToastAction>,
    at: Instant,
}

/// A button on a toast.
#[derive(Clone, PartialEq)]
enum ToastAction {
    Undo,
    Fix(crate::preflight::Fix),
}

/// Where a template puts a source once its size is known.
#[derive(Clone, Copy)]
enum Place {
    /// Fill the canvas (letterboxed).
    Fill,
    /// A corner picture this fraction of the canvas wide.
    Corner(f32),
    /// Centred, at its own size.
    Center,
}

/// A starting point for a new scene.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Template {
    Blank,
    Screen,
    Chatting,
    Gameplay,
    Tutorial,
    StartingSoon,
}

impl Template {
    const ALL: [Template; 5] = [Template::Screen, Template::Chatting, Template::Gameplay, Template::Tutorial, Template::StartingSoon];

    fn name(self) -> &'static str {
        match self {
            Template::Blank => "Scene",
            Template::Screen => "Screen",
            Template::Chatting => "Just chatting",
            Template::Gameplay => "Gameplay",
            Template::Tutorial => "Tutorial",
            Template::StartingSoon => "Starting soon",
        }
    }

    fn blurb(self) -> &'static str {
        match self {
            Template::Blank => "An empty scene.",
            Template::Screen => "Your whole screen.",
            Template::Chatting => "Your camera, full screen.",
            Template::Gameplay => "A game with your camera in the corner.",
            Template::Tutorial => "Your screen with your camera in the corner.",
            Template::StartingSoon => "A colour card with a “Starting soon” message.",
        }
    }
}

#[derive(Clone, Copy)]
enum DragMode {
    Move,
    /// One of the eight handles, by its place in the box's frame.
    Handle((f32, f32)),
    Rotate,
}

/// The eight resize/crop handles, in the box's frame (corners, then edges).
const HANDLES: [(f32, f32); 8] = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.5, 0.0), (1.0, 0.5), (0.5, 1.0), (0.0, 0.5)];

/// Pointer for a handle: diagonal for corners, straight for edges (by its
/// unrotated direction), or a crosshair while Alt (crop) is held.
fn handle_cursor(h: (f32, f32), crop: bool) -> egui::CursorIcon {
    use egui::CursorIcon::*;
    if crop {
        return Crosshair;
    }
    match h {
        (0.5, _) => ResizeVertical,
        (_, 0.5) => ResizeHorizontal,
        (x, y) if (x == 0.0) == (y == 0.0) => ResizeNwSe,
        _ => ResizeNeSw,
    }
}

struct Drag {
    id: u64,
    mode: DragMode,
    start_pointer: Pos2,
    start: Transform,
    src_size: (u32, u32),
    /// Which canvas the drag started on.
    vert: bool,
}

enum Rename {
    Scene(usize, String),
    Item(u64, String),
}

pub struct FerrousApp {
    pub(crate) engine: Engine,
    pub(crate) settings: Settings,
    pub(crate) scenes: SceneCollection,
    saved_scenes: SceneCollection,
    history: crate::history::History,
    last_scene_save: Instant,
    selected: Option<u64>,
    preview_tex: Option<TextureHandle>,
    preview_serial: u64,
    settings_win: SettingsWindow,
    probe: SharedProbe,
    toasts: Vec<Toast>,
    drag: Option<Drag>,
    rename: Option<Rename>,
    pending_fit: HashSet<u64>,
    add_source: crate::add_source::State,
    monitors: Vec<MonitorInfo>,
    windows: Vec<WindowInfo>,
    video_devices: Vec<String>,
    dshow_audio_devices: Vec<String>,
    /// Mixer channel whose advanced properties window is open.
    audio_props: Option<String>,
    /// Sources whose Properties / Filters windows are open (right-click menu).
    props_win: Option<u64>,
    filters_win: Option<u64>,
    audio_apps: Vec<String>,
    hotkeys: crate::hotkeys::Hotkeys,
    smooth: crate::docks::Smooth,
    setup_dismissed: bool,
    perf_state: crate::docks::PerfState,
    /// Animated meter/fader state per mixer channel.
    strips: HashMap<String, crate::mixer::Strip>,
    /// The modular dock layout (every card is a dock).
    layout: crate::layout::Layout,
    /// Name typed into View → Saved layouts → Save current.
    layout_name: String,
    /// Pixel widths the program and vertical previews were drawn at last
    /// frame (0 = not drawn), and the preview config last sent.
    preview_px: (u32, u32),
    preview_sent: Option<ferrous_engine::PreviewConfig>,
    /// Custom browser docks drawn this frame, and their live pages.
    web_frames: Vec<crate::webdock::WebFrame>,
    webdocks: crate::webdock::WebDocks,
    /// Draft list while the Custom Browser Docks dialog is open.
    dock_editor: Option<Vec<ferrous_core::CustomDock>>,
    pub(crate) remote: Option<crate::remote::Remote>,
    library: ferrous_core::library::Library,
    active: ferrous_core::library::Active,
    wizard: Option<crate::wizard::Wizard>,
    setup: Option<crate::setup::Setup>,
    import_report: Option<(String, ferrous_core::import_obs::ImportReport)>,
    /// Last state reported to remote clients, to emit change events.
    remote_seen: (usize, bool, bool, bool),
    pub(crate) studio_mode: bool,
    pub(crate) studio_scene: usize,
    studio_tex: Option<TextureHandle>,
    /// Extra windows: a program projector and the scenes multiview.
    projector_open: bool,
    multiview_open: bool,
    multiview_tex: Vec<Option<(u64, TextureHandle)>>,
    logo: TextureHandle,
    studio_serial: u64,
    vertical_tex: Option<TextureHandle>,
    vertical_serial: u64,
    /// Go-live prompt for destinations needing this session's server/key.
    session_prompt: Option<Vec<(String, String, String)>>,
    palette: crate::palette::Palette,
    /// The pre-flight window: what was about to start, and what was found.
    preflight: Option<(crate::preflight::Kind, Vec<crate::preflight::Check>)>,
    /// Page of the first-run tour being shown.
    tour_step: usize,
    troubleshooting: bool,
    troubleshooting_filter: String,
    /// The interface zoom last given to egui (to notice Ctrl +/− presses).
    scale_applied: f32,
    /// Where template sources go once their size is known.
    pending_place: HashMap<u64, Place>,
}

impl FerrousApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let library = ferrous_core::library::Library::default_root();
        let active = library.active();
        let mut settings = library.load_profile(&active.profile);
        let scenes = library.load_collection(&active.collection);
        let probe = settings_ui::start_probe(&settings.ffmpeg_path, cc.egui_ctx.clone());
        ferrous_link::discovery::start_listening();
        let engine = Engine::start(settings.clone(), scenes.clone());
        {
            let ctx = cc.egui_ctx.clone();
            engine.set_frame_notify(std::sync::Arc::new(move || ctx.request_repaint()));
        }
        let layout = crate::layout::Layout::load_migrated(&mut settings.ui);
        Self {
            layout,
            layout_name: String::new(),
            preview_px: (0, 0),
            preview_sent: None,
            web_frames: Vec::new(),
            webdocks: Default::default(),
            dock_editor: None,
            strips: HashMap::new(),
            engine,
            saved_scenes: scenes.clone(),
            history: crate::history::History::new(&scenes),
            scenes,
            settings,
            last_scene_save: Instant::now(),
            selected: None,
            preview_tex: None,
            preview_serial: 0,
            settings_win: SettingsWindow::new(),
            probe,
            toasts: Vec::new(),
            drag: None,
            rename: None,
            pending_fit: HashSet::new(),
            add_source: Default::default(),
            monitors: ferrous_capture::list_monitors(),
            windows: Vec::new(),
            video_devices: Vec::new(),
            dshow_audio_devices: Vec::new(),
            audio_props: None,
            props_win: None,
            filters_win: None,
            audio_apps: Vec::new(),
            hotkeys: crate::hotkeys::Hotkeys::new(cc.egui_ctx.clone()),
            smooth: Default::default(),
            setup_dismissed: false,
            perf_state: Default::default(),
            remote: None,
            library,
            active,
            wizard: None,
            setup: None,
            import_report: None,
            remote_seen: (usize::MAX, false, false, false),
            studio_mode: false,
            studio_scene: 0,
            studio_tex: None,
            projector_open: false,
            multiview_open: false,
            multiview_tex: Vec::new(),
            logo: crate::brand::logo_texture(&cc.egui_ctx),
            studio_serial: 0,
            vertical_tex: None,
            vertical_serial: 0,
            session_prompt: None,
            palette: Default::default(),
            preflight: None,
            tour_step: 0,
            troubleshooting: false,
            troubleshooting_filter: String::new(),
            scale_applied: 1.0,
            pending_place: HashMap::new(),
        }
    }

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        let text = text.into();
        if error {
            log::error!("{text}");
            // Say what to do about it, not what the encoder said.
            if let Some((plain, fix)) = crate::preflight::friendly(&text) {
                self.toasts.push(Toast { text: plain, detail: Some(text), error, action: fix.map(ToastAction::Fix), at: Instant::now() });
                return;
            }
        }
        self.toasts.push(Toast { text, detail: None, error, action: None, at: Instant::now() });
    }

    /// A message with an Undo button, for things that were just removed.
    fn toast_undo(&mut self, text: impl Into<String>) {
        self.toasts.push(Toast { text: text.into(), detail: None, error: false, action: Some(ToastAction::Undo), at: Instant::now() });
    }

    fn canvas_size(&self) -> (f32, f32) {
        let v = &self.engine.settings().video;
        (v.base_width as f32, v.base_height as f32)
    }

    /// The scene being edited: the studio preview scene in studio mode,
    /// otherwise the program scene.
    fn edit_index(&self) -> usize {
        let n = self.scenes.scenes.len();
        if self.studio_mode { self.studio_scene.min(n - 1) } else { self.scenes.current.min(n - 1) }
    }

    /// Items a clone can show: every capturable item in any scene, as
    /// `(id, "Scene › name")`. Scenes and other clones are left out.
    fn clonable_items(&self) -> Vec<(u64, String)> {
        self.scenes
            .scenes
            .iter()
            .flat_map(|s| s.items.iter().map(move |i| (s, i)))
            .filter(|(_, i)| !matches!(i.source, SourceConfig::Scene { .. } | SourceConfig::Clone { .. }))
            .map(|(s, i)| (i.id, format!("{} › {}", s.name, i.name)))
            .collect()
    }

    fn edit_scene(&self) -> &ferrous_core::Scene {
        &self.scenes.scenes[self.edit_index()]
    }

    fn edit_scene_mut(&mut self) -> &mut ferrous_core::Scene {
        let i = self.edit_index();
        &mut self.scenes.scenes[i]
    }

    fn item_mut(&mut self, id: u64) -> Option<&mut ferrous_core::SceneItem> {
        self.scenes.scenes.iter_mut().flat_map(|s| s.items.iter_mut()).find(|i| i.id == id)
    }

    fn save_scenes_if_changed(&mut self, force: bool) {
        if self.scenes != self.saved_scenes && (force || self.last_scene_save.elapsed() > Duration::from_secs(2)) {
            if let Err(e) = self.library.save_collection(&self.active.collection, &self.scenes) {
                log::warn!("saving scenes: {e}");
            }
            self.saved_scenes = self.scenes.clone();
            self.last_scene_save = Instant::now();
        }
    }

    // ---------------------------------------------------------------- frame

    /// Copy fader and mute state from the live mixer into what gets saved
    /// (settings for the device channels, the scene item for sources), so
    /// changes from the mixer, hotkeys or the remote API all persist.
    fn persist_mixer_state(&mut self) {
        let differs = |a: f32, b: f32| (a - b).abs() > 1e-4;
        for ch in self.engine.mixer().channels() {
            let (v, m) = (ch.volume(), ch.muted());
            match ch.id.as_str() {
                "desktop" => {
                    let a = &mut self.settings.audio;
                    if differs(a.desktop_volume, v) || a.desktop_muted != m {
                        a.desktop_volume = v;
                        a.desktop_muted = m;
                    }
                }
                "mic" => {
                    let a = &mut self.settings.audio;
                    if differs(a.mic_volume, v) || a.mic_muted != m {
                        a.mic_volume = v;
                        a.mic_muted = m;
                    }
                }
                other => {
                    let Some(id) = other.strip_prefix("source-").and_then(|n| n.parse::<u64>().ok()) else { continue };
                    if let Some(item) = self.item_mut(id) {
                        if differs(item.audio_volume, v) || item.audio_muted != m {
                            item.audio_volume = v;
                            item.audio_muted = m;
                        }
                    }
                }
            }
        }
    }

    fn tick(&mut self, ctx: &egui::Context) {
        self.sync_preview_config(ctx);
        self.persist_mixer_state();
        for ev in self.engine.drain_events() {
            match ev {
                EngineEvent::Info(m) => self.toast(m, false),
                EngineEvent::Error(m) => self.toast(m, true),
            }
        }
        // Auto "fit to screen" for new captures bigger than the canvas.
        let (cw, ch) = self.canvas_size();
        let pending: Vec<u64> = self.pending_fit.iter().copied().collect();
        for id in pending {
            if let Some(((w, h), _)) = self.engine.source_info(id) {
                if w > 0 && h > 0 {
                    if w as f32 > cw || h as f32 > ch {
                        if let Some(item) = self.item_mut(id) {
                            item.transform = Transform::fit_to(w, h, cw as u32, ch as u32);
                        }
                    }
                    self.pending_fit.remove(&id);
                }
            }
        }
        self.place_pending();
        for problem in self.hotkeys.sync(&self.settings.hotkeys) {
            self.toast(problem, true);
        }
        for action in self.hotkeys.poll() {
            self.perform(action);
        }
        self.tick_remote(ctx);
        // Resolve OBS-imported layouts once each source's size is known.
        let pending: Vec<u64> = self.scenes.scenes.iter().flat_map(|s| &s.items).filter(|i| i.pending_layout.is_some()).map(|i| i.id).collect();
        for id in pending {
            if let Some(((w, h), _)) = self.engine.source_info(id) {
                if w > 0 && h > 0 {
                    if let Some(item) = self.item_mut(id) {
                        let layout = item.pending_layout.take().unwrap();
                        item.transform = layout.resolve(&item.transform, w, h);
                    }
                }
            }
        }
        self.engine.set_scenes(&self.scenes);
        self.save_scenes_if_changed(false);

        // Previews are always opaque, where premultiplied == straight alpha,
        // so the cheaper constructor (no per-pixel conversion) is exact.
        if let Some(p) = self.engine.preview() {
            if p.serial != self.preview_serial {
                self.preview_serial = p.serial;
                let img = ColorImage::from_rgba_premultiplied([p.width as usize, p.height as usize], &p.rgba);
                match &mut self.preview_tex {
                    Some(t) if t.size() == [p.width as usize, p.height as usize] => {
                        t.set(img, TextureOptions::LINEAR)
                    }
                    _ => self.preview_tex = Some(ctx.load_texture("preview", img, TextureOptions::LINEAR)),
                }
            }
        }
        if let Some(p) = self.engine.vertical_preview().filter(|_| self.vertical_size().is_some()) {
            if p.serial != self.vertical_serial {
                self.vertical_serial = p.serial;
                let img = ColorImage::from_rgba_premultiplied([p.width as usize, p.height as usize], &p.rgba);
                match &mut self.vertical_tex {
                    Some(t) if t.size() == [p.width as usize, p.height as usize] => t.set(img, TextureOptions::LINEAR),
                    _ => self.vertical_tex = Some(ctx.load_texture("vertical", img, TextureOptions::LINEAR)),
                }
            }
        }
        self.engine.set_studio_scene(self.studio_mode.then(|| self.edit_index()));
        self.engine.set_multiview(self.multiview_open);
        let thumbs = if self.multiview_open { self.engine.multiview_thumbs() } else { Vec::new() };
        self.multiview_tex.resize_with(thumbs.len(), || None);
        for (slot, thumb) in self.multiview_tex.iter_mut().zip(&thumbs) {
            let Some(p) = thumb else { continue };
            if slot.as_ref().is_some_and(|(serial, _)| *serial == p.serial) {
                continue;
            }
            let img = ColorImage::from_rgba_premultiplied([p.width as usize, p.height as usize], &p.rgba);
            match slot {
                Some((s, t)) if t.size() == [p.width as usize, p.height as usize] => {
                    t.set(img, TextureOptions::LINEAR);
                    *s = p.serial;
                }
                _ => *slot = Some((p.serial, ctx.load_texture("multiview", img, TextureOptions::LINEAR))),
            }
        }
        if let Some(p) = self.engine.studio_preview().filter(|_| self.studio_mode) {
            if p.serial != self.studio_serial {
                self.studio_serial = p.serial;
                let img = ColorImage::from_rgba_premultiplied([p.width as usize, p.height as usize], &p.rgba);
                match &mut self.studio_tex {
                    Some(t) if t.size() == [p.width as usize, p.height as usize] => t.set(img, TextureOptions::LINEAR),
                    _ => self.studio_tex = Some(ctx.load_texture("studio", img, TextureOptions::LINEAR)),
                }
            }
        }
        self.toasts.retain(|t| t.at.elapsed() < Duration::from_secs(if t.error || t.action.is_some() { 10 } else { 5 }));
        // New previews, hotkeys, remote commands and input all wake the UI
        // themselves, and docks that animate schedule their own repaints;
        // this slow tick only keeps clocks and status text current.
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    // ------------------------------------------------------------- top bar

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(egui::Image::new(&self.logo).fit_to_exact_size(egui::vec2(20.0, 20.0)));
            ui.label(RichText::new("Ferrous").strong().color(theme::accent()));
            ui.add_space(12.0);
            ui.menu_button("File", |ui| {
                if ui.button("Open recordings folder").clicked() {
                    open_folder(&self.engine.settings().record.folder);
                    ui.close();
                }
                if ui.button("Open config folder").clicked() {
                    open_folder(&ferrous_core::paths::config_dir());
                    ui.close();
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            let simple = self.settings.ui.simple_mode;
            if !simple {
                ui.menu_button("Profile", |ui| self.library_menu(ui, ferrous_core::library::Kind::Profile));
                ui.menu_button("Scene Collection", |ui| self.library_menu(ui, ferrous_core::library::Kind::Collection));
            }
            if !simple {
            ui.menu_button("View", |ui| {
                ui.label(RichText::new("Docks").strong());
                if crate::docks::view_menu(ui, &mut self.layout, &mut self.settings.ui, &mut self.layout_name) {
                    self.dock_editor = Some(self.settings.ui.custom_docks.clone());
                }
                ui.separator();
                ui.label(RichText::new("Monitoring windows").strong());
                if ui.checkbox(&mut self.projector_open, "Program projector").on_hover_text("A window with the live program. Drag it to another monitor and double-click it for full screen.").changed() {
                    ui.close();
                }
                if ui.checkbox(&mut self.multiview_open, "Multiview (all scenes)").on_hover_text("Every scene at once; click one to switch. Starts the sources of scenes that aren't live while it's open.").changed() {
                    ui.close();
                }
            });
            }
            ui.menu_button("Tools", |ui| {
                #[cfg(windows)]
                {
                    let dll = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("ferrous_vcam.dll")));
                    let installed = ferrous_vcam::win::installed();
                    let label = if installed { "Reinstall Virtual Camera…" } else { "Install Virtual Camera…" };
                    if ui.add_enabled(dll.as_ref().is_some_and(|d| d.exists()), egui::Button::new(label)).on_hover_text("Registers Ferrous's camera with Windows (asks for administrator rights once)").clicked() {
                        if let Some(d) = &dll {
                            match ferrous_vcam::win::install(d) {
                                Ok(()) => self.toast("Virtual camera installed", false),
                                Err(e) => self.toast(format!("{e:#}"), true),
                            }
                        }
                        ui.close();
                    }
                    if installed && ui.button("Uninstall Virtual Camera…").clicked() {
                        if let Some(d) = &dll {
                            match ferrous_vcam::win::uninstall(d) {
                                Ok(()) => self.toast("Virtual camera removed", false),
                                Err(e) => self.toast(format!("{e:#}"), true),
                            }
                        }
                        ui.close();
                    }
                    ui.separator();
                }
                if !simple && ui.button("Setup & Benchmark…").clicked() {
                    self.setup = Some(crate::setup::Setup::new(&self.settings));
                    ui.close();
                }
                if ui.button("Auto-Configuration Wizard (quick, no benchmark)…").clicked() {
                    self.wizard = Some(crate::wizard::Wizard::new(&self.settings));
                    ui.close();
                }
            });
            ui.menu_button("Edit", |ui| {
                ui.add_enabled_ui(self.selected.is_some(), |ui| {
                    ui.menu_button("Transform", |ui| {
                        let mut act: Option<fn(&mut Transform)> = None;
                        if ui.button("Rotate 90° clockwise").clicked() {
                            act = Some(|t| t.rotation = (t.rotation + 90.0).rem_euclid(360.0));
                        }
                        if ui.button("Rotate 90° counter-clockwise").clicked() {
                            act = Some(|t| t.rotation = (t.rotation - 90.0).rem_euclid(360.0));
                        }
                        if ui.button("Rotate 180°").clicked() {
                            act = Some(|t| t.rotation = (t.rotation + 180.0).rem_euclid(360.0));
                        }
                        if widgets::tip(ui.button("Reset rotation"), "Reset rotation").clicked() {
                            act = Some(|t| t.rotation = 0.0);
                        }
                        ui.separator();
                        let fit = widgets::tip(ui.button("Fit to screen"), "Fit to screen").clicked();
                        let center = widgets::tip(ui.button("Center on screen"), "Center on screen").clicked();
                        if let Some(id) = self.selected {
                            let size = self.engine.source_info(id).map(|i| i.0).filter(|s| s.0 > 0);
                            let (cw, ch) = self.canvas_size();
                            if let Some(it) = self.item_mut(id).filter(|it| !it.locked) {
                                let t = &mut it.transform;
                                if let Some(f) = act {
                                    f(t);
                                }
                                if let Some((w, h)) = size {
                                    if fit {
                                        *t = fit_rotated(t, w, h, cw, ch);
                                    }
                                    if center {
                                        let (ow, oh) = t.output_size(w, h);
                                        t.x = ((cw - ow) / 2.0).round();
                                        t.y = ((ch - oh) / 2.0).round();
                                    }
                                }
                            }
                        }
                        if act.is_some() || fit || center {
                            ui.close();
                        }
                    });
                });
                ui.separator();
                if ui.button("Settings…").clicked() {
                    self.settings_win.open_with(&self.settings, &self.scenes);
                    ui.close();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Search everything…   Ctrl+K").clicked() {
                    self.palette.toggle();
                    ui.close();
                }
                if ui.button("Quick tour").clicked() {
                    self.tour_step = 0;
                    self.settings.ui.tour_done = false;
                    ui.close();
                }
                if ui.button("Troubleshooting").clicked() {
                    self.troubleshooting = true;
                    ui.close();
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // Simple / Advanced, always one click away.
                let mut simple_now = self.settings.ui.simple_mode;
                ui.horizontal(|ui| {
                    if ui.selectable_label(!simple_now, "Advanced").on_hover_text("Every panel and menu: studio mode, transitions, filters, stats, more.").clicked() {
                        simple_now = false;
                    }
                    if ui.selectable_label(simple_now, "Simple").on_hover_text("Just scenes, sources, the preview, the mixer and the big buttons.").clicked() {
                        simple_now = true;
                    }
                });
                if simple_now != self.settings.ui.simple_mode {
                    self.set_simple_mode(simple_now);
                }
                let st = self.engine.status();
                if st.streaming.active || st.streaming.reconnecting {
                    let text = if st.streaming.reconnecting && !st.streaming.active {
                        "RECONNECTING".to_string()
                    } else {
                        format!("LIVE {}", fmt_duration(st.streaming.elapsed))
                    };
                    widgets::pill(ui, &text, theme::live());
                }
                if st.recording.active {
                    widgets::pill(ui, &format!("● REC {}", fmt_duration(st.recording.elapsed)), theme::rec());
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let st = self.engine.status();
        ui.horizontal(|ui| {
            let small = |t: String| RichText::new(t).small().color(theme::muted());
            ui.label(small(st.renderer.clone()));
            ui.separator();
            ui.label(small(format!("{:.1} fps", st.fps)));
            ui.separator();
            if !self.settings.ui.simple_mode {
                ui.label(small(format!("render {:.1} ms", st.render_ms)));
                ui.separator();
                ui.label(small(format!("lagged {}", st.lagged_frames)));
            } else if st.lagged_frames > 0 {
                ui.label(RichText::new(format!("{} late frames", st.lagged_frames)).small().color(theme::warn()));
            }
            for (name, o) in [("stream", &st.streaming), ("rec", &st.recording)] {
                if o.active {
                    ui.separator();
                    let total = (o.frames_written + o.frames_skipped).max(1);
                    let pct = o.frames_skipped as f32 * 100.0 / total as f32;
                    let speed_color = if o.speed > 0.0 && o.speed < 0.97 { theme::warn() } else { theme::muted() };
                    ui.label(small(format!(
                        "{name}: {:.0} kb/s · skipped {} ({pct:.1}%)",
                        o.bitrate_kbps, o.frames_skipped
                    )));
                    ui.label(RichText::new(format!("{:.2}x", o.speed)).small().color(speed_color))
                        .on_hover_text("Encoder speed vs real time. Below 1.0x means the encoder is overloaded.");
                }
            }
        });
    }

    // ------------------------------------------------------- left: scenes

    fn scenes_dock(&mut self, ui: &mut egui::Ui) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::dock_title(ui, "Scenes");
            let mut switch_to = None;
            for i in 0..self.scenes.scenes.len() {
                if let Some(Rename::Scene(idx, buf)) = &mut self.rename {
                    if *idx == i {
                        let r = ui.text_edit_singleline(buf);
                        // Ask for focus once, when the box appears. Asking
                        // every frame hands focus straight back after Enter,
                        // so the box never reports that editing finished.
                        if !r.has_focus() && !r.lost_focus() {
                            r.request_focus();
                        }
                        if r.lost_focus() {
                            let cancelled = ui.input(|i| i.key_pressed(egui::Key::Escape));
                            let name = buf.trim().to_string();
                            if !name.is_empty() && !cancelled {
                                self.scenes.scenes[i].name = name;
                            }
                            self.rename = None;
                        }
                        continue;
                    }
                }
                let current = self.edit_index() == i;
                let live = self.studio_mode && self.scenes.current == i;
                let mut label = if live { format!("● {}", self.scenes.scenes[i].name) } else { self.scenes.scenes[i].name.clone() };
                if self.scenes.scenes[i].transition.is_some() {
                    label.push_str("  ⇄");
                }
                let r = ui.add_sized([ui.available_width(), 26.0], egui::Button::selectable(current, label));
                let count = self.scenes.scenes.len();
                r.context_menu(|ui| {
                    ui.set_min_width(210.0);
                    if ui.button("✏  Rename").clicked() {
                        self.rename = Some(Rename::Scene(i, self.scenes.scenes[i].name.clone()));
                        ui.close();
                    }
                    if ui.button("⧉  Duplicate").on_hover_text("Copies the scene. Screens, cameras and other live sources become clones of the originals, so nothing is captured twice.").clicked() {
                        if let Some(at) = self.scenes.duplicate_scene(i) {
                            self.history_note_scene_added(at);
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui.add_enabled(i > 0, egui::Button::new("▲  Move up")).clicked() {
                        self.scenes.move_scene(i, -1);
                        ui.close();
                    }
                    if ui.add_enabled(i + 1 < count, egui::Button::new("▼  Move down")).clicked() {
                        self.scenes.move_scene(i, 1);
                        ui.close();
                    }
                    ui.separator();
                    ui.menu_button("⇄  Transition", |ui| {
                        ui.set_min_width(250.0);
                        let mut own = self.scenes.scenes[i].transition.is_some();
                        if ui.checkbox(&mut own, "Use a different transition for this scene").on_hover_text("Used whenever this scene goes live.").changed() {
                            self.scenes.scenes[i].transition = own.then(|| self.scenes.transition.clone());
                        }
                        if let Some(t) = &mut self.scenes.scenes[i].transition {
                            transition_editor(ui, "scene_tkind", t);
                        } else {
                            ui.label(RichText::new("Following the main transition.").small().color(theme::muted()));
                        }
                    });
                    ui.separator();
                    if ui.add_enabled(count > 1, egui::Button::new(RichText::new("🗑  Delete").color(theme::live()))).on_hover_text("Ctrl+Z brings it back.").clicked() {
                        let name = self.scenes.scenes[i].name.clone();
                        self.scenes.remove_scene(i);
                        self.studio_scene = self.studio_scene.min(self.scenes.scenes.len() - 1);
                        self.toast_undo(format!("Deleted scene “{name}”"));
                        ui.close();
                    }
                });
                if r.clicked() {
                    switch_to = Some(i);
                }
                if r.double_clicked() {
                    self.rename = Some(Rename::Scene(i, self.scenes.scenes[i].name.clone()));
                }
            }
            if let Some(i) = switch_to {
                if self.studio_mode {
                    self.studio_scene = i;
                } else {
                    self.scenes.current = i;
                }
                self.selected = None;
            }
            ui.horizontal(|ui| {
                let mut template = None;
                ui.menu_button("+ New", |ui| {
                    ui.set_min_width(260.0);
                    if ui.button("Empty scene").clicked() {
                        template = Some(Template::Blank);
                        ui.close();
                    }
                    ui.separator();
                    ui.label(RichText::new("Start from a template").small().color(theme::muted()));
                    for t in Template::ALL {
                        if ui.button(t.name()).on_hover_text(t.blurb()).clicked() {
                            template = Some(t);
                            ui.close();
                        }
                    }
                });
                if let Some(t) = template {
                    self.add_scene_from_template(t);
                }
                let can_remove = self.scenes.scenes.len() > 1;
                if ui.add_enabled(can_remove, egui::Button::new("−")).on_hover_text("Remove scene").clicked() {
                    let i = self.edit_index();
                    let name = self.scenes.scenes[i].name.clone();
                    self.scenes.remove_scene(i);
                    self.studio_scene = self.studio_scene.min(self.scenes.scenes.len() - 1);
                    self.toast_undo(format!("Deleted scene “{name}”"));
                }
                if ui.button("✏").on_hover_text("Rename (or double-click)").clicked() {
                    let i = self.edit_index();
                    self.rename = Some(Rename::Scene(i, self.scenes.scenes[i].name.clone()));
                }
            });
        });
    }

    /// Makes a freshly added scene the one being edited.
    fn history_note_scene_added(&mut self, at: usize) {
        if self.studio_mode {
            self.studio_scene = at;
        } else {
            self.scenes.current = at;
        }
        self.selected = None;
    }

    fn sources_dock(&mut self, ui: &mut egui::Ui) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::dock_title(ui, "Sources");
            let ids: Vec<u64> = self.edit_scene().items.iter().rev().map(|i| i.id).collect();
            if ids.is_empty() {
                self.empty_sources(ui);
            }
            for id in ids {
                let errored = self.engine.source_info(id).and_then(|(_, e)| e).is_some();
                let selected = self.selected == Some(id);
                let mut rename_done = None;
                let mut toggle_vertical = None;
                let mut open_props = false;
                let mut menu_action = None;
                let vertical_on = self.vertical_size().is_some();
                let vertical_vis: Vec<(u64, bool)> = self.edit_scene().vertical.iter().map(|p| (p.item_id, p.visible)).collect();
                let cur = self.edit_index();
                let rename = &mut self.rename;
                let selected_slot = &mut self.selected;
                let Some(item) = self.scenes.scenes[cur].items.iter_mut().find(|i| i.id == id) else { continue };
                // OBS-style row: name on the left, fixed-size eye and lock
                // toggles on the right (so they line up in every row whatever
                // their state), right-click for the source menu.
                let row = ui.allocate_space(Vec2::new(ui.available_width(), 26.0)).1;
                let resp = ui.interact(row, egui::Id::new(("source_row", id)), Sense::click());
                let fill = if selected {
                    Some(theme::accent().linear_multiply(0.35))
                } else if resp.hovered() {
                    Some(theme::surface())
                } else {
                    None
                };
                if let Some(f) = fill {
                    ui.painter().rect_filled(row, 4.0, f);
                }
                if resp.clicked() || resp.secondary_clicked() {
                    *selected_slot = Some(id);
                }
                if resp.double_clicked() {
                    open_props = true;
                }
                ui.scope_builder(egui::UiBuilder::new().max_rect(row.shrink2(Vec2::new(6.0, 0.0))).layout(Layout::right_to_left(Align::Center)), |ui| {
                    let icon = |ui: &mut egui::Ui, glyph: &str, on: bool, tip: &str| -> bool {
                        let text = RichText::new(glyph).color(if on { theme::text() } else { theme::muted().linear_multiply(0.6) });
                        ui.add_sized([24.0, 22.0], egui::Button::new(text).frame(false)).on_hover_text(tip).clicked()
                    };
                    if icon(ui, if item.locked { "🔒" } else { "🔓" }, item.locked, "Lock position") {
                        item.locked = !item.locked;
                    }
                    if icon(ui, "👁", item.visible, "Show / hide") {
                        item.visible = !item.visible;
                    }
                    if vertical_on {
                        let vis = vertical_vis.iter().find(|(i, _)| *i == id).map(|(_, v)| *v).unwrap_or(item.visible);
                        if icon(ui, "📱", vis, "Show / hide on the vertical canvas") {
                            toggle_vertical = Some(id);
                        }
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        if let Some(Rename::Item(rid, buf)) = rename {
                            if *rid == id {
                                let r = ui.add(egui::TextEdit::singleline(buf).desired_width(ui.available_width()));
                                if !r.has_focus() && !r.lost_focus() {
                                    r.request_focus();
                                }
                                if r.lost_focus() {
                                    let cancelled = ui.input(|i| i.key_pressed(egui::Key::Escape));
                                    rename_done = Some(if cancelled { String::new() } else { buf.trim().to_string() });
                                }
                                return;
                            }
                        }
                        let mut text = RichText::new(&item.name);
                        if !item.visible {
                            text = text.color(theme::muted());
                        }
                        if errored {
                            text = text.color(theme::warn());
                        }
                        ui.add(egui::Label::new(text).truncate().selectable(false));
                    });
                });
                let (visible, locked, keep) = (item.visible, item.locked, item.keep_running);
                resp.context_menu(|ui| menu_action = source_menu(ui, visible, locked, keep));
                if let Some(id) = toggle_vertical {
                    if let Some((w, h)) = self.vertical_size() {
                        let (mw, mh) = self.canvas_size();
                        if let Some(p) = self.edit_scene_mut().vertical_placement_mut(id, (w as u32, h as u32), (mw as u32, mh as u32)) {
                            p.visible = !p.visible;
                        }
                    }
                }
                if let Some(name) = rename_done {
                    if !name.is_empty() {
                        if let Some(item) = self.item_mut(id) {
                            item.name = name;
                        }
                    }
                    self.rename = None;
                }
                if open_props {
                    menu_action = Some(SourceAction::Properties);
                }
                if let Some(a) = menu_action {
                    self.source_action(id, a, false);
                }
            }
            // F2 renames the selected source, as in OBS.
            if let Some(id) = self.selected {
                if self.rename.is_none() && !ui.ctx().egui_wants_keyboard_input() && ui.input(|i| i.key_pressed(egui::Key::F2)) {
                    self.source_action(id, SourceAction::Rename, false);
                }
            }
            ui.horizontal(|ui| {
                if ui.button("+").on_hover_text("Add a source").clicked() {
                    self.add_source.open();
                }
                let sel = self.selected;
                if ui.add_enabled(sel.is_some(), egui::Button::new("−")).on_hover_text("Remove source").clicked() {
                    if let Some(id) = sel {
                        self.source_action(id, SourceAction::Remove, false);
                    }
                }
                if ui.add_enabled(sel.is_some(), egui::Button::new("⬆")).on_hover_text("Move up").clicked() {
                    self.move_selected(1);
                }
                if ui.add_enabled(sel.is_some(), egui::Button::new("⬇")).on_hover_text("Move down").clicked() {
                    self.move_selected(-1);
                }
                if ui.add_enabled(self.history.can_undo(), egui::Button::new("↶")).on_hover_text("Undo (Ctrl+Z)").clicked() {
                    self.undo_edit();
                }
                if ui.add_enabled(self.history.can_redo(), egui::Button::new("↷")).on_hover_text("Redo (Ctrl+Shift+Z)").clicked() {
                    self.redo_edit();
                }
            });
        });
    }

    /// A new source of `kind` with sensible defaults. `None` if the user
    /// cancelled a file picker.
    fn make_source(&mut self, kind: &crate::add_source::Kind) -> Option<(String, SourceConfig)> {
        use crate::add_source::Kind;
        let (cw, ch) = self.canvas_size();
        Some(match kind {
            Kind::Display => ("Display Capture".into(), SourceConfig::DisplayCapture { monitor_index: 0, capture_cursor: true }),
            Kind::Game => ("Game Capture".into(), SourceConfig::GameCapture { program: String::new(), capture_cursor: true }),
            Kind::Window => {
                self.windows = ferrous_capture::list_windows();
                let title = self.windows.first().map(|w| w.title.clone()).unwrap_or_default();
                ("Window Capture".into(), SourceConfig::WindowCapture { title_contains: title, capture_cursor: true })
            }
            Kind::Image => {
                let path = rfd::FileDialog::new().add_filter("Images", &["png", "jpg", "jpeg", "bmp", "gif", "webp"]).pick_file()?;
                (path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or("Image".into()), SourceConfig::Image { path })
            }
            Kind::Camera => {
                self.refresh_devices();
                let device = self.video_devices.first().cloned().unwrap_or_default();
                ("Video Capture Device".into(), SourceConfig::VideoDevice { device, resolution: None, fps: None, audio_device: None })
            }
            Kind::Media => {
                let path = rfd::FileDialog::new()
                    .add_filter("Media", &["mp4", "mkv", "mov", "webm", "avi", "flv", "ts", "mp3", "wav", "ogg", "flac", "gif"])
                    .pick_file()?;
                (path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or("Media".into()), SourceConfig::Media { input: path.display().to_string(), looping: true, hw_decode: true })
            }
            Kind::AppAudio => {
                self.audio_apps = ferrous_capture::list_audio_apps();
                let process = self.audio_apps.first().cloned().unwrap_or_default();
                ("Application Audio".into(), SourceConfig::ApplicationAudio { process })
            }
            Kind::Text => (
                "Text".into(),
                SourceConfig::Text {
                    text: "Hello!".into(), from_file: None, font: String::new(), size: 72.0,
                    bgra: [255, 255, 255, 255], outline: ([0, 0, 0, 255], 0), background: [0, 0, 0, 0],
                    align: TextAlign::Left, wrap_width: 0,
                },
            ),
            Kind::Browser => ("Browser".into(), SourceConfig::Browser { url: "https://example.com".into(), width: 800, height: 600, fps: 30, css: String::new() }),
            Kind::Scene => {
                let own = self.edit_scene().name.clone();
                let other = self.scenes.scenes.iter().map(|s| s.name.clone()).find(|n| *n != own)?;
                (other.clone(), SourceConfig::Scene { name: other })
            }
            Kind::Link => {
                let first = ferrous_link::discovered().first().map(|a| a.name.clone()).unwrap_or_default();
                ("Ferrous Link".into(), SourceConfig::LinkReceiver { sender: first, pin: String::new() })
            }
            Kind::Color => ("Color".into(), SourceConfig::Color { bgra: [0x3d, 0x7a, 0xff, 255], width: 400, height: 300 }),
            Kind::Pattern => ("Test Pattern".into(), SourceConfig::TestPattern { width: cw as u32, height: ch as u32 }),
            Kind::Plugin(id, name) => (name.clone(), SourceConfig::Plugin { plugin: id.clone(), settings: String::new() }),
            Kind::Obs(id, name) => (name.clone(), SourceConfig::ObsPlugin { type_id: id.clone(), name: name.clone(), settings: String::new() }),
        })
    }

    /// Draws the Add Source window and carries out what it asks for.
    fn add_source_window(&mut self, ctx: &egui::Context) {
        use crate::add_source::{Catalog, Existing, Kind, Outcome};
        if !self.add_source.open {
            return;
        }
        let own = self.edit_scene().name.clone();
        let existing: Vec<Existing> = self
            .scenes
            .scenes
            .iter()
            .flat_map(|s| s.items.iter().map(move |i| (s, i)))
            .filter_map(|(s, i)| Some(Existing { id: i.id, name: i.name.clone(), scene: s.name.clone(), kind: Kind::of(&i.source)? }))
            .collect();
        let plugins: Vec<(String, String)> = ferrous_plugins::list()
            .into_iter()
            .filter(|(_, _, k)| *k == ferrous_plugins::Kind::VideoSource)
            .map(|(id, name, _)| (id, name))
            .collect();
        let obs = crate::obs_ui::types();
        let other_scenes = self.scenes.scenes.iter().any(|s| s.name != own);
        let recent = self.settings.ui.recent_sources.clone();
        let cat = Catalog { recent: &recent, existing: &existing, plugins: &plugins, obs: obs.as_deref(), other_scenes };
        let outcome = crate::add_source::show(ctx, &mut self.add_source, &cat);
        let visible = self.add_source.visible;
        let added = match outcome {
            Outcome::None => None,
            Outcome::New(kind, name) => {
                let made = self.make_source(&kind);
                if made.is_some() {
                    let r = &mut self.settings.ui.recent_sources;
                    r.retain(|k| *k != kind.key());
                    r.insert(0, kind.key());
                    r.truncate(8);
                }
                made.map(|(default, cfg)| (name.unwrap_or(default), cfg))
            }
            Outcome::Existing(id) => {
                // A source already in a scene is shown again as a clone of it.
                let original = self.scenes.scenes.iter().flat_map(|s| &s.items).find(|i| i.id == id).map(|i| i.name.clone());
                original.map(|n| (format!("{n} (clone)"), SourceConfig::Clone { source: id }))
            }
        };
        if let Some((name, cfg)) = added {
            let target = self.edit_index();
            let id = self.scenes.add_item_to(target, name, cfg);
            if let Some(item) = self.edit_scene_mut().items.iter_mut().find(|i| i.id == id) {
                item.visible = visible;
            }
            self.pending_fit.insert(id);
            self.selected = Some(id);
        }
    }

    /// A separate window with the live program: put it on another monitor and
    /// double-click for full screen (Esc leaves full screen).
    fn projector_window(&mut self, ctx: &egui::Context) {
        if !self.projector_open {
            return;
        }
        let mut close = false;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("ferrous-projector"),
            egui::ViewportBuilder::default().with_title("Ferrous Projector").with_inner_size([960.0, 540.0]).with_min_inner_size([320.0, 180.0]),
            |root, _| {
                let ctx = root.ctx().clone();
                if ctx.input(|i| i.viewport().close_requested()) {
                    close = true;
                }
                let full = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::BLACK)).show_inside(root, |ui| {
                    let (cw, ch) = self.canvas_size();
                    let avail = ui.max_rect();
                    let scale = (avail.width() / cw).min(avail.height() / ch).max(0.01);
                    let rect = Rect::from_center_size(avail.center(), Vec2::new(cw * scale, ch * scale));
                    // Ask the engine for a picture as big as this window shows it.
                    self.note_preview_drawn(ui, rect, false);
                    if let Some(tex) = &self.preview_tex {
                        ui.painter().image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    }
                    let r = ui.interact(avail, egui::Id::new("projector_area"), Sense::click());
                    if r.double_clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!full));
                    }
                    if full {
                        ctx.set_cursor_icon(egui::CursorIcon::None);
                    }
                });
                if full && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                }
            },
        );
        if close {
            self.projector_open = false;
        }
    }

    /// Every scene as a small live picture; click one to switch to it. The
    /// program is outlined in red and, in studio mode, the preview in green.
    fn multiview_window(&mut self, ctx: &egui::Context) {
        if !self.multiview_open {
            return;
        }
        let mut close = false;
        let mut go = None;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("ferrous-multiview"),
            egui::ViewportBuilder::default().with_title("Ferrous Multiview").with_inner_size([1000.0, 640.0]).with_min_inner_size([480.0, 320.0]),
            |root, _| {
                if root.ctx().input(|i| i.viewport().close_requested()) {
                    close = true;
                }
                egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::from_rgb(8, 8, 10)).inner_margin(8.0)).show_inside(root, |ui| {
                    let (cw, ch) = self.canvas_size();
                    let aspect = cw / ch;
                    let n = self.scenes.scenes.len();
                    let cols = if n <= 4 { 2 } else if n <= 9 { 3 } else { 4 };
                    let gap = 8.0;
                    let cell_w = ((ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32).max(60.0);
                    let cell = Vec2::new(cell_w, cell_w / aspect);
                    let live = self.scenes.current;
                    let preview = self.studio_mode.then_some(self.studio_scene);
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.spacing_mut().item_spacing = Vec2::splat(gap);
                        egui::Grid::new("multiview_grid").spacing(Vec2::splat(gap)).show(ui, |ui| {
                            for i in 0..n {
                                let (rect, resp) = ui.allocate_exact_size(cell, Sense::click());
                                let p = ui.painter();
                                p.rect_filled(rect, 2.0, Color32::BLACK);
                                if let Some(Some((_, tex))) = self.multiview_tex.get(i) {
                                    p.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                                }
                                let colour = if i == live { Some(theme::live()) } else if preview == Some(i) { Some(theme::ok()) } else { None };
                                if let Some(c) = colour {
                                    p.rect_stroke(rect, 2.0, Stroke::new(3.0, c), StrokeKind::Inside);
                                } else if resp.hovered() {
                                    p.rect_stroke(rect, 2.0, Stroke::new(2.0, theme::accent()), StrokeKind::Inside);
                                }
                                let label = p.layout_no_wrap(format!("{}  {}", i + 1, self.scenes.scenes[i].name), egui::FontId::proportional(13.0), Color32::WHITE);
                                let tag = Rect::from_min_size(rect.min, label.size() + Vec2::new(12.0, 6.0));
                                p.rect_filled(tag, 2.0, Color32::from_black_alpha(170));
                                p.galley(tag.min + Vec2::new(6.0, 3.0), label, Color32::WHITE);
                                if resp.clicked() {
                                    go = Some(i);
                                }
                                if (i + 1) % cols == 0 {
                                    ui.end_row();
                                }
                            }
                        });
                    });
                });
            },
        );
        if close {
            self.multiview_open = false;
        }
        if let Some(i) = go {
            if self.studio_mode {
                self.studio_scene = i;
            } else {
                self.scenes.current = i;
            }
            self.selected = None;
        }
    }

    fn undo_edit(&mut self) {
        if self.history.undo(&mut self.scenes) {
            self.after_history("Undone");
        }
    }

    fn redo_edit(&mut self) {
        if self.history.redo(&mut self.scenes) {
            self.after_history("Redone");
        }
    }

    fn after_history(&mut self, what: &str) {
        if self.selected.is_some_and(|id| !self.scenes.scenes.iter().any(|s| s.items.iter().any(|i| i.id == id))) {
            self.selected = None;
        }
        self.toast(what, false);
    }

    /// Ctrl+Z, Ctrl+Shift+Z and Ctrl+Y, unless a text field has the keyboard,
    /// then settles the edit for the history.
    fn history_tick(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();
        if !typing {
            let (undo, redo) = ctx.input_mut(|i| {
                let redo = i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Z) || i.consume_key(egui::Modifiers::COMMAND, egui::Key::Y);
                (!redo && i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z), redo)
            });
            if undo {
                self.undo_edit();
            } else if redo {
                self.redo_edit();
            }
        }
        let busy = typing || ctx.input(|i| i.pointer.any_down());
        self.history.observe(&self.scenes, busy);
    }

    fn refresh_devices(&mut self) {
        let ffmpeg = ferrous_encode::ffmpeg::locate(&self.settings.ffmpeg_path);
        self.video_devices = ferrous_capture::list_video_devices(&ffmpeg);
        self.dshow_audio_devices = ferrous_capture::list_audio_devices(&ffmpeg);
    }

    fn tick_remote(&mut self, ctx: &egui::Context) {
        let want = self.settings.remote.enabled.then(|| self.settings.remote.clone());
        let have = self.remote.as_ref().map(|r| r.config.clone());
        if want != have {
            self.remote = None;
            if let Some(cfg) = want {
                match crate::remote::start(&cfg, ctx) {
                    Ok(r) => {
                        self.toast(format!("Remote control listening on port {}", r.server.port()), false);
                        self.remote = Some(r);
                    }
                    Err(e) => {
                        self.toast(format!("Remote control couldn't start on port {}: {e}", cfg.port), true);
                        self.settings.remote.enabled = false;
                    }
                }
            }
        }
        let Some(calls) = self.remote.as_ref().map(|r| r.calls.clone()) else { return };
        for call in calls.try_iter() {
            let reply = self.handle_remote(&call.request_type, &call.data);
            let _ = call.reply.send(reply);
        }
        // Change events.
        let program = self.scenes.current;
        let st = self.engine.status();
        let now = (program, st.recording.active, st.streaming.active, self.studio_mode);
        if now != self.remote_seen {
            let before = self.remote_seen;
            self.remote_seen = now;
            if before.0 == usize::MAX {
                return;
            }
            let Some(r) = &self.remote else { return };
            use ferrous_remote::intent;
            use serde_json::json;
            if before.0 != now.0 {
                let name = self.scenes.scenes.get(program).map(|s| s.name.clone()).unwrap_or_default();
                r.server.emit("CurrentProgramSceneChanged", intent::SCENES, json!({ "sceneName": name }));
            }
            if before.1 != now.1 {
                let state = if now.1 { "OBS_WEBSOCKET_OUTPUT_STARTED" } else { "OBS_WEBSOCKET_OUTPUT_STOPPED" };
                r.server.emit("RecordStateChanged", intent::OUTPUTS, json!({ "outputActive": now.1, "outputState": state, "outputPath": st.recording_path.map(|p| p.display().to_string()) }));
            }
            if before.2 != now.2 {
                let state = if now.2 { "OBS_WEBSOCKET_OUTPUT_STARTED" } else { "OBS_WEBSOCKET_OUTPUT_STOPPED" };
                r.server.emit("StreamStateChanged", intent::OUTPUTS, json!({ "outputActive": now.2, "outputState": state }));
            }
            if before.3 != now.3 {
                r.server.emit("StudioModeStateChanged", intent::UI, json!({ "studioModeEnabled": now.3 }));
            }
        }
    }

    #[allow(deprecated)]
    fn library_menu(&mut self, ui: &mut egui::Ui, kind: ferrous_core::library::Kind) {
        use ferrous_core::library::Kind;
        let (names, current) = match kind {
            Kind::Profile => (self.library.profiles(), self.active.profile.clone()),
            Kind::Collection => (self.library.collections(), self.active.collection.clone()),
        };
        for n in &names {
            if ui.selectable_label(*n == current, n).clicked() && *n != current {
                self.switch_library(kind, n.clone());
                ui.close();
            }
        }
        ui.separator();
        if ui.button("New").clicked() {
            let name = self.library.unique_name(kind, if kind == Kind::Profile { "Profile" } else { "Scenes" });
            match kind {
                Kind::Profile => { let _ = self.library.save_profile(&name, &Settings::default()); }
                Kind::Collection => { let _ = self.library.save_collection(&name, &SceneCollection::default()); }
            }
            self.switch_library(kind, name);
            ui.close();
        }
        if widgets::tip(ui.button("Duplicate"), "Duplicate").clicked() {
            let name = self.library.unique_name(kind, &format!("{current} copy"));
            match kind {
                Kind::Profile => { let _ = self.library.save_profile(&name, &self.settings); }
                Kind::Collection => { let _ = self.library.save_collection(&name, &self.scenes); }
            }
            self.switch_library(kind, name);
            ui.close();
        }
        ui.menu_button("Rename", |ui| {
            let id = egui::Id::new(("rename_lib", kind == Kind::Profile));
            let mut buf = ui.data_mut(|d| d.get_temp::<String>(id)).unwrap_or_else(|| current.clone());
            let r = ui.text_edit_singleline(&mut buf);
            ui.data_mut(|d| d.insert_temp(id, buf.clone()));
            if (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) || widgets::tip(ui.button("Rename"), "Rename").clicked() {
                self.save_scenes_if_changed(true);
                match self.library.rename(kind, &current, &buf) {
                    Ok(new) => {
                        match kind {
                            Kind::Profile => self.active.profile = new,
                            Kind::Collection => self.active.collection = new,
                        }
                        let _ = self.library.set_active(&self.active);
                    }
                    Err(e) => self.toast(format!("Rename failed: {e}"), true),
                }
                ui.data_mut(|d| d.remove::<String>(id));
                ui.close();
            }
        });
        if ui.add_enabled(names.len() > 1, egui::Button::new("Remove")).clicked() {
            if let Err(e) = self.library.remove(kind, &current) {
                self.toast(e.to_string(), true);
            } else {
                let next = names.iter().find(|n| **n != current).cloned().unwrap_or_default();
                self.switch_library_unsaved(kind, next);
            }
            ui.close();
        }
        if kind == Kind::Collection {
            ui.separator();
            if ui.button("Import from OBS Studio…").clicked() {
                let mut dlg = rfd::FileDialog::new().add_filter("OBS scene collection", &["json"]);
                if let Some(d) = ferrous_core::import_obs::obs_collections_dir() {
                    dlg = dlg.set_directory(d);
                }
                if let Some(path) = dlg.pick_file() {
                    match ferrous_core::import_obs::import_file(&path) {
                        Ok((coll, report)) => {
                            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or("Imported".into());
                            let name = self.library.unique_name(Kind::Collection, &format!("{stem} (from OBS)"));
                            let _ = self.library.save_collection(&name, &coll);
                            self.switch_library(Kind::Collection, name.clone());
                            self.import_report = Some((name, report));
                        }
                        Err(e) => self.toast(format!("Import failed: {e}"), true),
                    }
                }
                ui.close();
            }
        }
    }

    fn switch_library(&mut self, kind: ferrous_core::library::Kind, name: String) {
        // Save the one we're leaving first.
        match kind {
            ferrous_core::library::Kind::Profile => {
                let _ = self.library.save_profile(&self.active.profile, &self.settings);
            }
            ferrous_core::library::Kind::Collection => self.save_scenes_if_changed(true),
        }
        self.switch_library_unsaved(kind, name);
    }

    fn switch_library_unsaved(&mut self, kind: ferrous_core::library::Kind, name: String) {
        match kind {
            ferrous_core::library::Kind::Profile => {
                let s = self.library.load_profile(&name);
                match self.engine.apply_settings(s.clone()) {
                    Ok(()) => {
                        self.settings = s;
                        self.layout = crate::layout::Layout::load_migrated(&mut self.settings.ui);
                        self.active.profile = name;
                    }
                    Err(e) => {
                        self.toast(format!("Can't switch profile now: {e}"), true);
                        return;
                    }
                }
            }
            ferrous_core::library::Kind::Collection => {
                self.scenes = self.library.load_collection(&name);
                self.saved_scenes = self.scenes.clone();
                self.history.reset(&self.scenes);
                self.active.collection = name;
                self.selected = None;
                self.studio_scene = self.scenes.current;
            }
        }
        let _ = self.library.set_active(&self.active);
    }

    /// Asks for per-session server URLs and keys (e.g. TikTok LIVE) before going live.
    fn session_prompt_window(&mut self, ctx: &egui::Context) {
        let Some(entries) = &mut self.session_prompt else { return };
        let mut go = false;
        let mut cancel = false;
        egui::Window::new("Go live").collapsible(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).default_width(460.0).show(ctx, |ui| {
            ui.label("These destinations issue a new server URL and stream key for every LIVE session. Paste this session's details (they aren't saved).");
            ui.label(RichText::new("TikTok: open LIVE Studio or LIVE Center on tiktok.com, set up your LIVE, then copy the Server URL and Stream Key.").small().color(theme::muted()));
            for (name, server, key) in entries.iter_mut() {
                ui.add_space(6.0);
                ui.label(RichText::new(name.as_str()).strong());
                ui.add(egui::TextEdit::singleline(server).hint_text("Server URL (rtmp://…)").desired_width(f32::INFINITY));
                ui.add(egui::TextEdit::singleline(key).hint_text("Stream key").password(true).desired_width(f32::INFINITY));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let ready = entries.iter().all(|(_, s, k)| !s.trim().is_empty() && !k.trim().is_empty());
                if ui.add_enabled(ready, egui::Button::new(RichText::new("Go Live").strong()).fill(theme::live())).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if go {
            let keys: std::collections::HashMap<String, (String, String)> =
                entries.iter().map(|(n, s, k)| (n.clone(), (s.trim().to_string(), k.trim().to_string()))).collect();
            self.session_prompt = None;
            match self.engine.start_streaming_with(&keys) {
                Ok(()) => self.toast("Going live…", false),
                Err(e) => self.toast(format!("Couldn't start stream: {e}"), true),
            }
        } else if cancel {
            self.session_prompt = None;
        }
    }

    fn import_report_window(&mut self, ctx: &egui::Context) {
        let Some((name, r)) = &self.import_report else { return };
        let mut open = true;
        egui::Window::new("Imported from OBS").open(&mut open).default_width(460.0).show(ctx, |ui| {
            ui.label(format!("Created scene collection \"{name}\": {} scenes, {} sources, {} filters.", r.scenes, r.items, r.filters));
            if r.skipped.is_empty() {
                ui.colored_label(theme::ok(), "Everything was imported.");
            } else {
                ui.label(RichText::new("Needs attention:").strong());
                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    for s in &r.skipped {
                        ui.label(RichText::new(format!("• {s}")).small());
                    }
                });
            }
        });
        if !open {
            self.import_report = None;
        }
    }

    fn dock_contents(&mut self, ui: &mut egui::Ui, d: crate::docks::Dock) {
        use crate::docks::Dock;
        match d {
            Dock::Scenes => self.scenes_dock(ui),
            Dock::Sources => self.sources_dock(ui),
            Dock::Controls => self.controls_dock(ui),
            Dock::Mixer => self.mixer_dock(ui),
            Dock::Preview | Dock::Vertical => self.canvas_dock(ui, d),
            Dock::Custom(id) => self.custom_dock(ui, id),
            Dock::Properties => {
                if self.selected.is_none() {
                    theme::card().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        widgets::dock_title(ui, "Properties");
                        ui.label(RichText::new("Select a source to edit it.").small().color(theme::muted()));
                    });
                }
                self.properties_dock(ui)
            }
            Dock::Stats => {
                let status = self.engine.status();
                let sys = self.engine.system();
                let destinations = self.engine.destination_status();
                let mixer = self.engine.mixer();
                let input = crate::docks::StatsInput {
                    status: &status,
                    sys: &sys,
                    destinations: &destinations,
                    record_kbps_setting: match self.settings.effective_record_encoder().rate_control {
                        ferrous_core::RateControl::Cbr | ferrous_core::RateControl::Vbr => self.settings.effective_record_encoder().bitrate_kbps,
                        _ => 40_000,
                    },
                    replay_active: self.engine.is_replay_active(),
                    channels: mixer.channels().iter().map(|c| (c.name.lock().clone(), c.buffered_ms(), c.active())).collect(),
                    links: self.engine.link_stats(),
                    remote_clients: self.remote.as_ref().map(|r| r.server.client_count()),
                    fps_target: self.settings.video.fps,
                };
                crate::docks::stats_dock(ui, &input, &mut self.settings.ui, &mut self.smooth);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            }
            Dock::Performance => {
                let perf = self.engine.perf();
                let only_current = self.settings.ui.perf_current_scene_only;
                let program = self.scenes.current;
                let mut rows = Vec::new();
                for (si, sc) in self.scenes.scenes.iter().enumerate() {
                    if only_current && si != program && si != self.edit_index() {
                        continue;
                    }
                    for it in &sc.items {
                        if let Some(p) = perf.get(&it.id) {
                            rows.push(crate::docks::PerfRow { id: it.id, scene: sc.name.clone(), name: it.name.clone(), kind: it.source.kind_label(), perf: p.clone() });
                        }
                    }
                }
                let sys = self.engine.system();
                let outputs = self.engine.output_pids();
                let fps = self.engine.status().fps.max(1.0);
                crate::docks::perf_dock(ui, &mut self.perf_state, &rows, &outputs, &sys, fps, &mut self.settings.ui, &mut self.smooth);
            }
        }
    }

    /// Runs an action from a button, hotkey or the remote API.
    pub fn perform(&mut self, action: HotkeyAction) {
        match action {
            HotkeyAction::ToggleStreaming => {
                if self.engine.is_streaming() {
                    self.engine.stop_streaming();
                } else if self.settings.destinations.iter().any(|d| d.enabled && d.ask_each_time) {
                    self.session_prompt = Some(
                        self.settings.destinations.iter().filter(|d| d.enabled && d.ask_each_time)
                            .map(|d| (d.name.clone(), String::new(), String::new())).collect(),
                    );
                } else if self.settings.stream.key.trim().is_empty() && self.settings.stream.server.starts_with("rtmp") {
                    self.toast("Add your stream key in Settings > Stream first.", true);
                    self.settings_win.open_with(&self.settings, &self.scenes);
                } else if let Err(e) = self.engine.start_streaming() {
                    self.toast(format!("Couldn't start stream: {e}"), true);
                } else {
                    self.toast("Stream starting…", false);
                }
            }
            HotkeyAction::ToggleRecording => {
                if self.engine.is_recording() {
                    self.engine.stop_recording();
                } else {
                    match self.engine.start_recording() {
                        Ok(p) => self.toast(format!("Recording to {}", p.display()), false),
                        Err(e) => self.toast(format!("Couldn't start recording: {e}"), true),
                    }
                }
            }
            HotkeyAction::ToggleReplayBuffer => {
                if self.engine.is_replay_active() {
                    self.engine.stop_replay();
                } else if let Err(e) = self.engine.start_replay() {
                    self.toast(format!("Couldn't start replay buffer: {e}"), true);
                }
            }
            HotkeyAction::SaveReplay => {
                if let Err(e) = self.engine.save_replay() {
                    self.toast(e.to_string(), true);
                }
            }
            HotkeyAction::SwitchScene { scene } => {
                if let Some(i) = self.scenes.scenes.iter().position(|s| s.name == scene) {
                    if self.studio_mode {
                        self.studio_scene = i;
                    } else {
                        self.scenes.current = i;
                    }
                }
            }
            HotkeyAction::Transition => {
                if self.studio_mode {
                    self.scenes.current = self.studio_scene.min(self.scenes.scenes.len() - 1);
                }
            }
            HotkeyAction::ToggleSourceVisibility { scene, source } => {
                if let Some(sc) = self.scenes.scenes.iter_mut().find(|s| s.name == scene) {
                    if let Some(it) = sc.items.iter_mut().find(|i| i.name == source) {
                        it.visible = !it.visible;
                    }
                }
            }
            HotkeyAction::ToggleMute { channel } => {
                if let Some(ch) = self.engine.mixer().channels().into_iter().find(|c| *c.name.lock() == channel) {
                    ch.set_muted(!ch.muted());
                }
            }
        }
    }

    /// +1 = toward the top of the list (drawn later / on top).
    /// Carries out a choice from a source's right-click menu.
    /// `vert`: chosen on the vertical canvas, where visibility and the
    /// transform commands apply to that canvas's own layout.
    fn source_action(&mut self, id: u64, action: SourceAction, vert: bool) {
        self.selected = Some(id);
        let (cw, ch) = if vert { self.vertical_size().unwrap_or_else(|| self.canvas_size()) } else { self.canvas_size() };
        let size = self.engine.source_info(id).map(|(s, _)| s);
        match action {
            SourceAction::Properties => self.props_win = Some(id),
            SourceAction::Filters => self.filters_win = Some(id),
            SourceAction::Rename => {
                let name = self.item_mut(id).map(|i| i.name.clone()).unwrap_or_default();
                self.rename = Some(Rename::Item(id, name));
            }
            SourceAction::Duplicate => {
                let scene = self.edit_index();
                if let Some(new) = self.scenes.duplicate_item(scene, id) {
                    self.selected = Some(new);
                    self.toast("Duplicated as a clone of the same source (no second capture).".to_string(), false);
                }
            }
            SourceAction::Remove => {
                let name = self.item_mut(id).map(|i| i.name.clone()).unwrap_or_default();
                self.edit_scene_mut().items.retain(|i| i.id != id);
                self.selected = None;
                self.toast_undo(format!("Removed “{name}”"));
                if self.props_win == Some(id) {
                    self.props_win = None;
                }
                if self.filters_win == Some(id) {
                    self.filters_win = None;
                }
            }
            SourceAction::MoveUp => self.move_selected(1),
            SourceAction::MoveDown => self.move_selected(-1),
            SourceAction::MoveTop | SourceAction::MoveBottom => {
                let items = &mut self.edit_scene_mut().items;
                if let Some(i) = items.iter().position(|it| it.id == id) {
                    let it = items.remove(i);
                    // Items are drawn bottom-up: the top of the list is the end.
                    if action == SourceAction::MoveTop { items.push(it) } else { items.insert(0, it) }
                }
            }
            SourceAction::ToggleVisible if vert => {
                if let Some((w, h)) = self.vertical_size() {
                    let (mw, mh) = self.canvas_size();
                    if let Some(p) = self.edit_scene_mut().vertical_placement_mut(id, (w as u32, h as u32), (mw as u32, mh as u32)) {
                        p.visible = !p.visible;
                    }
                }
            }
            SourceAction::ToggleVisible => {
                if let Some(i) = self.item_mut(id) {
                    i.visible = !i.visible;
                }
            }
            SourceAction::ToggleLock => {
                if let Some(i) = self.item_mut(id) {
                    i.locked = !i.locked;
                }
            }
            SourceAction::ToggleKeepRunning => {
                if let Some(i) = self.item_mut(id) {
                    i.keep_running = !i.keep_running;
                }
            }
            SourceAction::RotateCw | SourceAction::RotateCcw => {
                let step = if action == SourceAction::RotateCw { 90.0 } else { -90.0 };
                self.set_layout(vert, id, size.unwrap_or((0, 0)), |t| t.rotation = (t.rotation + step).rem_euclid(360.0));
            }
            SourceAction::ResetCrop => self.set_layout(vert, id, size.unwrap_or((0, 0)), |t| t.crop = [0; 4]),
            SourceAction::ResetTransform => self.set_layout(vert, id, size.unwrap_or((0, 0)), |t| *t = Transform::default()),
            SourceAction::FitToScreen | SourceAction::StretchToScreen | SourceAction::CenterOnScreen => {
                let Some((w, h)) = size.filter(|(w, _)| *w > 0) else { return };
                self.set_layout(vert, id, (w, h), |t| match action {
                    SourceAction::FitToScreen => *t = fit_rotated(t, w, h, cw, ch),
                    SourceAction::StretchToScreen => {
                        t.x = 0.0;
                        t.y = 0.0;
                        t.scale_x = cw / w.saturating_sub(t.crop[0] + t.crop[2]).max(1) as f32;
                        t.scale_y = ch / h.saturating_sub(t.crop[1] + t.crop[3]).max(1) as f32;
                    }
                    _ => {
                        let (ow, oh) = t.output_size(w, h);
                        t.x = (cw - ow) / 2.0;
                        t.y = (ch - oh) / 2.0;
                    }
                });
            }
        }
    }

    /// The Properties and Filters windows opened from the right-click menu.
    fn source_windows(&mut self, ctx: &egui::Context) {
        for (filters, slot) in [(false, self.props_win), (true, self.filters_win)] {
            let Some(id) = slot else { continue };
            let Some(name) = self.item_mut(id).map(|i| i.name.clone()) else {
                if filters { self.filters_win = None } else { self.props_win = None }
                continue;
            };
            let title = if filters { format!("Filters for '{name}'") } else { format!("Properties for '{name}'") };
            let mut open = true;
            let mut done = false;
            egui::Window::new(title)
                .id(egui::Id::new(("source_window", filters)))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(440.0)
                .show(ctx, |ui| {
                    // Sized to its contents; scrolls only when very tall.
                    let max_h = ctx.content_rect().height() * 0.75;
                    egui::ScrollArea::vertical().max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                        ui.set_width(440.0);
                        self.properties_contents(ui, id, if filters { PropsPart::Filters } else { PropsPart::Settings });
                    });
                    ui.add_space(6.0);
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        done = ui.button("Close").clicked();
                    });
                });
            if !open || done {
                if filters { self.filters_win = None } else { self.props_win = None }
            }
        }
    }

    fn move_selected(&mut self, dir: i32) {
        let Some(id) = self.selected else { return };
        let items = &mut self.edit_scene_mut().items;
        if let Some(i) = items.iter().position(|it| it.id == id) {
            let j = i as i32 + dir;
            if j >= 0 && (j as usize) < items.len() {
                items.swap(i, j as usize);
            }
        }
    }

    // ------------------------------------------------------ right: controls

    fn controls_dock(&mut self, ui: &mut egui::Ui) {
        if self.settings.ui.simple_mode {
            return self.simple_controls(ui);
        }
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::dock_title(ui, "Controls");
            let streaming = self.engine.is_streaming();
            if widgets::action_button(ui, if streaming { "Stop Streaming" } else { "Start Streaming" }, streaming).clicked() {
                self.request_start(crate::preflight::Kind::Stream);
            }
            let recording = self.engine.is_recording();
            if widgets::action_button(ui, if recording { "Stop Recording" } else { "Start Recording" }, recording).clicked() {
                self.request_start(crate::preflight::Kind::Record);
            }
            self.replay_and_vcam(ui);
            let label = if self.studio_mode { "Exit Studio Mode" } else { "Studio Mode" };
            if ui.add_sized([ui.available_width(), 28.0], egui::Button::selectable(self.studio_mode, label)).on_hover_text("Edit the next scene privately in a second preview, then push it live with Transition.").clicked() {
                self.studio_mode = !self.studio_mode;
                self.studio_scene = self.scenes.current;
                self.selected = None;
            }
            ui.horizontal(|ui| {
                if ui.button("⚙ Settings").clicked() {
                    self.settings_win.open_with(&self.settings, &self.scenes);
                }
                if ui.button("📁 Recordings").clicked() {
                    open_folder(&self.settings.record.folder);
                }
            });
        });
    }

    /// The Simple layout's controls: two big buttons and little else.
    fn simple_controls(&mut self, ui: &mut egui::Ui) {
        use crate::preflight::Kind as Start;
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::dock_title(ui, "Controls");
            let big = |ui: &mut egui::Ui, text: &str, active: bool| -> egui::Response {
                let base = if active { theme::live() } else { theme::accent().linear_multiply(0.75) };
                ui.add_sized([ui.available_width(), 52.0], egui::Button::new(RichText::new(text).size(18.0).strong().color(Color32::WHITE)).fill(base).corner_radius(10))
            };
            let recording = self.engine.is_recording();
            if big(ui, if recording { "■  Stop Recording" } else { "●  Start Recording" }, recording).on_hover_text("Saves a video of what's in the preview.").clicked() {
                self.request_start(Start::Record);
            }
            let st = self.engine.status();
            let hint = if recording { format!("Recording… {}", fmt_duration(st.recording.elapsed)) } else { format!("Saves to {}", self.settings.record.folder.display()) };
            ui.label(RichText::new(hint).small().color(theme::muted()));
            ui.add_space(6.0);
            let streaming = self.engine.is_streaming();
            if big(ui, if streaming { "■  Stop Streaming" } else { "📡  Go Live" }, streaming).on_hover_text("Streams the preview to your streaming service.").clicked() {
                self.request_start(Start::Stream);
            }
            let hint = if streaming { format!("Live… {}", fmt_duration(st.streaming.elapsed)) } else { format!("To {}", self.settings.stream.service) };
            ui.label(RichText::new(hint).small().color(theme::muted()));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("⚙ Settings").clicked() {
                    self.settings_win.open_with(&self.settings, &self.scenes);
                }
                if ui.button("📁 Recordings").clicked() {
                    open_folder(&self.settings.record.folder);
                }
            });
            egui::CollapsingHeader::new(RichText::new("More").small().color(theme::muted())).id_salt("simple_more").show(ui, |ui| {
                self.replay_and_vcam(ui);
            });
        });
    }

    /// Instant replay and the virtual camera.
    fn replay_and_vcam(&mut self, ui: &mut egui::Ui) {
        let replay = self.engine.is_replay_active();
        // Side by side when both labels fit, stacked full-width when the
        // dock is narrow (a fixed minimum width ran past the dock edge).
        let label = if replay { "Stop Replay Buffer" } else { "Start Replay Buffer" };
        let (mut toggle, mut save) = (false, false);
        let full = ui.available_width();
        if full >= 300.0 {
            ui.horizontal(|ui| {
                let w = (full - ui.spacing().item_spacing.x) / 2.0;
                toggle = ui.add_sized([w, 28.0], egui::Button::selectable(replay, label)).on_hover_text("Keeps the last few seconds in memory so you can save a replay after something happens.").clicked();
                save = ui.add_enabled_ui(replay, |ui| ui.add_sized([w, 28.0], egui::Button::new("💾 Save Replay")).on_hover_text("Saves the last stretch of the replay buffer to a file. Available while the buffer is running.")).inner.clicked();
            });
        } else {
            toggle = ui.add_sized([full, 28.0], egui::Button::selectable(replay, label)).on_hover_text("Keeps the last few seconds in memory so you can save a replay after something happens.").clicked();
            save = ui.add_enabled_ui(replay, |ui| ui.add_sized([full, 28.0], egui::Button::new("💾 Save Replay")).on_hover_text("Saves the last stretch of the replay buffer to a file. Available while the buffer is running.")).inner.clicked();
        }
        if toggle {
            self.perform(HotkeyAction::ToggleReplayBuffer);
        }
        if save {
            self.perform(HotkeyAction::SaveReplay);
        }
        let vcam = self.engine.virtual_camera_status();
        let label = match vcam {
            None => "Start Virtual Camera".to_string(),
            Some((true, _)) => "Stop Virtual Camera  ·  in use".to_string(),
            Some((false, _)) => "Stop Virtual Camera".to_string(),
        };
        let r = ui.add_sized([ui.available_width(), 28.0], egui::Button::selectable(vcam.is_some(), label)).on_hover_text(
            "\"Ferrous Virtual Camera\" (1080p30) for Zoom, Teams, Discord, browsers… Needs Windows 11 and the camera installed once (Tools → Install Virtual Camera).",
        );
        if r.clicked() {
            if vcam.is_some() {
                self.engine.stop_virtual_camera();
            } else if let Err(e) = self.engine.start_virtual_camera() {
                self.toast(format!("{e:#}"), true);
            }
        }
        if vcam.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    fn mixer_dock(&mut self, ui: &mut egui::Ui) {
        use crate::mixer;
        let mixer_handle = self.engine.mixer();
        let channels: Vec<_> = mixer_handle
            .channels()
            .into_iter()
            .filter(|ch| ch.id == "desktop" || ch.id == "mic" || ch.active())
            .collect();
        // Advance every strip's ballistics once per frame.
        let mut animating = false;
        for ch in &channels {
            let st = self.strips.entry(ch.id.clone()).or_default();
            st.tick(ch.levels.take(), ch.volume());
            animating |= st.animating(ch.volume());
        }
        self.strips.retain(|id, _| channels.iter().any(|c| &c.id == id));
        if animating {
            // ~60 fps while meters or faders move; an unthrottled repaint
            // spins as fast as the GPU allows when vsync is off.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(16));
        } else {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        let vertical = self.settings.ui.mixer_vertical;
        let mut set_volume: Vec<(String, f32)> = Vec::new();
        let mut open_props = None;
        let mut toggle_layout = false;
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::dock_title(ui, "Audio Mixer");
            let strip_menu = |ui: &mut egui::Ui, ch: &ferrous_audio::AudioChannel, set_volume: &mut Vec<(String, f32)>, open_props: &mut Option<String>, toggle_layout: &mut bool| {
                if ui.button(if ch.muted() { "Unmute" } else { "Mute" }).clicked() {
                    ch.set_muted(!ch.muted());
                    ui.close();
                }
                if ui.button("Reset volume to 0 dB").clicked() {
                    set_volume.push((ch.id.clone(), 1.0));
                    ui.close();
                }
                if ui.button("Filters, tracks, monitoring…").clicked() {
                    *open_props = Some(ch.id.clone());
                    ui.close();
                }
                ui.separator();
                if ui.button(if vertical { "Horizontal layout" } else { "Vertical layout" }).clicked() {
                    *toggle_layout = true;
                    ui.close();
                }
            };
            if vertical {
                egui::ScrollArea::horizontal().show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        let h = ui.available_height().max(240.0);
                        for ch in &channels {
                            let st = self.strips.get_mut(&ch.id).unwrap();
                            let (rect, resp) = ui.allocate_exact_size(Vec2::new(86.0, h), egui::Sense::click());
                            resp.context_menu(|ui| strip_menu(ui, ch, &mut set_volume, &mut open_props, &mut toggle_layout));
                            let mute_t = ui.ctx().animate_bool_with_time(egui::Id::new(("mute", &ch.id)), ch.muted(), 0.2);
                            let p = ui.painter_at(rect);
                            p.rect_filled(rect.shrink(1.0), 6, theme::surface().linear_multiply(0.35));
                            let name = ch.name.lock().clone();
                            p.text(rect.center_top() + Vec2::new(0.0, 10.0), Align2::CENTER_CENTER, &name, egui::FontId::proportional(11.5), theme::text());
                            p.text(rect.center_top() + Vec2::new(0.0, 25.0), Align2::CENTER_CENTER, mixer::fmt_db(ch.volume()), egui::FontId::proportional(9.5), if ch.active() { theme::muted() } else { theme::muted().linear_multiply(0.5) });
                            let body = Rect::from_min_max(rect.min + Vec2::new(8.0, 36.0), rect.max - Vec2::new(8.0, 34.0));
                            let meter_rect = Rect::from_min_size(body.min, Vec2::new(34.0, body.height()));
                            let fader_rect = Rect::from_min_max(Pos2::new(meter_rect.right() + 6.0, body.top() + 9.0), Pos2::new(body.right(), body.bottom()));
                            mixer::meter(ui, meter_rect, st, true, mute_t, ch.active());
                            let f = mixer::fader(ui, fader_rect, egui::Id::new(("fader", &ch.id)), st, ch.volume(), true, mute_t);
                            if let Some(g) = f.new_gain {
                                set_volume.push((ch.id.clone(), g));
                            }
                            f.response.context_menu(|ui| strip_menu(ui, ch, &mut set_volume, &mut open_props, &mut toggle_layout));
                            let btn_row = Rect::from_min_max(Pos2::new(rect.left() + 6.0, rect.bottom() - 30.0), Pos2::new(rect.right() - 6.0, rect.bottom() - 6.0));
                            ui.scope_builder(egui::UiBuilder::new().max_rect(btn_row), |ui| {
                                ui.horizontal_centered(|ui| {
                                    if mute_button(ui, ch.muted(), mute_t).clicked() {
                                        ch.set_muted(!ch.muted());
                                    }
                                    if ui.small_button("⚙").on_hover_text("Filters, tracks, monitoring").clicked() {
                                        open_props = Some(ch.id.clone());
                                    }
                                });
                            });
                            ui.add_space(4.0);
                        }
                    });
                });
            } else {
                for ch in &channels {
                    let st = self.strips.get_mut(&ch.id).unwrap();
                    ui.push_id(&ch.id, |ui| {
                        let mute_t = ui.ctx().animate_bool_with_time(egui::Id::new(("mute", &ch.id)), ch.muted(), 0.2);
                        let head = ui.horizontal(|ui| {
                            if mute_button(ui, ch.muted(), mute_t).clicked() {
                                ch.set_muted(!ch.muted());
                            }
                            // Right-hand items first, then the name gets what's
                            // left (truncated), so nothing overprints when narrow.
                            let name = ch.name.lock().clone();
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button("⚙").on_hover_text("Filters, tracks, monitoring").clicked() {
                                    open_props = Some(ch.id.clone());
                                }
                                ui.label(RichText::new(mixer::fmt_db(ch.volume())).small().color(theme::muted()));
                                let name_w = ui.fonts_mut(|f| f.layout_no_wrap(name.clone(), egui::FontId::proportional(14.0), theme::text()).size().x);
                                if !ch.active() && ui.available_width() > name_w + 70.0 {
                                    ui.label(RichText::new("no device").small().color(theme::muted().linear_multiply(0.7)));
                                }
                                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                    let r = ui.add(egui::Label::new(RichText::new(&name).strong()).truncate());
                                    if !ch.active() {
                                        r.on_hover_text(format!("{name}: no device"));
                                    }
                                });
                            });
                        });
                        let head_resp = ui.interact(head.response.rect, egui::Id::new(("strip_head", &ch.id)), egui::Sense::click());
                        head_resp.context_menu(|ui| strip_menu(ui, ch, &mut set_volume, &mut open_props, &mut toggle_layout));
                        let w = ui.available_width();
                        let (meter_rect, _) = ui.allocate_exact_size(Vec2::new(w, 26.0), egui::Sense::hover());
                        mixer::meter(ui, meter_rect, st, false, mute_t, ch.active());
                        let (fader_rect, _) = ui.allocate_exact_size(Vec2::new(w - 10.0, 20.0), egui::Sense::hover());
                        let f = mixer::fader(ui, fader_rect, egui::Id::new(("fader", &ch.id)), st, ch.volume(), false, mute_t);
                        if let Some(g) = f.new_gain {
                            set_volume.push((ch.id.clone(), g));
                        }
                        f.response.context_menu(|ui| strip_menu(ui, ch, &mut set_volume, &mut open_props, &mut toggle_layout));
                        ui.add_space(8.0);
                    });
                }
            }
        });
        for (id, g) in set_volume {
            if let Some(ch) = channels.iter().find(|c| c.id == id) {
                ch.set_volume(g);
            }
            match id.as_str() {
                "desktop" => self.settings.audio.desktop_volume = g,
                "mic" => self.settings.audio.mic_volume = g,
                _ => {}
            }
        }
        if let Some(id) = open_props {
            self.audio_props = Some(id);
        }
        if toggle_layout {
            self.settings.ui.mixer_vertical = !vertical;
        }
    }

    fn properties_dock(&mut self, ui: &mut egui::Ui) {
        if let Some(id) = self.selected {
            self.properties_contents(ui, id, PropsPart::All);
        }
    }

    /// A source's properties: its settings and transform, its filters, or
    /// both (the Properties dock). The Properties and Filters windows opened
    /// from a source's right-click menu show one part each, as in OBS.
    fn properties_contents(&mut self, ui: &mut egui::Ui, id: u64, part: PropsPart) {
        let info = self.engine.source_info(id);
        let mut refresh_monitors = false;
        let mut refresh_windows = false;
        let mut refresh_devices = false;
        let mut refresh_apps = false;
        let audio_apps = self.audio_apps.clone();
        let scene_names: Vec<String> = {
            let own = self.edit_scene().name.clone();
            self.scenes.scenes.iter().map(|s| s.name.clone()).filter(|n| *n != own).collect()
        };
        let clone_targets: Vec<(u64, String)> = self.clonable_items().into_iter().filter(|(i, _)| *i != id).collect();
        let video_devices = self.video_devices.clone();
        let dshow_audio = self.dshow_audio_devices.clone();
        let monitors = self.monitors.clone();
        let windows = self.windows.clone();
        let live_chain = self.engine.mixer().channel(&format!("source-{id}"));
        let Some(item) = self.item_mut(id) else { return };
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            if part == PropsPart::All {
                widgets::dock_title(ui, &format!("Properties · {}", item.source.kind_label()));
            }
            if let Some((_, Some(err))) = &info {
                ui.colored_label(theme::warn(), err);
            }
            if part != PropsPart::Filters {
                match &mut item.source {
                    SourceConfig::DisplayCapture { monitor_index, capture_cursor } => {
                        ui.horizontal(|ui| {
                            let text = monitors
                                .get(*monitor_index)
                                .map(|m| format!("{} ({}×{})", m.name, m.width, m.height))
                                .unwrap_or_else(|| format!("Display {}", *monitor_index + 1));
                            egui::ComboBox::from_id_salt("monitor").selected_text(text).width(200.0).show_ui(ui, |ui| {
                                for m in &monitors {
                                    ui.selectable_value(monitor_index, m.index, format!("{} ({}×{})", m.name, m.width, m.height));
                                }
                            });
                            if ui.small_button("⟳").clicked() {
                                refresh_monitors = true;
                            }
                        });
                        ui.checkbox(capture_cursor, "Capture cursor");
                    }
                    SourceConfig::WindowCapture { title_contains, capture_cursor } => {
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("window")
                                .selected_text(if title_contains.is_empty() { "Select a window" } else { title_contains.as_str() })
                                .width(200.0)
                                .show_ui(ui, |ui| {
                                    for w in &windows {
                                        let label = if w.process.is_empty() { w.title.clone() } else { format!("[{}] {}", w.process, w.title) };
                                        ui.selectable_value(title_contains, w.title.clone(), label);
                                    }
                                });
                            if ui.small_button("⟳").clicked() {
                                refresh_windows = true;
                            }
                        });
                        ui.label(RichText::new("Matches any window whose title contains this text:").small().color(theme::muted()));
                        ui.text_edit_singleline(title_contains);
                        ui.checkbox(capture_cursor, "Capture cursor");
                    }
                    SourceConfig::GameCapture { program, capture_cursor } => {
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("game_program")
                                .selected_text(if program.is_empty() { "Detect automatically" } else { program.as_str() })
                                .width(200.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(program, String::new(), "Detect automatically");
                                    let mut seen = std::collections::BTreeSet::new();
                                    for w in &windows {
                                        if !w.process.is_empty() && seen.insert(w.process.clone()) {
                                            ui.selectable_value(program, w.process.clone(), &w.process);
                                        }
                                    }
                                });
                            if ui.small_button("⟳").clicked() {
                                refresh_windows = true;
                            }
                        });
                        ui.label(RichText::new("Automatic follows the full-screen or borderless game in front, and picks it up again when it restarts. Nothing is injected into the game, so anti-cheat is fine with it.").small().color(theme::muted()));
                        ui.checkbox(capture_cursor, "Capture cursor");
                    }
                    SourceConfig::Image { path } => {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(path.display().to_string()).small());
                            if ui.small_button("Browse…").clicked() {
                                if let Some(p) = rfd::FileDialog::new().add_filter("Images", &["png", "jpg", "jpeg", "bmp", "gif", "webp"]).pick_file() {
                                    *path = p;
                                }
                            }
                        });
                    }
                    SourceConfig::Color { bgra, width, height } => {
                        let mut rgba = [bgra[2], bgra[1], bgra[0], bgra[3]];
                        ui.horizontal(|ui| {
                            widgets::row_label(ui, "Color");
                            if ui.color_edit_button_srgba_unmultiplied(&mut rgba).changed() {
                                *bgra = [rgba[2], rgba[1], rgba[0], rgba[3]];
                            }
                            widgets::row_label(ui, "Size");
                            ui.add(egui::DragValue::new(width).range(1..=7680));
                            ui.add(egui::DragValue::new(height).range(1..=4320));
                        });
                    }
                    SourceConfig::VideoDevice { device, resolution, fps, audio_device } => {
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("vdev")
                                .selected_text(if device.is_empty() { "Select a device" } else { device.as_str() })
                                .width(200.0)
                                .show_ui(ui, |ui| {
                                    for d in &video_devices {
                                        ui.selectable_value(device, d.clone(), d);
                                    }
                                });
                            if ui.small_button("⟳").clicked() {
                                refresh_devices = true;
                            }
                        });
                        let mut custom = resolution.is_some();
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut custom, "Resolution").changed() {
                                *resolution = custom.then_some([1920, 1080]);
                            }
                            if let Some([w, h]) = resolution {
                                ui.add(egui::DragValue::new(w).range(16..=7680));
                                ui.add(egui::DragValue::new(h).range(16..=4320));
                            }
                        });
                        let mut custom_fps = fps.is_some();
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut custom_fps, "FPS").changed() {
                                *fps = custom_fps.then_some(30);
                            }
                            if let Some(f) = fps {
                                ui.add(egui::DragValue::new(f).range(1..=240));
                            }
                        });
                        egui::ComboBox::from_id_salt("adev")
                            .selected_text(audio_device.clone().unwrap_or_else(|| "No device audio".into()))
                            .width(240.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(audio_device, None, "No device audio");
                                for d in &dshow_audio {
                                    ui.selectable_value(audio_device, Some(d.clone()), d);
                                }
                            });
                    }
                    SourceConfig::Text { text, from_file, font, size, bgra, outline, background, align, wrap_width } => {
                        let mut use_file = from_file.is_some();
                        if widgets::tip(ui.checkbox(&mut use_file, "Read from file"), "Read from file").changed() {
                            *from_file = if use_file { rfd::FileDialog::new().add_filter("Text", &["txt"]).pick_file() } else { None };
                        }
                        match from_file {
                            Some(p) => {
                                ui.label(RichText::new(p.display().to_string()).small());
                            }
                            None => {
                                ui.add(egui::TextEdit::multiline(text).desired_rows(3).desired_width(f32::INFINITY));
                            }
                        }
                        ui.horizontal(|ui| {
                            widgets::row_label(ui, "Font");
                            ui.add(egui::TextEdit::singleline(font).hint_text("default (Segoe UI)").desired_width(130.0));
                            if ui.small_button("…").clicked() {
                                if let Some(p) = rfd::FileDialog::new().add_filter("Fonts", &["ttf", "otf", "ttc"]).pick_file() {
                                    *font = p.display().to_string();
                                }
                            }
                            ui.add(egui::DragValue::new(size).range(4.0..=512.0).suffix(" px"));
                        });
                        ui.horizontal(|ui| {
                            let mut rgba = [bgra[2], bgra[1], bgra[0], bgra[3]];
                            widgets::row_label(ui, "Color");
                            if ui.color_edit_button_srgba_unmultiplied(&mut rgba).changed() {
                                *bgra = [rgba[2], rgba[1], rgba[0], rgba[3]];
                            }
                            let mut bg = [background[2], background[1], background[0], background[3]];
                            widgets::row_label(ui, "Background");
                            if ui.color_edit_button_srgba_unmultiplied(&mut bg).changed() {
                                *background = [bg[2], bg[1], bg[0], bg[3]];
                            }
                        });
                        ui.horizontal(|ui| {
                            let mut oc = [outline.0[2], outline.0[1], outline.0[0], outline.0[3]];
                            widgets::row_label(ui, "Outline");
                            if ui.color_edit_button_srgba_unmultiplied(&mut oc).changed() {
                                outline.0 = [oc[2], oc[1], oc[0], oc[3]];
                            }
                            ui.add(egui::DragValue::new(&mut outline.1).range(0..=32).suffix(" px"));
                        });
                        ui.horizontal(|ui| {
                            for (a, l) in [(TextAlign::Left, "Left"), (TextAlign::Center, "Center"), (TextAlign::Right, "Right")] {
                                ui.selectable_value(align, a, l);
                            }
                            ui.add(egui::DragValue::new(wrap_width).range(0..=7680).prefix("wrap ").suffix(" px"));
                        });
                    }
                    SourceConfig::LinkReceiver { sender, pin } => {
                        let found = ferrous_link::discovered();
                        egui::ComboBox::from_id_salt("linksrc").selected_text(if sender.is_empty() { "Select a sender" } else { sender.as_str() }).width(220.0).show_ui(ui, |ui| {
                            for a in &found {
                                let label = format!("{}  ({}×{} {:?}{})", a.name, a.width, a.height, a.codec, if a.encrypted { ", PIN" } else { "" });
                                ui.selectable_value(sender, a.name.clone(), label);
                            }
                            if found.is_empty() {
                                ui.label(RichText::new("No senders found yet…").small().color(theme::muted()));
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("or address");
                            ui.add(egui::TextEdit::singleline(sender).hint_text("192.168.1.20:47778").desired_width(150.0));
                        });
                        ui.horizontal(|ui| {
                            ui.label("PIN");
                            ui.add(egui::TextEdit::singleline(pin).desired_width(80.0).password(true));
                        });
                        if let Some(st) = ferrous_link::receiver::stats_for(sender) {
                            use std::sync::atomic::Ordering::Relaxed;
                            if st.connected.load(Relaxed) {
                                ui.colored_label(theme::ok(), format!(
                                    "● Connected to {} ({}): {} fps, {:.1} Mbps, ~{} ms latency, {} ms RTT",
                                    st.sender_name.lock(), st.codec.lock(), st.fps.load(Relaxed),
                                    st.kbps.load(Relaxed) as f32 / 1000.0, st.latency_ms.load(Relaxed), st.rtt_ms.load(Relaxed)
                                ));
                            } else if let Some(e) = st.error.lock().clone() {
                                ui.colored_label(theme::warn(), e);
                            }
                        }
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
                    }
                    SourceConfig::ObsPlugin { type_id, settings, .. } => crate::obs_ui::properties(ui, id, type_id, settings),
                    SourceConfig::Plugin { plugin, settings } => {
                        ui.label(RichText::new(plugin.as_str()).small().color(theme::muted()));
                        ui.add(egui::TextEdit::multiline(settings).desired_rows(3).desired_width(f32::INFINITY).hint_text("{ } settings JSON"));
                    }
                    SourceConfig::Clone { source } => {
                        let shown = clone_targets.iter().find(|(i, _)| i == source).map(|(_, n)| n.clone());
                        egui::ComboBox::from_id_salt("clone_of").selected_text(shown.clone().unwrap_or_else(|| "Original missing".into())).width(240.0).show_ui(ui, |ui| {
                            for (i, n) in &clone_targets {
                                ui.selectable_value(source, *i, n);
                            }
                        });
                        if shown.is_none() {
                            ui.colored_label(theme::warn(), "The original was removed. Pick another source.");
                        }
                        ui.label(RichText::new("Shows the original's live picture. Its own filters aren't copied; add this item's own below.").small().color(theme::muted()));
                    }
                    SourceConfig::Scene { name } => {
                        egui::ComboBox::from_id_salt("nested").selected_text(name.clone()).width(200.0).show_ui(ui, |ui| {
                            for n in &scene_names {
                                ui.selectable_value(name, n.clone(), n);
                            }
                        });
                    }
                    SourceConfig::Browser { url, width, height, fps, css } => {
                        ui.add(egui::TextEdit::singleline(url).hint_text("https://…").desired_width(f32::INFINITY));
                        ui.horizontal(|ui| {
                            ui.add(egui::DragValue::new(width).range(16..=3840));
                            ui.label("×");
                            ui.add(egui::DragValue::new(height).range(16..=2160));
                            ui.add(egui::DragValue::new(fps).range(1..=60).suffix(" fps"));
                        });
                        ui.label(RichText::new("Custom CSS").small().color(theme::muted()));
                        ui.add(egui::TextEdit::multiline(css).desired_rows(2).desired_width(f32::INFINITY).hint_text("body { background: transparent; }"));
                        ui.label(RichText::new("Rendered by Microsoft Edge (headless), transparent background.").small().color(theme::muted()));
                    }
                    SourceConfig::ApplicationAudio { process } => {
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("app")
                                .selected_text(if process.is_empty() { "Select an app" } else { process.as_str() })
                                .width(200.0)
                                .show_ui(ui, |ui| {
                                    for a in &audio_apps {
                                        ui.selectable_value(process, a.clone(), a);
                                    }
                                });
                            if ui.small_button("⟳").clicked() {
                                refresh_apps = true;
                            }
                        });
                        ui.label(RichText::new("Captures just this app's sound (Windows 10 2004 or later).").small().color(theme::muted()));
                    }
                    SourceConfig::Media { input, looping, hw_decode } => {
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(input).hint_text("file path or URL").desired_width(190.0));
                            if ui.small_button("Browse…").clicked() {
                                if let Some(p) = rfd::FileDialog::new().pick_file() {
                                    *input = p.display().to_string();
                                }
                            }
                        });
                        ui.checkbox(looping, "Loop");
                        ui.checkbox(hw_decode, "Hardware decoding");
                    }
                    SourceConfig::TestPattern { width, height } => {
                        ui.horizontal(|ui| {
                            widgets::row_label(ui, "Size");
                            ui.add(egui::DragValue::new(width).range(16..=7680));
                            ui.add(egui::DragValue::new(height).range(16..=4320));
                        });
                    }
                }
                ui.separator();
                // Position, size, crop and rotation are edited on the canvas
                // (and the right-click Transform menu), as in OBS.
                ui.horizontal(|ui| {
                    widgets::row_label(ui, "Opacity");
                    ui.add(egui::Slider::new(&mut item.transform.opacity, 0.0..=1.0));
                });
                ui.label(
                    RichText::new("Move, resize and rotate this source on the canvas: drag it, drag a handle (Shift: free aspect), Alt + drag a handle to crop. Right-click it for Fit, Center and Rotate.")
                        .small()
                        .color(theme::muted()),
                );
            }
            if part != PropsPart::Settings {
                egui::CollapsingHeader::new(format!("Filters ({})", item.video_filters.len()))
                    .id_salt("vfilters")
                    .show(ui, |ui| crate::filters_ui::video_filters(ui, "vf", &mut item.video_filters));
                if matches!(item.source, SourceConfig::Media { .. } | SourceConfig::VideoDevice { .. } | SourceConfig::ApplicationAudio { .. } | SourceConfig::LinkReceiver { .. }) {
                    egui::CollapsingHeader::new(format!("Audio filters ({})", item.audio_filters.len()))
                        .id_salt("afilters")
                        .show(ui, |ui| crate::filters_ui::audio_filters(ui, "af", &mut item.audio_filters, live_chain.as_ref().map(|c| &c.filters)));
                }
            }
        });
        if refresh_monitors {
            self.monitors = ferrous_capture::list_monitors();
        }
        if refresh_devices {
            self.refresh_devices();
        }
        if refresh_apps {
            self.audio_apps = ferrous_capture::list_audio_apps();
        }
        if refresh_windows || (self.windows.is_empty() && matches!(self.item_mut(id).map(|i| &i.source), Some(SourceConfig::WindowCapture { .. } | SourceConfig::GameCapture { .. }))) {
            self.windows = ferrous_capture::list_windows();
        }
    }

    // --------------------------------------------------------------- preview

    /// The mixer owns fader and mute state (and the device filter chains are
    /// edited from the mixer too); settings copied before those changed must
    /// not roll them back.
    fn with_mixer_state(&self, mut new: Settings) -> Settings {
        let (a, b) = (&self.settings.audio, &mut new.audio);
        (b.desktop_volume, b.mic_volume, b.desktop_muted, b.mic_muted) = (a.desktop_volume, a.mic_volume, a.desktop_muted, a.mic_muted);
        b.desktop_filters = a.desktop_filters.clone();
        b.mic_filters = a.mic_filters.clone();
        new
    }

    /// Take on settings from the settings window, setup or wizard, keeping
    /// the live dock layout (those windows edit a copy taken earlier).
    fn adopt_settings(&mut self, new: Settings) {
        let vertical_was_off = self.settings.video.vertical.is_none();
        let old = std::mem::replace(&mut self.settings, new);
        crate::layout::keep_layout(&old.ui, &mut self.settings.ui);

        if vertical_was_off && self.settings.video.vertical.is_some() {
            self.layout.open(crate::layout::Dock::Vertical);
        }
    }

    /// Every card is a dock: drag a tab to snap it beside, above, below or
    /// into another dock, or out of the window to float it.
    fn dock_area(&mut self, ui: &mut egui::Ui) {
        let style = crate::layout::style(ui.style().as_ref(), &self.settings.ui);
        let locked = self.settings.ui.lock_layout || self.settings.ui.simple_mode;
        let mates = self.layout.mates();
        let mut tree = std::mem::replace(&mut self.layout.tree, egui_stack::DockState::new(Vec::new()));
        let mut viewer = DockViewer { app: self, mates, closed: Vec::new(), locked };
        egui_stack::DockArea::new(&mut tree)
            .id(egui::Id::new("ember_docks"))
            .style(style)
            .draggable_tabs(!locked)
            .show_close_buttons(!locked)
            .show_leaf_close_all_buttons(false)
            .show_leaf_collapse_buttons(false)
            .show_window_collapse_buttons(false)
            .show_inside(ui, &mut viewer);
        let closed = std::mem::take(&mut viewer.closed);
        self.layout.tree = tree;
        for (d, mate) in closed {
            if let Some(m) = mate {
                self.layout.set_anchor(d, m);
            }
        }
        self.layout.ensure_preview();
        self.layout.store(&mut self.settings.ui);
    }

    /// A custom browser dock. On Windows the live page (a WebView2 child
    /// window) is placed over this rect after the frame is drawn; what's
    /// painted here shows while it loads, or while something covers it.
    fn custom_dock(&mut self, ui: &mut egui::Ui, id: u32) {
        let rect = ui.available_rect_before_wrap();
        ui.allocate_rect(rect, egui::Sense::hover());
        ui.painter().rect_filled(rect, 0.0, theme::bg());
        let Some(cfg) = self.settings.ui.custom_docks.iter().find(|c| c.id == id).cloned() else {
            ui.put(rect, egui::Label::new(RichText::new("This dock was removed.").color(theme::muted())));
            return;
        };
        let url = crate::webdock::normalize_url(&cfg.url);
        let error = self.webdocks.error(id).map(str::to_string);
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect.shrink(12.0)), |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((rect.height() / 2.0 - 50.0).max(0.0));
                if url.is_empty() {
                    ui.label(RichText::new("No address set").strong());
                    ui.label(RichText::new("View → Docks → Custom Browser Docks…").small().color(theme::muted()));
                    return;
                }
                match &error {
                    Some(e) => {
                        ui.label(RichText::new(e).small().color(theme::warn()));
                    }
                    None if cfg!(windows) => {
                        ui.label(RichText::new("Loading…").color(theme::muted()));
                    }
                    None => {
                        ui.label(RichText::new("Live pages in docks need Windows (WebView2).").small().color(theme::muted()));
                    }
                }
                ui.label(RichText::new(&url).small().monospace().color(theme::muted()));
                if (error.is_some() || cfg!(not(windows))) && ui.button("Open in browser").clicked() {
                    crate::webdock::open_external(&url);
                }
            });
        });
        self.web_frames.push(crate::webdock::WebFrame { id, url, rect: rect.intersect(ui.clip_rect()), layer: ui.layer_id() });
    }

    /// View → Docks → Custom Browser Docks…: add, rename, re-point or
    /// remove docks that show a web page.
    fn custom_docks_window(&mut self, ctx: &egui::Context) {
        let Some(draft) = self.dock_editor.as_mut() else { return };
        let mut open = true;
        let (mut apply, mut cancel) = (false, false);
        egui::Window::new("Custom Browser Docks").open(&mut open).collapsible(false).show(ctx, |ui| {
            ui.set_min_width(600.0);
            ui.label(
                RichText::new("Each one becomes a dock you can snap anywhere, like the built-in ones. Sign-ins (e.g. to chat) are remembered.")
                    .small()
                    .color(theme::muted()),
            );
            ui.add_space(6.0);
            let mut remove = None;
            egui::Grid::new("custom_docks").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
                if !draft.is_empty() {
                    ui.label(RichText::new("Dock name").small().strong());
                    ui.label(RichText::new("URL").small().strong());
                    ui.end_row();
                }
                for (i, d) in draft.iter_mut().enumerate() {
                    ui.add_sized([150.0, 24.0], egui::TextEdit::singleline(&mut d.name));
                    ui.add_sized([390.0, 24.0], egui::TextEdit::singleline(&mut d.url).hint_text("https://…"));
                    if ui.small_button("🗑").on_hover_text("Remove this dock").clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
            if let Some(i) = remove {
                draft.remove(i);
            }
            if draft.is_empty() {
                ui.label(RichText::new("No custom docks yet.").small().color(theme::muted()));
            }
            ui.add_space(4.0);
            let next_id = draft.iter().map(|d| d.id).chain(self.settings.ui.custom_docks.iter().map(|d| d.id)).max().unwrap_or(0) + 1;
            ui.horizontal(|ui| {
                if ui.button("+ Add dock").clicked() {
                    draft.push(ferrous_core::CustomDock { id: next_id, name: format!("Dock {}", draft.len() + 1), url: String::new() });
                }
                ui.menu_button("+ Common…", |ui| {
                    for (name, url, tip) in [
                        ("Twitch Chat", "https://www.twitch.tv/popout/CHANNEL/chat?darkpopout", "Replace CHANNEL with your channel name"),
                        ("Twitch Activity Feed", "https://dashboard.twitch.tv/popout/u/CHANNEL/stream-manager/activity-feed", "Replace CHANNEL with your channel name"),
                        ("YouTube Live Chat", "https://www.youtube.com/live_chat?is_popout=1&v=VIDEO_ID", "Replace VIDEO_ID with your stream's video id"),
                    ] {
                        if ui.button(name).on_hover_text(tip).clicked() {
                            draft.push(ferrous_core::CustomDock { id: next_id, name: name.into(), url: url.into() });
                            ui.close();
                        }
                    }
                });
            });
            ui.label(RichText::new("StreamElements, Streamlabs and other services give you a dock or popout URL in their dashboards.").small().color(theme::muted()));
            ui.separator();
            ui.horizontal(|ui| {
                apply = ui.button("Apply").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if apply {
            let mut new = self.dock_editor.take().unwrap_or_default();
            for d in &mut new {
                d.name = d.name.trim().to_string();
                if d.name.is_empty() {
                    d.name = format!("Dock {}", d.id);
                }
                d.url = crate::webdock::normalize_url(&d.url);
            }
            let old: Vec<u32> = self.settings.ui.custom_docks.iter().map(|d| d.id).collect();
            for id in old.iter().filter(|id| !new.iter().any(|d| d.id == **id)) {
                self.layout.close(crate::layout::Dock::Custom(*id));
            }
            for d in new.iter().filter(|d| !old.contains(&d.id)) {
                self.layout.open(crate::layout::Dock::Custom(d.id));
            }
            self.settings.ui.custom_docks = new;
            let _ = self.library.save_profile(&self.active.profile, &self.settings);
        } else if cancel || !open {
            self.dock_editor = None;
        }
    }

    fn canvas_dock(&mut self, ui: &mut egui::Ui, d: crate::layout::Dock) {
        match d {
            crate::layout::Dock::Vertical => {
                let full = ui.available_rect_before_wrap().shrink(4.0);
                let Some((vw, vh)) = self.vertical_size() else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(full.height() / 2.0 - 30.0);
                        ui.label(RichText::new("Vertical canvas is off").strong());
                        ui.label(RichText::new("A 9:16 canvas for TikTok, Shorts and Reels, alongside the main one.").small().color(theme::muted()));
                        ui.add_space(8.0);
                        if ui.button("Turn on 1080×1920").clicked() {
                            let mut s = self.settings.clone();
                            s.video.vertical = Some((1080, 1920));
                            match self.engine.apply_settings(s.clone()) {
                                Ok(()) => {
                                    self.settings = s;
                                    let _ = self.library.save_profile(&self.active.profile, &self.settings);
                                }
                                Err(e) => self.toast(e.to_string(), true),
                            }
                        }
                    });
                    return;
                };
                let scale = (full.width() / vw).min(full.height() / vh).max(0.01);
                let rect = Rect::from_center_size(full.center(), Vec2::new(vw * scale, vh * scale));
                ui.allocate_rect(full, egui::Sense::hover());
                self.canvas_pane(ui, rect, true);
            }
            _ => self.central(ui),
        }
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap().shrink(4.0);
        if !self.studio_mode {
            self.preview(ui, full);
            if self.edit_scene().items.is_empty() && self.settings.ui.preview_enabled {
                let p = ui.painter_at(full);
                p.text(full.center() - Vec2::new(0.0, 12.0), Align2::CENTER_CENTER, "This scene is empty", egui::FontId::proportional(22.0), theme::text());
                p.text(full.center() + Vec2::new(0.0, 18.0), Align2::CENTER_CENTER, "Add your screen, a game or a camera from the Sources panel", egui::FontId::proportional(14.0), theme::muted());
            }
            return;
        }
        // Studio mode: editable Preview | transition controls | live Program.
        let mid_w = 150.0;
        let half = (full.width() - mid_w) / 2.0;
        let left = Rect::from_min_size(full.min + Vec2::new(0.0, 22.0), Vec2::new(half, full.height() - 22.0));
        let mid = Rect::from_min_size(full.min + Vec2::new(half, 0.0), Vec2::new(mid_w, full.height()));
        let right = Rect::from_min_size(full.min + Vec2::new(half + mid_w, 22.0), Vec2::new(half, full.height() - 22.0));
        let p = ui.painter();
        p.text(left.center_top() - Vec2::new(0.0, 12.0), Align2::CENTER_CENTER, "PREVIEW", egui::FontId::proportional(12.0), theme::muted());
        p.text(right.center_top() - Vec2::new(0.0, 12.0), Align2::CENTER_CENTER, "PROGRAM", egui::FontId::proportional(12.0), theme::live());
        self.preview(ui, left);
        self.program_view(ui, right);
        ui.scope_builder(egui::UiBuilder::new().max_rect(mid.shrink2(Vec2::new(10.0, 0.0))), |ui| {
            ui.add_space(full.height() / 2.0 - 70.0);
            let t = &mut self.scenes.transition;
            if ui.add_sized([ui.available_width(), 34.0], egui::Button::new(RichText::new("Transition ▶").strong()).fill(theme::accent().linear_multiply(0.5))).on_hover_text("Sends the preview scene to the live output using the chosen transition.").clicked() {
                self.scenes.current = self.studio_scene.min(self.scenes.scenes.len() - 1);
            }
            ui.add_space(6.0);
            transition_editor(ui, "tkind", t);
        });
    }

    fn program_view(&mut self, ui: &mut egui::Ui, avail: Rect) {
        let (cw, ch) = self.canvas_size();
        let scale = (avail.width() / cw).min(avail.height() / ch).max(0.01);
        let rect = Rect::from_center_size(avail.center(), Vec2::new(cw * scale, ch * scale));
        let painter = ui.painter_at(rect.expand(4.0));
        painter.rect_filled(rect, 0, Color32::BLACK);
        self.note_preview_drawn(ui, rect, false);
        if !self.settings.ui.preview_enabled {
            self.preview_disabled_overlay(ui, rect);
        } else if let Some(tex) = &self.preview_tex {
            painter.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        painter.rect_stroke(rect, 0, Stroke::new(1.5, theme::live()), StrokeKind::Outside);
    }

    /// Remembers how many pixels wide a preview was drawn this frame, so the
    /// engine renders it at exactly that size (and only while it's shown).
    fn note_preview_drawn(&mut self, ui: &egui::Ui, rect: Rect, vert: bool) {
        let px = (rect.width() * ui.ctx().pixels_per_point()).round() as u32;
        let slot = if vert { &mut self.preview_px.1 } else { &mut self.preview_px.0 };
        *slot = (*slot).max(px.max(1));
        // It just became visible: apply that next frame rather than waiting
        // for the slow UI tick, since no preview frames are coming to wake us.
        if self.preview_sent.is_some_and(|c| !c.enabled) && self.settings.ui.preview_enabled {
            ui.ctx().request_repaint();
        }
    }

    fn preview_disabled_overlay(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter_at(rect);
        painter.text(rect.center() - Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, "Preview disabled", egui::FontId::proportional(16.0), theme::muted());
        let b = Rect::from_center_size(rect.center() + Vec2::new(0.0, 16.0), Vec2::new(130.0, 26.0));
        if ui.put(b, egui::Button::new("Enable preview")).clicked() {
            self.settings.ui.preview_enabled = true;
        }
    }

    /// Tells the engine what preview the UI will actually show: none while
    /// disabled, minimised or with no preview dock on screen; otherwise at
    /// the size it was drawn last frame and at the chosen rate.
    fn sync_preview_config(&mut self, ctx: &egui::Context) {
        let minimised = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        let (main_px, vert_px) = std::mem::take(&mut self.preview_px);
        let (cw, _) = self.canvas_size();
        let vw = self.vertical_size().map(|(w, _)| w).unwrap_or(1080.0);
        // Rounded up to 64 px so resizing a dock doesn't change it every frame.
        let fit = |px: u32, max: f32| -> u32 { (px.div_ceil(64) * 64).clamp(160, (max as u32).max(160)) };
        let cfg = ferrous_engine::PreviewConfig {
            enabled: self.settings.ui.preview_enabled && (!minimised || self.projector_open) && (main_px > 0 || vert_px > 0 || self.projector_open),
            max_fps: self.settings.ui.preview_max_fps,
            width: if main_px > 0 { fit(main_px, cw) } else { self.preview_sent.map_or(1280, |c| c.width) },
            vertical_width: if vert_px > 0 { fit(vert_px, vw) } else { self.preview_sent.map_or(720, |c| c.vertical_width) },
        };
        if self.preview_sent != Some(cfg) {
            self.engine.set_preview_config(cfg);
            self.preview_sent = Some(cfg);
        }
    }

    fn vertical_size(&self) -> Option<(f32, f32)> {
        self.engine.settings().video.vertical.map(|(w, h)| ((w & !1) as f32, (h & !1) as f32))
    }

    /// An item's transform and visibility on the main or vertical canvas.
    fn layout_of(&self, vert: bool, item: &ferrous_core::SceneItem, size: (u32, u32)) -> (Transform, bool) {
        match (vert, self.vertical_size()) {
            (true, Some((w, h))) => {
                let _ = size;
                let (mw, mh) = self.canvas_size();
                self.edit_scene().vertical_layout(item, (w as u32, h as u32), (mw as u32, mh as u32))
            }
            _ => (item.transform, item.visible),
        }
    }

    fn set_layout(&mut self, vert: bool, id: u64, size: (u32, u32), f: impl FnOnce(&mut Transform)) {
        if vert {
            if let Some((w, h)) = self.vertical_size() {
                let _ = size;
                let (mw, mh) = self.canvas_size();
                if let Some(p) = self.edit_scene_mut().vertical_placement_mut(id, (w as u32, h as u32), (mw as u32, mh as u32)) {
                    f(&mut p.transform);
                }
            }
        } else if let Some(item) = self.item_mut(id) {
            f(&mut item.transform);
        }
    }

    fn preview(&mut self, ui: &mut egui::Ui, avail: Rect) {
        self.canvas_pane(ui, avail, false)
    }

    fn canvas_pane(&mut self, ui: &mut egui::Ui, avail: Rect, vert: bool) {
        let (cw, ch) = if vert { self.vertical_size().unwrap_or((1080.0, 1920.0)) } else { self.canvas_size() };
        let scale = (avail.width() / cw).min(avail.height() / ch).max(0.01);
        let size = Vec2::new(cw * scale, ch * scale);
        let rect = Rect::from_center_size(avail.center(), size);
        // Interact over the whole pane so handles that stick out past the
        // canvas (the rotation knob of an item at the top edge) still work.
        ui.allocate_rect(rect, Sense::hover());
        let resp = ui.interact(avail.intersect(ui.clip_rect()), ui.id().with(("canvas", vert)), Sense::click_and_drag());
        let painter = ui.painter_at(avail);

        painter.rect_filled(rect, 0, Color32::BLACK);
        self.note_preview_drawn(ui, rect, vert);
        let tex = if vert { &self.vertical_tex } else if self.studio_mode { &self.studio_tex } else { &self.preview_tex };
        if !self.settings.ui.preview_enabled {
            self.preview_disabled_overlay(ui, rect);
        } else if let Some(tex) = tex {
            painter.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        } else {
            painter.text(rect.center(), Align2::CENTER_CENTER, "Starting…", egui::FontId::proportional(16.0), theme::muted());
        }
        painter.rect_stroke(rect, 0, Stroke::new(1.0, theme::border()), StrokeKind::Outside);

        let to_screen = |x: f32, y: f32| Pos2::new(rect.left() + x * scale, rect.top() + y * scale);
        let to_canvas = |p: Pos2| Pos2::new((p.x - rect.left()) / scale, (p.y - rect.top()) / scale);

        // Visible items, topmost first, for hit testing.
        let items: Vec<(u64, bool, (u32, u32), Transform)> = self
            .edit_scene()
            .items
            .iter()
            .rev()
            .filter_map(|i| {
                let ((w, h), _) = self.engine.source_info(i.id)?;
                if w == 0 {
                    return None;
                }
                let (t, visible) = self.layout_of(vert, i, (w, h));
                if !visible {
                    return None;
                }
                Some((i.id, i.locked, (w, h), t))
            })
            .collect();
        let hit = |cp: Pos2| items.iter().find(|i| i.3.contains(i.2 .0, i.2 .1, cp.x, cp.y));

        // Right-click: select what's under the pointer and show its menu
        // (as in OBS), followed by the preview options.
        if resp.secondary_clicked() {
            self.selected = resp.interact_pointer_pos().and_then(|p| hit(to_canvas(p))).map(|i| i.0);
        }
        // On the vertical canvas "Hide" means hidden there only.
        let target = self.selected.and_then(|id| {
            let it = self.edit_scene().items.iter().find(|i| i.id == id)?;
            let visible = if vert { self.layout_of(true, it, (0, 0)).1 } else { it.visible };
            Some((id, visible, it.locked, it.keep_running))
        });
        let mut menu_action = None;
        resp.context_menu(|ui| {
            if let Some((_, visible, locked, keep)) = target {
                menu_action = source_menu(ui, visible, locked, keep);
                ui.separator();
            }
            crate::docks::preview_menu(ui, &mut self.settings.ui);
        });
        if let (Some(a), Some((id, ..))) = (menu_action, target) {
            self.source_action(id, a, vert);
        }

        let handle_size = 10.0;
        let rot_arm = 26.0; // screen px from the top edge to the rotation knob
        // Screen positions of the selected item's resize and rotate handles.
        // Screen positions of the eight handles, the top-middle point and the
        // rotation knob above it.
        let handles = |t: &Transform, src: (u32, u32)| -> ([Pos2; 8], Pos2, Pos2) {
            let pts = HANDLES.map(|(fx, fy)| {
                let (x, y) = t.point(src.0, src.1, fx, fy);
                to_screen(x, y)
            });
            let top_mid = pts[4];
            let (c, s) = t.rotation_cs();
            // "Up" in the box's frame is (0,-1) rotated: (s, -c).
            let knob = top_mid + Vec2::new(s, -c) * rot_arm;
            (pts, top_mid, knob)
        };
        let handle_at = |t: &Transform, src: (u32, u32), p: Pos2| -> Option<(f32, f32)> {
            let (pts, ..) = handles(t, src);
            HANDLES.iter().zip(pts).find(|(_, hp)| (p - *hp).length() <= handle_size).map(|(h, _)| *h)
        };
        let selected_item = self.selected.and_then(|id| items.iter().find(|i| i.0 == id)).copied();

        if resp.drag_started() {
            // Hit-test where the button went down, not where the pointer is
            // once egui has decided it's a drag (a few pixels later), so a
            // quick grab of a small handle still gets the handle.
            if let Some(p) = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()) {
                let cp = to_canvas(p);
                let mut started = false;
                if let Some((id, locked, src, t)) = selected_item {
                    let (_, _, knob) = handles(&t, src);
                    if !locked && (p - knob).length() <= handle_size {
                        self.drag = Some(Drag { id, mode: DragMode::Rotate, start_pointer: cp, start: t, src_size: src, vert });
                        started = true;
                    } else if let Some(h) = handle_at(&t, src, p).filter(|_| !locked) {
                        self.drag = Some(Drag { id, mode: DragMode::Handle(h), start_pointer: cp, start: t, src_size: src, vert });
                        started = true;
                    }
                }
                if !started {
                    if let Some(it) = hit(cp) {
                        self.selected = Some(it.0);
                        if !it.1 {
                            self.drag = Some(Drag { id: it.0, mode: DragMode::Move, start_pointer: cp, start: it.3, src_size: it.2, vert });
                        }
                    }
                }
            }
        }
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let on_knob = selected_item.is_some_and(|(_, _, src, t)| (p - handles(&t, src).2).length() <= handle_size);
                if !on_knob {
                    self.selected = hit(to_canvas(p)).map(|i| i.0);
                }
            }
        }
        if let (Some(drag), Some(p)) = (self.drag.as_ref().filter(|d| d.vert == vert), resp.interact_pointer_pos()) {
            let cp = to_canvas(p);
            let d = cp - drag.start_pointer;
            let (id, mode, start, src) = (drag.id, drag.mode, drag.start, drag.src_size);
            let (snap, shift) = ui.input(|i| (!i.modifiers.alt, i.modifiers.shift));
            self.set_layout(vert, id, src, |tf| {
                match mode {
                    DragMode::Move => {
                        // Snap the (rotated) box's bounds to the canvas edges
                        // and centre; hold Alt to disable.
                        let (bx0, by0, bx1, by1) = start.bounds(src.0, src.1);
                        let (bw, bh) = (bx1 - bx0, by1 - by0);
                        let mut nx = bx0 + d.x;
                        let mut ny = by0 + d.y;
                        if snap {
                            let tol = 12.0;
                            for target in [0.0, cw - bw, (cw - bw) / 2.0] {
                                if (nx - target).abs() < tol {
                                    nx = target;
                                }
                            }
                            for target in [0.0, ch - bh, (ch - bh) / 2.0] {
                                if (ny - target).abs() < tol {
                                    ny = target;
                                }
                            }
                        }
                        tf.x = (start.x + nx - bx0).round();
                        tf.y = (start.y + ny - by0).round();
                    }
                    DragMode::Handle(h) => {
                        // Corners keep the shape (Shift: free), edges stretch,
                        // Alt crops instead; the opposite side stays put.
                        let how = if !snap { ferrous_core::HandleDrag::Crop } else { ferrous_core::HandleDrag::Scale { free: shift } };
                        *tf = start.drag_handle(src.0, src.1, h, (d.x, d.y), how);
                    }
                    DragMode::Rotate => {
                        let (cx, cy) = start.center(src.0, src.1);
                        let a = (cp.y - cy).atan2(cp.x - cx).to_degrees() + 90.0;
                        let mut deg = a.rem_euclid(360.0);
                        if shift {
                            deg = (deg / 15.0).round() * 15.0;
                        } else if snap {
                            // Gentle detents at the right angles.
                            for r in [0.0, 90.0, 180.0, 270.0, 360.0] {
                                if (deg - r).abs() < 3.0 {
                                    deg = r;
                                }
                            }
                        }
                        tf.rotation = if deg >= 359.95 { 0.0 } else { (deg * 10.0).round() / 10.0 };
                    }
                }
            });
        }
        if resp.drag_stopped() && self.drag.as_ref().is_some_and(|d| d.vert == vert) {
            self.drag = None;
        }

        // Selection outline, resize handle and rotation knob.
        if let Some((t, src, locked)) = self.selected.and_then(|id| {
            self.edit_scene().items.iter().find(|i| i.id == id).and_then(|i| {
                let ((w, h), _) = self.engine.source_info(id)?;
                let (t, _) = self.layout_of(vert, i, (w, h));
                Some((t, (w, h), i.locked))
            })
        }) {
            let color = if locked { theme::muted() } else { theme::accent() };
            let k = t.corners(src.0, src.1).map(|(x, y)| to_screen(x, y));
            painter.add(egui::Shape::closed_line(k.to_vec(), Stroke::new(1.5, color)));
            if !locked {
                let (pts, top_mid, knob) = handles(&t, src);
                for hp in pts {
                    let r = Rect::from_center_size(hp, Vec2::splat(handle_size - 2.0));
                    painter.rect_filled(r, CornerRadius::same(2), color);
                    painter.rect_stroke(r, CornerRadius::same(2), Stroke::new(1.0, theme::bg()), StrokeKind::Outside);
                }
                let alt = ui.input(|i| i.modifiers.alt);
                let active = self.drag.as_ref().and_then(|d| if let DragMode::Handle(h) = d.mode { Some(h) } else { None });
                if let Some(h) = active.or_else(|| resp.hover_pos().and_then(|p| handle_at(&t, src, p))) {
                    ui.ctx().set_cursor_icon(handle_cursor(h, alt));
                }
                painter.line_segment([top_mid, knob], Stroke::new(1.0, color));
                let hot = resp.hover_pos().is_some_and(|p| (p - knob).length() <= handle_size) || self.drag.as_ref().is_some_and(|d| matches!(d.mode, DragMode::Rotate));
                painter.circle_filled(knob, if hot { 6.5 } else { 5.0 }, color);
                painter.circle_stroke(knob, if hot { 6.5 } else { 5.0 }, Stroke::new(1.0, theme::bg()));
                if hot {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Alias);
                }
                if t.is_rotated() || self.drag.as_ref().is_some_and(|d| matches!(d.mode, DragMode::Rotate)) {
                    painter.text(knob + Vec2::new(10.0, -10.0), Align2::LEFT_BOTTOM, format!("{:.1}°", t.rotation), egui::FontId::proportional(11.0), color);
                }
            }
        }
    }

    /// Per-channel window: filters (and, for devices, persisted settings).
    #[allow(deprecated)]
    fn audio_props_window(&mut self, ctx: &egui::Context) {
        let Some(id) = self.audio_props.clone() else { return };
        let Some(ch) = self.engine.mixer().channel(&id) else {
            self.audio_props = None;
            return;
        };
        let mut open = true;
        let mut settings_changed = false;
        let title = format!("Audio: {}", ch.name.lock());
        egui::Window::new(title).id(egui::Id::new(("audio_props", &id))).open(&mut open).default_width(380.0).show(ctx, |ui| {
            // Tracks and monitoring live on the channel; persist them where the channel's settings live.
            let item_id: Option<u64> = id.strip_prefix("source-").and_then(|n| n.parse().ok());
            let (mut tracks, mut monitor) = match id.as_str() {
                "desktop" => (self.settings.audio.desktop_tracks, self.settings.audio.desktop_monitor),
                "mic" => (self.settings.audio.mic_tracks, self.settings.audio.mic_monitor),
                _ => item_id.and_then(|i| self.item_mut(i)).map(|it| (it.audio_tracks, it.audio_monitor)).unwrap_or((1, 0)),
            };
            let mut delay = match id.as_str() {
                "desktop" => self.settings.audio.desktop_delay_ms,
                "mic" => self.settings.audio.mic_delay_ms,
                _ => item_id.and_then(|i| self.item_mut(i)).map(|it| it.audio_sync_ms).unwrap_or(0),
            };
            let d0 = delay;
            let (t0, m0) = (tracks, monitor);
            widgets::section_title(ui, "Tracks");
            ui.horizontal(|ui| {
                for t in 0..6 {
                    let mut on = tracks & (1 << t) != 0;
                    if ui.checkbox(&mut on, format!("{}", t + 1)).changed() {
                        tracks ^= 1 << t;
                    }
                }
            });
            widgets::section_title(ui, "Sync offset");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut delay).range(0..=ferrous_audio::MAX_DELAY_MS).speed(5).suffix(" ms"));
                ui.label(RichText::new("Holds this audio back to line it up with the picture.").small().color(theme::muted()));
            });
            widgets::section_title(ui, "Monitoring");
            egui::ComboBox::from_id_salt("mon").selected_text(["Off", "Monitor only (mute output)", "Monitor and output"][monitor.min(2) as usize]).show_ui(ui, |ui| {
                ui.selectable_value(&mut monitor, 0, "Off");
                ui.selectable_value(&mut monitor, 1, "Monitor only (mute output)");
                ui.selectable_value(&mut monitor, 2, "Monitor and output");
            });
            if monitor != 0 && id == "desktop" {
                ui.colored_label(theme::warn(), "Monitoring desktop audio on the same device will echo.");
            }
            if (tracks, monitor) != (t0, m0) {
                match id.as_str() {
                    "desktop" => {
                        self.settings.audio.desktop_tracks = tracks;
                        self.settings.audio.desktop_monitor = monitor;
                        settings_changed = true;
                    }
                    "mic" => {
                        self.settings.audio.mic_tracks = tracks;
                        self.settings.audio.mic_monitor = monitor;
                        settings_changed = true;
                    }
                    _ => {
                        if let Some(it) = item_id.and_then(|i| self.item_mut(i)) {
                            it.audio_tracks = tracks;
                            it.audio_monitor = monitor;
                        }
                    }
                }
            }
            if delay != d0 {
                match id.as_str() {
                    "desktop" => {
                        self.settings.audio.desktop_delay_ms = delay;
                        settings_changed = true;
                    }
                    "mic" => {
                        self.settings.audio.mic_delay_ms = delay;
                        settings_changed = true;
                    }
                    _ => {
                        if let Some(it) = item_id.and_then(|i| self.item_mut(i)) {
                            it.audio_sync_ms = delay;
                        }
                    }
                }
            }
            ui.separator();
            widgets::section_title(ui, "Filters");
            match id.as_str() {
                "desktop" | "mic" => {
                    let list = if id == "desktop" { &mut self.settings.audio.desktop_filters } else { &mut self.settings.audio.mic_filters };
                    let before = list.clone();
                    crate::filters_ui::audio_filters(ui, &id, list, Some(&ch.filters));
                    settings_changed |= *list != before;
                }
                other => {
                    let item_id: Option<u64> = other.strip_prefix("source-").and_then(|n| n.parse().ok());
                    if let Some(item) = item_id.and_then(|i| self.item_mut(i)) {
                        crate::filters_ui::audio_filters(ui, other, &mut item.audio_filters, Some(&ch.filters));
                    }
                }
            }
        });
        if settings_changed {
            if let Err(e) = self.engine.apply_settings(self.settings.clone()) {
                self.toast(e.to_string(), true);
            }
            let _ = self.library.save_profile(&self.active.profile, &self.settings);
        }
        if !open {
            self.audio_props = None;
        }
    }

    #[allow(deprecated)]
    fn toasts_overlay(&mut self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }
        let mut clicked: Option<ToastAction> = None;
        let mut dismiss: Option<usize> = None;
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(Align2::RIGHT_BOTTOM, [-16.0, -40.0])
            .show(ctx, |ui| {
                for (n, t) in self.toasts.iter().enumerate() {
                    let color = if t.error { theme::live() } else { theme::ok() };
                    egui::Frame::new()
                        .fill(theme::surface())
                        .stroke(Stroke::new(1.0, color))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.set_max_width(420.0);
                            ui.label(&t.text);
                            if let Some(d) = &t.detail {
                                ui.label(RichText::new(d).small().color(theme::muted()));
                            }
                            if let Some(a) = &t.action {
                                let label = match a {
                                    ToastAction::Undo => "↶ Undo".to_string(),
                                    ToastAction::Fix(f) => f.label().to_string(),
                                };
                                ui.horizontal(|ui| {
                                    if ui.button(label).clicked() {
                                        clicked = Some(a.clone());
                                        dismiss = Some(n);
                                    }
                                });
                            }
                        });
                    ui.add_space(6.0);
                }
            });
        if let Some(n) = dismiss {
            self.toasts.remove(n);
        }
        match clicked {
            Some(ToastAction::Undo) => self.undo_edit(),
            Some(ToastAction::Fix(f)) => self.apply_fix(f),
            None => {}
        }
    }
}

/// The beginner-friendly layer: Simple mode, starting points, the
/// pre-flight check, the command palette and the tour.
impl FerrousApp {
    fn set_simple_mode(&mut self, simple: bool) {
        self.layout.set_simple(&mut self.settings.ui, simple);
        if simple {
            self.studio_mode = false;
        }
        self.toast(if simple { "Simple mode: just the essentials." } else { "Advanced mode: every panel and menu." }, false);
    }

    /// Start (or stop) recording / streaming from a button: when starting,
    /// look for the usual problems first.
    fn request_start(&mut self, kind: crate::preflight::Kind) {
        use crate::preflight::Kind;
        let (active, action) = match kind {
            Kind::Record => (self.engine.is_recording(), HotkeyAction::ToggleRecording),
            Kind::Stream => (self.engine.is_streaming(), HotkeyAction::ToggleStreaming),
        };
        if active || !self.settings.ui.preflight {
            self.perform(action);
            return;
        }
        let checks = self.run_preflight(kind);
        if checks.is_empty() {
            self.perform(action);
        } else {
            self.preflight = Some((kind, checks));
        }
    }

    fn run_preflight(&self, kind: crate::preflight::Kind) -> Vec<crate::preflight::Check> {
        use crate::preflight::{self, ItemInfo};
        let items = self
            .edit_scene()
            .items
            .iter()
            .map(|i| ItemInfo {
                id: i.id,
                name: i.name.clone(),
                visible: i.visible,
                error: self.engine.source_info(i.id).and_then(|(_, e)| e),
                display: match &i.source {
                    SourceConfig::DisplayCapture { monitor_index, .. } => Some(*monitor_index),
                    _ => None,
                },
                video: !matches!(i.source, SourceConfig::ApplicationAudio { .. }),
            })
            .collect();
        let channels = self.engine.mixer().channels();
        let muted = |id: &str| channels.iter().find(|c| c.id == id && c.active()).map(|c| c.muted());
        let (working, found) = match &*self.probe.lock() {
            ProbeState::Done(p) => (Some(p.working.iter().map(|e| e.name.to_string()).collect()), true),
            ProbeState::Failed(_) => (None, false),
            ProbeState::Running => (None, true),
        };
        let folder = &self.settings.record.folder;
        preflight::run(&preflight::Inputs {
            kind,
            settings: &self.settings,
            working_encoders: working,
            ffmpeg_found: found,
            items,
            monitors: self.monitors.len(),
            mic_muted: muted("mic"),
            desktop_muted: muted("desktop"),
            free_bytes: preflight::free_bytes(folder),
            folder_ok: kind != preflight::Kind::Record || preflight::folder_writable(folder),
        })
    }

    fn apply_fix(&mut self, fix: crate::preflight::Fix) {
        use crate::preflight::Fix;
        match fix {
            Fix::Settings(tab) => self.settings_win.open_at(&self.settings, &self.scenes, tab),
            Fix::Source(id) => self.source_action(id, SourceAction::Properties, false),
            Fix::Unmute(id) => {
                if let Some(c) = self.engine.mixer().channels().into_iter().find(|c| c.id == id) {
                    c.set_muted(false);
                }
            }
            Fix::AddSource => self.add_source.open(),
        }
    }

    fn preflight_window(&mut self, ctx: &egui::Context) {
        use crate::preflight::{Kind, Severity};
        let Some((kind, checks)) = &self.preflight else { return };
        let kind = *kind;
        let checks = checks.clone();
        let (mut go, mut cancel, mut fix) = (false, false, None);
        let blocked = checks.iter().any(|c| c.severity == Severity::Error);
        egui::Window::new(if kind == Kind::Stream { "Before you go live" } else { "Before you record" })
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .fixed_size([460.0, 0.0])
            .show(ctx, |ui| {
                ui.label(RichText::new(if blocked { "Something needs fixing first:" } else { "Worth a look before you start:" }).strong());
                ui.add_space(4.0);
                for c in &checks {
                    let (icon, color) = if c.severity == Severity::Error { ("⛔", theme::live()) } else { ("⚠", theme::warn()) };
                    ui.horizontal_top(|ui| {
                        ui.label(RichText::new(icon).color(color).size(18.0));
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&c.title).strong());
                            ui.label(RichText::new(&c.detail).small().color(theme::muted()));
                            if let Some(f) = &c.fix {
                                if ui.button(f.label()).clicked() {
                                    fix = Some(f.clone());
                                }
                            }
                        });
                    });
                    ui.add_space(6.0);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let label = if kind == Kind::Stream { "Go live anyway" } else { "Record anyway" };
                        if ui.add_enabled(!blocked, egui::Button::new(label)).on_disabled_hover_text("Fix the ⛔ items first.").clicked() {
                            go = true;
                        }
                    });
                });
            });
        if cancel {
            self.preflight = None;
        } else if go {
            self.preflight = None;
            self.perform(if kind == Kind::Stream { HotkeyAction::ToggleStreaming } else { HotkeyAction::ToggleRecording });
        } else if let Some(f) = fix {
            self.apply_fix(f);
            // Look again, so a fixed problem disappears from the list.
            let again = self.run_preflight(kind);
            self.preflight = if again.is_empty() { None } else { Some((kind, again)) };
        }
    }

    /// Adds a new source of `kind` to the scene being edited, as the Add
    /// Source window would. Returns its id (`None` if a file picker was cancelled).
    fn add_source_kind(&mut self, kind: crate::add_source::Kind) -> Option<u64> {
        let (name, cfg) = self.make_source(&kind)?;
        let r = &mut self.settings.ui.recent_sources;
        r.retain(|k| *k != kind.key());
        r.insert(0, kind.key());
        r.truncate(8);
        let target = self.edit_index();
        let id = self.scenes.add_item_to(target, name, cfg);
        self.pending_fit.insert(id);
        self.selected = Some(id);
        Some(id)
    }

    /// The Sources list of an empty scene: big tiles for the common things.
    fn empty_sources(&mut self, ui: &mut egui::Ui) {
        use crate::add_source::Kind;
        ui.label(RichText::new("What do you want to show?").strong());
        let tiles: [(&str, &str, Kind, &str); 6] = [
            ("🖥", "My screen", Kind::Display, "Everything on your monitor."),
            ("🪟", "A window", Kind::Window, "One program's window."),
            ("🎮", "A game", Kind::Game, "A game that's running (best quality for games)."),
            ("📷", "Camera", Kind::Camera, "A webcam or capture card."),
            ("🔤", "Text", Kind::Text, "A caption or title."),
            ("🖼", "Picture", Kind::Image, "A logo or any image file."),
        ];
        let w = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
        let mut pick = None;
        for row in tiles.chunks(2) {
            ui.horizontal(|ui| {
                for (icon, label, kind, tip) in row {
                    // Painted by hand so the icon and the label are each centred.
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 56.0), Sense::click());
                    let resp = resp.on_hover_text(*tip);
                    let fill = if resp.is_pointer_button_down_on() { theme::surface().gamma_multiply(0.8) } else if resp.hovered() { theme::surface().lerp_to_gamma(Color32::WHITE, 0.1) } else { theme::surface() };
                    let p = ui.painter();
                    p.rect_filled(rect, 8.0, fill);
                    if resp.hovered() {
                        p.rect_stroke(rect, 8.0, Stroke::new(1.0, theme::accent()), StrokeKind::Inside);
                    }
                    p.text(rect.center() - Vec2::new(0.0, 10.0), Align2::CENTER_CENTER, icon, egui::FontId::proportional(20.0), theme::text());
                    p.text(rect.center() + Vec2::new(0.0, 16.0), Align2::CENTER_CENTER, label, egui::FontId::proportional(13.0), theme::text());
                    if resp.clicked() {
                        pick = Some(kind.clone());
                    }
                }
            });
        }
        if ui.small_button("More kinds…").clicked() {
            self.add_source.open();
        }
        if let Some(k) = pick {
            self.add_source_kind(k);
        }
        ui.add_space(4.0);
    }

    /// A new scene, made current, from a template.
    fn add_scene_from_template(&mut self, t: Template) {
        use crate::add_source::Kind;
        let mut name = t.name().to_string();
        let mut n = 2;
        while self.scenes.scenes.iter().any(|s| s.name == name) {
            name = format!("{} {n}", t.name());
            n += 1;
        }
        let idx = self.scenes.add_scene(name.clone());
        self.history_note_scene_added(idx);
        let add = |app: &mut Self, kind: Kind, place: Place| {
            if let Some(id) = app.add_source_kind(kind) {
                app.pending_fit.remove(&id);
                app.pending_place.insert(id, place);
            }
        };
        match t {
            Template::Blank => self.rename = Some(Rename::Scene(idx, name)),
            Template::Screen => add(self, Kind::Display, Place::Fill),
            Template::Chatting => add(self, Kind::Camera, Place::Fill),
            Template::Gameplay => {
                add(self, Kind::Game, Place::Fill);
                add(self, Kind::Camera, Place::Corner(0.22));
            }
            Template::Tutorial => {
                add(self, Kind::Display, Place::Fill);
                add(self, Kind::Camera, Place::Corner(0.2));
            }
            Template::StartingSoon => {
                add(self, Kind::Color, Place::Fill);
                if let Some(id) = self.add_source_kind(Kind::Text) {
                    self.pending_fit.remove(&id);
                    if let Some(it) = self.item_mut(id) {
                        it.name = "Starting soon".into();
                        if let SourceConfig::Text { text, size, .. } = &mut it.source {
                            *text = "Starting soon…".into();
                            *size = 120.0;
                        }
                    }
                    self.pending_place.insert(id, Place::Center);
                }
            }
        }
        self.selected = None;
    }

    /// Moves template sources to their places once the engine knows their size.
    fn place_pending(&mut self) {
        let (cw, ch) = self.canvas_size();
        let ids: Vec<u64> = self.pending_place.keys().copied().collect();
        for id in ids {
            if self.item_mut(id).is_none() {
                self.pending_place.remove(&id);
                continue;
            }
            let Some(((w, h), _)) = self.engine.source_info(id) else { continue };
            if w == 0 || h == 0 {
                continue;
            }
            let place = self.pending_place.remove(&id).unwrap();
            let t = match place {
                Place::Fill => Transform::fit_to(w, h, cw as u32, ch as u32),
                Place::Corner(frac) => {
                    let s = cw * frac / w as f32;
                    let margin = cw * 0.02;
                    Transform { x: cw - w as f32 * s - margin, y: ch - h as f32 * s - margin, scale_x: s, scale_y: s, ..Transform::default() }
                }
                Place::Center => Transform { x: (cw - w as f32) / 2.0, y: (ch - h as f32) / 2.0, ..Transform::default() },
            };
            if let Some(it) = self.item_mut(id) {
                it.transform = t;
            }
        }
    }

    /// Everything the command palette can do right now.
    fn palette_items(&self) -> Vec<crate::palette::Item> {
        use crate::add_source::Kind;
        use crate::palette::{Cmd, Item};
        let rec = self.engine.is_recording();
        let live = self.engine.is_streaming();
        let mut v = vec![
            Item::new(Cmd::ToggleRecording, if rec { "Stop recording" } else { "Start recording" }, "Action", "record capture save video"),
            Item::new(Cmd::ToggleStreaming, if live { "Stop streaming" } else { "Go live (start streaming)" }, "Action", "stream broadcast twitch youtube"),
            Item::new(Cmd::OpenSettings, "Open settings", "Action", "preferences options"),
            Item::new(Cmd::OpenRecordings, "Open the recordings folder", "Action", "videos files saved"),
            Item::new(Cmd::ToggleMode, if self.settings.ui.simple_mode { "Switch to Advanced mode" } else { "Switch to Simple mode" }, "Action", "layout panels easy more controls"),
            Item::new(Cmd::Undo, "Undo", "Action", "back revert ctrl+z"),
            Item::new(Cmd::Redo, "Redo", "Action", "forward ctrl+y"),
            Item::new(Cmd::NewScene, "New scene", "Scene", "add create"),
            Item::new(Cmd::ToggleReplay, "Start / stop the replay buffer", "Action", "instant replay clip"),
            Item::new(Cmd::SaveReplay, "Save a replay clip", "Action", "instant replay"),
            Item::new(Cmd::Tour, "Take the quick tour", "Help", "guide start learn"),
            Item::new(Cmd::Troubleshooting, "Troubleshooting: something isn't working", "Help", "help problem fix black screen no sound lag"),
            Item::new(Cmd::Presets, "Recording presets and benchmark", "Action", "quality compare test speed"),
        ];
        if !self.settings.ui.simple_mode {
            v.push(Item::new(Cmd::StudioMode, "Studio mode on / off", "Action", "preview transition"));
        }
        for (i, sc) in self.scenes.scenes.iter().enumerate() {
            v.push(Item::new(Cmd::SwitchScene(i), format!("Switch to scene: {}", sc.name), "Scene", "go show"));
        }
        for k in Kind::BUILTIN {
            let words = match k {
                Kind::Display => "screen monitor desktop",
                Kind::Window => "program app",
                Kind::Game => "fullscreen gameplay",
                Kind::Camera => "webcam capture card video device",
                Kind::Image => "picture logo png jpg",
                Kind::Media => "video file music audio mp4",
                Kind::Text => "caption title words",
                Kind::Browser => "web page overlay alerts chat",
                Kind::AppAudio => "sound program volume",
                Kind::Color => "background solid",
                _ => "",
            };
            v.push(Item::new(Cmd::AddSource(k.clone()), format!("Add source: {}", k.title()), "Source", words));
        }
        if let Some(id) = self.selected {
            if let Some(it) = self.edit_scene().items.iter().find(|i| i.id == id) {
                v.push(Item::new(Cmd::DuplicateSource, format!("Duplicate “{}”", it.name), "Source", "copy clone"));
                v.push(Item::new(Cmd::ToggleSourceVisible, format!("{} “{}”", if it.visible { "Hide" } else { "Show" }, it.name), "Source", "eye visible"));
                v.push(Item::new(Cmd::RemoveSource, format!("Remove “{}”", it.name), "Source", "delete"));
            }
        }
        for (name, tab, words) in settings_ui::index() {
            v.push(Item::new(Cmd::SettingsTab(tab), format!("Setting: {name}"), "Settings", words));
        }
        v
    }

    fn run_command(&mut self, cmd: crate::palette::Cmd) {
        use crate::palette::Cmd;
        use crate::preflight::Kind as Start;
        match cmd {
            Cmd::ToggleRecording => self.request_start(Start::Record),
            Cmd::ToggleStreaming => self.request_start(Start::Stream),
            Cmd::ToggleReplay => self.perform(HotkeyAction::ToggleReplayBuffer),
            Cmd::SaveReplay => self.perform(HotkeyAction::SaveReplay),
            Cmd::OpenSettings => self.settings_win.open_with(&self.settings, &self.scenes),
            Cmd::SettingsTab(t) => self.settings_win.open_at(&self.settings, &self.scenes, t),
            Cmd::AddSource(k) => {
                self.add_source_kind(k);
            }
            Cmd::SwitchScene(i) => {
                if self.studio_mode {
                    self.studio_scene = i;
                } else {
                    self.scenes.current = i;
                }
                self.selected = None;
            }
            Cmd::NewScene => self.add_scene_from_template(Template::Blank),
            Cmd::ToggleMode => self.set_simple_mode(!self.settings.ui.simple_mode),
            Cmd::Undo => self.undo_edit(),
            Cmd::Redo => self.redo_edit(),
            Cmd::OpenRecordings => open_folder(&self.settings.record.folder),
            Cmd::Tour => {
                self.tour_step = 0;
                self.settings.ui.tour_done = false;
            }
            Cmd::Troubleshooting => self.troubleshooting = true,
            Cmd::DuplicateSource | Cmd::ToggleSourceVisible | Cmd::RemoveSource => {
                let Some(id) = self.selected else { return };
                match cmd {
                    Cmd::DuplicateSource => self.source_action(id, SourceAction::Duplicate, false),
                    Cmd::RemoveSource => self.source_action(id, SourceAction::Remove, false),
                    _ => {
                        if let Some(it) = self.item_mut(id) {
                            it.visible = !it.visible;
                        }
                    }
                }
            }
            Cmd::StudioMode => {
                self.studio_mode = !self.studio_mode;
                self.studio_scene = self.scenes.current;
                self.selected = None;
            }
            Cmd::Presets => self.settings_win.open_at(&self.settings, &self.scenes, "recording"),
        }
    }

    /// Per-frame hooks: zoom, Ctrl+K, the palette, pre-flight window, the
    /// tour and troubleshooting.
    fn helpers(&mut self, ctx: &egui::Context) {
        // Interface zoom: the setting and Ctrl +/− agree with each other.
        let z = ctx.zoom_factor();
        if (z - self.scale_applied).abs() > 0.001 {
            self.settings.ui.ui_scale = z.clamp(0.8, 2.0);
            self.scale_applied = z;
        } else if (self.settings.ui.ui_scale - self.scale_applied).abs() > 0.001 {
            let want = self.settings.ui.ui_scale.clamp(0.8, 2.0);
            ctx.set_zoom_factor(want);
            self.scale_applied = want;
        }
        theme::sync_access(ctx, self.settings.ui.high_contrast, self.settings.ui.meter_patterns, self.settings.ui.reduce_motion);

        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::K)) {
            self.palette.toggle();
        }
        if self.palette.open {
            let items = self.palette_items();
            if let Some(cmd) = self.palette.show(ctx, &items) {
                self.run_command(cmd);
            }
        }
        self.preflight_window(ctx);
        crate::help::troubleshooting(ctx, &mut self.troubleshooting, &mut self.troubleshooting_filter);

        let first_run_over = self.settings.ui.setup_done || self.setup_dismissed;
        if !self.settings.ui.tour_done && first_run_over && self.setup.is_none() && self.wizard.is_none() {
            let progress = crate::help::Progress { has_source: self.scenes.scenes.iter().any(|s| !s.items.is_empty()), recording: self.engine.is_recording() };
            if let crate::help::TourEvent::Done = crate::help::tour(ctx, &mut self.tour_step, progress) {
                self.settings.ui.tour_done = true;
                let _ = self.library.save_profile(&self.active.profile, &self.settings);
            }
        }
    }
}

impl eframe::App for FerrousApp {
    /// egui's own layout state (panel sizes, floating windows) is saved by
    /// eframe; we also save settings so dock visibility survives a crash.
    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        let _ = self.library.save_profile(&self.active.profile, &self.settings);
    }

    fn auto_save_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(10)
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let (tid, tglass, tmotion) = match self.settings_win.theme_preview() {
            Some((i, g, m)) => (i.to_string(), g, m),
            None => (self.settings.ui.theme.clone(), self.settings.ui.glass, self.settings.ui.motion),
        };
        theme::set_active(ctx.input(|i| i.viewport().focused.unwrap_or(true) && !i.viewport().minimized.unwrap_or(false)));
        theme::sync(&ctx, &tid, tglass, tmotion);
        theme::paint_backdrop(&ctx);
        let weather = theme::weather();
        if let Some((level, storm)) = weather {
            fx::step(&ctx, level, storm);
        } else if theme::glassy() && tmotion > 0 && theme::animated() && theme::is_moving() {
            ctx.request_repaint_after(std::time::Duration::from_millis(if tmotion >= 2 { 16 } else { 33 }));
        }
        self.tick(&ctx);

        egui::Panel::top("top")
            .frame(egui::Frame::new().fill(theme::chrome()).inner_margin(egui::Margin::symmetric(12, 8)))
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status")
            .frame(egui::Frame::new().fill(theme::chrome()).inner_margin(egui::Margin::symmetric(12, 4)))
            .show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::chrome()))
            .show(ui, |ui| self.dock_area(ui));
        if weather.is_some() {
            fx::paint(&ctx, theme::accent().lerp_to_gamma(Color32::WHITE, 0.45));
        }

        let probe = self.probe.lock().clone();
        if self.settings_win.open {
            self.settings_win.link_ports = self.engine.link_stats().into_iter().map(|(n, p, _)| (n, p)).collect();
        }
        match self.settings_win.show(&ctx, &probe) {
            SettingsAction::None => {}
            SettingsAction::Reprobe => {
                self.probe = settings_ui::start_probe(&self.settings.ffmpeg_path, ctx.clone());
            }
            SettingsAction::Apply(new) => {
                let new = *new;
                let ffmpeg_changed = new.ffmpeg_path != self.settings.ffmpeg_path;
                let new = self.with_mixer_state(new);
                match self.engine.apply_settings(new.clone()) {
                    Ok(()) => {
                        self.adopt_settings(new);
                        if let Err(e) = self.library.save_profile(&self.active.profile, &self.settings) {
                            self.toast(format!("Couldn't save settings: {e}"), true);
                        }
                        if ffmpeg_changed {
                            self.probe = settings_ui::start_probe(&self.settings.ffmpeg_path, ctx.clone());
                        }
                    }
                    Err(e) => self.toast(e.to_string(), true),
                }
            }
        }
        if let ProbeState::Failed(e) = &probe {
            if !self.toasts.iter().any(|t| t.text == *e) && self.preview_serial % 600 == 1 {
                self.toast(e.clone(), true);
            }
        }
        self.audio_props_window(&ctx);
        self.source_windows(&ctx);
        self.import_report_window(&ctx);
        self.session_prompt_window(&ctx);
        let probe_state = self.probe.lock().clone();
        // First run: open setup automatically.
        if !self.settings.ui.setup_done && self.setup.is_none() && !self.setup_dismissed {
            self.setup = Some(crate::setup::Setup::new(&self.settings));
        }
        let display = self.monitors.first().map(|m| (m.width, m.height)).filter(|(w, h)| *w > 0 && *h > 0)
            .unwrap_or((self.settings.video.base_width, self.settings.video.base_height));
        let ffmpeg = ferrous_encode::ffmpeg::locate(&self.settings.ffmpeg_path);
        let setup_out = self.setup.as_mut().map(|s| s.show(&ctx, &probe_state, display, ffmpeg));
        match setup_out {
            Some(crate::setup::Outcome::Close { completed }) => {
                self.setup = None;
                self.setup_dismissed = true;
                if completed {
                    self.settings.ui.setup_done = true;
                    let _ = self.library.save_profile(&self.active.profile, &self.settings);
                }
            }
            Some(crate::setup::Outcome::Apply(new)) => {
                let new = *new;
                self.setup = None;
                self.setup_dismissed = true;
                let new = self.with_mixer_state(new);
                match self.engine.apply_settings(new.clone()) {
                    Ok(()) => {
                        self.adopt_settings(new);
                        let _ = self.library.save_profile(&self.active.profile, &self.settings);
                        self.toast("Benchmark recommendations applied", false);
                    }
                    Err(e) => self.toast(e.to_string(), true),
                }
            }
            _ => {}
        }
        let outcome = self.wizard.as_mut().map(|w| w.show(&ctx, &probe_state, &self.monitors));
        if let Some(outcome) = outcome {
            match outcome {
                crate::wizard::Outcome::Open => {}
                crate::wizard::Outcome::Cancel => self.wizard = None,
                crate::wizard::Outcome::Apply(new) => {
                let new = *new;
                    self.wizard = None;
                    let new = self.with_mixer_state(new);
                    match self.engine.apply_settings(new.clone()) {
                        Ok(()) => {
                            self.adopt_settings(new);
                            let _ = self.library.save_profile(&self.active.profile, &self.settings);
                            self.toast("Auto-configuration applied", false);
                        }
                        Err(e) => self.toast(e.to_string(), true),
                    }
                }
            }
        }
        self.helpers(&ctx);
        self.projector_window(&ctx);
        self.multiview_window(&ctx);
        self.history_tick(&ctx);
        self.add_source_window(&ctx);
        self.toasts_overlay(&ctx);
        self.custom_docks_window(&ctx);
        // Last, once everything that could cover a page has been drawn.
        let frames = std::mem::take(&mut self.web_frames);
        let ids: Vec<u32> = self.settings.ui.custom_docks.iter().map(|c| c.id).collect();
        self.webdocks.sync(&ctx, frame, frames, &ids);
    }
}

impl Drop for FerrousApp {
    fn drop(&mut self) {
        self.save_scenes_if_changed(true);
        // Persist mixer volumes.
        let _ = self.library.save_profile(&self.active.profile, &self.settings);
    }
}

/// Kind, duration and the kind's own options.
fn transition_editor(ui: &mut egui::Ui, salt: &str, t: &mut ferrous_core::Transition) {
    egui::ComboBox::from_id_salt(salt).selected_text(t.kind.label()).width(ui.available_width()).height(400.0).show_ui(ui, |ui| {
        for k in TransitionKind::all() {
            let same = std::mem::discriminant(&k) == std::mem::discriminant(&t.kind);
            if ui.selectable_label(same, k.label()).clicked() && !same {
                t.kind = k;
            }
        }
    });
    ui.add(egui::DragValue::new(&mut t.duration_ms).range(0..=10_000).speed(10).suffix(" ms"));
    transition_options(ui, &mut t.kind);
}

fn transition_options(ui: &mut egui::Ui, kind: &mut TransitionKind) {
    match kind {
        TransitionKind::FadeToColor { bgra } => {
            let mut rgb = [bgra[2], bgra[1], bgra[0]];
            if ui.color_edit_button_srgb(&mut rgb).changed() {
                *bgra = [rgb[2], rgb[1], rgb[0], 255];
            }
        }
        TransitionKind::Swipe { direction } | TransitionKind::Slide { direction } => {
            egui::ComboBox::from_id_salt("tdir").selected_text(format!("{direction:?}")).show_ui(ui, |ui| {
                for d in [TransitionDirection::Left, TransitionDirection::Right, TransitionDirection::Up, TransitionDirection::Down] {
                    ui.selectable_value(direction, d, format!("{d:?}"));
                }
            });
        }
        TransitionKind::Luma { wipe, image, softness, invert } => {
            let current = image.as_ref().and_then(|p| p.file_name()).map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| wipe.clone());
            ui.horizontal(|ui| {
                ui.image(egui::load::SizedTexture::new(wipe_thumb(ui.ctx(), wipe, image.as_deref()).id(), Vec2::new(48.0, 27.0)));
                ui.menu_button(RichText::new(current).small(), |ui| {
                    ui.set_max_width(6.0 * 70.0);
                    egui::Grid::new("wipes").spacing([6.0, 6.0]).show(ui, |ui| {
                        for (i, name) in ferrous_core::import_obs::LUMA_WIPES.iter().enumerate() {
                            let tex = wipe_thumb(ui.ctx(), name, None);
                            let selected = image.is_none() && wipe == name;
                            let b = egui::Button::image(egui::load::SizedTexture::new(tex.id(), Vec2::new(64.0, 36.0))).selected(selected);
                            if ui.add(b).on_hover_text(*name).clicked() {
                                *wipe = name.to_string();
                                *image = None;
                                ui.close();
                            }
                            if i % 6 == 5 {
                                ui.end_row();
                            }
                        }
                    });
                    ui.separator();
                    if ui.button("Custom image…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().add_filter("Image", &["png", "jpg", "jpeg", "bmp", "webp"]).pick_file() {
                            *image = Some(p);
                        }
                        ui.close();
                    }
                });
            });
            ui.label(RichText::new("Softness").small().color(theme::muted()));
            ui.spacing_mut().slider_width = (ui.available_width() - 50.0).max(40.0);
            ui.add(egui::Slider::new(softness, 0.0..=1.0).max_decimals(2));
            ui.checkbox(invert, "Invert");
        }
        TransitionKind::Stinger { path, cut_point_ms } => {
            if ui.small_button("Choose video…").clicked() {
                if let Some(p) = rfd::FileDialog::new().add_filter("Video", &["webm", "mov", "mp4", "mkv"]).pick_file() {
                    *path = p;
                }
            }
            ui.label(RichText::new(path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()).small());
            ui.add(egui::DragValue::new(cut_point_ms).range(0..=10_000).prefix("cut at ").suffix(" ms"));
        }
        TransitionKind::Matte { path, layout, invert } => {
            if ui.small_button("Choose matte video…").clicked() {
                if let Some(p) = rfd::FileDialog::new().add_filter("Video", &["webm", "mov", "mp4", "mkv"]).pick_file() {
                    *path = p;
                }
            }
            ui.label(RichText::new(path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()).small());
            egui::ComboBox::from_id_salt("matte_layout").selected_text(layout.label()).show_ui(ui, |ui| {
                for l in ferrous_core::scene::MatteLayout::ALL {
                    ui.selectable_value(layout, l, l.label());
                }
            });
            ui.checkbox(invert, "Invert (white = outgoing)");
            ui.label(RichText::new("White parts of the video show the new scene, black parts the old one. Set the duration to the video's length.").small().color(theme::muted()));
        }
        _ => {}
    }
}

/// A small preview of a luma wipe (cached in egui's memory).
fn wipe_thumb(ctx: &egui::Context, wipe: &str, image: Option<&std::path::Path>) -> egui::TextureHandle {
    let id = egui::Id::new(("wipe_thumb", wipe, image));
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return t;
    }
    let (w, h) = (96u32, 54u32);
    let map = ferrous_render::transitions::luma_map(wipe, image, w, h);
    // Show it as the wipe looks a third of the way in: switched area in the accent colour.
    let rgba: Vec<u8> = map
        .iter()
        .flat_map(|l| {
            let v = *l as f32 / 255.0;
            let k = 0.12 + 0.88 * (1.0 - v);
            [(theme::accent().r() as f32 * k) as u8, (theme::accent().g() as f32 * k) as u8, (theme::accent().b() as f32 * k) as u8, 255]
        })
        .collect();
    let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
    let tex = ctx.load_texture(format!("wipe-{wipe}"), img, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    tex
}

fn open_folder(path: &std::path::Path) {
    let _ = std::fs::create_dir_all(path);
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(windows))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

struct DockViewer<'a> {
    app: &'a mut FerrousApp,
    /// Each dock's panel-mate at the start of the frame.
    mates: std::collections::HashMap<crate::layout::Dock, crate::layout::Dock>,
    closed: Vec<(crate::layout::Dock, Option<crate::layout::Dock>)>,
    locked: bool,
}

impl egui_stack::TabViewer for DockViewer<'_> {
    type Tab = crate::layout::Dock;

    fn id(&mut self, tab: &mut Self::Tab) -> egui::Id {
        egui::Id::new(("dock", tab.id()))
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        use crate::layout::Dock;
        match tab {
            Dock::Preview if self.app.studio_mode => "Preview  ·  Program".into(),
            Dock::Properties => match self.app.selected.and_then(|id| self.app.item_mut(id).map(|i| i.source.kind_label().to_string())) {
                Some(kind) => format!("Properties · {kind}").into(),
                None => "Properties".into(),
            },
            Dock::Vertical => match self.app.vertical_size() {
                Some((w, h)) => format!("Vertical Canvas · {}×{}", w as u32, h as u32).into(),
                None => "Vertical Canvas".into(),
            },
            Dock::Custom(id) => match self.app.settings.ui.custom_docks.iter().find(|c| c.id == *id) {
                Some(c) => c.name.clone().into(),
                None => "Custom Dock".into(),
            },
            d => d.title().into(),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        // The weather lands on whatever panel this is, the video preview included.
        fx::note_panel(ui.max_rect().expand(8.0));
        if let crate::layout::Dock::Custom(id) = *tab {
            self.app.custom_dock(ui, id);
        } else if tab.is_canvas() {
            self.app.canvas_dock(ui, *tab);
        } else {
            theme::in_dock(|| self.app.dock_contents(ui, *tab));
        }
    }

    fn is_closeable(&self, tab: &Self::Tab) -> bool {
        tab.closeable() && !self.locked
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> egui_stack::tab_viewer::OnCloseResponse {
        if !tab.closeable() {
            return egui_stack::tab_viewer::OnCloseResponse::Ignore;
        }
        self.closed.push((*tab, self.mates.get(tab).copied()));
        egui_stack::tab_viewer::OnCloseResponse::Close
    }

    fn scroll_bars(&self, tab: &Self::Tab) -> [bool; 2] {
        if tab.is_canvas() { [false, false] } else { [false, true] }
    }
}

/// Choices in a source's right-click menu.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceAction {
    Properties,
    Filters,
    Rename,
    Duplicate,
    Remove,
    MoveUp,
    MoveDown,
    MoveTop,
    MoveBottom,
    ToggleVisible,
    ToggleLock,
    ToggleKeepRunning,
    ResetTransform,
    FitToScreen,
    StretchToScreen,
    CenterOnScreen,
    RotateCw,
    RotateCcw,
    ResetCrop,
}

/// A source's right-click menu (Sources list and canvas): the everyday
/// actions first, layout tools in submenus, and the destructive one last.
fn source_menu(ui: &mut egui::Ui, visible: bool, locked: bool, keep_running: bool) -> Option<SourceAction> {
    let mut pick = None;
    ui.set_min_width(210.0);
    let mut item = |ui: &mut egui::Ui, label: &str, a: SourceAction| {
        if ui.button(label).clicked() {
            pick = Some(a);
            ui.close();
        }
    };
    item(ui, "⚙  Properties", SourceAction::Properties);
    item(ui, "🎚  Filters", SourceAction::Filters);
    ui.separator();
    item(ui, if visible { "🙈  Hide" } else { "👁  Show" }, SourceAction::ToggleVisible);
    item(ui, if locked { "🔓  Unlock" } else { "🔒  Lock" }, SourceAction::ToggleLock);
    item(ui, "⧉  Duplicate", SourceAction::Duplicate);
    item(ui, "✏  Rename   F2", SourceAction::Rename);
    ui.separator();
    ui.menu_button("↔  Transform", |ui| {
        item(ui, "Fit to Screen", SourceAction::FitToScreen);
        item(ui, "Stretch to Screen", SourceAction::StretchToScreen);
        item(ui, "Center on Screen", SourceAction::CenterOnScreen);
        ui.separator();
        item(ui, "Rotate 90° Clockwise", SourceAction::RotateCw);
        item(ui, "Rotate 90° Counter-clockwise", SourceAction::RotateCcw);
        ui.separator();
        item(ui, "Remove Crop", SourceAction::ResetCrop);
        item(ui, "Reset Transform", SourceAction::ResetTransform);
    });
    ui.menu_button("☰  Order", |ui| {
        item(ui, "Bring to Front", SourceAction::MoveTop);
        item(ui, "Move Up", SourceAction::MoveUp);
        item(ui, "Move Down", SourceAction::MoveDown);
        item(ui, "Send to Back", SourceAction::MoveBottom);
    });
    ui.menu_button("More", |ui| {
        item(ui, if keep_running { "Allow to sleep when hidden" } else { "Keep running when hidden" }, SourceAction::ToggleKeepRunning);
    });
    ui.separator();
    if ui.button(RichText::new("🗑  Remove").color(theme::live())).clicked() {
        pick = Some(SourceAction::Remove);
        ui.close();
    }
    pick
}

/// Which part of a source's properties to show.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PropsPart {
    All,
    Settings,
    Filters,
}

/// Speaker button that crossfades to red when muted.
fn mute_button(ui: &mut egui::Ui, muted: bool, t: f32) -> egui::Response {
    let fill = theme::surface().lerp_to_gamma(theme::live().linear_multiply(0.55), t);
    ui.add(egui::Button::new(RichText::new(if muted { "🔇" } else { "🔊" }).small()).fill(fill).min_size(Vec2::new(22.0, 18.0)))
        .on_hover_text(if muted { "Unmute" } else { "Mute" })
}

/// "Fit to screen" that respects rotation: the rotated box's bounds are
/// scaled to fit the canvas and centred.
fn fit_rotated(t: &Transform, src_w: u32, src_h: u32, cw: f32, ch: f32) -> Transform {
    let w = src_w.saturating_sub(t.crop[0] + t.crop[2]).max(1) as f32;
    let h = src_h.saturating_sub(t.crop[1] + t.crop[3]).max(1) as f32;
    let (c, s) = t.rotation_cs();
    let (bw, bh) = (w * c.abs() + h * s.abs(), w * s.abs() + h * c.abs());
    let k = (cw / bw).min(ch / bh);
    let (ow, oh) = (w * k, h * k);
    Transform { x: (cw - ow) / 2.0, y: (ch - oh) / 2.0, scale_x: k, scale_y: k, ..*t }
}
