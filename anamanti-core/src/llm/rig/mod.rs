//! [`LlmBackend`] implemented on top of the **rig-core** agent framework
//! (selected at runtime with `llm.engine="rig"`, the default).
//!
//! This routes the same `respond(turn) -> ReplyStream` seam through rig's unified
//! `CompletionModel` trait instead of the hand-rolled HTTP clients in `ollama.rs`
//! / `anthropic.rs`. One provider is chosen at construction.
//!
//! ## Tool calling and streaming
//!
//! Unlike the native backends (plain text only), the rig path supports **tool
//! calling** over the [`tools`] registry (timers, personality, font, and every
//! configured integration). `respond` runs a short negotiation loop: each round is
//! one completion; if the model asks for tools they are executed, their results
//! appended to the conversation, and the model is asked again — up to
//! [`MAX_TOOL_ROUNDS`] rounds, after which one final round is made with tool use
//! forbidden so the turn always ends in an answer.
//!
//! Every round is **streamed**: text deltas reach TTS as they arrive and tool calls
//! are collected from the same stream, so a plain answer is spoken token by token
//! and a tool turn speaks any lead-in text immediately. (rig-core releases before
//! 0.42 dropped Ollama tool calls that arrived in a non-final `done:false` chunk, so
//! tool turns used to fall back to a non-streaming `completion()`; 0.42's Ollama
//! adapter reads tool calls from every record — covered by
//! `tests::rig_ollama_stream_surfaces_a_tool_call_from_a_non_final_chunk`.) The
//! non-streaming path remains for a provider whose [`RoundModel::streams_tool_calls`]
//! is false.

pub mod tools;

use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};

use rig_core::client::completion::CompletionClient;
use rig_core::completion::{CompletionModel, CompletionRequest, ToolDefinition};
use rig_core::message::{AssistantContent, Message, ToolCall, ToolChoice};
use rig_core::providers::{anthropic, ollama};
use rig_core::streaming::StreamedAssistantContent;

use super::{ActionSink, LlmBackend, LlmTurn, ReplyStream};
pub use tools::{ToolDeps, Tools};

/// Upper bound on tool-negotiation rounds per turn, so a model that loops on tool
/// calls can never spin forever. One extra, tool-free round follows if it is hit.
const MAX_TOOL_ROUNDS: usize = 4;

/// Spoken when the model is still asking for tools after the final round and has
/// produced no text, so the user never gets silence.
const ROUND_CAP_FALLBACK: &str = "Sorry, I couldn't finish that one.";

/// Connect timeout for the rig HTTP client.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-read inactivity guard for the rig HTTP client: a read that sees no bytes for
/// this long fails. It resets on every chunk, so a long streamed reply is never cut;
/// it is generous because a non-streaming completion sends nothing until the whole
/// reply is ready.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

// ===========================================================================
// One round against a provider
// ===========================================================================

/// What one round of the turn loop sends.
#[derive(Clone)]
pub(crate) struct RoundRequest {
    /// System prompt plus tool guidance.
    pub(crate) preamble: String,
    /// The conversation so far; the last message is the prompt.
    pub(crate) messages: Vec<Message>,
    /// Tools advertised this round.
    pub(crate) tools: Vec<ToolDefinition>,
    /// Advertise `tools` but forbid calling them (`tool_choice: none`) — the final
    /// round after the cap, on providers that honour it.
    pub(crate) forbid_tools: bool,
    /// Output cap; `0` = provider default.
    pub(crate) max_tokens: u64,
}

/// One item of a streamed round.
#[derive(Debug)]
pub(crate) enum RoundEvent {
    /// A text delta.
    Text(String),
    /// A complete tool call.
    ToolCall(ToolCall),
}

/// A streamed round: text deltas and complete tool calls, in arrival order.
pub(crate) type RoundStream = Pin<Box<dyn Stream<Item = Result<RoundEvent>> + Send>>;

