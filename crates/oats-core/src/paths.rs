//! Where OatsNotes keeps its data: `~/Library/Application Support/OatsNotes` on macOS,
//! `$XDG_DATA_HOME/oatsnotes` on Linux (via `directories`). `OATS_DATA_DIR` overrides (tests,
//! a second profile).

use std::path::PathBuf;

/// The data directory (not created here).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("OATS_DATA_DIR") {
        return PathBuf::from(dir);
    }
    directories::ProjectDirs::from("", "", "OatsNotes")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".").join("oatsnotes-data"))
}

/// The transcript database.
pub fn database_path() -> PathBuf {
    data_dir().join("oats.sqlite")
}
