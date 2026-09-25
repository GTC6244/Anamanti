//! Selectable-model catalog for the settings model dropdown.
//!
//! The device settings screen (and the web config page) let the user pick a
//! *specific* Anthropic or OpenAI chat model. This module produces that list,
//! scoped to models released in the **last 12 months**:
//!
//! - **Live**, by querying each provider's `GET /v1/models` when its API key is
//!   present (Anthropic exposes `created_at` as RFC3339; OpenAI exposes `created`
//!   as a unix timestamp), filtered to `created >= now - 365d`.
//! - **Static fallback** to a curated built-in list when a key is missing or the
//!   API call fails/offline, so the dropdown is always populated. The static list
//!   is best-effort and is superseded by the live data whenever it is available.
//!
//! Results are cached briefly so opening the settings screen repeatedly does not
//! re-hit the provider APIs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::Mutex;

use super::anthropic_auth::{apply_auth, AnthropicAuth, AnthropicTokenProvider};

/// One selectable model for the dropdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelInfo {
    /// `anthropic` or `openai`.
    pub provider: String,
    /// The model id sent as `llm_model` (e.g. `claude-opus-5`, `gpt-4o-mini`).
    pub id: String,
    /// A human-friendly label for the dropdown (falls back to `id`).
    pub label: String,
}

impl ModelInfo {
    fn new(provider: &str, id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            provider: provider.to_string(),
            id: id.into(),
            label: label.into(),
        }
    }
}

/// How long a fetched catalog is reused before re-querying the provider APIs.
const CACHE_TTL: Duration = Duration::from_secs(3600);
/// The "last 12 months" window, in seconds.
const TWELVE_MONTHS_SECS: i64 = 365 * 24 * 60 * 60;

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Fetches + caches the selectable model list from the provider APIs, with a
/// curated static fallback. Shared (behind an `Arc`) by the device control path
/// and the web config page.
pub struct ModelCatalog {
    client: reqwest::Client,
    anthropic_base_url: String,
    anthropic_api_key: Option<String>,
    anthropic_auth: AnthropicAuth,
    anthropic_token: Option<Arc<AnthropicTokenProvider>>,
    openai_base_url: String,
    openai_api_key: Option<String>,
    cache: Mutex<Option<(Instant, Vec<ModelInfo>)>>,
}

