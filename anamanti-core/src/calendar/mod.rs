//! Read-only, web-based iCalendar (`.ics`) subscriptions, exposed to the LLM as the
//! `calendar_lookup` tool (wired in `llm/rig.rs`).
//!
//! One or more subscriptions are configured in the JSON config file's `calendar`
//! block. On each lookup the [`IcalSubscription`] source fetches the feeds (cached
//! with a TTL — `calendar.cache_ttl_secs`, default 5 min — since the fetch dominates
//! cost and parsing even a ~1 MB feed is ~20 ms), parses the `VEVENT`s, expands recurring
//! events within the requested [`TimeWindow`], and returns the matching
//! [`CalEvent`]s. Everything is read-only: we never write back to a feed.
//!
//! People linkage is intentionally light for v1: filtering by `person` is a fuzzy
//! match over each event's attendees / organizer / title (see [`person_matches`]),
//! which needs no email addresses in the feed and no HelixDB round-trip. A future
//! pass can canonicalize a spoken name against `Entity`/`User` nodes in the graph.

use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone};
use tokio::sync::Mutex;

mod parse;

/// A single, concrete calendar event instance in local wall-clock time. Recurring
/// events are already expanded to one `CalEvent` per occurrence.
#[derive(Clone, Debug)]
pub struct CalEvent {
    /// Which configured calendar this came from (its display name).
    pub calendar: String,
    pub summary: String,
    pub start: DateTime<Local>,
    pub end: Option<DateTime<Local>>,
    pub all_day: bool,
    pub location: Option<String>,
    pub organizer: Option<String>,
    pub attendees: Vec<String>,
    pub description: Option<String>,
}

/// A half-open `[start, end)` local-time window to query.
#[derive(Clone, Copy, Debug)]
pub struct TimeWindow {
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
}

/// A source of calendar events. Abstracted so the tool is testable offline and so a
/// CalDAV (or EventKit) backend can be dropped in later behind the same seam.
#[async_trait]
pub trait CalendarSource: Send + Sync {
    /// Every event instance overlapping `window`, across all configured calendars,
    /// sorted by start time.
    async fn events(&self, window: TimeWindow) -> Result<Vec<CalEvent>>;

    /// Display names of the configured calendars (for the tool description).
    fn calendar_names(&self) -> Vec<String>;
}

/// One configured calendar: a display name + its subscription URL.
#[derive(Clone, Debug)]
pub struct CalendarSpec {
    pub name: String,
    pub url: String,
}

/// A [`CalendarSource`] backed by web `.ics` subscription URLs.
pub struct IcalSubscription {
    client: reqwest::Client,
    specs: Vec<CalendarSpec>,
    ttl: StdDuration,
    cache: Mutex<Option<Cache>>,
}

struct Cache {
    fetched_at: Instant,
    /// `(calendar name, raw ics body)` for each feed that fetched successfully.
    bodies: Vec<(String, String)>,
}

impl IcalSubscription {
    /// Default cache TTL. The dominant cost of a lookup is the HTTPS fetch (parsing
    /// a ~1 MB feed is ~20 ms); calendar contents don't change second-to-second, so
    /// a few minutes of staleness makes back-to-back questions instant at negligible
    /// freshness cost. Override with `calendar.cache_ttl_secs` in the config file
    /// (`0` disables caching).
    pub const DEFAULT_TTL: StdDuration = StdDuration::from_secs(300);

    pub fn new(specs: Vec<CalendarSpec>) -> Self {
        Self::with_ttl(specs, Self::DEFAULT_TTL)
    }

    /// Like [`new`](Self::new) but with an explicit cache TTL.
    pub fn with_ttl(specs: Vec<CalendarSpec>, ttl: StdDuration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(StdDuration::from_secs(10))
            .user_agent("anamanti-core/0.1 (calendar)")
            .build()
            .unwrap_or_default();
        Self {
            client,
            specs,
            ttl,
            cache: Mutex::new(None),
        }
    }

    /// Fetch all feeds, or return the cached bodies if still fresh. Individual feed
    /// failures are logged and skipped so one bad URL doesn't sink the lookup.
    async fn bodies(&self) -> Vec<(String, String)> {
        let mut guard = self.cache.lock().await;
        if let Some(cache) = guard.as_ref() {
            if cache.fetched_at.elapsed() < self.ttl {
                return cache.bodies.clone();
            }
        }
        let mut bodies = Vec::new();
        for spec in &self.specs {
            match self.fetch_one(spec).await {
                Ok(body) => bodies.push((spec.name.clone(), body)),
                Err(e) => log::warn!("calendar '{}' fetch failed: {e:#}", spec.name),
            }
        }
        *guard = Some(Cache {
            fetched_at: Instant::now(),
            bodies: bodies.clone(),
        });
        bodies
    }

