//! Media context: WHAT is playing on the system-audio lane, so a YouTube video is never taken
//! for the user's own conversation (and is recorded as "watched: …" instead).
//!
//! Two permission-free signals, combined by [`resolve`] (pure, tested):
//! - the audio producers ([`oats_sources::AudioProducers`]: on macOS Core Audio's process
//!   objects) — which apps are playing sound;
//! - when one of them is Firefox, the selected tab of each Firefox window, read from its
//!   session store ([`firefox`]).
//!
//! [`classify_url`] / [`classify_bundle`] map a URL / bundle id to a [`MediaKind`]; the result
//! is a [`MediaContext`], published to the pipeline as an [`oats_core::MediaStamp`] by the
//! [`MediaWatcher`] thread.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod firefox;
mod watcher;

pub use firefox::{FirefoxSession, Tab, WindowTab};
pub use oats_core::{MediaClass, MediaStamp};
pub use oats_sources::Producer;
pub use watcher::{MediaWatcher, OnMediaChange, WatcherConfig};

/// A call app / service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MeetingApp {
    Zoom,
    Meet,
    Teams,
    Webex,
    Whereby,
    Jitsi,
    Slack,
    FaceTime,
    Discord,
}

impl MeetingApp {
    pub fn name(self) -> &'static str {
        match self {
            MeetingApp::Zoom => "Zoom",
            MeetingApp::Meet => "Google Meet",
            MeetingApp::Teams => "Microsoft Teams",
            MeetingApp::Webex => "Webex",
            MeetingApp::Whereby => "Whereby",
            MeetingApp::Jitsi => "Jitsi Meet",
            MeetingApp::Slack => "Slack huddle",
            MeetingApp::FaceTime => "FaceTime",
            MeetingApp::Discord => "Discord",
        }
    }
}

/// What kind of thing is playing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaKind {
    YouTube {
        video_id: String,
        /// The tab title without YouTube's decorations (empty when unknown).
        title: String,
        /// Not in the session store; filled by richer sources later.
        channel: Option<String>,
    },
    Meeting(MeetingApp),
    Music,
    /// A browser plays something unrecognised.
    Browser,
    Other,
}

impl MediaKind {
    /// The store's class.
    pub fn class(&self) -> MediaClass {
        match self {
            MediaKind::YouTube { .. } => MediaClass::YouTube,
            MediaKind::Meeting(_) => MediaClass::Meeting,
            MediaKind::Music => MediaClass::Music,
            MediaKind::Browser => MediaClass::Browser,
            MediaKind::Other => MediaClass::Other,
        }
    }

    /// More decisive kinds win when several producers play at once: a call beats a video
    /// (calls are never media, and losing a call's transcript is the worse mistake), a video
    /// beats music, anything recognised beats a bare browser.
    fn rank(&self) -> u8 {
        match self {
            MediaKind::Meeting(_) => 4,
            MediaKind::YouTube { .. } => 3,
            MediaKind::Music => 2,
            MediaKind::Browser => 1,
            MediaKind::Other => 0,
        }
    }
}

/// What is playing, and where it was seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaContext {
    /// The app producing the sound (the one the kind was decided from).
    pub producer: Option<Producer>,
    /// The browser tab, when the producer is Firefox and a tab explains the sound.
    pub tab: Option<Tab>,
    pub kind: MediaKind,
    /// The tab is a guess: not its window's selected tab (see [`resolve`]).
    pub background: bool,
}

impl MediaContext {
    /// Nothing known to be playing.
    pub fn nothing() -> Self {
        Self {
            producer: None,
            tab: None,
            kind: MediaKind::Other,
            background: false,
        }
    }

