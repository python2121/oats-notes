//! Video thumbnails for the Media view — OPT-IN (Preferences "Fetch video thumbnails"; off by
//! default). The app otherwise never touches the network itself (the model download and the
//! `claude` CLI are the only other network users), so nothing here runs while the pref is off:
//! [`Thumbs::get`] with `enabled == false` returns `None` without looking at the disk or
//! starting the worker.
//!
//! When on, a YouTube play's thumbnail (`https://i.ytimg.com/vi/<id>/mqdefault.jpg`, 320×180)
//! is fetched ONCE by `/usr/bin/curl` (no HTTP crate) on a background thread into
//! `<data dir>/thumbs/<id>.jpg` (directory 0700, file 0600), decoded there, and handed to the
//! UI thread, which makes it an egui texture kept for the session. A missing curl, a failed
//! fetch or a file that doesn't decode leaves the plain placeholder (no retry until the next
//! launch). Fetches are logged at debug level with the video id only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

/// Where curl is expected; without it nothing is fetched.
pub const CURL: &str = "/usr/bin/curl";

/// Downloads `url` into `out`; `true` on success. Swappable for tests.
pub type Fetcher = Arc<dyn Fn(&str, &Path) -> bool + Send + Sync>;

/// `<dir>/<video_id>.jpg`, or `None` for an id that is not a plain YouTube id
/// (`[A-Za-z0-9_-]`, 1–64 characters) — so nothing odd ever reaches a path or a URL.
pub fn thumb_path(dir: &Path, video_id: &str) -> Option<PathBuf> {
    let ok = !video_id.is_empty()
        && video_id.len() <= 64
        && video_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then(|| dir.join(format!("{video_id}.jpg")))
}

/// The thumbnail directory under the data directory.
pub fn default_dir() -> PathBuf {
    oats_core::paths::data_dir().join("thumbs")
}

