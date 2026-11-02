//! The in-session undo / redo of the user's own quick edits (⌘Z / ⇧⌘Z, Edit ▸ Undo / Redo):
//! ticking an action item, a todo's done / snooze / due / text, a hand edit of a note, and a
//! note moved by "Move to…" when the tree resolved the name. In memory only, at most
//! [`CAPACITY`] steps, gone at quit; nothing is written to the `changes` table (that is the
//! brain's change log, with its own Undo). Deletes, merges, brain actions and preferences are
//! not recorded (and don't clear the history).

use std::collections::VecDeque;

use oats_core::{EpisodeContent, Store, StoreError, TodoState};

/// How many steps are kept; the oldest goes first.
pub const CAPACITY: usize = 50;

/// One reversible edit: enough to apply it either way.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    /// An action item ticked (`done`) or unticked; `stamp` is its `done_at` while done (so an
    /// undone untick gets its first completion time back).
    ActionItemDone {
        episode_id: i64,
        text: String,
        done: bool,
        stamp: i64,
    },
    /// A todo's hand-edited fields before and after.
    Todo {
        id: i64,
        before: TodoState,
        after: TodoState,
    },
    /// A hand edit of a note: its text and `user_edited_at` before and after.
    NoteContent {
        id: i64,
        before: Box<EpisodeContent>,
        before_edited: Option<i64>,
        after: Box<EpisodeContent>,
        after_edited: Option<i64>,
    },
    /// A note moved from `from` (`None` = unfiled) to `to`.
    MoveNote { id: i64, from: Option<i64>, to: i64 },
}

/// A step of the history: the edit and its menu / toast label ("Mark complete").
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub label: String,
    pub edit: Edit,
}

impl Entry {
    pub fn new(label: &str, edit: Edit) -> Self {
        Self {
            label: label.to_owned(),
            edit,
        }
    }

    /// Apply the edit backwards (`undo`) or forwards again, straight against the store (never
    /// through the UI's recording paths). `Ok(false)`: its target is gone.
    pub fn apply(&self, store: &Store, undo: bool) -> Result<bool, StoreError> {
        match &self.edit {
            Edit::ActionItemDone {
                episode_id,
                text,
                done,
                stamp,
            } => store.set_action_item_done(*episode_id, text, *done != undo, *stamp),
            Edit::Todo { id, before, after } => {
                store.restore_todo_state(*id, if undo { before } else { after })
            }
            Edit::NoteContent {
                id,
                before,
                before_edited,
                after,
                after_edited,
            } => {
                if undo {
                    store.restore_episode_content(*id, before, *before_edited)
                } else {
                    store.restore_episode_content(*id, after, *after_edited)
                }
            }
            Edit::MoveNote { id, from, to } => {
                store.restore_episode_node(*id, if undo { *from } else { Some(*to) })
            }
        }
    }

    /// The node a moved note sits under after this step ran in that direction.
    pub fn moved_to(&self, undo: bool) -> Option<(i64, Option<i64>)> {
        match &self.edit {
            Edit::MoveNote { id, from, to } => Some((*id, if undo { *from } else { Some(*to) })),
            _ => None,
        }
    }
}

/// The undo and redo lists: a new step clears the redo list; undo moves a step across, redo
/// moves it back.
#[derive(Clone, Debug)]
pub struct UndoStack<T> {
    undo: VecDeque<T>,
    redo: Vec<T>,
}

impl<T> Default for UndoStack<T> {
    fn default() -> Self {
        Self {
            undo: VecDeque::new(),
            redo: Vec::new(),
        }
    }
}

impl<T> UndoStack<T> {
    /// A new user action.
    pub fn push(&mut self, t: T) {
        self.redo.clear();
        self.push_undo(t);
    }

    fn push_undo(&mut self, t: T) {
        self.undo.push_back(t);
        while self.undo.len() > CAPACITY {
            self.undo.pop_front();
        }
    }

    /// The step to undo next (taken off the list; hand it to [`Self::undone`] when it worked).
    pub fn take_undo(&mut self) -> Option<T> {
        self.undo.pop_back()
    }

    /// The step to redo next (taken off; [`Self::redone`] when it worked).
    pub fn take_redo(&mut self) -> Option<T> {
        self.redo.pop()
    }

    /// `t` was undone: it can be redone.
    pub fn undone(&mut self, t: T) {
        self.redo.push(t);
    }

    /// `t` was redone: it can be undone again (the rest of the redo list stays).
    pub fn redone(&mut self, t: T) {
        self.push_undo(t);
    }

    pub fn peek_undo(&self) -> Option<&T> {
        self.undo.back()
    }

    pub fn peek_redo(&self) -> Option<&T> {
        self.redo.last()
    }

    pub fn len(&self) -> (usize, usize) {
        (self.undo.len(), self.redo.len())
    }

