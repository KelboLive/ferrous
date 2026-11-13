//! Modular dock layout.
//!
//! Every card of the main window (preview, scenes, sources, mixer, …) is a
//! dock that can be dragged by its tab and snapped beside, above, below or
//! into (as a tab) any other dock, pulled out into a floating window, resized
//! with the splitters between docks, closed and reopened from View → Docks.
//! The whole arrangement is a tree that is saved with the profile, so it
//! comes back exactly as it was left; named layouts can be saved and recalled.

use std::collections::HashMap;

use eframe::egui::{self, Color32, CornerRadius, Margin, Stroke};
use egui_stack::{DockState, NodeIndex, OverlayType, Style, TabPath};
use ferrous_core::{DockState as LegacyDockState, SnapGuides, UiSettings};
use serde::{Deserialize, Serialize};

use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, PartialOrd, Ord)]
pub enum Dock {
    Preview,
    Vertical,
    Scenes,
    Sources,
    Controls,
    Mixer,
    Properties,
    Stats,
    Performance,
    /// A custom browser dock, by `CustomDock::id`.
    Custom(u32),
}

pub const ALL: [Dock; 9] =
    [Dock::Preview, Dock::Vertical, Dock::Scenes, Dock::Sources, Dock::Controls, Dock::Mixer, Dock::Properties, Dock::Stats, Dock::Performance];

impl Dock {
    pub fn id(self) -> &'static str {
        match self {
            Dock::Preview => "preview",
            Dock::Vertical => "vertical",
            Dock::Scenes => "scenes",
            Dock::Sources => "sources",
            Dock::Controls => "controls",
            Dock::Mixer => "mixer",
            Dock::Properties => "properties",
            Dock::Stats => "stats",
            Dock::Performance => "performance",
            Dock::Custom(_) => "custom",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Dock::Preview => "Preview",
            Dock::Vertical => "Vertical Canvas",
            Dock::Scenes => "Scenes",
            Dock::Sources => "Sources",
            Dock::Controls => "Controls",
            Dock::Mixer => "Audio Mixer",
            Dock::Properties => "Properties",
            Dock::Stats => "Stats",
            Dock::Performance => "Performance Monitor",
            // Named from settings by the dock viewer.
            Dock::Custom(_) => "Custom Dock",
        }
    }
    /// The preview can be moved anywhere but never closed.
    pub fn closeable(self) -> bool {
        self != Dock::Preview
    }
    /// Docks whose content fills them edge to edge: no scroll bars, no
    /// padding (the canvases, and web pages, which scroll themselves).
    pub fn is_canvas(self) -> bool {
        matches!(self, Dock::Preview | Dock::Vertical | Dock::Custom(_))
    }
    fn wide(self) -> bool {
        matches!(self, Dock::Stats | Dock::Performance | Dock::Vertical)
    }
}

// ------------------------------------------------------------------ presets

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preset {
    /// Scenes/Sources on the left, Controls/Mixer/Properties on the right.
    Ferrous,
    /// OBS's arrangement: preview on top, a row of docks along the bottom.
    Classic,
    /// Ferrous plus Stats and the Performance Monitor under the preview.
    Analytics,
    /// Ferrous with the 9:16 canvas beside the preview, for TikTok/Shorts.
    Vertical,
    /// The Simple mode: the Ferrous layout with room for the big buttons.
    /// Not offered in the presets menu.
    Simple,
}

pub const PRESETS: [Preset; 4] = [Preset::Ferrous, Preset::Classic, Preset::Analytics, Preset::Vertical];