/// The real fetcher: `/usr/bin/curl -q --silent --fail --max-time 5 --proto =https --output
/// <out> <url>` (`-q`: no ~/.curlrc), no stdin/stdout/stderr. `false` when curl is missing or
/// fails.
pub fn curl_fetch(url: &str, out: &Path) -> bool {
    if !Path::new(CURL).exists() {
        log::debug!("thumbnail: no {CURL}; not fetched");
        return false;
    }
    std::process::Command::new(CURL)
        .args([
            "-q",
            "--silent",
            "--fail",
            "--max-time",
            "5",
            "--proto",
            "=https",
        ])
        .arg("--output")
        .arg(out)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

enum Slot {
    Pending,
    Ready(egui::TextureHandle),
    Failed,
}

struct Job {
    video_id: String,
    path: PathBuf,
}

type Done = (String, Option<egui::ColorImage>);

/// The session's thumbnails (textures by video id) and the worker that fetches / decodes them.
pub struct Thumbs {
    dir: PathBuf,
    fetcher: Fetcher,
    slots: HashMap<String, Slot>,
    jobs: Option<Sender<Job>>,
    done_tx: Sender<Done>,
    done_rx: Receiver<Done>,
}

impl Default for Thumbs {
    fn default() -> Self {
        Self::new(default_dir())
    }
}

impl Thumbs {
    /// Thumbnails cached in `dir`, fetched with curl.
    pub fn new(dir: PathBuf) -> Self {
        Self::with_fetcher(dir, Arc::new(curl_fetch))
    }

    pub fn with_fetcher(dir: PathBuf, fetcher: Fetcher) -> Self {
        let (done_tx, done_rx) = channel();
        Thumbs {
            dir,
            fetcher,
            slots: HashMap::new(),
            jobs: None,
            done_tx,
            done_rx,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether the worker thread was ever started (it never is while thumbnails are off).
    pub fn worker_started(&self) -> bool {
        self.jobs.is_some()
    }

    /// The thumbnail of `video_id` when it is ready. With `enabled` false: `None`, and nothing
    /// is read, fetched or started. Otherwise the first call queues it (cached file, else one
    /// fetch) and later calls return the texture once the worker delivered it.
    pub fn get(
        &mut self,
        ctx: &egui::Context,
        video_id: &str,
        enabled: bool,
    ) -> Option<egui::TextureHandle> {
        if !enabled {
            return None;
        }
        self.collect(ctx);
        match self.slots.get(video_id) {
            Some(Slot::Ready(t)) => return Some(t.clone()),
            Some(Slot::Pending | Slot::Failed) => return None,
            None => {}
        }
        let Some(path) = thumb_path(&self.dir, video_id) else {
            self.slots.insert(video_id.to_owned(), Slot::Failed);
            return None;
        };
        let jobs = self.jobs.get_or_insert_with(|| {
            spawn_worker(self.fetcher.clone(), self.done_tx.clone(), ctx.clone())
        });
        let sent = jobs
            .send(Job {
                video_id: video_id.to_owned(),
                path,
            })
            .is_ok();
        self.slots.insert(
            video_id.to_owned(),
            if sent { Slot::Pending } else { Slot::Failed },
        );
        None
    }

    /// Turn what the worker delivered into textures.
    fn collect(&mut self, ctx: &egui::Context) {
        while let Ok((id, image)) = self.done_rx.try_recv() {
            let slot = match image {
                Some(img) => Slot::Ready(ctx.load_texture(
                    format!("thumb-{id}"),
                    img,
                    egui::TextureOptions::LINEAR,
                )),
                None => Slot::Failed,
            };
            self.slots.insert(id, slot);
        }
    }

    /// Drop a video's texture and delete its cached file (its last play was deleted).
    pub fn forget(&mut self, video_id: &str) {
        self.slots.remove(video_id);
        if let Some(path) = thumb_path(&self.dir, video_id)
            && path.exists()
            && let Err(e) = std::fs::remove_file(&path)
        {
            log::warn!("thumbnail: could not delete a cached file: {e}");
        }
    }
}

fn spawn_worker(fetcher: Fetcher, done: Sender<Done>, ctx: egui::Context) -> Sender<Job> {
    let (tx, rx) = channel::<Job>();
    let spawned = std::thread::Builder::new()
        .name("oats-thumbs".to_owned())
        .spawn(move || {
            while let Ok(job) = rx.recv() {
                let image = load_or_fetch(&fetcher, &job);
                if done.send((job.video_id, image)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
    if let Err(e) = spawned {
        log::warn!("thumbnail worker: {e}");
    }
    tx
}

/// The cached file, else one fetch into it; decoded.
fn load_or_fetch(fetcher: &Fetcher, job: &Job) -> Option<egui::ColorImage> {
    if !job.path.exists() {
        let dir = job.path.parent()?;
        if let Err(e) = oats_core::paths::secure_dir(dir) {
            log::warn!("thumbnail directory: {e}");
            return None;
        }
        let part = job.path.with_extension("jpg.part");
        log::debug!("thumbnail: fetching {}", job.video_id);
        let ok = fetcher(&oats_media::youtube_thumbnail_url(&job.video_id), &part);
        if !ok || !part.exists() {
            let _ = std::fs::remove_file(&part);
            log::debug!("thumbnail: fetch failed for {}", job.video_id);
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o600));
        }
        if std::fs::rename(&part, &job.path).is_err() {
            let _ = std::fs::remove_file(&part);
            return None;
        }
    }
    let bytes = std::fs::read(&job.path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(
        size,
        img.as_flat_samples().as_slice(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oats-thumbs-{tag}-{}", std::process::id()))
    }

    #[test]
    fn the_path_is_the_video_id_in_the_thumbs_dir_and_odd_ids_are_refused() {
        let dir = Path::new("/data/thumbs");
        assert_eq!(
            thumb_path(dir, "dQw4w9WgXcQ"),
            Some(PathBuf::from("/data/thumbs/dQw4w9WgXcQ.jpg"))
        );
        assert_eq!(
            thumb_path(dir, "a_b-C9"),
            Some(PathBuf::from("/data/thumbs/a_b-C9.jpg"))
        );
        for bad in ["", "../etc", "a/b", "a.b", "x y", &"a".repeat(65)] {
            assert_eq!(thumb_path(dir, bad), None, "{bad}");
        }
        assert!(default_dir().ends_with("thumbs"));
    }

    #[test]
    fn nothing_is_fetched_or_read_while_thumbnails_are_off() {
        let dir = temp("off");
        let _ = std::fs::remove_dir_all(&dir);
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let mut thumbs = Thumbs::with_fetcher(
            dir.clone(),
            Arc::new(move |_url: &str, _out: &Path| {
                c.fetch_add(1, Ordering::SeqCst);
                false
            }),
        );
        let ctx = egui::Context::default();
        for _ in 0..3 {
            assert!(thumbs.get(&ctx, "dQw4w9WgXcQ", false).is_none());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!thumbs.worker_started());
        assert!(!dir.exists(), "not even the directory");
    }

    #[test]
    fn when_on_a_thumbnail_is_fetched_once_into_the_dir_owner_only_and_decoded() {
        let dir = temp("on");
        let _ = std::fs::remove_dir_all(&dir);
        let calls = Arc::new(AtomicUsize::new(0));
        let urls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let (c, u) = (calls.clone(), urls.clone());
        // a tiny PNG stands in for YouTube's JPEG (the decoder guesses the format)
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(4, 2, image::Rgba([200, 10, 10, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let mut thumbs = Thumbs::with_fetcher(
            dir.clone(),
            Arc::new(move |url: &str, out: &Path| {
                c.fetch_add(1, Ordering::SeqCst);
                u.lock().unwrap().push(url.to_owned());
                std::fs::write(out, &png).is_ok()
            }),
        );
        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let tex = loop {
            if let Some(t) = thumbs.get(&ctx, "abc_DEF-123", true) {
                break t;
            }
            assert!(std::time::Instant::now() < deadline, "no thumbnail");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(tex.size(), [4, 2]);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "fetched once");
        assert_eq!(
            urls.lock().unwrap().as_slice(),
            ["https://i.ytimg.com/vi/abc_DEF-123/mqdefault.jpg"]
        );
        let file = dir.join("abc_DEF-123.jpg");
        assert!(file.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&file), 0o600);
            assert_eq!(mode(&dir), 0o700);
        }
        // a second session reads the cached file: no fetch
        let mut again = Thumbs::with_fetcher(
            dir.clone(),
            Arc::new(|_: &str, _: &Path| panic!("must not fetch a cached thumbnail")),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while again.get(&ctx, "abc_DEF-123", true).is_none() {
            assert!(std::time::Instant::now() < deadline, "no cached thumbnail");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        again.forget("abc_DEF-123");
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
