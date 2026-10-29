//! A lenient parser for the dates people type: `2026-11-04 09:00`, `11/4`, `4 nov`,
//! `tomorrow 9am`, `in 3 days`, `next friday at 14:00`. Pure and generic over the time zone.
//! Text it can't read is `None` — it never guesses. A date without a time is 09:00 local; a
//! local time that falls in a DST gap resolves forward.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};

/// The hour a date-only answer gets.
const DEFAULT_HOUR: u32 = 9;

/// Parse `text` relative to `now` (which fixes the zone and what "today" is).
pub fn parse<Tz: TimeZone>(text: &str, now: &DateTime<Tz>) -> Option<DateTime<Tz>> {
    let lower = text.to_lowercase().replace(',', " ");
    let mut toks: Vec<String> = lower
        .split_whitespace()
        .map(|t| t.trim_end_matches('.').to_owned())
        .filter(|t| !t.is_empty())
        .collect();
    if toks.is_empty() {
        return None;
    }
    if toks[0] == "in" {
        return relative(&toks, now);
    }
    // "9 am" -> "9am"
    let mut i = 0;
    while i + 1 < toks.len() {
        if (toks[i + 1] == "am" || toks[i + 1] == "pm")
            && toks[i].chars().all(|c| c.is_ascii_digit() || c == ':')
        {
            let next = toks.remove(i + 1);
            toks[i].push_str(&next);
        }
        i += 1;
    }
    // the time: the token after "at", else a token that is unmistakably a time
    let mut time = None;
    if let Some(at) = toks.iter().position(|t| t == "at") {
        let t = parse_time(toks.get(at + 1)?, true)?;
        toks.drain(at..=at + 1);
        time = Some(t);
    } else if let Some(p) = toks.iter().rposition(|t| parse_time(t, false).is_some()) {
        time = parse_time(&toks[p], false);
        toks.remove(p);
    }
    let today = now.date_naive();
    let date = if toks.is_empty() {
        time?;
        today
    } else {
        parse_date(&toks, today)?
    };
    let time = time.unwrap_or(NaiveTime::from_hms_opt(DEFAULT_HOUR, 0, 0)?);
    resolve(&now.timezone(), date.and_time(time))
}

fn relative<Tz: TimeZone>(toks: &[String], now: &DateTime<Tz>) -> Option<DateTime<Tz>> {
    // in <n> <unit> [at <time>]
    let (n, unit) = match toks {
        [_, n, unit, ..] => (n.parse::<i64>().ok()?, unit.as_str()),
        [_, nu] => {
            let split = nu.find(|c: char| !c.is_ascii_digit())?;
            (nu[..split].parse::<i64>().ok()?, &nu[split..])
        }
        _ => return None,
    };
    let rest: &[String] = if toks.len() >= 3 { &toks[3..] } else { &[] };
    if !(1..=100_000).contains(&n) {
        return None;
    }
    let unit = unit.trim_end_matches('s');
    let exact = match unit {
        "min" | "minute" | "m" => Some(Duration::try_minutes(n)?),
        "hour" | "hr" | "h" => Some(Duration::try_hours(n)?),
        _ => None,
    };
    if let Some(d) = exact {
        if !rest.is_empty() {
            return None;
        }
        return now.clone().checked_add_signed(d);
    }
    let days = match unit {
        "day" | "d" => n,
        "week" | "w" => n.checked_mul(7)?,
        _ => return None,
    };
    let date = now
        .date_naive()
        .checked_add_signed(Duration::try_days(days)?)?;
    let time = match rest {
        [] => NaiveTime::from_hms_opt(DEFAULT_HOUR, 0, 0)?,
        [at, t] if at == "at" => parse_time(t, true)?,
        [t] => parse_time(t, false)?,
        _ => return None,
    };
    resolve(&now.timezone(), date.and_time(time))
}

