//! Date helpers shared by interpretation and hotword commands. The model resolves phrases like
//! "next Thursday" itself (it is told the local date and time); what comes back is a calendar
//! date (`YYYY-MM-DD`), an optional clock time (`HH:MM`) and, for reminders, an offset in days
//! relative to the event. These turn those into Unix milliseconds in the local time zone.

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, TimeZone, Timelike};

/// `YYYY-MM-DD` (also accepts `MM/DD/YYYY` and `DD.MM.YYYY`, the forms ASR tends to produce).
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(s, "%m/%d/%Y"))
        .or_else(|_| NaiveDate::parse_from_str(s, "%d.%m.%Y"))
        .ok()
}

/// `HH:MM` (24 h) or `H:MM am/pm`.
pub fn parse_time(s: &str) -> Option<NaiveTime> {
    let mut s = s.trim().to_lowercase().replace(' ', "");
    if !s.contains(':') && (s.ends_with("am") || s.ends_with("pm")) {
        // "9am" → "9:00am"
        s.insert_str(s.len() - 2, ":00");
    }
    NaiveTime::parse_from_str(&s, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(&s, "%I:%M%p"))
        .ok()
}

/// A date (+ optional time) in `tz` → Unix ms. Without a time: local midnight (an all-day
/// event). A wall time that doesn't exist locally (skipped by a DST jump, e.g. a midnight in a
/// zone that springs forward at 00:00) resolves to the first valid instant after the gap.
pub fn to_ms<Tz: TimeZone>(date: NaiveDate, time: Option<NaiveTime>, tz: &Tz) -> Option<i64> {
    let t = time.unwrap_or(NaiveTime::MIN);
    let naive = date.and_time(t);
    // DST gaps are at most a couple of hours: step forward a quarter hour at a time
    (0..=12).find_map(|i| {
        tz.from_local_datetime(&(naive + chrono::Duration::minutes(15 * i)))
            .earliest()
            .map(|d| d.timestamp_millis())
    })
}

/// The largest day offset any model-supplied date math accepts (a year either way); larger
/// values are clamped, never passed to chrono's panicking constructors.
pub const MAX_OFFSET_DAYS: i64 = 366;

/// The local calendar date `days` days from `at` (negative = earlier), at the same clock time.
/// `days` is clamped to ±[`MAX_OFFSET_DAYS`]. A clock time that doesn't exist on the target
/// day (a DST gap) resolves forward to the first valid instant, as [`to_ms`] does.
pub fn shift_days<Tz: TimeZone>(at: &DateTime<Tz>, days: i64) -> Option<DateTime<Tz>> {
    let days = days.clamp(-MAX_OFFSET_DAYS, MAX_OFFSET_DAYS);
    let date = at
        .date_naive()
        .checked_add_signed(chrono::TimeDelta::try_days(days)?)?;
    let time = NaiveTime::from_hms_opt(at.hour(), at.minute(), at.second())?;
    let tz = at.timezone();
    let ms = to_ms(date, Some(time), &tz)?;
    tz.timestamp_millis_opt(ms).single()
}

/// The clock time a todo / reminder given only a date is due at (local): the morning of that
/// day, so "property taxes are due 12/12" pops up when the day starts, not at midnight.
pub fn default_due_time() -> NaiveTime {
    NaiveTime::from_hms_opt(9, 0, 0).unwrap_or(NaiveTime::MIN)
}

/// A due time from a date and an optional clock time: without a time, [`default_due_time`].
pub fn due_ms<Tz: TimeZone>(date: NaiveDate, time: Option<NaiveTime>, tz: &Tz) -> Option<i64> {
    to_ms(date, Some(time.unwrap_or_else(default_due_time)), tz)
}

/// The largest "in N …" a command may ask for, in minutes (a year).
pub const MAX_IN_MINUTES: i64 = MAX_OFFSET_DAYS * 24 * 60;

/// `minutes` after `at` ("remind me in 2 hours / 2 days / 3 weeks", resolved against the time
/// the command was SPOKEN). Whole days keep the clock time across a DST change (2 days from
/// 14:05 is 14:05); anything else is plain elapsed time. Clamped to [0, [`MAX_IN_MINUTES`]].
pub fn after_minutes<Tz: TimeZone>(at: &DateTime<Tz>, minutes: i64) -> Option<i64> {
    let minutes = minutes.clamp(0, MAX_IN_MINUTES);
    if minutes > 0 && minutes % (24 * 60) == 0 {
        return shift_days(at, minutes / (24 * 60)).map(|d| d.timestamp_millis());
    }
    Some(at.timestamp_millis() + minutes * 60_000)
}