impl Preset {
    pub fn label(self) -> &'static str {
        match self {
            Preset::Ferrous => "Ferrous (default)",
            Preset::Classic => "Classic (OBS-style)",
            Preset::Analytics => "Analytics",
            Preset::Vertical => "Vertical / TikTok",
            Preset::Simple => "Simple",
        }
    }

    pub fn build(self) -> DockState<Dock> {
        let mut s = DockState::new(vec![Dock::Preview]);
        let t = s.main_surface_mut();
        let root = NodeIndex::root();
        let controls = if self == Preset::Simple { 0.40 } else { 0.30 };
        let ember = |t: &mut egui_stack::Tree<Dock>| -> NodeIndex {
            let [center, left] = t.split_left(root, 0.19, vec![Dock::Scenes]);
            t.split_below(left, 0.42, vec![Dock::Sources]);
            let [center, right] = t.split_right(center, 0.74, vec![Dock::Controls]);
            // Properties open from a source's right-click menu (as in OBS);
            // the dock is still in View → Docks for anyone who wants it.
            t.split_below(right, controls, vec![Dock::Mixer]);
            center
        };
        match self {
            Preset::Ferrous | Preset::Simple => {
                ember(t);
            }
            Preset::Classic => {
                let [_, row] = t.split_below(root, 0.66, vec![Dock::Scenes]);
                let [_, b] = t.split_right(row, 0.18, vec![Dock::Sources]);
                let [_, b] = t.split_right(b, 0.30, vec![Dock::Mixer]);
                t.split_right(b, 0.62, vec![Dock::Controls]);
            }
            Preset::Analytics => {
                let center = ember(t);
                let [_, stats] = t.split_below(center, 0.60, vec![Dock::Stats]);
                t.split_right(stats, 0.48, vec![Dock::Performance]);
            }
            Preset::Vertical => {
                let center = ember(t);
                t.split_right(center, 0.66, vec![Dock::Vertical]);
            }
        }
        s
    }
}

// ------------------------------------------------------------------- layout

pub struct Layout {
    pub tree: DockState<Dock>,
    /// Where a closed dock goes back to: a dock it shared a panel with.
    anchors: HashMap<Dock, Dock>,
    /// Last serialized form, to avoid rewriting settings every frame.
    saved: Option<String>,
}

impl Layout {
    /// The saved layout, or the default one (honouring the old show/float
    /// switches of earlier versions).
    pub fn load(ui: &UiSettings) -> Self {
        if let Some(tree) = ui.layout.as_deref().and_then(parse) {
            return Self { tree, anchors: HashMap::new(), saved: ui.layout.clone() };
        }
        let mut l = Self::from_preset(if ui.simple_mode { Preset::Simple } else { Preset::Ferrous });
        // Docks that were never part of the old layout start closed.
        l.close(Dock::Vertical);
        for d in [Dock::Scenes, Dock::Sources, Dock::Controls, Dock::Mixer, Dock::Properties] {
            match ui.docks.get(d.id()) {
                Some(LegacyDockState::Hidden) => l.close(d),
                Some(LegacyDockState::Floating) => l.float(d, egui::pos2(200.0, 150.0)),
                _ => {}
            }
        }
        for d in [Dock::Stats, Dock::Performance] {
            match ui.docks.get(d.id()) {
                Some(LegacyDockState::Docked) => l.open(d),
                Some(LegacyDockState::Floating) => {
                    l.open(d);
                    l.float(d, egui::pos2(260.0, 200.0));
                }
                _ => {}
            }
        }
        l
    }

    /// [`Layout::load`], plus the preview guarantee and one-time updates to
    /// saved layouts (recorded in `ui.layout_rev`).
    pub fn load_migrated(ui: &mut UiSettings) -> Self {
        let mut l = Self::load(ui);
        l.ensure_preview();
        if ui.layout_rev < 1 {
            // Properties moved to the sources' right-click menu (as in OBS).
            l.close(Dock::Properties);
            ui.layout_rev = 1;
        }
        if ui.layout_rev < 2 {
            // Simple mode is new: anyone who already arranged their docks
            // keeps the full interface.
            if ui.layout.is_some() {
                ui.simple_mode = false;
            }
            ui.layout_rev = 2;
        }
        l
    }

    pub fn from_preset(p: Preset) -> Self {
        Self { tree: p.build(), anchors: HashMap::new(), saved: None }
    }

