//! The native macOS menu bar, built with `muda` (pure Rust over AppKit) from the same
//! [`OatsApp::menu_model`] the in-window egui bar draws on Linux — one source of truth for
//! labels, order, key equivalents, enabled and checked state. macOS only.
//!
//! Key handling: AppKit offers a key press to the menu bar only when it carries ⌘ (or ⌃, or is a
//! function key). Every menu key equivalent here is a ⌘-chord, so while this menu is installed
//! the app's own key monitor skips them (`OatsApp::has_native_menu`) and nothing fires twice.
//! Quit goes through [`OatsApp::request_quit`] and the window close (so prefs save and listening
//! stops cleanly, and the status item's close-hides-the-window doesn't apply), not
//! `NSApp terminate`.
//!
//! Clicks arrive through the app's single `muda` event handler ([`crate::menu_events`]), which
//! the status item's menu shares; [`OatsApp::take_native_menu_ids`] hands this menu its ids.
//!
//! The AppKit menu is `!Send` (`Rc` inside `muda`), so it lives in [`NativeMenuApp`], a thin
//! eframe wrapper around [`OatsApp`], rather than inside the app state.

use std::collections::HashMap;

use egui::{Key, KeyboardShortcut};
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{AboutMetadata, CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};

use crate::app::{Command, MenuEntry, OatsApp, menu_shortcut};

const QUIT_ID: &str = "app.quit";
const APP_NAME: &str = "OatsNotes";

enum Handle {
    Plain(MenuItem),
    Check(CheckMenuItem),
}

struct Item {
    handle: Handle,
    cmd: Command,
    enabled: bool,
    checked: Option<bool>,
    label: String,
}

/// The installed menu bar and its live item handles.
pub struct NativeMenu {
    _menu: Menu,
    items: Vec<Item>,
    ids: HashMap<String, usize>,
}

/// What the user picked in the menu bar since the last frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Picked {
    Command(Command),
    Quit,
}

fn menu_id(cmd: Command) -> String {
    format!("cmd.{cmd:?}")
}

/// egui's shortcut as a `muda` accelerator; `None` for keys the menus don't use.
pub fn accelerator(s: &KeyboardShortcut) -> Option<Accelerator> {
    let code = match s.logical_key {
        Key::A => Code::KeyA,
        Key::B => Code::KeyB,
        Key::C => Code::KeyC,
        Key::D => Code::KeyD,
        Key::E => Code::KeyE,
        Key::F => Code::KeyF,
        Key::G => Code::KeyG,
        Key::H => Code::KeyH,
        Key::I => Code::KeyI,
        Key::J => Code::KeyJ,
        Key::K => Code::KeyK,
        Key::L => Code::KeyL,
        Key::M => Code::KeyM,
        Key::N => Code::KeyN,
        Key::O => Code::KeyO,
        Key::P => Code::KeyP,
        Key::Q => Code::KeyQ,
        Key::R => Code::KeyR,
        Key::S => Code::KeyS,
        Key::T => Code::KeyT,
        Key::U => Code::KeyU,
        Key::V => Code::KeyV,
        Key::W => Code::KeyW,
        Key::X => Code::KeyX,
        Key::Y => Code::KeyY,
        Key::Z => Code::KeyZ,
        Key::OpenBracket => Code::BracketLeft,
        Key::CloseBracket => Code::BracketRight,
        Key::Comma => Code::Comma,
        Key::Num0 => Code::Digit0,
        Key::Num1 => Code::Digit1,
        Key::Num2 => Code::Digit2,
        Key::Num3 => Code::Digit3,
        Key::Num4 => Code::Digit4,
        Key::Num5 => Code::Digit5,
        Key::Num6 => Code::Digit6,
        Key::Num7 => Code::Digit7,
        Key::Num8 => Code::Digit8,
        Key::Num9 => Code::Digit9,
        _ => return None,
    };
    let m = s.modifiers;
    let mut mods = Modifiers::empty();
    if m.command || m.mac_cmd {
        mods |= Modifiers::META;
    }
    if m.shift {
        mods |= Modifiers::SHIFT;
    }
    if m.alt {
        mods |= Modifiers::ALT;
    }
    if m.ctrl {
        mods |= Modifiers::CONTROL;
    }
    Some(Accelerator::new(mods, code))
}

