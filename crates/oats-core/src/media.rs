//! Media on the system-audio lane: what kind of thing is playing (a YouTube video, music, a
//! call, a browser tab, something else), the stamp a system segment carries, the policy for
//! YouTube / music ([`MediaPolicy`]) and the shared slot through which the media resolver
//! (`oats-media`, on its own thread) tells the pipeline what is playing ([`MediaFeed`]).
//!
//! Meetings are never media: the policy applies to [`MediaClass::YouTube`] and
//! [`MediaClass::Music`] only. Pure, no platform code.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// What is playing, as stored in `segments.media_kind` / `media_plays.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaClass {
    YouTube,
    Music,
    /// A call (Zoom, Meet, Teams, …): a conversation, never media.
    Meeting,
    /// A browser plays something that isn't recognised.
    Browser,
    Other,
}

impl MediaClass {
    /// The store's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            MediaClass::YouTube => "youtube",
            MediaClass::Music => "music",
            MediaClass::Meeting => "meeting",
            MediaClass::Browser => "browser",
            MediaClass::Other => "other",
        }
    }

    /// The inverse of [`Self::as_str`] (`None` for anything else).
    pub fn parse(s: &str) -> Option<MediaClass> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "youtube" => MediaClass::YouTube,
            "music" => MediaClass::Music,
            "meeting" => MediaClass::Meeting,
            "browser" => MediaClass::Browser,
            "other" => MediaClass::Other,
            _ => return None,
        })
    }

    /// Media proper (a video, music): informational content, not a conversation. The policy
    /// and the brain's media handling apply to these only.
    pub fn is_media(self) -> bool {
        matches!(self, MediaClass::YouTube | MediaClass::Music)
    }
}

/// What happens to the system lane while YouTube or music plays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MediaPolicy {
    /// Neither transcribed nor logged (VAD is skipped too; the microphone's echo check still
    /// runs, so the room copy of the sound is dropped).
    Ignore,
    /// Not transcribed, but each play is logged in `media_plays` (title, URL, start / end).
    #[default]
    LogOnly,
    /// Transcribed, segments stamped with the media, plays logged.
    Transcribe,
}

impl MediaPolicy {
    /// The `meta` key the policy is kept under.
    pub const META_KEY: &'static str = "media_policy";
    /// The environment override.
    pub const ENV: &'static str = "OATS_MEDIA_POLICY";
    /// All three, in menu order.
    pub const ALL: [MediaPolicy; 3] = [
        MediaPolicy::Ignore,
        MediaPolicy::LogOnly,
        MediaPolicy::Transcribe,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            MediaPolicy::Ignore => "ignore",
            MediaPolicy::LogOnly => "log",
            MediaPolicy::Transcribe => "transcribe",
        }
    }

    /// `ignore` | `log` (`log-only`, `logonly`) | `transcribe`, case-insensitive.
    pub fn parse(s: &str) -> Option<MediaPolicy> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_', ' '], "")
            .as_str()
        {
            "ignore" | "off" | "skip" => Some(MediaPolicy::Ignore),
            "log" | "logonly" => Some(MediaPolicy::LogOnly),
            "transcribe" | "on" => Some(MediaPolicy::Transcribe),
            _ => None,
        }
    }

    /// The menu label.
    pub fn label(self) -> &'static str {
        match self {
            MediaPolicy::Ignore => "Ignore",
            MediaPolicy::LogOnly => "Log only",
            MediaPolicy::Transcribe => "Transcribe",
        }
    }

    /// The policy in force: `OATS_MEDIA_POLICY` when set and valid, else the stored value,
    /// else the default (log only).
    pub fn effective(stored: Option<&str>) -> MediaPolicy {
        Self::resolve(std::env::var(Self::ENV).ok().as_deref(), stored)
    }

    /// [`Self::effective`] with the environment passed in (tests).
    pub fn resolve(env: Option<&str>, stored: Option<&str>) -> MediaPolicy {
        if let Some(v) = env {
            match Self::parse(v) {
                Some(p) => return p,
                None => log::warn!("{}={v:?} is not ignore|log|transcribe", Self::ENV),
            }
        }
        stored.and_then(Self::parse).unwrap_or_default()
    }

    /// The environment overrides the stored policy (the UI then shows it as fixed).
    pub fn env_override() -> Option<MediaPolicy> {
        std::env::var(Self::ENV)
            .ok()
            .as_deref()
            .and_then(Self::parse)
    }

    /// The system lane is not transcribed while media plays.
    pub fn skips_transcription(self) -> bool {
        matches!(self, MediaPolicy::Ignore | MediaPolicy::LogOnly)
    }

    /// Plays are written to `media_plays`.
    pub fn logs_plays(self) -> bool {
        matches!(self, MediaPolicy::LogOnly | MediaPolicy::Transcribe)
    }

    pub(crate) fn to_u8(self) -> u8 {
        match self {
            MediaPolicy::Ignore => 0,
            MediaPolicy::LogOnly => 1,
            MediaPolicy::Transcribe => 2,
        }
    }

    pub(crate) fn from_u8(v: u8) -> MediaPolicy {
        match v {
            0 => MediaPolicy::Ignore,
            2 => MediaPolicy::Transcribe,
            _ => MediaPolicy::LogOnly,
        }
    }
}

