//! The Audio Mixer's animated meters and faders, modelled on OBS's:
//!
//! - stereo meters on a -60..0 dBFS scale with green/yellow/red zones
//!   (OBS's -20 and -9 dB boundaries), peak bars with OBS's "fast" fall-off
//!   (20.5 dB/s), a brighter RMS ("magnitude") section with 300 ms
//!   integration, a peak-hold line, and a clip light that latches for 2 s;
//! - a fader on OBS's cubic curve (unity sits about three quarters along)
//!   whose handle glides to new values set by hotkeys, the remote API or a
//!   reset, grows under the pointer, and supports drag, Ctrl-drag for fine
//!   control, the scroll wheel (0.5 dB steps) and double-click to 0 dB;
//! - muting fades the meter to grey rather than snapping;
//! - horizontal strips (default) or OBS's vertical layout.

use std::time::Instant;

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use ferrous_audio::Levels;

use crate::theme;

pub const MIN_DB: f32 = -60.0;
/// OBS "fast" decay rate.
pub const DECAY_DB_PER_S: f32 = 20.53;
pub const HOLD_S: f32 = 1.5;
pub const CLIP_S: f32 = 2.0;
pub const RMS_TAU_S: f32 = 0.3;
pub const FADER_TAU_S: f32 = 0.07;
/// Highest fader gain (+6 dB).
pub const MAX_GAIN: f32 = 2.0;

pub fn lin_to_db(v: f32) -> f32 {
    if v <= 1e-7 { f32::NEG_INFINITY } else { 20.0 * v.log10() }
}

pub fn db_to_lin(db: f32) -> f32 {
    if db == f32::NEG_INFINITY { 0.0 } else { 10f32.powf(db / 20.0) }
}

/// Fader position (0..1) from gain, on OBS's cubic curve scaled to +6 dB.
pub fn gain_to_pos(gain: f32) -> f32 {
    (gain.clamp(0.0, MAX_GAIN) / MAX_GAIN).cbrt()
}

pub fn pos_to_gain(pos: f32) -> f32 {
    pos.clamp(0.0, 1.0).powi(3) * MAX_GAIN
}

/// Meter position (0..1) for a level in dB.
pub fn db_to_meter(db: f32) -> f32 {
    ((db - MIN_DB) / -MIN_DB).clamp(0.0, 1.0)
}

/// Displayed state of one channel strip, advanced every frame.
#[derive(Clone, Debug)]
pub struct Strip {
    pub peak: [f32; 2],
    pub rms: [f32; 2],
    pub hold: [f32; 2],
    hold_age: [f32; 2],
    clip_age: f32,
    /// Displayed fader position (0..1), eased toward the real value.
    pub fader: f32,
    dragging: bool,
    last: Option<Instant>,
}

impl Default for Strip {
    fn default() -> Self {
        Self {
            peak: [MIN_DB; 2],
            rms: [MIN_DB; 2],
            hold: [MIN_DB; 2],
            hold_age: [0.0; 2],
            clip_age: f32::INFINITY,
            fader: gain_to_pos(1.0),
            dragging: false,
            last: None,
        }
    }
}

impl Strip {
    /// Advance the ballistics by `dt` seconds with the levels measured since
    /// the last frame (`None`: no audio arrived, the meter falls).
    pub fn step(&mut self, levels: Option<Levels>, gain: f32, dt: f32) {
        let dt = dt.clamp(0.0, 0.25);
        for side in 0..2 {
            let (pk, rms) = match levels {
                Some(l) => (lin_to_db(l.peak[side]).max(MIN_DB), lin_to_db(l.rms[side]).max(MIN_DB)),
                None => (MIN_DB, MIN_DB),
            };
            self.peak[side] = if pk >= self.peak[side] { pk } else { (self.peak[side] - DECAY_DB_PER_S * dt).max(pk) };
            let k = 1.0 - (-dt / RMS_TAU_S).exp();
            self.rms[side] += (rms - self.rms[side]) * k;
            self.hold_age[side] += dt;
            if self.peak[side] >= self.hold[side] {
                self.hold[side] = self.peak[side];
                self.hold_age[side] = 0.0;
            } else if self.hold_age[side] > HOLD_S {
                self.hold[side] = (self.hold[side] - DECAY_DB_PER_S * dt).max(self.peak[side]);
            }
            if let Some(l) = levels {
                if l.peak[side] >= 0.999 {
                    self.clip_age = 0.0;
                }
            }
        }
        self.clip_age += dt;
        let target = gain_to_pos(gain);
        if self.dragging {
            self.fader = target;
        } else {
            self.fader += (target - self.fader) * (1.0 - (-dt / FADER_TAU_S).exp());
            if (self.fader - target).abs() < 1e-4 {
                self.fader = target;
            }
        }
    }

