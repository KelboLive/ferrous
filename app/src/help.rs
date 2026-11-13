//! Gentle onboarding: a three-step tour for first-time users and a
//! troubleshooting page for the problems people actually hit.

use eframe::egui::{self, Align2, RichText};

use crate::theme;

/// What the tour can tell about the user's progress, so each step can show a tick.
#[derive(Clone, Copy, Default)]
pub struct Progress {
    pub has_source: bool,
    pub recording: bool,
}

pub enum TourEvent {
    Stay,
    /// Finished or skipped; don't show it again.
    Done,
}

const STEPS: [(&str, &str); 3] = [
    ("1  Add something to show", "Pick what to put on screen with the tiles in Sources (left): your screen, a game, a window or a camera. It appears in the big preview straight away."),
    ("2  Check your sound", "Say something. The bar for your microphone in the Audio Mixer (right) should jump. Drag a fader to change a volume, or click the speaker to mute it."),
    ("3  Record or go live", "Press Start Recording to save a video (Recordings opens the folder). Going live asks for your stream key once. Lost? Press Ctrl+K and type what you want."),
];

/// The tour card, floating at the bottom of the window. `step` is the page.
pub fn tour(ctx: &egui::Context, step: &mut usize, progress: Progress) -> TourEvent {
    let mut event = TourEvent::Stay;
    *step = (*step).min(STEPS.len() - 1);
    egui::Window::new("tour")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_BOTTOM, [0.0, -48.0])
        .fixed_size([440.0, 0.0])
        .frame(egui::Frame::window(&ctx.global_style()).stroke(egui::Stroke::new(2.0, theme::accent())))
        .show(ctx, |ui| {
            let (title, body) = STEPS[*step];
            let done = match *step {
                0 => progress.has_source,
                2 => progress.recording,
                _ => false,
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).strong().size(16.0));
                if done {
                    ui.label(RichText::new("✔ done").color(theme::ok()));
                }
            });
            ui.label(body);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for i in 0..STEPS.len() {
                    ui.label(RichText::new(if i == *step { "●" } else { "○" }).color(theme::accent()));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if *step + 1 == STEPS.len() {
                        if ui.button(RichText::new("Got it").strong()).clicked() {
                            event = TourEvent::Done;
                        }
                    } else if ui.button(RichText::new("Next ▶").strong()).clicked() {
                        *step += 1;
                    }
                    if *step > 0 && ui.button("◀ Back").clicked() {
                        *step -= 1;
                    }
                    if ui.button("Skip tour").clicked() {
                        event = TourEvent::Done;
                    }
                });
            });
        });
    event
}

/// Problems people run into, with what to do about each: (problem, answer).
pub const FAQ: &[(&str, &str)] = &[
    ("My screen or game capture is black", "Use Window Capture or Display Capture for most programs. For a game, add Game Capture and pick the game while it is running. Some programs block capture; if one does, capture the whole display instead. A laptop with two graphics chips may need Ferrous set to the same one as the game in Windows Graphics settings."),
    ("Nobody can hear me / I can't hear the game", "Look at the Audio Mixer: the microphone and desktop bars should move when there is sound. If the speaker icon is crossed out, click it. Open Settings → Audio and make sure the right microphone and output device are chosen."),
    ("The video stutters or drops frames", "Open Settings → Recording (or Stream) and pick the Light quality profile, or lower the frame rate in Settings → Video. If you have an NVIDIA, AMD or Intel graphics card, choose its encoder instead of Software (x264) so your processor is free for the game."),
    ("The stream key is rejected or the stream won't connect", "Copy the key again from your streaming site's dashboard (it is long and easy to cut short) and paste it into Settings → Stream. Check that the server matches the service and that your internet is up."),
    ("My recordings are huge", "Choose the Balanced or Small files quality profile in Settings → Recording. Recording at a lower resolution or 30 frames per second helps too. The hint under Bitrate shows roughly how many gigabytes an hour will take."),
    ("My recording won't play, or has no sound in some players", "Record to MKV (the default), which survives crashes, then remux to MP4 if a site needs it. If a player is silent, try VLC, or record with the AAC audio codec in Settings → Recording."),
    ("HDR videos look washed out or too dark", "HDR10 output needs an HEVC or AV1 encoder and a player that understands HDR. On a normal screen it can look dull. Raise the “nits” value next to HDR10 if the picture is dim, or turn HDR off for everyday recording."),
    ("Where did my recording go?", "Click Recordings in the Controls panel, or File → Open recordings folder. You can change the folder in Settings → Recording."),
    ("I clicked something and want it back", "Press Ctrl+Z. Removing a source or scene also shows an Undo button for a few seconds."),
    ("Everything is too small or hard to read", "Press Ctrl and + to zoom in, or open Settings → General for Interface size and High-contrast colours."),
    ("I want more controls", "Switch from Simple to Advanced at the top right. Everything you had is waiting there: more panels, studio mode, transitions, filters and more. You can switch back at any time."),
];

pub fn troubleshooting(ctx: &egui::Context, open: &mut bool, filter: &mut String) {
    let mut is_open = *open;
    egui::Window::new("Troubleshooting").open(&mut is_open).default_size([560.0, 460.0]).show(ctx, |ui| {
        ui.add(egui::TextEdit::singleline(filter).hint_text("🔍 Describe the problem…").desired_width(f32::INFINITY));
        ui.add_space(4.0);
        let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut any = false;
            for (q, a) in FAQ.iter().filter(|(q, a)| {
                let hay = format!("{q} {a}").to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            }) {
                any = true;
                egui::CollapsingHeader::new(RichText::new(*q).strong()).id_salt(q).default_open(!words.is_empty()).show(ui, |ui| {
                    ui.label(*a);
                    ui.add_space(4.0);
                });
            }
            if !any {
                ui.label(RichText::new("No match. Try fewer words, such as “black”, “audio” or “key”.").color(theme::muted()));
            }
        });
    });
    *open = is_open;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_faq_entry_is_filled_in_and_findable() {
        assert!(FAQ.len() >= 8);
        for (q, a) in FAQ {
            assert!(q.len() > 10 && a.len() > 40, "{q}");
        }
        let hit = |w: &str| FAQ.iter().any(|(q, a)| format!("{q} {a}").to_lowercase().contains(w));
        assert!(hit("black") && hit("stream key") && hit("ctrl+z"));
    }
}
