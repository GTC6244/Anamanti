//! Web search: the pluggable [`SearchProvider`] backends (keyless DuckDuckGo
//! Instant Answer, or Tavily with a key) and the `internet_search` tool.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use rig_core::completion::ToolDefinition;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{definition, parse_args, ToolCtx, VoiceTool};

/// Tool name: search the public web.
pub(super) const INTERNET_SEARCH: &str = "internet_search";

/// Default number of search results to fold into a tool result.
const DEFAULT_SEARCH_RESULTS: u8 = 3;

/// Per-request deadline for a web-search call. Search is on the spoken path, so a
/// stalled provider must fail fast enough for the model to apologize.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);

/// `User-Agent` component label for search requests.
const COMPONENT: &str = "search";

/// Pluggable web-search implementation, injected so the tool is testable offline.
#[async_trait]
pub(crate) trait SearchProvider: Send + Sync {
    /// Run `query` and return a compact, speakable text summary of up to
    /// `max_results` findings.
    async fn search(&self, query: &str, max_results: u8) -> Result<String>;
}

/// Keyless web search via DuckDuckGo's Instant Answer API (`no auth`). Folds the
/// abstract and related-topic snippets into a short summary.
struct DuckDuckGoSearch {
    base_url: String,
}

impl DuckDuckGoSearch {
    fn new() -> Self {
        Self::with_base_url("https://api.duckduckgo.com")
    }

    /// Point the search at a specific Instant-Answer endpoint (tests).
    fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

/// The parts of an Instant Answer response the summary uses.
#[derive(Debug, Default, Deserialize)]
struct InstantAnswer {
    #[serde(rename = "AbstractText", default)]
    abstract_text: Option<String>,
    #[serde(rename = "RelatedTopics", default)]
    related_topics: Vec<RelatedTopic>,
}

#[derive(Debug, Default, Deserialize)]
struct RelatedTopic {
    #[serde(rename = "Text", default)]
    text: Option<String>,
}

#[async_trait]
impl SearchProvider for DuckDuckGoSearch {
    async fn search(&self, query: &str, max_results: u8) -> Result<String> {
        let answer: InstantAnswer = crate::http::send_json(
            crate::http::get(&self.base_url, COMPONENT).query(&[
                ("q", query),
                ("format", "json"),
                ("no_html", "1"),
                ("no_redirect", "1"),
                ("t", "anamanti-core"),
            ]),
            SEARCH_TIMEOUT,
            "DuckDuckGo Instant Answer",
        )
        .await?;

        let mut lines: Vec<String> = Vec::new();
        if let Some(text) = answer.abstract_text.filter(|s| !s.is_empty()) {
            lines.push(text);
        }
        for topic in answer.related_topics {
            if lines.len() >= max_results as usize {
                break;
            }
            if let Some(text) = topic.text.filter(|s| !s.is_empty()) {
                lines.push(text);
            }
        }
        if lines.is_empty() {
            lines.push(format!("No web results found for \"{query}\"."));
        }
        Ok(lines.join("\n"))
    }
}

/// Real general web search via the **Tavily** API, through the shared
/// [`crate::tavily::TavilyClient`]. Unlike DuckDuckGo's Instant Answer API, this
/// returns actual web results for news / weather / current-event queries.
struct TavilySearch {
    client: crate::tavily::TavilyClient,
}

impl TavilySearch {
    fn new(api_key: &str) -> Self {
        Self {
            client: crate::tavily::TavilyClient::new(api_key, SEARCH_TIMEOUT, COMPONENT),
        }
    }

