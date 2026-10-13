//! The shared OAuth 2.0 **loopback** consent machinery behind
//! [`crate::drive_consent`] and [`crate::spotify_consent`].
//!
//! Both flows are authorization-code + PKCE with a loopback redirect, driven from the
//! config page: generate a PKCE pair and a CSRF `state`, open the Mac's browser at the
//! provider's authorization URL, accept the redirect on a loopback listener, verify
//! `state`, then exchange the code at the provider's token endpoint. Per provider only
//! the endpoints, the redirect binding and the client-authentication style
//! ([`ClientAuth`]) differ; this module owns everything else.
//!
//! **`state`:** every authorization request carries a fresh random `state`
//! ([`new_state`]) and [`wait_for_code`] rejects a callback whose `state` is missing or
//! different, so a page that drives the browser to the loopback port with its own
//! authorization code cannot link the attacker's account.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Per-request budget for token-endpoint calls.
pub const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);

/// A PKCE `(verifier, challenge)` pair: a random 40-byte base64url verifier and its
/// SHA-256 base64url challenge (`code_challenge_method=S256`).
pub fn pkce_pair() -> (String, String) {
    use sha2::{Digest, Sha256};
    let verifier = random_token(40);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// A fresh, unguessable OAuth `state` value (32 random bytes, base64url).
pub fn new_state() -> String {
    random_token(32)
}

fn random_token(len: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Best-effort: open the Mac's default browser at `url`. Non-fatal — the URL is also
/// logged by the callers so a headless/SSH operator can paste it manually.
pub fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    #[cfg(unix)]
    {
        if let Err(e) = std::process::Command::new(program).arg(url).spawn() {
            log::warn!("could not open a browser ({e}); paste this URL manually: {url}");
        }
    }
    #[cfg(not(unix))]
    {
        log::info!("open this URL to consent: {url}");
    }
}

/// What one loopback request carried.
#[derive(Debug, PartialEq, Eq)]
enum Callback {
    /// `?code=` with a matching `state`.
    Code(String),
    /// `?error=` from the provider.
    Error(String),
    /// `?code=` whose `state` was missing or wrong.
    BadState,
    /// Anything else (e.g. `/favicon.ico`).
    Stray,
}

/// Classify one redirect request line's query against `expected_state`.
fn classify(path: &str, expected_state: &str) -> Callback {
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let (mut code, mut err, mut state) = (None, None, None);
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "error" => err = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => {}
        }
    }
    if let Some(e) = err {
        return Callback::Error(e);
    }
    match code {
        Some(c) if state.as_deref() == Some(expected_state) => Callback::Code(c),
        Some(_) => Callback::BadState,
        None => Callback::Stray,
    }
}

/// Accept loopback connections until one carries `?code=` with the expected `state`
/// (or `?error=`), replying to each with a small HTML page. Stray requests (e.g.
/// `/favicon.ico`) are ignored. A code with a missing or mismatched `state` fails the
/// consent. `provider_label` (e.g. "Spotify") only appears in the browser page text.
pub async fn wait_for_code(
    listener: &TcpListener,
    provider_label: &str,
    expected_state: &str,
) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await.context("accepting redirect")?;
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let path = req
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");
        let callback = classify(path, expected_state);
        let msg = match &callback {
            Callback::Code(_) => format!(
                "{provider_label} linked. You can close this tab and return to the orchestrator."
            ),
            Callback::Error(_) | Callback::BadState => {
                "Consent failed. Close this tab and try again.".to_string()
            }
            Callback::Stray => format!("Waiting for {provider_label}…"),
        };
        let body = format!("<html><body><h2>{msg}</h2></body></html>");
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.flush().await;

        match callback {
            Callback::Code(c) => return Ok(c),
            Callback::Error(e) => bail!("consent failed: {e}"),
            Callback::BadState => {
                bail!("consent failed: the callback's OAuth state did not match this request")
            }
            Callback::Stray => {} // keep waiting
        }
    }
}

/// [`wait_for_code`] bounded by `timeout`.
pub async fn wait_for_code_within(
    listener: &TcpListener,
    provider_label: &str,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    tokio::time::timeout(
        timeout,
        wait_for_code(listener, provider_label, expected_state),
    )
    .await
    .map_err(|_| anyhow!("timed out waiting for browser consent"))?
}

/// How the client credentials are presented at the token endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientAuth {
    /// `client_id` + `client_secret` as form fields (Google).
    Form,
    /// HTTP Basic auth with the id/secret (Spotify).
    Basic,
}

/// A provider's token endpoint plus the client credentials and auth style.
#[derive(Clone, Copy, Debug)]
pub struct TokenEndpoint<'a> {
    pub url: &'a str,
    pub client_id: &'a str,
    pub client_secret: &'a str,
    pub auth: ClientAuth,
    /// `User-Agent` component label (e.g. `"drive-consent"`).
    pub component: &'static str,
}

/// A token-endpoint response (both the code exchange and a refresh).
#[derive(Debug, Default, Deserialize)]
pub struct TokenResponse {
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_description: Option<String>,
}

