//! One-time Spotify OAuth **consent**, run inside the orchestrator.
//!
//! Mirrors [`crate::drive_consent`]: OAuth 2.0 authorization-code + PKCE (+ `state`)
//! with a **loopback** redirect, driven from the config page (see
//! [`crate::webconfig`]); the shared mechanics live in [`crate::oauth_loopback`].
//! Spotify authenticates the client with HTTP Basic auth ([`ClientAuth::Basic`]).
//! The resulting refresh token is stored in the orchestrator's settings
//! ([`crate::settings::SpotifyConfig`]) and drives the `spotify_control` rig tool.
//!
//! **Key difference from Google:** Spotify requires the redirect URI to be an
//! **exact, pre-registered** value — it does not allow an arbitrary loopback port.
//! So consent uses a **fixed redirect URL** (default
//! [`DEFAULT_REDIRECT_URL`], configurable via [`crate::settings::SpotifyConfig`]) and
//! the operator must add that exact value to their Spotify app's Redirect URIs. The
//! consent listener binds the URL's own host+port; if it is busy, consent fails with a
//! clear message rather than silently using a different (unregistered) port.
//!
//! Requires **Spotify Premium** to actually play, though consent itself works on
//! any account.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::TcpListener;

use crate::oauth_loopback::{self, ClientAuth, TokenEndpoint};

const AUTH_ENDPOINT: &str = "https://accounts.spotify.com/authorize";
const TOKEN_ENDPOINT: &str = "https://accounts.spotify.com/api/token";

/// Spotify authenticates the client at the token endpoint with HTTP Basic auth.
const CLIENT_AUTH: ClientAuth = ClientAuth::Basic;

/// Scopes needed to control playback and read the device list.
pub const SPOTIFY_SCOPE: &str = "user-modify-playback-state user-read-playback-state";

/// Default loopback redirect URL, used when the operator hasn't configured one
/// (`SpotifyConfig::redirect_url`). Its host+port is where the consent listener binds,
/// and this exact string must be registered in the Spotify app's Redirect URIs.
pub const DEFAULT_REDIRECT_URL: &str = "http://127.0.0.1:8888/callback";

/// The result of a successful consent: a long-lived refresh token and the granted
/// scope.
#[derive(Debug, Clone)]
pub struct ConsentOutcome {
    pub refresh_token: String,
    pub scope: String,
}

/// Build the Spotify authorization URL for the loopback consent request.
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
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", scope)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url.to_string())
}

/// Parse the loopback `redirect_url` into the `(host, port)` the consent listener
/// must bind so it actually receives Spotify's callback. The full URL string is what
/// gets sent as `redirect_uri` (it must exactly match a registered Redirect URI); this
/// only derives *where to listen*.
fn redirect_bind_target(redirect_url: &str) -> Result<(String, u16)> {
    let parsed = url::Url::parse(redirect_url)
        .with_context(|| format!("parsing Spotify redirect URL {redirect_url:?}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("redirect URL {redirect_url:?} has no host"))?
        .to_string();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow!("redirect URL {redirect_url:?} has no port"))?;
    Ok((host, port))
}

/// Run the full loopback consent flow against a **fixed** redirect URL (must match a
/// Redirect URI registered in the Spotify app): bind the loopback redirect on that
/// URL's host+port, open the browser, wait (up to `timeout`) for Spotify to redirect
/// back with the authorization code (and the matching `state`), then exchange it for a
/// refresh token.
pub async fn run_consent(
    client_id: &str,
    client_secret: &str,
    redirect_url: &str,
    scope: &str,
    timeout: Duration,
) -> Result<ConsentOutcome> {
    if client_id.is_empty() || client_secret.is_empty() {
        bail!("Spotify client id/secret are not set");
    }
    let (host, port) = redirect_bind_target(redirect_url)?;
    let (verifier, challenge) = oauth_loopback::pkce_pair();
    let state = oauth_loopback::new_state();
    let listener = TcpListener::bind((host.as_str(), port))
        .await
        .with_context(|| {
            format!(
                "binding loopback redirect on {host}:{port} (is another consent or app using it?)"
            )
        })?;
    let url = auth_url(client_id, redirect_url, scope, &challenge, &state)?;

    oauth_loopback::open_browser(&url);
    log::info!(
        "spotify consent: approve access in the browser (or open {url}); \
         redirect {redirect_url} must be registered in the Spotify app"
    );

    let code = oauth_loopback::wait_for_code_within(&listener, "Spotify", &state, timeout).await?;

    // Exchange the authorization code for tokens (Basic auth with the client id/secret,
    // plus the PKCE verifier).
    let parsed = oauth_loopback::token_request(
        &TokenEndpoint {
            url: TOKEN_ENDPOINT,
            client_id,
            client_secret,
            auth: CLIENT_AUTH,
            component: "spotify",
        },
        &[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_url),
            ("code_verifier", verifier.as_str()),
        ],
        "token exchange",
    )
    .await?;
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
    fn redirect_bind_target_parses_host_and_port() {
        assert_eq!(
            redirect_bind_target("http://127.0.0.1:8888/callback").unwrap(),
            ("127.0.0.1".to_string(), 8888)
        );
        assert_eq!(
            redirect_bind_target("http://127.0.0.1:9099/cb").unwrap(),
            ("127.0.0.1".to_string(), 9099)
        );
        // No explicit port falls back to the scheme's default.
        assert_eq!(
            redirect_bind_target("http://127.0.0.1/callback").unwrap(),
            ("127.0.0.1".to_string(), 80)
        );
        assert!(redirect_bind_target("not a url").is_err());
    }

    #[test]
    fn auth_url_contains_required_params() {
        let url = auth_url(
            "cid123",
            "http://127.0.0.1:8888/callback",
            SPOTIFY_SCOPE,
            "chal",
            "st4te",
        )
        .unwrap();
        assert!(url.starts_with(AUTH_ENDPOINT));
        assert!(url.contains("client_id=cid123"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st4te"));
        // redirect_uri + scope are percent-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8888%2Fcallback"));
        assert!(url.contains("user-modify-playback-state"));
    }

    #[test]
    fn spotify_uses_basic_client_auth() {
        assert_eq!(CLIENT_AUTH, ClientAuth::Basic);
    }
}
