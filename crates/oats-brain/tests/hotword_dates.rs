//! Hotword detection variants, and spoken commands' dates resolved across month and year
//! boundaries and daylight-saving transitions. The date helpers and `hotword::execute` are
//! generic over the time zone, so the DST cases use a small rule-based zone defined here
//! (a US-Eastern-like zone for 2026, and one that springs forward at midnight like Chile's)
//! instead of depending on the machine's `TZ`.

use chrono::{
    DateTime, Datelike, FixedOffset, MappedLocalTime, NaiveDate, NaiveDateTime, NaiveTime,
    TimeZone, Timelike, Utc,
};
use oats_brain::hotword::{self, CommandResult, detect};
use oats_brain::{Executed, HotwordConfig, dates};
use oats_core::{Store, TranscriptRow};
use serde_json::json;

// ---- a rule-based DST zone ---------------------------------------------------------------------

/// Standard offset, DST offset, and the UTC instants (seconds) DST starts and ends.
#[derive(Clone, Copy, Debug)]
struct Zone {
    std: i32,
    dst: i32,
    start: i64,
    end: i64,
}

const H: i32 = 3600;

/// US Eastern, 2026: EDT from 2026-03-08 02:00 EST (07:00Z) to 2026-11-01 02:00 EDT (06:00Z).
fn eastern() -> Zone {
    Zone {
        std: -5 * H,
        dst: -4 * H,
        start: utc(2026, 3, 8, 7, 0),
        end: utc(2026, 11, 1, 6, 0),
    }
}

/// Springs forward at local midnight (Chile-style): 2026-09-06 00:00 -04 becomes 01:00 -03, so
/// that day has no midnight.
fn midnight_spring() -> Zone {
    Zone {
        std: -4 * H,
        dst: -3 * H,
        start: utc(2026, 9, 6, 4, 0),
        end: utc(2027, 4, 4, 3, 0),
    }
}

fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(h, min, 0)
        .unwrap()
        .and_utc()
        .timestamp()
}

impl Zone {
    fn at_utc(&self, secs: i64) -> FixedOffset {
        let o = if self.start <= secs && secs < self.end {
            self.dst
        } else {
            self.std
        };
        FixedOffset::east_opt(o).unwrap()
    }
}

impl TimeZone for Zone {
    type Offset = FixedOffset;

    fn from_offset(_: &FixedOffset) -> Self {
        // only reached when chrono rebuilds a zone from an offset (e.g. `with_timezone`
        // round trips); the tests never mix zones
        eastern()
    }

    fn offset_from_local_date(&self, local: &NaiveDate) -> MappedLocalTime<FixedOffset> {
        self.offset_from_local_datetime(&local.and_time(NaiveTime::MIN))
    }

    fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> MappedLocalTime<FixedOffset> {
        // larger offset first: it maps to the earlier instant (what `earliest()` returns)
        let mut valid = Vec::new();
        for o in [self.dst, self.std] {
            let secs = local.and_utc().timestamp() - i64::from(o);
            if self.at_utc(secs).local_minus_utc() == o {
                valid.push(FixedOffset::east_opt(o).unwrap());
            }
        }
        match valid.as_slice() {
            [] => MappedLocalTime::None,
            [one] => MappedLocalTime::Single(*one),
            [a, b] => MappedLocalTime::Ambiguous(*a, *b),
            _ => unreachable!(),
        }
    }

    fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
        self.offset_from_utc_datetime(&utc.and_time(NaiveTime::MIN))
    }

    fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
        self.at_utc(utc.and_utc().timestamp())
    }
}

#[test]
fn the_test_zone_behaves_like_eastern() {
    let z = eastern();
    let d = |m, d| NaiveDate::from_ymd_opt(2026, m, d).unwrap();
    let t = |h, m| NaiveTime::from_hms_opt(h, m, 0);
    // a winter and a summer noon
    assert_eq!(
        dates::to_ms(d(1, 15), t(12, 0), &z),
        Some(utc(2026, 1, 15, 17, 0) * 1000)
    );
    assert_eq!(
        dates::to_ms(d(7, 15), t(12, 0), &z),
        Some(utc(2026, 7, 15, 16, 0) * 1000)
    );
    // the gap and the overlap
    assert!(matches!(
        z.from_local_datetime(&d(3, 8).and_time(t(2, 30).unwrap())),
        MappedLocalTime::None
    ));
    assert!(matches!(
        z.from_local_datetime(&d(11, 1).and_time(t(1, 30).unwrap())),
        MappedLocalTime::Ambiguous(..)
    ));
}

