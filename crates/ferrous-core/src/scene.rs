use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What a scene item shows. Each variant maps to a source implementation in
/// `ferrous-capture`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceConfig {
    /// Capture a whole monitor via Windows Graphics Capture.
    DisplayCapture { monitor_index: usize, capture_cursor: bool },
    /// Capture a single window (matched by a substring of its title).
    WindowCapture { title_contains: String, capture_cursor: bool },
    /// Follows a game: the full-screen (or borderless) app in front, or the
    /// named program (`"game.exe"`; empty = detect automatically). Uses the
    /// same low-overhead Windows capture as the other sources, with no code
    /// injected into the game, so anti-cheat leaves it alone.
    GameCapture { program: String, capture_cursor: bool },
    /// A still image from disk (PNG, JPEG, BMP, GIF, WebP).
    Image { path: PathBuf },
    /// A solid color rectangle.
    Color { bgra: [u8; 4], width: u32, height: u32 },
    /// Animated bars, handy for testing outputs without capture permissions.
    TestPattern { width: u32, height: u32 },
    /// Webcam or capture card (DirectShow on Windows).
    VideoDevice {
        device: String,
        #[serde(default)]
        resolution: Option<[u32; 2]>,
        #[serde(default)]
        fps: Option<u32>,
        /// Optional DirectShow audio device captured alongside (e.g. a capture card's audio).
        #[serde(default)]
        audio_device: Option<String>,
    },
    /// Text, optionally read from a file (re-read every second, for
    /// now-playing info, counters and the like).
    Text {
        text: String,
        #[serde(default)]
        from_file: Option<PathBuf>,
        /// Font file path or a font file name in the system fonts folder. Empty = default.
        #[serde(default)]
        font: String,
        size: f32,
        bgra: [u8; 4],
        /// Outline colour and width in pixels (0 = none).
        #[serde(default)]
        outline: ([u8; 4], u32),
        /// Background colour (alpha 0 = transparent).
        #[serde(default)]
        background: [u8; 4],
        #[serde(default)]
        align: TextAlign,
        /// Wrap long lines at this width (0 = no wrapping).
        #[serde(default)]
        wrap_width: u32,
    },
    /// Another scene used as a source (nested scene).
    Scene { name: String },
    /// Shows another item's live picture without capturing it again. The
    /// clone has its own placement, crop and filters; `source` is the
    /// original item's id.
    Clone { source: u64 },
    /// A web page (overlays, alerts, chat widgets) rendered by Chromium/Edge.
    Browser {
        url: String,
        width: u32,
        height: u32,
        #[serde(default)]
        fps: u32,
        /// CSS injected into the page, e.g. to hide backgrounds.
        #[serde(default)]
        css: String,
    },
    /// Video/audio received from another Ferrous over Ferrous Link.
    /// `sender` is a discovered sender's name or a `host:port` address.
    LinkReceiver { sender: String, #[serde(default)] pin: String },
    /// A source provided by a plugin DLL. `settings` is a JSON object.
    Plugin { plugin: String, #[serde(default)] settings: String },
    /// A source from an OBS plugin, run on the real libobs in a helper
    /// process. `type_id` is OBS's source id; `settings` its JSON settings.
    ObsPlugin { type_id: String, #[serde(default)] name: String, #[serde(default)] settings: String },
    /// Audio from one application (WASAPI process loopback, Windows 10 2004+).
    ApplicationAudio { process: String },
    /// A video/audio file or network URL (HLS, RTMP, SRT…), decoded by FFmpeg.
    Media {
        input: String,
        looping: bool,
        #[serde(default = "yes")]
        hw_decode: bool,
    },
}

fn yes() -> bool {
    true
}

impl SourceConfig {
    pub fn kind_label(&self) -> &'static str {
        match self {
            SourceConfig::DisplayCapture { .. } => "Display Capture",
            SourceConfig::WindowCapture { .. } => "Window Capture",
            SourceConfig::GameCapture { .. } => "Game Capture",
            SourceConfig::Image { .. } => "Image",
            SourceConfig::Color { .. } => "Color",
            SourceConfig::TestPattern { .. } => "Test Pattern",
            SourceConfig::VideoDevice { .. } => "Video Capture Device",
            SourceConfig::Media { .. } => "Media Source",
            SourceConfig::ApplicationAudio { .. } => "Application Audio",
            SourceConfig::Text { .. } => "Text",
            SourceConfig::Scene { .. } => "Scene",
            SourceConfig::Clone { .. } => "Source Clone",
            SourceConfig::Browser { .. } => "Browser",
            SourceConfig::Plugin { .. } => "Plugin Source",
            SourceConfig::ObsPlugin { .. } => "OBS Plugin Source",
            SourceConfig::LinkReceiver { .. } => "Ferrous Link",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// What dragging a canvas handle does (see [`Transform::drag_handle`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HandleDrag {
    /// Resize. `free`: corners don't keep the aspect ratio (Shift held).
    Scale { free: bool },
    /// Crop the dragged sides (Alt held).
    Crop,
}

/// Placement of a source on the canvas, in canvas pixels.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub x: f32,
    pub y: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    /// Crop in source pixels: left, top, right, bottom.
    #[serde(default)]
    pub crop: [u32; 4],
    /// 0.0 to 1.0.
    #[serde(default = "one")]
    pub opacity: f32,
    /// Degrees clockwise, about the centre of the scaled, cropped box.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rotation: f32,
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

fn one() -> f32 {
    1.0
}

impl Default for Transform {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0, scale_x: 1.0, scale_y: 1.0, crop: [0; 4], opacity: 1.0, rotation: 0.0 }
    }
}

impl Transform {
    /// Size of the source on the canvas once cropped and scaled.
    pub fn output_size(&self, src_w: u32, src_h: u32) -> (f32, f32) {
        let w = src_w.saturating_sub(self.crop[0] + self.crop[2]) as f32;
        let h = src_h.saturating_sub(self.crop[1] + self.crop[3]) as f32;
        (w * self.scale_x, h * self.scale_y)
    }

    /// Whether the box is rotated by anything but a whole number of turns.
    pub fn is_rotated(&self) -> bool {
        self.rotation.rem_euclid(360.0).abs() > 1e-4 && (self.rotation.rem_euclid(360.0) - 360.0).abs() > 1e-4
    }

    /// Rotation as (cos, sin), clockwise in canvas (y-down) coordinates.
    pub fn rotation_cs(&self) -> (f32, f32) {
        let r = self.rotation.to_radians();
        (r.cos(), r.sin())
    }

    /// Centre of the box on the canvas.
    pub fn center(&self, src_w: u32, src_h: u32) -> (f32, f32) {
        let (w, h) = self.output_size(src_w, src_h);
        (self.x + w / 2.0, self.y + h / 2.0)
    }

    /// The four corners on the canvas (top-left, top-right, bottom-right,
    /// bottom-left of the unrotated box), after rotation.
    pub fn corners(&self, src_w: u32, src_h: u32) -> [(f32, f32); 4] {
        let (w, h) = self.output_size(src_w, src_h);
        let (cx, cy) = (self.x + w / 2.0, self.y + h / 2.0);
        let (c, s) = self.rotation_cs();
        [(-w / 2.0, -h / 2.0), (w / 2.0, -h / 2.0), (w / 2.0, h / 2.0), (-w / 2.0, h / 2.0)].map(|(lx, ly)| (cx + lx * c - ly * s, cy + lx * s + ly * c))
    }

    /// Canvas position of a point given in the box's own frame, as a
    /// fraction of its size: (0,0) top-left … (1,1) bottom-right.
    pub fn point(&self, src_w: u32, src_h: u32, fx: f32, fy: f32) -> (f32, f32) {
        let (w, h) = self.output_size(src_w, src_h);
        let (cx, cy) = (self.x + w / 2.0, self.y + h / 2.0);
        let (c, s) = self.rotation_cs();
        let (lx, ly) = ((fx - 0.5) * w, (fy - 0.5) * h);
        (cx + lx * c - ly * s, cy + lx * s + ly * c)
    }

    /// Dragging one of the box's eight handles, OBS-style. `handle` is the
    /// handle's place in the box's frame (each coordinate 0, 0.5 or 1;
    /// (1,1) is the bottom-right corner) and `delta` the pointer movement
    /// in canvas pixels. The opposite handle stays where it is on the canvas.
    ///
    /// - `HandleDrag::Scale`: every handle resizes keeping the item's shape
    ///   (its current width-to-height ratio, even if it was stretched
    ///   earlier); with `free` (Shift) corners and edges stretch freely.
    /// - `HandleDrag::Crop` (Alt): the dragged sides are cropped (or
    ///   uncropped) instead, with the picture itself staying in place.
    pub fn drag_handle(&self, src_w: u32, src_h: u32, handle: (f32, f32), delta: (f32, f32), how: HandleDrag) -> Transform {
        let mut t = *self;
        let (w, h) = self.output_size(src_w, src_h);
        if w <= 0.0 || h <= 0.0 {
            return t;
        }
        let (c, s) = self.rotation_cs();
        // Pointer movement in the box's own (unrotated) axes.
        let (dx, dy) = (delta.0 * c + delta.1 * s, -delta.0 * s + delta.1 * c);
        // How each side moves: +1 for the right/bottom side, -1 for left/top.
        let sx = if handle.0 >= 0.99 { 1.0 } else if handle.0 <= 0.01 { -1.0 } else { 0.0 };
        let sy = if handle.1 >= 0.99 { 1.0 } else if handle.1 <= 0.01 { -1.0 } else { 0.0 };
        let anchor_f = (1.0 - handle.0, 1.0 - handle.1);
        let anchor = self.point(src_w, src_h, anchor_f.0, anchor_f.1);
        match how {
            HandleDrag::Scale { free } => {
                let mut fx = if sx != 0.0 { ((w + sx * dx) / w).max(0.02) } else { 1.0 };
                let mut fy = if sy != 0.0 { ((h + sy * dy) / h).max(0.02) } else { 1.0 };
                if !free {
                    // Follow whichever axis moved more (an edge only has
                    // one) and scale both by it, so the shape is kept.
                    let f = if (fx - 1.0).abs() >= (fy - 1.0).abs() { fx } else { fy };
                    fx = f;
                    fy = f;
                }
                t.scale_x = self.scale_x * fx;
                t.scale_y = self.scale_y * fy;
            }
            HandleDrag::Crop => {
                let (cw, ch) = ((dx / self.scale_x).round() as i64, (dy / self.scale_y).round() as i64);
                let [mut l, mut tp, mut r, mut b] = self.crop.map(i64::from);
                // Dragging a side inwards crops it; outwards gives pixels back.
                if sx < 0.0 { l += cw } else if sx > 0.0 { r -= cw }
                if sy < 0.0 { tp += ch } else if sy > 0.0 { b -= ch }
                let (sw, sh) = (src_w as i64, src_h as i64);
                l = l.clamp(0, sw - 1);
                r = r.clamp(0, sw - 1 - l);
                tp = tp.clamp(0, sh - 1);
                b = b.clamp(0, sh - 1 - tp);
                t.crop = [l as u32, tp as u32, r as u32, b as u32];
            }
        }
        // Put the anchor back where it was.
        let moved = t.point(src_w, src_h, anchor_f.0, anchor_f.1);
        t.x += anchor.0 - moved.0;
        t.y += anchor.1 - moved.1;
        t
    }

    /// Axis-aligned bounds of the rotated box: (x0, y0, x1, y1).
    pub fn bounds(&self, src_w: u32, src_h: u32) -> (f32, f32, f32, f32) {
        let k = self.corners(src_w, src_h);
        let xs = k.map(|p| p.0);
        let ys = k.map(|p| p.1);
        (
            xs.iter().cloned().fold(f32::INFINITY, f32::min),
            ys.iter().cloned().fold(f32::INFINITY, f32::min),
            xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
            ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
        )
    }

    /// Canvas point to the box's own unrotated coordinates (0,0 = its
    /// top-left, w,h = bottom-right).
    pub fn to_local(&self, src_w: u32, src_h: u32, px: f32, py: f32) -> (f32, f32) {
        let (w, h) = self.output_size(src_w, src_h);
        let (cx, cy) = (self.x + w / 2.0, self.y + h / 2.0);
        let (c, s) = self.rotation_cs();
        let (dx, dy) = (px - cx, py - cy);
        (dx * c + dy * s + w / 2.0, -dx * s + dy * c + h / 2.0)
    }

    /// Whether a canvas point lies on the (rotated) box.
    pub fn contains(&self, src_w: u32, src_h: u32, px: f32, py: f32) -> bool {
        let (w, h) = self.output_size(src_w, src_h);
        let (lx, ly) = self.to_local(src_w, src_h, px, py);
        lx >= 0.0 && ly >= 0.0 && lx <= w && ly <= h
    }

    /// Scale uniformly so a `src_w x src_h` source fits inside the canvas,
    /// centered (OBS's "Fit to screen").
    pub fn fit_to(src_w: u32, src_h: u32, canvas_w: u32, canvas_h: u32) -> Self {
        if src_w == 0 || src_h == 0 {
            return Self::default();
        }
        let s = (canvas_w as f32 / src_w as f32).min(canvas_h as f32 / src_h as f32);
        let w = src_w as f32 * s;
        let h = src_h as f32 * s;
        Self {
            x: (canvas_w as f32 - w) / 2.0,
            y: (canvas_h as f32 - h) / 2.0,
            scale_x: s,
            scale_y: s,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneItem {
    /// Unique within the collection; sources are cached by id so two scenes
    /// can't accidentally share a capture session.
    pub id: u64,
    pub name: String,
    pub source: SourceConfig,
    pub visible: bool,
    #[serde(default)]
    pub locked: bool,
    /// Keep the source running while it isn't on screen (a hidden item, or
    /// one in a scene that isn't showing). Off, it stops after a short
    /// grace period and starts again when shown, like OBS's "shutdown
    /// source when not visible"; on, it never sleeps (a webcam that has to
    /// appear instantly, an overlay that keeps its state).
    #[serde(default)]
    pub keep_running: bool,
    pub transform: Transform,
    #[serde(default)]
    pub video_filters: crate::filters::VideoFilters,
    /// Applies to the source's own audio channel, if it has one.
    #[serde(default)]
    pub audio_filters: crate::filters::AudioFilters,
    /// Tracks (bit 0 = track 1) the source's audio is mixed into.
    #[serde(default = "track1")]
    pub audio_tracks: u32,
    /// 0 off, 1 monitor only, 2 monitor and output.
    #[serde(default)]
    pub audio_monitor: u8,
    /// Mixer fader (linear gain, 0 to 2) and mute for the source's audio.
    #[serde(default = "unity")]
    pub audio_volume: f32,
    #[serde(default)]
    pub audio_muted: bool,
    /// Sync offset: holds the source's audio back this many milliseconds.
    #[serde(default)]
    pub audio_sync_ms: u32,
    /// Layout imported from OBS that needs the source's size to resolve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_layout: Option<PendingLayout>,
}

/// OBS-style placement: `transform.x/y` is the item's alignment point, and
/// an optional bounding box scales the source. Resolved once the source's
/// native size is known.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingLayout {
    /// OBS alignment flags (left 1, right 2, top 4, bottom 8; 0 = centre).
    pub align: u32,
    /// Bounds width, height, OBS bounds type, bounds alignment.
    pub bounds: Option<(f32, f32, u32, u32)>,
}

fn align_frac(align: u32) -> (f32, f32) {
    let fx = if align & 1 != 0 { 0.0 } else if align & 2 != 0 { 1.0 } else { 0.5 };
    let fy = if align & 4 != 0 { 0.0 } else if align & 8 != 0 { 1.0 } else { 0.5 };
    (fx, fy)
}

impl PendingLayout {
    /// Final top-left based transform for a source of `src_w x src_h`.
    pub fn resolve(&self, t: &Transform, src_w: u32, src_h: u32) -> Transform {
        let w = src_w.saturating_sub(t.crop[0] + t.crop[2]).max(1) as f32;
        let h = src_h.saturating_sub(t.crop[1] + t.crop[3]).max(1) as f32;
        let mut out = *t;
        let (ax, ay) = align_frac(self.align);
        match self.bounds {
            Some((bw, bh, kind, balign)) if kind != 0 => {
                let (sx, sy) = match kind {
                    1 => (bw / w, bh / h),                                   // stretch
                    2 => { let s = (bw / w).min(bh / h); (s, s) }           // scale inner
                    3 => { let s = (bw / w).max(bh / h); (s, s) }           // scale outer
                    4 => (bw / w, bw / w),                                   // to width
                    5 => (bh / h, bh / h),                                   // to height
                    _ => { let s = (bw / w).min(bh / h).min(1.0); (s, s) }  // max size only
                };
                let (bx, by) = (t.x - bw * ax, t.y - bh * ay);
                let (cw, ch) = (w * sx, h * sy);
                let (fx, fy) = align_frac(balign);
                out.scale_x = sx;
                out.scale_y = sy;
                out.x = bx + (bw - cw) * fx;
                out.y = by + (bh - ch) * fy;
            }
            _ => {
                out.x = t.x - w * t.scale_x * ax;
                out.y = t.y - h * t.scale_y * ay;
            }
        }
        // OBS rotates about the item's position (its alignment point);
        // Ferrous rotates about the box's centre: move the box so it lands in
        // the same place.
        if t.rotation != 0.0 {
            let (bw, bh) = (w * out.scale_x, h * out.scale_y);
            let (px, py) = (t.x, t.y);
            let (c0x, c0y) = (out.x + bw / 2.0, out.y + bh / 2.0);
            let (c, s) = t.rotation_cs();
            let (dx, dy) = (c0x - px, c0y - py);
            let (cx, cy) = (px + dx * c - dy * s, py + dx * s + dy * c);
            out.x = cx - bw / 2.0;
            out.y = cy - bh / 2.0;
        }
        out
    }
}

fn unity() -> f32 {
    1.0
}

fn track1() -> u32 {
    1
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub name: String,
    /// Bottom-most first (drawn first), like OBS's list reversed.
    pub items: Vec<SceneItem>,
    /// Layout of the same items on the vertical (9:16) canvas. Items without
    /// an entry are fitted to the canvas width automatically.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vertical: Vec<VerticalPlacement>,
    /// Used instead of the collection's transition when this scene goes live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<Transition>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerticalPlacement {
    pub item_id: u64,
    pub visible: bool,
    pub transform: Transform,
}

impl Scene {
    /// Vertical-canvas transform and visibility for an item. Items without
    /// their own placement keep the main layout, scaled to the vertical
    /// width and centred, so the composition looks right until customised.
    pub fn vertical_layout(&self, item: &SceneItem, canvas: (u32, u32), main: (u32, u32)) -> (Transform, bool) {
        match self.vertical.iter().find(|p| p.item_id == item.id) {
            Some(p) => (p.transform, p.visible),
            None => {
                let s = canvas.0 as f32 / main.0.max(1) as f32;
                let dy = (canvas.1 as f32 - main.1 as f32 * s) / 2.0;
                let t = item.transform;
                (Transform { x: t.x * s, y: t.y * s + dy, scale_x: t.scale_x * s, scale_y: t.scale_y * s, ..t }, item.visible)
            }
        }
    }

    /// Mutable placement, created from the automatic layout on first use.
    pub fn vertical_placement_mut(&mut self, item_id: u64, canvas: (u32, u32), main: (u32, u32)) -> Option<&mut VerticalPlacement> {
        if !self.vertical.iter().any(|p| p.item_id == item_id) {
            let item = self.items.iter().find(|i| i.id == item_id)?;
            let (transform, visible) = self.vertical_layout(item, canvas, main);
            self.vertical.push(VerticalPlacement { item_id, visible, transform });
        }
        self.vertical.iter_mut().find(|p| p.item_id == item_id)
    }
}

impl Scene {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), items: Vec::new(), vertical: Vec::new(), transition: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransitionKind {
    Cut,
    Fade,
    FadeToColor { bgra: [u8; 4] },
    /// Incoming scene slides over the outgoing one.
    Swipe { direction: TransitionDirection },
    /// Incoming scene pushes the outgoing one out.
    Slide { direction: TransitionDirection },
    /// A video (ideally with alpha, e.g. WebM VP9) played over the switch;
    /// the scene cuts at `cut_point_ms`.
    Stinger { path: PathBuf, cut_point_ms: u32 },
    /// Track-matte stinger: a video's brightness decides where the incoming
    /// scene shows (white = incoming, black = outgoing) while it plays, so a
    /// shape or effect can reveal the new scene. With a layout other than
    /// `Luma`, the video also carries a picture half that is drawn on top.
    Matte {
        path: PathBuf,
        layout: MatteLayout,
        invert: bool,
    },
    /// Luma wipe: a greyscale map decides when each pixel switches (dark
    /// first). `wipe` names a built-in map (OBS's names, e.g. "clock");
    /// `image` overrides it with a picture of your own.
    Luma {
        wipe: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<PathBuf>,
        /// Width of the blend band, 0 (hard edge) to 1.
        softness: f32,
        invert: bool,
    },
}

/// How a track-matte video is laid out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatteLayout {
    /// The whole video is the matte (a pre-rendered greyscale matte).
    #[default]
    Luma,
    /// Picture on the left, matte on the right (video twice as wide).
    Horizontal,
    /// Picture on top, matte below (video twice as tall).
    Vertical,
}

impl MatteLayout {
    pub const ALL: [MatteLayout; 3] = [Self::Luma, Self::Horizontal, Self::Vertical];

    pub fn label(self) -> &'static str {
        match self {
            Self::Luma => "Matte only",
            Self::Horizontal => "Picture | matte",
            Self::Vertical => "Picture over matte",
        }
    }
}

impl TransitionKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Cut => "Cut",
            Self::Fade => "Fade",
            Self::FadeToColor { .. } => "Fade to Color",
            Self::Swipe { .. } => "Swipe",
            Self::Slide { .. } => "Slide",
            Self::Stinger { .. } => "Stinger",
            Self::Matte { .. } => "Track Matte",
            Self::Luma { .. } => "Luma Wipe",
        }
    }

    pub fn all() -> Vec<TransitionKind> {
        vec![
            Self::Cut,
            Self::Fade,
            Self::FadeToColor { bgra: [0, 0, 0, 255] },
            Self::Swipe { direction: TransitionDirection::Left },
            Self::Slide { direction: TransitionDirection::Left },
            Self::Stinger { path: PathBuf::new(), cut_point_ms: 500 },
            Self::Matte { path: PathBuf::new(), layout: MatteLayout::Luma, invert: false },
            Self::Luma { wipe: "linear-h".into(), image: None, softness: 0.08, invert: false },
        ]
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub kind: TransitionKind,
    pub duration_ms: u32,
}

