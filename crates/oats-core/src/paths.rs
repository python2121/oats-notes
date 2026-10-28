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

/// The empty working directory the brain runs `claude` in (never the data dir itself, which
/// holds the database): `<data dir>/claude-work`.
pub fn claude_work_dir() -> PathBuf {
    data_dir().join("claude-work")
}

/// Create the data directory owner-only (0700). The default location is also tightened if it
/// already exists with a looser mode (an older build created it 0755); a directory named by
/// `OATS_DATA_DIR` that already exists is left as the user made it (it may be shared). Unix
/// only; elsewhere it is just created.
pub fn secure_data_dir() -> std::io::Result<PathBuf> {
    let dir = data_dir();
    if std::env::var_os("OATS_DATA_DIR").is_some() && dir.exists() {
        return Ok(dir);
    }
    secure_dir(&dir)?;
    Ok(dir)
}

/// [`secure_data_dir`] for any directory.
pub fn secure_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn secure_dir_creates_and_tightens() -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "oats-paths-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        super::secure_dir(&dir)?;
        assert_eq!(std::fs::metadata(&dir)?.permissions().mode() & 0o777, 0o700);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
        super::secure_dir(&dir)?;
        assert_eq!(std::fs::metadata(&dir)?.permissions().mode() & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
