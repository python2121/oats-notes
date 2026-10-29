//! Firefox's session store, read from disk: which tab each window shows.
//!
//! Firefox writes its running session to
//! `<profile>/sessionstore-backups/recovery.jsonlz4` (macOS profiles live under
//! `~/Library/Application Support/Firefox/Profiles/`, Linux under `~/.mozilla/firefox/`).
//! The format is "mozlz4": the 8-byte magic `mozLz40\0`, the decompressed size as a 4-byte
//! little-endian integer, then one LZ4 block of JSON. In the JSON, `windows[].selected` is the
//! 1-based index of the window's selected tab, `windows[].tabs[].index` the 1-based index of
//! the tab's current history entry and `entries[].url` / `.title` what it shows.
//!
//! Why this and not a browser extension or AppleScript: it needs no permission (no
//! Automation / Accessibility grant, nothing installed into the browser), works the same on
//! Linux, and is cheap — the file is only decompressed and parsed again when its mtime
//! changes. The cost is latency: Firefox rewrites the file at most every 15 s
//! (`browser.sessionstore.interval`) and only when something changed.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The mozlz4 magic.
pub const MOZLZ4_MAGIC: &[u8; 8] = b"mozLz40\0";

/// Session files above this size are not read (a real one is well under 10 MB).
const MAX_DECOMPRESSED: usize = 256 * 1024 * 1024;

/// One tab's current page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub url: String,
    pub title: String,
}

/// One open tab and where it sits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowTab {
    pub tab: Tab,
    /// The window's index in the session file.
    pub window: usize,
    /// It is its window's selected tab.
    pub selected: bool,
    /// This is the window Firefox had in front (`selectedWindow`).
    pub front_window: bool,
    /// When the tab was last shown (unix ms), `0` when the file doesn't say.
    pub last_accessed_ms: i64,
}

/// What a read of the session file found.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub path: PathBuf,
    /// The file's mtime: how fresh the answer is.
    pub modified: SystemTime,
    /// Every open tab, front window first ([`all_tabs`]).
    pub tabs: Vec<WindowTab>,
}

/// Why a session file could not be read.
#[derive(Debug)]
pub enum SessionError {
    /// No profile has a `recovery.jsonlz4` (Firefox not installed or not running).
    NotFound,
    Io(std::io::Error),
    /// Not mozlz4, or the LZ4 block is corrupt.
    Format(String),
    Json(serde_json::Error),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no Firefox session file"),
            Self::Io(e) => write!(f, "reading the Firefox session: {e}"),
            Self::Format(e) => write!(f, "Firefox session file: {e}"),
            Self::Json(e) => write!(f, "Firefox session JSON: {e}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// Decompress a mozlz4 file's bytes.
pub fn decode_mozlz4(bytes: &[u8]) -> Result<Vec<u8>, SessionError> {
    if bytes.len() < 12 || &bytes[..8] != MOZLZ4_MAGIC {
        return Err(SessionError::Format("not a mozlz4 file".into()));
    }
    let mut size = [0u8; 4];
    size.copy_from_slice(&bytes[8..12]);
    let size = u32::from_le_bytes(size) as usize;
    if size > MAX_DECOMPRESSED {
        return Err(SessionError::Format(format!(
            "declared size {size} too large"
        )));
    }
    lz4_flex::block::decompress(&bytes[12..], size).map_err(|e| SessionError::Format(e.to_string()))
}

/// Compress `data` as mozlz4 (tests and fixtures).
pub fn encode_mozlz4(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2 + 12);
    out.extend_from_slice(MOZLZ4_MAGIC);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&lz4_flex::block::compress(data));
    out
}

