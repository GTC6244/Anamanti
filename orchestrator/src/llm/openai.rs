//! Cloud [`LlmBackend`] backed by the **OpenAI Chat Completions API**.
//!
//! Symmetric with [`crate::llm::anthropic`]: Rust has no official OpenAI SDK, so
//! this talks raw HTTP to `POST /v1/chat/completions` with `stream: true`. The
//! response is Server-Sent Events; the reply tokens ride on `choices[0].delta.content`
//! which map onto [`ReplyStream`]. Used when the model selector picks a specific
//! OpenAI (GPT / o-series) model.

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::json;

use super::{line_stream, LlmBackend, LlmTurn, ReplyStream};

/// Config + HTTP client for the OpenAI Chat Completions API.
pub struct OpenAiBackend {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl OpenAiBackend {
    /// `model` defaults to `gpt-4o-mini` at the config layer. `max_tokens` is kept
    /// modest for spoken replies. `base_url` is the API root
    /// (`https://api.openai.com`), overridable for testing.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        max_tokens: u32,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            max_tokens,
        }
    }
}

#[async_trait]
impl LlmBackend for OpenAiBackend {
    fn name(&self) -> &str {
        "openai"
    }

    async fn respond(&self, turn: LlmTurn) -> Result<ReplyStream> {
        // `max_completion_tokens` is the current field for Chat Completions (the
        // older `max_tokens` is deprecated and rejected by o-series models).
        let body = json!({
            "model": self.model,
            "max_completion_tokens": self.max_tokens,
            "stream": true,
            "messages": [
                {"role": "system", "content": turn.system_prompt},
                {"role": "user", "content": turn.user_message},
            ],
        });

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("POST /v1/chat/completions to OpenAI")?
            .error_for_status()
            .context("OpenAI returned an error status")?;

        let lines = line_stream(resp.bytes_stream());
        let tokens = async_stream::try_stream! {
            futures_util::pin_mut!(lines);
            while let Some(line) = lines.next().await {
                let line = line?;
                // SSE: only `data:` payload lines carry content; `event:` lines and
                // blank separators are ignored.
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() {
                    continue;
                }
                if payload == "[DONE]" {
                    break;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
                    continue;
                };
                if let Some(err) = v.get("error") {
                    let msg = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown OpenAI stream error");
                    Err(anyhow::anyhow!("OpenAI stream error: {msg}"))?;
                }
                if let Some(text) = v
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("delta"))
                    .and_then(|d| d.get("content"))
                    .and_then(|t| t.as_str())
                {
                    if !text.is_empty() {
                        yield text.to_string();
                    }
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
    async fn parses_sse_choice_deltas() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;

            let body =
                "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\"Sure\"}}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\", done.\"}}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                        data: [DONE]\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });

        let backend = OpenAiBackend::new(format!("http://{addr}"), "k", "gpt-4o-mini", 256);
        let stream = backend.respond(LlmTurn::new("sys", "hi")).await.unwrap();
        assert_eq!(collect_reply(stream).await.unwrap(), "Sure, done.");
        server.await.unwrap();
    }
}