    async fn fetch_one(&self, spec: &CalendarSpec) -> Result<String> {
        let url = normalize_url(&spec.url);
        let resp = self.client.get(&url).send().await?.error_for_status()?;
        Ok(resp.text().await?)
    }
}

#[async_trait]
impl CalendarSource for IcalSubscription {
    async fn events(&self, window: TimeWindow) -> Result<Vec<CalEvent>> {
        let bodies = self.bodies().await;
        let mut all = Vec::new();
        for (name, body) in &bodies {
            all.extend(parse::events_in_window(name, body, window));
        }
        all.sort_by_key(|e| e.start);
        Ok(all)
    }

    fn calendar_names(&self) -> Vec<String> {
        self.specs.iter().map(|s| s.name.clone()).collect()
    }
}

/// `webcal://` is just HTTP(S) `.ics` — rewrite it so `reqwest` can fetch it.
fn normalize_url(url: &str) -> String {
    let trimmed = url.trim();
    if let Some(rest) = trimmed
        .strip_prefix("webcal://")
        .or_else(|| trimmed.strip_prefix("WEBCAL://"))
    {
        format!("https://{rest}")
    } else {
        trimmed.to_string()
    }
}

/// Build the calendar source from the configured subscriptions + cache TTL.
/// Returns `None` (tool not advertised) when there are no usable subscriptions.
pub fn build(specs: Vec<CalendarSpec>, ttl: StdDuration) -> Option<Arc<dyn CalendarSource>> {
    if specs.is_empty() {
        return None;
    }
    log::info!(
        "calendar_lookup enabled with {} subscription(s): {} (cache TTL {}s)",
        specs.len(),
        specs
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        ttl.as_secs()
    );
    Some(Arc::new(IcalSubscription::with_ttl(specs, ttl)))
}

// ---------------------------------------------------------------------------
// Window resolution + filtering + rendering (used by the calendar_lookup tool)
// ---------------------------------------------------------------------------

/// Resolve a `when` argument into a concrete window plus a human label for the
/// answer. Accepts `today`, `tomorrow`, `this_week`, `next_7_days`, `next_14_days`,
/// `next_30_days`/`next_month`, `this_month`, `on:YYYY-MM-DD`, and
/// `range:YYYY-MM-DD..YYYY-MM-DD`. `None` / unrecognized defaults to the next 7 days
/// (returning an `Err` only for a malformed explicit date).
pub fn resolve_window(
    when: Option<&str>,
    now: DateTime<Local>,
) -> Result<(TimeWindow, String), String> {
    let start_of = |d: NaiveDate| -> DateTime<Local> {
        Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap_or(now)
    };
    let today = now.date_naive();
    let spec = when.map(str::trim).filter(|s| !s.is_empty());

    match spec {
        None | Some("next_7_days") | Some("this_week") | Some("week") => Ok((
            TimeWindow {
                start: now,
                end: start_of(today + Duration::days(7)),
            },
            "the next 7 days".to_string(),
        )),
        Some("today") => Ok((
            TimeWindow {
                start: start_of(today),
                end: start_of(today + Duration::days(1)),
            },
            format!("today ({})", today.format("%a %b %-d")),
        )),
        Some("tomorrow") => {
            let tmr = today + Duration::days(1);
            Ok((
                TimeWindow {
                    start: start_of(tmr),
                    end: start_of(tmr + Duration::days(1)),
                },
                format!("tomorrow ({})", tmr.format("%a %b %-d")),
            ))
        }
        Some("next_14_days") | Some("next_2_weeks") | Some("two_weeks") => Ok((
            TimeWindow {
                start: now,
                end: start_of(today + Duration::days(14)),
            },
            "the next 14 days".to_string(),
        )),
        Some("next_30_days") | Some("next_month") | Some("month") => Ok((
            TimeWindow {
                start: now,
                end: start_of(today + Duration::days(30)),
            },
            "the next 30 days".to_string(),
        )),
        Some("this_month") => {
            // Through the last day of the current calendar month (exclusive end =
            // first day of next month).
            let (y, m) = (today.year(), today.month());
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            let first_next = NaiveDate::from_ymd_opt(ny, nm, 1).unwrap_or(today);
            Ok((
                TimeWindow {
                    start: now,
                    end: start_of(first_next),
                },
                format!("the rest of {}", today.format("%B")),
            ))
        }
        Some(s) if s.starts_with("on:") => {
            let date = parse_date(&s[3..])?;
            Ok((
                TimeWindow {
                    start: start_of(date),
                    end: start_of(date + Duration::days(1)),
                },
                date.format("%a %b %-d, %Y").to_string(),
            ))
        }
        Some(s) if s.starts_with("range:") => {
            let body = &s[6..];
            let (a, b) = body
                .split_once("..")
                .ok_or_else(|| "range must look like range:YYYY-MM-DD..YYYY-MM-DD".to_string())?;
            let from = parse_date(a.trim())?;
            let to = parse_date(b.trim())?;
            if to < from {
                return Err("range end is before its start".to_string());
            }
            Ok((
                TimeWindow {
                    start: start_of(from),
                    end: start_of(to + Duration::days(1)),
                },
                format!(
                    "{} to {}",
                    from.format("%a %b %-d"),
                    to.format("%a %b %-d, %Y")
                ),
            ))
        }
        Some(other) => Err(format!(
            "unrecognized time window '{other}' (use today, tomorrow, this_week, \
             next_7_days, on:YYYY-MM-DD, or range:YYYY-MM-DD..YYYY-MM-DD)"
        )),
    }
}