impl NativeMenu {
    /// Build the menu bar from the app's model and install it as the NSApp main menu.
    /// `None` (logged) when AppKit refuses — the window then keeps its own egui menu bar.
    pub fn install(app: &OatsApp) -> Option<Self> {
        let menu = Menu::new();
        let mut items = Vec::new();
        let mut ids = HashMap::new();
        let ok = |r: muda::Result<()>| {
            if let Err(e) = r {
                log::warn!("native menu: {e}");
            }
        };

        // the application menu (bold, named after the app)
        let app_menu = Submenu::new(APP_NAME, true);
        let about = PredefinedMenuItem::about(
            Some("About OatsNotes"),
            Some(AboutMetadata {
                name: Some(APP_NAME.to_owned()),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                ..Default::default()
            }),
        );
        let quit = MenuItem::with_id(
            QUIT_ID,
            "Quit OatsNotes",
            true,
            Some(Accelerator::new(Modifiers::META, Code::KeyQ)),
        );
        for it in [
            &about as &dyn muda::IsMenuItem,
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(Some("Services")),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(Some("Hide OatsNotes")),
            &PredefinedMenuItem::hide_others(Some("Hide Others")),
            &PredefinedMenuItem::show_all(Some("Show All")),
            &PredefinedMenuItem::separator(),
            &quit,
        ] {
            ok(app_menu.append(it));
        }
        ok(menu.append(&app_menu));

        // File / Edit / Listen / View / Brain from the shared model (Edit's Undo / Redo carry no
        // accelerator: `menu_shortcut` has none for them, see `app::UNDO_SHORTCUT`)
        for section in app.menu_model() {
            let sub = Submenu::new(section.title, true);
            for entry in &section.entries {
                match entry {
                    MenuEntry::Separator => ok(sub.append(&PredefinedMenuItem::separator())),
                    MenuEntry::Heading(text) => {
                        ok(sub.append(&MenuItem::new(*text, false, None)));
                    }
                    MenuEntry::Item {
                        label,
                        cmd,
                        enabled,
                        checked,
                    } => {
                        let accel = menu_shortcut(*cmd).as_ref().and_then(accelerator);
                        let id = menu_id(*cmd);
                        let handle = match checked {
                            Some(on) => {
                                let c =
                                    CheckMenuItem::with_id(id.clone(), label, *enabled, *on, accel);
                                ok(sub.append(&c));
                                Handle::Check(c)
                            }
                            None => {
                                let m = MenuItem::with_id(id.clone(), label, *enabled, accel);
                                ok(sub.append(&m));
                                Handle::Plain(m)
                            }
                        };
                        ids.insert(id, items.len());
                        items.push(Item {
                            handle,
                            cmd: *cmd,
                            enabled: *enabled,
                            checked: *checked,
                            label: label.clone(),
                        });
                    }
                }
            }
            ok(menu.append(&sub));
        }

        // the standard Window menu
        let window = Submenu::new("Window", true);
        for it in [
            &PredefinedMenuItem::minimize(Some("Minimize")) as &dyn muda::IsMenuItem,
            &PredefinedMenuItem::maximize(Some("Zoom")),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::bring_all_to_front(Some("Bring All to Front")),
        ] {
            ok(window.append(it));
        }
        ok(menu.append(&window));
        window.set_as_windows_menu_for_nsapp();

        menu.init_for_nsapp();
        log::info!("native menu bar installed ({} items)", items.len());
        Some(Self {
            _menu: menu,
            items,
            ids,
        })
    }

