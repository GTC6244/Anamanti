//! Cadora shared-household **voice API** client — the control plane for the
//! `shopping_list_add` and `calendar_add` rig tools (`crate::llm::rig`).
//!
//! Cadora is the shared backend ("one brain") behind the family's apps (NextHaul
//! et al.); it already ships a headless **voice surface** designed for exactly this:
//! *"three thin front-ends… adding a fourth surface is one adapter + one route."*
//! This orchestrator is that fourth surface. We never touch Supabase, RLS, or the
//! service-role key directly — Cadora's server owns member attribution, list
//! creation, dedupe, and the spoken confirmation. We only make two HTTPS calls:
//!
//! Cadora's API runs on Cloudflare Workers at `api.getcadora.com`, with every Cadora
//! route under `/cadora/*` (the old Fly server, `cadora-server.fly.dev`, was retired
//! 2026-10-17). The base URL therefore includes the `/cadora` prefix, and the paths
//! below are relative to it. Errors come back as `{code, message}`.
//!
//! - **Link once** — the family mints a **6-digit code** in the NextHaul app
//!   (Settings → Voice & Integrations, via `POST /voice/links/pair`); the orchestrator
//!   (the "speaker") redeems it via `POST /voice/links/redeem` for a durable `vl_…`
//!   **voice-link token**, stored like the Spotify refresh token (0600 settings file).
//! - **Each command** — `POST /voice/command` with `Authorization: Bearer vl_…`
//!   and a canonical direct-action body: `{action:"add_shopping_item", item,
//!   quantity}` or `{action:"add_event", title, starts_at, ends_at?, all_day?,
//!   location?, notes?}` (one-off, household-visible events; Cadora #231). Our LLM
//!   has already parsed intent, so we send the action directly (skipping Cadora's
//!   NLU). The server replies `{ok, speech}`; we relay `speech` for the assistant to
//!   say.
//!
//! The [`CadoraController`] trait is the seam the rig tools depend on, so they are
//! unit-testable without hitting the network — mirroring how the `spotify_control`
//! tool holds a [`crate::music::SpotifyController`].
//!
//! **Signed-in access** (saved recipes from Cadora and NextHaul, and the NextHaul
//! shopping list) needs a user bearer token, not the voice link: see [`apps`]. Linking
//! there with a household's family invite code also mints this voice link for Cadora,
//! so one code per app covers everything.

pub mod apps;

pub use apps::{
    CoreIdentity, HouseholdApp, HouseholdApps, HouseholdRecipes, LinkedHousehold,
    NextHaulShoppingList, RecipeHit, RecipeTier, SavedRecipe, ShoppingList, VoiceShoppingList,
};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use chrono::{Days, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use serde_json::{json, Value};

/// The default Cadora API base URL: the Workers host plus the `/cadora` route
/// prefix. Override via the `cadora.base_url` config key.
pub const DEFAULT_BASE_URL: &str = "https://api.getcadora.com/cadora";

/// The retired Fly host. A base URL still pointing here (an old `anamanti.json` or a
/// persisted setting) is treated as unset, so it falls back to [`DEFAULT_BASE_URL`].
const RETIRED_FLY_HOST: &str = "cadora-server.fly.dev";

/// The Workers API host. Its Cadora routes live under `/cadora`, so a bare-origin base
/// URL for this host gets the prefix added.
const API_HOST: &str = "api.getcadora.com";

/// Resolve a configured base URL to the one to call: trims whitespace and any
/// trailing slash, maps blank or the retired Fly host to [`DEFAULT_BASE_URL`], and
/// adds the `/cadora` prefix to a bare `api.getcadora.com` origin. Any other URL (a
/// local or staging server) is used as given.
pub fn normalize_base_url(configured: Option<&str>) -> String {
    let url = configured
        .map(str::trim)
        .unwrap_or("")
        .trim_end_matches('/');
    if url.is_empty() || host_of(url) == Some(RETIRED_FLY_HOST) {
        return DEFAULT_BASE_URL.to_string();
    }
    if host_of(url) == Some(API_HOST) && !url.contains(&format!("{API_HOST}/")) {
        return format!("{url}/cadora");
    }
    url.to_string()
}

/// The host of an `http(s)://host[:port]/…` URL, without the port.
fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split('/').next()?;
    authority.split(':').next().filter(|h| !h.is_empty())
}