    /// Write the layout into settings if it changed.
    pub fn store(&mut self, ui: &mut UiSettings) {
        let s = serialize(&self.tree);
        if self.saved.as_deref() != Some(&s) || ui.layout.as_deref() != Some(&s) {
            ui.layout = Some(s.clone());
            self.saved = Some(s);
        }
    }

    pub fn find(&self, d: Dock) -> Option<TabPath> {
        self.tree.find_tab(&d)
    }

    pub fn is_open(&self, d: Dock) -> bool {
        self.find(d).is_some()
    }

    pub fn is_floating(&self, d: Dock) -> bool {
        self.find(d).is_some_and(|p| !p.surface.is_main())
    }

    /// Remember which dock `d` shares a panel with, so reopening puts it back.
    pub fn remferrous_anchor(&mut self, d: Dock) {
        if let Some(mate) = self.mate_of(d) {
            self.anchors.insert(d, mate);
        }
    }

    fn mate_of(&self, d: Dock) -> Option<Dock> {
        let path = self.find(d)?;
        let leaf = self.tree.leaf(egui_stack::NodePath { surface: path.surface, node: path.node }).ok()?;
        leaf.tabs().iter().copied().find(|t| *t != d)
    }

    /// Snapshot of every dock's panel-mate (taken before a frame, so that a
    /// dock closed from its tab still knows where it lived).
    pub fn mates(&self) -> HashMap<Dock, Dock> {
        let open: Vec<Dock> = self.tree.iter_all_tabs().map(|(_, d)| *d).collect();
        open.into_iter().filter_map(|d| self.mate_of(d).map(|m| (d, m))).collect()
    }

    pub fn set_anchor(&mut self, d: Dock, mate: Dock) {
        self.anchors.insert(d, mate);
    }

    pub fn close(&mut self, d: Dock) {
        if !d.closeable() {
            return;
        }
        self.remferrous_anchor(d);
        if let Some(p) = self.find(d) {
            self.tree.remove_tab(p);
        }
    }

    /// Show a dock: back in the panel it was closed from if that panel still
    /// exists, otherwise as a new column (or a new row for wide docks).
    pub fn open(&mut self, d: Dock) {
        if let Some(p) = self.find(d) {
            let _ = self.tree.set_active_tab(p);
            return;
        }
        if let Some(anchor) = self.anchors.get(&d).copied() {
            if let Some(p) = self.find(anchor) {
                if let Ok(leaf) = self.tree.leaf_mut(egui_stack::NodePath { surface: p.surface, node: p.node }) {
                    leaf.append_tab(d);
                    if let Some(p) = self.find(d) {
                        let _ = self.tree.set_active_tab(p);
                    }
                    return;
                }
            }
        }
        let t = self.tree.main_surface_mut();
        if t.is_empty() {
            *t = egui_stack::Tree::new(vec![d]);
        } else if d.wide() {
            if d == Dock::Vertical {
                // Next to the preview, so both canvases are visible together.
                if let Some((node, _)) = t.find_tab(&Dock::Preview) {
                    t.split_right(node, 0.66, vec![d]);
                    return;
                }
            }
            t.split_below(NodeIndex::root(), 0.70, vec![d]);
        } else {
            t.split_right(NodeIndex::root(), 0.80, vec![d]);
        }
    }

    /// Pull a dock out into its own floating window.
    pub fn float(&mut self, d: Dock, at: egui::Pos2) {
        if self.is_floating(d) {
            return;
        }
        self.remferrous_anchor(d);
        if let Some(p) = self.find(d) {
            self.tree.remove_tab(p);
        }
        let size = if d.wide() { egui::vec2(640.0, 380.0) } else { egui::vec2(340.0, 380.0) };
        let s = self.tree.add_window(vec![d]);
        if let Some(w) = self.tree.get_window_state_mut(s) {
            w.set_position(at).set_size(size);
        }
    }

    /// Put a floating dock back into the main window.
    pub fn dock_back(&mut self, d: Dock) {
        if !self.is_floating(d) {
            return;
        }
        if let Some(p) = self.find(d) {
            self.tree.remove_tab(p);
        }
        self.open(d);
    }