impl ModelCatalog {
    pub fn new(
        anthropic_base_url: impl Into<String>,
        anthropic_api_key: Option<String>,
        openai_base_url: impl Into<String>,
        openai_api_key: Option<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            anthropic_base_url: anthropic_base_url.into().trim_end_matches('/').to_string(),
            anthropic_api_key: anthropic_api_key.filter(|k| !k.is_empty()),
            anthropic_auth: AnthropicAuth::ApiKey,
            anthropic_token: None,
            openai_base_url: openai_base_url.into().trim_end_matches('/').to_string(),
            openai_api_key: openai_api_key.filter(|k| !k.is_empty()),
            cache: Mutex::new(None),
        }
    }

    /// Select the Anthropic auth mode for model listing (subscription needs the
    /// token provider). Defaults to API-key mode.
    pub fn with_anthropic_auth(
        mut self,
        auth: AnthropicAuth,
        token: Option<Arc<AnthropicTokenProvider>>,
    ) -> Self {
        self.anthropic_auth = auth;
        self.anthropic_token = token;
        self
    }

    /// The current selectable models (Anthropic then OpenAI, newest-first within
    /// each provider). Served from the cache when fresh; otherwise fetched live
    /// (falling back to the static list per provider on any failure).
    pub async fn models(&self) -> Vec<ModelInfo> {
        {
            let cache = self.cache.lock().await;
            if let Some((at, models)) = cache.as_ref() {
                if at.elapsed() < CACHE_TTL {
                    return models.clone();
                }
            }
        }

        let now = chrono::Utc::now().timestamp();
        let (anthropic, openai) = tokio::join!(self.anthropic(now), self.openai(now));
        let mut models = anthropic;
        models.extend(openai);

        let mut cache = self.cache.lock().await;
        *cache = Some((Instant::now(), models.clone()));
        models
    }

    /// Anthropic models from the last 12 months; static fallback on any failure.
    async fn anthropic(&self, now: i64) -> Vec<ModelInfo> {
        // Resolve credentials per auth mode; without them, serve the fallback list.
        let bearer = match self.anthropic_auth {
            AnthropicAuth::ApiKey => {
                if self.anthropic_api_key.is_none() {
                    return anthropic_fallback();
                }
                None
            }
            AnthropicAuth::Subscription => match &self.anthropic_token {
                None => return anthropic_fallback(),
                Some(tp) => match tp.bearer().await {
                    Ok(b) => Some(b),
                    Err(e) => {
                        log::warn!("Anthropic subscription token unavailable ({e:#}); using static fallback");
                        return anthropic_fallback();
                    }
                },
            },
        };
        match self.fetch_anthropic(bearer.as_deref(), now).await {
            Ok(models) if !models.is_empty() => models,
            Ok(_) => anthropic_fallback(),
            Err(e) => {
                log::warn!("listing Anthropic models failed ({e:#}); using static fallback");
                anthropic_fallback()
            }
        }
    }

    async fn fetch_anthropic(
        &self,
        bearer: Option<&str>,
        now: i64,
    ) -> anyhow::Result<Vec<ModelInfo>> {
        let api_key = self.anthropic_api_key.as_deref().unwrap_or_default();
        let req = self
            .client
            .get(format!("{}/v1/models?limit=1000", self.anthropic_base_url))
            .header("anthropic-version", ANTHROPIC_VERSION);
        let v: serde_json::Value = apply_auth(req, self.anthropic_auth, api_key, bearer)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let cutoff = now - TWELVE_MONTHS_SECS;
        let mut out: Vec<(i64, ModelInfo)> = Vec::new();
        for m in v
            .get("data")
            .and_then(|d| d.as_array())
            .into_iter()
            .flatten()
        {
            let Some(id) = m.get("id").and_then(|x| x.as_str()) else {
                continue;
            };
            // `created_at` is RFC3339; keep only models within the window. Undateable
            // entries are kept defensively rather than silently dropped.
            let created = m
                .get("created_at")
                .and_then(|x| x.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.timestamp());
            if matches!(created, Some(ts) if ts < cutoff) {
                continue;
            }
            let label = m.get("display_name").and_then(|x| x.as_str()).unwrap_or(id);
            out.push((
                created.unwrap_or(now),
                ModelInfo::new("anthropic", id, label),
            ));
        }
        out.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
        Ok(out.into_iter().map(|(_, m)| m).collect())
    }

    /// OpenAI chat models from the last 12 months; static fallback on any failure.
    async fn openai(&self, now: i64) -> Vec<ModelInfo> {
        let Some(key) = self.openai_api_key.as_deref() else {
            return openai_fallback();
        };
        match self.fetch_openai(key, now).await {
            Ok(models) if !models.is_empty() => models,
            Ok(_) => openai_fallback(),
            Err(e) => {
                log::warn!("listing OpenAI models failed ({e:#}); using static fallback");
                openai_fallback()
            }
        }
    }

    async fn fetch_openai(&self, key: &str, now: i64) -> anyhow::Result<Vec<ModelInfo>> {
        let v: serde_json::Value = self
            .client
            .get(format!("{}/v1/models", self.openai_base_url))
            .header("authorization", format!("Bearer {key}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let cutoff = now - TWELVE_MONTHS_SECS;
        let mut out: Vec<(i64, ModelInfo)> = Vec::new();
        for m in v
            .get("data")
            .and_then(|d| d.as_array())
            .into_iter()
            .flatten()
        {
            let Some(id) = m.get("id").and_then(|x| x.as_str()) else {
                continue;
            };
            if !is_openai_chat_model(id) {
                continue;
            }
            let created = m.get("created").and_then(|x| x.as_i64());
            if matches!(created, Some(ts) if ts < cutoff) {
                continue;
            }
            out.push((created.unwrap_or(now), ModelInfo::new("openai", id, id)));
        }
        out.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
        Ok(out.into_iter().map(|(_, m)| m).collect())
    }
}

/// Whether an OpenAI model id is a text chat model (as opposed to an embedding,
/// audio, image, moderation, or realtime model that must not appear in the
/// assistant's chat-model dropdown).
fn is_openai_chat_model(id: &str) -> bool {
    let id = id.to_lowercase();
    const EXCLUDE: &[&str] = &[
        "embedding",
        "tts",
        "whisper",
        "audio",
        "realtime",
        "image",
        "dall-e",
        "dalle",
        "moderation",
        "transcribe",
    ];
    if EXCLUDE.iter().any(|bad| id.contains(bad)) {
        return false;
    }
    // gpt-*, chatgpt-*, or an o-series id (o1 / o3 / o4-mini …).
    id.starts_with("gpt")
        || id.starts_with("chatgpt")
        || (id.starts_with('o') && id[1..].chars().next().is_some_and(|c| c.is_ascii_digit()))
}

/// Curated Anthropic models from roughly the last 12 months. Best-effort; the live
/// list supersedes this whenever the API is reachable.
fn anthropic_fallback() -> Vec<ModelInfo> {
    [
        ("claude-opus-5", "Claude Opus 5"),
        ("claude-opus-4-8", "Claude Opus 4.8"),
        ("claude-opus-4-7", "Claude Opus 4.7"),
        ("claude-opus-4-6", "Claude Opus 4.6"),
        ("claude-sonnet-5", "Claude Sonnet 5"),
        ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
        ("claude-haiku-4-5", "Claude Haiku 4.5"),
        ("claude-fable-5-1", "Claude Fable 5.1"),
        ("claude-fable-5", "Claude Fable 5"),
    ]
    .into_iter()
    .map(|(id, label)| ModelInfo::new("anthropic", id, label))
    .collect()
}

/// Curated OpenAI chat models from roughly the last 12 months. Best-effort; the
/// live list supersedes this whenever the API is reachable.
fn openai_fallback() -> Vec<ModelInfo> {
    [
        "gpt-4o",
        "gpt-4o-mini",
        "gpt-4.1",
        "gpt-4.1-mini",
        "o4-mini",
        "o3",
    ]
    .into_iter()
    .map(|id| ModelInfo::new("openai", id, id))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_chat_filter_keeps_chat_models_and_drops_the_rest() {
        assert!(is_openai_chat_model("gpt-4o"));
        assert!(is_openai_chat_model("gpt-4.1-mini"));
        assert!(is_openai_chat_model("o3"));
        assert!(is_openai_chat_model("o4-mini"));
        assert!(is_openai_chat_model("chatgpt-4o-latest"));

        assert!(!is_openai_chat_model("text-embedding-3-small"));
        assert!(!is_openai_chat_model("gpt-4o-mini-tts"));
        assert!(!is_openai_chat_model("whisper-1"));
        assert!(!is_openai_chat_model("dall-e-3"));
        assert!(!is_openai_chat_model("omni-moderation-latest"));
        assert!(!is_openai_chat_model("gpt-4o-realtime-preview"));
    }

    #[tokio::test]
    async fn anthropic_live_filters_to_last_12_months_newest_first() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // now = 2026-09-18; cutoff = 2025-09-18. `old` is out, the two recent are in.
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-18T00:00:00Z")
            .unwrap()
            .timestamp();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let body = serde_json::json!({
                "data": [
                    {"id": "claude-old", "display_name": "Old", "created_at": "2024-01-01T00:00:00Z"},
                    {"id": "claude-newer", "display_name": "Newer", "created_at": "2026-05-01T00:00:00Z"},
                    {"id": "claude-newest", "display_name": "Newest", "created_at": "2026-08-01T00:00:00Z"},
                ]
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });

        let catalog = ModelCatalog::new(
            format!("http://{addr}"),
            Some("k".into()),
            "http://unused",
            None,
        );
        let models = catalog.fetch_anthropic(None, now).await.unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["claude-newest", "claude-newer"]);
    }

    #[tokio::test]
    async fn missing_keys_yield_static_fallbacks() {
        let catalog = ModelCatalog::new("http://unused", None, "http://unused", None);
        let models = catalog.models().await;
        assert!(models.iter().any(|m| m.id == "claude-opus-5"));
        assert!(models.iter().any(|m| m.id == "gpt-4o-mini"));
        // Anthropic entries precede OpenAI entries.
        let first_openai = models.iter().position(|m| m.provider == "openai").unwrap();
        let last_anthropic = models
            .iter()
            .rposition(|m| m.provider == "anthropic")
            .unwrap();
        assert!(last_anthropic < first_openai);
    }
}