/// A wall-clock time to an instant; ambiguous (clocks back) takes the earlier, a gap
/// (clocks forward) moves on to the first valid time.
fn resolve<Tz: TimeZone>(tz: &Tz, local: NaiveDateTime) -> Option<DateTime<Tz>> {
    for step in 0..=6 {
        let candidate = local.checked_add_signed(Duration::try_minutes(30 * step)?)?;
        if let Some(d) = tz.from_local_datetime(&candidate).earliest() {
            return Some(d);
        }
    }
    None
}

/// `noon`, `9am`, `9:30pm`, `14:00`; a bare hour (`9`) only when `loose` (after "at").
fn parse_time(t: &str, loose: bool) -> Option<NaiveTime> {
    if t == "noon" {
        return NaiveTime::from_hms_opt(12, 0, 0);
    }
    if t == "midnight" {
        return NaiveTime::from_hms_opt(0, 0, 0);
    }
    let (body, ampm) = if let Some(b) = t.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = t.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (t, None)
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => {
            if m.len() != 2 {
                return None;
            }
            (h, m)
        }
        None => {
            if ampm.is_none() && !loose {
                return None;
            }
            (body, "0")
        }
    };
    if h.is_empty() || h.len() > 2 || !h.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut hour: u32 = h.parse().ok()?;
    let min: u32 = m.parse().ok()?;
    if let Some(pm) = ampm {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour %= 12;
        if pm {
            hour += 12;
        }
    }
    NaiveTime::from_hms_opt(hour, min, 0)
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

fn month_of(t: &str) -> Option<u32> {
    if t.len() < 3 {
        return None;
    }
    let i = MONTHS.iter().position(|m| t.starts_with(m))?;
    // the full name or a plain abbreviation ("sept" too), nothing else
    let full = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ][i];
    (full.starts_with(t) || (i == 8 && t == "sept")).then_some(i as u32 + 1)
}

fn weekday_of(t: &str) -> Option<u32> {
    if t.len() < 3 {
        return None;
    }
    let i = WEEKDAYS.iter().position(|d| t.starts_with(d))?;
    let full = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ][i];
    (full.starts_with(t) || matches!(t, "tues" | "thur" | "thurs")).then_some(i as u32)
}

fn day_number(t: &str) -> Option<u32> {
    let t = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|s| t.strip_suffix(s))
        .unwrap_or(t);
    if t.is_empty() || t.len() > 2 || !t.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    t.parse().ok()
}

fn year_number(t: &str) -> Option<i32> {
    if t.len() == 4 && t.chars().all(|c| c.is_ascii_digit()) {
        t.parse().ok()
    } else {
        None
    }
}

/// A date without a year: this year's, or next year's when it has already passed.
fn yearless(month: u32, day: u32, today: NaiveDate) -> Option<NaiveDate> {
    let this = NaiveDate::from_ymd_opt(today.year(), month, day);
    match this {
        Some(d) if d >= today => Some(d),
        _ => NaiveDate::from_ymd_opt(today.year() + 1, month, day),
    }
}

fn parse_date(toks: &[String], today: NaiveDate) -> Option<NaiveDate> {
    let t: Vec<&str> = toks.iter().map(String::as_str).collect();
    match t.as_slice() {
        ["today"] => return Some(today),
        ["tomorrow" | "tmrw"] => return today.checked_add_signed(Duration::try_days(1)?),
        [d] | ["next" | "this" | "on", d] => {
            if let Some(wd) = weekday_of(d) {
                let from = today.weekday().num_days_from_monday();
                let mut ahead = (wd + 7 - from) % 7;
                if ahead == 0 {
                    ahead = 7;
                }
                return today.checked_add_signed(Duration::try_days(i64::from(ahead))?);
            }
        }
        _ => {}
    }
    let t: Vec<&str> = if t.first() == Some(&"on") {
        t[1..].to_vec()
    } else {
        t
    };
    match t.as_slice() {
        [one] => {
            if let Some((y, rest)) = one.split_once('-') {
                let (m, d) = rest.split_once('-')?;
                if y.len() != 4 {
                    return None;
                }
                return NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
            }
            let parts: Vec<&str> = one.split('/').collect();
            match parts.as_slice() {
                [m, d] => yearless(m.parse().ok()?, d.parse().ok()?, today),
                [m, d, y] => {
                    let y: i32 = y.parse().ok()?;
                    let y = if y < 100 { 2000 + y } else { y };
                    NaiveDate::from_ymd_opt(y, m.parse().ok()?, d.parse().ok()?)
                }
                _ => None,
            }
        }
        [a, b] => {
            let (m, d) = match (month_of(a), month_of(b)) {
                (Some(m), None) => (m, day_number(b)?),
                (None, Some(m)) => (m, day_number(a)?),
                _ => return None,
            };
            yearless(m, d, today)
        }
        [a, b, c] => {
            let y = year_number(c)?;
            let (m, d) = match (month_of(a), month_of(b)) {
                (Some(m), None) => (m, day_number(b)?),
                (None, Some(m)) => (m, day_number(a)?),
                _ => return None,
            };
            NaiveDate::from_ymd_opt(y, m, d)
        }
        _ => None,
    }
}