    /// Advance using wall-clock time.
    pub fn tick(&mut self, levels: Option<Levels>, gain: f32) {
        let now = Instant::now();
        let dt = self.last.map(|t| (now - t).as_secs_f32()).unwrap_or(0.0);
        self.last = Some(now);
        self.step(levels, gain, dt);
    }

    pub fn clipping(&self) -> bool {
        self.clip_age < CLIP_S
    }

    /// Still moving: keep repainting.
    pub fn animating(&self, gain: f32) -> bool {
        (self.fader - gain_to_pos(gain)).abs() > 1e-4 || self.peak.iter().chain(&self.hold).any(|v| *v > MIN_DB + 0.01) || self.clipping()
    }
}

// ------------------------------------------------------------------ drawing

/// Point along a meter or fader: `t` 0..1 from the low end, `u` 0..1 across.
fn along(rect: Rect, vertical: bool, t: f32, u: f32) -> Pos2 {
    if vertical {
        Pos2::new(rect.left() + u * rect.width(), rect.bottom() - t * rect.height())
    } else {
        Pos2::new(rect.left() + t * rect.width(), rect.top() + u * rect.height())
    }
}

fn span(rect: Rect, vertical: bool, t0: f32, t1: f32) -> Rect {
    Rect::from_two_pos(along(rect, vertical, t0, 0.0), along(rect, vertical, t1, 1.0))
}

const ZONES: [(f32, f32); 3] = [(MIN_DB, -20.0), (-20.0, -9.0), (-9.0, 0.0)];

fn zone_color(i: usize) -> Color32 {
    if theme::patterns() {
        // Okabe–Ito: bluish green, yellow, vermillion. Distinct for the
        // common kinds of colour blindness, and the zones are hatched too.
        return [Color32::from_rgb(0x00, 0x9e, 0x73), Color32::from_rgb(0xf0, 0xe4, 0x42), Color32::from_rgb(0xd5, 0x5e, 0x00)][i];
    }
    [theme::ok(), theme::warn(), theme::live()][i]
}

/// Diagonal hatching over `rect`, `step` points apart.
fn hatch(p: &egui::Painter, rect: Rect, step: f32) {
    let p = p.with_clip_rect(rect);
    let stroke = Stroke::new(1.0, Color32::from_black_alpha(120));
    let span = rect.width() + rect.height();
    let mut d = 0.0;
    while d < span {
        p.line_segment([Pos2::new(rect.left() + d, rect.top()), Pos2::new(rect.left() + d - rect.height(), rect.bottom())], stroke);
        d += step;
    }
}

fn grey(c: Color32, t: f32) -> Color32 {
    let g = ((c.r() as u32 + c.g() as u32 + c.b() as u32) / 3) as u8;
    lerp_color(c, Color32::from_gray(g / 2 + 40), t)
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()), m(a.a(), b.a()))
}

/// One side of the meter.
#[allow(clippy::too_many_arguments)]
fn bar(p: &egui::Painter, rect: Rect, vertical: bool, peak: f32, rms: f32, hold: f32, mute_t: f32, active: bool) {
    p.rect_filled(rect, CornerRadius::same(2), theme::bg());
    for (i, (a, b)) in ZONES.iter().enumerate() {
        let c = grey(zone_color(i), mute_t);
        let (ta, tb) = (db_to_meter(*a), db_to_meter(*b));
        p.rect_filled(span(rect, vertical, ta, tb), 0, c.linear_multiply(0.13));
        if !active {
            continue;
        }
        // Peak section (softer), then the RMS section (full brightness).
        let tp = db_to_meter(peak).min(tb);
        if tp > ta {
            p.rect_filled(span(rect, vertical, ta, tp), 0, c.linear_multiply(0.62));
        }
        let tr = db_to_meter(rms).min(tb);
        if tr > ta {
            p.rect_filled(span(rect, vertical, ta, tr), 0, c);
        }
    }
    if theme::patterns() {
        // Loud zones are marked by texture, not just colour: sparse hatching
        // for “getting loud”, dense for “too loud”.
        for (i, (a, b)) in ZONES.iter().enumerate().skip(1) {
            hatch(p, span(rect, vertical, db_to_meter(*a), db_to_meter(*b)), if i == 1 { 7.0 } else { 3.5 });
        }
    }
    if active && hold > MIN_DB + 0.5 {
        let t = db_to_meter(hold);
        let c = grey(zone_color(if hold >= -9.0 { 2 } else if hold >= -20.0 { 1 } else { 0 }), mute_t);
        let a = along(rect, vertical, t, 0.0);
        let b = along(rect, vertical, t, 1.0);
        let w = if vertical { Vec2::new(0.0, 1.0) } else { Vec2::new(1.0, 0.0) };
        p.line_segment([a - w, b - w], Stroke::new(2.0, c));
    }
}

