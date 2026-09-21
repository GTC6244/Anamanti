//! Parse an iCalendar (`.ics`) body into concrete [`CalEvent`]s that fall inside a
//! [`TimeWindow`], expanding recurring events (`RRULE`) as we go.
//!
//! The `ical` crate gives us raw `VEVENT` properties (name + params + value); this
//! module turns those into wall-clock `DateTime<Local>` events. Recurrence is
//! delegated to the `rrule` crate by reconstructing the original `DTSTART` / `RRULE`
//! / `EXDATE` lines and expanding them within the query window.

use std::io::BufReader;

use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use ical::parser::ical::component::IcalEvent;
use ical::property::Property;

use super::{CalEvent, TimeWindow};

/// Hard cap on how many recurrence instances we expand for a single event, so a
/// daily "forever" rule inside a wide window can't blow up.
const MAX_OCCURRENCES: u16 = 730;

/// Parse every `VEVENT` in `ics_body` and return the instances that overlap
/// `window`, tagged with `calendar_name`. Malformed calendars/events are skipped
/// rather than failing the whole lookup.
pub fn events_in_window(calendar_name: &str, ics_body: &str, window: TimeWindow) -> Vec<CalEvent> {
    let reader = ical::IcalParser::new(BufReader::new(ics_body.as_bytes()));
    let mut out = Vec::new();
    for cal in reader {
        let cal = match cal {
            Ok(c) => c,
            Err(e) => {
                log::debug!("calendar '{calendar_name}': skipping unparseable VCALENDAR: {e}");
                continue;
            }
        };
        for ev in &cal.events {
            collect_event(calendar_name, ev, window, &mut out);
        }
    }
    out
}

fn collect_event(calendar_name: &str, ev: &IcalEvent, window: TimeWindow, out: &mut Vec<CalEvent>) {
    let props = &ev.properties;

    let dtstart = match find_prop(props, "DTSTART") {
        Some(p) => p,
        None => return, // an event with no start is not something we can place
    };
    let start_val = match dtstart.value.as_deref() {
        Some(v) => v,
        None => return,
    };
    let start_tzid = param(dtstart, "TZID");
    let all_day = is_date_only(dtstart, start_val);

    let (start, _) = match parse_ics_datetime(start_val, start_tzid, all_day) {
        Some(v) => v,
        None => return,
    };

    // Event duration, derived from DTEND when present (else 1 day for all-day
    // events, otherwise a zero-length point in time).
    let end = find_prop(props, "DTEND").and_then(|p| {
        let v = p.value.as_deref()?;
        parse_ics_datetime(v, param(p, "TZID"), is_date_only(p, v)).map(|(dt, _)| dt)
    });
    let duration = match end {
        Some(e) if e > start => e - start,
        _ if all_day => Duration::days(1),
        _ => Duration::zero(),
    };

    let summary = text_prop(props, "SUMMARY").unwrap_or_else(|| "(untitled event)".to_string());
    let location = text_prop(props, "LOCATION");
    let description = text_prop(props, "DESCRIPTION");
    let organizer = find_prop(props, "ORGANIZER").map(person_label);
    let attendees: Vec<String> = props
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case("ATTENDEE"))
        .map(person_label)
        .collect();

    let make = |occ_start: DateTime<Local>| CalEvent {
        calendar: calendar_name.to_string(),
        summary: summary.clone(),
        start: occ_start,
        end: Some(occ_start + duration),
        all_day,
        location: location.clone(),
        organizer: organizer.clone(),
        attendees: attendees.clone(),
        description: description.clone(),
    };

    if let Some(rrule) = find_prop(props, "RRULE") {
        let exdates: Vec<&Property> = props
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case("EXDATE"))
            .collect();
        for occ in expand_recurring(dtstart, rrule, &exdates, duration, window) {
            out.push(make(occ));
        }
    } else if overlaps(start, start + duration, window) {
        out.push(make(start));
    }
}

