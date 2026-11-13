use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use eframe::egui::{self, Color32, CornerRadius, Stroke, Visuals};

const fn rgb(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// Background texture drawn behind the panels.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    None,
    Stars,
    Horizon,
    /// Falling rain that lands on the panels and drips down them.
    Weather,
    /// Weather plus lightning.
    Storm,
    /// Dots plus drifting blossom petals.
    Petals,
}

/// A soft coloured glow behind the panels: centre and radius as fractions of
/// the window, colour, strength (0-255).
pub struct Blob(pub f32, pub f32, pub f32, pub u32, pub u8);

pub struct Palette {
    pub id: &'static str,
    pub name: &'static str,
    pub tagline: &'static str,
    pub bg: Color32,
    pub panel: Color32,
    pub surface: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent2: Color32,
    pub blobs: &'static [Blob],
    pub detail: Detail,
    /// Themes with a backdrop draw panels as glass over it.
    pub glass: bool,
}

pub static PALETTES: &[Palette] = &[
    Palette { id: "ember", name: "Ember", tagline: "the classic charcoal and orange", bg: rgb(0x14151a), panel: rgb(0x1b1d24), surface: rgb(0x242730), border: rgb(0x303440), text: rgb(0xe6e8ee), muted: rgb(0x8b91a1), accent: rgb(0xff7a3d), accent2: rgb(0xff7a3d), blobs: &[], detail: Detail::None, glass: false },
    Palette { id: "amoled", name: "AMOLED Black", tagline: "true black, white accent", bg: rgb(0x000000), panel: rgb(0x08080a), surface: rgb(0x121214), border: rgb(0x26262a), text: rgb(0xf2f2f2), muted: rgb(0x8a8a90), accent: rgb(0xffffff), accent2: rgb(0xc9c9d0), blobs: &[], detail: Detail::None, glass: false },
    Palette { id: "neon_noir", name: "Neon Noir", tagline: "black with hot pink and violet glow", bg: rgb(0x030005), panel: rgb(0x120612), surface: rgb(0x1e0c1f), border: rgb(0x4a1442), text: rgb(0xffe9f6), muted: rgb(0xb57aa6), accent: rgb(0xff2d95), accent2: rgb(0xa43bff), blobs: &[Blob(0.12, 0.15, 0.45, 0x8a0a6a, 150), Blob(0.9, 0.9, 0.5, 0x5a10b0, 150), Blob(0.5, 1.0, 0.4, 0x7a0a50, 90)], detail: Detail::None, glass: true },
    Palette { id: "aurora", name: "Aurora Glass", tagline: "northern lights, green into violet", bg: rgb(0x010508), panel: rgb(0x07131a), surface: rgb(0x0e2028), border: rgb(0x25556a), text: rgb(0xeafff6), muted: rgb(0x7fb5a8), accent: rgb(0x3dffb0), accent2: rgb(0x8a5bff), blobs: &[Blob(0.2, 0.1, 0.5, 0x00a070, 150), Blob(0.7, 0.05, 0.5, 0x6a2cc0, 150), Blob(0.95, 0.85, 0.45, 0x0a6a9a, 120)], detail: Detail::Stars, glass: true },
    Palette { id: "synthwave", name: "Synthwave Horizon", tagline: "retro grid, magenta and cyan", bg: rgb(0x07021a), panel: rgb(0x120a26), surface: rgb(0x1c1038), border: rgb(0x4b1a7d), text: rgb(0xfdeaff), muted: rgb(0xb08ad6), accent: rgb(0xff3df0), accent2: rgb(0x22e4ff), blobs: &[Blob(0.5, 1.05, 0.6, 0xb0108a, 170), Blob(0.1, 0.1, 0.4, 0x4a10a0, 130)], detail: Detail::Horizon, glass: true },
    Palette { id: "sakura", name: "Sakura Night", tagline: "cherry-blossom pink to peach", bg: rgb(0x050204), panel: rgb(0x150a12), surface: rgb(0x22111c), border: rgb(0x5a2a44), text: rgb(0xfff0f6), muted: rgb(0xc795ad), accent: rgb(0xff8ac2), accent2: rgb(0xffb48a), blobs: &[Blob(0.88, 0.1, 0.45, 0x8a2a60, 150), Blob(0.1, 0.9, 0.45, 0x80402a, 130)], detail: Detail::Petals, glass: true },
    Palette { id: "nebula", name: "Nebula", tagline: "starfield, violet and electric blue", bg: rgb(0x02000a), panel: rgb(0x0c0820), surface: rgb(0x150f33), border: rgb(0x352a7a), text: rgb(0xefeaff), muted: rgb(0x9a8fd0), accent: rgb(0x8b5cff), accent2: rgb(0x38a8ff), blobs: &[Blob(0.15, 0.15, 0.5, 0x4a14a0, 160), Blob(0.85, 0.75, 0.55, 0x0a46a8, 160), Blob(0.55, 0.05, 0.3, 0x7a1e70, 120)], detail: Detail::Stars, glass: true },
    Palette { id: "vapor", name: "Vapor Sunset", tagline: "plum with pink-to-orange sunset", bg: rgb(0x0e0220), panel: rgb(0x1a0a30), surface: rgb(0x28123f), border: rgb(0x6a2d7a), text: rgb(0xfff0f8), muted: rgb(0xc79ad6), accent: rgb(0xff5fa8), accent2: rgb(0xffa04d), blobs: &[Blob(0.5, 1.1, 0.7, 0xc0406a, 190), Blob(0.05, 0.05, 0.4, 0x5a14a0, 150), Blob(0.95, 0.9, 0.4, 0xc06a2a, 120)], detail: Detail::None, glass: true },
    Palette { id: "thunderhead", name: "Thunderhead", tagline: "storm slate, lightning blue", bg: rgb(0x04060a), panel: rgb(0x0c111c), surface: rgb(0x141b2c), border: rgb(0x2c3b55), text: rgb(0xeef3ff), muted: rgb(0x8493b3), accent: rgb(0x7aa8ff), accent2: rgb(0xe6f0ff), blobs: &[Blob(0.6, 0.0, 0.55, 0x2a4a9a, 150), Blob(0.05, 0.95, 0.4, 0x1a2a50, 120)], detail: Detail::Storm, glass: true },
    Palette { id: "rainfall", name: "Rainfall", tagline: "steady rain that lands on the panels and drips down the glass", bg: rgb(0x04070b), panel: rgb(0x0b1219), surface: rgb(0x131c27), border: rgb(0x2a3d52), text: rgb(0xe6eef7), muted: rgb(0x7f93a8), accent: rgb(0x5fc8ff), accent2: rgb(0x3a7bd5), blobs: &[Blob(0.5, 0.0, 0.7, 0x1c3f66, 150), Blob(0.0, 1.0, 0.5, 0x10304a, 120), Blob(1.0, 0.9, 0.45, 0x143a58, 110)], detail: Detail::Weather, glass: true },
];

