//! The command palette (Ctrl+K): type what you want to do, press Enter.
//! Every action, scene, source type and setting is reachable by name, so
//! nobody has to know where anything lives.

use eframe::egui::{self, Align2, Key, RichText};

use crate::add_source::Kind;
use crate::theme;

#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    ToggleRecording,
    ToggleStreaming,
    ToggleReplay,
    SaveReplay,
    OpenSettings,
    /// A settings tab by key ("stream", "audio"…).
    SettingsTab(&'static str),
    AddSource(Kind),
    SwitchScene(usize),
    NewScene,
    ToggleMode,
    Undo,
    Redo,
    OpenRecordings,
    Tour,
    Troubleshooting,
    DuplicateSource,
    ToggleSourceVisible,
    RemoveSource,
    StudioMode,
    Presets,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub cmd: Cmd,
    pub label: String,
    /// Group shown on the right, and extra words that should find it.
    pub group: &'static str,
    pub words: String,
}

impl Item {
    pub fn new(cmd: Cmd, label: impl Into<String>, group: &'static str, words: &str) -> Self {
        Self { cmd, label: label.into(), group, words: words.to_string() }
    }
}

#[derive(Default)]
pub struct Palette {
    pub open: bool,
    query: String,
    selected: usize,
    just_opened: bool,
}

impl Palette {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.query.clear();
        self.selected = 0;
        self.just_opened = self.open;
    }

    /// Draws the palette over `items`; the chosen command, if any.
    pub fn show(&mut self, ctx: &egui::Context, items: &[Item]) -> Option<Cmd> {
        if !self.open {
            return None;
        }
        let hits = search(items, &self.query, 9);
        self.selected = self.selected.min(hits.len().saturating_sub(1));
        let (down, up, enter, esc) = ctx.input_mut(|i| {
            (i.consume_key(egui::Modifiers::NONE, Key::ArrowDown), i.consume_key(egui::Modifiers::NONE, Key::ArrowUp), i.consume_key(egui::Modifiers::NONE, Key::Enter), i.consume_key(egui::Modifiers::NONE, Key::Escape))
        });
        if down && self.selected + 1 < hits.len() {
            self.selected += 1;
        }
        if up && self.selected > 0 {
            self.selected -= 1;
        }
        let mut chosen = None;
        if enter {
            chosen = hits.get(self.selected).map(|i| i.cmd.clone());
        }
        egui::Window::new("Command palette")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, [0.0, 80.0])
            .fixed_size([520.0, 0.0])
            .show(ctx, |ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.query).hint_text("🔍 Type a command, scene, source or setting…").desired_width(f32::INFINITY));
                if self.just_opened {
                    r.request_focus();
                    self.just_opened = false;
                }
                if r.changed() {
                    self.selected = 0;
                }
                ui.add_space(4.0);
                if hits.is_empty() {
                    ui.label(RichText::new("Nothing matches. Try “record”, “mic”, “scene” or “bitrate”.").small().color(theme::muted()));
                }
                for (n, it) in hits.iter().enumerate() {
                    let row = ui.horizontal(|ui| {
                        let sel = n == self.selected;
                        let r = ui.add_sized([ui.available_width() - 110.0, 24.0], egui::Button::selectable(sel, &it.label));
                        ui.label(RichText::new(it.group).small().color(theme::muted()));
                        r
                    });
                    if row.inner.clicked() {
                        chosen = Some(it.cmd.clone());
                    }
                    if row.inner.hovered() && ui.input(|i| i.pointer.delta().length() > 0.0) {
                        self.selected = n;
                    }
                }
                ui.add_space(2.0);
                ui.label(RichText::new("↑↓ choose · Enter run · Esc close").small().color(theme::muted()));
            });
        if esc || chosen.is_some() {
            self.open = false;
        }
        chosen
    }
}

/// How well `query` matches `text` (higher is better); `None` if it doesn't.
/// Every word of the query must appear, as a substring or at least in order.
pub fn score(query: &str, text: &str, fuzzy: bool) -> Option<i32> {
    let text = text.to_lowercase();
    let mut total = 0;
    for tok in query.to_lowercase().split_whitespace() {
        if let Some(at) = text.find(tok) {
            let boundary = at == 0 || !text[..at].chars().last().is_some_and(|c| c.is_alphanumeric());
            total += 10 + if boundary { 6 } else { 0 } - (at as i32).min(8);
        } else {
            // Typos and skipped letters ("strmng"), but only for longer words:
            // three letters in order match almost anything.
            let mut it = text.chars();
            if fuzzy && tok.chars().count() >= 4 && tok.chars().all(|c| it.any(|t| t == c)) {
                total += 2;
            } else {
                return None;
            }
        }
    }
    Some(total)
}

/// The best `limit` items for `query` (the first ones when it's empty).
pub fn search<'a>(items: &'a [Item], query: &str, limit: usize) -> Vec<&'a Item> {
    if query.trim().is_empty() {
        return items.iter().take(limit).collect();
    }
    let mut scored: Vec<(i32, usize, &Item)> = items
        .iter()
        .enumerate()
        .filter_map(|(n, it)| {
            let label = score(query, &it.label, true)?;
            // Matches in the title count double; keywords only break ties.
            Some((label * 2 + score(query, &it.words, false).unwrap_or(0) / 2, n, it))
        })
        .collect();
    // Items that match only through their keywords still show up.
    for (n, it) in items.iter().enumerate() {
        if !scored.iter().any(|s| s.1 == n) {
            if let Some(w) = score(query, &format!("{} {}", it.label, it.words), false) {
                scored.push((w / 2, n, it));
            }
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(limit).map(|s| s.2).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<Item> {
        vec![
            Item::new(Cmd::ToggleRecording, "Start / stop recording", "Action", "record capture save video"),
            Item::new(Cmd::ToggleStreaming, "Go live / stop streaming", "Action", "stream twitch youtube broadcast"),
            Item::new(Cmd::SettingsTab("audio"), "Settings: Microphone and audio", "Settings", "mic sound volume"),
            Item::new(Cmd::SwitchScene(1), "Switch to scene: Gameplay", "Scene", ""),
        ]
    }

    #[test]
    fn finds_by_title_and_keywords() {
        let it = items();
        assert_eq!(search(&it, "rec", 5)[0].cmd, Cmd::ToggleRecording);
        assert_eq!(search(&it, "twitch", 5)[0].cmd, Cmd::ToggleStreaming);
        assert_eq!(search(&it, "mic", 5)[0].cmd, Cmd::SettingsTab("audio"));
        assert_eq!(search(&it, "game", 5)[0].cmd, Cmd::SwitchScene(1));
    }

    #[test]
    fn all_words_must_match_and_nonsense_finds_nothing() {
        let it = items();
        assert!(search(&it, "scene xyzzy", 5).is_empty());
        assert!(search(&it, "qqq", 5).is_empty());
        assert_eq!(search(&it, "", 2).len(), 2);
    }

    #[test]
    fn short_words_do_not_match_by_scattered_letters() {
        assert!(score("mic", "Go live (start streaming)", true).is_none());
        assert!(score("strmng", "Go live (start streaming)", true).is_some());
    }

    #[test]
    fn word_starts_beat_middles() {
        assert!(score("rec", "recording", true).unwrap() > score("rec", "directory", true).unwrap());
    }
}