    #[cfg(test)]
    fn with_base_url(base_url: impl Into<String>, api_key: &str) -> Self {
        Self {
            client: crate::tavily::TavilyClient::with_base_url(
                base_url,
                api_key,
                SEARCH_TIMEOUT,
                COMPONENT,
            ),
        }
    }
}

#[async_trait]
impl SearchProvider for TavilySearch {
    async fn search(&self, query: &str, max_results: u8) -> Result<String> {
        let resp = self
            .client
            .search(&crate::tavily::SearchRequest {
                query,
                max_results,
                search_depth: "basic",
                include_answer: Some(true),
            })
            .await?;

        // Prefer Tavily's synthesized answer when present; otherwise fold result
        // snippets into a compact summary.
        if let Some(answer) = resp.answer.filter(|s| !s.is_empty()) {
            return Ok(answer);
        }
        let lines: Vec<String> = resp
            .results
            .into_iter()
            .take(max_results as usize)
            .filter_map(|r| {
                let content = r.content.filter(|c| !c.is_empty())?;
                Some(match r.title.filter(|t| !t.is_empty()) {
                    Some(title) => format!("{title}: {content}"),
                    None => content,
                })
            })
            .collect();
        if lines.is_empty() {
            anyhow::bail!("Tavily returned no results for \"{query}\"");
        }
        Ok(lines.join("\n"))
    }
}

/// Build a search provider from a backend label + optional API key. `tavily` (with
/// a key) gives real general web results; anything else (or Tavily with no key)
/// falls back to the keyless DuckDuckGo Instant Answer API, which only answers
/// encyclopedic/entity queries (news/weather return nothing).
pub(super) fn build_search_provider(
    provider: &str,
    api_key: Option<&str>,
) -> Arc<dyn SearchProvider> {
    match (provider.to_lowercase().as_str(), api_key) {
        ("tavily", Some(key)) => {
            log::info!("web search provider: Tavily");
            Arc::new(TavilySearch::new(key))
        }
        ("tavily", None) => {
            log::warn!(
                "search provider `tavily` selected but no API key is set; \
                 falling back to DuckDuckGo Instant Answer (entity queries only)"
            );
            Arc::new(DuckDuckGoSearch::new())
        }
        _ => {
            log::info!(
                "web search provider: DuckDuckGo Instant Answer (keyless; entity queries \
                 only — choose Tavily + a key for general search)"
            );
            Arc::new(DuckDuckGoSearch::new())
        }
    }
}

/// Typed arguments for `internet_search`.
#[derive(Debug, Deserialize)]
struct SearchArgs {
    /// The search query.
    query: String,
    /// Maximum number of results to summarize (defaults to
    /// [`DEFAULT_SEARCH_RESULTS`], clamped to 1..=10).
    #[serde(default)]
    max_results: Option<u8>,
}

/// The `internet_search` tool over `provider`.
pub(crate) fn internet_search(provider: Arc<dyn SearchProvider>) -> Arc<dyn VoiceTool> {
    Arc::new(InternetSearch { provider })
}

struct InternetSearch {
    provider: Arc<dyn SearchProvider>,
}

#[async_trait]
impl VoiceTool for InternetSearch {
    fn name(&self) -> &'static str {
        INTERNET_SEARCH
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            INTERNET_SEARCH,
            "Search the public web for current, real-time, or factual information the \
             assistant does not already know (news, weather, prices, events, look-ups). \
             Returns a short text summary.",
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
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You have an `internet_search` tool that fetches live information from the web. \
             Whenever the user asks about current events, news, weather, sports, prices, or any \
             real-time or factual detail you are not certain of, you MUST call `internet_search` \
             first and base your answer on its results. Never claim you lack real-time or internet \
             access — use the tool.",
        )
    }

    async fn invoke(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<String> {
        let args: SearchArgs = parse_args(args, INTERNET_SEARCH)?;
        let max = args
            .max_results
            .unwrap_or(DEFAULT_SEARCH_RESULTS)
            .clamp(1, 10);
        self.provider
            .search(&args.query, max)
            .await
            .context("web search failed")
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::StaticSearch;
    use super::*;
    use crate::http::test_support::{refused_base, serve_sequence};

    #[tokio::test]
    async fn internet_search_invoke_runs_the_provider() {
        let tool = internet_search(Arc::new(StaticSearch("Sunny, 21C.")));
        let out = tool
            .invoke(
                &json!({ "query": "weather in paris" }),
                &ToolCtx { actions: None },
            )
            .await
            .unwrap();
        assert_eq!(out, "Sunny, 21C.");
    }

    #[tokio::test]
    async fn duckduckgo_summarizes_instant_answer_json() {
        let (base, server) = serve_sequence(vec![r#"{"AbstractText":"Rust is a systems language.","RelatedTopics":[{"Text":"Rust (programming language)"},{"Name":"group"}]}"#.into()]);
        let out = DuckDuckGoSearch::with_base_url(base)
            .search("rust lang", 3)
            .await
            .unwrap();
        assert_eq!(
            out,
            "Rust is a systems language.\nRust (programming language)"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tavily_prefers_the_synthesized_answer_then_snippets() {
        let (base, server) = serve_sequence(vec![
            r#"{"answer":"It is 21C and sunny in Paris.","results":[{"title":"Weather","content":"Paris weather details"}]}"#.into(),
            r#"{"answer":"","results":[{"title":"Weather","content":"Paris details"},{"content":"More"}]}"#.into(),
        ]);
        let tavily = TavilySearch::with_base_url(base, "k");
        let out = tavily.search("weather in paris", 3).await.unwrap();
        assert_eq!(out, "It is 21C and sunny in Paris.");
        let out = tavily.search("weather in paris", 3).await.unwrap();
        assert_eq!(out, "Weather: Paris details\nMore");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn search_failures_reach_the_model_with_the_tool_prefix() {
        let tool = internet_search(Arc::new(DuckDuckGoSearch::with_base_url(refused_base())));
        let err = tool
            .invoke(&json!({ "query": "x" }), &ToolCtx { actions: None })
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.starts_with("web search failed: DuckDuckGo Instant Answer: request failed"),
            "{text}"
        );
    }
}
