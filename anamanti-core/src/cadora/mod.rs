//! Cadora shared-household **voice API** client — the control plane for the
//! `shopping_list_add` rig tool (`crate::llm::rig`).
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
//!   and the canonical direct-action body `{action, item, quantity}`. Our LLM has
//!   already parsed intent, so we send the action directly (skipping Cadora's NLU).
//!   The server replies `{ok, speech}`; we relay `speech` for the assistant to say.
//!
//! The [`GroceryController`] trait is the seam the rig tool depends on, so the tool
//! is unit-testable without hitting the network — mirroring how the `spotify_control`
//! tool holds a [`crate::music::SpotifyController`].

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
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
fn server_message(body: &Value) -> Option<&str> {
    body.get("message")
        .or_else(|| body.get("error"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Read a response body as JSON, tolerating an empty or non-JSON body (an edge error
/// page, say) as `Null` when the status is already an error, so the status-based
/// message still reaches the user. A non-JSON *success* body is an error.
async fn read_body(resp: reqwest::Response, what: &str) -> Result<(reqwest::StatusCode, Value)> {
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

/// A voice command the shopping tool can request. Mirrors Cadora's canonical
/// `/voice/command` action set; v1 exposes only `add_shopping_item`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroceryCommand {
    /// Add an item (optionally with a quantity) to the household shopping list.
    AddItem {
        item: String,
        /// 1–99; `None` lets the server default it to 1.
        quantity: Option<u32>,
    },
}

/// The control seam the `shopping_list_add` rig tool depends on. Returns a short,
/// speakable confirmation (Cadora's own `speech`) the model relays to the user.
#[async_trait]
pub trait GroceryController: Send + Sync {
    async fn command(&self, cmd: GroceryCommand) -> Result<String>;
}

/// Per-request budget for Cadora calls, so a stalled server can't hang a voice turn.
const CADORA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// `User-Agent` component label for Cadora requests.
const COMPONENT: &str = "cadora";

/// Live Cadora voice-API implementation of [`GroceryController`].
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

    /// `POST /voice/command` with the direct-action body. Returns Cadora's spoken
    /// confirmation on success, or a descriptive error the model relays.
    async fn add_item(&self, item: &str, quantity: Option<u32>) -> Result<String> {
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

        let resp = crate::http::send(
            crate::http::post(format!("{}/voice/command", self.base_url), COMPONENT)
                .bearer_auth(&self.link_token)
                .json(&payload),
            CADORA_TIMEOUT,
            "calling the Cadora voice API",
        )
        .await?;

        let (status, body) = read_body(resp, "the Cadora voice API response").await?;

        if !status.is_success() {
            // 401 = the voice link is invalid/revoked; other codes carry `{code, message}`.
            let msg = server_message(&body).unwrap_or("the Cadora voice API rejected the request");
            if status.as_u16() == 401 {
                bail!("the shopping list isn't linked (the voice link was revoked) — re-link it on the config page");
            }
            bail!("Cadora voice API error ({status}): {msg}");
        }

        // Treat anything but `ok == true` as an error and surface the server's own
        // message. Note the Workers API answers `ok:true` even when the command failed
        // server-side (its `speech` is then an apology, e.g. "Sorry, something went
        // wrong…"); we relay that speech verbatim, so the user still hears it.
        if body.get("ok").and_then(Value::as_bool) != Some(true) {
            let msg = body
                .get("speech")
                .or_else(|| body.get("error"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("the Cadora voice API could not add that item");
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
impl GroceryController for CadoraVoiceApi {
    async fn command(&self, cmd: GroceryCommand) -> Result<String> {
        match cmd {
            GroceryCommand::AddItem { item, quantity } => self.add_item(&item, quantity).await,
        }
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
            .command(GroceryCommand::AddItem {
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
        api.command(GroceryCommand::AddItem {
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
            .command(GroceryCommand::AddItem {
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
            .command(GroceryCommand::AddItem {
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
            .command(GroceryCommand::AddItem {
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
            .command(GroceryCommand::AddItem {
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
            .command(GroceryCommand::AddItem {
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