impl Default for Transition {
    fn default() -> Self {
        Self { kind: TransitionKind::Fade, duration_ms: 300 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneCollection {
    pub scenes: Vec<Scene>,
    /// The program (live) scene.
    #[serde(default)]
    pub current: usize,
    #[serde(default)]
    next_id: u64,
    /// Used whenever the program scene changes.
    #[serde(default)]
    pub transition: Transition,
}

impl Default for SceneCollection {
    fn default() -> Self {
        let mut c = Self { scenes: vec![Scene::new("Scene")], current: 0, next_id: 1, transition: Transition::default() };
        let id = c.alloc_id();
        c.scenes[0].items.push(SceneItem {
            id,
            name: "Display Capture".into(),
            source: SourceConfig::DisplayCapture { monitor_index: 0, capture_cursor: true },
            visible: true,
            locked: false,
    keep_running: false,
            transform: Transform::default(),
            video_filters: Vec::new(),
            audio_filters: Vec::new(),
            audio_tracks: 1,
            audio_monitor: 0,
            audio_volume: 1.0,
            audio_muted: false,
            audio_sync_ms: 0,
            pending_layout: None,
        });
        c
    }
}

impl SceneCollection {
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn current_scene(&self) -> &Scene {
        &self.scenes[self.current.min(self.scenes.len() - 1)]
    }

    pub fn current_scene_mut(&mut self) -> &mut Scene {
        let i = self.current.min(self.scenes.len() - 1);
        &mut self.scenes[i]
    }

    pub fn add_scene(&mut self, name: impl Into<String>) -> usize {
        self.scenes.push(Scene::new(name));
        self.scenes.len() - 1
    }

    /// Removes a scene, keeping at least one.
    pub fn remove_scene(&mut self, index: usize) {
        if self.scenes.len() <= 1 || index >= self.scenes.len() {
            return;
        }
        self.scenes.remove(index);
        if self.current >= self.scenes.len() {
            self.current = self.scenes.len() - 1;
        }
    }

    /// Where `id` lives: (scene index, item index).
    fn locate(&self, id: u64) -> Option<(usize, usize)> {
        self.scenes.iter().enumerate().find_map(|(si, sc)| sc.items.iter().position(|i| i.id == id).map(|ii| (si, ii)))
    }

    /// The source a clone of `id` should point at: `id` itself, or, when
    /// `id` is already a clone, what it clones (clones of clones show nothing).
    fn clone_target(&self, id: u64) -> Option<u64> {
        let (si, ii) = self.locate(id)?;
        Some(match &self.scenes[si].items[ii].source {
            SourceConfig::Clone { source } => *source,
            _ => id,
        })
    }

    /// Adds, right above `id` in `scene`, a Source Clone showing the same
    /// picture with the same layout. Duplicating without opening the
    /// capture a second time (a webcam can't be opened twice).
    pub fn duplicate_item(&mut self, scene: usize, id: u64) -> Option<u64> {
        let target = self.clone_target(id)?;
        let scene = scene.min(self.scenes.len() - 1);
        let at = self.scenes[scene].items.iter().position(|i| i.id == id)?;
        let new_id = self.alloc_id();
        let src = &self.scenes[scene].items[at];
        let mut item = src.clone();
        item.id = new_id;
        item.name = format!("{} copy", src.name);
        item.source = SourceConfig::Clone { source: target };
        item.video_filters = Default::default();
        item.audio_filters = Default::default();
        item.audio_muted = true;
        item.pending_layout = None;
        self.scenes[scene].items.insert(at + 1, item);
        if let Some(p) = self.scenes[scene].vertical.iter().find(|p| p.item_id == id).cloned() {
            self.scenes[scene].vertical.push(VerticalPlacement { item_id: new_id, ..p });
        }
        Some(new_id)
    }

    /// Copies a scene with its layout. Captures (screens, cameras, browsers,
    /// plugins) become clones of the originals so nothing is opened twice;
    /// images, colours, text and the like are copied outright.
    pub fn duplicate_scene(&mut self, index: usize) -> Option<usize> {
        let mut copy = self.scenes.get(index)?.clone();
        copy.name = format!("{} copy", copy.name);
        let mut remap = Vec::new();
        for item in &mut copy.items {
            let old = item.id;
            item.id = self.alloc_id();
            remap.push((old, item.id));
            let live = matches!(
                item.source,
                SourceConfig::DisplayCapture { .. }
                    | SourceConfig::WindowCapture { .. }
                    | SourceConfig::GameCapture { .. }
                    | SourceConfig::VideoDevice { .. }
                    | SourceConfig::Browser { .. }
                    | SourceConfig::LinkReceiver { .. }
                    | SourceConfig::Plugin { .. }
                    | SourceConfig::ObsPlugin { .. }
            );
            if live {
                item.source = SourceConfig::Clone { source: old };
                item.audio_muted = true;
            }
        }
        for p in &mut copy.vertical {
            if let Some((_, new)) = remap.iter().find(|(o, _)| *o == p.item_id) {
                p.item_id = *new;
            }
        }
        let at = index + 1;
        self.scenes.insert(at, copy);
        if self.current >= at {
            self.current += 1;
        }
        Some(at)
    }

    /// Moves a scene up (-1) or down (+1) the list; returns its new index.
    pub fn move_scene(&mut self, index: usize, delta: isize) -> Option<usize> {
        let to = index.checked_add_signed(delta).filter(|t| *t < self.scenes.len() && index < self.scenes.len())?;
        let was_current = self.current == index;
        let other_current = self.current == to;
        self.scenes.swap(index, to);
        if was_current {
            self.current = to;
        } else if other_current {
            self.current = index;
        }
        Some(to)
    }

    pub fn add_item(&mut self, name: impl Into<String>, source: SourceConfig) -> u64 {
        let cur = self.current;
        self.add_item_to(cur, name, source)
    }

    pub fn add_item_to(&mut self, scene: usize, name: impl Into<String>, source: SourceConfig) -> u64 {
        let id = self.alloc_id();
        let scene = scene.min(self.scenes.len() - 1);
        self.scenes[scene].items.push(SceneItem {
            id,
            name: name.into(),
            source,
            visible: true,
            locked: false,
            keep_running: false,
            transform: Transform::default(),
            video_filters: Vec::new(),
            audio_filters: Vec::new(),
            audio_tracks: 1,
            audio_monitor: 0,
            audio_volume: 1.0,
            audio_muted: false,
            audio_sync_ms: 0,
            pending_layout: None,
        });
        id
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut c: Self = serde_json::from_str(&text)?;
        if c.scenes.is_empty() {
            c.scenes.push(Scene::new("Scene"));
        }
        // A hand-edited file can point at a scene that isn't there.
        c.current = c.current.min(c.scenes.len() - 1);
        // Repair id counter in case the file was edited by hand.
        let max = c.scenes.iter().flat_map(|s| &s.items).map(|i| i.id).max().unwrap_or(0);
        c.next_id = c.next_id.max(max + 1);
        Ok(c)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn default_path() -> PathBuf {
        crate::paths::config_dir().join("scenes.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_repairs_an_out_of_range_current_scene_and_an_empty_file() {
        let dir = std::env::temp_dir().join("ferrous-scene-load-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = SceneCollection::default();
        c.add_scene("Two");
        c.current = 1;
        c.save(&dir.join("c.json")).unwrap();
        let text = std::fs::read_to_string(dir.join("c.json")).unwrap().replace("\"current\": 1", "\"current\": 40");
        std::fs::write(dir.join("c.json"), text).unwrap();
        let l = SceneCollection::load(&dir.join("c.json")).unwrap();
        assert_eq!(l.current, l.scenes.len() - 1);
        std::fs::write(dir.join("e.json"), "{\"scenes\": [], \"current\": 3}").unwrap();
        let e = SceneCollection::load(&dir.join("e.json")).unwrap();
        assert_eq!((e.scenes.len(), e.current), (1, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_and_ids() {
        let mut c = SceneCollection::default();
        c.add_scene("Gaming");
        c.current = 1;
        let a = c.add_item("Cam", SourceConfig::TestPattern { width: 640, height: 360 });
        let b = c.add_item("Logo", SourceConfig::Color { bgra: [0, 0, 255, 255], width: 10, height: 10 });
        assert_ne!(a, b);
        let json = serde_json::to_string(&c).unwrap();
        let back: SceneCollection = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn fit_to_screen_letterboxes() {
        let t = Transform::fit_to(1280, 1024, 1920, 1080);
        let (w, h) = t.output_size(1280, 1024);
        assert!((h - 1080.0).abs() < 0.01);
        assert!((t.x - (1920.0 - w) / 2.0).abs() < 0.01);
    }

    #[test]
    fn keeps_one_scene() {
        let mut c = SceneCollection::default();
        c.remove_scene(0);
        assert_eq!(c.scenes.len(), 1);
    }
}

#[cfg(test)]
mod rotation_tests {
    use super::*;

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3
    }

    #[test]
    fn corners_rotate_clockwise_about_centre() {
        // 100x50 box at (0,0), centre (50,25), rotated 90° clockwise.
        let t = Transform { rotation: 90.0, ..Default::default() };
        let k = t.corners(100, 50);
        // Top-left (-50,-25) from centre goes to (25,-50) from centre.
        assert!(close(k[0], (75.0, -25.0)), "{:?}", k[0]);
        assert!(close(k[2], (25.0, 75.0)), "{:?}", k[2]);
        let b = t.bounds(100, 50);
        assert!(close((b.0, b.1), (25.0, -25.0)) && close((b.2, b.3), (75.0, 75.0)), "{b:?}");
        assert!(t.contains(100, 50, 50.0, 70.0));
        assert!(!t.contains(100, 50, 90.0, 25.0));
        assert!(!Transform::default().is_rotated());
        assert!(!Transform { rotation: 360.0, ..Default::default() }.is_rotated());
        assert!(t.is_rotated());
    }

    #[test]
    fn local_coordinates_round_trip() {
        let t = Transform { x: 10.0, y: 20.0, scale_x: 2.0, scale_y: 1.5, rotation: 33.0, ..Default::default() };
        for (i, c) in t.corners(40, 30).iter().enumerate() {
            let l = t.to_local(40, 30, c.0, c.1);
            let want = [(0.0, 0.0), (80.0, 0.0), (80.0, 45.0), (0.0, 45.0)][i];
            assert!(close(l, want), "{i}: {l:?}");
        }
    }

    #[test]
    fn obs_rotation_about_the_position_point() {
        // OBS: top-left aligned 200x100 at (500,300) rotated 90°: the box
        // swings clockwise about (500,300), so it hangs down-left of it.
        let pl = PendingLayout { align: 5, bounds: None };
        let t = Transform { x: 500.0, y: 300.0, rotation: 90.0, ..Default::default() };
        let r = pl.resolve(&t, 200, 100);
        let b = r.bounds(200, 100);
        assert!(close((b.0, b.1), (400.0, 300.0)) && close((b.2, b.3), (500.0, 500.0)), "{b:?}");
        // The pivot (OBS's top-left corner) stays put.
        assert!(close(r.corners(200, 100)[0], (500.0, 300.0)));
    }

    #[test]
    fn handles_scale_crop_and_keep_the_opposite_side_put() {
        let t = Transform { x: 100.0, y: 50.0, ..Default::default() };
        let near = |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).abs() < 0.6 && (a.1 - b.1).abs() < 0.6;
        // Bottom-right corner: proportional, top-left stays put.
        let r = t.drag_handle(400, 200, (1.0, 1.0), (40.0, 5.0), HandleDrag::Scale { free: false });
        assert!((r.scale_x - 1.1).abs() < 1e-4 && (r.scale_y - 1.1).abs() < 1e-4);
        assert!(near((r.x, r.y), (100.0, 50.0)));
        // Shift: free aspect.
        let r = t.drag_handle(400, 200, (1.0, 1.0), (40.0, 20.0), HandleDrag::Scale { free: true });
        assert!((r.scale_x - 1.1).abs() < 1e-4 && (r.scale_y - 1.1).abs() < 1e-4);
        let r = t.drag_handle(400, 200, (1.0, 1.0), (40.0, 40.0), HandleDrag::Scale { free: true });
        assert!((r.scale_x - 1.1).abs() < 1e-4 && (r.scale_y - 1.2).abs() < 1e-4);
        // Left edge: keeps the shape too (both scales follow), right edge stays.
        let r = t.drag_handle(400, 200, (0.0, 0.5), (40.0, 0.0), HandleDrag::Scale { free: false });
        assert!((r.scale_x - 0.9).abs() < 1e-4 && (r.scale_y - 0.9).abs() < 1e-4);
        assert!(near(r.point(400, 200, 1.0, 0.5), t.point(400, 200, 1.0, 0.5)));
        // Shift on an edge stretches that axis only.
        let r = t.drag_handle(400, 200, (0.0, 0.5), (40.0, 0.0), HandleDrag::Scale { free: true });
        assert!((r.scale_x - 0.9).abs() < 1e-4 && r.scale_y == 1.0);
        // An item stretched earlier keeps its distortion's ratio, not a square one.
        let stretched = Transform { scale_x: 2.0, scale_y: 1.0, ..t };
        let r = stretched.drag_handle(400, 200, (1.0, 1.0), (80.0, 0.0), HandleDrag::Scale { free: false });
        assert!((r.scale_x / r.scale_y - 2.0).abs() < 1e-4);
        // Alt on the left edge: crop 40 px; the picture doesn't move.
        let r = t.drag_handle(400, 200, (0.0, 0.5), (40.0, 0.0), HandleDrag::Crop);
        assert_eq!(r.crop, [40, 0, 0, 0]);
        assert!(near((r.x, r.y), (140.0, 50.0)), "left edge moved in by the crop");
        assert_eq!(r.output_size(400, 200), (360.0, 200.0));
        // Dragging back out restores it, never below zero.
        let back = r.drag_handle(400, 200, (0.0, 0.5), (-100.0, 0.0), HandleDrag::Crop);
        assert_eq!(back.crop, [0, 0, 0, 0]);
        // Rotated box: the anchor corner stays put.
        let rot = Transform { x: 100.0, y: 50.0, rotation: 30.0, ..Default::default() };
        let r = rot.drag_handle(400, 200, (1.0, 1.0), (30.0, 30.0), HandleDrag::Scale { free: false });
        assert!(near(r.point(400, 200, 0.0, 0.0), rot.point(400, 200, 0.0, 0.0)));
        assert!(r.scale_x > 1.0);
        // Crop on a rotated box: the far side stays put too.
        let r = rot.drag_handle(400, 200, (1.0, 0.5), (-50.0, 0.0), HandleDrag::Crop);
        assert!(r.crop[2] > 0);
        assert!(near(r.point(400, 200, 0.0, 0.5), rot.point(400, 200, 0.0, 0.5)));
    }

    #[test]
    fn duplicating_an_item_clones_it_instead_of_reopening_the_capture() {
        let mut c = SceneCollection::default();
        let cam = c.add_item("Cam", SourceConfig::VideoDevice { device: "x".into(), resolution: None, fps: None, audio_device: None });
        let copy = c.duplicate_item(0, cam).unwrap();
        let it = c.scenes[0].items.iter().find(|i| i.id == copy).unwrap();
        assert_eq!(it.source, SourceConfig::Clone { source: cam });
        assert_eq!(it.name, "Cam copy");
        // A copy of a copy points at the original.
        let again = c.duplicate_item(0, copy).unwrap();
        let it = c.scenes[0].items.iter().find(|i| i.id == again).unwrap();
        assert_eq!(it.source, SourceConfig::Clone { source: cam });
        assert!(c.duplicate_item(0, 9999).is_none());
    }

    #[test]
    fn duplicating_a_scene_keeps_cheap_sources_and_clones_captures() {
        let mut c = SceneCollection::default();
        let n = c.scenes[0].items.len();
        let color = c.add_item("Bg", SourceConfig::Color { bgra: [1, 2, 3, 255], width: 8, height: 8 });
        let disp = c.add_item("Screen", SourceConfig::DisplayCapture { monitor_index: 0, capture_cursor: true });
        let at = c.duplicate_scene(0).unwrap();
        assert_eq!(at, 1);
        assert_eq!(c.scenes.len(), 2);
        let copy = &c.scenes[1];
        assert_eq!(copy.name, format!("{} copy", c.scenes[0].name));
        assert_eq!(copy.items.len(), n + 2);
        let mut ids: Vec<u64> = c.scenes.iter().flat_map(|s| s.items.iter().map(|i| i.id)).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "ids must stay unique across scenes");
        assert!(matches!(copy.items[n].source, SourceConfig::Color { .. }));
        assert_eq!(copy.items[n + 1].source, SourceConfig::Clone { source: disp });
        let _ = color;
    }

    #[test]
    fn moving_a_scene_keeps_the_current_one_current() {
        let mut c = SceneCollection::default();
        c.add_scene("B");
        c.add_scene("C");
        c.current = 0;
        assert_eq!(c.move_scene(0, 1), Some(1));
        assert_eq!(c.current, 1);
        assert_eq!(c.scenes[1].name, "Scene");
        assert_eq!(c.move_scene(2, 1), None);
        assert_eq!(c.move_scene(0, -1), None);
        c.current = 2;
        c.move_scene(0, 1);
        assert_eq!(c.current, 2);
    }
}