//! One-time Google Drive OAuth **consent**, run inside the orchestrator.
//!
//! This replaces the standalone `tools/google_photo_consent.py`: the orchestrator
//! now performs the consent itself, driven from the config page (see
//! [`crate::webconfig`]). The flow is OAuth 2.0 authorization-code + PKCE with a
//! **loopback** redirect (`127.0.0.1:<ephemeral>`), which requires a Google Cloud
//! OAuth client of type **"Desktop app"** (the TV/Ambient client can't do loopback).
//! The shared loopback mechanics (PKCE, `state`, the redirect listener, token parsing)
//! live in [`crate::oauth_loopback`]; Google takes the client credentials as form
//! fields ([`ClientAuth::Form`]).
//!
//! The resulting refresh token is stored in the orchestrator's settings
//! ([`crate::settings::DriveConfig`]); the device pulls it (plus the client
//! credentials and folder ids) over Wyoming (`ambient-get-drive-token`) and mints
//! Drive access tokens on-device. Consent runs once on the Mac (real browser +
//! keyboard); tokens then live on the orchestrator and the device.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::TcpListener;

use crate::oauth_loopback::{self, ClientAuth, TokenEndpoint};

const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const DRIVE_FILES_ENDPOINT: &str = "https://www.googleapis.com/drive/v3/files";

/// `User-Agent` component label for Drive requests.
const COMPONENT: &str = "drive";

/// Budget for the one-shot folder verification call.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for each folder-list page.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// The result of a successful consent: a long-lived refresh token, an optional
/// first access token (usable immediately to verify folders), and the granted scope.
#[derive(Debug, Clone)]
pub struct ConsentOutcome {
    pub refresh_token: String,
    pub access_token: Option<String>,
    pub scope: String,
}

fn token_endpoint<'a>(client_id: &'a str, client_secret: &'a str) -> TokenEndpoint<'a> {
    TokenEndpoint {
        url: TOKEN_ENDPOINT,
        client_id,
        client_secret,
        auth: ClientAuth::Form,
        component: COMPONENT,
    }
}

/// Build the Google authorization URL for the loopback consent request.
fn auth_url(
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    challenge: &str,
    state: &str,
) -> Result<String> {
    let mut url = url::Url::parse(AUTH_ENDPOINT).context("parsing auth endpoint")?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", scope)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url.to_string())
}

/// Run the full loopback consent flow: bind an ephemeral loopback redirect port,
/// open the browser, wait (up to `timeout`) for Google to redirect back with the
/// authorization code (and the matching `state`), then exchange it for a refresh token.
pub async fn run_consent(
    client_id: &str,
    client_secret: &str,
    scope: &str,
    timeout: Duration,
) -> Result<ConsentOutcome> {
    if client_id.is_empty() || client_secret.is_empty() {
        bail!("Drive OAuth client id/secret are not set (a 'Desktop app' client is required)");
    }
    let (verifier, challenge) = oauth_loopback::pkce_pair();
    let state = oauth_loopback::new_state();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding loopback redirect listener")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let url = auth_url(client_id, &redirect_uri, scope, &challenge, &state)?;

    oauth_loopback::open_browser(&url);
    log::info!("drive consent: approve access in the browser (or open {url})");

    let code =
        oauth_loopback::wait_for_code_within(&listener, "Google Drive", &state, timeout).await?;

    let parsed = oauth_loopback::token_request(
        &token_endpoint(client_id, client_secret),
        &[
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri.as_str()),
        ],
        "token exchange",
    )
    .await?;
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

/// GET a Drive API URL with a bearer token; a non-2xx is `"{status}: {body}"`.
async fn drive_get(
    url: url::Url,
    access_token: &str,
    timeout: Duration,
    what: &str,
) -> Result<serde_json::Value> {
    let resp = crate::http::send(
        crate::http::get(url, COMPONENT).bearer_auth(access_token),
        timeout,
        what,
    )
    .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{status}: {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("parsing {what} response: {text}"))
}

/// Count image files visible in a Drive folder, to verify a link/folder id from the
/// config page. Uses the immediate access token from consent (or a freshly minted
/// one). Returns the number of images (capped at the page size).
pub async fn verify_folder(access_token: &str, folder_id: &str) -> Result<usize> {
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
    let v = drive_get(url, access_token, VERIFY_TIMEOUT, "verify").await?;
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
    let parsed = oauth_loopback::token_request(
        &token_endpoint(client_id, client_secret),
        &[
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ],
        "token refresh",
    )
    .await?;
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
    let q = "mimeType = 'application/vnd.google-apps.folder' and trashed = false";
    let mut folders: Vec<DriveFolderInfo> = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let mut params: Vec<(&str, String)> = vec![
            ("q", q.to_string()),
            (
                "fields",
                "nextPageToken,files(id,name,shared,ownedByMe)".to_string(),
            ),
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
        let v = drive_get(url, access_token, LIST_TIMEOUT, "folder-list").await?;
        if let Some(arr) = v["files"].as_array() {
            for f in arr {
                let Some(id) = f["id"].as_str() else { continue };
                let name = f["name"].as_str().unwrap_or(id);
                let shared =
                    f["ownedByMe"].as_bool() == Some(false) || f["shared"].as_bool() == Some(true);
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
    fn auth_url_contains_required_params() {
        let url = auth_url(
            "cid.apps",
            "http://127.0.0.1:5000",
            "scope-x",
            "chal",
            "st4te",
        )
        .unwrap();
        assert!(url.starts_with(AUTH_ENDPOINT));
        assert!(url.contains("client_id=cid.apps"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("state=st4te"));
        // redirect_uri and scope are percent-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5000"));
        assert!(url.contains("scope=scope-x"));
    }

    #[test]
    fn google_sends_client_credentials_as_form_fields() {
        assert_eq!(token_endpoint("a", "b").auth, ClientAuth::Form);
    }
}
