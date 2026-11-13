//! Rain that lands on the tops of the docks, pools along their top edges and
//! runs off the sides.
//!
//! Drops fall only in the open air above each dock (never across a dock, the
//! video or the other docks). Each one that lands splashes and leaves a bit of
//! water on the dock's top edge. Beads of water merge as they gather, creep
//! toward the nearest side, hang from the corner as a swelling drop, then
//! let go and fall down the gutter. Everything is a small particle model,
//! stepped once per frame.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use eframe::egui::{self, Color32, Pos2, Rect, Shape, Stroke, vec2};

#[derive(Clone, Copy)]
enum Kind {
    /// A streak of rain; `near` (0.45–1) is how close it is: bigger, faster, brighter.
    Rain { near: f32, born: f32 },
    /// A drop shed from a corner, accelerating under gravity.
    Drop { r: f32 },
}

struct Fall {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    kind: Kind,
}

/// Water gathered on a dock's top edge.
struct Bead {
    x: f32,
    /// Mass in drop-units; radius grows with its square root.
    m: f32,
}

struct Dock {
    rect: Rect,
    /// Nothing sits directly above it, so rain can reach its top.
    exposed: bool,
    beads: Vec<Bead>,
    /// How wet the top edge is (0–1): drives the thin film along it.
    wet: f32,
    /// Water hanging from the left and right corners.
    hang: [f32; 2],
    seen: bool,
}

struct Splash {
    p: Pos2,
    age: f32,
    seed: u32,
    size: f32,
}

struct Sim {
    falls: Vec<Fall>,
    docks: HashMap<(i32, i32, i32), Dock>,
    noted: Vec<Rect>,
    splashes: Vec<Splash>,
    area: Rect,
    last: Option<Instant>,
    rng: u32,
    spawn: f32,
    flash: f32,
    next_flash: f32,
    storm: bool,
}

impl Sim {
    fn rand(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng >> 8) as f32 / 16_777_216.0
    }
}

static SIM: Mutex<Option<Sim>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Sim) -> R) -> R {
    let mut g = SIM.lock().unwrap();
    let sim = g.get_or_insert_with(|| Sim {
        falls: Vec::new(),
        docks: HashMap::new(),
        noted: Vec::new(),
        splashes: Vec::new(),
        area: Rect::NOTHING,
        last: None,
        rng: 0x9e3779b9,
        spawn: 0.0,
        flash: 0.0,
        next_flash: 6.0,
        storm: false,
    });
    f(sim)
}

/// Records a dock's rectangle for next frame.
pub fn note_panel(r: Rect) {
    if r.width() > 60.0 && r.height() > 40.0 {
        with(|s| s.noted.push(r));
    }
}

const WIND: f32 = -0.16;
const GRAVITY: f32 = 1500.0;

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn bead_radius(m: f32) -> f32 {
    1.5 + 1.25 * m.sqrt()
}