/// The server's human-readable error, if any: the Workers API's `message`, else the
/// legacy `error` field.
pub(crate) fn server_message(body: &Value) -> Option<&str> {
    body.get("message")
        .or_else(|| body.get("error"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Read a response body as JSON, tolerating an empty or non-JSON body (an edge error
/// page, say) as `Null` when the status is already an error, so the status-based
/// message still reaches the user. A non-JSON *success* body is an error.
pub(crate) async fn read_body(
    resp: reqwest::Response,
    what: &str,
) -> Result<(reqwest::StatusCode, Value)> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .with_context(|| format!("reading {what}"))?;
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => Ok((status, v)),
        Err(_) if !status.is_success() => Ok((status, Value::Null)),
        Err(e) => Err(e).with_context(|| format!("parsing {what}")),
    }
}

/// A voice command the Cadora tools can request. Mirrors Cadora's canonical
/// `/voice/command` action set (`agenda_today` is deliberately not exposed — the
/// assistant's own `calendar_lookup` answers schedule questions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CadoraCommand {
    /// Add an item (optionally with a quantity) to the household shopping list.
    AddItem {
        item: String,
        /// 1–99; `None` lets the server default it to 1.
        quantity: Option<u32>,
    },
    /// Add a one-off event to the household calendar.
    AddEvent(NewEvent),
}

/// A one-off calendar event, with times already resolved to what Cadora accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    pub title: String,
    /// RFC 3339 with an offset (`2026-10-22T16:00:00-04:00`), or a bare date
    /// (`2026-10-22`) for an all-day event.
    pub starts_at: String,
    /// RFC 3339 end, timed events only.
    pub ends_at: Option<String>,
    pub all_day: bool,
    pub location: Option<String>,
    pub notes: Option<String>,
}

/// The control seam the `shopping_list_add` / `calendar_add` rig tools depend on.
/// Returns a short, speakable confirmation (Cadora's own `speech`) the model relays.
#[async_trait]
pub trait CadoraController: Send + Sync {
    async fn command(&self, cmd: CadoraCommand) -> Result<String>;
}

/// The resolved `starts_at` / `ends_at` / `all_day` of an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventTimes {
    pub starts_at: String,
    pub ends_at: Option<String>,
    pub all_day: bool,
}

/// Resolve spoken-style event times — a `date` (`YYYY-MM-DD`), an optional 24-hour
/// `start` (`HH:MM`) and an optional `end` (`HH:MM`) — in `tz` (the Core's local
/// zone) into what Cadora accepts. No start time ⇒ an all-day event sent as a bare
/// date. Timed events are sent as RFC 3339 instants with this zone's offset *for that
/// date* (DST-correct), so they don't depend on the household timezone being set in
/// Cadora. An end at or before the start is taken to cross midnight.
pub fn resolve_event_times<Tz: TimeZone>(
    tz: &Tz,
    date: &str,
    start: Option<&str>,
    end: Option<&str>,
) -> Result<EventTimes> {
    let day = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
        .map_err(|_| anyhow!("the date must be YYYY-MM-DD, got {date:?}"))?;
    let start = start.map(str::trim).filter(|s| !s.is_empty());
    let end = end.map(str::trim).filter(|s| !s.is_empty());
    let Some(start) = start else {
        if end.is_some() {
            bail!("an end time needs a start time");
        }
        return Ok(EventTimes {
            starts_at: day.format("%Y-%m-%d").to_string(),
            ends_at: None,
            all_day: true,
        });
    };
    let start_at = local_instant(tz, day.and_time(parse_hhmm(start)?))?;
    let ends_at = match end {
        None => None,
        Some(end) => {
            let mut end_naive = day.and_time(parse_hhmm(end)?);
            if end_naive <= start_at.naive_local() {
                end_naive = end_naive
                    .checked_add_days(Days::new(1))
                    .context("end time out of range")?;
            }
            Some(local_instant(tz, end_naive)?.to_rfc3339())
        }
    };
    Ok(EventTimes {
        starts_at: start_at.to_rfc3339(),
        ends_at,
        all_day: false,
    })
}

