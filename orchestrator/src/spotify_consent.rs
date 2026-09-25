//! One-time Spotify OAuth **consent**, run inside the orchestrator.
//!
//! Mirrors [`crate::drive_consent`]: OAuth 2.0 authorization-code + PKCE with a
//! **loopback** redirect, driven from the config page (see [`crate::webconfig`]).
//! The resulting refresh token is stored in the orchestrator's settings
//! ([`crate::settings::SpotifyConfig`]) and drives the `spotify_control` rig tool.
//!
//! **Key difference from Google:** Spotify requires the redirect URI to be an
//! **exact, pre-registered** value — it does not allow an arbitrary loopback port.
//! So consent binds a **fixed** port (default 8888) and the operator must add
//! `http://127.0.0.1:<port>/callback` to their Spotify app's Redirect URIs. If the
//! port is busy, consent fails with a clear message rather than silently using a
//! different (unregistered) port.
//!
//! Requires **Spotify Premium** to actually play, though consent itself works on
//! any account.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const AUTH_ENDPOINT: &str = "https://accounts.spotify.com/authorize";
const TOKEN_ENDPOINT: &str = "https://accounts.spotify.com/api/token";

/// Scopes needed to control playback and read the device list.
pub const SPOTIFY_SCOPE: &str = "user-modify-playback-state user-read-playback-state";

/// Default loopback port for the consent redirect. The operator registers
/// `http://127.0.0.1:8888/callback` in the Spotify app.
pub const DEFAULT_CONSENT_PORT: u16 = 8888;

/// The result of a successful consent: a long-lived refresh token and the granted
/// scope.
#[derive(Debug, Clone)]
pub struct ConsentOutcome {
    pub refresh_token: String,
    pub scope: String,
}

/// A PKCE `(verifier, challenge)` pair: a random 40-byte base64url verifier and its
/// SHA-256 base64url challenge (`code_challenge_method=S256`).
fn pkce_pair() -> (String, String) {
    use rand::RngCore;
    use sha2::{Digest, Sha256};
    let mut bytes = [0u8; 40];
    rand::thread_rng().fill_bytes(&mut bytes);
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let verifier = engine.encode(bytes);
    let challenge = engine.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// Build the Spotify authorization URL for the loopback consent request.
fn auth_url(client_id: &str, redirect_uri: &str, scope: &str, challenge: &str) -> Result<String> {
    let mut url = url::Url::parse(AUTH_ENDPOINT).context("parsing auth endpoint")?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", scope)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.to_string())
}

/// Best-effort: open the Mac's default browser at `url`. Non-fatal — the URL is also
/// logged so a headless/SSH operator can paste it manually.
fn open_browser(url: &str) {
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

/// Run the full loopback consent flow on a **fixed** port (must match a Redirect URI
/// registered in the Spotify app): bind the loopback redirect, open the browser,
/// wait (up to `timeout`) for Spotify to redirect back with the authorization code,
/// then exchange it for a refresh token.
pub async fn run_consent(
    client_id: &str,
    client_secret: &str,
    port: u16,
    scope: &str,
    timeout: Duration,
) -> Result<ConsentOutcome> {
    if client_id.is_empty() || client_secret.is_empty() {
        bail!("Spotify client id/secret are not set");
    }
    let (verifier, challenge) = pkce_pair();
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| {
            format!(
            "binding loopback redirect on 127.0.0.1:{port} (is another consent or app using it?)"
        )
        })?;
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let url = auth_url(client_id, &redirect_uri, scope, &challenge)?;

    open_browser(&url);
    log::info!(
        "spotify consent: approve access in the browser (or open {url}); \
         redirect {redirect_uri} must be registered in the Spotify app"
    );

    let code = tokio::time::timeout(timeout, wait_for_code(&listener))
        .await
        .map_err(|_| anyhow!("timed out waiting for browser consent"))??;

    exchange_code(
        client_id,
        client_secret,
        &code,
        &verifier,
        &redirect_uri,
        scope,
    )
    .await
}

/// Accept loopback connections until one carries `?code=` (or `?error=`), replying
/// with a small HTML page. Stray requests (e.g. `/favicon.ico`) are ignored.
async fn wait_for_code(listener: &TcpListener) -> Result<String> {
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
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let mut code = None;
        let mut err = None;
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "error" => err = Some(v.into_owned()),
                _ => {}
            }
        }
        let msg = if code.is_some() {
            "Spotify linked. You can close this tab and return to the orchestrator."
        } else if err.is_some() {
            "Consent failed. Close this tab and try again."
        } else {
            "Waiting for Spotify…"
        };
        let body = format!("<html><body><h2>{msg}</h2></body></html>");
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.flush().await;

        if let Some(e) = err {
            bail!("consent failed: {e}");
        }
        if let Some(c) = code {
            return Ok(c);
        }
        // No code and no error (a stray request) — keep waiting.
    }
}

#[derive(Deserialize)]
struct TokenResp {
    refresh_token: Option<String>,
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Exchange the authorization code for tokens at Spotify's token endpoint (Basic
/// auth with the client id/secret, plus the PKCE verifier).
async fn exchange_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    scope: &str,
) -> Result<ConsentOutcome> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building token-exchange HTTP client")?;
    let resp = client
        .post(TOKEN_ENDPOINT)
        .basic_auth(client_id, Some(client_secret))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("token exchange request")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("token exchange failed ({status}): {text}");
    }
    let parsed: TokenResp =
        serde_json::from_str(&text).with_context(|| format!("parsing token response: {text}"))?;
    if let Some(e) = parsed.error {
        bail!(
            "token exchange error: {e} {}",
            parsed.error_description.unwrap_or_default()
        );
    }
    let refresh_token = parsed
        .refresh_token
        .ok_or_else(|| anyhow!("no refresh_token returned (re-run consent)"))?;
    Ok(ConsentOutcome {
        refresh_token,
        scope: parsed.scope.unwrap_or_else(|| scope.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_pair_is_url_safe_and_distinct() {
        let (verifier, challenge) = pkce_pair();
        for s in [&verifier, &challenge] {
            assert!(!s.is_empty());
            assert!(!s.contains('='));
            assert!(!s.contains('+'));
            assert!(!s.contains('/'));
        }
        assert_ne!(verifier, challenge);
        let (v2, _) = pkce_pair();
        assert_ne!(verifier, v2);
    }

    #[test]
    fn auth_url_contains_required_params() {
        let url = auth_url(
            "cid123",
            "http://127.0.0.1:8888/callback",
            SPOTIFY_SCOPE,
            "chal",
        )
        .unwrap();
        assert!(url.starts_with(AUTH_ENDPOINT));
        assert!(url.contains("client_id=cid123"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        // redirect_uri + scope are percent-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8888%2Fcallback"));
        assert!(url.contains("user-modify-playback-state"));
    }
}