// ---- detection ---------------------------------------------------------------------------------

#[test]
fn hotword_variants() {
    let cfg = HotwordConfig::default();
    let d = |s: &str| detect(s, &cfg);
    // ASR punctuation and casing
    assert_eq!(d("Hey, computer. Add milk.").as_deref(), Some("Add milk."));
    assert_eq!(d("HEY OATS!!! buy bread").as_deref(), Some("buy bread"));
    assert_eq!(d("hey oats, remind me").as_deref(), Some("remind me"));
    // mid-sentence; the command keeps its original words, case and dates
    assert_eq!(
        d("and then I said hey computer Call Dr. Smith on 3/4/2027 at 9am").as_deref(),
        Some("Call Dr. Smith on 3/4/2027 at 9am")
    );
    // the hotword alone: detected, nothing to do
    assert_eq!(d("Hey, computer.").as_deref(), Some(""));
    assert_eq!(d("hey oats").as_deref(), Some(""));
    // not hotwords
    assert_eq!(d("hey there computer"), None);
    assert_eq!(d("computer, hey"), None);
    assert_eq!(d("hey computers are great"), None);
    assert_eq!(d("they computer"), None);
    assert_eq!(d("   "), None);
    // the same hotword twice: everything after the first goes to the model, which can return
    // several actions for it
    assert_eq!(
        d("hey computer add milk, hey computer call mum").as_deref(),
        Some("add milk, hey computer call mum")
    );
    // custom phrases are matched on whole words too
    let custom = HotwordConfig {
        phrases: vec!["okay notes".into(), "hey computer".into()],
    };
    assert_eq!(
        detect("Okay, notes: lunch", &custom).as_deref(),
        Some("lunch")
    );
    assert_eq!(detect("okay notebook lunch", &custom), None);
}

#[test]
#[ignore = "bug: with two different hotwords in one utterance the phrase listed first in the config wins, not the one spoken first, so the earlier command is dropped"]
fn the_first_spoken_hotword_wins() {
    let cfg = HotwordConfig::default(); // ["hey computer", "hey oats"]
    assert_eq!(
        detect(
            "hey oats remind me to call mum and hey computer add bread",
            &cfg
        )
        .as_deref(),
        Some("remind me to call mum and hey computer add bread")
    );
}

// ---- executing commands --------------------------------------------------------------------------

fn segment(s: &Store, text: &str) -> TranscriptRow {
    let sid = s.begin_session(0, "e").unwrap();
    s.insert_segment(sid, 1_000, 2_000, text, None).unwrap()
}

fn action(intent: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut a = json!({"intent": intent, "title": null, "date": null, "time": null,
        "all_day": false, "relative_to_event": false, "offset_days": null, "text": null,
        "details": null, "node_name": null, "category_name": null});
    for (k, v) in extra.as_object().unwrap() {
        a[k] = v.clone();
    }
    a
}

fn run<Tz: TimeZone>(actions: Vec<serde_json::Value>, tz: &Tz) -> (Store, Vec<Executed>)
where
    Tz::Offset: std::fmt::Display,
{
    let s = Store::open_in_memory().unwrap();
    let seg = segment(&s, "hey computer …");
    let tree = s.nodes().unwrap();
    let result: CommandResult = hotword::parse_command(&json!({ "actions": actions })).unwrap();
    let done = hotword::execute(&s, &tree, &seg, &result, 5, tz).unwrap();
    (s, done)
}

fn local<Tz: TimeZone>(ms: i64, tz: &Tz) -> DateTime<Tz> {
    tz.timestamp_millis_opt(ms).single().unwrap()
}