/// Every open tab in a session JSON (current entry), front window first, each window's
/// tabs in order. Hidden tabs (tab groups, extensions) are included; closed ones are not.
pub fn all_tabs(session: &serde_json::Value) -> Vec<WindowTab> {
    let Some(windows) = session.get("windows").and_then(|w| w.as_array()) else {
        return Vec::new();
    };
    let front = session
        .get("selectedWindow")
        .and_then(|v| v.as_u64())
        .unwrap_or(1) as usize;
    let mut out = Vec::new();
    for (wi, window) in windows.iter().enumerate() {
        let Some(tabs) = window.get("tabs").and_then(|t| t.as_array()) else {
            continue;
        };
        let selected = window.get("selected").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        for (ti, tab) in tabs.iter().enumerate() {
            let Some(entry) = current_entry(tab) else {
                continue;
            };
            out.push(WindowTab {
                tab: entry,
                window: wi,
                selected: ti + 1 == selected,
                front_window: wi + 1 == front,
                last_accessed_ms: tab
                    .get("lastAccessed")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
            });
        }
    }
    // stable: window order within each group is kept
    out.sort_by_key(|w| !w.front_window);
    out
}

/// The selected tab of each window, front window first.
pub fn selected_tabs(session: &serde_json::Value) -> Vec<WindowTab> {
    all_tabs(session)
        .into_iter()
        .filter(|t| t.selected)
        .collect()
}

/// A tab object's current history entry.
fn current_entry(tab: &serde_json::Value) -> Option<Tab> {
    let entries = tab.get("entries")?.as_array()?;
    if entries.is_empty() {
        return None;
    }
    let index = tab
        .get("index")
        .and_then(|v| v.as_u64())
        .map(|i| i as usize)
        .unwrap_or(entries.len())
        .clamp(1, entries.len());
    let entry = entries.get(index - 1)?;
    let url = entry.get("url")?.as_str()?.to_string();
    let title = entry
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    Some(Tab { url, title })
}

/// Read and parse one session file.
pub fn read_session_file(path: &Path) -> Result<SessionSnapshot, SessionError> {
    let modified = fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(SessionError::Io)?;
    let bytes = fs::read(path).map_err(SessionError::Io)?;
    let json = decode_mozlz4(&bytes)?;
    let value: serde_json::Value = serde_json::from_slice(&json).map_err(SessionError::Json)?;
    Ok(SessionSnapshot {
        path: path.to_path_buf(),
        modified,
        tabs: all_tabs(&value),
    })
}

/// The directories Firefox profiles may live in on this platform.
pub fn default_roots() -> Vec<PathBuf> {
    let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) else {
        return Vec::new();
    };
    if cfg!(target_os = "macos") {
        vec![home.join("Library/Application Support/Firefox/Profiles")]
    } else {
        vec![
            home.join(".mozilla/firefox"),
            home.join("snap/firefox/common/.mozilla/firefox"),
            home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
        ]
    }
}

/// The most recently written `recovery.jsonlz4` under `roots` (one level of profile
/// directories): the profile Firefox is running now. Picking by mtime rather than
/// `profiles.ini` follows a user who switches between profiles (or editions).
pub fn newest_recovery_file(roots: &[PathBuf]) -> Option<(PathBuf, SystemTime)> {
    let mut best: Option<(PathBuf, SystemTime)> = None;
    for root in roots {
        let Ok(dir) = fs::read_dir(root) else {
            continue;
        };
        for profile in dir.flatten() {
            let file = profile.path().join("sessionstore-backups/recovery.jsonlz4");
            let Ok(modified) = fs::metadata(&file).and_then(|m| m.modified()) else {
                continue;
            };
            if best.as_ref().is_none_or(|(_, t)| modified > *t) {
                best = Some((file, modified));
            }
        }
    }
    best
}

/// A cached reader: [`FirefoxSession::snapshot`] only re-reads the file when its path or mtime
/// changed, so polling it every few seconds costs a directory scan and a few `stat`s.
#[derive(Debug)]
pub struct FirefoxSession {
    roots: Vec<PathBuf>,
    cached: Option<SessionSnapshot>,
}

impl Default for FirefoxSession {
    fn default() -> Self {
        Self::new()
    }
}

impl FirefoxSession {
    /// The platform's default profile locations.
    pub fn new() -> Self {
        Self::with_roots(default_roots())
    }

