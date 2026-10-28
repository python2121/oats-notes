//! Finding the operating system's own UI font, so the app's text looks native: SF Pro on macOS,
//! the desktop's configured font on Linux (KDE / GNOME, else fontconfig's `system-ui`), Segoe UI
//! on Windows. The font is READ from the user's system at runtime and never bundled — SF Pro in
//! particular may not be redistributed. Everything here is best-effort: any failure (missing
//! file, no fontconfig, a font egui can't parse) returns `None` and the theme falls back to the
//! bundled Inter.
//!
//! egui panics on a font file it can't parse, so every candidate is validated with `skrifa` —
//! the parser egui itself uses — before it is handed over.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A system font ready for egui.
#[derive(Clone)]
pub struct SystemFont {
    pub bytes: Vec<u8>,
    /// Face index inside a collection (`.ttc`); 0 for a plain font file.
    pub index: u32,
    /// Human-readable origin, for the log ("SF Pro (/System/Library/Fonts/SFNS.ttf)").
    pub description: String,
}

impl std::fmt::Debug for SystemFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemFont")
            .field("description", &self.description)
            .field("index", &self.index)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Font files above this are refused (system UI fonts are a few MB; SF Pro is ~8 MB).
const MAX_FONT_BYTES: u64 = 64 * 1024 * 1024;

/// The system UI font, or `None` (logged) when it can't be found or parsed.
pub fn discover() -> Option<SystemFont> {
    for (path, index, label) in candidates() {
        match load(&path, index, &label) {
            Ok(font) => return Some(font),
            Err(e) => log::debug!("system font candidate {}: {e}", path.display()),
        }
    }
    log::info!("no usable system UI font found; using the bundled font");
    None
}

/// Read and validate one candidate.
fn load(path: &Path, index: u32, label: &str) -> Result<SystemFont, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_FONT_BYTES {
        return Err(format!("{} bytes is too large for a UI font", meta.len()));
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    validate(&bytes, index)?;
    Ok(SystemFont {
        bytes,
        index,
        description: format!("{label} ({})", path.display()),
    })
}

/// The same parse egui will do, plus a check that it actually draws Latin text.
pub fn validate(bytes: &[u8], index: u32) -> Result<(), String> {
    use skrifa::MetadataProvider;
    let font = skrifa::FontRef::from_index(bytes, index).map_err(|e| e.to_string())?;
    let charmap = font.charmap();
    if ['A', 'a', '0', ' ']
        .iter()
        .all(|c| charmap.map(*c).is_some())
    {
        Ok(())
    } else {
        Err("font lacks basic Latin glyphs".to_owned())
    }
}

/// Where to look, in order. Each entry: font file, face index, label.
fn candidates() -> Vec<(PathBuf, u32, String)> {
    let mut out = Vec::new();
    if cfg!(target_os = "macos") {
        // SF Pro: the variable system font every AppKit control uses.
        out.push((
            PathBuf::from("/System/Library/Fonts/SFNS.ttf"),
            0,
            "SF Pro".to_owned(),
        ));
        out.push((
            PathBuf::from("/System/Library/Fonts/HelveticaNeue.ttc"),
            0,
            "Helvetica Neue".to_owned(),
        ));
    } else if cfg!(target_os = "windows") {
        let root =
            std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        out.push((
            root.join("Fonts").join("segoeui.ttf"),
            0,
            "Segoe UI".to_owned(),
        ));
    } else {
        // Linux / BSD: the desktop's configured UI font first, then fontconfig's idea of one.
        let mut patterns: Vec<String> = desktop_font_family()
            .map(|f| format!("{f}:style=Regular"))
            .into_iter()
            .collect();
        patterns.push("system-ui".to_owned());
        patterns.push("sans-serif".to_owned());
        for pattern in patterns {
            if let Some((path, index, family)) = fc_match(&pattern)
                && !out.iter().any(|(p, i, _)| *p == path && *i == index)
            {
                out.push((path, index, family));
            }
        }
    }
    out
}

/// The UI font family the desktop environment is configured with, when it says.
fn desktop_font_family() -> Option<String> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_uppercase();
    if desktop.contains("KDE") {
        kde_font_family()
    } else if desktop.contains("GNOME") || desktop.contains("UNITY") || desktop.contains("CINNAMON")
    {
        gnome_font_family()
    } else {
        None
    }
}

/// KDE Plasma: `[General] font=Noto Sans,10,-1,5,400,…` in `kdeglobals` (absent = the default,
/// which fontconfig's `system-ui` then answers).
fn kde_font_family() -> Option<String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let text = std::fs::read_to_string(config.join("kdeglobals")).ok()?;
    parse_kde_font(&text)
}

/// The family from a `kdeglobals` file's `[General]` `font=` entry.
pub fn parse_kde_font(kdeglobals: &str) -> Option<String> {
    let mut in_general = false;
    for line in kdeglobals.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_general = line == "[General]";
        } else if in_general && let Some(value) = line.strip_prefix("font=") {
            let family = value.split(',').next().unwrap_or("").trim();
            if !family.is_empty() {
                return Some(family.to_owned());
            }
        }
    }
    None
}

/// GNOME: `gsettings get org.gnome.desktop.interface font-name` → `'Cantarell 11'`.
fn gnome_font_family() -> Option<String> {
    let out = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "font-name"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_gsettings_font(&String::from_utf8_lossy(&out.stdout))
}

/// The family from a gsettings font name (`'Adwaita Sans 11'` → `Adwaita Sans`).
pub fn parse_gsettings_font(value: &str) -> Option<String> {
    let v = value.trim().trim_matches('\'').trim();
    // drop a trailing point size
    let family = match v.rsplit_once(' ') {
        Some((head, tail)) if tail.parse::<f32>().is_ok() => head,
        _ => v,
    };
    let family = family.trim();
    (!family.is_empty()).then(|| family.to_owned())
}

/// Resolve a fontconfig pattern to (file, face index, family) with `fc-match`.
fn fc_match(pattern: &str) -> Option<(PathBuf, u32, String)> {
    let out = Command::new("fc-match")
        .args(["-f", "%{file}\n%{index}\n%{family[0]}", pattern])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let file = lines.next()?.trim();
    let index = lines
        .next()
        .and_then(|i| i.trim().parse().ok())
        .unwrap_or(0);
    let family = lines.next().unwrap_or(pattern).trim().to_owned();
    (!file.is_empty()).then(|| (PathBuf::from(file), index, family))
}