/// Expand a recurring event within `window` and return the instance start times
/// (already clamped to the window). Returns empty on any parse failure — a broken
/// rule shouldn't surface a single stale (possibly years-old) base instance.
fn expand_recurring(
    dtstart: &Property,
    rrule: &Property,
    exdates: &[&Property],
    duration: Duration,
    window: TimeWindow,
) -> Vec<DateTime<Local>> {
    let mut block = match reconstruct(dtstart) {
        Some(l) => l,
        None => return Vec::new(),
    };
    match reconstruct(rrule) {
        Some(l) => {
            block.push('\n');
            block.push_str(&l);
        }
        None => return Vec::new(),
    }
    for ex in exdates {
        if let Some(l) = reconstruct(ex) {
            block.push('\n');
            block.push_str(&l);
        }
    }

    let set: rrule::RRuleSet = match block.parse() {
        Ok(s) => s,
        Err(e) => {
            log::debug!("skipping unparseable RRULE block ({e}): {block:?}");
            return Vec::new();
        }
    };

    // Widen the lower bound by the event duration so a long instance that started
    // just before the window but is still ongoing is included.
    let after = (window.start - duration).with_timezone(&rrule::Tz::LOCAL);
    let before = window.end.with_timezone(&rrule::Tz::LOCAL);
    let result = set.after(after).before(before).all(MAX_OCCURRENCES);

    result
        .dates
        .into_iter()
        .map(|d| d.with_timezone(&Local))
        .filter(|start| overlaps(*start, *start + duration, window))
        .collect()
}

// --- property helpers ------------------------------------------------------

fn find_prop<'a>(props: &'a [Property], name: &str) -> Option<&'a Property> {
    props.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

fn param<'a>(p: &'a Property, key: &str) -> Option<&'a str> {
    p.params
        .as_ref()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v.first())
        .map(String::as_str)
}

/// A text property, unescaped per RFC 5545 (`\n`, `\,`, `\;`, `\\`).
fn text_prop(props: &[Property], name: &str) -> Option<String> {
    let v = find_prop(props, name)?.value.as_deref()?;
    if v.is_empty() {
        None
    } else {
        Some(unescape(v))
    }
}

/// A human label for an `ORGANIZER`/`ATTENDEE`: prefer the `CN` (common name)
/// param, else the address with any `mailto:` scheme stripped.
fn person_label(p: &Property) -> String {
    if let Some(cn) = param(p, "CN") {
        let cn = cn.trim().trim_matches('"');
        if !cn.is_empty() {
            return unescape(cn);
        }
    }
    let v = p.value.as_deref().unwrap_or("");
    let stripped = v
        .strip_prefix("mailto:")
        .or_else(|| v.strip_prefix("MAILTO:"))
        .unwrap_or(v);
    stripped.to_string()
}

/// True when this DTSTART/DTEND is a date (all-day), not a date-time: either an
/// explicit `VALUE=DATE` param or a bare `YYYYMMDD` value.
fn is_date_only(p: &Property, value: &str) -> bool {
    if param(p, "VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) {
        return true;
    }
    value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit())
}

/// Reconstruct an iCalendar content line (`NAME;PARAM=v:VALUE`) from a parsed
/// property, so it can be fed back to the `rrule` parser.
fn reconstruct(p: &Property) -> Option<String> {
    let value = p.value.as_ref()?;
    let mut s = p.name.to_uppercase();
    if let Some(params) = &p.params {
        for (k, vs) in params {
            s.push(';');
            s.push_str(&k.to_uppercase());
            s.push('=');
            s.push_str(&vs.join(","));
        }
    }
    s.push(':');
    s.push_str(value);
    Some(s)
}

// --- datetime parsing ------------------------------------------------------