    /// Explicit profile roots (tests).
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            cached: None,
        }
    }

    /// The current snapshot (from the cache when the newest file hasn't changed).
    pub fn snapshot(&mut self) -> Result<&SessionSnapshot, SessionError> {
        let (path, modified) = newest_recovery_file(&self.roots).ok_or(SessionError::NotFound)?;
        let fresh = self
            .cached
            .as_ref()
            .is_some_and(|c| c.path == path && c.modified == modified);
        if !fresh {
            self.cached = Some(read_session_file(&path)?);
        }
        self.cached.as_ref().ok_or(SessionError::NotFound)
    }

    /// The selected tab of every window, front window first.
    pub fn selected_tabs(&mut self) -> Vec<WindowTab> {
        self.tabs().into_iter().filter(|t| t.selected).collect()
    }

    /// Every open tab, front window first (empty when Firefox isn't there).
    pub fn tabs(&mut self) -> Vec<WindowTab> {
        match self.snapshot() {
            Ok(s) => s.tabs.clone(),
            Err(SessionError::NotFound) => Vec::new(),
            Err(e) => {
                log::debug!("{e}");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_json() -> serde_json::Value {
        serde_json::json!({
            "selectedWindow": 2,
            "windows": [
                { "selected": 2, "tabs": [
                    { "index": 1, "entries": [{ "url": "https://example.com/", "title": "Example" }] },
                    { "index": 2, "lastAccessed": 1700000000000_i64, "entries": [
                        { "url": "https://www.youtube.com/", "title": "YouTube" },
                        { "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ", "title": "Rick Astley - Never Gonna Give You Up - YouTube" }
                    ] }
                ] },
                { "selected": 1, "tabs": [
                    { "index": 1, "entries": [{ "url": "https://meet.google.com/abc-defg-hij", "title": "Meet" }] }
                ] }
            ]
        })
    }

    #[test]
    fn mozlz4_round_trips_and_rejects_other_files() {
        let data = serde_json::to_vec(&session_json()).unwrap();
        let packed = encode_mozlz4(&data);
        assert_eq!(&packed[..8], MOZLZ4_MAGIC);
        assert_eq!(decode_mozlz4(&packed).unwrap(), data);
        assert!(decode_mozlz4(b"not mozlz4 at all").is_err());
        let mut lying = packed.clone();
        lying[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_mozlz4(&lying).is_err());
    }

    #[test]
    fn picks_each_windows_selected_tab_and_its_current_entry_front_window_first() {
        let tabs = selected_tabs(&session_json());
        assert_eq!(tabs.len(), 2);
        assert!(tabs[0].front_window);
        assert_eq!(tabs[0].tab.url, "https://meet.google.com/abc-defg-hij");
        assert_eq!(
            tabs[1].tab.url,
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        assert_eq!(tabs[1].last_accessed_ms, 1_700_000_000_000);
        assert!(selected_tabs(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn reads_the_newest_profile_and_caches_by_mtime() {
        let root = std::env::temp_dir().join(format!("oats-ff-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (profile, url) in [
            ("a.default", "https://old.example/"),
            ("b.release", "https://www.youtube.com/watch?v=x"),
        ] {
            let dir = root.join(profile).join("sessionstore-backups");
            fs::create_dir_all(&dir).unwrap();
            let json = serde_json::json!({ "windows": [{ "selected": 1, "tabs": [{ "index": 1, "entries": [{ "url": url, "title": "t" }] }] }] });
            fs::write(
                dir.join("recovery.jsonlz4"),
                encode_mozlz4(&serde_json::to_vec(&json).unwrap()),
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut session = FirefoxSession::with_roots(vec![root.clone()]);
        let tabs = session.selected_tabs();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].tab.url, "https://www.youtube.com/watch?v=x");
        assert!(
            session
                .snapshot()
                .unwrap()
                .path
                .ends_with("b.release/sessionstore-backups/recovery.jsonlz4")
        );
        let _ = fs::remove_dir_all(&root);
        assert!(
            FirefoxSession::with_roots(vec![root])
                .selected_tabs()
                .is_empty()
        );
    }
}
