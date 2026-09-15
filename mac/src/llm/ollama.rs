//! Local [`LlmBackend`] backed by an **Ollama** server (llama.cpp under the hood).
//! Ollama's `/api/chat` with `stream: true` returns newline-delimited JSON, one
//! object per token chunk (`{"message":{"content":"..."},"done":false}`), which
//! maps cleanly onto [`ReplyStream`].

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::json;

use super::{line_stream, LlmBackend, LlmTurn, ReplyStream};

/// Config + HTTP client for a local Ollama endpoint.
pub struct OllamaBackend {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

impl OllamaBackend {
    /// `base_url` is the Ollama root (e.g. `http://127.0.0.1:11434`).
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
        }
    }
}

#[async_trait]
impl LlmBackend for OllamaBackend {
    fn name(&self) -> &str {
        "ollama"
    }

    async fn respond(&self, turn: LlmTurn) -> Result<ReplyStream> {
        let body = json!({
            "model": self.model,
            "stream": true,
            "messages": [
                {"role": "system", "content": turn.system_prompt},
                {"role": "user", "content": turn.user_message},
            ],
        });

        let resp = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&body)
            .send()
            .await
            .context("POST /api/chat to Ollama")?
            .error_for_status()
            .context("Ollama returned an error status")?;

        let lines = line_stream(resp.bytes_stream());
        let tokens = async_stream::try_stream! {
            futures_util::pin_mut!(lines);
            while let Some(line) = lines.next().await {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                // Each line is a standalone JSON object; a parse failure on one
                // line shouldn't abort the reply, so skip unparseable lines.
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if let Some(tok) = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_str()) {
                    if !tok.is_empty() {
                        yield tok.to_string();
                    }
                }
                if v.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
                    break;
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

    /// Exercise the streaming NDJSON parse without a live Ollama by pointing the
    /// backend at a tiny local HTTP server that replays a recorded response.
    #[tokio::test]
    async fn parses_ndjson_token_stream() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Read (and ignore) the request headers/body up to a blank line — the
            // client sends a small JSON body; we don't need to parse it.
            let mut buf = [0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;

            let body = "{\"message\":{\"content\":\"Hi\"},\"done\":false}\n\
                        {\"message\":{\"content\":\" there\"},\"done\":false}\n\
                        {\"message\":{\"content\":\"\"},\"done\":true}\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });

        let backend = OllamaBackend::new(format!("http://{addr}"), "test-model");
        let stream = backend.respond(LlmTurn::new("sys", "hello")).await.unwrap();
        assert_eq!(collect_reply(stream).await.unwrap(), "Hi there");
        server.await.unwrap();
    }
}
