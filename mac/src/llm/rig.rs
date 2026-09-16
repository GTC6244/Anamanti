//! [`LlmBackend`] implemented on top of the **rig-core** agent framework
//! (`AMBIENT_LLM_ENGINE=rig`, feature `rig`).
//!
//! This routes the same `respond(turn) -> ReplyStream` seam through rig's unified
//! `CompletionModel` trait instead of the hand-rolled HTTP clients in `ollama.rs`
//! / `anthropic.rs`. One provider is chosen at construction; both funnel into
//! rig's single `StreamingCompletionResponse`.
//!
//! ## Tool calling
//!
//! Unlike the native backends (plain text only), the rig path supports **tool
//! calling**. When tools are configured (currently [`InternetSearch`], enabled via
//! `AMBIENT_WEB_SEARCH`), `respond` runs a short negotiation loop: it streams a
//! turn, and if the model asks to call a tool it executes it, appends the result
//! to the conversation, and streams again — up to [`MAX_TOOL_ROUNDS`] rounds —
//! forwarding assistant text deltas as reply tokens throughout. A turn that needs
//! no tool streams straight through in one pass (no regression for the common
//! case).
//!
//! Kept behind a feature flag during the migration so the default build stays lean
//! and the proven native backends remain the fallback until this reaches parity.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

use rig_core::client::completion::CompletionClient;
use rig_core::completion::{CompletionModel, ToolDefinition};
use rig_core::message::{AssistantContent, Message, ToolCall};
use rig_core::providers::{anthropic, ollama};
use rig_core::streaming::{StreamedAssistantContent, StreamingCompletionResponse};
use rig_core::tool::PortableTool;

use super::{LlmBackend, LlmTurn, ReplyStream};

/// Upper bound on tool-negotiation rounds per turn, so a model that loops on tool
/// calls can never spin forever. Each round is one streamed completion pass.
pub const MAX_TOOL_ROUNDS: usize = 4;

/// Appended to the system preamble when tools are available, so the model
/// actually calls `internet_search` for real-time questions instead of refusing
/// with "I don't have real-time access".
const TOOL_GUIDANCE: &str = "You have an `internet_search` tool that fetches live \
information from the web. Whenever the user asks about current events, news, weather, \
sports, prices, or any real-time or factual detail you are not certain of, you MUST call \
`internet_search` first and base your answer on its results. Never claim you lack \
real-time or internet access — use the tool.";

/// Default number of search results to fold into a tool result.
pub const DEFAULT_SEARCH_RESULTS: u8 = 3;

// ===========================================================================
// Web search tool
// ===========================================================================

/// Typed arguments for [`InternetSearch`] (the plan's `SearchArgs`).
#[derive(Debug, Deserialize)]
pub struct SearchArgs {
    /// The search query.
    pub query: String,
    /// Maximum number of results to summarize (defaults to
    /// [`DEFAULT_SEARCH_RESULTS`], clamped to 1..=10).
    #[serde(default)]
    pub max_results: Option<u8>,
}

/// A concrete, `std::error::Error` failure for the search tool (rig requires the
/// tool's error type to implement `std::error::Error`, which `anyhow::Error` does
/// not).
#[derive(Debug)]
pub struct SearchError(pub String);

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "web search failed: {}", self.0)
    }
}

impl std::error::Error for SearchError {}

/// Pluggable web-search implementation, injected so the tool is testable offline.
#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// Run `query` and return a compact, speakable text summary of up to
    /// `max_results` findings.
    async fn search(&self, query: &str, max_results: u8) -> Result<String>;
}

/// Keyless web search via DuckDuckGo's Instant Answer API (`no auth`). Folds the
/// abstract and related-topic snippets into a short summary.
pub struct DuckDuckGoSearch {
    client: reqwest::Client,
    base_url: String,
}

impl Default for DuckDuckGoSearch {
    fn default() -> Self {
        Self::with_base_url("https://api.duckduckgo.com")
    }
}

