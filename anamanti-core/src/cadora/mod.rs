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

/// The default Cadora app-server base URL (the Fastify server on Fly). Matches the
/// NextHaul/Cadora client default; override via the `cadora.base_url` config key.
pub const DEFAULT_BASE_URL: &str = "https://cadora-server.fly.dev";

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

/// Live Cadora voice-API implementation of [`GroceryController`].
///
/// Holds the app-server base URL and the long-lived `vl_…` voice-link token minted
/// by the pairing-code redeem flow. Every command is one `POST /voice/command`.
pub struct CadoraVoiceApi {
    http: reqwest::Client,
    /// Base URL with any trailing slash trimmed (e.g. `https://cadora-server.fly.dev`).
    base_url: String,
    /// The `vl_…` voice-link bearer token.
    link_token: String,
}

impl CadoraVoiceApi {
    pub fn new(base_url: impl Into<String>, link_token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
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

        let resp = self
            .http
            .post(format!("{}/voice/command", self.base_url))
            .bearer_auth(&self.link_token)
            .json(&payload)
            .send()
            .await
            .context("calling the Cadora voice API")?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context("parsing the Cadora voice API response")?;

        if !status.is_success() {
            // 401 = the voice link is invalid/revoked; other codes carry an `error`.
            let msg = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the Cadora voice API rejected the request");
            if status.as_u16() == 401 {
                bail!("the shopping list isn't linked (the voice link was revoked) — re-link it on the config page");
            }
            bail!("Cadora voice API error ({status}): {msg}");
        }

        // A 200 doesn't mean the command succeeded: Cadora returns `{ok:true}` even
        // for no-ops (e.g. an empty item → `{ok:true,"speech":"I didn't catch what
        // to add"}`), and reports failures in-band as `{ok:false, ...}`. Treat
        // anything but `ok == true` as an error and surface the server's own message.
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
    let base_url = base_url.trim_end_matches('/');
    let code = code.trim();
    if code.is_empty() {
        bail!("enter the 6-digit pairing code shown in the Cadora app");
    }

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/voice/links/redeem"))
        .json(&json!({ "code": code }))
        .send()
        .await
        .context("calling the Cadora pairing endpoint")?;

    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .context("parsing the Cadora pairing response")?;

    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("invalid or expired code");
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

    #[tokio::test]
    async fn redeem_surfaces_a_dead_code() {
        let (base, _h) =
            fake_server("400 Bad Request", r#"{"error":"invalid or expired code"}"#).await;
        let err = redeem_pairing_code(&base, "000000").await.unwrap_err();
        assert!(err.to_string().contains("invalid or expired code"));
    }
}