/// `HH:MM` (24-hour; `HH:MM:SS` tolerated).
fn parse_hhmm(t: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(t, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(t, "%H:%M:%S"))
        .map_err(|_| anyhow!("times must be 24-hour HH:MM, got {t:?}"))
}

/// A local wall-clock time as an instant in `tz`. An ambiguous time (the repeated
/// hour when clocks fall back) takes the earlier instant; a skipped one (spring
/// forward) is an error rather than a silent shift.
fn local_instant<Tz: TimeZone>(tz: &Tz, naive: NaiveDateTime) -> Result<chrono::DateTime<Tz>> {
    tz.from_local_datetime(&naive)
        .earliest()
        .with_context(|| format!("{naive} doesn't exist in the local timezone (clock change)"))
}

/// Per-request budget for Cadora calls, so a stalled server can't hang a voice turn.
const CADORA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// `User-Agent` component label for Cadora requests.
const COMPONENT: &str = "cadora";

/// Live Cadora voice-API implementation of [`CadoraController`].
///
/// Holds the app-server base URL and the long-lived `vl_…` voice-link token minted
/// by the pairing-code redeem flow. Every command is one `POST /voice/command`.
pub struct CadoraVoiceApi {
    /// Base URL, already normalized (e.g. `https://api.getcadora.com/cadora`).
    base_url: String,
    /// The `vl_…` voice-link bearer token.
    link_token: String,
}

impl CadoraVoiceApi {
    pub fn new(base_url: impl Into<String>, link_token: impl Into<String>) -> Self {
        Self {
            base_url: normalize_base_url(Some(&base_url.into())),
            link_token: link_token.into(),
        }
    }

    /// The `add_shopping_item` body.
    fn add_item_payload(item: &str, quantity: Option<u32>) -> Result<Value> {
        let item = item.trim();
        if item.is_empty() {
            bail!("no item to add — say what to put on the shopping list");
        }
        // Cadora validates quantity as a positive int ≤ 99; drop 0 and clamp the top.
        let quantity = quantity.filter(|q| *q >= 1).map(|q| q.min(99));
        let mut payload = json!({ "action": "add_shopping_item", "item": item });
        if let Some(q) = quantity {
            payload["quantity"] = json!(q);
        }
        Ok(payload)
    }

    /// The `add_event` body (Cadora caps: title 200, location 200, notes 2000 chars).
    fn add_event_payload(event: &NewEvent) -> Result<Value> {
        let title = event.title.trim();
        if title.is_empty() {
            bail!("no event title — say what the event is");
        }
        let clip = |s: &str, max: usize| s.trim().chars().take(max).collect::<String>();
        let mut payload = json!({
            "action": "add_event",
            "title": clip(title, 200),
            "starts_at": event.starts_at,
        });
        if event.all_day {
            payload["all_day"] = json!(true);
        }
        if let Some(end) = &event.ends_at {
            payload["ends_at"] = json!(end);
        }
        let optional = |v: &Option<String>, max: usize| {
            v.as_deref().map(|s| clip(s, max)).filter(|s| !s.is_empty())
        };
        if let Some(location) = optional(&event.location, 200) {
            payload["location"] = json!(location);
        }
        if let Some(notes) = optional(&event.notes, 2000) {
            payload["notes"] = json!(notes);
        }
        Ok(payload)
    }