/// Stereo meter with clip light and dB scale. `rect` covers bars and scale.
pub fn meter(ui: &mut egui::Ui, rect: Rect, s: &Strip, vertical: bool, mute_t: f32, active: bool) {
    let p = ui.painter_at(rect.expand(2.0));
    let clip = 7.0;
    let scale = 12.0;
    let (bars, clip_rect, scale_rect) = if vertical {
        let bars = Rect::from_min_max(Pos2::new(rect.left(), rect.top() + clip + 2.0), Pos2::new(rect.right() - scale, rect.bottom()));
        let clip_rect = Rect::from_min_size(Pos2::new(rect.left(), rect.top()), Vec2::new(bars.width(), clip));
        let scale_rect = Rect::from_min_max(Pos2::new(bars.right() + 2.0, bars.top()), Pos2::new(rect.right(), bars.bottom()));
        (bars, clip_rect, scale_rect)
    } else {
        let bars = Rect::from_min_max(rect.min, Pos2::new(rect.right() - clip - 3.0, rect.bottom() - scale));
        let clip_rect = Rect::from_min_max(Pos2::new(bars.right() + 3.0, bars.top()), Pos2::new(rect.right(), bars.bottom()));
        let scale_rect = Rect::from_min_max(Pos2::new(bars.left(), bars.bottom() + 1.0), Pos2::new(bars.right(), rect.bottom()));
        (bars, clip_rect, scale_rect)
    };
    let gap = 2.0;
    let (b0, b1) = if vertical {
        let w = (bars.width() - gap) / 2.0;
        (Rect::from_min_size(bars.min, Vec2::new(w, bars.height())), Rect::from_min_size(bars.min + Vec2::new(w + gap, 0.0), Vec2::new(w, bars.height())))
    } else {
        let h = (bars.height() - gap) / 2.0;
        (Rect::from_min_size(bars.min, Vec2::new(bars.width(), h)), Rect::from_min_size(bars.min + Vec2::new(0.0, h + gap), Vec2::new(bars.width(), h)))
    };
    bar(&p, b0, vertical, s.peak[0], s.rms[0], s.hold[0], mute_t, active);
    bar(&p, b1, vertical, s.peak[1], s.rms[1], s.hold[1], mute_t, active);

    let clip_on = active && s.clipping();
    let pulse = if clip_on { 0.75 + 0.25 * (ui.input(|i| i.time) as f32 * 8.0).sin() } else { 0.0 };
    p.rect_filled(clip_rect, CornerRadius::same(2), if clip_on { theme::live().linear_multiply(pulse) } else { theme::live().linear_multiply(0.12) });

    // Scale: ticks every 5 dB, labels every 10 (every 20 when cramped).
    let font = FontId::proportional(8.5);
    let len = if vertical { scale_rect.height() } else { scale_rect.width() };
    let label_step = if len < 160.0 { 20 } else { 10 };
    for db in (MIN_DB as i32..=0).step_by(5) {
        let t = db_to_meter(db as f32);
        let major = db % label_step == 0;
        if vertical {
            let y = bars.bottom() - t * bars.height();
            p.line_segment([Pos2::new(scale_rect.left(), y), Pos2::new(scale_rect.left() + if major { 3.0 } else { 2.0 }, y)], Stroke::new(1.0, theme::muted().linear_multiply(0.6)));
            if major {
                p.text(Pos2::new(scale_rect.right(), y), Align2::RIGHT_CENTER, db.abs().to_string(), font.clone(), theme::muted());
            }
        } else {
            let x = bars.left() + t * bars.width();
            p.line_segment([Pos2::new(x, scale_rect.top()), Pos2::new(x, scale_rect.top() + if major { 3.0 } else { 2.0 })], Stroke::new(1.0, theme::muted().linear_multiply(0.6)));
            if major {
                let align = if db == 0 { Align2::RIGHT_TOP } else if db == MIN_DB as i32 { Align2::LEFT_TOP } else { Align2::CENTER_TOP };
                p.text(Pos2::new(x, scale_rect.top() + 3.0), align, db.to_string(), font.clone(), theme::muted());
            }
        }
    }
}