impl DuckDuckGoSearch {
    /// Point the search at a specific Instant-Answer endpoint (overridable for
    /// tests).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }
}

#[async_trait]
impl SearchProvider for DuckDuckGoSearch {
    async fn search(&self, query: &str, max_results: u8) -> Result<String> {
        let value: Value = self
            .client
            .get(&self.base_url)
            .query(&[
                ("q", query),
                ("format", "json"),
                ("no_html", "1"),
                ("no_redirect", "1"),
                ("t", "ambient-orchestrator"),
            ])
            .send()
            .await
            .context("GET DuckDuckGo Instant Answer")?
            .error_for_status()
            .context("DuckDuckGo returned an error status")?
            .json()
            .await
            .context("parsing DuckDuckGo JSON")?;

        let mut lines: Vec<String> = Vec::new();
        if let Some(abstract_text) = value
            .get("AbstractText")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            lines.push(abstract_text.to_string());
        }
        if let Some(topics) = value.get("RelatedTopics").and_then(Value::as_array) {
            for topic in topics {
                if lines.len() >= max_results as usize {
                    break;
                }
                if let Some(text) = topic
                    .get("Text")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    lines.push(text.to_string());
                }
            }
        }
        if lines.is_empty() {
            lines.push(format!("No web results found for \"{query}\"."));
        }
        Ok(lines.join("\n"))
    }
}

/// Real general web search via the **Tavily** API (`https://api.tavily.com`), an
/// LLM-oriented search service. Needs `TAVILY_API_KEY`. Unlike DuckDuckGo's
/// Instant Answer API, this returns actual web results for news / weather /
/// current-event queries.
pub struct TavilySearch {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl TavilySearch {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url("https://api.tavily.com", api_key)
    }

    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }
}

#[async_trait]
impl SearchProvider for TavilySearch {
    async fn search(&self, query: &str, max_results: u8) -> Result<String> {
        let body = json!({
            "api_key": self.api_key,
            "query": query,
            "max_results": max_results,
            "search_depth": "basic",
            "include_answer": true,
        });
        let value: Value = self
            .client
            .post(format!("{}/search", self.base_url))
            .json(&body)
            .send()
            .await
            .context("POST Tavily /search")?
            .error_for_status()
            .context("Tavily returned an error status")?
            .json()
            .await
            .context("parsing Tavily JSON")?;

        // Prefer Tavily's synthesized answer when present; otherwise fold result
        // snippets into a compact summary.
        if let Some(answer) = value
            .get("answer")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Ok(answer.to_string());
        }
        let mut lines: Vec<String> = Vec::new();
        if let Some(results) = value.get("results").and_then(Value::as_array) {
            for result in results.iter().take(max_results as usize) {
                let title = result.get("title").and_then(Value::as_str).unwrap_or("");
                let content = result.get("content").and_then(Value::as_str).unwrap_or("");
                if !content.is_empty() {
                    lines.push(if title.is_empty() {
                        content.to_string()
                    } else {
                        format!("{title}: {content}")
                    });
                }
            }
        }
        if lines.is_empty() {
            anyhow::bail!("Tavily returned no results for \"{query}\"");
        }
        Ok(lines.join("\n"))
    }
}

/// The web-search tool the model can call. Genuinely implements rig's
/// [`PortableTool`]; the runtime path uses [`InternetSearch::definition`] to
/// advertise it and [`InternetSearch::invoke`] to execute it from raw JSON args.
pub struct InternetSearch {
    provider: Arc<dyn SearchProvider>,
}

impl InternetSearch {
    pub fn new(provider: Arc<dyn SearchProvider>) -> Self {
        Self { provider }
    }

