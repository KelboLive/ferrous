//! Undo and redo for scene edits: snapshots of the scene collection, taken
//! once an edit has settled (button released, field left), so dragging a
//! source or a slider is one step, not hundreds.

use std::time::{Duration, Instant};

use ferrous_core::SceneCollection;

const LIMIT: usize = 100;
/// How often the collection is compared with the last snapshot.
const CHECK_EVERY: Duration = Duration::from_millis(150);

pub struct History {
    undo: Vec<SceneCollection>,
    redo: Vec<SceneCollection>,
    base: SceneCollection,
    checked: Instant,
}

/// Which scene is live isn't an edit, so it isn't compared or undone.
fn same(a: &SceneCollection, b: &SceneCollection) -> bool {
    a.scenes == b.scenes && a.transition == b.transition
}

impl History {
    pub fn new(now: &SceneCollection) -> Self {
        Self { undo: Vec::new(), redo: Vec::new(), base: now.clone(), checked: Instant::now() }
    }

    /// Forgets everything (a different collection was loaded).
    pub fn reset(&mut self, now: &SceneCollection) {
        self.undo.clear();
        self.redo.clear();
        self.base = now.clone();
    }

    /// Call every frame. `busy` while the user is still dragging or typing.
    pub fn observe(&mut self, now: &SceneCollection, busy: bool) {
        if busy || self.checked.elapsed() < CHECK_EVERY {
            return;
        }
        self.checked = Instant::now();
        if !same(now, &self.base) {
            self.undo.push(std::mem::replace(&mut self.base, now.clone()));
            if self.undo.len() > LIMIT {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Restores the previous snapshot into `now`, keeping the live scene.
    pub fn undo(&mut self, now: &mut SceneCollection) -> bool {
        let Some(prev) = self.undo.pop() else { return false };
        self.step(now, prev, true)
    }

    pub fn redo(&mut self, now: &mut SceneCollection) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        self.step(now, next, false)
    }

    fn step(&mut self, now: &mut SceneCollection, target: SceneCollection, undoing: bool) -> bool {
        // Edits not yet snapshotted go onto the opposite stack with the rest.
        let current = std::mem::replace(&mut self.base, target.clone());
        let from = if same(now, &current) { current } else { now.clone() };
        if undoing { &mut self.redo } else { &mut self.undo }.push(from);
        let live = now.current;
        *now = target;
        now.current = live.min(now.scenes.len().saturating_sub(1));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrous_core::SourceConfig;

    fn settle(h: &mut History, c: &SceneCollection) {
        h.checked -= CHECK_EVERY;
        h.observe(c, false);
    }

    #[test]
    fn edits_undo_and_redo_in_order_and_keep_the_live_scene() {
        let mut c = SceneCollection::default();
        let mut h = History::new(&c);
        let n = c.scenes[0].items.len();
        c.add_item_to(0, "a", SourceConfig::Color { bgra: [0; 4], width: 4, height: 4 });
        settle(&mut h, &c);
        c.add_item_to(0, "b", SourceConfig::Color { bgra: [0; 4], width: 4, height: 4 });
        settle(&mut h, &c);
        assert!(h.can_undo() && !h.can_redo());
        assert!(h.undo(&mut c));
        assert_eq!(c.scenes[0].items.len(), n + 1);
        assert!(h.undo(&mut c));
        assert_eq!(c.scenes[0].items.len(), n);
        assert!(!h.undo(&mut c));
        assert!(h.redo(&mut c) && h.redo(&mut c));
        assert_eq!(c.scenes[0].items.len(), n + 2);
        assert!(!h.can_redo());
    }

    #[test]
    fn a_busy_edit_is_one_step_and_a_new_edit_clears_redo() {
        let mut c = SceneCollection::default();
        let id = c.add_item_to(0, "a", SourceConfig::Color { bgra: [0; 4], width: 4, height: 4 });
        let mut h = History::new(&c);
        for x in 0..50 {
            c.scenes[0].items.last_mut().unwrap().transform.x = x as f32;
            h.checked -= CHECK_EVERY;
            h.observe(&c, true); // still dragging
        }
        assert!(!h.can_undo());
        settle(&mut h, &c);
        assert_eq!(h.undo.len(), 1);
        h.undo(&mut c);
        assert_eq!(c.scenes[0].items.last().unwrap().transform.x, 0.0);
        assert!(h.can_redo());
        c.scenes[0].items.retain(|i| i.id != id);
        settle(&mut h, &c);
        assert!(!h.can_redo());
    }

    #[test]
    fn switching_scenes_is_not_an_edit() {
        let mut c = SceneCollection::default();
        c.scenes.push(ferrous_core::Scene::new("B"));
        let mut h = History::new(&c);
        c.current = 1;
        settle(&mut h, &c);
        assert!(!h.can_undo());
    }
}