/// What the fader did this frame.
pub struct FaderResponse {
    pub new_gain: Option<f32>,
    pub response: egui::Response,
}

/// The fader. `gain` is the channel's current linear gain.
pub fn fader(ui: &mut egui::Ui, rect: Rect, id: egui::Id, s: &mut Strip, gain: f32, vertical: bool, mute_t: f32) -> FaderResponse {
    let resp = ui.interact(rect, id, Sense::click_and_drag());
    let mut new_gain = None;
    let len = if vertical { rect.height() } else { rect.width() };
    let handle = 14.0;
    let usable = (len - handle).max(1.0);
    let pos_at = |p: Pos2| -> f32 {
        if vertical { (rect.bottom() - handle / 2.0 - p.y) / usable } else { (p.x - rect.left() - handle / 2.0) / usable }
    };
    if resp.drag_started() {
        s.dragging = true;
    }
    if resp.dragged() {
        let fine = ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
        let pos = if fine {
            let d = resp.drag_delta();
            s.fader + (if vertical { -d.y } else { d.x }) / usable * 0.2
        } else if let Some(p) = resp.interact_pointer_pos() {
            pos_at(p)
        } else {
            s.fader
        };
        let pos = pos.clamp(0.0, 1.0);
        s.fader = pos;
        new_gain = Some(pos_to_gain(pos));
    }
    if resp.drag_stopped() {
        s.dragging = false;
    }
    if resp.double_clicked() {
        s.dragging = false;
        new_gain = Some(1.0);
    } else if resp.clicked() && !s.dragging {
        if let Some(p) = resp.interact_pointer_pos() {
            new_gain = Some(pos_to_gain(pos_at(p)));
        }
    }
    if resp.hovered() {
        // One 0.5 dB step per wheel notch (raw events, not smoothed scrolling).
        let d: f32 = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::MouseWheel { delta, .. } => Some(delta.y + if vertical { 0.0 } else { delta.x }),
                    _ => None,
                })
                .sum()
        });
        if d.abs() > 1e-3 {
            let db = lin_to_db(gain).max(-96.0) + d.signum() * 0.5;
            let g = if db <= -95.0 { 0.0 } else { db_to_lin(db).min(MAX_GAIN) };
            new_gain = Some(g);
        }
    }

    // Draw.
    let p = ui.painter_at(rect.expand(3.0));
    let hover_t = ui.ctx().animate_bool_with_time(id.with("hover"), resp.hovered() || resp.dragged(), 0.12);
    let track_w = 4.0 + 2.0 * hover_t;
    let track = if vertical {
        Rect::from_center_size(rect.center(), Vec2::new(track_w, len - handle / 2.0))
    } else {
        Rect::from_center_size(rect.center(), Vec2::new(len - handle / 2.0, track_w))
    };
    p.rect_filled(track, CornerRadius::same(3), theme::bg());
    let accent = grey(theme::accent(), mute_t);
    let c = |t: f32| if vertical { Pos2::new(rect.center().x, rect.bottom() - handle / 2.0 - t * usable) } else { Pos2::new(rect.left() + handle / 2.0 + t * usable, rect.center().y) };
    let hc = c(s.fader);
    let filled = if vertical {
        Rect::from_min_max(Pos2::new(track.left(), hc.y), Pos2::new(track.right(), track.bottom()))
    } else {
        Rect::from_min_max(Pos2::new(track.left(), track.top()), Pos2::new(hc.x, track.bottom()))
    };
    p.rect_filled(filled, CornerRadius::same(3), accent.linear_multiply(0.85));
    // Unity (0 dB) notch.
    let u = c(gain_to_pos(1.0));
    let n = if vertical { Vec2::new(track_w / 2.0 + 4.0, 0.0) } else { Vec2::new(0.0, track_w / 2.0 + 4.0) };
    p.line_segment([u - n, u + n], Stroke::new(1.0, theme::muted().linear_multiply(0.7)));
    // Handle.
    let size = if vertical { Vec2::new(16.0, 10.0) } else { Vec2::new(10.0, 16.0) } * (1.0 + 0.15 * hover_t);
    let h = Rect::from_center_size(hc, size);
    p.rect_filled(h.translate(Vec2::new(0.0, 1.0)), CornerRadius::same(3), Color32::from_black_alpha(90));
    p.rect_filled(h, CornerRadius::same(3), lerp_color(theme::surface(), theme::text(), 0.15 + 0.35 * hover_t));
    p.rect_stroke(h, CornerRadius::same(3), Stroke::new(1.0 + hover_t, lerp_color(theme::border(), accent, hover_t)), StrokeKind::Inside);
    let grip = if vertical { Vec2::new(4.0, 0.0) } else { Vec2::new(0.0, 4.0) };
    p.line_segment([hc - grip, hc + grip], Stroke::new(1.0, theme::bg()));

    let resp = resp.on_hover_text_at_pointer(format!(
        "{}  ·  drag, Ctrl-drag for fine, scroll ±0.5 dB, double-click for 0 dB",
        fmt_db(gain)
    ));
    FaderResponse { new_gain, response: resp }
}

