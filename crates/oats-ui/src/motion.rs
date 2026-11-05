//! Completing a task is a transition, not a jump (the Things / Reminders pattern): the store is
//! updated at once, but the list keeps drawing the row in its OLD place for a hold
//! ([`HOLD_DONE`], [`HOLD_REOPEN`] for the mirror), ticked and dimmed, so the user sees what
//! they did and can untick to cancel; then the old slot retracts while the row's new slot
//! expands ([`crate::widgets::reveal`]), so the row "moves down" into the completed block (or
//! back up). Used by the Todos view (key: the todo id) and by action items on note cards and
//! the Next steps (key: episode id + item text).
//!
//! Each list arranges its items with [`arrange`] (the data order plus an `Old` copy where the
//! row sat last frame) and remembers what it drew ([`record`]), so the old place is known per
//! list without the list knowing the sort rules. Nothing here polls: the hold schedules one
//! repaint for its end, the move one for its end, and egui's animations repaint only while
//! they run.

use std::hash::Hash;

use egui::{Context, Id};

/// How long a completed row stays in its old place before it moves (seconds).
pub const HOLD_DONE: f64 = 0.6;
/// The same for a reopened row (the mirror).
pub const HOLD_REOPEN: f64 = 0.3;
/// How long the move lasts: the longer of the reveal's open / close, plus a little.
pub const MOVE: f64 = 0.25;

/// One row in transition.
#[derive(Clone, Debug, PartialEq)]
pub struct Moving<K> {
    pub key: K,
    /// The state it moves to (true = completed).
    pub done: bool,
    /// The egui time it was first seen by a list (`None` until then).
    pub since: Option<f64>,
    /// The egui time the hold ended and the move began.
    pub moved_at: Option<f64>,
    /// Unique per transition: the reveal ids carry it, so each transition's slots start fresh.
    pub serial: u64,
}

/// The rows in transition in one view.
#[derive(Clone, Debug, PartialEq)]
pub struct Moves<K> {
    pub items: Vec<Moving<K>>,
    serial: u64,
}

impl<K> Default for Moves<K> {
    fn default() -> Self {
        Moves {
            items: Vec::new(),
            serial: 0,
        }
    }
}

impl<K: Clone + PartialEq> Moves<K> {
    /// The user toggled `key` to `done`: start its transition — or, when it is already in one
    /// (an untick during the hold), cancel it, so the row simply stays where it is.
    pub fn toggle(&mut self, key: K, done: bool) {
        if let Some(i) = self.items.iter().position(|m| m.key == key) {
            self.items.remove(i);
            return;
        }
        self.serial += 1;
        self.items.push(Moving {
            key,
            done,
            since: None,
            moved_at: None,
            serial: self.serial,
        });
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Drop the transitions whose item now reads otherwise (an undo, a failed write, a reload
    /// that changed it): `items` are (key, completed) as the data says.
    pub fn drop_mismatched(&mut self, items: impl IntoIterator<Item = (K, bool)>) {
        for (k, done) in items {
            self.items.retain(|m| m.key != k || m.done == done);
        }
    }

    /// Keep only the transitions whose key is among `keys` (a reload found the item gone).
    pub fn retain_keys(&mut self, keys: &[K]) {
        self.items.retain(|m| keys.contains(&m.key));
    }

    /// Advance the phases to egui's clock: stamp new transitions, start the move after the
    /// hold, end it after [`MOVE`]. Schedules one repaint for the next boundary; no repaint at
    /// all once nothing is moving. Idempotent within a frame. With motion off (egui's
    /// `animation_time` 0) nothing moves.
    pub fn tick(&mut self, ctx: &Context) {
        if self.items.is_empty() {
            return;
        }
        if !crate::widgets::motion(ctx) {
            self.items.clear();
            return;
        }
        let now = ctx.input(|i| i.time);
        self.items.retain_mut(|m| {
            let since = *m.since.get_or_insert(now);
            let hold = if m.done { HOLD_DONE } else { HOLD_REOPEN };
            if m.moved_at.is_none() && now < since + hold {
                ctx.request_repaint_after_secs((since + hold - now) as f32);
                return true;
            }
            let moved = *m.moved_at.get_or_insert(now);
            if now < moved + MOVE {
                ctx.request_repaint_after_secs((moved + MOVE - now) as f32);
                true
            } else {
                false
            }
        });
    }
}

/// Where one entry of an arranged list is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Slot {
    /// In its place, as the data says.
    Plain,
    /// The copy in its OLD place: shown in full during the hold, retracting during the move.
    Old { serial: u64, moving: bool },
    /// Its NEW place (the data's): closed during the hold, expanding during the move.
    New { serial: u64, moving: bool },
}

impl Slot {
    /// Whether the entry reads as completed where it is drawn (`done` = the data's state): an
    /// old copy still sits among the items of the other state.
    pub fn shows_done(self, done: bool) -> bool {
        match self {
            Slot::Old { .. } => !done,
            _ => done,
        }
    }

