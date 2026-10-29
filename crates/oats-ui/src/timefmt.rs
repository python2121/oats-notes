//! Every clock time the UI shows goes through here: "14:05" (24-hour) or "2:05 pm" (12-hour,
//! the default). Date-only labels do not belong here.
//!
//! The pure helpers take the preference explicitly. Draw code, which has no handle on the
//! prefs, reads the UI thread's current choice (`set_24h`, refreshed by the app every frame)
//! through [`clock`], [`clock_col`], [`range`] and [`hm`].

use std::cell::Cell;

use chrono::{DateTime, TimeZone, Timelike};

thread_local! {
    static CLOCK_24H: Cell<bool> = const { Cell::new(false) };
}

/// Set the UI thread's clock style (the `AppPrefs::clock_24h` pref).
pub fn set_24h(on: bool) {
    CLOCK_24H.with(|c| c.set(on));
}

/// The UI thread's current clock style.
pub fn is_24h() -> bool {
    CLOCK_24H.with(Cell::get)
}

/// Width, in characters, of a padded 12-hour time (" 2:05 pm" / "12:05 pm").
pub const COL_CHARS: usize = 8;

fn parts12(h: u32) -> (u32, &'static str) {
    (
        if h.is_multiple_of(12) { 12 } else { h % 12 },
        if h < 12 { "am" } else { "pm" },
    )
}

/// "14:05" or "2:05 pm".
pub fn fmt_hm(h: u32, m: u32, h24: bool) -> String {
    if h24 {
        format!("{h:02}:{m:02}")
    } else {
        let (h12, ap) = parts12(h);
        format!("{h12}:{m:02} {ap}")
    }
}

/// The time of day of `d`, in the given style.
pub fn clock_of<Tz: TimeZone>(d: &DateTime<Tz>, h24: bool) -> String {
    fmt_hm(d.hour(), d.minute(), h24)
}

/// Pad a 12-hour time to [`COL_CHARS`] (" 2:05 pm", "12:05 pm") so a monospace column lines
/// up; 24-hour times are always 5 wide and pass through.
pub fn pad_col(s: String) -> String {
    format!("{s:>w$}", w = if s.ends_with('m') { COL_CHARS } else { 0 })
}

/// "2:05–2:31 pm", "11:50 am–12:10 pm", "14:05–14:31". The am/pm is not repeated when both
/// ends share it.
pub fn range_hm(a: (u32, u32), b: (u32, u32), h24: bool) -> String {
    if h24 {
        return format!("{}–{}", fmt_hm(a.0, a.1, true), fmt_hm(b.0, b.1, true));
    }
    let (ha, apa) = parts12(a.0);
    let (hb, apb) = parts12(b.0);
    if apa == apb {
        format!("{ha}:{:02}–{hb}:{:02} {apb}", a.1, b.1)
    } else {
        format!("{ha}:{:02} {apa}–{hb}:{:02} {apb}", a.1, b.1)
    }
}

/// Clock time of a unix-ms timestamp in `tz`, "--:--" when unrepresentable.
pub fn clock_with<Tz: TimeZone>(ms: i64, tz: &Tz, h24: bool) -> String {
    tz.timestamp_millis_opt(ms)
        .single()
        .map(|d| clock_of(&d, h24))
        .unwrap_or_else(|| "--:--".to_owned())
}

/// [`clock_with`] in the UI's current style.
pub fn clock<Tz: TimeZone>(ms: i64, tz: &Tz) -> String {
    clock_with(ms, tz, is_24h())
}

/// [`clock`] padded for a monospace time column.
pub fn clock_col<Tz: TimeZone>(ms: i64, tz: &Tz) -> String {
    pad_col(clock(ms, tz))
}

/// A start–end range of two timestamps in the UI's current style.
pub fn range<Tz: TimeZone>(a: i64, b: i64, tz: &Tz) -> String {
    match (
        tz.timestamp_millis_opt(a).single(),
        tz.timestamp_millis_opt(b).single(),
    ) {
        (Some(x), Some(y)) => range_hm((x.hour(), x.minute()), (y.hour(), y.minute()), is_24h()),
        _ => "--:--".to_owned(),
    }
}

/// The time of `d` in the UI's current style.
pub fn hm<Tz: TimeZone>(d: &DateTime<Tz>) -> String {
    clock_of(d, is_24h())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    #[test]
    fn single_times() {
        assert_eq!(fmt_hm(14, 5, true), "14:05");
        assert_eq!(fmt_hm(14, 5, false), "2:05 pm");
        assert_eq!(fmt_hm(0, 0, true), "00:00");
        assert_eq!(fmt_hm(0, 0, false), "12:00 am");
        assert_eq!(fmt_hm(12, 0, false), "12:00 pm");
        assert_eq!(fmt_hm(12, 30, true), "12:30");
        assert_eq!(fmt_hm(23, 59, false), "11:59 pm");
        assert_eq!(fmt_hm(9, 7, false), "9:07 am");
    }

    #[test]
    fn column_padding_aligns() {
        assert_eq!(pad_col(fmt_hm(14, 5, false)), " 2:05 pm");
        assert_eq!(pad_col(fmt_hm(12, 5, false)), "12:05 pm");
        assert_eq!(pad_col(fmt_hm(14, 5, true)), "14:05");
    }

    #[test]
    fn ranges() {
        assert_eq!(range_hm((14, 5), (14, 31), false), "2:05–2:31 pm");
        assert_eq!(range_hm((14, 5), (14, 31), true), "14:05–14:31");
        assert_eq!(range_hm((11, 50), (12, 10), false), "11:50 am–12:10 pm");
        assert_eq!(range_hm((23, 50), (0, 10), false), "11:50 pm–12:10 am");
        assert_eq!(range_hm((0, 5), (0, 40), false), "12:05–12:40 am");
        assert_eq!(range_hm((9, 0), (10, 0), false), "9:00–10:00 am");
    }

    #[test]
    fn from_timestamps_and_the_thread_style() {
        let tz = FixedOffset::east_opt(3600).unwrap();
        let ms = 1_793_106_300_000; // 14:05 at +01:00
        assert_eq!(clock_with(ms, &tz, true), "14:05");
        assert_eq!(clock_with(ms, &tz, false), "2:05 pm");
        set_24h(true);
        assert_eq!(clock(ms, &tz), "14:05");
        assert_eq!(range(ms, ms + 26 * 60_000, &tz), "14:05–14:31");
        set_24h(false);
        assert_eq!(clock_col(ms, &tz), " 2:05 pm");
        assert_eq!(range(ms, ms + 26 * 60_000, &tz), "2:05–2:31 pm");
    }
}
