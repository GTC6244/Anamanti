//! Anthropic authentication mode: **API key** or **subscription (OAuth)**.
//!
//! The API-key path sends `x-api-key` (from `ANTHROPIC_API_KEY`). The subscription
//! path uses a Claude Pro/Max **OAuth** token — `Authorization: Bearer <token>` plus
//! the `anthropic-beta: oauth-2025-04-20` header (per the claude-api skill's auth
//! reference) and *no* `x-api-key`.
//!
//! The subscription token is resolved by [`AnthropicTokenProvider`], in order:
//!   1. `ANTHROPIC_OAUTH_TOKEN` — a long-lived token from `claude setup-token`
//!      (the `claude` CLI's subscription token). Simplest; no per-request work.
//!   2. `AMBIENT_ANTHROPIC_TOKEN_CMD` — a command that prints a fresh access token
//!      to stdout (default `ant auth print-credentials --access-token`, for hosts
//!      with the `ant` CLI logged in). Cached briefly and refreshed on a 401.
//!
//! Both the chat backend ([`crate::llm::anthropic`]) and the model catalog
//! ([`crate::llm::catalog`]) authenticate through here so the two never drift.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// The `anthropic-beta` value required for OAuth (subscription) tokens.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

/// How long a fetched subscription token is reused before re-querying `ant`.
const TOKEN_TTL: Duration = Duration::from_secs(300);

/// Which credential the Anthropic backend/catalog authenticates with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AnthropicAuth {
    /// `x-api-key` from `ANTHROPIC_API_KEY` (default).
    #[default]
    ApiKey,
    /// Claude subscription OAuth (`Authorization: Bearer` + `anthropic-beta`).
    Subscription,
}

impl AnthropicAuth {
    /// Canonical lowercase label (persisted / reported to the UI).
    pub fn as_str(self) -> &'static str {
        match self {
            AnthropicAuth::ApiKey => "apikey",
            AnthropicAuth::Subscription => "subscription",
        }
    }

    /// Parse a label; anything other than `subscription`/`oauth` is `ApiKey`.
    pub fn from_label(label: &str) -> Self {
        match label.to_lowercase().as_str() {
            "subscription" | "oauth" | "sub" => AnthropicAuth::Subscription,
            _ => AnthropicAuth::ApiKey,
        }
    }
}

/// Apply the correct Anthropic auth headers to a request builder. `bearer` is only
/// consulted in [`AnthropicAuth::Subscription`] mode.
pub fn apply_auth(
    req: reqwest::RequestBuilder,
    auth: AnthropicAuth,
    api_key: &str,
    bearer: Option<&str>,
) -> reqwest::RequestBuilder {
    match auth {
        AnthropicAuth::ApiKey => req.header("x-api-key", api_key),
        AnthropicAuth::Subscription => req
            .header(
                "authorization",
                format!("Bearer {}", bearer.unwrap_or_default()),
            )
            .header("anthropic-beta", OAUTH_BETA),
    }
}

/// Fetches + caches a subscription OAuth access token via the `ant` CLI. The fetch
/// command is injectable so tests never depend on a real `ant` install.
pub struct AnthropicTokenProvider {
    fetch: Arc<dyn Fn() -> Result<String> + Send + Sync>,
    cache: Mutex<Option<(Instant, String)>>,
}

impl AnthropicTokenProvider {
    /// Production provider: env token, else a token-printing CLI.
    pub fn new() -> Self {
        Self::with_fetcher(Arc::new(default_fetch_token))
    }

    /// Provider with a custom token fetcher (tests).
    pub fn with_fetcher(fetch: Arc<dyn Fn() -> Result<String> + Send + Sync>) -> Self {
        Self {
            fetch,
            cache: Mutex::new(None),
        }
    }

