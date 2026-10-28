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

/// The local calendar date `days` days from `at` (negative = earlier), at the same clock time.
pub fn shift_days<Tz: TimeZone>(at: &DateTime<Tz>, days: i64) -> Option<DateTime<Tz>> {
    let date = at
        .date_naive()
        .checked_add_signed(chrono::Duration::days(days))?;
    let time = NaiveTime::from_hms_opt(at.hour(), at.minute(), at.second())?;
    at.timezone()
        .from_local_datetime(&date.and_time(time))
        .earliest()
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
}