    /// The preview must always exist somewhere; a layout that lost it (an old
    /// or hand-edited file) gets it back.
    pub fn ensure_preview(&mut self) {
        if !self.is_open(Dock::Preview) {
            let t = self.tree.main_surface_mut();
            if t.is_empty() {
                *t = egui_stack::Tree::new(vec![Dock::Preview]);
            } else {
                t.split_left(NodeIndex::root(), 0.6, vec![Dock::Preview]);
            }
        }
    }

    #[cfg(test)]
    pub fn window_count(&self) -> usize {
        self.tree.iter_surfaces_indexed().filter(|(i, s)| !i.is_main() && s.iter_all_tabs().next().is_some()).count()
    }
}

/// Copy the layout fields of `old` into `new` (used when another window
/// hands back a whole Settings it copied before the layout changed).
pub fn keep_layout(old: &UiSettings, new: &mut UiSettings) {
    new.layout = old.layout.clone();
    new.saved_layouts = old.saved_layouts.clone();
    new.snap_guides = old.snap_guides;
    new.lock_layout = old.lock_layout;
    new.custom_docks = old.custom_docks.clone();
    new.preview_enabled = old.preview_enabled;
    new.preview_max_fps = old.preview_max_fps;
    new.layout_rev = old.layout_rev;
    new.simple_mode = old.simple_mode;
    new.advanced_layout = old.advanced_layout.clone();
    new.tour_done = old.tour_done;
}

/// RON rather than JSON: panel rects that haven't been laid out yet are
/// infinite, which JSON can't represent.
pub fn serialize(tree: &DockState<Dock>) -> String {
    ron::to_string(tree).unwrap_or_default()
}

pub fn parse(s: &str) -> Option<DockState<Dock>> {
    ron::from_str(s).ok()
}

// -------------------------------------------------------------------- style