/// Steps the simulation. `level`: 1 = reduced (half the rain), 2 = full.
pub fn step(ctx: &egui::Context, level: u8, storm: bool) {
    let now = Instant::now();
    with(|s| {
        s.storm = storm;
        s.area = ctx.content_rect();
        let dt = s.last.map_or(0.016, |l| now.duration_since(l).as_secs_f32()).clamp(0.001, 0.06);
        s.last = Some(now);

        // Docks as laid out last frame; state carries over for unchanged ones.
        let noted = std::mem::take(&mut s.noted);
        for d in s.docks.values_mut() {
            d.seen = false;
        }
        for r in &noted {
            let key = ((r.left() / 6.0).round() as i32, (r.top() / 6.0).round() as i32, (r.width() / 6.0).round() as i32);
            let d = s.docks.entry(key).or_insert_with(|| Dock { rect: *r, exposed: true, beads: Vec::new(), wet: 0.0, hang: [0.0; 2], seen: true });
            d.rect = *r;
            d.seen = true;
        }
        s.docks.retain(|_, d| d.seen);
        let rects: Vec<Rect> = s.docks.values().map(|d| d.rect).collect();
        for d in s.docks.values_mut() {
            d.exposed = !rects.iter().any(|o| o.bottom() <= d.rect.top() + 6.0 && o.bottom() > d.rect.top() - 60.0 && o.right() > d.rect.left() + 20.0 && o.left() < d.rect.right() - 20.0);
        }

        // Rain appears in the air just above each exposed dock.
        let exposed: Vec<Rect> = s.docks.values().filter(|d| d.exposed).map(|d| d.rect).collect();
        let total_w: f32 = exposed.iter().map(|r| r.width()).sum();
        let rate = total_w * if level >= 2 { 0.11 } else { 0.055 };
        s.spawn += rate * dt;
        while s.spawn >= 1.0 && !exposed.is_empty() && s.falls.len() < 260 {
            s.spawn -= 1.0;
            let mut pick = s.rand() * total_w;
            let mut r = exposed[0];
            for e in &exposed {
                if pick <= e.width() {
                    r = *e;
                    break;
                }
                pick -= e.width();
            }
            let (rx, rn, rh) = (s.rand(), s.rand(), s.rand());
            let near = 0.45 + rn * 0.55;
            let speed = 520.0 + near * 620.0;
            let h = 70.0 + rh * 90.0;
            let y = r.top() - h;
            s.falls.push(Fall { x: r.left() + 6.0 + rx * (r.width() - 12.0) - h * WIND, y, vx: speed * WIND, vy: speed, kind: Kind::Rain { near, born: y } });
        }
        if exposed.is_empty() {
            s.spawn = 0.0;
        }

        // Falling things: land on a dock top, or leave the window.
        let area = s.area;
        let keys: Vec<(i32, i32, i32)> = s.docks.keys().copied().collect();
        let mut i = 0;
        while i < s.falls.len() {
            let f = &mut s.falls[i];
            if let Kind::Drop { .. } = f.kind {
                f.vy += GRAVITY * dt;
            }
            let py = f.y;
            f.x += f.vx * dt;
            f.y += f.vy * dt;
            let (nx, ny, kind) = (f.x, f.y, f.kind);
            let landed = keys.iter().copied().find(|k| {
                let r = s.docks[k].rect;
                nx >= r.left() + 3.0 && nx <= r.right() - 3.0 && py < r.top() && ny >= r.top()
            });
            let inside = || rects.iter().any(|r| r.shrink2(vec2(2.0, 2.0)).contains(Pos2::new(nx, ny)) && !(py < r.top() && ny >= r.top()));
            if let Some(k) = landed {
                let (mass, size) = match kind {
                    Kind::Rain { near, .. } => (0.5 + near * 0.5, 0.6 + near * 0.5),
                    Kind::Drop { r } => (r * r * 0.5, 0.8 + r * 0.25),
                };
                let seed = s.rng;
                let top = s.docks[&k].rect.top();
                s.splashes.push(Splash { p: Pos2::new(nx, top), age: 0.0, seed, size });
                let d = s.docks.get_mut(&k).unwrap();
                d.wet = (d.wet + 0.035).min(1.0);
                // Join a bead that's close, or start a new one.
                match d.beads.iter().position(|b| (b.x - nx).abs() < 7.0) {
                    Some(i) => {
                        let b = &mut d.beads[i];
                        b.x = (b.x * b.m + nx * mass) / (b.m + mass);
                        b.m += mass;
                    }
                    None if d.beads.len() < 30 => d.beads.push(Bead { x: nx, m: mass }),
                    None => {}
                }
                s.falls.swap_remove(i);
            } else if ny > area.bottom() + 20.0 || nx < area.left() - 60.0 || nx > area.right() + 60.0 || inside() {
                s.falls.swap_remove(i);
            } else {
                i += 1;
            }
        }

        // Water on the docks: beads merge, creep toward the nearest side, hang, let go.
        let mut released: Vec<(f32, f32, f32)> = Vec::new();
        let mut rng = s.rng;
        let mut rnd = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            (rng >> 8) as f32 / 16_777_216.0
        };
        for d in s.docks.values_mut() {
            let (l, r, top) = (d.rect.left(), d.rect.right(), d.rect.top());
            let mid = (l + r) / 2.0;
            let mut to_hang: [f32; 2] = [0.0; 2];
            d.beads.retain_mut(|b| {
                let dir = if b.x < mid { -1.0 } else { 1.0 };
                // Heavier beads move faster; a little wobble keeps them organic.
                b.x += dir * (10.0 + 24.0 * b.m) * dt;
                if b.x <= l + 2.0 {
                    to_hang[0] += b.m;
                    return false;
                }
                if b.x >= r - 2.0 {
                    to_hang[1] += b.m;
                    return false;
                }
                true
            });
            // Merge neighbours that touch.
            d.beads.sort_by(|a, b| a.x.total_cmp(&b.x));
            let mut k = 0;
            while k + 1 < d.beads.len() {
                let (a, b) = (&d.beads[k], &d.beads[k + 1]);
                if b.x - a.x < (bead_radius(a.m) + bead_radius(b.m)) * 0.8 {
                    let m = a.m + b.m;
                    let x = (a.x * a.m + b.x * b.m) / m;
                    d.beads[k] = Bead { x, m };
                    d.beads.remove(k + 1);
                } else {
                    k += 1;
                }
            }
            for (side, (h, add)) in d.hang.iter_mut().zip(to_hang).enumerate() {
                *h += add;
                if *h >= 2.4 {
                    let x = if side == 0 { l - 1.5 } else { r + 1.5 };
                    released.push((x, top + 4.0, 1.5 + rnd() * 0.9));
                    *h = 0.35;
                }
            }
            // The film dries slowly once the rain stops.
            d.wet = (d.wet - 0.05 * dt).max(0.0);
        }
        s.rng = rng;
        for (x, y, r) in released {
            s.falls.push(Fall { x, y, vx: 0.0, vy: 30.0, kind: Kind::Drop { r } });
        }

        s.splashes.iter_mut().for_each(|p| p.age += dt);
        s.splashes.retain(|p| p.age < 0.5);

        if s.storm {
            s.next_flash -= dt;
            if s.next_flash <= 0.0 {
                s.flash = 0.5;
                s.next_flash = 7.0 + s.rand() * 12.0;
            }
            s.flash = (s.flash - dt).max(0.0);
        } else {
            s.flash = 0.0;
        }
    });
    ctx.request_repaint_after(std::time::Duration::from_millis(if level >= 2 { 16 } else { 33 }));
}