static CURRENT: AtomicUsize = AtomicUsize::new(0);
/// 0 = Off, 1 = Light, 2 = Full.
static GLASS: AtomicU8 = AtomicU8::new(2);
/// Animated backdrops: 0 off, 1 reduced, 2 full.
static MOTION: AtomicU8 = AtomicU8::new(2);

fn p() -> &'static Palette {
    &PALETTES[CURRENT.load(Ordering::Relaxed).min(PALETTES.len() - 1)]
}

/// Picks the theme by id (unknown ids fall back to Ferrous) and glass level
/// (0 off, 1 light, 2 full). Call `apply` afterwards.
pub fn set(id: &str, glass: u8, motion: u8) {
    CURRENT.store(PALETTES.iter().position(|t| t.id == id).unwrap_or(0), Ordering::Relaxed);
    GLASS.store(glass.min(2), Ordering::Relaxed);
    MOTION.store(motion.min(2), Ordering::Relaxed);
}

/// Seconds since start, for animation.
pub fn time() -> f32 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f32()
}

/// Whether Ferrous's window is the one being used. Animations stop while it
/// isn't (a game in front, the window minimised): a streamer's UI is mostly
/// out of focus, and moving backdrops there would be pure waste.
static ACTIVE: AtomicU8 = AtomicU8::new(1);

