//! The Media view (`View::Media`, ⌘6): the watch history — every play in `media_plays` (calls
//! left out), newest first, under the usual day headings. One row per play, consecutive plays
//! of the same URL at most 10 min apart folded into one (`oats_core::store::collapse_plays`,
//! durations summed): a thumbnail slot, the title, "YouTube · Firefox" / "Spotify", "2:05–2:41
//! pm · 36 min" and the URL as a link. Right-click: Open in browser, Copy link, Mark as a call,
//! Show transcript (when something was transcribed), Delete. Paged 200 plays at a time; the
//! next (older) page is fetched as the bottom edge comes within reach.
//!
//! Thumbnails are opt-in (see `thumbs`); without them, and for anything but YouTube, the slot
//! is a flat placeholder with a ▶.

use std::sync::Arc;

use chrono::{Datelike, Local};
use egui::text::{LayoutJob, TextWrapping};
use egui::{Align2, Rect, Sense, TextFormat, pos2, vec2};
use oats_core::store::{MediaEntry, collapse_plays};
use oats_core::{MediaClass, MediaPlay};

use crate::theme::Tokens;
use crate::thumbs::Thumbs;
use crate::transcript::{TranscriptFocus, day_heading, day_of, focus_row, near_viewport};

/// Plays per page.
pub const MEDIA_PAGE: usize = 200;
/// The thumbnail slot, in points (16:9).
pub const THUMB_SIZE: egui::Vec2 = egui::vec2(96.0, 54.0);
const ROW_H: f32 = 72.0;
const MARGIN: f32 = 14.0;

/// The loaded part of the history: rows (folded), and where the next page starts.
#[derive(Clone, Debug, Default)]
pub struct MediaPages {
    pub entries: Vec<MediaEntry>,
    /// `(started_at, id)` of the oldest play loaded: the next page starts below it.
    pub oldest: Option<(i64, i64)>,
    /// Older plays exist.
    pub has_more: bool,
    /// Plays loaded so far (a reload re-reads as many).
    pub plays: usize,
}

impl MediaPages {
    /// Add a page (newest first, older than everything loaded); `more`: older plays remain.
    pub fn append(&mut self, page: Vec<MediaPlay>, more: bool) {
        if let Some(last) = page.last() {
            self.oldest = Some((last.started_at, last.id));
        }
        self.plays += page.len();
        self.has_more = more;
        collapse_plays(&mut self.entries, page);
    }

    pub fn clear(&mut self) {
        *self = MediaPages::default();
    }

    /// The row holding this play.
    pub fn row_of(&self, play: i64) -> Option<&MediaEntry> {
        self.entries.iter().find(|e| e.ids.contains(&play))
    }
}

/// What the user did in the Media view.
#[derive(Clone, Debug, PartialEq)]
pub enum MediaAction {
    /// Open the URL in the browser.
    OpenUrl(String),
    CopyLink(String),
    /// "Mark as a call" on every play of the row.
    MarkAsCall(Vec<i64>),
    /// Jump to the first transcribed line of these plays.
    ShowTranscript(Vec<i64>),
    /// Delete these plays (one folded row).
    Delete(Vec<i64>),
    /// Fetch the next (older) page.
    LoadOlder,
}

/// "YouTube · Firefox", "Spotify", "Music · Firefox", "Discord".
pub fn source_line(p: &MediaPlay) -> String {
    let producer = p.producer.trim();
    let service = match p.kind {
        MediaClass::YouTube => Some("YouTube"),
        MediaClass::Music => p.url.as_deref().map(music_service),
        _ => None,
    };
    match (service, producer.is_empty()) {
        (Some(s), false) if !s.eq_ignore_ascii_case(producer) => format!("{s} · {producer}"),
        (Some(s), _) => s.to_owned(),
        (None, false) => producer.to_owned(),
        (None, true) => match p.kind {
            MediaClass::Music => "Music".to_owned(),
            _ => "Media".to_owned(),
        },
    }
}

/// The music service of a page, by host.
pub(crate) fn music_service(url: &str) -> &'static str {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|r| r.split(['/', '?', '#']).next())
        .unwrap_or("")
        .to_ascii_lowercase();
    if host.contains("spotify") {
        "Spotify"
    } else if host.contains("music.apple") {
        "Apple Music"
    } else if host.contains("music.youtube") {
        "YouTube Music"
    } else if host.contains("soundcloud") {
        "SoundCloud"
    } else if host.contains("bandcamp") {
        "Bandcamp"
    } else if host.contains("tidal") {
        "Tidal"
    } else if host.contains("deezer") {
        "Deezer"
    } else {
        "Music"
    }
}