#[test]
fn several_actions_with_relative_reminders_across_the_year_end() {
    let tz = FixedOffset::east_opt(2 * H).unwrap();
    let (s, done) = run(
        vec![
            action(
                "add_event",
                json!({"title": "New year's eve party", "date": "2026-12-31", "all_day": true}),
            ),
            action(
                "add_reminder",
                json!({"relative_to_event": true, "offset_days": -1}),
            ),
            action(
                "add_event",
                json!({"title": "Dentist", "date": "2027-01-01", "time": "09:15"}),
            ),
            action(
                "add_reminder",
                json!({"relative_to_event": true, "offset_days": -2}),
            ),
            // no date and an offset: relative even without the flag
            action(
                "add_reminder",
                json!({"offset_days": -7, "text": "Book the dentist"}),
            ),
            // after the event
            action(
                "add_reminder",
                json!({"relative_to_event": true, "offset_days": 3, "text": "Pay the dentist"}),
            ),
        ],
        &tz,
    );
    assert_eq!(done.len(), 6, "{done:?}");
    let Executed::Event(party) = &done[0] else {
        panic!("{done:?}")
    };
    let Executed::Event(dentist) = &done[2] else {
        panic!("{done:?}")
    };
    assert!(party.all_day);
    assert!(!dentist.all_day, "a time makes it a timed event");
    let r = |i: usize| match &done[i] {
        Executed::Reminder(r) => r.clone(),
        other => panic!("{other:?}"),
    };
    // each relative reminder hangs off the event spoken just before it
    assert_eq!(r(1).event_id, Some(party.id));
    assert_eq!(r(1).text, "New year's eve party tomorrow");
    assert_eq!(
        local(r(1).due, &tz).date_naive(),
        NaiveDate::from_ymd_opt(2026, 12, 30).unwrap()
    );
    assert_eq!(r(3).event_id, Some(dentist.id));
    assert_eq!(r(3).text, "Dentist in 2 days");
    let due = local(r(3).due, &tz);
    assert_eq!(
        (due.date_naive(), due.hour(), due.minute()),
        (NaiveDate::from_ymd_opt(2026, 12, 30).unwrap(), 9, 15),
        "same clock time, two days earlier, across the year boundary"
    );
    assert_eq!(r(4).event_id, Some(dentist.id));
    assert_eq!(
        local(r(4).due, &tz).date_naive(),
        NaiveDate::from_ymd_opt(2026, 12, 25).unwrap()
    );
    assert_eq!(r(5).text, "Pay the dentist");
    assert_eq!(
        local(r(5).due, &tz).date_naive(),
        NaiveDate::from_ymd_opt(2027, 1, 4).unwrap()
    );
    assert_eq!(
        done[2].describe(&tz),
        "Event: Dentist at Fri 1 Jan 2027 09:15"
    );
    assert_eq!(s.events_from(i64::MIN, 10).unwrap().len(), 2);
    assert_eq!(s.reminders(false, 10).unwrap().len(), 4);
}

#[test]
fn relative_reminders_across_month_ends_and_leap_years() {
    let tz = Utc;
    let cases = [
        ("2026-03-01", -1, (2026, 2, 28)),
        ("2028-03-01", -1, (2028, 2, 29)),
        ("2026-05-01", -1, (2026, 4, 30)),
        ("2026-01-31", 1, (2026, 2, 1)),
        ("2026-03-31", -31, (2026, 2, 28)),
        ("2027-01-05", -7, (2026, 12, 29)),
    ];
    for (date, offset, (y, m, d)) in cases {
        let (_, done) = run(
            vec![
                action("add_event", json!({"title": "Thing", "date": date})),
                action(
                    "add_reminder",
                    json!({"relative_to_event": true, "offset_days": offset}),
                ),
            ],
            &tz,
        );
        let Executed::Reminder(r) = &done[1] else {
            panic!("{date}: {done:?}")
        };
        let due = local(r.due, &tz);
        assert_eq!(
            (due.year(), due.month(), due.day()),
            (y, m, d),
            "{date} {offset:+} days"
        );
    }
    // the date formats ASR produces
    let (_, done) = run(
        vec![
            action(
                "add_event",
                json!({"title": "US style", "date": "12/31/2026"}),
            ),
            action(
                "add_event",
                json!({"title": "EU style", "date": "31.12.2026"}),
            ),
            action(
                "add_event",
                json!({"title": "Spoken", "date": "next tuesday"}),
            ),
        ],
        &tz,
    );
    let day = |e: &Executed| match e {
        Executed::Event(e) => Some(local(e.when_start, &tz).date_naive()),
        _ => None,
    };
    assert_eq!(day(&done[0]), NaiveDate::from_ymd_opt(2026, 12, 31));
    assert_eq!(day(&done[1]), NaiveDate::from_ymd_opt(2026, 12, 31));
    assert!(matches!(&done[2], Executed::Unknown(t) if t.contains("without a date")));
}