    /// `POST /voice/command` with a direct-action body. Returns Cadora's spoken
    /// confirmation on success, or a descriptive error the model relays.
    async fn post_command(&self, payload: &Value) -> Result<String> {
        let resp = crate::http::send(
            crate::http::post(format!("{}/voice/command", self.base_url), COMPONENT)
                .bearer_auth(&self.link_token)
                .json(payload),
            CADORA_TIMEOUT,
            "calling the Cadora voice API",
        )
        .await?;

        let (status, body) = read_body(resp, "the Cadora voice API response").await?;

        if !status.is_success() {
            // 401 = the voice link is invalid/revoked; other codes carry `{code, message}`.
            let msg = server_message(&body).unwrap_or("the Cadora voice API rejected the request");
            if status.as_u16() == 401 {
                bail!("Cadora isn't linked (the voice link was revoked) — re-link it on the config page");
            }
            bail!("Cadora voice API error ({status}): {msg}");
        }

        // Treat anything but `ok == true` as an error and surface the server's own
        // message. Note the Workers API answers `ok:true` even when a shopping add
        // failed server-side (its `speech` is then an apology, e.g. "Sorry, something
        // went wrong…"); we relay that speech verbatim, so the user still hears it.
        // (`add_event` failures come back as real 4xx/5xx statuses instead.)
        if body.get("ok").and_then(Value::as_bool) != Some(true) {
            let msg = body
                .get("speech")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| server_message(&body))
                .unwrap_or("the Cadora voice API could not do that");
            bail!("{msg}");
        }

        // Success is `{ ok: true, speech: "Added 2 milk to your shopping list." }`.
        // Relay Cadora's spoken confirmation verbatim — never fabricate one, or we'd
        // risk claiming success the server never confirmed.
        let speech = body
            .get("speech")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .context("the Cadora voice API returned success without a spoken confirmation")?;
        Ok(speech)
    }
}

#[async_trait]
impl CadoraController for CadoraVoiceApi {
    async fn command(&self, cmd: CadoraCommand) -> Result<String> {
        let payload = match &cmd {
            CadoraCommand::AddItem { item, quantity } => Self::add_item_payload(item, *quantity)?,
            CadoraCommand::AddEvent(event) => Self::add_event_payload(event)?,
        };
        self.post_command(&payload).await
    }
}