    /// The title to show: the video's, else the tab's, else the app's name.
    pub fn title(&self) -> String {
        if let MediaKind::YouTube { title, .. } = &self.kind
            && !title.is_empty()
        {
            return title.clone();
        }
        if let Some(tab) = self.tab.as_ref().filter(|t| !t.title.is_empty()) {
            return tab.title.clone();
        }
        if let MediaKind::Meeting(app) = self.kind {
            return app.name().to_string();
        }
        self.producer
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    /// The stamp the pipeline puts on system segments; `None` when nothing plays.
    pub fn stamp(&self) -> Option<MediaStamp> {
        if self.producer.is_none() && self.tab.is_none() {
            return None;
        }
        Some(MediaStamp {
            class: self.kind.class(),
            url: self.tab.as_ref().map(|t| t.url.clone()),
            title: self.title(),
            producer: self
                .producer
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_default(),
        })
    }
}

/// The host of a URL (lower case, without `www.` / `m.`), and the rest after it.
fn split_url(url: &str) -> Option<(String, &str)> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..end].rsplit('@').next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("m."))
        .map(str::to_string)
        .unwrap_or(host);
    Some((host, &rest[end..]))
}

/// The value of query parameter `key` in `path_and_query`.
fn query_param<'a>(path_and_query: &'a str, key: &str) -> Option<&'a str> {
    let query = path_and_query.split_once('?')?.1;
    let query = query.split('#').next().unwrap_or(query);
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key && !v.is_empty()).then_some(v)
    })
}

/// A YouTube video id (11 characters of `[A-Za-z0-9_-]`; anything that looks like one).
fn video_id(s: &str) -> Option<String> {
    let id: String = s
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    (!id.is_empty() && id.len() <= 64).then_some(id)
}

