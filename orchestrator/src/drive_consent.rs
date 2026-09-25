//! One-time Google Drive OAuth **consent**, run inside the orchestrator.
//!
//! This replaces the standalone `tools/google_photo_consent.py`: the orchestrator
//! now performs the consent itself, driven from the config page (see
//! [`crate::webconfig`]). The flow is OAuth 2.0 authorization-code + PKCE with a
//! **loopback** redirect (`127.0.0.1:<ephemeral>`), which requires a Google Cloud
//! OAuth client of type **"Desktop app"** (the TV/Ambient client can't do loopback).
//!
//! The resulting refresh token is stored in the orchestrator's settings
//! ([`crate::settings::DriveConfig`]); the device pulls it (plus the client
//! credentials and folder ids) over Wyoming (`ambient-get-drive-token`) and mints
//! Drive access tokens on-device. Consent runs once on the Mac (real browser +
//! keyboard); tokens then live on the orchestrator and the device.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const DRIVE_FILES_ENDPOINT: &str = "https://www.googleapis.com/drive/v3/files";

/// The result of a successful consent: a long-lived refresh token, an optional
/// first access token (usable immediately to verify folders), and the granted scope.
#[derive(Debug, Clone)]
pub struct ConsentOutcome {
    pub refresh_token: String,
    pub access_token: Option<String>,
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
    let verifier = engine.encode(bytes);
    let challenge = engine.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// Build the Google authorization URL for the loopback consent request.
fn auth_url(client_id: &str, redirect_uri: &str, scope: &str, challenge: &str) -> Result<String> {
    let mut url = url::Url::parse(AUTH_ENDPOINT).context("parsing auth endpoint")?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", scope)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
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

/// Run the full loopback consent flow: bind an ephemeral loopback redirect port,
/// open the browser, wait (up to `timeout`) for Google to redirect back with the
/// authorization code, then exchange it for a refresh token.
pub async fn run_consent(
    client_id: &str,
    client_secret: &str,
    scope: &str,
    timeout: Duration,
) -> Result<ConsentOutcome> {
    if client_id.is_empty() || client_secret.is_empty() {
        bail!("Drive OAuth client id/secret are not set (a 'Desktop app' client is required)");
    }
    let (verifier, challenge) = pkce_pair();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding loopback redirect listener")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let url = auth_url(client_id, &redirect_uri, scope, &challenge)?;

    open_browser(&url);
    log::info!("drive consent: approve access in the browser (or open {url})");

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
        let has_result = code.is_some() || err.is_some();
        let msg = if code.is_some() {
            "Linked. You can close this tab and return to the orchestrator."
        } else if err.is_some() {
            "Consent failed. Close this tab and try again."
        } else {
            "Waiting for Google…"
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
        let _ = has_result;
    }
}

#[derive(Deserialize)]
struct TokenResp {
    refresh_token: Option<String>,
    access_token: Option<String>,
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Exchange the authorization code for tokens at Google's token endpoint.
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
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code),
            ("code_verifier", verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
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
    let refresh_token = parsed.refresh_token.ok_or_else(|| {
        anyhow!(
            "no refresh_token returned (re-run consent; confirm the OAuth client is type \
             'Desktop app')"
        )
    })?;
    Ok(ConsentOutcome {
        refresh_token,
        access_token: parsed.access_token,
        scope: parsed.scope.unwrap_or_else(|| scope.to_string()),
    })
}

/// Count image files visible in a Drive folder, to verify a link/folder id from the
/// config page. Uses the immediate access token from consent (or a freshly minted
/// one). Returns the number of images (capped at the page size).
pub async fn verify_folder(access_token: &str, folder_id: &str) -> Result<usize> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("building verify HTTP client")?;
    let q = format!("'{folder_id}' in parents and mimeType contains 'image/' and trashed = false");
    let url = url::Url::parse_with_params(
        DRIVE_FILES_ENDPOINT,
        &[
            ("q", q.as_str()),
            ("fields", "files(id,name)"),
            ("pageSize", "10"),
            ("corpora", "user"),
        ],
    )
    .context("building verify URL")?;
    let resp = client
        .get(url)
        .bearer_auth(access_token)
        .send()
        .await
        .context("verify request")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{status}: {text}");
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing verify response: {text}"))?;
    Ok(v["files"].as_array().map(|a| a.len()).unwrap_or(0))
}

/// Mint a short-lived Drive access token from the stored refresh token
/// (`grant_type=refresh_token`). Unlike consent, this needs no browser — it's used
/// by the config page's folder picker to list the account's folders on demand.
pub async fn mint_access_token(
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<String> {
    if client_id.is_empty() || client_secret.is_empty() || refresh_token.is_empty() {
        bail!("Drive is not linked yet (missing client id/secret or refresh token)");
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building token-refresh HTTP client")?;
    let resp = client
        .post(TOKEN_ENDPOINT)
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .context("token refresh request")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("token refresh failed ({status}): {text}");
    }
    let parsed: TokenResp =
        serde_json::from_str(&text).with_context(|| format!("parsing token response: {text}"))?;
    if let Some(e) = parsed.error {
        bail!(
            "token refresh error: {e} {}",
            parsed.error_description.unwrap_or_default()
        );
    }
    parsed
        .access_token
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("token refresh returned no access token"))
}

/// A Drive folder the user can pick for the slideshow (config-page folder picker).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DriveFolderInfo {
    pub id: String,
    pub name: String,
    /// Shared *with* the user (not owned by them) — surfaced so the picker can badge it.
    pub shared: bool,
}