/// Split a trailing ` @ <date>` off a todo's text: `("call landlord", Some("tomorrow 9am"))`.
pub fn split_at_suffix(text: &str) -> (&str, Option<&str>) {
    match text.rsplit_once(" @ ") {
        Some((head, tail)) if !head.trim().is_empty() => (head.trim(), Some(tail.trim())),
        _ => (text.trim(), None),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Local, Timelike};

    fn now() -> DateTime<FixedOffset> {
        // Thursday 2026-10-29 14:00
        FixedOffset::east_opt(3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 29, 14, 0, 0)
            .single()
            .unwrap()
    }

    fn p(s: &str) -> Option<String> {
        parse(s, &now()).map(|d| d.format("%Y-%m-%d %H:%M").to_string())
    }

    #[test]
    fn absolute_dates() {
        assert_eq!(p("2026-11-04").as_deref(), Some("2026-11-04 09:00"));
        assert_eq!(p("2026-11-04 09:30").as_deref(), Some("2026-11-04 09:30"));
        assert_eq!(p("11/4").as_deref(), Some("2026-11-04 09:00"));
        assert_eq!(p("11/4/2027").as_deref(), Some("2027-11-04 09:00"));
        assert_eq!(p("4 nov").as_deref(), Some("2026-11-04 09:00"));
        assert_eq!(p("Nov 4th").as_deref(), Some("2026-11-04 09:00"));
        assert_eq!(p("november 4 2027").as_deref(), Some("2027-11-04 09:00"));
        // already passed this year: next year; today itself stays
        assert_eq!(p("3 oct").as_deref(), Some("2027-10-03 09:00"));
        assert_eq!(p("29 oct").as_deref(), Some("2026-10-29 09:00"));
    }

    #[test]
    fn words_and_times() {
        assert_eq!(p("today").as_deref(), Some("2026-10-29 09:00"));
        assert_eq!(p("tomorrow 9am").as_deref(), Some("2026-10-30 09:00"));
        assert_eq!(p("tomorrow at 9").as_deref(), Some("2026-10-30 09:00"));
        assert_eq!(p("tomorrow at 9:30").as_deref(), Some("2026-10-30 09:30"));
        assert_eq!(p("tomorrow 9 pm").as_deref(), Some("2026-10-30 21:00"));
        assert_eq!(p("tomorrow noon").as_deref(), Some("2026-10-30 12:00"));
        assert_eq!(p("nov 4 at 14:00").as_deref(), Some("2026-11-04 14:00"));
        assert_eq!(p("14:00").as_deref(), Some("2026-10-29 14:00"));
        assert_eq!(p("12am tomorrow").as_deref(), Some("2026-10-30 00:00"));
    }

    #[test]
    fn weekdays_take_the_next_occurrence() {
        assert_eq!(p("friday").as_deref(), Some("2026-10-30 09:00"));
        assert_eq!(p("next friday at 3pm").as_deref(), Some("2026-10-30 15:00"));
        assert_eq!(p("thursday").as_deref(), Some("2026-11-05 09:00"));
        assert_eq!(p("mon 10am").as_deref(), Some("2026-11-02 10:00"));
    }

    #[test]
    fn relative_times() {
        assert_eq!(p("in 2 hours").as_deref(), Some("2026-10-29 16:00"));
        assert_eq!(p("in 30 min").as_deref(), Some("2026-10-29 14:30"));
        assert_eq!(p("in 3 days").as_deref(), Some("2026-11-01 09:00"));
        assert_eq!(p("in 1 week").as_deref(), Some("2026-11-05 09:00"));
        assert_eq!(p("in 2 days at 5pm").as_deref(), Some("2026-10-31 17:00"));
        assert_eq!(p("in 3d").as_deref(), Some("2026-11-01 09:00"));
    }

    #[test]
    fn unreadable_text_is_none() {
        for s in [
            "",
            "   ",
            "soon",
            "someday",
            "13/45",
            "2/30",
            "2026-13-01",
            "25:00",
            "tomorrow at 25",
            "in days",
            "in 0 days",
            "in 2 fortnights",
            "nov",
            "4",
            "tomorrow banana",
            "9am pm",
            "nov 4 5 6",
            "13pm",
        ] {
            assert_eq!(p(s), None, "{s:?}");
        }
    }

    #[test]
    fn dst_gaps_resolve_forward_and_overlaps_take_the_first() {
        // America/New_York isn't available without chrono-tz; use a fixed-rule zone.
        #[derive(Clone, Copy, Debug)]
        struct Gap;
        #[derive(Clone, Copy, Debug)]
        struct GapOffset(i32);
        impl chrono::Offset for GapOffset {
            fn fix(&self) -> FixedOffset {
                FixedOffset::east_opt(self.0).unwrap()
            }
        }
        impl std::fmt::Display for GapOffset {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        // local 02:00..03:00 on 2026-11-05 does not exist (+0 before 02:00 local, +1h after)
        impl TimeZone for Gap {
            type Offset = GapOffset;
            fn from_offset(_: &GapOffset) -> Self {
                Gap
            }
            fn offset_from_local_date(&self, _: &NaiveDate) -> chrono::LocalResult<GapOffset> {
                chrono::LocalResult::Single(GapOffset(0))
            }
            fn offset_from_local_datetime(
                &self,
                l: &NaiveDateTime,
            ) -> chrono::LocalResult<GapOffset> {
                let gap = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
                if l.date() == gap && l.hour() == 2 {
                    chrono::LocalResult::None
                } else if l.date() == gap && l.hour() < 2 || l.date() < gap {
                    chrono::LocalResult::Single(GapOffset(0))
                } else {
                    chrono::LocalResult::Single(GapOffset(3600))
                }
            }
            fn offset_from_utc_date(&self, _: &NaiveDate) -> GapOffset {
                GapOffset(0)
            }
            fn offset_from_utc_datetime(&self, _: &NaiveDateTime) -> GapOffset {
                GapOffset(0)
            }
        }
        let now = Gap
            .with_ymd_and_hms(2026, 10, 29, 14, 0, 0)
            .single()
            .unwrap();
        let d = parse("nov 5 at 2:30", &now).unwrap();
        assert_eq!(d.naive_local().format("%H:%M").to_string(), "03:00");
        let d = parse("nov 5 at 3:30", &now).unwrap();
        assert_eq!(d.naive_local().format("%H:%M").to_string(), "03:30");
    }

    #[test]
    fn works_in_the_local_zone_and_splits_the_suffix() {
        let n = Local::now();
        let d = parse("tomorrow 9am", &n).unwrap();
        assert_eq!(d.hour(), 9);
        assert_eq!(
            split_at_suffix("call landlord @ tomorrow 9am"),
            ("call landlord", Some("tomorrow 9am"))
        );
        assert_eq!(split_at_suffix("email a@b.com"), ("email a@b.com", None));
        assert_eq!(split_at_suffix("@ tomorrow"), ("@ tomorrow", None));
    }
}