/// What a row is called: the title, else the URL's host and path, else the app.
pub fn entry_title(p: &MediaPlay) -> String {
    if !p.title.trim().is_empty() {
        return p.title.trim().to_owned();
    }
    if let Some(url) = &p.url {
        return url
            .split("://")
            .nth(1)
            .unwrap_or(url)
            .trim_start_matches("www.")
            .to_owned();
    }
    if !p.producer.trim().is_empty() {
        return p.producer.trim().to_owned();
    }
    "Something played".to_owned()
}

/// "36 min", "1 h 5 min", "2 h", "45 s".
pub fn duration_label(ms: i64) -> String {
    let secs = (ms.max(0) + 500) / 1000;
    if secs < 60 {
        return format!("{secs} s");
    }
    let mins = (secs + 30) / 60;
    match (mins / 60, mins % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// "2:05–2:41 pm · 36 min".
pub fn time_line(e: &MediaEntry) -> String {
    format!(
        "{} · {}",
        crate::timefmt::range(e.started_at, e.ended_at, &Local),
        duration_label(e.duration_ms)
    )
}

/// One line of text cut to `width` with an ellipsis.
fn one_line(
    ui: &egui::Ui,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    width: f32,
) -> Arc<egui::Galley> {
    let mut job = LayoutJob::single_section(
        text.to_owned(),
        TextFormat {
            font_id: font,
            color,
            ..Default::default()
        },
    );
    job.wrap = TextWrapping {
        max_width: width.max(10.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

/// The view, in `ui`'s rect. `previews`: the thumbnails pref.
pub fn media_view(
    ui: &mut egui::Ui,
    pages: &MediaPages,
    focus: &mut TranscriptFocus,
    thumbs: &mut Thumbs,
    previews: bool,
) -> Vec<MediaAction> {
    let t = Tokens::get(ui.ctx());
    let mut actions = Vec::new();
    if pages.entries.is_empty() && !pages.has_more {
        let rect = ui.available_rect_before_wrap();
        ui.put(
            rect,
            egui::Label::new(
                egui::RichText::new("Nothing watched yet.")
                    .font(t.font(12.5))
                    .color(t.text_tertiary),
            ),
        );
        return actions;
    }
    let this_year = Local::now().year();
    let jumping = focus.pending.is_some();
    egui::ScrollArea::vertical()
        .id_salt("media-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let view = ui.clip_rect();
            ui.add_space(6.0);
            let mut last_day = None;
            for e in &pages.entries {
                let day = day_of(e.started_at, &Local);
                if last_day != Some(day) {
                    if last_day.is_some() {
                        ui.add_space(10.0);
                    }
                    last_day = Some(day);
                    let heading = day
                        .map(|d| day_heading(d, this_year))
                        .unwrap_or_else(|| "Unknown date".to_owned());
                    ui.horizontal(|ui| {
                        ui.add_space(MARGIN);
                        ui.label(
                            egui::RichText::new(heading)
                                .font(crate::theme::font_medium(ui.ctx(), 13.5))
                                .color(t.text),
                        );
                    });
                    ui.add_space(4.0);
                }
                let rect = media_row(ui, &t, e, thumbs, previews, &mut actions);
                if let Some(id) = focus
                    .pending
                    .filter(|p| e.ids.contains(p))
                    .or_else(|| focus.flash.map(|f| f.0).filter(|p| e.ids.contains(p)))
                {
                    focus_row(ui, &t, focus, id, rect);
                }
            }
            if pages.has_more {
                ui.add_space(8.0);
                let r = ui
                    .horizontal(|ui| {
                        ui.add_space(MARGIN);
                        ui.label(
                            egui::RichText::new("Loading older…")
                                .font(t.font(11.5))
                                .color(t.text_tertiary),
                        );
                    })
                    .response
                    .rect;
                if !jumping && near_viewport(r, view) {
                    actions.push(MediaAction::LoadOlder);
                }
            }
            ui.add_space(10.0);
        });
    actions
}

/// One play: thumbnail, title, source, time · duration, link. Returns its rect.
fn media_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    e: &MediaEntry,
    thumbs: &mut Thumbs,
    previews: bool,
    actions: &mut Vec<MediaAction>,
) -> Rect {
    let p = &e.play;
    let title = entry_title(p);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &title));
    let url = p.url.clone();
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);
        let row = rect.shrink2(vec2(4.0, 1.0));
        if resp.hovered() || resp.context_menu_opened() {
            painter.rect_filled(row, 6.0, t.hover.gamma_multiply(0.6));
        }
        // the thumbnail slot
        let slot = Rect::from_min_size(
            pos2(
                rect.left() + MARGIN,
                rect.top() + (ROW_H - THUMB_SIZE.y) / 2.0,
            ),
            THUMB_SIZE,
        );
        let video = url.as_deref().and_then(oats_media::youtube_video_id);
        let tex = video
            .as_deref()
            .and_then(|id| thumbs.get(ui.ctx(), id, previews));
        match tex {
            Some(tex) => {
                egui::Image::new((tex.id(), THUMB_SIZE))
                    .corner_radius(4.0)
                    .paint_at(ui, slot);
            }
            None => {
                painter.rect(
                    slot,
                    4.0,
                    t.field,
                    egui::Stroke::new(1.0, t.button_border),
                    egui::StrokeKind::Inside,
                );
                painter.text(
                    slot.center(),
                    Align2::CENTER_CENTER,
                    "▶",
                    t.font(16.0),
                    t.text_tertiary,
                );
            }
        }
    }
    // the text column: title, source, time · duration, and the link (its own click target)
    let x = rect.left() + MARGIN + THUMB_SIZE.x + 12.0;
    let width = rect.right() - MARGIN - x;
    let mut y = rect.top() + 5.0;
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);
        for (text, font, color, gap) in [
            (
                title.as_str(),
                crate::theme::font_medium(ui.ctx(), 13.5),
                t.text,
                2.0,
            ),
            (&source_line(p), t.font(11.5), t.text_secondary, 0.0),
            (&time_line(e), t.font(11.5), t.text_tertiary, 1.0),
        ] {
            let g = one_line(ui, text, font, color, width);
            let h = g.size().y;
            painter.galley(pos2(x, y), g, color);
            y += h + gap;
        }
    }
    if let Some(u) = &url {
        let galley = one_line(ui, u, t.font(11.0), t.accent, width);
        let at = pos2(x, y);
        let link_rect = Rect::from_min_size(at, galley.size());
        let link = ui
            .interact(
                link_rect,
                ui.id().with(("media-link", p.id)),
                Sense::click(),
            )
            .on_hover_text(u.as_str());
        link.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Link, true, u.as_str()));
        if ui.is_rect_visible(link_rect) {
            ui.painter().galley(at, galley, t.accent);
            if link.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                ui.painter().hline(
                    link_rect.x_range(),
                    link_rect.bottom() - 1.0,
                    egui::Stroke::new(1.0, t.accent_alpha(0.7)),
                );
            }
        }
        if link.clicked() {
            actions.push(MediaAction::OpenUrl(u.clone()));
        }
    }
    resp.context_menu(|ui| {
        if let Some(u) = &url {
            if ui.button("Open in browser").clicked() {
                actions.push(MediaAction::OpenUrl(u.clone()));
                ui.close();
            }
            if ui.button("Copy link").clicked() {
                actions.push(MediaAction::CopyLink(u.clone()));
                ui.close();
            }
            ui.separator();
        }
        if ui
            .button("Mark as a call")
            .on_hover_text("This was a conversation, not media")
            .clicked()
        {
            actions.push(MediaAction::MarkAsCall(e.ids.clone()));
            ui.close();
        }
        if e.segments_count > 0 && ui.button("Show transcript").clicked() {
            actions.push(MediaAction::ShowTranscript(e.ids.clone()));
            ui.close();
        }
        ui.separator();
        if ui.button("Delete").clicked() {
            actions.push(MediaAction::Delete(e.ids.clone()));
            ui.close();
        }
    });
    rect
}