/// List the account's Drive folders (owned + "Shared with me"), name-ordered, so the
/// config page can offer a picker instead of hand-typed folder ids. Mirrors the
/// device's `listDriveFolders` query so the picker shows exactly the folders the
/// slideshow can later read. Paginates up to `max` folders. Needs `drive.readonly`.
pub async fn list_folders(access_token: &str, max: usize) -> Result<Vec<DriveFolderInfo>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building folder-list HTTP client")?;
    let q = "mimeType = 'application/vnd.google-apps.folder' and trashed = false";
    let mut folders: Vec<DriveFolderInfo> = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let mut params: Vec<(&str, String)> = vec![
            ("q", q.to_string()),
            ("fields", "nextPageToken,files(id,name,shared,ownedByMe)".to_string()),
            ("pageSize", "100".to_string()),
            ("orderBy", "name".to_string()),
            ("corpora", "user".to_string()),
            ("supportsAllDrives", "true".to_string()),
            ("includeItemsFromAllDrives", "true".to_string()),
        ];
        if let Some(tok) = &page_token {
            params.push(("pageToken", tok.clone()));
        }
        let url = url::Url::parse_with_params(DRIVE_FILES_ENDPOINT, &params)
            .context("building folder-list URL")?;
        let resp = client
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await
            .context("folder-list request")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{status}: {text}");
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing folder-list response: {text}"))?;
        if let Some(arr) = v["files"].as_array() {
            for f in arr {
                let Some(id) = f["id"].as_str() else { continue };
                let name = f["name"].as_str().unwrap_or(id);
                let shared = f["ownedByMe"].as_bool() == Some(false) || f["shared"].as_bool() == Some(true);
                folders.push(DriveFolderInfo {
                    id: id.to_string(),
                    name: name.to_string(),
                    shared,
                });
                if folders.len() >= max {
                    return Ok(folders);
                }
            }
        }
        match v["nextPageToken"].as_str() {
            Some(t) if !t.is_empty() => page_token = Some(t.to_string()),
            _ => break,
        }
    }
    Ok(folders)
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
    fn auth_url_contains_required_params() {
        let url = auth_url("cid.apps", "http://127.0.0.1:5000", "scope-x", "chal").unwrap();
        assert!(url.starts_with(AUTH_ENDPOINT));
        assert!(url.contains("client_id=cid.apps"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        // redirect_uri and scope are percent-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5000"));
        assert!(url.contains("scope=scope-x"));
    }
}