/// The provider seam the turn loop runs over. Implemented by [`Model`] (the real
/// rig providers) and by test doubles.
#[async_trait]
pub(crate) trait RoundModel: Send + Sync {
    /// Whether tool calls surface reliably from this provider's stream. When false,
    /// rounds that advertise tools use the non-streaming [`Self::complete`].
    fn streams_tool_calls(&self) -> bool;
    /// Whether the provider honours `tool_choice: none`. When false, the final
    /// round after the cap simply advertises no tools.
    fn honours_tool_choice_none(&self) -> bool;
    /// Open a streamed round.
    async fn stream(&self, req: RoundRequest) -> Result<RoundStream>;
    /// Run a round to completion: the reply text (trimmed) and any tool calls.
    async fn complete(&self, req: RoundRequest) -> Result<(String, Vec<ToolCall>)>;
}

/// The rig completion model for the selected provider, wrapped in `Arc` so the
/// backend is cheaply clonable into the (`'static`) reply stream.
#[derive(Clone)]
enum Model {
    Ollama(Arc<ollama::CompletionModel>),
    Anthropic(Arc<anthropic::completion::CompletionModel>),
}

/// Build the rig request for `r` on model `m`.
fn rig_request<M: CompletionModel + Clone>(m: &M, r: RoundRequest) -> CompletionRequest {
    let mut messages = r.messages;
    let prompt = messages
        .pop()
        .expect("conversation always has at least the user message");
    let mut builder = m
        .completion_request(prompt)
        .messages(messages)
        .preamble(r.preamble);
    if !r.tools.is_empty() {
        builder = builder.tools(r.tools);
        if r.forbid_tools {
            builder = builder.tool_choice(ToolChoice::None);
        }
    }
    if r.max_tokens > 0 {
        builder = builder.max_tokens(r.max_tokens);
    }
    builder.build()
}

async fn stream_on<M: CompletionModel + Clone>(m: &M, r: RoundRequest) -> Result<RoundStream> {
    let response = m
        .stream(rig_request(m, r))
        .await
        .context("opening rig completion stream")?;
    Ok(Box::pin(response.filter_map(|part| {
        futures_util::future::ready(match part {
            Ok(StreamedAssistantContent::Text(t)) => Some(Ok(RoundEvent::Text(t.text))),
            Ok(StreamedAssistantContent::ToolCall { tool_call, .. }) => {
                Some(Ok(RoundEvent::ToolCall(tool_call)))
            }
            Ok(_) => None,
            Err(e) => Some(Err(anyhow::Error::new(e).context("rig completion stream"))),
        })
    })))
}