pub fn fmt_db(gain: f32) -> String {
    let db = lin_to_db(gain);
    if db == f32::NEG_INFINITY || db < -95.0 { "-inf dB".into() } else { format!("{db:+.1} dB").replace("+0.0", "0.0") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lv(peak: f32, rms: f32) -> Option<Levels> {
        Some(Levels { peak: [peak; 2], rms: [rms; 2] })
    }

    #[test]
    fn fader_curve_round_trips_and_unity_sits_high() {
        for g in [0.0, 0.01, 0.25, 1.0, 1.5, 2.0] {
            assert!((pos_to_gain(gain_to_pos(g)) - g).abs() < 1e-5);
        }
        let u = gain_to_pos(1.0);
        assert!((0.75..0.82).contains(&u), "{u}");
        // -20 dB is still well up the fader, like OBS.
        assert!(gain_to_pos(0.1) > 0.35);
    }

    #[test]
    fn peak_attacks_instantly_and_falls_at_obs_rate() {
        let mut s = Strip::default();
        s.step(lv(1.0, 0.5), 1.0, 1.0 / 60.0);
        assert!(s.peak[0].abs() < 0.01);
        // One second of silence: falls 20.5 dB (in 60 steps).
        for _ in 0..60 {
            s.step(None, 1.0, 1.0 / 60.0);
        }
        assert!((s.peak[0] + DECAY_DB_PER_S).abs() < 0.2, "{}", s.peak[0]);
    }

    #[test]
    fn hold_line_waits_then_falls() {
        let mut s = Strip::default();
        s.step(lv(0.5, 0.2), 1.0, 0.016);
        let h0 = s.hold[0];
        for _ in 0..80 {
            s.step(lv(0.01, 0.01), 1.0, 0.016); // 1.28 s < hold time
        }
        assert_eq!(s.hold[0], h0);
        for _ in 0..60 {
            s.step(lv(0.01, 0.01), 1.0, 0.016);
        }
        assert!(s.hold[0] < h0 - 5.0);
    }

    #[test]
    fn rms_integrates_over_300ms() {
        let mut s = Strip::default();
        let target = lin_to_db(0.5);
        for _ in 0..30 {
            s.step(lv(0.5, 0.5), 1.0, 0.01);
        }
        // After one time constant: 63% of the way from -60 dB.
        let expect = MIN_DB + (target - MIN_DB) * (1.0 - (-1f32).exp());
        assert!((s.rms[0] - expect).abs() < 0.1, "{} vs {expect}", s.rms[0]);
    }

    #[test]
    fn clip_light_latches_for_two_seconds() {
        let mut s = Strip::default();
        s.step(lv(1.0, 0.7), 1.0, 0.016);
        assert!(s.clipping());
        for _ in 0..19 {
            s.step(None, 1.0, 0.1);
        }
        assert!(s.clipping());
        s.step(None, 1.0, 0.2);
        assert!(!s.clipping());
    }

    #[test]
    fn fader_glides_to_external_changes() {
        let mut s = Strip::default();
        s.step(None, 0.0, 0.016); // e.g. a hotkey sets -inf
        assert!(s.fader > 0.5, "doesn't jump");
        for _ in 0..60 {
            s.step(None, 0.0, 0.016);
        }
        assert!(s.fader < 0.01, "arrives within a second: {}", s.fader);
    }
}
