//! Pluggable LLM orchestrator (Plan.MD Phase 4, bullet 2; §5 "LLM abstraction").
//!
//! Every backend implements one trait — [`LlmBackend`] — whose `respond` returns a
//! **stream of reply tokens**. The pipeline (`orchestrator.rs`) never names a
//! concrete backend: it is handed a `Box<dyn LlmBackend>` chosen from config, so
//! local (Ollama / llama.cpp) and cloud (Claude) are swappable without touching
//! the turn logic. This is the locked "pluggable behind a trait" decision.

pub mod anthropic;
pub mod anthropic_auth;
pub mod catalog;
pub mod mock;
pub mod ollama;
pub mod openai;
#[cfg(feature = "rig")]
pub mod rig;

use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};

/// A streamed reply: reply-token fragments in arrival order. Fragments concatenate
/// to the full reply text; each is rendered on the device as it lands (Phase 5)
/// and the concatenation is sent to Piper for synthesis (Phase 4).
pub type ReplyStream = Pin<Box<dyn Stream<Item = Result<String>> + Send>>;

/// One request to a backend: a system prompt (which carries the persistent-memory
/// context) plus the user's transcript for this turn. Kept single-turn for v1;
/// cross-session continuity comes from the memory store, not a message history.
#[derive(Debug, Clone)]
pub struct LlmTurn {
    pub system_prompt: String,
    pub user_message: String,
}

impl LlmTurn {
    pub fn new(system_prompt: impl Into<String>, user_message: impl Into<String>) -> Self {
        Self {
            system_prompt: system_prompt.into(),
            user_message: user_message.into(),
        }
    }
}

/// A swappable LLM backend. Implementations are `Send + Sync` so one instance is
/// shared across concurrent turns behind an `Arc`.
#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// A short backend label for logs/settings (e.g. `"ollama"`, `"anthropic"`).
    fn name(&self) -> &str;

    /// Produce a streaming reply to `turn`. Errors surface either here (request
    /// setup failed) or as an `Err` item inside the stream (mid-stream failure).
    async fn respond(&self, turn: LlmTurn) -> Result<ReplyStream>;
}

/// Drain a [`ReplyStream`] into the complete reply text. Used when a caller only
/// needs the final string (e.g. TTS synthesis, or tests).
pub async fn collect_reply(mut stream: ReplyStream) -> Result<String> {
    let mut out = String::new();
    while let Some(token) = stream.next().await {
        out.push_str(&token?);
    }
    Ok(out)
}

/// Turn a byte stream (an HTTP response body) into a stream of complete text lines,
/// splitting on `\n`. Both streaming LLM wire formats used here are line-oriented:
/// Ollama emits newline-delimited JSON, Claude emits SSE (`event:`/`data:` lines).
/// A trailing partial line (no terminating `\n`) is yielded at end-of-stream.
pub(crate) fn line_stream(
    body: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
) -> impl Stream<Item = Result<String>> + Send + 'static {
    async_stream::try_stream! {
        let mut buf: Vec<u8> = Vec::new();
        futures_util::pin_mut!(body);
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            buf.extend_from_slice(&chunk);
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                // Drop the trailing '\n' (and a '\r' if present).
                let end = line.len().saturating_sub(1);
                let end = if end > 0 && line[end - 1] == b'\r' { end - 1 } else { end };
                yield String::from_utf8_lossy(&line[..end]).into_owned();
            }
        }
        if !buf.is_empty() {
            yield String::from_utf8_lossy(&buf).into_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn line_stream_splits_on_newlines_including_trailing_partial() {
        let chunks = vec![
            Ok(bytes::Bytes::from_static(b"one\ntw")),
            Ok(bytes::Bytes::from_static(b"o\r\nthree")),
        ];
        let body = futures_util::stream::iter(chunks);
        let lines: Vec<String> = line_stream(body)
            .map(|l| l.unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(lines, vec!["one", "two", "three"]);
    }

    #[tokio::test]
    async fn collect_reply_concatenates_tokens() {
        let stream: ReplyStream = Box::pin(futures_util::stream::iter(vec![
            Ok("Hello".to_string()),
            Ok(", ".to_string()),
            Ok("world".to_string()),
        ]));
        assert_eq!(collect_reply(stream).await.unwrap(), "Hello, world");
    }
}