    /// The rig tool definition to advertise on a completion request.
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: self.description(),
            parameters: self.parameters(),
        }
    }

    /// Execute the tool from the model's raw JSON arguments (the runtime path),
    /// routed through the rig [`PortableTool::call`] implementation.
    pub async fn invoke(&self, arguments: &Value) -> Result<String> {
        let args: SearchArgs = serde_json::from_value(arguments.clone())
            .context("parsing internet_search arguments")?;
        self.call(args)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}

impl PortableTool for InternetSearch {
    const NAME: &'static str = "internet_search";
    type Args = SearchArgs;
    type Output = String;
    type Error = SearchError;

    fn description(&self) -> String {
        "Search the public web for current, real-time, or factual information the \
         assistant does not already know (news, weather, prices, events, look-ups). \
         Returns a short text summary."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query, phrased as you would type it into a search engine."
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "description": "How many results to summarize (default 3)."
                }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, arguments: Self::Args) -> Result<Self::Output, Self::Error> {
        let max = arguments
            .max_results
            .unwrap_or(DEFAULT_SEARCH_RESULTS)
            .clamp(1, 10);
        self.provider
            .search(&arguments.query, max)
            .await
            .map_err(|e| SearchError(format!("{e:#}")))
    }
}

/// The set of tools available to a turn: their advertised definitions plus a
/// dispatcher that executes a call by name. Cheaply shared behind an `Arc`.
pub struct Tools {
    definitions: Vec<ToolDefinition>,
    search: Arc<InternetSearch>,
}

impl Tools {
    /// Build the tool set from a search provider (currently the only tool).
    pub fn new(provider: Arc<dyn SearchProvider>) -> Self {
        let search = Arc::new(InternetSearch::new(provider));
        Self {
            definitions: vec![search.definition()],
            search,
        }
    }

    /// Execute a model-requested tool call by name, returning its text result.
    async fn dispatch(&self, name: &str, arguments: &Value) -> Result<String> {
        match name {
            InternetSearch::NAME => self.search.invoke(arguments).await,
            other => anyhow::bail!("model called unknown tool `{other}`"),
        }
    }
}

/// Choose the search backend from the environment. `AMBIENT_SEARCH_PROVIDER=tavily`
/// (with `TAVILY_API_KEY`) gives real general web results; the default is the
/// keyless DuckDuckGo Instant Answer API, which only answers encyclopedic/entity
/// queries (news/weather/current events return nothing).
fn default_search_provider() -> Arc<dyn SearchProvider> {
    match std::env::var("AMBIENT_SEARCH_PROVIDER")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "tavily" => match std::env::var("TAVILY_API_KEY") {
            Ok(key) if !key.is_empty() => {
                log::info!("web search provider: Tavily");
                Arc::new(TavilySearch::new(key))
            }
            _ => {
                log::warn!(
                    "AMBIENT_SEARCH_PROVIDER=tavily but TAVILY_API_KEY is unset; \
                     falling back to DuckDuckGo Instant Answer (entity queries only)"
                );
                Arc::new(DuckDuckGoSearch::default())
            }
        },
        _ => {
            log::info!(
                "web search provider: DuckDuckGo Instant Answer (keyless; entity queries \
                 only — set AMBIENT_SEARCH_PROVIDER=tavily + TAVILY_API_KEY for general search)"
            );
            Arc::new(DuckDuckGoSearch::default())
        }
    }
}

/// Build the tool set from a feature flag: `Some(tools)` when web search is on.
/// The search backend is chosen from the environment (see
/// [`default_search_provider`]).
pub fn tools_from_flag(web_search: bool) -> Option<Arc<Tools>> {
    web_search.then(|| Arc::new(Tools::new(default_search_provider())))
}

// ===========================================================================
// Backend
// ===========================================================================

/// The rig completion model for the selected provider, wrapped in `Arc` so the
/// backend is cheaply clonable into the (`'static`) reply stream. `Arc<M>` is
/// itself a `CompletionModel`, so both variants stay usable as models.
#[derive(Clone)]
enum Model {
    Ollama(Arc<ollama::CompletionModel>),
    Anthropic(Arc<anthropic::completion::CompletionModel>),
}