#[test]
fn relative_reminders_keep_the_clock_time_across_dst() {
    let z = eastern();
    // spring: an event after the change, its reminder a week earlier, before it
    let (_, done) = run(
        vec![
            action(
                "add_event",
                json!({"title": "Standup", "date": "2026-03-12", "time": "09:00"}),
            ),
            action(
                "add_reminder",
                json!({"relative_to_event": true, "offset_days": -7}),
            ),
        ],
        &z,
    );
    let (Executed::Event(ev), Executed::Reminder(r)) = (&done[0], &done[1]) else {
        panic!("{done:?}")
    };
    assert_eq!(ev.when_start, utc(2026, 3, 12, 13, 0) * 1000, "09:00 EDT");
    assert_eq!(r.due, utc(2026, 3, 5, 14, 0) * 1000, "09:00 EST");
    assert_eq!(
        ev.when_start - r.due,
        (7 * 24 - 1) * 3_600_000,
        "a 23-hour day in between"
    );
    let due = local(r.due, &z);
    assert_eq!((due.day(), due.hour(), due.minute()), (5, 9, 0));

    // autumn: an all-day event after the change, reminder before it; local midnight both times
    let (_, done) = run(
        vec![
            action(
                "add_event",
                json!({"title": "Taxes", "date": "2026-11-05", "all_day": true}),
            ),
            action(
                "add_reminder",
                json!({"relative_to_event": true, "offset_days": -7}),
            ),
        ],
        &z,
    );
    let (Executed::Event(ev), Executed::Reminder(r)) = (&done[0], &done[1]) else {
        panic!("{done:?}")
    };
    assert_eq!(ev.when_start, utc(2026, 11, 5, 5, 0) * 1000, "midnight EST");
    assert_eq!(r.due, utc(2026, 10, 29, 4, 0) * 1000, "midnight EDT");
    assert_eq!(local(r.due, &z).hour(), 0);
    assert_eq!(
        done[1].describe(&z),
        "Reminder: Taxes in one week on Thu 29 Oct 2026"
    );

    // the repeated hour: the earlier of the two instants
    let (_, done) = run(
        vec![action(
            "add_event",
            json!({"title": "Night bus", "date": "2026-11-01", "time": "01:30"}),
        )],
        &z,
    );
    let Executed::Event(ev) = &done[0] else {
        panic!("{done:?}")
    };
    assert_eq!(
        ev.when_start,
        utc(2026, 11, 1, 5, 30) * 1000,
        "01:30 EDT, not EST"
    );
}

#[test]
// regression test (the gap is now resolved forward by `dates::to_ms`)
fn an_event_in_the_dst_gap_is_not_silently_dropped() {
    let z = eastern();
    let (s, done) = run(
        vec![action(
            "add_event",
            json!({"title": "Early train", "date": "2026-03-08", "time": "02:30"}),
        )],
        &z,
    );
    assert_eq!(done.len(), 1, "every action reports something: {done:?}");
    // either resolved forward (to 03:30 EDT) or reported as not understood
    match &done[0] {
        // somewhere between 03:00 and 03:30 EDT
        Executed::Event(e) => assert!(
            (utc(2026, 3, 8, 7, 0) * 1000..=utc(2026, 3, 8, 7, 30) * 1000).contains(&e.when_start),
            "{e:?}"
        ),
        Executed::Unknown(_) => assert!(s.events_from(i64::MIN, 10).unwrap().is_empty()),
        other => panic!("{other:?}"),
    }
}

#[test]
// regression test (the gap is now resolved forward by `dates::to_ms`)
fn a_missing_midnight_resolves_to_the_first_instant_of_the_day() {
    let z = midnight_spring();
    let day = NaiveDate::from_ymd_opt(2026, 9, 6).unwrap();
    // the day starts at 01:00 -03 = 04:00Z
    assert_eq!(
        dates::to_ms(day, None, &z),
        Some(utc(2026, 9, 6, 4, 0) * 1000)
    );
    let (_, done) = run(
        vec![action(
            "add_event",
            json!({"title": "Fiestas Patrias prep", "date": "2026-09-06", "all_day": true}),
        )],
        &z,
    );
    assert!(matches!(&done[..], [Executed::Event(_)]), "{done:?}");
}

#[test]
#[ignore = "bug: a relative reminder whose shifted clock time falls in a DST gap is refused as 'reminder without a date' instead of moving to the next valid time"]
fn a_relative_reminder_landing_in_the_dst_gap_still_gets_a_date() {
    let z = eastern();
    let (_, done) = run(
        vec![
            action(
                "add_event",
                json!({"title": "Night shift", "date": "2026-03-15", "time": "02:30"}),
            ),
            action(
                "add_reminder",
                json!({"relative_to_event": true, "offset_days": -7}),
            ),
        ],
        &z,
    );
    assert!(matches!(&done[1], Executed::Reminder(_)), "{done:?}");
}