    /// A valid Bearer token, served from cache when fresh or fetched via `ant`. The
    /// subprocess runs on a blocking thread so it never stalls the async runtime.
    pub async fn bearer(&self) -> Result<String> {
        {
            let cache = self.cache.lock().unwrap();
            if let Some((at, tok)) = cache.as_ref() {
                if at.elapsed() < TOKEN_TTL {
                    return Ok(tok.clone());
                }
            }
        }
        let fetch = self.fetch.clone();
        let tok = tokio::task::spawn_blocking(move || fetch())
            .await
            .context("subscription token fetch task")??;
        // Strip *all* whitespace (OAuth tokens contain none), so a value that got
        // line-wrapped in a dotfile / has an embedded newline still yields a valid
        // `Authorization` header rather than a "failed to parse header value" error.
        let tok: String = tok.split_whitespace().collect();
        if tok.is_empty() {
            anyhow::bail!(
                "no subscription token available — set ANTHROPIC_OAUTH_TOKEN (run `claude setup-token`) on the orchestrator host"
            );
        }
        *self.cache.lock().unwrap() = Some((Instant::now(), tok.clone()));
        Ok(tok)
    }

    /// Drop the cached token so the next [`Self::bearer`] re-fetches (used after a 401).
    pub fn invalidate(&self) {
        *self.cache.lock().unwrap() = None;
    }
}

impl Default for AnthropicTokenProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve a subscription token: a long-lived env token first (from
/// `claude setup-token`), else a configurable token-printing CLI.
fn default_fetch_token() -> Result<String> {
    // 1. A long-lived OAuth token supplied directly (e.g. `claude setup-token`).
    if let Ok(tok) = std::env::var("ANTHROPIC_OAUTH_TOKEN") {
        if !tok.trim().is_empty() {
            return Ok(tok);
        }
    }
    // 2. A command that prints a fresh access token (default: the `ant` CLI).
    let cmd = std::env::var("AMBIENT_ANTHROPIC_TOKEN_CMD")
        .unwrap_or_else(|_| "ant auth print-credentials --access-token".to_string());
    let mut parts = cmd.split_whitespace();
    let program = parts
        .next()
        .context("AMBIENT_ANTHROPIC_TOKEN_CMD is empty")?;
    let args: Vec<&str> = parts.collect();
    let out = std::process::Command::new(program)
        .args(&args)
        .output()
        .with_context(|| format!("running `{cmd}` for a subscription token"))?;
    if !out.status.success() {
        anyhow::bail!(
            "could not obtain an Anthropic subscription token. Set ANTHROPIC_OAUTH_TOKEN \
             (run `claude setup-token`) or make `{cmd}` work. stderr: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn label_round_trips() {
        assert_eq!(
            AnthropicAuth::from_label("subscription"),
            AnthropicAuth::Subscription
        );
        assert_eq!(
            AnthropicAuth::from_label("oauth"),
            AnthropicAuth::Subscription
        );
        assert_eq!(AnthropicAuth::from_label("apikey"), AnthropicAuth::ApiKey);
        assert_eq!(AnthropicAuth::from_label("anything"), AnthropicAuth::ApiKey);
        assert_eq!(AnthropicAuth::Subscription.as_str(), "subscription");
        assert_eq!(AnthropicAuth::ApiKey.as_str(), "apikey");
    }

    #[tokio::test]
    async fn caches_token_and_refetches_after_invalidate() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let provider = AnthropicTokenProvider::with_fetcher(Arc::new(move || {
            let n = c.fetch_add(1, Ordering::SeqCst);
            Ok(format!("  tok-{n}\n")) // whitespace is trimmed
        }));

        assert_eq!(provider.bearer().await.unwrap(), "tok-0");
        assert_eq!(provider.bearer().await.unwrap(), "tok-0"); // cached, no re-fetch
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        provider.invalidate();
        assert_eq!(provider.bearer().await.unwrap(), "tok-1"); // re-fetched
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn empty_token_is_an_error() {
        let provider = AnthropicTokenProvider::with_fetcher(Arc::new(|| Ok("   \n".to_string())));
        assert!(provider.bearer().await.is_err());
    }

    #[test]
    fn subscription_headers_use_bearer_and_beta() {
        // A lightweight structural check that apply_auth compiles + selects headers.
        let client = reqwest::Client::new();
        let _api = apply_auth(client.get("http://x"), AnthropicAuth::ApiKey, "k", None);
        let _sub = apply_auth(
            client.get("http://x"),
            AnthropicAuth::Subscription,
            "",
            Some("tok"),
        );
    }
}
