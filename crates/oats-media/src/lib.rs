//! Media context: WHAT is playing on the system-audio lane, so a YouTube video is never taken
//! for the user's own conversation (and is recorded as "watched: …" instead).
//!
//! Two permission-free signals, combined by [`resolve`] (pure, tested):
//! - the audio producers ([`oats_sources::AudioProducers`]: on macOS Core Audio's process
//!   objects) — which apps are playing sound;
//! - when one of them is Firefox, the selected tab of each Firefox window, read from its
//!   session store ([`firefox`]).
//!
//! [`classify_url`] / [`classify_bundle`] map a URL / bundle id to a [`MediaKind`]; the user's
//! per-app class ([`ProducerOverrides`], from the store's `producer_overrides`) is applied
//! before them — except for the locked call apps (Zoom, Teams, Webex, FaceTime, and Meet /
//! Zoom / Teams / Webex pages), which are always meetings. Slack and Discord are media by
//! default (the user doesn't take calls on them; "On a call" or "Mark as a call" says
//! otherwise). The result is a [`MediaContext`], published to the pipeline as an
//! [`oats_core::MediaStamp`] by the [`MediaWatcher`] thread.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::collections::HashMap;

pub mod firefox;
mod watcher;

pub use firefox::{FirefoxSession, Tab, WindowTab};
pub use oats_core::{MediaClass, MediaPolicy, MediaStamp, ProducerClass, ProducerOverride};
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
    FaceTime,
    /// An app the user classes as Meeting (its name is the producer's).
    App,
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
            MeetingApp::FaceTime => "FaceTime",
            MeetingApp::App => "Call",
        }
    }

    /// A built-in call app / page: always a meeting, never overridable ("you'd never watch a
    /// video through them").
    pub fn locked(self) -> bool {
        !matches!(self, MeetingApp::App)
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
    /// An app whose sound is media by its class (Slack, Discord by default; any app the user
    /// classes Media).
    AppMedia,
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
            MediaKind::AppMedia => MediaClass::App,
            MediaKind::Browser => MediaClass::Browser,
            MediaKind::Other => MediaClass::Other,
        }
    }

    /// More decisive kinds win when several producers play at once: a call beats a video
    /// (calls are never media, and losing a call's transcript is the worse mistake), a video
    /// beats music, anything recognised beats a bare browser.
    fn rank(&self) -> u8 {
        match self {
            MediaKind::Meeting(_) => 5,
            MediaKind::YouTube { .. } => 4,
            MediaKind::Music | MediaKind::AppMedia => 3,
            MediaKind::Browser => 2,
            MediaKind::Other => 1,
        }
    }

    /// A built-in call (see [`MeetingApp::locked`]).
    pub fn is_locked_meeting(&self) -> bool {
        matches!(self, MediaKind::Meeting(app) if app.locked())
    }

    /// Media proper (see [`MediaClass::is_media`]).
    pub fn is_media(&self) -> bool {
        self.class().is_media()
    }
}

/// The user's per-app classes, keyed by lower-case bundle id (see
/// [`oats_core::Store::producer_overrides`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProducerOverrides {
    by_bundle: HashMap<String, ProducerOverride>,
}

impl ProducerOverrides {
    pub fn new(rows: impl IntoIterator<Item = ProducerOverride>) -> Self {
        Self {
            by_bundle: rows
                .into_iter()
                .map(|o| (o.bundle_id.to_ascii_lowercase(), o))
                .collect(),
        }
    }

    /// The override for `bundle_id`, if any.
    pub fn get(&self, bundle_id: &str) -> Option<&ProducerOverride> {
        self.by_bundle.get(&bundle_id.to_ascii_lowercase())
    }

    /// The class `bundle_id` is treated as: locked apps are Meeting, else the override,
    /// else its built-in default ([`default_class`]).
    pub fn effective_class(&self, bundle_id: &str) -> ProducerClass {
        if is_locked(bundle_id) {
            return ProducerClass::Meeting;
        }
        match self.get(bundle_id).map(|o| o.class) {
            Some(c) if c != ProducerClass::Auto => c,
            _ => default_class(bundle_id),
        }
    }

    /// "Transcribe anyway" is set for `bundle_id`.
    pub fn transcribe_anyway(&self, bundle_id: &str) -> bool {
        self.get(bundle_id).is_some_and(|o| o.transcribe_anyway)
    }
}