/// Redeem a spoken **6-digit pairing code** (minted in the Cadora app) for a durable
/// `vl_…` voice-link token, via `POST /voice/links/redeem`. This is the one-time
/// linking step; the returned token is then stored and used on every command.
///
/// The redeem route is unauthenticated by design (the code *is* the proof) and is
/// single-use, short-TTL, and throttled server-side.
pub async fn redeem_pairing_code(base_url: &str, code: &str) -> Result<String> {
    let base_url = normalize_base_url(Some(base_url));
    let code = code.trim();
    if code.is_empty() {
        bail!("enter the 6-digit pairing code shown in the Cadora app");
    }

    let resp = crate::http::send(
        crate::http::post(format!("{base_url}/voice/links/redeem"), COMPONENT)
            .json(&json!({ "code": code })),
        CADORA_TIMEOUT,
        "calling the Cadora pairing endpoint",
    )
    .await?;

    let (status, body) = read_body(resp, "the Cadora pairing response").await?;

    if !status.is_success() {
        if status.as_u16() == 429 {
            bail!("could not link the shopping list: too many attempts — wait a few minutes and try again");
        }
        let msg = server_message(&body).unwrap_or("invalid or expired code");
        bail!("could not link the shopping list: {msg}");
    }

    body.get("token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|t| !t.is_empty())
        .context("the Cadora pairing response carried no token")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spawn a one-shot fake HTTP server that captures the request and replies with
    /// `status`/`body`. Returns its `http://127.0.0.1:PORT` base and a handle whose
    /// join yields the raw request text (headers + body) for assertions.
    async fn fake_server(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            req
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn add_item_sends_action_and_returns_speech() {
        let (base, handle) = fake_server(
            "200 OK",
            r#"{"ok":true,"speech":"Added 2 milk to your shopping list."}"#,
        )
        .await;
        let api = CadoraVoiceApi::new(base, "vl_test");
        let speech = api
            .command(CadoraCommand::AddItem {
                item: "milk".into(),
                quantity: Some(2),
            })
            .await
            .unwrap();
        assert_eq!(speech, "Added 2 milk to your shopping list.");

        let req = handle.await.unwrap();
        let lower = req.to_lowercase();
        assert!(
            req.contains("POST /voice/command"),
            "posts to /voice/command"
        );
        assert!(
            lower.contains("authorization: bearer vl_test"),
            "sends the link token"
        );
        assert!(
            req.contains("\"action\":\"add_shopping_item\""),
            "sends the action"
        );
        assert!(req.contains("\"item\":\"milk\""), "sends the item");
        assert!(req.contains("\"quantity\":2"), "sends the quantity");
    }

    #[tokio::test]
    async fn add_item_omits_quantity_when_absent() {
        let (base, handle) = fake_server(
            "200 OK",
            r#"{"ok":true,"speech":"Added bread to your shopping list."}"#,
        )
        .await;
        let api = CadoraVoiceApi::new(base, "vl_test");
        api.command(CadoraCommand::AddItem {
            item: "bread".into(),
            quantity: None,
        })
        .await
        .unwrap();
        let req = handle.await.unwrap();
        assert!(!req.contains("quantity"), "no quantity field when unset");
    }

    #[tokio::test]
    async fn empty_item_is_rejected_before_any_request() {
        // No server needed — the guard fires before the HTTP call.
        let api = CadoraVoiceApi::new("http://127.0.0.1:1", "vl_test");
        let err = api
            .command(CadoraCommand::AddItem {
                item: "   ".into(),
                quantity: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no item"));
    }

    #[tokio::test]
    async fn ok_false_on_200_is_an_error_not_a_fake_success() {
        // Cadora replies 200 with `ok:false` for a no-op (e.g. it couldn't parse an
        // item). We must relay its `speech` as an error, never fabricate a success.
        let (base, _h) = fake_server(
            "200 OK",
            r#"{"ok":false,"speech":"I didn't catch what to add"}"#,
        )
        .await;
        let api = CadoraVoiceApi::new(base, "vl_test");
        let err = api
            .command(CadoraCommand::AddItem {
                item: "milk".into(),
                quantity: None,
            })
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert_eq!(
            msg, "I didn't catch what to add",
            "relays the server's speech as the error: {msg}"
        );
        assert!(
            !msg.contains("Added"),
            "does not fabricate an 'Added …' confirmation: {msg}"
        );
    }

    #[tokio::test]
    async fn revoked_link_gives_a_relink_hint() {
        let (base, _h) = fake_server(
            "401 Unauthorized",
            r#"{"error":"invalid or revoked voice link"}"#,
        )
        .await;
        let api = CadoraVoiceApi::new(base, "vl_dead");
        let err = api
            .command(CadoraCommand::AddItem {
                item: "milk".into(),
                quantity: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("re-link"),
            "hints at re-linking: {err}"
        );
    }

    #[tokio::test]
    async fn redeem_returns_the_token() {
        let (base, handle) =
            fake_server("200 OK", r#"{"token":"vl_minted","provider":"generic"}"#).await;
        let token = redeem_pairing_code(&base, "042913").await.unwrap();
        assert_eq!(token, "vl_minted");
        let req = handle.await.unwrap();
        assert!(req.contains("POST /voice/links/redeem"));
        assert!(req.contains("\"code\":\"042913\""));
    }

    fn event(title: &str, starts_at: &str) -> NewEvent {
        NewEvent {
            title: title.into(),
            starts_at: starts_at.into(),
            ends_at: None,
            all_day: false,
            location: None,
            notes: None,
        }
    }

    #[tokio::test]
    async fn add_event_sends_the_event_and_returns_speech() {
        let (base, handle) = fake_server(
            "201 Created",
            r#"{"ok":true,"speech":"Added Soccer to the calendar for Thursday, October 22 at 4 PM.","event":{"id":"e1"}}"#,
        )
        .await;
        let api = CadoraVoiceApi::new(base, "vl_test");
        let speech = api
            .command(CadoraCommand::AddEvent(NewEvent {
                ends_at: Some("2026-10-22T17:00:00-04:00".into()),
                location: Some("  Lions Park ".into()),
                notes: Some("   ".into()),
                ..event("Soccer", "2026-10-22T16:00:00-04:00")
            }))
            .await
            .unwrap();
        assert_eq!(
            speech,
            "Added Soccer to the calendar for Thursday, October 22 at 4 PM."
        );
        let req = handle.await.unwrap();
        assert!(req.contains("POST /voice/command"));
        assert!(req.contains("\"action\":\"add_event\""));
        assert!(req.contains("\"title\":\"Soccer\""));
        assert!(req.contains("\"starts_at\":\"2026-10-22T16:00:00-04:00\""));
        assert!(req.contains("\"ends_at\":\"2026-10-22T17:00:00-04:00\""));
        assert!(
            req.contains("\"location\":\"Lions Park\""),
            "trims location"
        );
        assert!(!req.contains("notes"), "drops blank notes");
        assert!(!req.contains("all_day"), "timed event sends no all_day");
    }

    #[tokio::test]
    async fn add_event_all_day_sends_the_flag() {
        let (base, handle) = fake_server(
            "201 Created",
            r#"{"ok":true,"speech":"Added Pizza day to the calendar for Friday, October 23."}"#,
        )
        .await;
        let api = CadoraVoiceApi::new(base, "vl_test");
        api.command(CadoraCommand::AddEvent(NewEvent {
            all_day: true,
            ..event("Pizza day", "2026-10-23")
        }))
        .await
        .unwrap();
        let req = handle.await.unwrap();
        assert!(req.contains("\"all_day\":true"));
        assert!(req.contains("\"starts_at\":\"2026-10-23\""));
    }

    #[tokio::test]
    async fn add_event_rejection_surfaces_the_server_message() {
        let (base, _h) = fake_server(
            "400 Bad Request",
            r#"{"code":"invalid_request","message":"ends_at must be after starts_at"}"#,
        )
        .await;
        let api = CadoraVoiceApi::new(base, "vl_test");
        let err = api
            .command(CadoraCommand::AddEvent(event(
                "Soccer",
                "2026-10-22T16:00:00-04:00",
            )))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("ends_at must be after starts_at"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn add_event_without_a_title_fails_before_any_request() {
        let api = CadoraVoiceApi::new("http://127.0.0.1:1", "vl_test");
        let err = api
            .command(CadoraCommand::AddEvent(event("  ", "2026-10-22")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no event title"));
    }

    fn toronto() -> chrono_tz::Tz {
        chrono_tz::America::Toronto
    }

    #[test]
    fn resolve_all_day_is_a_bare_date() {
        let t = resolve_event_times(&toronto(), "2026-10-23", None, None).unwrap();
        assert_eq!(
            t,
            EventTimes {
                starts_at: "2026-10-23".into(),
                ends_at: None,
                all_day: true
            }
        );
    }

    #[test]
    fn resolve_timed_uses_the_offset_for_that_date() {
        // EDT before the Nov 1 2026 fall-back, EST after.
        let t =
            resolve_event_times(&toronto(), "2026-10-22", Some("16:00"), Some("17:30")).unwrap();
        assert_eq!(t.starts_at, "2026-10-22T16:00:00-04:00");
        assert_eq!(t.ends_at.as_deref(), Some("2026-10-22T17:30:00-04:00"));
        assert!(!t.all_day);
        let t = resolve_event_times(&toronto(), "2026-11-05", Some("09:15"), None).unwrap();
        assert_eq!(t.starts_at, "2026-11-05T09:15:00-05:00");
        assert_eq!(t.ends_at, None);
    }

    #[test]
    fn resolve_end_before_start_crosses_midnight() {
        let t =
            resolve_event_times(&toronto(), "2026-10-22", Some("21:00"), Some("01:00")).unwrap();
        assert_eq!(t.ends_at.as_deref(), Some("2026-10-23T01:00:00-04:00"));
    }

    #[test]
    fn resolve_rejects_bad_input() {
        let tz = toronto();
        assert!(resolve_event_times(&tz, "Thursday", None, None).is_err());
        assert!(resolve_event_times(&tz, "2026-10-22", Some("4pm"), None).is_err());
        assert!(resolve_event_times(&tz, "2026-10-22", None, Some("17:00")).is_err());
        // 02:30 on the spring-forward day doesn't exist.
        assert!(resolve_event_times(&tz, "2026-03-08", Some("02:30"), None).is_err());
    }

    #[test]
    fn normalize_base_url_defaults_and_migrates() {
        assert_eq!(normalize_base_url(None), DEFAULT_BASE_URL);
        assert_eq!(normalize_base_url(Some("  ")), DEFAULT_BASE_URL);
        // The retired Fly host (an old seed or persisted value) falls back to the default.
        assert_eq!(
            normalize_base_url(Some("https://cadora-server.fly.dev")),
            DEFAULT_BASE_URL
        );
        assert_eq!(
            normalize_base_url(Some("https://cadora-server.fly.dev/")),
            DEFAULT_BASE_URL
        );
        // A bare Workers origin gets the /cadora route prefix.
        assert_eq!(
            normalize_base_url(Some("https://api.getcadora.com/")),
            DEFAULT_BASE_URL
        );
        assert_eq!(
            normalize_base_url(Some("https://api.getcadora.com/cadora/")),
            DEFAULT_BASE_URL
        );
        // Anything else (local/staging) is used as given, minus a trailing slash.
        assert_eq!(
            normalize_base_url(Some("http://localhost:8787/cadora/")),
            "http://localhost:8787/cadora"
        );
    }

    #[tokio::test]
    async fn workers_error_shape_message_is_surfaced() {
        let (base, _h) = fake_server(
            "400 Bad Request",
            r#"{"code":"invalid_request","message":"quantity must be 1–99"}"#,
        )
        .await;
        let api = CadoraVoiceApi::new(base, "vl_test");
        let err = api
            .command(CadoraCommand::AddItem {
                item: "milk".into(),
                quantity: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("quantity must be 1–99"),
            "surfaces the Workers `message`: {err}"
        );
    }

    #[tokio::test]
    async fn non_json_error_body_still_reports_the_status() {
        let (base, _h) = fake_server("502 Bad Gateway", "<html>bad gateway</html>").await;
        let api = CadoraVoiceApi::new(base, "vl_test");
        let err = api
            .command(CadoraCommand::AddItem {
                item: "milk".into(),
                quantity: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("502"), "reports the status: {err}");
    }

    #[tokio::test]
    async fn redeem_surfaces_a_workers_dead_code() {
        let (base, _h) = fake_server(
            "400 Bad Request",
            r#"{"code":"invalid_code","message":"invalid or expired code"}"#,
        )
        .await;
        let err = redeem_pairing_code(&base, "000000").await.unwrap_err();
        assert!(err.to_string().contains("invalid or expired code"));
    }

    #[tokio::test]
    async fn redeem_rate_limit_says_to_wait() {
        let (base, _h) = fake_server(
            "429 Too Many Requests",
            r#"{"code":"rate_limited","message":"too many attempts, please try again later"}"#,
        )
        .await;
        let err = redeem_pairing_code(&base, "123456").await.unwrap_err();
        assert!(err.to_string().contains("too many attempts"), "{err}");
    }

    #[tokio::test]
    async fn redeem_surfaces_a_dead_code() {
        let (base, _h) =
            fake_server("400 Bad Request", r#"{"error":"invalid or expired code"}"#).await;
        let err = redeem_pairing_code(&base, "000000").await.unwrap_err();
        assert!(err.to_string().contains("invalid or expired code"));
    }
}
