//! The media resolver's thread: polls the audio producers every couple of seconds and, while
//! Firefox is one of them, its session store every few seconds; publishes what plays to the
//! pipeline's [`MediaFeed`] and tells the host on every change. Idle (blocked, no timer)
//! while not active — the host activates it while listening with system audio.
//!
//! With a database path it also re-reads the user's per-app classes (`producer_overrides`)
//! every few seconds — a change in Preferences takes effect at the next look, no restart —
//! and records every app it sees playing (`producers_seen`, the Preferences "Apps" table).

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use oats_core::{MediaFeed, Store};
use oats_sources::{AudioProducers, ProducerWatch};

use crate::firefox::FirefoxSession;
use crate::{MediaContext, ProducerOverrides, WindowTab, is_firefox, resolve};

/// How often the watcher looks.
#[derive(Clone, Copy, Debug)]
pub struct WatcherConfig {
    /// The producers (Core Audio: a few property reads).
    pub producers_every: Duration,
    /// Firefox's session file (a directory scan + `stat`; parsed only when rewritten).
    pub firefox_every: Duration,
    /// The per-app classes in the store (one small SELECT).
    pub overrides_every: Duration,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            producers_every: Duration::from_secs(2),
            firefox_every: Duration::from_secs(5),
            overrides_every: Duration::from_secs(5),
        }
    }
}

enum Msg {
    Active(bool),
    Stop,
}

/// Called with the new context on every change (`None`: nothing plays / inactive).
pub type OnMediaChange = Box<dyn Fn(Option<&MediaContext>) + Send>;

/// The running watcher; dropping it stops the thread.
pub struct MediaWatcher {
    tx: Sender<Msg>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MediaWatcher {
    /// Start the thread (inactive until [`Self::set_active`]). `store`: the database the
    /// per-app classes are read from and the seen apps written to (`None`: no overrides).
    pub fn spawn(
        producers: Box<dyn AudioProducers>,
        firefox: FirefoxSession,
        feed: MediaFeed,
        on_change: OnMediaChange,
        config: WatcherConfig,
        store: Option<PathBuf>,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("oats-media".to_owned())
            .spawn(move || {
                let mut w = Worker {
                    producers,
                    firefox,
                    feed,
                    on_change,
                    config,
                    watch: ProducerWatch::new(),
                    playing: Vec::new(),
                    tabs: Vec::new(),
                    last_firefox: None,
                    current: None,
                    store_path: store,
                    store: None,
                    overrides: ProducerOverrides::default(),
                    last_overrides: None,
                };
                w.run(&rx);
            })
            .map_err(|e| log::warn!("could not start the media watcher: {e}"))
            .ok();
        Self { tx, thread }
    }

    /// Watch (`true`, while listening with system audio) or idle.
    pub fn set_active(&self, active: bool) {
        let _ = self.tx.send(Msg::Active(active));
    }
}

impl Drop for MediaWatcher {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Worker {
    producers: Box<dyn AudioProducers>,
    firefox: FirefoxSession,
    feed: MediaFeed,
    on_change: OnMediaChange,
    config: WatcherConfig,
    watch: ProducerWatch,
    playing: Vec<oats_sources::Producer>,
    tabs: Vec<WindowTab>,
    last_firefox: Option<Instant>,
    current: Option<MediaContext>,
    store_path: Option<PathBuf>,
    /// Opened on first use, on this thread.
    store: Option<Store>,
    overrides: ProducerOverrides,
    last_overrides: Option<Instant>,
}

impl Worker {
    fn run(&mut self, rx: &Receiver<Msg>) {
        let mut active = false;
        loop {
            let msg = if active {
                match rx.recv_timeout(self.config.producers_every) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => return,
                }
            };
            match msg {
                Some(Msg::Stop) => return,
                Some(Msg::Active(a)) => {
                    if a != active {
                        log::debug!("media watcher {}", if a { "on" } else { "off" });
                    }
                    active = a;
                    if !active {
                        self.watch = ProducerWatch::new();
                        self.playing.clear();
                        self.publish(None);
                        continue;
                    }
                }
                None => {}
            }
            self.tick();
        }
    }

    /// The store, opened on first use (`None` without a path, or when it can't be opened).
    fn store(&mut self) -> Option<&Store> {
        if self.store.is_none() {
            let path = self.store_path.take()?;
            match Store::open(&path) {
                Ok(s) => self.store = Some(s),
                Err(e) => log::warn!("media watcher: cannot open the store: {e}"),
            }
        }
        self.store.as_ref()
    }

    /// Re-read the per-app classes when due; `true` when they changed.
    fn reload_overrides(&mut self) -> bool {
        let due = self
            .last_overrides
            .is_none_or(|t| t.elapsed() >= self.config.overrides_every);
        if !due {
            return false;
        }
        self.last_overrides = Some(Instant::now());
        let Some(store) = self.store() else {
            return false;
        };
        match store.producer_overrides() {
            Ok(rows) => {
                let next = ProducerOverrides::new(rows);
                if next != self.overrides {
                    log::debug!("per-app classes changed");
                    self.overrides = next;
                    return true;
                }
            }
            Err(e) => log::warn!("media watcher: per-app classes: {e}"),
        }
        false
    }