/// Dock chrome matched to the Ferrous theme, with accent-coloured snap guides.
pub fn style(egui_style: &egui::Style, cfg: &UiSettings) -> Style {
    let mut s = Style::from_egui(egui_style);
    let accent = theme::accent();
    s.dock_area_padding = Some(Margin::same(6));
    s.main_surface_border_stroke = Stroke::NONE;

    s.separator.width = 6.0;
    s.separator.extra_interact_width = 4.0;
    s.separator.color_idle = theme::chrome();
    s.separator.color_hovered = accent.linear_multiply(0.55);
    s.separator.color_dragged = accent;

    s.tab_bar.bg_fill = theme::chrome();
    s.tab_bar.height = 28.0;
    s.tab_bar.hline_color = theme::border();
    s.tab_bar.corner_radius = CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 };

    let tab = |bg: Color32, text: Color32, outline: Color32| egui_stack::TabInteractionStyle {
        outline_color: outline,
        corner_radius: CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 },
        bg_fill: bg,
        text_color: text,
    };
    s.tab.active = tab(theme::tint(theme::panel(), 0.5), theme::text(), theme::border());
    s.tab.focused = tab(theme::tint(theme::panel(), 0.5), Color32::WHITE, accent.linear_multiply(0.8));
    s.tab.hovered = tab(theme::tint(theme::surface(), 0.7), theme::text(), theme::border());
    s.tab.inactive = tab(theme::chrome(), theme::muted(), Color32::TRANSPARENT);
    s.tab.active_with_kb_focus = s.tab.active.clone();
    s.tab.focused_with_kb_focus = s.tab.focused.clone();
    s.tab.inactive_with_kb_focus = s.tab.inactive.clone();
    s.tab.hline_below_active_tab_name = true;
    s.tab.tab_body.inner_margin = Margin::same(8);
    s.tab.tab_body.stroke = Stroke::new(1.0, if theme::glassy() { theme::border().lerp_to_gamma(Color32::WHITE, 0.22) } else { theme::border() });
    let br = if theme::glassy() { 12 } else { 8 };
    s.tab.tab_body.corner_radius = CornerRadius { nw: 0, ne: br, sw: br, se: br };
    s.tab.tab_body.bg_fill = theme::tint(theme::panel(), 0.5);

    s.buttons.close_tab_color = theme::muted();
    s.buttons.close_tab_active_color = theme::live();
    s.buttons.close_tab_bg_fill = theme::surface();

    // Snap guides.
    let o = &mut s.overlay;
    o.overlay_type = match cfg.snap_guides {
        SnapGuides::Zones => OverlayType::Zones,
        SnapGuides::Compass => OverlayType::Widgets,
    };
    o.selection_color = accent.linear_multiply(0.35);
    o.selection_stroke_width = 2.0;
    o.zone_fill = accent.linear_multiply(0.18);
    o.zone_stroke = Stroke::new(2.0, accent);
    o.zone_corner_radius = CornerRadius::same(8);
    o.button_color = theme::surface();
    o.button_border_stroke = Stroke::new(1.0, accent);
    o.hovered_leaf_highlight = egui_stack::LeafHighlighting {
        color: accent.linear_multiply(0.06),
        corner_radius: CornerRadius::same(8),
        stroke: Stroke::new(1.0, accent.linear_multiply(0.6)),
        expansion: 0.0,
    };
    o.surface_fade_opacity = 0.35;
    // A generous edge band: dropping near an edge splits, the middle tabs.
    o.feel.zone_edge_coverage = 0.30;
    o.feel.fade_hold_time = 0.15;
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docks(l: &Layout) -> Vec<Dock> {
        let mut v: Vec<Dock> = l.tree.iter_all_tabs().map(|(_, d)| *d).collect();
        v.sort();
        v
    }

    #[test]
    fn presets_contain_expected_docks() {
        for p in PRESETS {
            let l = Layout::from_preset(p);
            let d = docks(&l);
            assert!(d.contains(&Dock::Preview), "{p:?}");
            assert!(d.contains(&Dock::Scenes) && d.contains(&Dock::Sources) && d.contains(&Dock::Mixer), "{p:?}");
            // No duplicates.
            let mut u = d.clone();
            u.dedup();
            assert_eq!(u, d, "{p:?}");
        }
        assert!(docks(&Layout::from_preset(Preset::Analytics)).contains(&Dock::Performance));
        assert!(docks(&Layout::from_preset(Preset::Vertical)).contains(&Dock::Vertical));
    }

    #[test]
    fn layout_round_trips_through_settings() {
        let mut l = Layout::from_preset(Preset::Classic);
        l.float(Dock::Mixer, egui::pos2(50.0, 60.0));
        let mut ui = UiSettings::default();
        l.store(&mut ui);
        assert!(parse(ui.layout.as_deref().unwrap()).is_some());
        let back = Layout::load(&ui);
        assert_eq!(docks(&back), docks(&l));
        assert!(back.is_floating(Dock::Mixer));
        assert!(!back.is_floating(Dock::Scenes));
    }

    #[test]
    fn closing_and_reopening_returns_to_the_same_panel() {
        let mut l = Layout::from_preset(Preset::Ferrous);
        // Put Stats as a tab next to Sources, then close and reopen it.
        l.open(Dock::Stats);
        l.close(Dock::Stats);
        let src = l.find(Dock::Sources).unwrap();
        let leaf = l.tree.leaf_mut(egui_stack::NodePath { surface: src.surface, node: src.node }).unwrap();
        leaf.append_tab(Dock::Stats);
        l.close(Dock::Stats);
        assert!(!l.is_open(Dock::Stats));
        l.open(Dock::Stats);
        let a = l.find(Dock::Stats).unwrap();
        let b = l.find(Dock::Sources).unwrap();
        assert_eq!((a.surface, a.node), (b.surface, b.node));
    }

    #[test]
    fn float_and_dock_back() {
        let mut l = Layout::from_preset(Preset::Ferrous);
        l.float(Dock::Controls, egui::pos2(10.0, 10.0));
        assert!(l.is_floating(Dock::Controls));
        assert_eq!(l.window_count(), 1);
        l.dock_back(Dock::Controls);
        assert!(l.is_open(Dock::Controls) && !l.is_floating(Dock::Controls));
        assert_eq!(l.window_count(), 0);
    }

    #[test]
    fn preview_cannot_be_closed_and_is_restored() {
        let mut l = Layout::from_preset(Preset::Ferrous);
        l.close(Dock::Preview);
        assert!(l.is_open(Dock::Preview));
        let p = l.find(Dock::Preview).unwrap();
        l.tree.remove_tab(p);
        l.ensure_preview();
        assert!(l.is_open(Dock::Preview));
    }

    #[test]
    fn legacy_dock_switches_are_honoured() {
        let mut ui = UiSettings::default();
        ui.docks.insert("mixer".into(), LegacyDockState::Hidden);
        ui.docks.insert("stats".into(), LegacyDockState::Docked);
        ui.docks.insert("controls".into(), LegacyDockState::Floating);
        let l = Layout::load(&ui);
        assert!(!l.is_open(Dock::Mixer));
        assert!(l.is_open(Dock::Stats));
        assert!(l.is_floating(Dock::Controls));
        assert!(!l.is_open(Dock::Vertical));
    }

    #[test]
    fn corrupt_layout_falls_back_to_default() {
        let ui = UiSettings { layout: Some("{not json".into()), ..Default::default() };
        let l = Layout::load(&ui);
        assert!(l.is_open(Dock::Preview) && l.is_open(Dock::Scenes));
    }

    #[test]
    fn properties_dock_closes_once_for_old_layouts_and_reopening_sticks() {
        // A layout saved before properties moved to the right-click menu.
        let mut ui = UiSettings::default();
        let mut old = Layout::from_preset(Preset::Ferrous);
        old.open(Dock::Properties);
        old.store(&mut ui);
        assert!(Layout::load(&ui).is_open(Dock::Properties));

        let mut l = Layout::load_migrated(&mut ui);
        assert!(!l.is_open(Dock::Properties), "closed by the one-time update");
        assert_eq!(ui.layout_rev, 2);
        assert!(!ui.simple_mode, "an existing, customised layout keeps the full interface");

        // Re-opened by hand: stays open on the next load.
        l.open(Dock::Properties);
        l.store(&mut ui);
        assert!(Layout::load_migrated(&mut ui).is_open(Dock::Properties));
    }

    #[test]
    fn fresh_profiles_start_simple_and_modes_swap_layouts() {
        let mut ui = UiSettings::default();
        let mut l = Layout::load_migrated(&mut ui);
        assert!(ui.simple_mode && ui.layout_rev == 2);
        // Advanced: arrange something, go Simple and back; it comes back.
        l.set_simple(&mut ui, false);
        l.open(Dock::Stats);
        let advanced = serialize(&l.tree);
        l.set_simple(&mut ui, true);
        assert!(!l.is_open(Dock::Stats) && ui.advanced_layout.is_some());
        l.set_simple(&mut ui, false);
        assert!(l.is_open(Dock::Stats));
        assert_eq!(serialize(&l.tree), advanced);
        assert!(ui.advanced_layout.is_none());
    }

    #[test]
    fn presets_leave_properties_to_the_right_click_menu() {
        for p in PRESETS {
            assert!(!Layout::from_preset(p).is_open(Dock::Properties), "{}", p.label());
        }
    }
}
// ------------------------------------------------------------ simple/advanced

impl Layout {
    /// Switch between the Simple and Advanced interfaces. The layout of the
    /// mode being left is parked in `ui.advanced_layout` (Advanced only), and
    /// the other one comes back.
    pub fn set_simple(&mut self, ui: &mut UiSettings, simple: bool) {
        if ui.simple_mode == simple {
            return;
        }
        ui.simple_mode = simple;
        if simple {
            ui.advanced_layout = Some(serialize(&self.tree));
            *self = Self::from_preset(Preset::Simple);
        } else {
            let back = ui.advanced_layout.take().as_deref().and_then(parse);
            self.tree = back.unwrap_or_else(|| Preset::Ferrous.build());
            self.ensure_preview();
        }
    }
}