pub fn set_active(active: bool) {
    ACTIVE.store(u8::from(active), Ordering::Relaxed);
}

fn motion() -> u8 {
    if glassy() && ACTIVE.load(Ordering::Relaxed) == 1 { MOTION.load(Ordering::Relaxed) } else { 0 }
}

/// The panels are glass: the theme has a backdrop and the user hasn't turned it off.
pub fn glassy() -> bool {
    p().glass && GLASS.load(Ordering::Relaxed) > 0
}

fn full_glass() -> bool {
    p().glass && GLASS.load(Ordering::Relaxed) >= 2
}

pub fn bg() -> Color32 {
    p().bg
}
pub fn panel() -> Color32 {
    p().panel
}
pub fn surface() -> Color32 {
    p().surface
}
pub fn border() -> Color32 {
    p().border
}
pub fn text() -> Color32 {
    p().text
}
pub fn muted() -> Color32 {
    p().muted
}
pub fn accent() -> Color32 {
    p().accent
}
pub fn accent2() -> Color32 {
    p().accent2
}
pub fn live() -> Color32 {
    rgb(0xe5484d)
}
pub fn rec() -> Color32 {
    rgb(0xe5484d)
}
pub fn ok() -> Color32 {
    rgb(0x46c97a)
}
pub fn warn() -> Color32 {
    rgb(0xf2c04a)
}

/// Fill for the window chrome (top bar, status bar, dock area): solid, or
/// clear so the backdrop shows through.
pub fn chrome() -> Color32 {
    if glassy() { Color32::TRANSPARENT } else { bg() }
}

/// `c` made see-through on glass themes (opaque on flat ones).
pub fn tint(c: Color32, a: f32) -> Color32 {
    glass_alpha(c, a)
}

fn glass_alpha(c: Color32, a: f32) -> Color32 {
    if glassy() { c.gamma_multiply(a) } else { c }
}

/// Text colour that reads on top of `c`.
pub fn on(c: Color32) -> Color32 {
    let l = 0.299 * f32::from(c.r()) + 0.587 * f32::from(c.g()) + 0.114 * f32::from(c.b());
    if l > 150.0 { Color32::BLACK } else { Color32::WHITE }
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut v = Visuals::dark();
    let r = if glassy() { 12 } else { 8 };
    v.panel_fill = glass_alpha(panel(), 0.5);
    v.window_fill = glass_alpha(panel(), 0.92);
    v.extreme_bg_color = glass_alpha(bg(), 0.7);
    v.faint_bg_color = glass_alpha(surface(), 0.6);
    v.override_text_color = Some(text());
    v.window_stroke = Stroke::new(1.0, if glassy() { border().lerp_to_gamma(Color32::WHITE, 0.25) } else { border() });
    v.window_corner_radius = CornerRadius::same(r + 2);
    v.menu_corner_radius = CornerRadius::same(r);
    v.selection.bg_fill = accent().linear_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, accent());
    v.hyperlink_color = accent();
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(r - 2);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, border());
    v.widgets.inactive.weak_bg_fill = glass_alpha(surface(), 0.8);
    v.widgets.inactive.bg_fill = glass_alpha(surface(), 0.8);
    v.widgets.hovered.weak_bg_fill = surface().lerp_to_gamma(Color32::WHITE, 0.08);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, accent().linear_multiply(0.6));
    v.widgets.active.weak_bg_fill = surface().lerp_to_gamma(Color32::WHITE, 0.14);
    ctx.set_visuals_of(egui::Theme::Dark, v);

    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.spacing.interact_size.y = 26.0;
    });
}