/// Parse an iCalendar date or date-time into local wall-clock time. Returns the
/// instant plus whether it was an all-day date. Handles UTC (`…Z`), `TZID`-tagged,
/// floating, and date-only forms.
fn parse_ics_datetime(
    value: &str,
    tzid: Option<&str>,
    all_day: bool,
) -> Option<(DateTime<Local>, bool)> {
    let v = value.trim();

    if all_day || (v.len() == 8 && v.bytes().all(|b| b.is_ascii_digit())) {
        let date = NaiveDate::parse_from_str(v, "%Y%m%d").ok()?;
        let naive = date.and_hms_opt(0, 0, 0)?;
        let local = Local.from_local_datetime(&naive).single()?;
        return Some((local, true));
    }

    if let Some(stripped) = v.strip_suffix('Z') {
        let naive = NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S").ok()?;
        return Some((Utc.from_utc_datetime(&naive).with_timezone(&Local), false));
    }

    let naive = NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").ok()?;
    if let Some(tzid) = tzid {
        if let Ok(tz) = tzid.parse::<chrono_tz::Tz>() {
            if let Some(dt) = tz.from_local_datetime(&naive).single() {
                return Some((dt.with_timezone(&Local), false));
            }
        }
    }
    // Floating time: interpret in the orchestrator's local zone.
    let local = Local.from_local_datetime(&naive).single()?;
    Some((local, false))
}

/// Half-open overlap test: `[start, end)` intersects `[window.start, window.end)`.
/// Zero-length events count as present when they fall on/after the window start.
fn overlaps(start: DateTime<Local>, end: DateTime<Local>, window: TimeWindow) -> bool {
    let effective_end = if end > start { end } else { start };
    start < window.end && effective_end >= window.start
}

/// Unescape RFC 5545 TEXT values.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(',') => out.push(','),
                Some(';') => out.push(';'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    fn window(days_from_now: i64, span_days: i64) -> TimeWindow {
        let start = (Local::now() + Duration::days(days_from_now))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let start = Local.from_local_datetime(&start).single().unwrap();
        TimeWindow {
            start,
            end: start + Duration::days(span_days),
        }
    }

    #[test]
    fn parses_single_timed_event() {
        let now = Local::now();
        let dt = now + Duration::hours(2);
        let stamp = dt.format("%Y%m%dT%H%M%S").to_string();
        let ics = format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:Dentist\r\nDTSTART:{stamp}\r\nDTEND:{stamp}\r\nLOCATION:Clinic\r\nATTENDEE;CN=Alice Smith:mailto:alice@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        // Window anchored around `now` (not the calendar day) so the test is robust
        // regardless of time of day — `now + 2h` can otherwise cross midnight.
        let win = TimeWindow {
            start: now - Duration::hours(1),
            end: now + Duration::hours(3),
        };
        let events = events_in_window("Work", &ics, win);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary, "Dentist");
        assert_eq!(events[0].calendar, "Work");
        assert_eq!(events[0].attendees, vec!["Alice Smith".to_string()]);
    }

    #[test]
    fn expands_weekly_recurrence_into_window() {
        // A weekly event anchored well in the past should still surface this week.
        let anchor = (Local::now() - Duration::days(90))
            .date_naive()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        let stamp = anchor.format("%Y%m%dT%H%M%S").to_string();
        let ics = format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:Standup\r\nDTSTART:{stamp}\r\nDTEND:{stamp}\r\nRRULE:FREQ=DAILY\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let events = events_in_window("Work", &ics, window(0, 7));
        assert!(
            events.len() >= 6,
            "daily rule should yield ~7 instances in a 7-day window, got {}",
            events.len()
        );
        assert!(events.iter().all(|e| e.summary == "Standup"));
    }

    #[test]
    fn skips_events_outside_window() {
        let past = (Local::now() - Duration::days(10))
            .format("%Y%m%dT%H%M%S")
            .to_string();
        let ics = format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:Old\r\nDTSTART:{past}\r\nDTEND:{past}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let events = events_in_window("Work", &ics, window(0, 7));
        assert!(events.is_empty());
    }

    #[test]
    fn parses_all_day_date_event() {
        let today = Local::now().date_naive();
        let stamp = today.format("%Y%m%d").to_string();
        let ics = format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:{stamp}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let events = events_in_window("Home", &ics, window(0, 1));
        assert_eq!(events.len(), 1);
        assert!(events[0].all_day);
        assert_eq!(events[0].start.day(), today.day());
    }
}