#[cfg(test)]
mod tests {
    use super::*;

    fn play(kind: MediaClass, url: Option<&str>, title: &str, producer: &str) -> MediaPlay {
        MediaPlay {
            id: 1,
            started_at: 0,
            ended_at: 60_000,
            kind,
            url: url.map(str::to_owned),
            title: title.to_owned(),
            producer: producer.to_owned(),
            segments_count: 0,
        }
    }

    #[test]
    fn rows_read_service_and_app() {
        let yt = play(
            MediaClass::YouTube,
            Some("https://youtu.be/a"),
            "A",
            "Firefox",
        );
        assert_eq!(source_line(&yt), "YouTube · Firefox");
        let app = play(MediaClass::Music, None, "", "Spotify");
        assert_eq!(source_line(&app), "Spotify");
        let web = play(
            MediaClass::Music,
            Some("https://open.spotify.com/track/1"),
            "",
            "Firefox",
        );
        assert_eq!(source_line(&web), "Spotify · Firefox");
        let discord = play(MediaClass::App, None, "", "Discord");
        assert_eq!(source_line(&discord), "Discord");
        assert_eq!(entry_title(&web), "open.spotify.com/track/1");
        assert_eq!(entry_title(&discord), "Discord");
    }

    #[test]
    fn durations() {
        assert_eq!(duration_label(45_000), "45 s");
        assert_eq!(duration_label(36 * 60_000), "36 min");
        assert_eq!(duration_label(65 * 60_000), "1 h 5 min");
        assert_eq!(duration_label(120 * 60_000), "2 h");
    }
}