/// "in 2 days", "in an hour", "in three weeks", "in 90 minutes" → minutes; `None` for
/// anything else. The model normally fills `in_minutes` itself; this is the fallback when it
/// put the phrase in the date field instead.
pub fn parse_in_phrase(s: &str) -> Option<i64> {
    let lower = s.trim().to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| c.is_whitespace() || c == ',' || c == '.')
        .filter(|w| !w.is_empty())
        .collect();
    let rest = match words.as_slice() {
        ["in", rest @ ..] => rest,
        rest => rest,
    };
    let [n, unit] = rest else { return None };
    let n: i64 = match *n {
        "a" | "an" | "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        n => n.parse().ok().filter(|n: &i64| (0..=100_000).contains(n))?,
    };
    let per = match unit.trim_end_matches('s') {
        "minute" | "min" => 1,
        "hour" | "hr" => 60,
        "day" => 24 * 60,
        "week" | "wk" => 7 * 24 * 60,
        _ => return None,
    };
    Some((n * per).min(MAX_IN_MINUTES))
}

/// "2026-10-28 (Wednesday) 14:05, Europe/Oslo"-style context line for the prompts.
pub fn describe_now(now: &DateTime<Local>) -> String {
    format!(
        "{} ({}) {} local time (UTC{})",
        now.format("%Y-%m-%d"),
        now.weekday(),
        now.format("%H:%M"),
        now.format("%:z")
    )
}