async fn complete_on<M: CompletionModel + Clone>(
    m: &M,
    r: RoundRequest,
) -> Result<(String, Vec<ToolCall>)> {
    let response = m
        .completion(rig_request(m, r))
        .await
        .context("rig completion request")?;
    let mut text = String::new();
    let mut calls = Vec::new();
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
impl RoundModel for Model {
    fn streams_tool_calls(&self) -> bool {
        // rig-core 0.42 surfaces streamed tool calls for both providers (Anthropic
        // emits them at `content_block_stop`; Ollama from any record, final or not).
        true
    }

    fn honours_tool_choice_none(&self) -> bool {
        // rig's Ollama provider ignores `tool_choice` (it logs a warning).
        matches!(self, Model::Anthropic(_))
    }

    async fn stream(&self, req: RoundRequest) -> Result<RoundStream> {
        match self {
            Model::Ollama(m) => stream_on(m, req).await,
            Model::Anthropic(m) => stream_on(m, req).await,
        }
    }

    async fn complete(&self, req: RoundRequest) -> Result<(String, Vec<ToolCall>)> {
        match self {
            Model::Ollama(m) => complete_on(m, req).await,
            Model::Anthropic(m) => complete_on(m, req).await,
        }
    }
}

// ===========================================================================
// Backend
// ===========================================================================

/// A rig-backed LLM backend. Holds a ready provider model, the `max_tokens` cap
/// (`0` = provider default), and the tool set.
pub struct RigBackend {
    name: &'static str,
    model: Arc<dyn RoundModel>,
    max_tokens: u64,
    tools: Arc<Tools>,
}

/// A keep-alive-tuned HTTP client in **rig's** reqwest version.
///
/// rig-core 0.42 pulls reqwest 0.13, a different major than the crate-wide
/// reqwest 0.12 behind `crate::http`, so the shared client cannot be injected
/// into rig's builders (their `HttpClientExt` bound is on rig's reqwest type).
/// We build an equivalently tuned client from rig's own re-exported reqwest with
/// the same keep-alive policy as [`crate::http`], plus a connect timeout and a
/// per-read inactivity guard (never a whole-request deadline, which would cut a
/// long streamed reply).
fn rig_tuned_client() -> rig_core::http_client::ReqwestClient {
    rig_core::http_client::ReqwestClient::builder()
        .pool_idle_timeout(crate::http::POOL_IDLE_TIMEOUT)
        .tcp_keepalive(crate::http::TCP_KEEPALIVE)
        .http2_keep_alive_interval(crate::http::HTTP2_KEEPALIVE_INTERVAL)
        .http2_keep_alive_while_idle(true)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_IDLE_TIMEOUT)
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
        Ok(Self::with_model(
            "ollama",
            Arc::new(Model::Ollama(Arc::new(client.completion_model(model)))),
            0,
            tools,
        ))
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
        Ok(Self::with_model(
            "anthropic",
            Arc::new(Model::Anthropic(Arc::new(client.completion_model(model)))),
            max_tokens as u64,
            tools,
        ))
    }

    /// A backend over any [`RoundModel`] (tests use a scripted model).
    pub(crate) fn with_model(
        name: &'static str,
        model: Arc<dyn RoundModel>,
        max_tokens: u64,
        tools: Arc<Tools>,
    ) -> Self {
        Self {
            name,
            model,
            max_tokens,
            tools,
        }
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

/// Joins the text of successive rounds into one reply: the first text of a later
/// round is separated from what was already said by a space (unless either side
/// already has whitespace there), so "Let me check." + "It's sunny." never glues.
#[derive(Default)]
struct ReplyJoin {
    last: Option<char>,
}

impl ReplyJoin {
    /// The chunk to yield for `text`; `round_start` marks the round's first text.
    fn chunk(&mut self, text: &str, round_start: bool) -> String {
        let glue = round_start
            && self.last.is_some_and(|c| !c.is_whitespace())
            && !text.starts_with(char::is_whitespace);
        if let Some(c) = text.chars().last() {
            self.last = Some(c);
        }
        if glue {
            format!(" {text}")
        } else {
            text.to_string()
        }
    }

    fn is_empty(&self) -> bool {
        self.last.is_none()
    }
}

/// Execute each requested tool call and append the assistant's tool-call message
/// plus every result to `messages`, so the next round sees the outcomes. A tool
/// error becomes the tool result (redacted — it may embed a provider URL) so the
/// model can apologize aloud.
async fn run_tools(
    tools: &Tools,
    messages: &mut Vec<Message>,
    text: String,
    calls: Vec<ToolCall>,
    actions: Option<&ActionSink>,
) {
    let mut content: Vec<AssistantContent> = Vec::with_capacity(calls.len() + 1);
    if !text.trim().is_empty() {
        content.push(AssistantContent::text(text));
    }
    content.extend(calls.iter().cloned().map(AssistantContent::ToolCall));
    messages.push(Message::Assistant {
        id: None,
        content: content.into_iter().collect(),
    });
    for call in calls {
        log::info!(
            "rig tool call: {}({})",
            call.function.name,
            call.function.arguments
        );
        let tool_start = Instant::now();
        let result = tools
            .dispatch(&call.function.name, &call.function.arguments, actions)
            .await
            .unwrap_or_else(|e| crate::http::redact_secrets(&format!("tool error: {e:#}")));
        log::info!(
            "rig tool result: {} in {}ms ({} chars): {}",
            call.function.name,
            tool_start.elapsed().as_millis(),
            result.len(),
            result.chars().take(200).collect::<String>()
        );
        messages.push(Message::tool_result(
            call.id.as_str().to_string(),
            call.function.name,
            result,
        ));
    }
}

/// The turn loop: stream (or complete) rounds, run requested tools, repeat.
fn run_turn(
    model: Arc<dyn RoundModel>,
    tools: Arc<Tools>,
    max_tokens: u64,
    turn: LlmTurn,
) -> ReplyStream {
    let stream = async_stream::try_stream! {
        let started = Instant::now();
        let preamble = tools.preamble(&turn.system_prompt);
        let defs = tools.definitions().to_vec();
        let actions = turn.actions;
        let stream_rounds = defs.is_empty() || model.streams_tool_calls();
        let mut messages = seed_messages(turn.history, turn.user_message);
        let mut reply = ReplyJoin::default();

        for round in 0..=MAX_TOOL_ROUNDS {
            let last = round == MAX_TOOL_ROUNDS;
            if last {
                log::warn!(
                    "rig: the model was still calling tools after {MAX_TOOL_ROUNDS} rounds; \
                     asking for a final answer with tool use forbidden"
                );
            }
            let forbid = last && model.honours_tool_choice_none();
            let req = RoundRequest {
                preamble: preamble.clone(),
                messages: messages.clone(),
                tools: if last && !forbid { Vec::new() } else { defs.clone() },
                forbid_tools: forbid,
                max_tokens,
            };
            let round_start = Instant::now();
            let mut text = String::new();
            let mut calls = Vec::new();
            if stream_rounds {
                let mut events = model.stream(req).await?;
                while let Some(event) = events.next().await {
                    match event? {
                        RoundEvent::Text(delta) if !delta.is_empty() => {
                            if reply.is_empty() {
                                log::info!(
                                    "rig LLM stream: first token in {}ms",
                                    started.elapsed().as_millis()
                                );
                            }
                            let chunk = reply.chunk(&delta, text.is_empty());
                            text.push_str(&delta);
                            yield chunk;
                        }
                        RoundEvent::Text(_) => {}
                        RoundEvent::ToolCall(call) => calls.push(call),
                    }
                }
            } else {
                let (t, c) = model.complete(req).await?;
                if !t.is_empty() {
                    if reply.is_empty() {
                        log::info!(
                            "rig LLM: first text in {}ms",
                            started.elapsed().as_millis()
                        );
                    }
                    yield reply.chunk(&t, true);
                }
                text = t;
                calls = c;
            }
            log::info!(
                "rig LLM round {} in {}ms ({} reply chars, {} tool call(s))",
                round + 1,
                round_start.elapsed().as_millis(),
                text.len(),
                calls.len(),
            );
            if last {
                if !calls.is_empty() {
                    log::warn!(
                        "rig: ignoring {} tool call(s) requested in the final round",
                        calls.len()
                    );
                }
                if text.trim().is_empty() {
                    yield reply.chunk(ROUND_CAP_FALLBACK, true);
                }
                break;
            }
            if calls.is_empty() {
                break;
            }
            run_tools(&tools, &mut messages, text, calls, actions.as_ref()).await;
        }
        log::info!("rig LLM turn complete in {}ms", started.elapsed().as_millis());
    };
    Box::pin(stream)
}

#[async_trait]
impl LlmBackend for RigBackend {
    fn name(&self) -> &str {
        self.name
    }

    async fn respond(&self, turn: LlmTurn) -> Result<ReplyStream> {
        Ok(run_turn(
            self.model.clone(),
            self.tools.clone(),
            self.max_tokens,
            turn,
        ))
    }
}

#[cfg(test)]
mod tests;