fn alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a.clamp(0.0, 1.0) * 255.0) as u8)
}

/// A drop of water on glass: dark underside, clear body, bright rim, sharp highlight.
fn glass_drop(p: &egui::Painter, c: Pos2, rx: f32, ry: f32, tint: Color32, strength: f32) {
    let ell = |centre: Pos2, ax: f32, ay: f32, col: Color32| p.add(Shape::ellipse_filled(centre, vec2(ax, ay), col));
    // Contact shadow below, refracted light inside, rim, highlight.
    ell(c + vec2(rx * 0.12, ry * 0.45), rx * 1.02, ry * 0.9, Color32::from_black_alpha((70.0 * strength) as u8));
    ell(c, rx, ry, alpha(tint, 0.22 * strength));
    ell(c + vec2(rx * 0.18, ry * 0.32), rx * 0.62, ry * 0.5, alpha(Color32::WHITE, 0.16 * strength));
    p.add(Shape::ellipse_stroke(c, vec2(rx, ry), Stroke::new(0.9, alpha(Color32::WHITE, 0.55 * strength))));
    ell(c + vec2(-rx * 0.38, -ry * 0.42), rx * 0.3, ry * 0.24, alpha(Color32::WHITE, 0.95 * strength));
}

/// Draws the weather over the docks (call after they are painted).
pub fn paint(ctx: &egui::Context, tint: Color32) {
    let p = ctx.layer_painter(egui::LayerId::background());
    with(|s| {
        // Rain streaks: a tail that fades out behind a bright head, brighter and longer when near.
        for f in &s.falls {
            match f.kind {
                Kind::Rain { near, born } => {
                    let fade = smooth((f.y - born) / 36.0);
                    let len = 14.0 + near * 26.0;
                    let dir = vec2(f.vx, f.vy).normalized();
                    let head = Pos2::new(f.x, f.y);
                    let tail = head - dir * len;
                    let w = 0.6 + near * 0.9;
                    let n = vec2(-dir.y, dir.x) * (w * 0.5);
                    let a = 0.55 * near * fade;
                    let mut m = egui::Mesh::default();
                    m.colored_vertex(tail - n, alpha(tint, 0.0));
                    m.colored_vertex(tail + n, alpha(tint, 0.0));
                    m.colored_vertex(head + n, alpha(tint, a));
                    m.colored_vertex(head - n, alpha(tint, a));
                    m.add_triangle(0, 1, 2);
                    m.add_triangle(0, 2, 3);
                    p.add(Shape::mesh(m));
                }
                Kind::Drop { r } => {
                    // A falling drop is rounder at the bottom with a short tail above it.
                    let c = Pos2::new(f.x, f.y);
                    let tail = (f.vy * 0.012).min(12.0);
                    p.add(Shape::convex_polygon(vec![c + vec2(-r * 0.55, 0.0), c + vec2(r * 0.55, 0.0), c + vec2(0.0, -tail - r)], alpha(tint, 0.2), Stroke::NONE));
                    glass_drop(&p, c, r, r * 1.08, tint, 1.0);
                }
            }
        }
        for d in s.docks.values() {
            let r = d.rect;
            // The film along the top edge, a bright hairline with a soft glow.
            if d.wet > 0.02 {
                let a = (d.wet * 0.7).min(0.7);
                p.line_segment([Pos2::new(r.left() + 6.0, r.top() + 0.5), Pos2::new(r.right() - 6.0, r.top() + 0.5)], Stroke::new(1.0, alpha(Color32::WHITE, a * 0.55)));
                p.line_segment([Pos2::new(r.left() + 6.0, r.top() + 1.5), Pos2::new(r.right() - 6.0, r.top() + 1.5)], Stroke::new(1.0, alpha(tint, a * 0.35)));
            }
            for b in &d.beads {
                let rad = bead_radius(b.m);
                glass_drop(&p, Pos2::new(b.x, r.top() + 1.0 + rad * 0.55), rad * 1.35, rad * 0.95, tint, 1.0);
            }
            // Water gathering at a corner: a bulb that swells and stretches.
            for (side, x) in [(0, r.left() - 0.5), (1, r.right() + 0.5)] {
                let m = d.hang[side];
                if m > 0.3 {
                    let rad = 1.2 + 1.3 * (m / 2.4).min(1.0).sqrt();
                    let len = 3.0 + (m / 2.4).min(1.0) * 7.0;
                    let c = Pos2::new(x, r.top() + 3.0 + len);
                    p.add(Shape::convex_polygon(vec![Pos2::new(x - rad * 0.5, r.top() + 1.0), Pos2::new(x + rad * 0.5, r.top() + 1.0), c + vec2(rad * 0.9, 0.0), c + vec2(-rad * 0.9, 0.0)], alpha(tint, 0.2), Stroke::NONE));
                    glass_drop(&p, c, rad, rad * 1.1, tint, 1.0);
                }
            }
        }
        // Splashes: a few droplets thrown up, and a ring spreading along the edge.
        for sp in &s.splashes {
            let t = (sp.age / 0.5).clamp(0.0, 1.0);
            let fade = 1.0 - t;
            for k in 0..5u32 {
                let h = sp.seed.wrapping_add(k.wrapping_mul(2654435761));
                let ang = -0.5 - (h % 100) as f32 / 100.0 * 2.1;
                let spd = (34.0 + ((h >> 8) % 50) as f32) * (0.7 + sp.size * 0.4);
                let q = sp.p + vec2(ang.cos() * spd * sp.age, ang.sin() * spd * sp.age + GRAVITY * 0.28 * sp.age * sp.age);
                if q.y < sp.p.y - 0.5 {
                    p.circle_filled(q, 0.7 + sp.size * 0.5, alpha(Color32::WHITE, 0.55 * fade));
                }
            }
            let rx = 2.0 + t * 9.0 * sp.size;
            p.add(Shape::ellipse_stroke(sp.p + vec2(0.0, 1.0), vec2(rx, rx * 0.28), Stroke::new(0.9, alpha(tint, 0.5 * fade))));
        }
        if s.flash > 0.0 {
            let f = s.flash / 0.5;
            let pulse = ((f * 9.0).sin().abs() * f).clamp(0.0, 1.0);
            p.rect_filled(s.area, 0.0, Color32::from_rgba_unmultiplied(200, 220, 255, (pulse * 38.0) as u8));
        }
    });
}
