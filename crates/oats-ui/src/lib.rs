//! OatsNotes' egui front end (eframe): the sidebar (views + the category/project/topic tree),
//! the transcript list, the Notes view (the brain's episodes), the month Calendar, the Todos
//! view and the reminder / confirmation pop-ups,
//! the listen controls and status line, the theme (the SwiftInvert shell's dark charcoal tokens
//! and the OS UI font) and, on macOS, the native menu bar.
//!
//! This crate depends on `oats-core` and the platform-neutral `oats-status` contract only. The binary wires in the speech engine (through the
//! [`oats_core::Pipeline`] it hands over) and the microphone (through an [`AudioStarter`]) and
//! the status item (an `oats_status::StatusItem` built by the binary's chosen implementation).

pub mod app;
pub mod calendar;
pub mod menu_events;
pub mod move_to;
#[cfg(target_os = "macos")]
pub mod native_menu;
pub mod notes;
pub mod search;
#[cfg(feature = "selftest")]
pub mod selftest;
pub mod sidebar;
pub mod system_font;
pub mod theme;
pub mod timefmt;
pub mod toast;
pub mod todos;
pub mod topic;
pub mod transcript;

pub use app::{
    AppOptions, AppPrefs, AudioRequest, AudioStarter, Chime, Command, MenuEntry, MenuSection,
    MenuState, OatsApp, SHORTCUTS, StartedAudio, build_menu_model,
};
