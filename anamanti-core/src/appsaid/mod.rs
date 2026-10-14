//! AppSaid HTML-push client — the control plane for the `send_phone_message` rig
//! tool (`crate::llm::rig`).
//!
//! AppSaid is a small, self-hosted push service (a Cloudflare-Worker-style HTTP
//! endpoint) that delivers notifications to phones running its companion app. The
//! assistant uses it to push a short message — often a Google Maps link — to a
//! named family member's phone ("text Mom the directions", "send this to my
//! phone"). We never touch the phones directly: AppSaid's worker owns delivery and
//! HTML sanitization. We make one HTTPS call per message:
//!
//! - **Send** — `POST {worker_url}/v1/messages` with
//!   `{ token, user, title, body, url, url_title }`. `token` is the sender app token
//!   (a SECRET), `user` is the recipient's public AppSaid `user_key`, `body` is HTML
//!   (sanitized server-side, so plaintext is fine), and `url` renders as a tappable
//!   button. A success is `202` with `{ id, status: "queued", created_at }`; a
//!   non-2xx returns `{ error, message }`.
//!
//! The [`PhoneMessenger`] trait is the seam the rig tool depends on, so the tool is
//! unit-testable without hitting the network — mirroring how the `shopping_list_add`
//! tool holds a [`crate::cadora::GroceryController`].

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

/// A message the phone tool wants pushed to a family member's phone. Intent has
/// already been parsed by the LLM; the messenger resolves the recipient name to a
/// `user_key`, builds the outbound link, and issues the send.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutgoingMessage {
    /// Family-member name to send to (case-insensitive). `None` uses the configured
    /// default recipient; if that is also unset the send fails with a clear error.
    pub recipient: Option<String>,
    /// The message text (becomes the notification title + body).
    pub message: String,
    /// An explicit link to attach as the tappable button, if any.
    pub url: Option<String>,
    /// A place/address to turn into a Google Maps link when `url` is not given.
    pub place: Option<String>,
}

/// The control seam the `send_phone_message` rig tool depends on. Returns a short,
/// speakable confirmation (e.g. `Sent to Mom.`) the model relays to the user.
#[async_trait]
pub trait PhoneMessenger: Send + Sync {
    async fn send(&self, msg: OutgoingMessage) -> Result<String>;
}

/// Normalize a user-entered AppSaid pairing code: uppercase, with spaces and dashes
/// stripped. AppSaid's claim endpoint is itself case/space/dash-insensitive, so this
/// is defense-in-depth — it also makes `"ab 2k"` / `"ab-2k"` from a phone display
/// resolve the same as `"AB2K"`.
pub fn normalize_pairing_code(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(char::to_uppercase)
        .collect()
}

/// Per-request budget for AppSaid calls.
const APPSAID_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// `User-Agent` component label for AppSaid requests.
const COMPONENT: &str = "appsaid";