/// What is playing on the system lane right now, as the pipeline stamps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaStamp {
    pub class: MediaClass,
    /// The page / stream (a YouTube watch URL, a Meet link); `None` when unknown.
    pub url: Option<String>,
    /// The video's title, the app's name, … (may be empty).
    pub title: String,
    /// The app producing the sound ("Firefox", "Spotify"; may be empty).
    pub producer: String,
}

impl MediaStamp {
    /// YouTube or music (see [`MediaClass::is_media`]).
    pub fn is_media(&self) -> bool {
        self.class.is_media()
    }

    /// What identifies one play: the URL, else the title.
    pub fn play_key(&self) -> &str {
        self.url.as_deref().unwrap_or(&self.title)
    }
}

/// One logged play (`media_plays`): a contiguous stretch of the same media.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPlay {
    pub id: i64,
    pub started_at: i64,
    pub ended_at: i64,
    pub kind: MediaClass,
    pub url: Option<String>,
    pub title: String,
    pub producer: String,
    /// System segments transcribed during the play (0 unless the policy transcribes).
    pub segments_count: i64,
}

/// The slot the media resolver writes and the pipeline reads: cheap to read on every system
/// chunk (an atomic version check; the lock is taken only after a change). Clones share it.
#[derive(Clone, Debug, Default)]
pub struct MediaFeed {
    inner: Arc<(AtomicU64, Mutex<Option<MediaStamp>>)>,
}

impl MediaFeed {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish what is playing (`None`: nothing known). A value equal to the current one
    /// changes nothing.
    pub fn set(&self, stamp: Option<MediaStamp>) {
        if let Ok(mut slot) = self.inner.1.lock() {
            if *slot == stamp {
                return;
            }
            *slot = stamp;
            self.inner.0.fetch_add(1, Ordering::Release);
        }
    }

    /// What is playing.
    pub fn current(&self) -> Option<MediaStamp> {
        self.inner.1.lock().ok().and_then(|s| s.clone())
    }

    /// Bumped by every change.
    pub fn version(&self) -> u64 {
        self.inner.0.load(Ordering::Acquire)
    }
}

/// A reader that caches the feed's value between changes.
#[derive(Debug)]
pub(crate) struct FeedReader {
    feed: MediaFeed,
    seen: u64,
    value: Option<MediaStamp>,
}

impl FeedReader {
    pub(crate) fn new(feed: MediaFeed) -> Self {
        let value = feed.current();
        let seen = feed.version();
        Self { feed, seen, value }
    }

    pub(crate) fn get(&mut self) -> Option<&MediaStamp> {
        let v = self.feed.version();
        if v != self.seen {
            self.seen = v;
            self.value = self.feed.current();
        }
        self.value.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_and_policies_round_trip() {
        for c in [
            MediaClass::YouTube,
            MediaClass::Music,
            MediaClass::Meeting,
            MediaClass::Browser,
            MediaClass::Other,
        ] {
            assert_eq!(MediaClass::parse(c.as_str()), Some(c));
        }
        assert!(MediaClass::YouTube.is_media() && MediaClass::Music.is_media());
        assert!(!MediaClass::Meeting.is_media() && !MediaClass::Browser.is_media());
        for p in MediaPolicy::ALL {
            assert_eq!(MediaPolicy::parse(p.as_str()), Some(p));
            assert_eq!(MediaPolicy::from_u8(p.to_u8()), p);
        }
        assert_eq!(MediaPolicy::parse("Log-Only"), Some(MediaPolicy::LogOnly));
        assert_eq!(MediaPolicy::resolve(None, None), MediaPolicy::LogOnly);
        assert_eq!(
            MediaPolicy::resolve(None, Some("ignore")),
            MediaPolicy::Ignore
        );
        assert_eq!(
            MediaPolicy::resolve(Some("transcribe"), Some("ignore")),
            MediaPolicy::Transcribe
        );
        assert_eq!(
            MediaPolicy::resolve(Some("bogus"), Some("ignore")),
            MediaPolicy::Ignore
        );
    }

    #[test]
    fn the_feed_reader_sees_changes() {
        let feed = MediaFeed::new();
        let mut reader = FeedReader::new(feed.clone());
        assert!(reader.get().is_none());
        let stamp = MediaStamp {
            class: MediaClass::YouTube,
            url: Some("https://www.youtube.com/watch?v=a".into()),
            title: "A".into(),
            producer: "Firefox".into(),
        };
        feed.set(Some(stamp.clone()));
        let v = feed.version();
        feed.set(Some(stamp.clone()));
        assert_eq!(feed.version(), v, "an equal value is no change");
        assert_eq!(reader.get(), Some(&stamp));
        assert_eq!(stamp.play_key(), "https://www.youtube.com/watch?v=a");
        feed.set(None);
        assert!(reader.get().is_none());
    }
}