/// `host` is `domain` or a subdomain of it.
fn is_domain(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// What a page is, from its URL alone (`title` left empty for YouTube; see [`classify_tab`]).
pub fn classify_url(url: &str) -> MediaKind {
    let Some((host, path)) = split_url(url) else {
        return MediaKind::Browser;
    };
    let first = path
        .trim_start_matches('/')
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let youtube = |id: Option<String>| match id {
        Some(video_id) => MediaKind::YouTube {
            video_id,
            title: String::new(),
            channel: None,
        },
        None => MediaKind::Browser,
    };
    match host.as_str() {
        "music.youtube.com" => MediaKind::Music,
        "youtube.com" | "youtube-nocookie.com" => match first {
            "watch" => youtube(query_param(path, "v").and_then(video_id)),
            "shorts" | "live" | "embed" => youtube(
                path.trim_start_matches('/')
                    .split('/')
                    .nth(1)
                    .and_then(video_id),
            ),
            _ => MediaKind::Browser,
        },
        "youtu.be" => youtube(video_id(path.trim_start_matches('/'))),
        "meet.google.com" => MediaKind::Meeting(MeetingApp::Meet),
        "teams.microsoft.com" | "teams.live.com" | "teams.cloud.microsoft" => {
            MediaKind::Meeting(MeetingApp::Teams)
        }
        "whereby.com" => MediaKind::Meeting(MeetingApp::Whereby),
        "meet.jit.si" => MediaKind::Meeting(MeetingApp::Jitsi),
        "open.spotify.com" | "music.apple.com" | "soundcloud.com" | "tidal.com"
        | "listen.tidal.com" | "deezer.com" | "music.amazon.com" => MediaKind::Music,
        "app.slack.com" if path.contains("/huddle") => MediaKind::Meeting(MeetingApp::Slack),
        h if is_domain(h, "zoom.us") || is_domain(h, "zoom.com") => {
            MediaKind::Meeting(MeetingApp::Zoom)
        }
        h if is_domain(h, "webex.com") => MediaKind::Meeting(MeetingApp::Webex),
        h if is_domain(h, "bandcamp.com") => MediaKind::Music,
        _ => MediaKind::Browser,
    }
}

/// A YouTube tab title without the " - YouTube" suffix and the "(3) " / "(99+) " notification
/// count (a 4-digit "(2020) " is part of the title).
pub fn clean_youtube_title(title: &str) -> String {
    let mut t = title.trim();
    if let Some(rest) = t.strip_prefix('(')
        && let Some((count, after)) = rest.split_once(") ")
        && !count.is_empty()
        && count.len() <= 3
        && count.chars().all(|c| c.is_ascii_digit() || c == '+')
    {
        t = after;
    }
    t.strip_suffix(" - YouTube")
        .or_else(|| t.strip_suffix(" – YouTube"))
        .unwrap_or(t)
        .trim()
        .to_string()
}

/// [`classify_url`] with the tab's title filled in.
pub fn classify_tab(tab: &Tab) -> MediaKind {
    match classify_url(&tab.url) {
        MediaKind::YouTube {
            video_id, channel, ..
        } => MediaKind::YouTube {
            video_id,
            title: clean_youtube_title(&tab.title),
            channel,
        },
        other => other,
    }
}

/// Firefox (any edition): its tabs can explain the sound.
pub fn is_firefox(bundle_id: &str) -> bool {
    let b = bundle_id.to_ascii_lowercase();
    b.starts_with("org.mozilla.") || b == "firefox" || b.starts_with("firefox")
}

/// What an app is, from its bundle id alone.
pub fn classify_bundle(bundle_id: &str) -> MediaKind {
    let b = bundle_id.to_ascii_lowercase();
    let starts = |p: &str| b == p || b.starts_with(&format!("{p}."));
    if starts("us.zoom.xos") || b.starts_with("us.zoom.") {
        MediaKind::Meeting(MeetingApp::Zoom)
    } else if starts("com.microsoft.teams2") || starts("com.microsoft.teams") {
        MediaKind::Meeting(MeetingApp::Teams)
    } else if starts("com.cisco.webexmeetingsapp") || starts("com.webex.meetingmanager") {
        MediaKind::Meeting(MeetingApp::Webex)
    } else if starts("com.apple.facetime") {
        MediaKind::Meeting(MeetingApp::FaceTime)
    } else if starts("com.hnc.discord") {
        MediaKind::Meeting(MeetingApp::Discord)
    } else if starts("com.spotify.client")
        || starts("com.apple.music")
        || starts("com.tidal.desktop")
    {
        MediaKind::Music
    } else if is_firefox(&b)
        || starts("com.google.chrome")
        || starts("com.apple.safari")
        || starts("com.apple.webkit")
        || starts("company.thebrowser.browser")
        || starts("com.brave.browser")
        || starts("com.microsoft.edgemac")
        || starts("com.vivaldi.vivaldi")
        || starts("com.operasoftware.opera")
    {
        MediaKind::Browser
    } else {
        MediaKind::Other
    }
}

/// Decide what is playing from the producers and Firefox's open tabs (front window first).
///
/// For a Firefox producer: a selected tab that explains the sound wins (front window
/// first); else a call in any tab (a Meet tab behind the email being read); else the most
/// recently shown YouTube / music tab that isn't selected — the video kept playing after the
/// user switched tabs (`background = true`: the session store has no "this tab plays sound"
/// flag, so this is a guess); else a bare browser with the front tab. Of several producers
/// the most decisive kind wins (see `MediaKind::rank`; a call beats a video); ties go to the
/// first. No producer = nothing playing.
pub fn resolve(producers: &[Producer], firefox_tabs: &[WindowTab]) -> MediaContext {
    let mut best: Option<MediaContext> = None;
    for p in producers {
        let mut ctx = MediaContext {
            producer: Some(p.clone()),
            tab: None,
            kind: classify_bundle(&p.bundle_id),
            background: false,
        };
        if is_firefox(&p.bundle_id) {
            firefox_context(&mut ctx, firefox_tabs);
        }
        if best
            .as_ref()
            .is_none_or(|b| ctx.kind.rank() > b.kind.rank())
        {
            best = Some(ctx);
        }
    }
    best.unwrap_or_else(MediaContext::nothing)
}

/// [`resolve`]'s Firefox rules.
fn firefox_context(ctx: &mut MediaContext, tabs: &[WindowTab]) {
    let classified: Vec<(&WindowTab, MediaKind)> =
        tabs.iter().map(|w| (w, classify_tab(&w.tab))).collect();
    let browser = MediaKind::Browser.rank();
    // 1. a selected tab that explains the sound (front window first on equal rank)
    let selected = classified
        .iter()
        .filter(|(w, _)| w.selected)
        .max_by_key(|(w, kind)| (kind.rank(), w.front_window));
    if let Some((w, kind)) = selected.filter(|(_, k)| k.rank() > browser) {
        ctx.kind = kind.clone();
        ctx.tab = Some(w.tab.clone());
        return;
    }
    // 2. a call in any tab; 3. the most recently shown video / music tab
    let meeting = classified
        .iter()
        .filter(|(_, k)| matches!(k, MediaKind::Meeting(_)))
        .max_by_key(|(w, _)| w.last_accessed_ms);
    let media = classified
        .iter()
        .filter(|(w, k)| !w.selected && matches!(k, MediaKind::YouTube { .. } | MediaKind::Music))
        .max_by_key(|(w, _)| w.last_accessed_ms);
    if let Some((w, kind)) = meeting.or(media) {
        ctx.kind = kind.clone();
        ctx.tab = Some(w.tab.clone());
        ctx.background = true;
        return;
    }
    // 4. a browser playing something: the front tab, for the record
    ctx.tab = selected.map(|(w, _)| w.tab.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(url: &str, title: &str, front: bool) -> WindowTab {
        WindowTab {
            tab: Tab {
                url: url.into(),
                title: title.into(),
            },
            window: usize::from(!front),
            selected: true,
            front_window: front,
            last_accessed_ms: 0,
        }
    }

    fn background(url: &str, title: &str, last_accessed_ms: i64) -> WindowTab {
        WindowTab {
            selected: false,
            last_accessed_ms,
            ..tab(url, title, true)
        }
    }

    #[test]
    fn a_video_left_playing_in_a_background_tab_is_a_guess_and_a_background_call_wins() {
        let ff = Producer::from_bundle(10, "org.mozilla.firefox");
        // what the spike found: the YouTube home page selected, watch tabs behind it
        let tabs = [
            background("https://www.youtube.com/watch?v=old", "Old - YouTube", 100),
            background(
                "https://www.youtube.com/watch?v=recent",
                "Recent - YouTube",
                900,
            ),
            tab("https://www.youtube.com/", "YouTube", true),
        ];
        let ctx = resolve(std::slice::from_ref(&ff), &tabs);
        assert!(ctx.background);
        assert_eq!(ctx.title(), "Recent");
        assert_eq!(ctx.kind.class(), MediaClass::YouTube);
        let mut with_call = tabs.to_vec();
        with_call.push(background("https://meet.google.com/abc", "Meet - abc", 50));
        let ctx = resolve(std::slice::from_ref(&ff), &with_call);
        assert_eq!(ctx.kind, MediaKind::Meeting(MeetingApp::Meet));
        // a selected video beats a background call guess? no: a selected video is certain
        let selected_video = [
            background("https://meet.google.com/abc", "Meet", 50),
            tab("https://youtu.be/xyz", "Xyz - YouTube", true),
        ];
        let ctx = resolve(std::slice::from_ref(&ff), &selected_video);
        assert!(!ctx.background);
        assert_eq!(ctx.title(), "Xyz");
    }

    #[test]
    fn classifies_urls() {
        let yt = |u: &str| match classify_url(u) {
            MediaKind::YouTube { video_id, .. } => Some(video_id),
            _ => None,
        };
        assert_eq!(
            yt("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=42s").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            yt("https://m.youtube.com/watch?feature=share&v=abc_DEF-123").as_deref(),
            Some("abc_DEF-123")
        );
        assert_eq!(
            yt("https://youtu.be/dQw4w9WgXcQ?si=x").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            yt("https://www.youtube.com/shorts/AbCdEfGhIjK").as_deref(),
            Some("AbCdEfGhIjK")
        );
        assert_eq!(classify_url("https://www.youtube.com/"), MediaKind::Browser);
        assert_eq!(
            classify_url("https://www.youtube.com/results?search_query=x"),
            MediaKind::Browser
        );
        assert_eq!(
            classify_url("https://music.youtube.com/watch?v=x"),
            MediaKind::Music
        );
        assert_eq!(
            classify_url("https://meet.google.com/abc-defg-hij"),
            MediaKind::Meeting(MeetingApp::Meet)
        );
        assert_eq!(
            classify_url("https://us02web.zoom.us/j/123"),
            MediaKind::Meeting(MeetingApp::Zoom)
        );
        assert_eq!(
            classify_url("https://teams.microsoft.com/l/meetup-join/x"),
            MediaKind::Meeting(MeetingApp::Teams)
        );
        assert_eq!(
            classify_url("https://whereby.com/room"),
            MediaKind::Meeting(MeetingApp::Whereby)
        );
        assert_eq!(
            classify_url("https://open.spotify.com/track/1"),
            MediaKind::Music
        );
        assert_eq!(
            classify_url("https://example.com/youtube.com/watch?v=x"),
            MediaKind::Browser
        );
        assert_eq!(classify_url("https://notzoom.us/j/1"), MediaKind::Browser);
        assert_eq!(classify_url("about:blank"), MediaKind::Browser);
    }

    #[test]
    fn cleans_youtube_titles() {
        assert_eq!(
            clean_youtube_title("(3) Rick Astley - Never Gonna Give You Up - YouTube"),
            "Rick Astley - Never Gonna Give You Up"
        );
        assert_eq!(
            clean_youtube_title("(2020) remastered - YouTube"),
            "(2020) remastered"
        );
        assert_eq!(clean_youtube_title("Plain"), "Plain");
    }

    #[test]
    fn classifies_bundles() {
        assert_eq!(classify_bundle("com.spotify.client"), MediaKind::Music);
        assert_eq!(classify_bundle("com.apple.Music"), MediaKind::Music);
        assert_eq!(
            classify_bundle("us.zoom.xos"),
            MediaKind::Meeting(MeetingApp::Zoom)
        );
        assert_eq!(
            classify_bundle("com.microsoft.teams2"),
            MediaKind::Meeting(MeetingApp::Teams)
        );
        assert_eq!(classify_bundle("org.mozilla.firefox"), MediaKind::Browser);
        assert_eq!(
            classify_bundle("com.google.Chrome.helper"),
            MediaKind::Browser
        );
        assert_eq!(classify_bundle("com.example.game"), MediaKind::Other);
    }

    #[test]
    fn resolves_firefox_youtube_and_prefers_a_call() {
        let ff = Producer::from_bundle(10, "org.mozilla.firefox");
        let tabs = [
            tab("https://news.example/", "News", true),
            tab(
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                "(1) Never Gonna Give You Up - YouTube",
                false,
            ),
        ];
        let ctx = resolve(std::slice::from_ref(&ff), &tabs);
        assert_eq!(
            ctx.kind,
            MediaKind::YouTube {
                video_id: "dQw4w9WgXcQ".into(),
                title: "Never Gonna Give You Up".into(),
                channel: None
            }
        );
        let stamp = ctx.stamp().unwrap();
        assert_eq!(stamp.class, MediaClass::YouTube);
        assert_eq!(
            stamp.url.as_deref(),
            Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        );
        assert_eq!(stamp.title, "Never Gonna Give You Up");
        assert_eq!(stamp.producer, "Firefox");

        // the same video while Zoom plays: a call
        let zoom = Producer::from_bundle(20, "us.zoom.xos");
        let ctx = resolve(&[ff.clone(), zoom], &tabs);
        assert_eq!(ctx.kind, MediaKind::Meeting(MeetingApp::Zoom));
        assert_eq!(ctx.stamp().unwrap().class, MediaClass::Meeting);

        // Firefox on a page that explains nothing: a browser, the front tab kept
        let ctx = resolve(std::slice::from_ref(&ff), &tabs[..1]);
        assert_eq!(ctx.kind, MediaKind::Browser);
        assert_eq!(
            ctx.tab.as_ref().map(|t| t.url.as_str()),
            Some("https://news.example/")
        );

        // Spotify alone; nothing at all
        let ctx = resolve(&[Producer::from_bundle(5, "com.spotify.client")], &tabs);
        assert_eq!(ctx.kind, MediaKind::Music);
        assert_eq!(ctx.stamp().unwrap().title, "Spotify");
        assert!(resolve(&[], &tabs).stamp().is_none());
        // a Chrome producer never borrows Firefox's tabs
        let ctx = resolve(&[Producer::from_bundle(6, "com.google.Chrome")], &tabs);
        assert_eq!(ctx.kind, MediaKind::Browser);
        assert!(ctx.tab.is_none());
    }
}