/// Redeem a short **pairing code** shown on a recipient's AppSaid phone app for that
/// phone's durable public `user_key` — the TV-style onboarding from the AppSaid
/// integration guide. One `POST {worker_url}/v1/pair/claim` with `{ code }`; the claim
/// is unauthenticated (the phone isn't linked to us yet) and single-use, so a code can
/// be redeemed exactly once within its ~10 min lifetime.
///
/// Maps the documented statuses to speakable errors: `400` (not a well-formed 4-char
/// code), `404` (invalid, already used, or expired), `429` (rate limited). On success
/// returns the recipient's `user_key`, which the caller persists against a name.
pub async fn claim_pairing_code(worker_url: &str, code: &str) -> Result<String> {
    let worker_url = worker_url.trim().trim_end_matches('/');
    if worker_url.is_empty() {
        bail!("set the AppSaid worker URL before linking a phone");
    }
    let code = normalize_pairing_code(code);
    if code.is_empty() {
        bail!("enter the pairing code shown in the AppSaid app");
    }

    let resp = crate::http::send(
        crate::http::post(format!("{worker_url}/v1/pair/claim"), COMPONENT)
            .json(&json!({ "code": code })),
        APPSAID_TIMEOUT,
        "calling the AppSaid pairing endpoint",
    )
    .await?;

    let status = resp.status();
    // The body may be absent/non-JSON on some error paths; tolerate that.
    let body: Value = resp.json().await.unwrap_or(Value::Null);

    if !status.is_success() {
        let server_msg = body
            .get("message")
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let hint = match status.as_u16() {
            404 => "that pairing code is invalid, already used, or expired — ask for a fresh one",
            400 => "that doesn't look like a valid pairing code",
            429 => "too many attempts — wait a minute and try again",
            _ => server_msg.unwrap_or("the AppSaid service rejected the pairing code"),
        };
        bail!("AppSaid pairing failed ({status}): {hint}");
    }

    body.get("user_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .context("the AppSaid pairing response carried no user_key")
}

/// Build a Google Maps search link for a free-text place/address, URL-encoding the
/// query per the Maps URL scheme.
pub fn maps_search_url(place: &str) -> String {
    url::Url::parse_with_params(
        "https://www.google.com/maps/search/",
        &[("api", "1"), ("query", place.trim())],
    )
    .map(String::from)
    .unwrap_or_else(|_| {
        format!(
            "https://www.google.com/maps/search/?api=1&query={}",
            place.trim()
        )
    })
}

/// Live AppSaid implementation of [`PhoneMessenger`].
///
/// Holds the worker base URL, the secret sender app token, the recipient roster
/// (display name → `user_key`), and an optional default recipient. Every send is one
/// `POST {worker_url}/v1/messages`.
pub struct AppSaidClient {
    /// Worker base URL with any trailing slash trimmed (e.g. `https://appsaid.you.workers.dev`).
    worker_url: String,
    /// The sender app token (SECRET).
    app_token: String,
    /// Recipients as `(display_name, user_key)`, preserving the configured casing for
    /// the spoken confirmation; lookups are case-insensitive.
    recipients: Vec<(String, String)>,
    /// Default recipient name used when the model omits `recipient`.
    default_recipient: Option<String>,
}

impl AppSaidClient {
    pub fn new(
        worker_url: impl Into<String>,
        app_token: impl Into<String>,
        recipients: Vec<(String, String)>,
        default_recipient: Option<String>,
    ) -> Self {
        Self {
            worker_url: worker_url.into().trim_end_matches('/').to_string(),
            app_token: app_token.into(),
            recipients,
            default_recipient,
        }
    }

    /// Resolve a recipient name to its `(display_name, user_key)`, case-insensitively.
    fn resolve<'a>(&'a self, name: &str) -> Option<(&'a str, &'a str)> {
        let needle = name.trim();
        self.recipients
            .iter()
            .find(|(display, _)| display.trim().eq_ignore_ascii_case(needle))
            .map(|(display, key)| (display.as_str(), key.as_str()))
    }

    /// The known recipient names, for a helpful error the model can relay.
    fn known_names(&self) -> String {
        self.recipients
            .iter()
            .map(|(display, _)| display.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[async_trait]
impl PhoneMessenger for AppSaidClient {
    async fn send(&self, msg: OutgoingMessage) -> Result<String> {
        let message = msg.message.trim();
        if message.is_empty() {
            bail!("no message to send — say what to text");
        }

        // Resolve the recipient: an explicit name, else the configured default.
        let (display, user_key) = match msg
            .recipient
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(name) => self.resolve(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "I don't have a phone contact named \"{name}\". Known contacts: {}.",
                    self.known_names()
                )
            })?,
            None => {
                let default = self
                    .default_recipient
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context(
                        "no recipient was given and no default contact is set — say who to send it to",
                    )?;
                self.resolve(default).ok_or_else(|| {
                    anyhow::anyhow!(
                        "the default contact \"{default}\" isn't in the phone contacts. Known contacts: {}.",
                        self.known_names()
                    )
                })?
            }
        };

        // Build the tappable link: an explicit URL wins; otherwise a place becomes a
        // Google Maps search link.
        let link = msg
            .url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                msg.place
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(maps_search_url)
            });

        // AppSaid sanitizes HTML server-side, so sending the plaintext message as the
        // body is safe. The message doubles as the notification title.
        let mut payload = json!({
            "token": self.app_token,
            "user": user_key,
            "title": message,
            "body": message,
        });
        if let Some(u) = &link {
            payload["url"] = json!(u);
            payload["url_title"] = json!("Open");
        }

        let resp = crate::http::send(
            crate::http::post(format!("{}/v1/messages", self.worker_url), COMPONENT).json(&payload),
            APPSAID_TIMEOUT,
            "calling the AppSaid send API",
        )
        .await?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context("parsing the AppSaid send response")?;

        if !status.is_success() {
            let msg = body
                .get("message")
                .or_else(|| body.get("error"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("the AppSaid service rejected the message");
            bail!("AppSaid error ({status}): {msg}");
        }

        Ok(format!("Sent to {display}."))
    }
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

    fn client(base: String) -> AppSaidClient {
        AppSaidClient::new(
            base,
            "app_secret",
            vec![
                ("Mom".to_string(), "user_mom".to_string()),
                ("Dad".to_string(), "user_dad".to_string()),
            ],
            Some("Mom".to_string()),
        )
    }

    #[tokio::test]
    async fn send_posts_token_user_and_body() {
        let (base, handle) = fake_server(
            "202 Accepted",
            r#"{"id":"m1","status":"queued","created_at":"t"}"#,
        )
        .await;
        let api = client(base);
        let speech = api
            .send(OutgoingMessage {
                recipient: Some("mom".into()), // case-insensitive
                message: "dinner at 6".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(speech, "Sent to Mom.");

        let req = handle.await.unwrap();
        assert!(
            req.contains("POST /v1/messages"),
            "posts to /v1/messages: {req}"
        );
        assert!(req.contains("\"token\":\"app_secret\""), "sends the token");
        assert!(
            req.contains("\"user\":\"user_mom\""),
            "sends the resolved user_key"
        );
        assert!(req.contains("dinner at 6"), "sends the message body");
    }

    #[tokio::test]
    async fn place_builds_a_maps_url_button() {
        let (base, handle) = fake_server(
            "202 Accepted",
            r#"{"id":"m1","status":"queued","created_at":"t"}"#,
        )
        .await;
        let api = client(base);
        api.send(OutgoingMessage {
            recipient: Some("Dad".into()),
            message: "here's the restaurant".into(),
            place: Some("Blue Bottle Coffee, Oakland".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        let req = handle.await.unwrap();
        assert!(
            req.contains("https://www.google.com/maps/search/"),
            "builds a maps link: {req}"
        );
        assert!(
            req.contains("query=Blue+Bottle+Coffee") || req.contains("query=Blue%20Bottle"),
            "url-encodes the place: {req}"
        );
        assert!(
            req.contains("\"url_title\":\"Open\""),
            "adds a button title"
        );
    }

    #[tokio::test]
    async fn explicit_url_wins_over_place() {
        let (base, handle) = fake_server(
            "202 Accepted",
            r#"{"id":"m1","status":"queued","created_at":"t"}"#,
        )
        .await;
        let api = client(base);
        api.send(OutgoingMessage {
            recipient: Some("Mom".into()),
            message: "look at this".into(),
            url: Some("https://example.com/x".into()),
            place: Some("ignored place".into()),
        })
        .await
        .unwrap();

        let req = handle.await.unwrap();
        assert!(
            req.contains("https://example.com/x"),
            "uses the explicit url"
        );
        assert!(!req.contains("maps/search"), "does not build a maps link");
    }

    #[tokio::test]
    async fn missing_recipient_uses_the_default() {
        let (base, handle) = fake_server(
            "202 Accepted",
            r#"{"id":"m1","status":"queued","created_at":"t"}"#,
        )
        .await;
        let api = client(base);
        let speech = api
            .send(OutgoingMessage {
                recipient: None,
                message: "on my way".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            speech, "Sent to Mom.",
            "falls back to the default recipient"
        );
        let req = handle.await.unwrap();
        assert!(req.contains("\"user\":\"user_mom\""));
    }

    #[tokio::test]
    async fn unknown_recipient_errors_and_lists_known_names() {
        // No server needed — resolution fails before any HTTP call.
        let api = AppSaidClient::new(
            "http://127.0.0.1:1",
            "app_secret",
            vec![("Mom".to_string(), "user_mom".to_string())],
            None,
        );
        let err = api
            .send(OutgoingMessage {
                recipient: Some("Grandpa".into()),
                message: "hi".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Grandpa"),
            "names the unknown recipient: {msg}"
        );
        assert!(msg.contains("Mom"), "lists the known recipients: {msg}");
    }

    #[tokio::test]
    async fn no_recipient_and_no_default_is_a_clear_error() {
        let api = AppSaidClient::new(
            "http://127.0.0.1:1",
            "app_secret",
            vec![("Mom".to_string(), "user_mom".to_string())],
            None,
        );
        let err = api
            .send(OutgoingMessage {
                recipient: None,
                message: "hi".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no recipient"));
    }

    #[test]
    fn normalize_pairing_code_uppercases_and_strips_spaces_and_dashes() {
        assert_eq!(normalize_pairing_code("ab2k"), "AB2K");
        assert_eq!(normalize_pairing_code("  ab-2k "), "AB2K");
        assert_eq!(normalize_pairing_code("a b 2 k"), "AB2K");
    }

    #[tokio::test]
    async fn claim_returns_the_user_key_and_posts_normalized_code() {
        let (base, handle) = fake_server("200 OK", r#"{"user_key":"gdksxryuunome6dp"}"#).await;
        let key = claim_pairing_code(&base, "ab-2k").await.unwrap();
        assert_eq!(key, "gdksxryuunome6dp");

        let req = handle.await.unwrap();
        assert!(
            req.contains("POST /v1/pair/claim"),
            "posts to the claim endpoint: {req}"
        );
        assert!(
            req.contains("\"code\":\"AB2K\""),
            "sends the normalized code: {req}"
        );
    }

    #[tokio::test]
    async fn claim_maps_404_to_a_fresh_code_hint() {
        let (base, _h) = fake_server("404 Not Found", r#"{"error":"not_found"}"#).await;
        let err = claim_pairing_code(&base, "ZZZZ").await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("expired") || msg.contains("fresh one"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn claim_with_blank_code_errors_before_any_network() {
        // No server bound — resolution must fail before the HTTP call.
        let err = claim_pairing_code("http://127.0.0.1:1", "   ")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("pairing code"), "{err}");
    }

    #[tokio::test]
    async fn non_2xx_surfaces_the_server_message() {
        let (base, _h) = fake_server(
            "400 Bad Request",
            r#"{"error":"bad_request","message":"unknown user"}"#,
        )
        .await;
        let api = client(base);
        let err = api
            .send(OutgoingMessage {
                recipient: Some("Mom".into()),
                message: "hi".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown user"), "{err}");
    }
}