/// Paints the theme's backdrop (glows, stars, grid…) behind everything.
/// Call first each frame; a no-op for flat themes.
pub fn paint_backdrop(ctx: &egui::Context) {
    let t = p();
    if !glassy() {
        return;
    }
    let area = ctx.content_rect();
    let paint = ctx.layer_painter(egui::LayerId::background());
    paint.rect_filled(area, 0.0, t.bg);
    let scale = area.width().max(area.height());
    let strength = if full_glass() { 1.0 } else { 0.6 };
    for b in t.blobs {
        let c = rgb(b.3);
        let drift = if motion() > 0 { let k = time() * 0.12 + b.3 as f32; (k.sin() * 0.025, (k * 0.8).cos() * 0.02) } else { (0.0, 0.0) };
        let centre = egui::pos2(area.left() + (b.0 + drift.0) * area.width(), area.top() + (b.1 + drift.1) * area.height());
        let radius = b.2 * scale;
        let mut mesh = egui::Mesh::default();
        let a = (f32::from(b.4) * 1.5 * strength).min(255.0);
        let col = |alpha: f32| Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha as u8);
        mesh.colored_vertex(centre, col(a));
        // Two rings give a soft shoulder: bright core, long gentle falloff.
        const N: usize = 40;
        for (r_frac, alpha) in [(0.45, a * 0.5), (1.0, 0.0)] {
            for i in 0..N {
                let ang = i as f32 / N as f32 * std::f32::consts::TAU;
                mesh.colored_vertex(centre + egui::vec2(ang.cos(), ang.sin()) * radius * r_frac, col(alpha));
            }
        }
        let n = N as u32;
        for i in 0..n {
            let (a1, a2) = (1 + i, 1 + (i + 1) % n);
            mesh.add_triangle(0, a1, a2);
            mesh.add_triangle(a1, a2, a2 + n);
            mesh.add_triangle(a1, a2 + n, a1 + n);
        }
        paint.add(egui::Shape::mesh(mesh));
    }
    match t.detail {
        Detail::None => {}
        Detail::Stars => {
            for i in 0..140u32 {
                let h = i.wrapping_mul(2654435761).rotate_left(7) ^ i.wrapping_mul(40503);
                let (fx, fy) = ((h & 0xffff) as f32 / 65535.0, ((h >> 16) & 0xffff) as f32 / 65535.0);
                let mut bright = 70 + (h % 160) as u8;
                if motion() > 0 {
                    bright = (f32::from(bright) * (0.55 + 0.45 * (time() * (0.6 + (h % 7) as f32 * 0.25) + (h % 31) as f32).sin())).clamp(0.0, 255.0) as u8;
                }
                paint.circle_filled(egui::pos2(area.left() + fx * area.width(), area.top() + fy * area.height()), if h % 7 == 0 { 1.4 } else { 0.8 }, Color32::from_rgba_unmultiplied(bright, bright, 255, 200));
            }
        }
        Detail::Horizon => {
            let hy = area.top() + area.height() * 0.8;
            let c = t.accent.gamma_multiply(0.28);
            let s = Stroke::new(1.0, c);
            for i in -24..=24 {
                paint.line_segment([egui::pos2(area.center().x + i as f32 * 14.0, hy), egui::pos2(area.center().x + i as f32 * 110.0, area.bottom())], s);
            }
            let phase = if motion() > 0 { (time() * 0.35).fract() } else { 0.0 };
            for k in 0..=9 {
                let u = (k as f32 + phase) / 9.0;
                if u <= 0.0 || u > 1.0 {
                    continue;
                }
                let y = hy + u.powi(2) * (area.bottom() - hy);
                paint.line_segment([egui::pos2(area.left(), y), egui::pos2(area.right(), y)], s);
            }
        }
        Detail::Weather | Detail::Storm => {
            // The rain itself is drawn after the panels (see `paint_weather`).
            if motion() == 0 && t.detail == Detail::Storm {
                let s = Stroke::new(1.0, Color32::from_rgba_unmultiplied(120, 150, 220, 36));
                for i in 0..120u32 {
                    let h = i.wrapping_mul(2246822519).rotate_left(11) ^ i.wrapping_mul(3266489917);
                    let (fx, fy) = ((h & 0xffff) as f32 / 65535.0, ((h >> 16) & 0xffff) as f32 / 65535.0);
                    let p0 = egui::pos2(area.left() + fx * area.width(), area.top() + fy * area.height());
                    paint.line_segment([p0, p0 + egui::vec2(-8.0, 26.0)], s);
                }
            }
        }
        Detail::Petals => {
            if t.detail == Detail::Petals && motion() > 0 {
                let tm = time();
                let n = if motion() >= 2 { 26 } else { 12 };
                for i in 0..n as u32 {
                    let h = i.wrapping_mul(2654435761).rotate_left(5);
                    let (fx, ph, sp) = ((h & 0xffff) as f32 / 65535.0, ((h >> 16) & 0xff) as f32, 22.0 + (h % 17) as f32);
                    let y = (tm * sp + ph * 11.0) % (area.height() + 40.0) - 20.0;
                    let x = area.left() + fx * area.width() + (tm * 0.7 + ph).sin() * 26.0;
                    let a = tm * 1.3 + ph;
                    let (cx, cy) = (a.cos(), a.sin());
                    let r = 3.5 + (h % 4) as f32;
                    let col = Color32::from_rgba_unmultiplied(255, 170 + (h % 60) as u8, 205, 150);
                    let petal: Vec<egui::Pos2> = (0..8)
                        .map(|k| {
                            let th = k as f32 / 8.0 * std::f32::consts::TAU;
                            let (ex, ey) = (th.cos() * r, th.sin() * r * 0.5);
                            egui::pos2(x + ex * cx - ey * cy, area.top() + y + ex * cy + ey * cx)
                        })
                        .collect();
                    paint.add(egui::Shape::convex_polygon(petal, col, Stroke::NONE));
                }
            }
            let c = Color32::from_rgba_unmultiplied(255, 190, 220, 22);
            let mut y = area.top();
            while y < area.bottom() {
                let mut x = area.left();
                while x < area.right() {
                    paint.circle_filled(egui::pos2(x, y), 1.0, c);
                    x += 18.0;
                }
                y += 18.0;
            }
        }
    }
}