    /// Bring labels and enabled/checked state up to date with the app, and return what was
    /// picked: `clicked` are the menu event ids routed to this menu since the last frame.
    pub fn update(&mut self, app: &OatsApp, clicked: Vec<String>) -> Vec<Picked> {
        let mut picked = Vec::new();
        for id in clicked {
            if id == QUIT_ID {
                picked.push(Picked::Quit);
            } else if let Some(item) = self.ids.get(&id).and_then(|&i| self.items.get(i)) {
                // a disabled item can't be clicked; guard anyway
                if item.enabled {
                    picked.push(Picked::Command(item.cmd));
                }
            }
        }

        // keyed by menu id (Command isn't Hash)
        let mut state: HashMap<String, (String, bool, Option<bool>)> = HashMap::new();
        for section in app.menu_model() {
            for entry in section.entries {
                if let MenuEntry::Item {
                    label,
                    cmd,
                    enabled,
                    checked,
                } = entry
                {
                    state.insert(menu_id(cmd), (label, enabled, checked));
                }
            }
        }
        for item in &mut self.items {
            let Some((label, enabled, checked)) = state.remove(&menu_id(item.cmd)) else {
                continue;
            };
            if label != item.label {
                match &item.handle {
                    Handle::Plain(m) => m.set_text(&label),
                    Handle::Check(c) => c.set_text(&label),
                }
                item.label = label;
            }
            if enabled != item.enabled {
                match &item.handle {
                    Handle::Plain(m) => m.set_enabled(enabled),
                    Handle::Check(c) => c.set_enabled(enabled),
                }
                item.enabled = enabled;
            }
            if let (Handle::Check(c), Some(on)) = (&item.handle, checked) {
                // AppKit toggles a check item on click by itself: always re-assert the model
                if item.checked != Some(on) || c.is_checked() != on {
                    c.set_checked(on);
                }
                item.checked = Some(on);
            }
        }
        picked
    }
}

/// [`OatsApp`] with the native macOS menu bar attached: the menu is polled every frame and its
/// picks run as app commands. Everything else is the app itself.
pub struct NativeMenuApp {
    app: OatsApp,
    menu: NativeMenu,
}

impl NativeMenuApp {
    /// Install the native menu for `app`; hands the app back (with its in-window menu bar
    /// restored) when AppKit refuses.
    pub fn new(mut app: OatsApp) -> Result<Self, Box<OatsApp>> {
        match NativeMenu::install(&app) {
            Some(menu) => {
                app.set_native_menu(true);
                Ok(Self { app, menu })
            }
            None => {
                app.set_native_menu(false);
                Err(Box::new(app))
            }
        }
    }
}

impl eframe::App for NativeMenuApp {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let clicked = self.app.take_native_menu_ids();
        for p in self.menu.update(&self.app, clicked) {
            match p {
                Picked::Command(c) => self.app.run(c),
                Picked::Quit => self.app.request_quit(ctx),
            }
        }
        self.app.logic(ctx, frame);
        // and once more after the app ran: what `logic` just changed (the engine became ready,
        // the brain became available, listening stopped) must reach the menu bar now — no
        // repaint may follow, and the window may be hidden (then only `logic` runs)
        let _ = self.menu.update(&self.app, Vec::new());
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
        // commands run from the window (sidebar, pill, keys) likewise
        let _ = self.menu.update(&self.app, Vec::new());
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.app.save(storage);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.app.raw_input_hook(ctx, raw_input);
    }

    fn on_exit(&mut self) {
        self.app.on_exit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_menu_shortcut_maps_to_a_command_accelerator() {
        for (s, cmd) in crate::app::SHORTCUTS {
            let a = accelerator(s);
            assert!(a.is_some(), "{cmd:?} has no accelerator");
            assert!(
                a.is_some_and(|a| a.modifiers().contains(Modifiers::META)),
                "{cmd:?}: menu key equivalents must carry Cmd to reach the menu"
            );
        }
    }

    #[test]
    fn undo_and_redo_have_no_accelerator() {
        // AppKit would hand ⌘Z to the menu before any focused text field saw it
        for cmd in [Command::Undo, Command::Redo] {
            assert_eq!(menu_shortcut(cmd).as_ref().and_then(accelerator), None);
        }
    }

    #[test]
    fn menu_ids_are_unique_per_command() {
        let ids: std::collections::HashSet<String> = crate::app::SHORTCUTS
            .iter()
            .map(|(_, c)| menu_id(*c))
            .collect();
        assert_eq!(ids.len(), crate::app::SHORTCUTS.len());
    }

    #[test]
    fn view_and_listen_accelerators() {
        let get = |cmd| menu_shortcut(cmd).as_ref().and_then(accelerator);
        assert_eq!(
            get(Command::ShowTranscripts),
            Some(Accelerator::new(Modifiers::META, Code::Digit1))
        );
        assert_eq!(
            get(Command::ToggleListening),
            Some(Accelerator::new(Modifiers::META, Code::KeyL))
        );
        assert_eq!(
            get(Command::ToggleSidebar),
            Some(Accelerator::new(
                Modifiers::META | Modifiers::SHIFT,
                Code::KeyS
            ))
        );
    }
}
