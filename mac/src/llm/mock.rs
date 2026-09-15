//! A dependency-free, deterministic [`LlmBackend`] used for offline runs and
//! tests. It echoes a canned reply word-by-word so the full STT → LLM → TTS
//! pipeline can be exercised without a local model server or a network call.

use anyhow::Result;
use async_trait::async_trait;
use futures_util::stream;

use super::{LlmBackend, LlmTurn, ReplyStream};

/// Streams a fixed reply (default: a short acknowledgement that quotes the user)
/// as individual word tokens, proving token-by-token streaming end to end.
pub struct MockLlm {
    template: String,
}

impl MockLlm {
    /// `template` may contain `{msg}`, replaced by the user's transcript.
    pub fn new(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
        }
    }
}

impl Default for MockLlm {
    fn default() -> Self {
        Self::new("You said: {msg}")
    }
}

#[async_trait]
impl LlmBackend for MockLlm {
    fn name(&self) -> &str {
        "mock"
    }

    async fn respond(&self, turn: LlmTurn) -> Result<ReplyStream> {
        let reply = self.template.replace("{msg}", turn.user_message.trim());
        // Split into words but keep the spaces so the concatenation is faithful.
        let mut tokens: Vec<Result<String>> = Vec::new();
        for (i, word) in reply.split(' ').enumerate() {
            if i > 0 {
                tokens.push(Ok(" ".to_string()));
            }
            tokens.push(Ok(word.to_string()));
        }
        Ok(Box::pin(stream::iter(tokens)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::collect_reply;

    #[tokio::test]
    async fn echoes_the_user_message_token_by_token() {
        let llm = MockLlm::default();
        let stream = llm
            .respond(LlmTurn::new("sys", "turn on the lights"))
            .await
            .unwrap();
        assert_eq!(
            collect_reply(stream).await.unwrap(),
            "You said: turn on the lights"
        );
    }
}
