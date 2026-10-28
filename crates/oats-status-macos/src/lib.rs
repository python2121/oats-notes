//! The macOS status item (a menu bar extra), built with `tray-icon` (pure Rust over AppKit via
//! objc2): a template waveform icon that macOS tints for light and dark menu bars, with a
//! "live" dot while listening, and a menu — Start/Stop Listening, Show OatsNotes, Quit.
//!
//! This is the macOS implementation of the [`oats_status`] contract; a KDE/Linux crate
//! (StatusNotifierItem, e.g. over `ksni`) plugs in the same way. Off macOS this crate still
//! exports [`factory`], which returns `Err`, so the app compiles everywhere.
//!
//! Menu events: `tray-icon`'s menu is `muda`, whose event stream is global and shared with the
//! app's native menu bar. This crate installs no event handler of its own; the host drains the
//! one stream and forwards ids to [`oats_status::StatusItem::handle_menu_id`]. Every id here
//! starts with [`ID_PREFIX`].

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use oats_status::StatusItemFactory;

/// The prefix of every menu id this item owns (the native menu bar uses `cmd.` / `app.`).
pub const ID_PREFIX: &str = "status.";

/// The status item factory. Call it on the main thread after `NSApplication` exists (inside
/// eframe's app-creator closure).
#[cfg(target_os = "macos")]
pub fn factory() -> StatusItemFactory {
    Box::new(|wake| {
        mac::MacStatusItem::new(wake).map(|s| Box::new(s) as Box<dyn oats_status::StatusItem>)
    })
}

/// The status item factory. No status item off macOS (from this crate).
#[cfg(not(target_os = "macos"))]
pub fn factory() -> StatusItemFactory {
    Box::new(|_wake| Err("no status item on this platform".to_owned()))
}

/// The icon raster: 44 px square, shown at 22 pt (2×).
pub const ICON_PX: u32 = 44;

/// The app glyph's five bars (relative heights), as in `scripts/generate_icon.swift`.
const BAR_HEIGHTS: [f32; 5] = [0.34, 0.62, 0.86, 0.56, 0.30];

/// The status icon as straight RGBA (black + alpha: a template image), `ICON_PX` square.
/// `listening` adds a filled dot in the top-right corner.
pub fn icon_rgba(listening: bool) -> Vec<u8> {
    // the Swift icon's proportions (bar 70, gap 46, tallest 470 on 1024), scaled so the
    // tallest possible bar is 30 px (15 pt) of the 44 px canvas
    let size = ICON_PX as f32;
    let scale = 30.0 / 470.0;
    let bar_w = 70.0 * scale;
    let gap = 46.0 * scale;
    let max_h = 470.0 * scale;
    let total_w = 5.0 * bar_w + 4.0 * gap;
    let left = size / 2.0 - total_w / 2.0;
    let bars: Vec<(f32, f32, f32, f32)> = BAR_HEIGHTS
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let x0 = left + i as f32 * (bar_w + gap);
            let bh = max_h * h;
            (x0, size / 2.0 - bh / 2.0, x0 + bar_w, size / 2.0 + bh / 2.0)
        })
        .collect();
    // the "live" dot: clear of the two short right-hand bars
    let dot = (size - 6.0, 7.0, 4.5);

    // a capsule (bar with fully rounded ends) and a disc, sampled 4×4 per pixel
    let in_bar = |x: f32, y: f32, (x0, y0, x1, y1): (f32, f32, f32, f32)| {
        let r = (x1 - x0) / 2.0;
        let cx = (x0 + x1) / 2.0;
        let cy = y.clamp(y0 + r, (y1 - r).max(y0 + r));
        (x - cx).powi(2) + (y - cy).powi(2) <= r * r
    };
    let in_dot = |x: f32, y: f32| (x - dot.0).powi(2) + (y - dot.1).powi(2) <= dot.2 * dot.2;

    const SS: u32 = 4;
    let mut out = Vec::with_capacity((ICON_PX * ICON_PX * 4) as usize);
    for py in 0..ICON_PX {
        for px in 0..ICON_PX {
            let mut hits = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                    if bars.iter().any(|b| in_bar(x, y, *b)) || (listening && in_dot(x, y)) {
                        hits += 1;
                    }
                }
            }
            let alpha = (hits * 255 / (SS * SS)) as u8;
            out.extend_from_slice(&[0, 0, 0, alpha]);
        }
    }
    out
}

#[cfg(target_os = "macos")]
mod mac {
    use oats_status::{StatusAction, StatusItem, StatusState};
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    use super::{ICON_PX, icon_rgba};

    const TOGGLE_ID: &str = "status.toggle";
    const SHOW_ID: &str = "status.show";
    const QUIT_ID: &str = "status.quit";

