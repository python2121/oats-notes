//! Spotify's web player (`open.spotify.com`) in a browser tab: the tab's URL is normally the
//! playlist / album page, so the track comes from the title. While a track plays the player
//! sets `document.title` to `"<Track> • <Artist>"` (U+2022 with spaces); paused or idle it reads
//! `"Spotify – Web Player"` / `"Spotify"` (a title without the separator). Pure.
//!
//! Not checked against a live session store yet (no Spotify tab was open when this was
//! written): the parser is lenient — a middle dot `·` is accepted as the separator too, and
//! surrounding whitespace is trimmed.

/// One track as the web player's title names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub track: String,
    pub artist: String,
}

impl Track {
    /// `"<Track> — <Artist>"` (em dash): a play's title.
    pub fn label(&self) -> String {
        format!("{} — {}", self.track, self.artist)
    }
}

/// The separators the player puts between the track and the artist.
const SEPARATORS: [&str; 2] = [" \u{2022} ", " \u{00b7} "];

/// The track a Spotify web-player title names; `None` when it names none (paused / idle:
/// "Spotify – Web Player", "Spotify", an empty title, a part missing).
pub fn parse_title(title: &str) -> Option<Track> {
    let t = title.trim();
    let (track, artist) = SEPARATORS.iter().find_map(|sep| t.split_once(sep))?;
    let (track, artist) = (track.trim(), artist.trim());
    if track.is_empty() || artist.is_empty() {
        return None;
    }
    Some(Track {
        track: track.to_owned(),
        artist: artist.to_owned(),
    })
}

/// A page of Spotify's web player (`open.spotify.com`).
pub fn is_web_player(url: &str) -> bool {
    crate::split_url(url).is_some_and(|(host, _)| host == "open.spotify.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_playing_title_names_the_track_and_artist() {
        let t = parse_title("Never Gonna Give You Up • Rick Astley").unwrap();
        assert_eq!(t.track, "Never Gonna Give You Up");
        assert_eq!(t.artist, "Rick Astley");
        assert_eq!(t.label(), "Never Gonna Give You Up — Rick Astley");
        // several artists, extra whitespace, a middle dot
        let t = parse_title("  Song A  •  X, Y ").unwrap();
        assert_eq!((t.track.as_str(), t.artist.as_str()), ("Song A", "X, Y"));
        let t = parse_title("Song B · Z").unwrap();
        assert_eq!((t.track.as_str(), t.artist.as_str()), ("Song B", "Z"));
        // a hyphenated track is not split on the hyphen
        let t = parse_title("Up - Remastered 2009 • Band").unwrap();
        assert_eq!(t.track, "Up - Remastered 2009");
    }

    #[test]
    fn idle_titles_name_nothing() {
        for idle in [
            "Spotify – Web Player",
            "Spotify - Web Player",
            "Spotify",
            "Spotify Web Player: Music for everyone",
            "",
            " • Artist",
            "Track • ",
            "Track•Artist",
        ] {
            assert_eq!(parse_title(idle), None, "{idle:?}");
        }
    }

    #[test]
    fn the_web_player_host() {
        assert!(is_web_player("https://open.spotify.com/playlist/37i9"));
        assert!(!is_web_player("https://spotify.com/"));
        assert!(!is_web_player("https://music.apple.com/x"));
    }
}