    pub fn is_empty(&self) -> bool {
        self.undo.is_empty() && self.redo.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oats_core::{ActionItem, NewTodo};

    #[test]
    fn undo_and_redo_run_in_order() {
        let mut s = UndoStack::default();
        s.push(1);
        s.push(2);
        s.push(3);
        let a = s.take_undo();
        assert_eq!(a, Some(3));
        s.undone(3);
        assert_eq!(s.take_undo(), Some(2));
        s.undone(2);
        assert_eq!(s.peek_redo(), Some(&2));
        assert_eq!(s.take_redo(), Some(2));
        s.redone(2);
        assert_eq!(s.peek_redo(), Some(&3), "redo keeps the rest of the list");
        assert_eq!(s.peek_undo(), Some(&2));
        assert_eq!(s.len(), (2, 1));
    }

    #[test]
    fn a_new_action_clears_redo() {
        let mut s = UndoStack::default();
        s.push(1);
        s.push(2);
        let t = s.take_undo().unwrap_or_default();
        s.undone(t);
        assert_eq!(s.len(), (1, 1));
        s.push(9);
        assert_eq!(s.len(), (2, 0));
        assert_eq!(s.take_redo(), None);
        assert_eq!(s.take_undo(), Some(9));
    }

    #[test]
    fn capacity_drops_the_oldest() {
        let mut s = UndoStack::default();
        for i in 0..CAPACITY + 5 {
            s.push(i);
        }
        assert_eq!(s.len(), (CAPACITY, 0));
        let mut last = None;
        while let Some(t) = s.take_undo() {
            last = Some(t);
        }
        assert_eq!(last, Some(5));
        assert!(s.is_empty());
    }

    #[test]
    fn a_failed_step_is_simply_dropped() {
        let mut s = UndoStack::default();
        s.push(1);
        // the caller takes it, the store says the target is gone, nothing is handed back
        assert_eq!(s.take_undo(), Some(1));
        assert!(s.is_empty());
    }

    #[test]
    fn action_item_undo_restores_the_first_completion_time() -> Result<(), StoreError> {
        let store = Store::open_in_memory()?;
        let e = store.insert_episode(
            0,
            10,
            &EpisodeContent {
                title: "Sync".into(),
                action_items: vec![ActionItem {
                    text: "ship it".into(),
                    done_at: Some(500),
                    ..Default::default()
                }],
                ..Default::default()
            },
            1,
        )?;
        let done_at = |s: &Store| -> Result<Option<i64>, StoreError> {
            Ok(s.episode(e.id)?
                .and_then(|e| e.action_items.first().and_then(|a| a.done_at)))
        };
        // the user unticked it
        assert!(store.set_action_item_done(e.id, "ship it", false, 900)?);
        let step = Entry::new(
            "Mark open",
            Edit::ActionItemDone {
                episode_id: e.id,
                text: "ship it".into(),
                done: false,
                stamp: 500,
            },
        );
        assert!(step.apply(&store, true)?);
        assert_eq!(done_at(&store)?, Some(500));
        assert!(step.apply(&store, false)?);
        assert_eq!(done_at(&store)?, None);
        let gone = Entry::new(
            "Mark open",
            Edit::ActionItemDone {
                episode_id: 9999,
                text: "ship it".into(),
                done: false,
                stamp: 500,
            },
        );
        assert!(!gone.apply(&store, true)?, "a missing note can't be undone");
        Ok(())
    }

    #[test]
    fn todo_and_move_steps_apply_both_ways() -> Result<(), StoreError> {
        let store = Store::open_in_memory()?;
        let t = store.add_todo(
            &NewTodo {
                text: "call".into(),
                ..Default::default()
            },
            1,
        )?;
        let before = t.state();
        store.complete_todo(t.id, 50)?;
        let after = store.todo(t.id)?.map(|t| t.state());
        let step = Entry::new(
            "Complete todo",
            Edit::Todo {
                id: t.id,
                before: before.clone(),
                after: after.clone().unwrap_or(before.clone()),
            },
        );
        assert!(step.apply(&store, true)?);
        assert_eq!(store.todo(t.id)?.map(|t| t.done()), Some(false));
        assert!(step.apply(&store, false)?);
        assert_eq!(store.todo(t.id)?.map(|t| t.completed_at), Some(Some(50)));

        let work = store.categories()?[0].id;
        let e = store.insert_episode(0, 10, &EpisodeContent::default(), 1)?;
        assert!(store.move_episode(e.id, work, 5)?);
        let mv = Entry::new(
            "Move note",
            Edit::MoveNote {
                id: e.id,
                from: None,
                to: work,
            },
        );
        assert!(mv.apply(&store, true)?);
        assert_eq!(store.episode(e.id)?.and_then(|e| e.node_id), None);
        assert_eq!(mv.moved_to(true), Some((e.id, None)));
        assert!(mv.apply(&store, false)?);
        assert_eq!(store.episode(e.id)?.and_then(|e| e.node_id), Some(work));
        Ok(())
    }
}
