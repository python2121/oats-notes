//! The status-bar item contract: the small icon + menu the app keeps in the system's status
//! area (the macOS menu bar extra; on KDE/Linux a StatusNotifierItem). Like the speech engine,
//! it is a swappable component: this crate is the platform-neutral contract, an implementation
//! crate per platform (`oats-status-macos`, …) provides a [`StatusItemFactory`], and the binary
//! picks one. The UI only ever sees `Box<dyn StatusItem>`.
//!
//! The UI pushes [`StatusState`] in ([`StatusItem::update`]) and drains [`StatusAction`]s out
//! ([`StatusItem::poll`]) once per frame — also while the window is hidden, which is when the
//! status item matters most.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

/// What the status item shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusState {
    /// Listening is on (the icon shows the "live" variant, the menu offers "Stop Listening").
    pub listening: bool,
    /// The listen toggle can be used; `false` while the engine is loading or has failed.
    pub enabled: bool,
    /// The hover text, e.g. "OatsNotes — Listening · 12 segments today".
    pub tooltip: String,
}

/// What the user did with the status item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusAction {
    /// Start or stop listening.
    ToggleListening,
    /// Bring the (possibly hidden) main window back to the front.
    ShowWindow,
    /// Quit the app for real (closing the window only hides it while a status item exists).
    Quit,
}

/// A platform status item. Created on the UI thread (AppKit requires it) and polled from the
/// UI thread every frame; it need not be `Send`.
pub trait StatusItem {
    /// Show `state`. Called only when the state changed; implementations still only re-set
    /// what differs from what they show.
    fn update(&mut self, state: &StatusState);

    /// The actions picked since the last call, oldest first.
    fn poll(&mut self) -> Vec<StatusAction>;

    /// A menu event id from a stream the item shares with the host. On platforms where menu
    /// events arrive through one global stream (macOS: `muda`'s single event handler, also used
    /// by the app's menu bar) the host drains that stream and forwards every id here first;
    /// the item returns `true` when the id was one of its own (and queues the action for
    /// [`Self::poll`]), `false` to let the host route it elsewhere. Items with their own event
    /// delivery (e.g. a D-Bus StatusNotifierItem) keep the default, which claims nothing.
    fn handle_menu_id(&mut self, _id: &str) -> bool {
        false
    }
}

/// Builds the platform status item. Called once, on the UI thread, after the platform's
/// application object exists (on macOS: inside eframe's app-creator closure). The argument is
/// the `wake` callback: implementations that deliver events on another thread call it after
/// queueing an action, so the UI runs a frame and polls (the host passes
/// `egui::Context::request_repaint`). `Err` = no status item; the app runs without one.
pub type StatusItemFactory =
    Box<dyn FnOnce(Box<dyn Fn() + Send + Sync>) -> Result<Box<dyn StatusItem>, String>>;

/// A status item that shows nothing and is never clicked: platforms without one, and tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoStatusItem;

impl StatusItem for NoStatusItem {
    fn update(&mut self, _state: &StatusState) {}

    fn poll(&mut self) -> Vec<StatusAction> {
        Vec::new()
    }
}

/// A factory for [`NoStatusItem`].
pub fn no_status_item() -> StatusItemFactory {
    Box::new(|_wake| Ok(Box::new(NoStatusItem) as Box<dyn StatusItem>))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_status_item_polls_empty_and_claims_no_ids() {
        let mut item = match no_status_item()(Box::new(|| {})) {
            Ok(item) => item,
            Err(e) => unreachable!("NoStatusItem can't fail: {e}"),
        };
        item.update(&StatusState {
            listening: true,
            enabled: true,
            tooltip: "OatsNotes — Listening".to_owned(),
        });
        assert!(item.poll().is_empty());
        assert!(!item.handle_menu_id("status.toggle"));
        assert!(item.poll().is_empty());
    }
}