/// POST `params` (form-encoded) to the token endpoint and parse the response. `what`
/// names the operation in errors ("token exchange", "token refresh"): a non-2xx is
/// `"{what} failed ({status}): {body}"`, an in-band `error` is
/// `"{what} error: {error} {description}"`.
pub async fn token_request(
    endpoint: &TokenEndpoint<'_>,
    params: &[(&str, &str)],
    what: &str,
) -> Result<TokenResponse> {
    let mut form: Vec<(&str, &str)> = Vec::with_capacity(params.len() + 2);
    let mut req = crate::http::post(endpoint.url, endpoint.component);
    match endpoint.auth {
        ClientAuth::Form => {
            form.push(("client_id", endpoint.client_id));
            form.push(("client_secret", endpoint.client_secret));
        }
        ClientAuth::Basic => {
            req = req.basic_auth(endpoint.client_id, Some(endpoint.client_secret));
        }
    }
    form.extend_from_slice(params);
    let resp =
        crate::http::send(req.form(&form), TOKEN_TIMEOUT, &format!("{what} request")).await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    parse_token_response(status, &text, what)
}

fn parse_token_response(
    status: reqwest::StatusCode,
    text: &str,
    what: &str,
) -> Result<TokenResponse> {
    if !status.is_success() {
        bail!("{what} failed ({status}): {text}");
    }
    let parsed: TokenResponse =
        serde_json::from_str(text).with_context(|| format!("parsing token response: {text}"))?;
    if let Some(e) = &parsed.error {
        bail!(
            "{what} error: {e} {}",
            parsed.error_description.as_deref().unwrap_or_default()
        );
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_pair_is_url_safe_and_distinct() {
        let (verifier, challenge) = pkce_pair();
        // base64url (no padding): only URL-safe alphabet, no '=' padding.
        for s in [&verifier, &challenge] {
            assert!(!s.is_empty());
            assert!(!s.contains('='));
            assert!(!s.contains('+'));
            assert!(!s.contains('/'));
        }
        assert_ne!(verifier, challenge);
        // Two calls produce different verifiers (CSPRNG).
        let (v2, _) = pkce_pair();
        assert_ne!(verifier, v2);
    }

    #[test]
    fn states_are_fresh_and_url_safe() {
        let (a, b) = (new_state(), new_state());
        assert_ne!(a, b);
        assert!(a.len() >= 40, "{a}");
        assert!(!a.contains(['=', '+', '/']));
    }

    #[test]
    fn classify_checks_state() {
        assert_eq!(
            classify("/cb?code=C1&state=S", "S"),
            Callback::Code("C1".into())
        );
        assert_eq!(classify("/cb?code=C1&state=X", "S"), Callback::BadState);
        assert_eq!(classify("/cb?code=C1", "S"), Callback::BadState);
        assert_eq!(
            classify("/cb?error=access_denied&state=S", "S"),
            Callback::Error("access_denied".into())
        );
        assert_eq!(classify("/favicon.ico", "S"), Callback::Stray);
    }

    /// Drive `wait_for_code` with real loopback requests.
    async fn run(paths: Vec<&'static str>, expected: &str) -> Result<String> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            for path in paths {
                let Ok(mut s) = tokio::net::TcpStream::connect(addr).await else {
                    return;
                };
                let _ = s
                    .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                    .await;
                let mut sink = Vec::new();
                let _ = s.read_to_end(&mut sink).await;
            }
        });
        let out = wait_for_code_within(&listener, "Test", expected, Duration::from_secs(5)).await;
        client.abort();
        out
    }

    #[tokio::test]
    async fn wait_for_code_accepts_the_matching_state_after_strays() {
        let code = run(vec!["/favicon.ico", "/cb?code=abc&state=S1"], "S1")
            .await
            .unwrap();
        assert_eq!(code, "abc");
    }

    #[tokio::test]
    async fn wait_for_code_rejects_a_mismatched_state() {
        let err = run(vec!["/cb?code=abc&state=EVIL"], "S1")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("state"), "{err}");
    }

    #[tokio::test]
    async fn wait_for_code_rejects_a_missing_state() {
        assert!(run(vec!["/cb?code=abc"], "S1").await.is_err());
    }

    #[test]
    fn token_response_errors_are_reported() {
        let ok = parse_token_response(
            reqwest::StatusCode::OK,
            r#"{"refresh_token":"r","scope":"s"}"#,
            "token exchange",
        )
        .unwrap();
        assert_eq!(ok.refresh_token.as_deref(), Some("r"));
        let e = parse_token_response(reqwest::StatusCode::BAD_REQUEST, "nope", "token exchange")
            .unwrap_err();
        assert_eq!(
            format!("{e}"),
            "token exchange failed (400 Bad Request): nope"
        );
        let e = parse_token_response(
            reqwest::StatusCode::OK,
            r#"{"error":"invalid_grant","error_description":"bad"}"#,
            "token refresh",
        )
        .unwrap_err();
        assert_eq!(format!("{e}"), "token refresh error: invalid_grant bad");
    }
}