    /// One look: producers, Firefox when it plays, resolve, publish changes.
    fn tick(&mut self) {
        let mut changed = self.reload_overrides();
        if let Some(now) = self.watch.poll(self.producers.as_mut()) {
            log::debug!(
                "audio producers: {:?}",
                now.iter().map(|p| p.bundle_id.as_str()).collect::<Vec<_>>()
            );
            let seen: Vec<(String, String)> = now
                .iter()
                .map(|p| (p.bundle_id.clone(), p.name.clone()))
                .collect();
            if !seen.is_empty()
                && let Some(store) = self.store()
                && let Err(e) = store.note_producers_seen(&seen, now_ms())
            {
                log::warn!("media watcher: recording the apps seen: {e}");
            }
            self.playing = now;
            changed = true;
        }
        let firefox_plays = self.playing.iter().any(|p| is_firefox(&p.bundle_id));
        if firefox_plays {
            let due = changed
                || self
                    .last_firefox
                    .is_none_or(|t| t.elapsed() >= self.config.firefox_every);
            if due {
                self.last_firefox = Some(Instant::now());
                let tabs = self.firefox.tabs();
                if tabs != self.tabs {
                    self.tabs = tabs;
                    changed = true;
                }
            }
        } else if !self.tabs.is_empty() {
            self.tabs.clear();
            changed = true;
        }
        if changed {
            let ctx = resolve(&self.playing, &self.tabs, &self.overrides);
            let ctx = ctx.stamp().is_some().then_some(ctx);
            self.publish(ctx);
        }
    }

    fn publish(&mut self, ctx: Option<MediaContext>) {
        if ctx == self.current {
            return;
        }
        match &ctx {
            Some(c) => log::info!(
                "media: {} {:?}{}",
                c.kind.class().as_str(),
                c.producer.as_ref().map(|p| p.name.as_str()).unwrap_or(""),
                if c.background {
                    " (background tab)"
                } else {
                    ""
                }
            ),
            None => log::debug!("media: nothing playing"),
        }
        log::debug!("media title: {:?}", ctx.as_ref().map(|c| c.title()));
        self.feed.set(ctx.as_ref().and_then(|c| c.stamp()));
        (self.on_change)(ctx.as_ref());
        self.current = ctx;
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oats_core::MediaClass;
    use oats_sources::{FixedProducers, Producer};
    use std::sync::{Arc, Mutex};

    #[test]
    fn publishes_what_plays_and_clears_when_inactive() {
        let root = std::env::temp_dir().join(format!("oats-mw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("p.default").join("sessionstore-backups");
        std::fs::create_dir_all(&dir).unwrap();
        let json = serde_json::json!({ "windows": [{ "selected": 1, "tabs": [{ "index": 1,
            "entries": [{ "url": "https://www.youtube.com/watch?v=abc", "title": "A talk - YouTube" }] }] }] });
        std::fs::write(
            dir.join("recovery.jsonlz4"),
            crate::firefox::encode_mozlz4(&serde_json::to_vec(&json).unwrap()),
        )
        .unwrap();

        let fixed = FixedProducers::default();
        let feed = MediaFeed::new();
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let seen2 = seen.clone();
        let watcher = MediaWatcher::spawn(
            Box::new(fixed.clone()),
            FirefoxSession::with_roots(vec![root.clone()]),
            feed.clone(),
            Box::new(move |c| seen2.lock().unwrap().push(c.map(|c| c.title()))),
            WatcherConfig {
                producers_every: Duration::from_millis(10),
                firefox_every: Duration::from_millis(10),
                overrides_every: Duration::from_millis(10),
            },
            None,
        );
        fixed.set(vec![Producer::from_bundle(42, "org.mozilla.firefox")]);
        watcher.set_active(true);
        let deadline = Instant::now() + Duration::from_secs(5);
        while feed.current().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let stamp = feed.current().unwrap();
        assert_eq!(stamp.class, MediaClass::YouTube);
        assert_eq!(stamp.title, "A talk");
        watcher.set_active(false);
        let deadline = Instant::now() + Duration::from_secs(5);
        while feed.current().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(feed.current().is_none());
        drop(watcher);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen, vec![Some("A talk".to_string()), None]);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn wait(what: &str, pred: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pred() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn records_seen_apps_and_follows_a_class_change_without_a_restart() {
        let dir = std::env::temp_dir().join(format!("oats-mw-ov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("oats.sqlite");
        Store::open(&db).unwrap();
        let fixed = FixedProducers::default();
        let feed = MediaFeed::new();
        let watcher = MediaWatcher::spawn(
            Box::new(fixed.clone()),
            FirefoxSession::with_roots(vec![]),
            feed.clone(),
            Box::new(|_| {}),
            WatcherConfig {
                producers_every: Duration::from_millis(10),
                firefox_every: Duration::from_millis(10),
                overrides_every: Duration::from_millis(10),
            },
            Some(db.clone()),
        );
        fixed.set(vec![Producer::from_bundle(7, "com.hnc.Discord")]);
        watcher.set_active(true);
        wait("Discord as media", || {
            feed.current().is_some_and(|s| s.class == MediaClass::App)
        });
        let store = Store::open(&db).unwrap();
        wait("Discord recorded as seen", || {
            store
                .producers_seen()
                .unwrap()
                .iter()
                .any(|p| p.bundle_id == "com.hnc.discord")
        });
        store
            .set_producer_override("com.hnc.discord", oats_core::ProducerClass::Meeting, false)
            .unwrap();
        wait("Discord as a call", || {
            feed.current()
                .is_some_and(|s| s.class == MediaClass::Meeting)
        });
        drop(watcher);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