/// Local midnight of the day containing `ms`, in Unix ms.
pub fn local_midnight_ms(ms: i64) -> i64 {
    let Some(d) = Local.timestamp_millis_opt(ms).single() else {
        return ms;
    };
    to_ms(d.date_naive(), None, &Local).unwrap_or(ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    #[test]
    fn parses_dates_and_times() {
        assert_eq!(
            parse_date("2026-12-12"),
            NaiveDate::from_ymd_opt(2026, 12, 12)
        );
        assert_eq!(
            parse_date("12/12/2026"),
            NaiveDate::from_ymd_opt(2026, 12, 12)
        );
        assert_eq!(parse_date("5.3.2027"), NaiveDate::from_ymd_opt(2027, 3, 5));
        assert_eq!(parse_date("tomorrow"), None);
        assert_eq!(parse_time("14:05"), NaiveTime::from_hms_opt(14, 5, 0));
        assert_eq!(parse_time("2:05 PM"), NaiveTime::from_hms_opt(14, 5, 0));
        assert_eq!(parse_time("9 am"), NaiveTime::from_hms_opt(9, 0, 0));
        assert_eq!(parse_time(""), None);
    }

    /// A zone whose clocks jump from 00:00 straight to 01:00 on 2026-03-01 (as e.g.
    /// America/Santiago and Asia/Beirut do at local midnight).
    #[derive(Clone, Copy, Debug)]
    struct MidnightDst;

    impl TimeZone for MidnightDst {
        type Offset = FixedOffset;
        fn from_offset(_: &FixedOffset) -> Self {
            MidnightDst
        }
        fn offset_from_local_date(&self, _: &NaiveDate) -> chrono::LocalResult<FixedOffset> {
            chrono::LocalResult::None
        }
        fn offset_from_local_datetime(
            &self,
            local: &chrono::NaiveDateTime,
        ) -> chrono::LocalResult<FixedOffset> {
            let switch = NaiveDate::from_ymd_opt(2026, 3, 1).and_then(|d| d.and_hms_opt(0, 0, 0));
            let Some(switch) = switch else {
                return chrono::LocalResult::None;
            };
            let gap_end = switch + chrono::Duration::hours(1);
            match FixedOffset::east_opt(if *local < switch { 0 } else { 3600 }) {
                Some(_) if *local >= switch && *local < gap_end => chrono::LocalResult::None,
                Some(o) => chrono::LocalResult::Single(o),
                None => chrono::LocalResult::None,
            }
        }
        fn offset_from_utc_date(&self, _: &NaiveDate) -> FixedOffset {
            FixedOffset::east_opt(0).unwrap()
        }
        fn offset_from_utc_datetime(&self, utc: &chrono::NaiveDateTime) -> FixedOffset {
            let switch = NaiveDate::from_ymd_opt(2026, 3, 1)
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .unwrap();
            FixedOffset::east_opt(if *utc < switch { 0 } else { 3600 }).unwrap()
        }
    }

    #[test]
    fn a_skipped_midnight_resolves_to_the_end_of_the_gap() {
        let d = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        // 01:00 local (+01:00) = 00:00Z, the first instant of the day
        let ms = to_ms(d, None, &MidnightDst).unwrap();
        assert_eq!(
            ms,
            d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis()
        );
    }

    #[test]
    fn resolves_in_zone_and_shifts_days() {
        let tz = FixedOffset::east_opt(3600).unwrap();
        let d = NaiveDate::from_ymd_opt(2026, 12, 12).unwrap();
        let midnight = to_ms(d, None, &tz).unwrap();
        // 2026-12-12T00:00+01:00 = 2026-12-11T23:00Z
        assert_eq!(midnight, 1_797_030_000_000);
        let at = to_ms(d, NaiveTime::from_hms_opt(9, 30, 0), &tz).unwrap();
        assert_eq!(at - midnight, (9 * 3600 + 30 * 60) * 1000);
        let dt = tz.timestamp_millis_opt(at).single().unwrap();
        let week_before = shift_days(&dt, -7).unwrap();
        assert_eq!(
            week_before.date_naive(),
            NaiveDate::from_ymd_opt(2026, 12, 5).unwrap()
        );
        assert_eq!(week_before.hour(), 9);
    }

    #[test]
    fn huge_offsets_are_clamped_not_a_panic() {
        let tz = FixedOffset::east_opt(0).unwrap();
        let at = tz.with_ymd_and_hms(2026, 10, 28, 9, 0, 0).single().unwrap();
        let far = shift_days(&at, i64::MAX).unwrap();
        assert_eq!(
            far.date_naive(),
            NaiveDate::from_ymd_opt(2027, 10, 29).unwrap()
        );
        let back = shift_days(&at, i64::MIN).unwrap();
        assert_eq!(
            back.date_naive(),
            NaiveDate::from_ymd_opt(2025, 10, 27).unwrap()
        );
    }

    #[test]
    fn a_shift_into_a_dst_gap_moves_forward() {
        // 2026-03-08 00:30 doesn't exist in MidnightDst on 03-01; shift 7 days back from 03-08
        let d = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap();
        let ms = to_ms(d, NaiveTime::from_hms_opt(0, 30, 0), &MidnightDst).unwrap();
        let at = MidnightDst.timestamp_millis_opt(ms).single().unwrap();
        let back = shift_days(&at, -7).expect("a gap time still resolves");
        assert_eq!(
            back.date_naive(),
            NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()
        );
        assert_eq!((back.hour(), back.minute()), (1, 0), "the end of the gap");
    }

    #[test]
    fn dates_without_a_time_are_due_in_the_morning() {
        let tz = FixedOffset::east_opt(3600).unwrap();
        let d = NaiveDate::from_ymd_opt(2026, 12, 12).unwrap();
        let due = due_ms(d, None, &tz).unwrap();
        let at = tz.timestamp_millis_opt(due).single().unwrap();
        assert_eq!((at.date_naive(), at.hour(), at.minute()), (d, 9, 0));
        let timed = due_ms(d, NaiveTime::from_hms_opt(18, 30, 0), &tz).unwrap();
        assert_eq!(tz.timestamp_millis_opt(timed).single().unwrap().hour(), 18);
    }

    #[test]
    fn in_phrases_and_minutes_from_the_spoken_time() {
        assert_eq!(parse_in_phrase("in 2 days"), Some(2 * 1440));
        assert_eq!(parse_in_phrase("In an hour"), Some(60));
        assert_eq!(parse_in_phrase("in three weeks"), Some(3 * 7 * 1440));
        assert_eq!(parse_in_phrase("90 minutes"), Some(90));
        assert_eq!(parse_in_phrase("in 1 hr."), Some(60));
        assert_eq!(parse_in_phrase("next tuesday"), None);
        assert_eq!(parse_in_phrase("in 2 fortnights"), None);
        assert_eq!(parse_in_phrase("in 99999 weeks"), Some(MAX_IN_MINUTES));
        let tz = FixedOffset::east_opt(0).unwrap();
        let at = tz
            .with_ymd_and_hms(2026, 10, 29, 14, 5, 0)
            .single()
            .unwrap();
        let two_days = after_minutes(&at, 2 * 1440).unwrap();
        let d = tz.timestamp_millis_opt(two_days).single().unwrap();
        assert_eq!(
            (d.date_naive(), d.hour(), d.minute()),
            (NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(), 14, 5)
        );
        assert_eq!(
            after_minutes(&at, 90),
            Some(at.timestamp_millis() + 90 * 60_000)
        );
        assert_eq!(after_minutes(&at, -5), Some(at.timestamp_millis()));
    }
}