fn parse_date(s: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|_| format!("'{s}' is not a valid YYYY-MM-DD date"))
}

/// Fuzzy person match: every whitespace token of `needle` must appear (case-
/// insensitively) somewhere in the event's title, organizer, or attendee list.
pub fn person_matches(ev: &CalEvent, needle: &str) -> bool {
    let mut hay = ev.summary.to_lowercase();
    if let Some(o) = &ev.organizer {
        hay.push(' ');
        hay.push_str(&o.to_lowercase());
    }
    for a in &ev.attendees {
        hay.push(' ');
        hay.push_str(&a.to_lowercase());
    }
    let needle = needle.to_lowercase();
    let mut tokens = needle.split_whitespace().peekable();
    if tokens.peek().is_none() {
        return true;
    }
    tokens.all(|t| hay.contains(t))
}

/// Free-text match over title/description/location.
pub fn query_matches(ev: &CalEvent, needle: &str) -> bool {
    let needle = needle.to_lowercase();
    ev.summary.to_lowercase().contains(&needle)
        || ev
            .description
            .as_deref()
            .is_some_and(|d| d.to_lowercase().contains(&needle))
        || ev
            .location
            .as_deref()
            .is_some_and(|l| l.to_lowercase().contains(&needle))
}

/// Render events into a compact, speakable text block for the model to answer from.
pub fn render_events(
    events: &[CalEvent],
    window_label: &str,
    max: usize,
    person: Option<&str>,
    query: Option<&str>,
) -> String {
    let mut filters = String::new();
    if let Some(p) = person.map(str::trim).filter(|s| !s.is_empty()) {
        filters.push_str(&format!(" involving {p}"));
    }
    if let Some(q) = query.map(str::trim).filter(|s| !s.is_empty()) {
        filters.push_str(&format!(" matching \"{q}\""));
    }

    if events.is_empty() {
        return format!("No events found for {window_label}{filters}.");
    }

    let shown = events.len().min(max);
    let mut out = format!(
        "{} event{} for {window_label}{filters}:",
        events.len(),
        if events.len() == 1 { "" } else { "s" }
    );
    if events.len() > shown {
        out = format!(
            "{} event{} for {window_label}{filters} (showing the first {shown}):",
            events.len(),
            if events.len() == 1 { "" } else { "s" }
        );
    }

    for ev in events.iter().take(shown) {
        out.push_str("\n- ");
        out.push_str(&format_time(ev));
        out.push_str(" — ");
        out.push_str(&ev.summary);
        if !ev.attendees.is_empty() {
            out.push_str(&format!(" (with {})", ev.attendees.join(", ")));
        } else if let Some(o) = &ev.organizer {
            out.push_str(&format!(" (organizer: {o})"));
        }
        if let Some(loc) = &ev.location {
            out.push_str(&format!(" at {loc}"));
        }
        out.push_str(&format!(" [{}]", ev.calendar));
    }
    out
}

