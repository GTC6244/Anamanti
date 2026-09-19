//! Cloud [`LlmBackend`] backed by the **Claude Messages API**.
//!
//! Rust has no official Anthropic SDK, so this talks raw HTTP to
//! `POST /v1/messages` with `stream: true` (per the claude-api skill's cURL
//! reference). The response is Server-Sent Events; the reply tokens ride on
//! `content_block_delta` events as `delta.text`, which map onto [`ReplyStream`].

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};

use super::anthropic_auth::{apply_auth, AnthropicAuth, AnthropicTokenProvider};
use super::{line_stream, LlmBackend, LlmTurn, ReplyStream};

/// Anthropic API version pinned per the claude-api skill.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Config + HTTP client for the Claude Messages API. Authenticates with either an
/// API key (`x-api-key`) or a Claude subscription OAuth token (`Authorization:
/// Bearer` + `anthropic-beta`), selected by [`AnthropicAuth`].
pub struct AnthropicBackend {
    client: reqwest::Client,
    base_url: String,
    auth: AnthropicAuth,
    /// API key (used in `ApiKey` mode; empty in subscription mode).
    api_key: String,
    /// Subscription token source (used in `Subscription` mode).
    token: Option<Arc<AnthropicTokenProvider>>,
    model: String,
    max_tokens: u32,
}

impl AnthropicBackend {
    /// API-key backend. `model` defaults to `claude-opus-5` at the config layer;
    /// `max_tokens` is kept modest for spoken replies; `base_url` is the API root
    /// (`https://api.anthropic.com`), overridable for testing.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        max_tokens: u32,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth: AnthropicAuth::ApiKey,
            api_key: api_key.into(),
            token: None,
            model: model.into(),
            max_tokens,
        }
    }

    /// Subscription (OAuth) backend: tokens come from `token` (the `ant` CLI).
    pub fn with_subscription(
        base_url: impl Into<String>,
        token: Arc<AnthropicTokenProvider>,
        model: impl Into<String>,
        max_tokens: u32,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth: AnthropicAuth::Subscription,
            api_key: String::new(),
            token: Some(token),
            model: model.into(),
            max_tokens,
        }
    }

    /// Send one Messages request with the current auth applied. `bearer` is the
    /// subscription token when in OAuth mode.
    async fn post(&self, body: &Value, bearer: Option<&str>) -> reqwest::Result<reqwest::Response> {
        let req = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        apply_auth(req, self.auth, &self.api_key, bearer)
            .json(body)
            .send()
            .await
    }
}

#[async_trait]
impl LlmBackend for AnthropicBackend {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn respond(&self, turn: LlmTurn) -> Result<ReplyStream> {
        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "stream": true,
            "system": turn.system_prompt,
            "messages": [{"role": "user", "content": turn.user_message}],
        });

        // In subscription mode, mint/refresh a Bearer token; on a 401 (expired
        // token) invalidate the cache and retry once with a fresh token.
        let mut bearer = match self.auth {
            AnthropicAuth::Subscription => Some(
                self.token
                    .as_ref()
                    .context("subscription auth selected but no token provider is wired")?
                    .bearer()
                    .await?,
            ),
            AnthropicAuth::ApiKey => None,
        };

        let mut resp = self
            .post(&body, bearer.as_deref())
            .await
            .context("POST /v1/messages to Anthropic")?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            && self.auth == AnthropicAuth::Subscription
        {
            if let Some(tp) = &self.token {
                tp.invalidate();
                bearer = Some(tp.bearer().await?);
                resp = self
                    .post(&body, bearer.as_deref())
                    .await
                    .context("POST /v1/messages to Anthropic (after token refresh)")?;
            }
        }
        let resp = resp
            .error_for_status()
            .context("Anthropic returned an error status")?;

        let lines = line_stream(resp.bytes_stream());
        let tokens = async_stream::try_stream! {
            futures_util::pin_mut!(lines);
            while let Some(line) = lines.next().await {
                let line = line?;
                // SSE: we only care about `data:` payload lines; `event:` lines
                // and blank separators are ignored.
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() || payload == "[DONE]" {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
                    continue;
                };
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("content_block_delta") => {
                        if let Some(text) = v
                            .get("delta")
                            .and_then(|d| d.get("text"))
                            .and_then(|t| t.as_str())
                        {
                            if !text.is_empty() {
                                yield text.to_string();
                            }
                        }
                    }
                    Some("error") => {
                        let msg = v
                            .get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown Anthropic stream error");
                        Err(anyhow::anyhow!("Anthropic stream error: {msg}"))?;
                    }
                    Some("message_stop") => break,
                    _ => {}
                }
            }
        };

        Ok(Box::pin(tokens))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::collect_reply;

    /// Replay a recorded SSE stream from a local socket to exercise the parser
    /// without calling the real API.
    #[tokio::test]
    async fn parses_sse_content_block_deltas() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;

            let body = "event: message_start\n\
                        data: {\"type\":\"message_start\"}\n\n\
                        event: content_block_delta\n\
                        data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Sure\"}}\n\n\
                        event: content_block_delta\n\
                        data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\", done.\"}}\n\n\
                        event: message_stop\n\
                        data: {\"type\":\"message_stop\"}\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });

        let backend = AnthropicBackend::new(format!("http://{addr}"), "k", "claude-opus-5", 256);
        let stream = backend.respond(LlmTurn::new("sys", "hi")).await.unwrap();
        assert_eq!(collect_reply(stream).await.unwrap(), "Sure, done.");
        server.await.unwrap();
    }
}