    fn icon(listening: bool) -> Result<Icon, String> {
        Icon::from_rgba(icon_rgba(listening), ICON_PX, ICON_PX).map_err(|e| e.to_string())
    }

    /// The installed menu bar extra. Dropping it removes it from the menu bar.
    pub struct MacStatusItem {
        tray: TrayIcon,
        toggle: MenuItem,
        idle_icon: Icon,
        live_icon: Icon,
        shown: StatusState,
        queued: Vec<StatusAction>,
        /// Unused on macOS: menu events reach us through the host on the UI thread (whose
        /// `muda` handler already wakes egui). Kept for the contract.
        _wake: Box<dyn Fn() + Send + Sync>,
    }

    impl MacStatusItem {
        pub fn new(wake: Box<dyn Fn() + Send + Sync>) -> Result<Self, String> {
            let shown = StatusState {
                listening: false,
                enabled: false,
                tooltip: "OatsNotes".to_owned(),
            };
            let toggle = MenuItem::with_id(TOGGLE_ID, "Start Listening", shown.enabled, None);
            let menu = Menu::new();
            let sep_a = PredefinedMenuItem::separator();
            let show = MenuItem::with_id(SHOW_ID, "Show OatsNotes", true, None);
            let sep_b = PredefinedMenuItem::separator();
            let quit = MenuItem::with_id(QUIT_ID, "Quit OatsNotes", true, None);
            menu.append_items(&[&toggle, &sep_a, &show, &sep_b, &quit])
                .map_err(|e| format!("status menu: {e}"))?;
            let idle_icon = icon(false)?;
            let live_icon = icon(true)?;
            let tray = TrayIconBuilder::new()
                .with_id("oatsnotes")
                .with_icon_templated(idle_icon.clone())
                .with_tooltip(&shown.tooltip)
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(true)
                .with_autosave_name("OatsNotesStatusItem")
                .build()
                .map_err(|e| format!("status item: {e}"))?;
            log::info!("status item installed in the menu bar");
            Ok(Self {
                tray,
                toggle,
                idle_icon,
                live_icon,
                shown,
                queued: Vec::new(),
                _wake: wake,
            })
        }
    }

    impl StatusItem for MacStatusItem {
        fn update(&mut self, state: &StatusState) {
            if state.listening != self.shown.listening {
                let icon = if state.listening {
                    &self.live_icon
                } else {
                    &self.idle_icon
                };
                if let Err(e) = self.tray.set_icon_templated(Some(icon.clone())) {
                    log::warn!("status item icon: {e}");
                }
                self.toggle.set_text(if state.listening {
                    "Stop Listening"
                } else {
                    "Start Listening"
                });
            }
            if state.enabled != self.shown.enabled {
                self.toggle.set_enabled(state.enabled);
            }
            if state.tooltip != self.shown.tooltip
                && let Err(e) = self.tray.set_tooltip(Some(&state.tooltip))
            {
                log::warn!("status item tooltip: {e}");
            }
            self.shown = state.clone();
        }

        fn poll(&mut self) -> Vec<StatusAction> {
            std::mem::take(&mut self.queued)
        }

        fn handle_menu_id(&mut self, id: &str) -> bool {
            let action = match id {
                TOGGLE_ID => StatusAction::ToggleListening,
                SHOW_ID => StatusAction::ShowWindow,
                QUIT_ID => StatusAction::Quit,
                _ => return false,
            };
            // a disabled item can't be clicked; guard anyway
            if action != StatusAction::ToggleListening || self.shown.enabled {
                self.queued.push(action);
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(rgba: &[u8], x: u32, y: u32) -> u8 {
        rgba[((y * ICON_PX + x) * 4 + 3) as usize]
    }

    #[test]
    fn icons_are_black_template_rasters() {
        for listening in [false, true] {
            let rgba = icon_rgba(listening);
            assert_eq!(rgba.len(), (ICON_PX * ICON_PX * 4) as usize);
            assert!(rgba.chunks(4).all(|p| p[..3] == [0, 0, 0]));
            // the middle (tallest) bar is solid at the centre, the corners are clear
            assert_eq!(alpha_at(&rgba, ICON_PX / 2, ICON_PX / 2), 255);
            assert_eq!(alpha_at(&rgba, 0, ICON_PX - 1), 0);
        }
    }

    #[test]
    fn listening_icon_adds_the_dot() {
        let idle = icon_rgba(false);
        let live = icon_rgba(true);
        assert_eq!(alpha_at(&idle, ICON_PX - 6, 7), 0);
        assert_eq!(alpha_at(&live, ICON_PX - 6, 7), 255);
        let ink = |v: &[u8]| v.chunks(4).map(|p| p[3] as u32).sum::<u32>();
        assert!(ink(&live) > ink(&idle));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn factory_fails_off_macos() {
        assert!(factory()(Box::new(|| {})).is_err());
    }
}