fn format_time(ev: &CalEvent) -> String {
    if ev.all_day {
        return format!("{} (all day)", ev.start.format("%a %b %-d"));
    }
    let start = ev.start.format("%a %b %-d, %-I:%M %p").to_string();
    match ev.end {
        Some(end) if end > ev.start => {
            // Same day → just show the end clock time; otherwise show the full end.
            if end.date_naive() == ev.start.date_naive() {
                format!("{start}–{}", end.format("%-I:%M %p"))
            } else {
                format!("{start} – {}", end.format("%a %b %-d, %-I:%M %p"))
            }
        }
        _ => start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 9, 19, 10, 0, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn normalize_webcal_to_https() {
        assert_eq!(normalize_url("webcal://x/y.ics"), "https://x/y.ics");
        assert_eq!(normalize_url("https://x/y.ics"), "https://x/y.ics");
    }

    #[test]
    fn resolve_today_and_tomorrow() {
        let (w, label) = resolve_window(Some("today"), now()).unwrap();
        assert_eq!(w.start.date_naive(), now().date_naive());
        assert!(label.starts_with("today"));

        let (w2, _) = resolve_window(Some("tomorrow"), now()).unwrap();
        assert_eq!(
            w2.start.date_naive(),
            now().date_naive() + Duration::days(1)
        );
    }

    #[test]
    fn resolve_month_windows() {
        let (w, label) = resolve_window(Some("next_30_days"), now()).unwrap();
        assert_eq!(w.start, now());
        assert_eq!(w.end.date_naive(), now().date_naive() + Duration::days(30));
        assert_eq!(label, "the next 30 days");

        // `next_month` is an alias for the 30-day window.
        let (w2, _) = resolve_window(Some("next_month"), now()).unwrap();
        assert_eq!(w2.end, w.end);

        let (w3, _) = resolve_window(Some("next_14_days"), now()).unwrap();
        assert_eq!(w3.end.date_naive(), now().date_naive() + Duration::days(14));

        // this_month runs to the first of next month (now() is 2026-09-19).
        let (w4, label4) = resolve_window(Some("this_month"), now()).unwrap();
        assert_eq!(
            w4.end.date_naive(),
            NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()
        );
        assert_eq!(label4, "the rest of September");
    }

    #[test]
    fn resolve_default_is_next_week() {
        let (w, label) = resolve_window(None, now()).unwrap();
        assert_eq!(w.start, now());
        assert_eq!(label, "the next 7 days");
    }

    #[test]
    fn resolve_on_and_range() {
        let (w, _) = resolve_window(Some("on:2026-09-25"), now()).unwrap();
        assert_eq!(
            w.start.date_naive(),
            NaiveDate::from_ymd_opt(2026, 9, 25).unwrap()
        );

        let (w2, _) = resolve_window(Some("range:2026-09-20..2026-09-22"), now()).unwrap();
        assert_eq!(
            w2.start.date_naive(),
            NaiveDate::from_ymd_opt(2026, 9, 20).unwrap()
        );
        // end is exclusive: day after the 22nd.
        assert_eq!(
            w2.end.date_naive(),
            NaiveDate::from_ymd_opt(2026, 9, 23).unwrap()
        );
    }

    #[test]
    fn resolve_bad_date_errors() {
        assert!(resolve_window(Some("on:not-a-date"), now()).is_err());
        assert!(resolve_window(Some("banana"), now()).is_err());
    }

    fn sample(summary: &str, attendees: &[&str]) -> CalEvent {
        CalEvent {
            calendar: "Work".into(),
            summary: summary.into(),
            start: now(),
            end: Some(now() + Duration::hours(1)),
            all_day: false,
            location: Some("Room 1".into()),
            organizer: Some("Bob".into()),
            attendees: attendees.iter().map(|s| s.to_string()).collect(),
            description: None,
        }
    }

    #[test]
    fn person_match_is_fuzzy_and_multitoken() {
        let ev = sample("1:1", &["Alice Smith", "Carol"]);
        assert!(person_matches(&ev, "alice"));
        assert!(person_matches(&ev, "Alice Smith"));
        assert!(person_matches(&ev, "smith alice"));
        assert!(!person_matches(&ev, "dave"));
    }

    #[test]
    fn render_empty_and_nonempty() {
        assert_eq!(
            render_events(&[], "today", 10, Some("Alice"), None),
            "No events found for today involving Alice."
        );
        let text = render_events(&[sample("Sync", &["Alice"])], "today", 10, None, None);
        assert!(text.contains("Sync"));
        assert!(text.contains("with Alice"));
        assert!(text.contains("[Work]"));
    }
}
