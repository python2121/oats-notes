//! The app's one menu event stream, shared by the native menu bar and the status item.
//!
//! On macOS both menus are `muda` menus, and `muda` delivers every click through ONE global
//! event handler — a second `set_event_handler` would replace the first. So the app installs
//! the handler once ([`MenuEvents::install`]): it forwards each id over a channel and wakes egui
//! (a menu click doesn't, by itself, and the window may be hidden). Once per frame
//! [`MenuEvents::route`] drains the channel: each id is offered to the status item first
//! ([`oats_status::StatusItem::handle_menu_id`]; its ids start with `status.`), and the rest
//! are kept for the native menu bar, which takes them with [`MenuEvents::take_native`].
//!
//! The routing is platform-neutral (plain strings); only `install` is macOS-specific.

use std::sync::mpsc::Receiver;

use oats_status::StatusItem;

/// The drained-but-not-yet-taken menu ids.
pub struct MenuEvents {
    rx: Receiver<String>,
    native: Vec<String>,
}

impl MenuEvents {
    /// Route ids arriving on `rx` (tests; [`Self::install`] on macOS).
    pub fn from_receiver(rx: Receiver<String>) -> Self {
        Self {
            rx,
            native: Vec::new(),
        }
    }

    /// Install the app's single `muda` event handler: every menu click's id goes into the
    /// returned stream, and egui is woken to route it.
    #[cfg(target_os = "macos")]
    pub fn install(ctx: &egui::Context) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let repaint = ctx.clone();
        muda::MenuEvent::set_event_handler(Some(move |e: muda::MenuEvent| {
            let _ = tx.send(e.id.0);
            repaint.request_repaint();
        }));
        Self::from_receiver(rx)
    }

    /// Drain the stream: ids the status item claims go to it, the rest are kept for the native
    /// menu bar when `keep_rest` (dropped otherwise — nothing else could own them).
    pub fn route(&mut self, mut status: Option<&mut (dyn StatusItem + '_)>, keep_rest: bool) {
        while let Ok(id) = self.rx.try_recv() {
            if let Some(s) = status.as_deref_mut()
                && s.handle_menu_id(&id)
            {
                continue;
            }
            if keep_rest {
                self.native.push(id);
            } else {
                log::debug!("menu event {id:?} has no owner");
            }
        }
    }

    /// The ids routed to the native menu bar since the last call, oldest first.
    pub fn take_native(&mut self) -> Vec<String> {
        std::mem::take(&mut self.native)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oats_status::{StatusAction, StatusState};

    #[derive(Default)]
    struct Claims(Vec<String>);

    impl StatusItem for Claims {
        fn update(&mut self, _state: &StatusState) {}
        fn poll(&mut self) -> Vec<StatusAction> {
            Vec::new()
        }
        fn handle_menu_id(&mut self, id: &str) -> bool {
            let mine = id.starts_with("status.");
            if mine {
                self.0.push(id.to_owned());
            }
            mine
        }
    }

    #[test]
    fn status_ids_go_to_the_status_item_and_the_rest_to_the_menu_bar() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut ev = MenuEvents::from_receiver(rx);
        let mut item = Claims::default();
        for id in ["cmd.ShowAll", "status.toggle", "app.quit", "status.quit"] {
            let _ = tx.send(id.to_owned());
        }
        ev.route(Some(&mut item), true);
        assert_eq!(item.0, ["status.toggle", "status.quit"]);
        assert_eq!(ev.take_native(), ["cmd.ShowAll", "app.quit"]);
        assert!(ev.take_native().is_empty());

        // without a status item or a native menu, nothing piles up
        let _ = tx.send("cmd.ShowAll".to_owned());
        ev.route(None, false);
        assert!(ev.take_native().is_empty());
    }
}
