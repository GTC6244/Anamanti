//! The one **Tavily** search API client (`https://api.tavily.com/search`).
//!
//! Tavily backs both the web-search tool (`llm::rig`'s `TavilySearch`) and recipe
//! source discovery ([`crate::recipe`]). Both callers used to hand-roll the same POST
//! with different timeouts; this client owns the request shape, rides the
//! [`crate::http::shared_client`] pool, and makes the per-request timeout mandatory
//! (it is a constructor argument). The key travels in the JSON body — never in a URL —
//! and request errors go through [`crate::http`]'s redacting helpers.

use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// The production Tavily API root.
pub const DEFAULT_BASE_URL: &str = "https://api.tavily.com";

/// A Tavily client bound to one API key and one per-request timeout.
#[derive(Clone, Debug)]
pub struct TavilyClient {
    base_url: String,
    api_key: String,
    timeout: Duration,
    /// The `User-Agent` component label (e.g. `"recipe"`, `"search"`).
    component: &'static str,
}

/// One `/search` request.
#[derive(Clone, Debug, Serialize)]
pub struct SearchRequest<'a> {
    pub query: &'a str,
    pub max_results: u8,
    /// `"basic"` or `"advanced"`.
    pub search_depth: &'a str,
    /// Ask Tavily for a synthesized answer. Omitted from the body when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_answer: Option<bool>,
}

/// The parts of a `/search` response the Core uses. Every field is optional so a
/// sparse response still parses.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SearchResponse {
    /// The synthesized answer (only when `include_answer` was requested).
    #[serde(default)]
    pub answer: Option<String>,
    #[serde(default)]
    pub results: Vec<SearchResult>,
}

/// One ranked search hit.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SearchResult {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Serialize)]
struct Body<'a> {
    api_key: &'a str,
    #[serde(flatten)]
    req: &'a SearchRequest<'a>,
}

impl TavilyClient {
    /// A client for the production API.
    pub fn new(api_key: impl Into<String>, timeout: Duration, component: &'static str) -> Self {
        Self::with_base_url(DEFAULT_BASE_URL, api_key, timeout, component)
    }

    /// A client for a specific API root (tests point this at a loopback server).
    pub fn with_base_url(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        timeout: Duration,
        component: &'static str,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            timeout,
            component,
        }
    }

    /// `POST /search`.
    pub async fn search(&self, req: &SearchRequest<'_>) -> Result<SearchResponse> {
        let body = Body {
            api_key: &self.api_key,
            req,
        };
        crate::http::send_json(
            crate::http::post(format!("{}/search", self.base_url), self.component).json(&body),
            self.timeout,
            "Tavily search",
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::test_support::serve_statuses;

    #[tokio::test]
    async fn search_parses_answer_and_results() {
        let (base, server) = serve_statuses(vec![(
            "200 OK",
            r#"{"answer":"A","results":[{"title":"T","url":"https://u","content":"C"},{"url":"https://v"}]}"#.into(),
        )]);
        let client = TavilyClient::with_base_url(base, "k", Duration::from_secs(5), "test");
        let resp = client
            .search(&SearchRequest {
                query: "q",
                max_results: 3,
                search_depth: "basic",
                include_answer: Some(true),
            })
            .await
            .unwrap();
        assert_eq!(resp.answer.as_deref(), Some("A"));
        assert_eq!(resp.results.len(), 2);
        assert_eq!(resp.results[1].url.as_deref(), Some("https://v"));
        assert_eq!(resp.results[1].title, None);
        server.await.unwrap();
    }

    #[test]
    fn body_omits_include_answer_when_unset_and_carries_the_key() {
        let req = SearchRequest {
            query: "q",
            max_results: 5,
            search_depth: "basic",
            include_answer: None,
        };
        let v = serde_json::to_value(Body {
            api_key: "k",
            req: &req,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"api_key":"k","query":"q","max_results":5,"search_depth":"basic"})
        );
    }
}