/// An app that is always a call (Zoom, Teams, Webex, FaceTime): its class can't be changed.
pub fn is_locked(bundle_id: &str) -> bool {
    classify_bundle(bundle_id).is_locked_meeting()
}

/// What the built-in rules make of an app: Meeting (call apps), Media (Slack, Discord,
/// music apps), else Auto (a browser is decided by its tab; anything else is not media).
pub fn default_class(bundle_id: &str) -> ProducerClass {
    match classify_bundle(bundle_id) {
        MediaKind::Meeting(_) => ProducerClass::Meeting,
        MediaKind::AppMedia | MediaKind::Music | MediaKind::YouTube { .. } => ProducerClass::Media,
        MediaKind::Browser | MediaKind::Other => ProducerClass::Auto,
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
    /// The app is classed Ignore: neither transcribed nor logged.
    pub ignored: bool,
    /// A media app the user transcribes anyway (whatever the media policy).
    pub transcribe_anyway: bool,
}

impl MediaContext {
    /// Nothing known to be playing.
    pub fn nothing() -> Self {
        Self {
            producer: None,
            tab: None,
            kind: MediaKind::Other,
            ignored: false,
            transcribe_anyway: false,
        }
    }

    /// How decisive this context is among several producers: an ignored app loses to
    /// anything (see `MediaKind::rank`).
    fn rank(&self) -> u8 {
        if self.ignored { 0 } else { self.kind.rank() }
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
        if let MediaKind::Meeting(app) = self.kind
            && app != MeetingApp::App
        {
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
        let policy = if self.ignored {
            Some(MediaPolicy::Ignore)
        } else if self.transcribe_anyway && self.kind.is_media() {
            Some(MediaPolicy::Transcribe)
        } else {
            None
        };
        Some(MediaStamp {
            class: if self.ignored {
                MediaClass::App
            } else {
                self.kind.class()
            },
            url: self.tab.as_ref().map(|t| t.url.clone()),
            title: self.title(),
            producer: self
                .producer
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_default(),
            bundle_id: self
                .producer
                .as_ref()
                .map(|p| p.bundle_id.clone())
                .unwrap_or_default(),
            policy,
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
        // the user doesn't take calls on Slack / Discord: any of their pages is media by default
        // (the class of the desktop app, see `web_app_bundle`)
        "app.slack.com" | "discord.com" | "ptb.discord.com" | "canary.discord.com" => {
            MediaKind::AppMedia
        }
        h if is_domain(h, "zoom.us") || is_domain(h, "zoom.com") => {
            MediaKind::Meeting(MeetingApp::Zoom)
        }
        h if is_domain(h, "webex.com") => MediaKind::Meeting(MeetingApp::Webex),
        h if is_domain(h, "bandcamp.com") => MediaKind::Music,
        _ => MediaKind::Browser,
    }
}

/// The desktop app a web page stands for (its per-app class applies to the page): any
/// `app.slack.com` / `discord.com` page.
pub fn web_app_bundle(url: &str) -> Option<&'static str> {
    let (host, _) = split_url(url)?;
    match host.as_str() {
        "app.slack.com" => Some("com.tinyspeck.slackmacgap"),
        "discord.com" | "ptb.discord.com" | "canary.discord.com" => Some("com.hnc.discord"),
        _ => None,
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

/// The well-known apps the classifier maps web pages (and desktop apps) to, as
/// `(bundle id, name, web too)`: the locked call apps first, then Slack and Discord (whose
/// web pages take the desktop app's class). The Preferences Apps table always lists them, so
/// an app the user only runs in the browser can still be viewed and changed.
pub const KNOWN_APPS: &[(&str, &str, bool)] = &[
    ("us.zoom.xos", "Zoom", false),
    ("com.microsoft.teams2", "Microsoft Teams", false),
    ("com.cisco.webexmeetingsapp", "Webex", false),
    ("com.apple.facetime", "FaceTime", false),
    ("com.tinyspeck.slackmacgap", "Slack", true),
    ("com.hnc.discord", "Discord", true),
];

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
    } else if starts("com.hnc.discord") || starts("com.tinyspeck.slackmacgap") {
        MediaKind::AppMedia
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

/// Decide what is playing from the producers and Firefox's open tabs.
///
/// For a Firefox producer ONLY the selected tab decides (the session store has no "this tab
/// plays sound" flag, so guessing from background tabs was wrong more often than right): the
/// selected tab of the most recently focused window (`selectedWindow`), else of the first
/// window. YouTube / music → media; a call URL (Meet, Whereby, Jitsi, Zoom, Teams, Webex) →
/// Meeting; a Slack / Discord page → the class of the desktop app (`web_app_bundle`, with its
/// override); anything else → Browser (transcribed as "them", not media).
///
/// The user's class for the app (`overrides`) then applies, unless the result is a built-in
/// call (locked): Meeting makes it a call, Media makes it media (a video / music tab stays
/// what it is), Ignore marks it ignored; "transcribe anyway" is kept for media. Of several
/// producers the most decisive kind wins (see `MediaKind::rank`; a call beats a video, an
/// ignored app loses to anything); ties go to the first. No producer = nothing playing.
pub fn resolve(
    producers: &[Producer],
    firefox_tabs: &[WindowTab],
    overrides: &ProducerOverrides,
) -> MediaContext {
    let mut best: Option<MediaContext> = None;
    for p in producers {
        let mut ctx = MediaContext {
            producer: Some(p.clone()),
            tab: None,
            kind: classify_bundle(&p.bundle_id),
            ignored: false,
            transcribe_anyway: false,
        };
        let mut override_id = p.bundle_id.as_str();
        if is_firefox(&p.bundle_id)
            && let Some(app) = firefox_context(&mut ctx, firefox_tabs)
        {
            override_id = app;
        }
        apply_override(&mut ctx, overrides.get(override_id));
        if best.as_ref().is_none_or(|b| ctx.rank() > b.rank()) {
            best = Some(ctx);
        }
    }
    best.unwrap_or_else(MediaContext::nothing)
}

/// [`resolve`]'s per-app class.
fn apply_override(ctx: &mut MediaContext, ov: Option<&ProducerOverride>) {
    if ctx.kind.is_locked_meeting() {
        return;
    }
    let Some(ov) = ov else {
        return;
    };
    let keep_media = matches!(ctx.kind, MediaKind::YouTube { .. } | MediaKind::Music);
    match ov.class {
        ProducerClass::Auto => {}
        ProducerClass::Meeting => ctx.kind = MediaKind::Meeting(MeetingApp::App),
        ProducerClass::Media | ProducerClass::Ignore if !keep_media => {
            ctx.kind = MediaKind::AppMedia;
        }
        ProducerClass::Media | ProducerClass::Ignore => {}
    }
    ctx.ignored = ov.class == ProducerClass::Ignore;
    ctx.transcribe_anyway = ov.transcribe_anyway && ctx.kind.is_media() && !ctx.ignored;
}

/// [`resolve`]'s Firefox rules: the selected tab of the front window decides. Returns the
/// desktop bundle id whose class applies when the tab is a Slack / Discord page.
fn firefox_context(ctx: &mut MediaContext, tabs: &[WindowTab]) -> Option<&'static str> {
    let selected = tabs
        .iter()
        .filter(|w| w.selected)
        .min_by_key(|w| (!w.front_window, w.window));
    let w = selected?;
    ctx.kind = classify_tab(&w.tab);
    ctx.tab = Some(w.tab.clone());
    let app = web_app_bundle(&w.tab.url);
    log::debug!(
        "media: firefox selected tab host {:?} -> {}",
        split_url(&w.tab.url).map(|(h, _)| h).unwrap_or_default(),
        ctx.kind.class().as_str()
    );
    app
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> ProducerOverrides {
        ProducerOverrides::default()
    }

    fn ov(bundle: &str, class: ProducerClass, transcribe_anyway: bool) -> ProducerOverrides {
        ProducerOverrides::new([ProducerOverride {
            bundle_id: bundle.into(),
            class,
            transcribe_anyway,
        }])
    }

    #[test]
    fn slack_and_discord_are_media_by_default_and_call_apps_are_locked() {
        for b in ["com.hnc.Discord", "com.tinyspeck.slackmacgap"] {
            assert_eq!(classify_bundle(b), MediaKind::AppMedia);
            assert_eq!(default_class(b), ProducerClass::Media);
            assert!(!is_locked(b));
            let ctx = resolve(&[Producer::from_bundle(1, b)], &[], &none());
            let stamp = ctx.stamp().unwrap();
            assert_eq!(stamp.class, MediaClass::App);
            assert_eq!(stamp.bundle_id, b);
            assert_eq!(stamp.policy, None);
        }
        assert_eq!(
            classify_url("https://app.slack.com/huddle/T1/C2"),
            MediaKind::AppMedia
        );
        assert_eq!(
            classify_url("https://discord.com/channels/1/2"),
            MediaKind::AppMedia
        );
        assert_eq!(classify_url("https://discord.com/"), MediaKind::AppMedia);
        assert_eq!(
            classify_url("https://app.slack.com/client/T1/C2"),
            MediaKind::AppMedia
        );
        for b in [
            "us.zoom.xos",
            "com.microsoft.teams2",
            "com.cisco.webexmeetingsapp",
            "com.apple.FaceTime",
        ] {
            assert!(is_locked(b), "{b}");
            assert_eq!(default_class(b), ProducerClass::Meeting);
            // an override can't make a call app media
            let o = ov(b, ProducerClass::Ignore, false);
            assert_eq!(o.effective_class(b), ProducerClass::Meeting);
            let ctx = resolve(&[Producer::from_bundle(1, b)], &[], &o);
            assert!(!ctx.ignored);
            assert_eq!(ctx.stamp().unwrap().class, MediaClass::Meeting);
        }
        // a Meet tab stays a call even in a browser classed Media
        let ff = Producer::from_bundle(10, "org.mozilla.firefox");
        let meet = [tab("https://meet.google.com/abc", "Meet", true)];
        let ctx = resolve(
            std::slice::from_ref(&ff),
            &meet,
            &ov("org.mozilla.firefox", ProducerClass::Media, false),
        );
        assert_eq!(ctx.kind, MediaKind::Meeting(MeetingApp::Meet));
        assert_eq!(default_class("com.example.game"), ProducerClass::Auto);
    }

    #[test]
    fn overrides_apply_before_the_built_in_rules() {
        let discord = Producer::from_bundle(1, "com.hnc.Discord");
        // Meeting: a call, titled by the app
        let o = ov("com.hnc.discord", ProducerClass::Meeting, false);
        assert_eq!(o.effective_class("com.hnc.Discord"), ProducerClass::Meeting);
        let ctx = resolve(std::slice::from_ref(&discord), &[], &o);
        assert_eq!(ctx.kind, MediaKind::Meeting(MeetingApp::App));
        assert_eq!(ctx.title(), "Discord");
        assert_eq!(ctx.stamp().unwrap().class, MediaClass::Meeting);
        // transcribe anyway: still media, with its own policy
        let o = ov("com.hnc.discord", ProducerClass::Auto, true);
        let stamp = resolve(std::slice::from_ref(&discord), &[], &o)
            .stamp()
            .unwrap();
        assert_eq!(stamp.class, MediaClass::App);
        assert_eq!(stamp.policy, Some(MediaPolicy::Transcribe));
        // Ignore: skipped, and it loses to anything else playing
        let o = ov("com.hnc.discord", ProducerClass::Ignore, true);
        let ctx = resolve(std::slice::from_ref(&discord), &[], &o);
        assert!(ctx.ignored);
        assert_eq!(ctx.stamp().unwrap().policy, Some(MediaPolicy::Ignore));
        let game = Producer::from_bundle(2, "com.example.game");
        let ctx = resolve(&[discord.clone(), game], &[], &o);
        assert_eq!(ctx.kind, MediaKind::Other);
        // Media: an unknown app becomes media
        let o = ov("com.example.player", ProducerClass::Media, false);
        let ctx = resolve(&[Producer::from_bundle(3, "com.example.player")], &[], &o);
        assert_eq!(ctx.kind, MediaKind::AppMedia);
        // Auto: the built-in rules
        let ctx = resolve(std::slice::from_ref(&discord), &[], &none());
        assert_eq!(ctx.kind, MediaKind::AppMedia);
        // a call app playing alongside Discord wins
        let zoom = Producer::from_bundle(4, "us.zoom.xos");
        let ctx = resolve(&[discord, zoom], &[], &none());
        assert_eq!(ctx.kind, MediaKind::Meeting(MeetingApp::Zoom));
    }

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

    fn ff_ctx(tabs: &[WindowTab], o: &ProducerOverrides) -> MediaContext {
        let ff = Producer::from_bundle(10, "org.mozilla.firefox");
        resolve(std::slice::from_ref(&ff), tabs, o)
    }

    #[test]
    fn only_the_selected_tab_decides_for_a_browser() {
        let idle_meet = background("https://meet.google.com/abc", "Meet - abc", 50);
        let idle_video = background("https://www.youtube.com/watch?v=old", "Old - YouTube", 900);
        // Slack channel page selected: media by default (the desktop app's class)
        let slack = tab(
            "https://app.slack.com/client/T1/C2",
            "general - Slack",
            true,
        );
        let ctx = ff_ctx(&[idle_meet.clone(), slack.clone()], &none());
        assert_eq!(ctx.kind, MediaKind::AppMedia);
        assert_eq!(ctx.stamp().unwrap().class, MediaClass::App);
        // ... with Slack classed Meeting (keyed by the desktop bundle id): a call
        let o = ov("com.tinyspeck.slackmacgap", ProducerClass::Meeting, false);
        let ctx = ff_ctx(std::slice::from_ref(&slack), &o);
        assert_eq!(ctx.kind, MediaKind::Meeting(MeetingApp::App));
        // Discord likewise
        let discord = tab("https://discord.com/app", "Discord", true);
        assert_eq!(
            ff_ctx(std::slice::from_ref(&discord), &none()).kind,
            MediaKind::AppMedia
        );
        let o = ov("com.hnc.discord", ProducerClass::Ignore, false);
        assert!(ff_ctx(&[discord], &o).ignored);
        // an idle Meet tab behind a selected YouTube video: media
        let video = tab("https://youtu.be/xyz", "Xyz - YouTube", true);
        let ctx = ff_ctx(&[idle_meet.clone(), video], &none());
        assert_eq!(ctx.kind.class(), MediaClass::YouTube);
        assert_eq!(ctx.title(), "Xyz");
        // an idle Meet tab (and a video) behind a news page: just a browser
        let news = tab("https://news.example/", "News", true);
        let ctx = ff_ctx(&[idle_meet.clone(), idle_video.clone(), news], &none());
        assert_eq!(ctx.kind, MediaKind::Browser);
        assert_eq!(ctx.stamp().unwrap().class, MediaClass::Browser);
        // a background video alone is not a guess any more
        let home = tab("https://www.youtube.com/", "YouTube", true);
        assert_eq!(
            ff_ctx(&[idle_video, home], &none()).kind,
            MediaKind::Browser
        );
        // the Meet tab selected: a call
        let meet = tab("https://meet.google.com/abc", "Meet - abc", true);
        assert_eq!(
            ff_ctx(&[meet], &none()).kind,
            MediaKind::Meeting(MeetingApp::Meet)
        );
    }

    #[test]
    fn several_windows_use_the_front_windows_selected_tab_else_the_first() {
        let front = tab("https://news.example/", "News", true);
        let other = tab("https://meet.google.com/abc", "Meet", false);
        assert_eq!(
            ff_ctx(&[other.clone(), front.clone()], &none()).kind,
            MediaKind::Browser
        );
        // no window flagged as front: the first window's
        let mut a = tab("https://news.example/", "News", false);
        a.window = 0;
        let mut b = tab("https://meet.google.com/abc", "Meet", false);
        b.window = 1;
        assert_eq!(ff_ctx(&[b, a], &none()).kind, MediaKind::Browser);
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
            tab("https://news.example/", "News", false),
            tab(
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                "(1) Never Gonna Give You Up - YouTube",
                true,
            ),
        ];
        let ctx = resolve(std::slice::from_ref(&ff), &tabs, &none());
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
        let ctx = resolve(&[ff.clone(), zoom], &tabs, &none());
        assert_eq!(ctx.kind, MediaKind::Meeting(MeetingApp::Zoom));
        assert_eq!(ctx.stamp().unwrap().class, MediaClass::Meeting);

        // Firefox on a page that explains nothing: a browser, the front tab kept
        let ctx = resolve(std::slice::from_ref(&ff), &tabs[..1], &none());
        assert_eq!(ctx.kind, MediaKind::Browser);
        assert_eq!(
            ctx.tab.as_ref().map(|t| t.url.as_str()),
            Some("https://news.example/")
        );

        // Spotify alone; nothing at all
        let ctx = resolve(
            &[Producer::from_bundle(5, "com.spotify.client")],
            &tabs,
            &none(),
        );
        assert_eq!(ctx.kind, MediaKind::Music);
        assert_eq!(ctx.stamp().unwrap().title, "Spotify");
        assert!(resolve(&[], &tabs, &none()).stamp().is_none());
        // a Chrome producer never borrows Firefox's tabs
        let ctx = resolve(
            &[Producer::from_bundle(6, "com.google.Chrome")],
            &tabs,
            &none(),
        );
        assert_eq!(ctx.kind, MediaKind::Browser);
        assert!(ctx.tab.is_none());
    }
}