    /// Draw `add` in this slot: plainly, or through a [`crate::widgets::reveal`] keyed on
    /// `salt` and the transition. `None` when the slot is closed (nothing laid out).
    pub fn show<R>(
        self,
        ui: &mut egui::Ui,
        salt: impl Hash + Copy + std::fmt::Debug,
        add: impl FnOnce(&mut egui::Ui) -> R,
    ) -> Option<R> {
        match self {
            Slot::Plain => Some(add(ui)),
            Slot::Old { serial, moving } => {
                crate::widgets::reveal(ui, Id::new((salt, "old", serial)), !moving, add)
            }
            Slot::New { serial, moving } => {
                let id = Id::new((salt, "new", serial));
                if moving {
                    crate::widgets::reveal_seed_height(
                        ui.ctx(),
                        Id::new((salt, "old", serial)),
                        id,
                    );
                }
                crate::widgets::reveal(ui, id, moving, add)
            }
        }
    }
}

/// What a list drew last frame: (key, is an old copy).
type Drawn<K> = (u64, Vec<(K, bool)>);

/// The list `list` drew last frame (or this one), if it did.
fn drawn<K: Clone + Send + Sync + 'static>(ctx: &Context, list: Id) -> Option<Vec<(K, bool)>> {
    let frame = ctx.cumulative_frame_nr();
    ctx.data(|d| d.get_temp::<Drawn<K>>(list))
        .filter(|(f, _)| f + 1 >= frame)
        .map(|(_, v)| v)
}

/// Arrange a list for drawing: `keys` = its items in data order with their completed state;
/// returns (index into `keys`, slot) in drawing order. A transition gets an `Old` copy where
/// its row sat in what this list drew last frame — only when the list showed the row then
/// (an old copy continues; a plain row becomes one on the transition's first frame) — and its
/// data place becomes `New`. Records this frame's drawing for the next.
pub fn arrange<K: Clone + PartialEq + Send + Sync + 'static>(
    ctx: &Context,
    list: Id,
    keys: &[(K, bool)],
    moves: &Moves<K>,
) -> Vec<(usize, Slot)> {
    let now = ctx.input(|i| i.time);
    let prev = drawn::<K>(ctx, list);
    let out = arrange_with(keys, moves, prev.as_deref(), now);
    let record: Vec<(K, bool)> = out
        .iter()
        .filter_map(|(i, s)| match s {
            Slot::New { .. } => None,
            Slot::Old { .. } => Some((keys[*i].0.clone(), true)),
            Slot::Plain => Some((keys[*i].0.clone(), false)),
        })
        .collect();
    let frame = ctx.cumulative_frame_nr();
    ctx.data_mut(|d| d.insert_temp::<Drawn<K>>(list, (frame, record)));
    out
}