/// A rig-backed LLM backend. Holds a ready provider model, the `max_tokens` cap
/// (`0` = provider default), and an optional tool set.
pub struct RigBackend {
    name: &'static str,
    model: Model,
    max_tokens: u64,
    tools: Option<Arc<Tools>>,
}

impl RigBackend {
    /// Local Ollama via rig. `base_url` is the Ollama root (no API key).
    pub fn ollama(base_url: &str, model: &str, tools: Option<Arc<Tools>>) -> Result<Self> {
        let client = ollama::Client::builder()
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
        tools: Option<Arc<Tools>>,
    ) -> Result<Self> {
        let client = anthropic::Client::builder()
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

/// Open a streamed completion for the running conversation. Generic over the
/// provider model so both variants share one path.
async fn build_and_stream<M>(
    model: M,
    system_prompt: &str,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
    max_tokens: u64,
) -> Result<StreamingCompletionResponse>
where
    M: CompletionModel + Clone,
{
    let (prompt, history) = messages
        .split_last()
        .expect("conversation always has at least the user message");
    // When tools are available, tell the model to use them (some models otherwise
    // refuse real-time questions instead of calling the tool).
    let preamble = if tool_defs.is_empty() {
        system_prompt.to_string()
    } else {
        format!("{system_prompt}\n\n{TOOL_GUIDANCE}")
    };
    let mut builder = model
        .completion_request(prompt.clone())
        .messages(history.iter().cloned())
        .preamble(preamble);
    if !tool_defs.is_empty() {
        builder = builder.tools(tool_defs.to_vec());
    }
    if max_tokens > 0 {
        builder = builder.max_tokens(max_tokens);
    }
    model
        .stream(builder.build())
        .await
        .context("opening rig completion stream")
}

/// Dispatch a streamed completion on the concrete provider model.
async fn open_stream(
    model: &Model,
    system_prompt: &str,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
    max_tokens: u64,
) -> Result<StreamingCompletionResponse> {
    match model {
        Model::Ollama(m) => {
            build_and_stream(m.clone(), system_prompt, messages, tool_defs, max_tokens).await
        }
        Model::Anthropic(m) => {
            build_and_stream(m.clone(), system_prompt, messages, tool_defs, max_tokens).await
        }
    }
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
        let system_prompt = turn.system_prompt;
        let tool_defs: Vec<ToolDefinition> = tools
            .as_ref()
            .map(|t| t.definitions.clone())
            .unwrap_or_default();

        let stream = async_stream::try_stream! {
            let mut messages: Vec<Message> = vec![Message::user(turn.user_message)];

            for _round in 0..MAX_TOOL_ROUNDS {
                let response =
                    open_stream(&model, &system_prompt, &messages, &tool_defs, max_tokens).await?;
                futures_util::pin_mut!(response);

                // Forward assistant text as reply tokens; collect any tool calls to
                // execute once this pass completes.
                let mut calls: Vec<ToolCall> = Vec::new();
                while let Some(part) = response.next().await {
                    match part? {
                        StreamedAssistantContent::Text(text) => {
                            if !text.text.is_empty() {
                                yield text.text;
                            }
                        }
                        StreamedAssistantContent::ToolCall { tool_call, .. } => {
                            calls.push(tool_call);
                        }
                        _ => {}
                    }
                }

                // No tool calls → the model has finished; stop.
                if calls.is_empty() {
                    break;
                }
                let Some(tools) = tools.as_ref() else {
                    break;
                };

                // Record the assistant's tool-call turn, then each tool's result,
                // so the next pass sees the outcomes and can answer.
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
                    let result = tools
                        .dispatch(&call.function.name, &call.function.arguments)
                        .await
                        .unwrap_or_else(|e| format!("tool error: {e:#}"));
                    log::info!(
                        "rig tool result ({} chars): {}",
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
mod tests {
    use super::*;
    use crate::llm::collect_reply;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A canned search provider so tool tests never touch the network.
    struct StaticSearch(&'static str);

    #[async_trait]
    impl SearchProvider for StaticSearch {
        async fn search(&self, _query: &str, _max: u8) -> Result<String> {
            Ok(self.0.to_string())
        }
    }

    /// Serve a fixed sequence of raw HTTP responses, one per inbound connection.
    fn serve_sequence(bodies: Vec<String>) -> (String, tokio::task::JoinHandle<()>) {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let listener = TcpListener::from_std(std_listener).unwrap();
            for body in bodies {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
                sock.flush().await.unwrap();
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn internet_search_definition_advertises_typed_args() {
        let tool = InternetSearch::new(Arc::new(StaticSearch("x")));
        let def = tool.definition();
        assert_eq!(def.name, "internet_search");
        assert_eq!(def.parameters["required"][0], "query");
        assert_eq!(def.parameters["properties"]["query"]["type"], "string");
    }

    #[tokio::test]
    async fn internet_search_invoke_runs_the_provider() {
        let tool = InternetSearch::new(Arc::new(StaticSearch("Sunny, 21C.")));
        let out = tool
            .invoke(&json!({ "query": "weather in paris" }))
            .await
            .unwrap();
        assert_eq!(out, "Sunny, 21C.");
    }

    #[tokio::test]
    async fn duckduckgo_summarizes_instant_answer_json() {
        let body = r#"{"AbstractText":"Rust is a systems language.","RelatedTopics":[{"Text":"Rust (programming language)"}]}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });
        let ddg = DuckDuckGoSearch::with_base_url(format!("http://{addr}"));
        let out = ddg.search("rust lang", 3).await.unwrap();
        assert!(out.contains("systems language"));
        assert!(out.contains("Rust (programming language)"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tavily_prefers_the_synthesized_answer() {
        let body = r#"{"answer":"It is 21C and sunny in Paris.","results":[{"title":"Weather","content":"Paris weather details"}]}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });
        let tavily = TavilySearch::with_base_url(format!("http://{addr}"), "k");
        let out = tavily.search("weather in paris", 3).await.unwrap();
        assert_eq!(out, "It is 21C and sunny in Paris.");
        server.await.unwrap();
    }

    /// End-to-end tool loop: round 0 the fake Ollama asks for `internet_search`;
    /// round 1 (after the tool result is threaded back) it streams the answer.
    #[tokio::test]
    async fn rig_ollama_runs_tool_then_streams_answer() {
        let tool_call = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"internet_search\",\"arguments\":{\"query\":\"weather in paris\"}}}]},\"done\":true,\"done_reason\":\"stop\"}\n";
        let answer = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"It is sunny\"},\"done\":false}\n\
                      {\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\" in Paris.\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let (url, server) = serve_sequence(vec![tool_call.to_string(), answer.to_string()]);

        let tools = Some(Arc::new(Tools::new(Arc::new(StaticSearch("Sunny, 21C.")))));
        let backend = RigBackend::ollama(&url, "test-model", tools).unwrap();
        let stream = backend
            .respond(LlmTurn::new("sys", "what is the weather in paris"))
            .await
            .unwrap();
        assert_eq!(collect_reply(stream).await.unwrap(), "It is sunny in Paris.");
        server.await.unwrap();
    }

    /// Without tools, a turn still streams straight through in one pass.
    #[tokio::test]
    async fn rig_ollama_streams_text_deltas() {
        let body = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"done\":false}\n\
                    {\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\" there\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let (url, server) = serve_sequence(vec![body.to_string()]);
        let backend = RigBackend::ollama(&url, "test-model", None).unwrap();
        let stream = backend.respond(LlmTurn::new("sys", "hello")).await.unwrap();
        assert_eq!(collect_reply(stream).await.unwrap(), "Hi there");
        server.await.unwrap();
    }
}