thread_local! {
    static IN_DOCK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with dock chrome active: cards drop their own border and title
/// because the dock's tab already provides both.
pub fn in_dock<R>(f: impl FnOnce() -> R) -> R {
    let old = IN_DOCK.with(|c| c.replace(true));
    let r = f();
    IN_DOCK.with(|c| c.set(old));
    r
}

pub fn is_in_dock() -> bool {
    IN_DOCK.with(|c| c.get())
}

/// Card-like frame used for the dock sections.
pub fn card() -> Card {
    if is_in_dock() {
        return Card(egui::Frame::new());
    }
    Card(
        egui::Frame::new()
            .fill(glass_alpha(surface(), 0.6))
            .stroke(Stroke::new(1.0, if glassy() { border().lerp_to_gamma(Color32::WHITE, 0.22) } else { border() }))
            .corner_radius(CornerRadius::same(if glassy() { 12 } else { 8 }))
            .inner_margin(egui::Margin::same(10)),
    )
}

/// A [`egui::Frame`] that, on glass themes, adds the light sheen and bright
/// top-left edge afterwards.
pub struct Card(egui::Frame);

impl Card {
    pub fn show<R>(self, ui: &mut egui::Ui, f: impl FnOnce(&mut egui::Ui) -> R) -> egui::InnerResponse<R> {
        let r = self.0.show(ui, f);
        if full_glass() && !is_in_dock() {
            sheen(ui.painter(), r.response.rect, 12.0);
        }
        r
    }
}

/// Light falling across a glass panel from the top-left, with a bright edge.
pub fn sheen(paint: &egui::Painter, rect: egui::Rect, radius: f32) {
    let hi = Color32::from_rgba_unmultiplied(255, 255, 255, 20);
    let lo = Color32::from_rgba_unmultiplied(255, 255, 255, 0);
    let mut m = egui::Mesh::default();
    let inner = rect.shrink(radius * 0.5);
    for (pt, c) in [(inner.left_top(), hi), (inner.right_top(), lo), (inner.right_bottom(), lo), (inner.left_bottom(), lo)] {
        m.colored_vertex(pt, c);
    }
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    paint.add(egui::Shape::mesh(m));
    let edge = accent().lerp_to_gamma(Color32::WHITE, 0.6).gamma_multiply(0.55);
    paint.line_segment([rect.left_top() + egui::vec2(radius, 0.5), rect.right_top() + egui::vec2(-radius * 2.0, 0.5)], Stroke::new(1.0, edge));
    paint.line_segment([rect.left_top() + egui::vec2(0.5, radius), rect.left_bottom() + egui::vec2(0.5, -radius * 2.0)], Stroke::new(1.0, edge.gamma_multiply(0.6)));
}

/// A rounded rectangle filled with a left-to-right gradient.
pub fn gradient_rect(paint: &egui::Painter, rect: egui::Rect, c1: Color32, c2: Color32, radius: f32) {
    let r = radius.min(rect.height() / 2.0).min(rect.width() / 2.0);
    let mut pts: Vec<egui::Pos2> = Vec::new();
    let corners = [
        (rect.right_top() + egui::vec2(-r, r), -std::f32::consts::FRAC_PI_2),
        (rect.right_bottom() + egui::vec2(-r, -r), 0.0),
        (rect.left_bottom() + egui::vec2(r, -r), std::f32::consts::FRAC_PI_2),
        (rect.left_top() + egui::vec2(r, r), std::f32::consts::PI),
    ];
    for (c, start) in corners {
        for i in 0..=8 {
            let a = start + i as f32 / 8.0 * std::f32::consts::FRAC_PI_2;
            pts.push(c + egui::vec2(a.cos(), a.sin()) * r);
        }
    }
    let mut m = egui::Mesh::default();
    let centre = rect.center();
    let col = |x: f32| c1.lerp_to_gamma(c2, ((x - rect.left()) / rect.width()).clamp(0.0, 1.0));
    m.colored_vertex(centre, col(centre.x));
    for p in &pts {
        m.colored_vertex(*p, col(p.x));
    }
    let n = pts.len() as u32;
    for i in 1..=n {
        m.add_triangle(0, i, if i == n { 1 } else { i + 1 });
    }
    paint.add(egui::Shape::mesh(m));
}

/// Applies the chosen theme when it changes. Cheap enough to call every frame.
pub fn sync(ctx: &egui::Context, id: &str, glass: u8, motion: u8) {
    static LAST: std::sync::Mutex<Option<(String, u8, u8)>> = std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if last.as_ref().is_some_and(|(i, g, m)| i == id && *g == glass && *m == motion) {
        return;
    }
    set(id, glass, motion);
    apply(ctx);
    ctx.request_repaint();
    *last = Some((id.to_string(), glass, motion));
}

/// Whether this theme currently has weather to simulate (and how strong).
pub fn weather() -> Option<(u8, bool)> {
    let m = motion();
    match p().detail {
        Detail::Weather if m > 0 => Some((m, false)),
        Detail::Storm if m > 0 => Some((m, true)),
        _ => None,
    }
}

/// The theme has anything that moves besides weather (drifting glows etc.).
pub fn animated() -> bool {
    !p().blobs.is_empty() || p().detail != Detail::None
}

/// Animation is currently running (on, and the window is in use).
pub fn is_moving() -> bool {
    motion() > 0
}
