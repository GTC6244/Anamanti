//! [`LlmBackend`] implemented on top of the **rig-core** agent framework
//! (selected at runtime with `llm.engine="rig"`, the default).
//!
//! This routes the same `respond(turn) -> ReplyStream` seam through rig's unified
//! `CompletionModel` trait instead of the hand-rolled HTTP clients in `ollama.rs`
//! / `anthropic.rs`. One provider is chosen at construction.
//!
//! ## Tool calling
//!
//! Unlike the native backends (plain text only), the rig path supports **tool
//! calling** over the [`tools`] registry (timers, personality, font, and every
//! configured integration). `respond` runs a short negotiation loop: if the model
//! asks to call a tool it executes it, appends the result to the conversation and
//! asks again — up to [`MAX_TOOL_ROUNDS`] rounds.

pub mod tools;

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;

use rig_core::client::completion::CompletionClient;
use rig_core::completion::{CompletionModel, ToolDefinition};
use rig_core::message::{AssistantContent, Message, ToolCall};
use rig_core::providers::{anthropic, ollama};

use super::{LlmBackend, LlmTurn, ReplyStream};
pub use tools::{ToolDeps, Tools};

/// Upper bound on tool-negotiation rounds per turn, so a model that loops on tool
/// calls can never spin forever.
const MAX_TOOL_ROUNDS: usize = 4;

/// The rig completion model for the selected provider, wrapped in `Arc` so the
/// backend is cheaply clonable into the (`'static`) reply stream.
#[derive(Clone)]
enum Model {
    Ollama(Arc<ollama::CompletionModel>),
    Anthropic(Arc<anthropic::completion::CompletionModel>),
}

/// A rig-backed LLM backend. Holds a ready provider model, the `max_tokens` cap
/// (`0` = provider default), and the tool set.
pub struct RigBackend {
    name: &'static str,
    model: Model,
    max_tokens: u64,
    tools: Arc<Tools>,
}

/// A keep-alive-tuned HTTP client in **rig's** reqwest version.
///
/// rig-core 0.42 pulls reqwest 0.13, a different major than the crate-wide
/// reqwest 0.12 behind `crate::http`, so the shared client cannot be injected
/// into rig's builders (their `HttpClientExt` bound is on rig's reqwest type).
/// We build an equivalently tuned client from rig's own re-exported reqwest with
/// the same keep-alive policy as [`crate::http`].
fn rig_tuned_client() -> rig_core::http_client::ReqwestClient {
    rig_core::http_client::ReqwestClient::builder()
        .pool_idle_timeout(crate::http::POOL_IDLE_TIMEOUT)
        .tcp_keepalive(crate::http::TCP_KEEPALIVE)
        .http2_keep_alive_interval(crate::http::HTTP2_KEEPALIVE_INTERVAL)
        .http2_keep_alive_while_idle(true)
        .build()
        .unwrap_or_default()
}

impl RigBackend {
    /// Local Ollama via rig. `base_url` is the Ollama root (no API key).
    pub fn ollama(base_url: &str, model: &str, tools: Arc<Tools>) -> Result<Self> {
        let client = ollama::Client::builder()
            .http_client(rig_tuned_client())
            .api_key(ollama::OllamaApiKey::default())
            .base_url(base_url)
            .build()
            .context("building rig Ollama client")?;
        Ok(Self {
            name: "ollama",
            model: Model::Ollama(Arc::new(client.completion_model(model))),
            max_tokens: 0,
            tools,
        })
    }

    /// Cloud Anthropic via rig. Pins the API version rig ships as "latest".
    pub fn anthropic(
        base_url: &str,
        api_key: &str,
        model: &str,
        max_tokens: u32,
        tools: Arc<Tools>,
    ) -> Result<Self> {
        let client = anthropic::Client::builder()
            .http_client(rig_tuned_client())
            .api_key(anthropic::client::AnthropicKey::from(api_key))
            .base_url(base_url)
            .anthropic_version(anthropic::completion::ANTHROPIC_VERSION_LATEST)
            .build()
            .context("building rig Anthropic client")?;
        Ok(Self {
            name: "anthropic",
            model: Model::Anthropic(Arc::new(client.completion_model(model))),
            max_tokens: max_tokens as u64,
            tools,
        })
    }
}