/// [`arrange`]'s pure part (`prev`: what the list drew last frame).
pub fn arrange_with<K: PartialEq>(
    keys: &[(K, bool)],
    moves: &Moves<K>,
    prev: Option<&[(K, bool)]>,
    now: f64,
) -> Vec<(usize, Slot)> {
    let mut out: Vec<(usize, Slot)> = (0..keys.len()).map(|i| (i, Slot::Plain)).collect();
    let Some(prev) = prev else {
        return out;
    };
    let mut olds = Vec::new();
    for m in &moves.items {
        let Some(i) = keys.iter().position(|(k, _)| *k == m.key) else {
            continue;
        };
        if keys[i].1 != m.done {
            continue;
        }
        let Some(p) = prev.iter().position(|(k, _)| *k == m.key) else {
            continue;
        };
        // a plain row becomes an old copy only on the transition's first frame (a list that
        // showed the row in its new place already keeps it there)
        if !prev[p].1 && m.since.is_some_and(|s| s < now) {
            continue;
        }
        let moving = m.moved_at.is_some();
        out[i].1 = Slot::New {
            serial: m.serial,
            moving,
        };
        olds.push((p, i, m.serial, moving));
    }
    olds.sort_by_key(|o| o.0);
    for (p, i, serial, moving) in olds {
        // the p-th place among the entries drawn last frame (new places weren't)
        let mut seen = 0;
        let mut at = out.len();
        for (j, e) in out.iter().enumerate() {
            if seen == p {
                at = j;
                break;
            }
            if !matches!(e.1, Slot::New { .. }) {
                seen += 1;
            }
        }
        out.insert(at, (i, Slot::Old { serial, moving }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moving(key: i64, done: bool, since: Option<f64>, moved: bool) -> Moves<i64> {
        let mut m = Moves::default();
        m.toggle(key, done);
        m.items[0].since = since;
        m.items[0].moved_at = moved.then_some(1.0);
        m
    }

    /// A completed row keeps its old place (an old copy) and gets its new place (closed).
    #[test]
    fn a_completed_row_is_drawn_in_its_old_place_and_its_new_one() {
        // data after the tick: b moved to the completed block
        let keys = [(1, false), (3, false), (2, true), (9, true)];
        let prev = [(1, false), (2, false), (3, false), (9, false)];
        let m = moving(2, true, None, false);
        let out = arrange_with(&keys, &m, Some(&prev), 0.0);
        let serial = m.items[0].serial;
        assert_eq!(
            out,
            vec![
                (0, Slot::Plain),
                (
                    2,
                    Slot::Old {
                        serial,
                        moving: false
                    }
                ),
                (1, Slot::Plain),
                (
                    2,
                    Slot::New {
                        serial,
                        moving: false
                    }
                ),
                (3, Slot::Plain),
            ]
        );
        assert!(!out[1].1.shows_done(true), "the old copy reads as open");
        // the next frames: the old copy continues where it was drawn
        let prev2 = [(1, false), (2, true), (3, false), (9, false)];
        let out2 = arrange_with(&keys, &m, Some(&prev2), 5.0);
        assert_eq!(out2, out);
    }

    #[test]
    fn a_list_that_did_not_show_the_row_draws_it_plainly() {
        let keys = [(1, false), (2, true)];
        let m = moving(2, true, Some(0.0), false);
        assert_eq!(
            arrange_with(&keys, &m, None, 1.0),
            vec![(0, Slot::Plain), (1, Slot::Plain)]
        );
        // it showed the row already in its new place (a plain row after the first frame)
        let prev = [(1, false), (2, false)];
        assert_eq!(
            arrange_with(&keys, &m, Some(&prev), 1.0),
            vec![(0, Slot::Plain), (1, Slot::Plain)]
        );
    }

    #[test]
    fn a_mismatched_transition_is_ignored_and_dropped() {
        let keys = [(1, false), (2, false)];
        let prev = [(1, false), (2, false)];
        let mut m = moving(2, true, None, false);
        assert_eq!(
            arrange_with(&keys, &m, Some(&prev), 0.0),
            vec![(0, Slot::Plain), (1, Slot::Plain)]
        );
        m.drop_mismatched(keys.iter().copied());
        assert!(m.is_empty());
    }

    #[test]
    fn toggling_a_row_in_transition_cancels_it() {
        let mut m = Moves::default();
        m.toggle(4, true);
        assert_eq!(m.items.len(), 1);
        m.toggle(4, false);
        assert!(m.is_empty());
        m.toggle(4, false);
        m.toggle(5, true);
        assert_eq!(m.items[0].serial + 1, m.items[1].serial);
    }

    /// The phases follow egui's clock and end; nothing is left requesting repaints.
    #[test]
    fn the_hold_then_the_move_then_nothing() {
        let ctx = Context::default();
        ctx.global_style_mut(|s| s.animation_time = 0.1);
        let mut m: Moves<i64> = Moves::default();
        m.toggle(1, true);
        let mut t = 0.0;
        let frame = |m: &mut Moves<i64>, t: f64| {
            let input = egui::RawInput {
                time: Some(t),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| m.tick(ui.ctx()));
            out.textures_delta.clear();
        };
        frame(&mut m, t);
        assert_eq!(m.items[0].since, Some(0.0));
        t += HOLD_DONE / 2.0;
        frame(&mut m, t);
        assert!(m.items[0].moved_at.is_none(), "still holding");
        t = HOLD_DONE + 0.01;
        frame(&mut m, t);
        assert!(m.items[0].moved_at.is_some(), "moving");
        t += MOVE + 0.01;
        frame(&mut m, t);
        assert!(m.is_empty(), "done");
        // motion off: a transition ends at once
        ctx.global_style_mut(|s| s.animation_time = 0.0);
        m.toggle(2, true);
        frame(&mut m, t);
        assert!(m.is_empty());
    }
}
