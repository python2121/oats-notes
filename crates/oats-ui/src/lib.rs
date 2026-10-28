//! OatsNotes' egui front end (eframe): the sidebar (views + categories), the transcript list,
//! the listen controls and status line, the theme (the SwiftInvert shell's dark charcoal tokens
//! and the OS UI font) and, on macOS, the native menu bar.
//!
//! This crate depends on `oats-core` only. The binary wires in the speech engine (through the
//! [`oats_core::Pipeline`] it hands over) and the microphone (through an [`AudioStarter`]).

pub mod app;
#[cfg(target_os = "macos")]
pub mod native_menu;
#[cfg(feature = "selftest")]
pub mod selftest;
pub mod sidebar;
pub mod system_font;
pub mod theme;
pub mod transcript;

pub use app::{
    AppOptions, AppPrefs, AudioStarter, Command, MenuEntry, MenuSection, MenuState, OatsApp,
    SHORTCUTS, build_menu_model,
};