/// Build the initial message list for a turn: replay any prior conversation
/// `history` (`(user, assistant)` pairs, oldest first) followed by this turn's
/// user message. History is non-empty only for a **follow-up** turn.
fn seed_messages(history: Vec<(String, String)>, user_message: String) -> Vec<Message> {
    let mut messages = Vec::with_capacity(history.len() * 2 + 1);
    for (user, assistant) in history {
        messages.push(Message::user(user));
        messages.push(Message::assistant(assistant));
    }
    messages.push(Message::user(user_message));
    messages
}

/// Non-streaming completion used for tool-negotiation rounds. Returns the assistant
/// text (if any) plus every tool call the model requested.
async fn open_completion(
    model: &Model,
    preamble: &str,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
    max_tokens: u64,
) -> Result<(String, Vec<ToolCall>)> {
    match model {
        Model::Ollama(m) => {
            build_and_complete(m.clone(), preamble, messages, tool_defs, max_tokens).await
        }
        Model::Anthropic(m) => {
            build_and_complete(m.clone(), preamble, messages, tool_defs, max_tokens).await
        }
    }
}

async fn build_and_complete<M>(
    model: M,
    preamble: &str,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
    max_tokens: u64,
) -> Result<(String, Vec<ToolCall>)>
where
    M: CompletionModel + Clone,
{
    let (prompt, history) = messages
        .split_last()
        .expect("conversation always has at least the user message");
    let mut builder = model
        .completion_request(prompt.clone())
        .messages(history.iter().cloned())
        .preamble(preamble.to_string());
    if !tool_defs.is_empty() {
        builder = builder.tools(tool_defs.to_vec());
    }
    if max_tokens > 0 {
        builder = builder.max_tokens(max_tokens);
    }
    let response = model
        .completion(builder.build())
        .await
        .context("rig completion request")?;

    let mut text = String::new();
    let mut calls: Vec<ToolCall> = Vec::new();
    for content in response.choice.into_iter() {
        match content {
            AssistantContent::Text(t) => text.push_str(&t.text),
            AssistantContent::ToolCall(tc) => calls.push(tc),
            _ => {}
        }
    }
    Ok((text.trim().to_string(), calls))
}

#[async_trait]
impl LlmBackend for RigBackend {
    fn name(&self) -> &str {
        self.name
    }

    async fn respond(&self, turn: LlmTurn) -> Result<ReplyStream> {
        let model = self.model.clone();
        let max_tokens = self.max_tokens;
        let tools = self.tools.clone();
        let preamble = tools.preamble(&turn.system_prompt);
        let actions = turn.actions.clone();
        let tool_defs: Vec<ToolDefinition> = tools.definitions().to_vec();

        // Drive the tool-negotiation rounds with a **non-streaming** completion.
        let stream = async_stream::try_stream! {
            let mut messages: Vec<Message> = seed_messages(turn.history, turn.user_message);

            for round in 0..MAX_TOOL_ROUNDS {
                let rt_start = std::time::Instant::now();
                let (text, calls) =
                    open_completion(&model, &preamble, &messages, &tool_defs, max_tokens)
                        .await?;
                log::info!(
                    "rig LLM round {} round-trip in {}ms ({} reply chars, {} tool call(s))",
                    round + 1,
                    rt_start.elapsed().as_millis(),
                    text.len(),
                    calls.len(),
                );
                if !text.is_empty() {
                    yield text;
                }
                if calls.is_empty() {
                    break;
                }
                messages.push(Message::Assistant {
                    id: None,
                    content: calls.iter().cloned().map(AssistantContent::ToolCall).collect(),
                });
                for call in &calls {
                    log::info!(
                        "rig tool call: {}({})",
                        call.function.name,
                        call.function.arguments
                    );
                    let tool_start = std::time::Instant::now();
                    let result = tools
                        .dispatch(&call.function.name, &call.function.arguments, actions.as_ref())
                        .await
                        .unwrap_or_else(|e| format!("tool error: {e:#}"));
                    log::info!(
                        "rig tool result: {} in {}ms ({} chars): {}",
                        call.function.name,
                        tool_start.elapsed().as_millis(),
                        result.len(),
                        result.chars().take(200).collect::<String>()
                    );
                    messages.push(Message::tool_result(
                        call.id.as_str().to_string(),
                        call.function.name.clone(),
                        result,
                    ));
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests;
