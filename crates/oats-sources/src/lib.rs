//! Audio producers: which processes are playing sound right now. Like the speech engine and
//! the status item, the implementation is per platform — this crate is only the contract:
//!
//! - macOS: `oats-sources-macos` reads Core Audio's process objects
//!   (`kAudioHardwarePropertyProcessObjectList` → per process its bundle id, pid and
//!   `kAudioProcessPropertyIsRunningOutput`); no TCC permission is needed for that.
//! - Linux (not built): PipeWire's playback streams carry `application.name` /
//!   `application.process.id` / `application.process.binary`.
//!
//! The answer feeds `oats-media`, which combines it with the browser's selected tab to tell
//! a YouTube video from a call.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

/// One process that is producing audio output.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Producer {
    /// The process id (`0` when the platform doesn't say).
    pub pid: i32,
    /// The app's bundle identifier on macOS (`org.mozilla.firefox`, `us.zoom.xos`); on Linux
    /// the binary name. Empty when unknown.
    pub bundle_id: String,
    /// A human name ("Firefox", "zoom.us"); falls back to the bundle id's last component.
    pub name: String,
}

impl Producer {
    /// A producer from its bundle id alone, the display name derived from it.
    pub fn from_bundle(pid: i32, bundle_id: &str) -> Self {
        Self {
            pid,
            bundle_id: bundle_id.to_string(),
            name: display_name(bundle_id),
        }
    }
}

/// A readable name for a bundle id: a few known apps by name, else its last dotted component.
pub fn display_name(bundle_id: &str) -> String {
    let known = [
        ("org.mozilla.firefox", "Firefox"),
        (
            "org.mozilla.firefoxdeveloperedition",
            "Firefox Developer Edition",
        ),
        ("org.mozilla.nightly", "Firefox Nightly"),
        ("com.google.Chrome", "Chrome"),
        ("com.apple.Safari", "Safari"),
        ("com.spotify.client", "Spotify"),
        ("com.apple.Music", "Music"),
        ("us.zoom.xos", "Zoom"),
        ("com.microsoft.teams2", "Microsoft Teams"),
        ("com.microsoft.teams", "Microsoft Teams"),
    ];
    if let Some((_, name)) = known
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(bundle_id))
    {
        return (*name).to_string();
    }
    bundle_id
        .rsplit('.')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(bundle_id)
        .to_string()
}

/// The platform's view of which processes are producing audio output.
///
/// Implementations are cheap enough to call every couple of seconds (a handful of property
/// reads) and must not block on a permission prompt. `Send` so a polling thread can own one.
pub trait AudioProducers: Send {
    /// The processes producing output right now, in a stable order (by pid). Empty when
    /// nothing plays or the platform can't tell. Our own process is left out.
    fn running_output(&mut self) -> Vec<Producer>;
}

/// No producer detection (a platform without one, or tests): nothing is ever reported.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProducers;

impl AudioProducers for NoProducers {
    fn running_output(&mut self) -> Vec<Producer> {
        Vec::new()
    }
}

/// A fixed answer, for tests and demos: reports whatever was last [`FixedProducers::set`].
#[derive(Clone, Debug, Default)]
pub struct FixedProducers(pub std::sync::Arc<std::sync::Mutex<Vec<Producer>>>);

impl FixedProducers {
    /// Replace the reported producers.
    pub fn set(&self, producers: Vec<Producer>) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = producers;
        }
    }
}

impl AudioProducers for FixedProducers {
    fn running_output(&mut self) -> Vec<Producer> {
        self.0.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Watches the producers for changes: [`ProducerWatch::poll`] returns `Some(new list)` only
/// when the set differs from the previous poll ("emit changes only").
#[derive(Debug, Default)]
pub struct ProducerWatch {
    last: Option<Vec<Producer>>,
}

impl ProducerWatch {
    /// A watch that reports the first poll as a change.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask `source`; `Some` when the answer changed since the last poll.
    pub fn poll(&mut self, source: &mut dyn AudioProducers) -> Option<Vec<Producer>> {
        let mut now = source.running_output();
        now.sort_by(|a, b| {
            a.pid
                .cmp(&b.pid)
                .then_with(|| a.bundle_id.cmp(&b.bundle_id))
        });
        if self.last.as_ref() == Some(&now) {
            return None;
        }
        self.last = Some(now.clone());
        Some(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_known_apps_and_falls_back_to_the_last_component() {
        assert_eq!(display_name("org.mozilla.firefox"), "Firefox");
        assert_eq!(display_name("com.example.Player"), "Player");
        assert_eq!(display_name(""), "");
    }

    #[test]
    fn the_watch_reports_changes_only() {
        let fixed = FixedProducers::default();
        let mut source = fixed.clone();
        let mut watch = ProducerWatch::new();
        assert_eq!(watch.poll(&mut source), Some(vec![]));
        assert_eq!(watch.poll(&mut source), None);
        fixed.set(vec![Producer::from_bundle(7, "org.mozilla.firefox")]);
        let changed = watch.poll(&mut source);
        assert_eq!(changed.map(|v| v.len()), Some(1));
        assert_eq!(watch.poll(&mut source), None);
        assert_eq!(NoProducers.running_output(), vec![]);
    }
}
