//! Entity/topic extraction for the GraphRAG memory (memory_plan.md Goal 4).
//!
//! The background ingester turns each conversation turn into graph structure: not
//! just a vector, but `Entity` nodes (people, places, things, topics) linked to the
//! turn, so retrieval can traverse relationships and not only nearest-neighbor
//! vectors. Extraction is a **generative** step (read text → emit structured
//! entities), so it needs a chat model — an embedding model cannot do it.
//!
//! Production uses [`AnthropicEntityExtractor`] (Claude Haiku 4.5, chosen in the
//! plan: cheap, good structured output, runs in the background so latency is
//! irrelevant). Tests use [`MockEntityExtractor`], a deterministic keyword matcher
//! so the pipeline is verifiable with no network or API key.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

/// A named entity extracted from conversation text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    /// Canonical surface form (e.g. `"jazz"`, `"Portland"`, `"Sam"`).
    pub name: String,
    /// Coarse type: `person` | `place` | `thing` | `topic` (free-form; used only
    /// as a node property, not enforced).
    pub kind: String,
}

/// A swappable entity extractor. `Send + Sync` for sharing behind an `Arc`.
#[async_trait]
pub trait EntityExtractor: Send + Sync {
    /// Extract entities/topics from `text`. Returning an empty vec is fine (many
    /// turns have nothing worth a node). Errors should be transient/network only;
    /// the ingester logs and continues.
    async fn extract(&self, text: &str) -> Result<Vec<Entity>>;
}

/// Anthropic-backed extractor (Claude Haiku 4.5). Non-streaming `POST /v1/messages`
/// asking for a compact JSON array of `{name, kind}`.
pub struct AnthropicEntityExtractor {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

const ANTHROPIC_VERSION: &str = "2023-06-01";

impl AnthropicEntityExtractor {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    fn system_prompt() -> &'static str {
        "You extract named entities and topics from a short conversation turn for a \
         personal assistant's memory graph. Return ONLY a compact JSON array of \
         objects with keys \"name\" and \"kind\". \"kind\" is one of: person, place, \
         thing, topic. Use the canonical lowercase surface form for topics/things and \
         proper casing for names/places. If nothing is worth remembering, return []. \
         No prose, no code fences, JSON only."
    }
}

#[async_trait]
impl EntityExtractor for AnthropicEntityExtractor {
    async fn extract(&self, text: &str) -> Result<Vec<Entity>> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let body = json!({
            "model": self.model,
            "max_tokens": 512,
            "system": Self::system_prompt(),
            "messages": [{"role": "user", "content": text}],
        });
        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("POST /v1/messages to Anthropic (entity extraction)")?;
        let status = resp.status();
        let payload = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("Anthropic extraction HTTP {status}: {payload}");
        }
        let parsed: MessagesResponse =
            serde_json::from_str(&payload).context("parsing Anthropic messages response")?;
        let joined: String = parsed
            .content
            .into_iter()
            .filter_map(|b| b.text)
            .collect::<Vec<_>>()
            .join("");
        Ok(parse_entities_json(&joined))
    }
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct RawEntity {
    name: String,
    #[serde(default)]
    kind: Option<String>,
}

/// Parse the model's reply into entities. Tolerates code fences and surrounding
/// prose by extracting the first `[...]` array; drops blank names; dedupes.
pub fn parse_entities_json(reply: &str) -> Vec<Entity> {
    let slice = match (reply.find('['), reply.rfind(']')) {
        (Some(a), Some(b)) if b > a => &reply[a..=b],
        _ => return Vec::new(),
    };
    let raw: Vec<RawEntity> = serde_json::from_str(slice).unwrap_or_default();
    let mut out: Vec<Entity> = Vec::new();
    for r in raw {
        let name = r.name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let kind = r
            .kind
            .map(|k| k.trim().to_lowercase())
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| "topic".to_string());
        if !out.iter().any(|e| e.name.eq_ignore_ascii_case(&name)) {
            out.push(Entity { name, kind });
        }
    }
    out
}

/// Extractor that always returns no entities. Used when no extraction model is
/// configured (e.g. `ANTHROPIC_API_KEY` absent): the graph still gets turns +
/// vectors, just no entity nodes/edges, so recall degrades to pure vector KNN.
#[derive(Default)]
pub struct NoopEntityExtractor;

#[async_trait]
impl EntityExtractor for NoopEntityExtractor {
    async fn extract(&self, _text: &str) -> Result<Vec<Entity>> {
        Ok(Vec::new())
    }
}

/// Deterministic keyword-matching extractor for tests (no network). Recognizes a
/// small fixed vocabulary so integration tests can assert on graph structure.
#[derive(Default)]
pub struct MockEntityExtractor;

#[async_trait]
impl EntityExtractor for MockEntityExtractor {
    async fn extract(&self, text: &str) -> Result<Vec<Entity>> {
        const VOCAB: &[(&str, &str)] = &[
            ("jazz", "topic"),
            ("music", "topic"),
            ("weather", "topic"),
            ("portland", "place"),
            ("coffee", "thing"),
            ("sam", "person"),
        ];
        let lower = text.to_lowercase();
        let mut out = Vec::new();
        for (kw, kind) in VOCAB {
            if lower.contains(kw) {
                out.push(Entity {
                    name: kw.to_string(),
                    kind: kind.to_string(),
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_array_with_fences_and_prose() {
        let reply = "Here you go:\n```json\n[{\"name\":\"jazz\",\"kind\":\"topic\"},{\"name\":\"Portland\",\"kind\":\"place\"}]\n```";
        let ents = parse_entities_json(reply);
        assert_eq!(ents.len(), 2);
        assert_eq!(ents[0].name, "jazz");
        assert_eq!(ents[1].kind, "place");
    }

    #[test]
    fn empty_or_garbage_yields_nothing() {
        assert!(parse_entities_json("no json here").is_empty());
        assert!(parse_entities_json("[]").is_empty());
    }

    #[tokio::test]
    async fn mock_extractor_finds_vocab() {
        let ents = MockEntityExtractor
            .extract("I love jazz music")
            .await
            .unwrap();
        assert!(ents.iter().any(|e| e.name == "jazz"));
        assert!(ents.iter().any(|e| e.name == "music"));
    }

    /// Exercise the real Anthropic HTTP client + response parsing against a canned
    /// Messages response served from a local socket (no network, no API key).
    #[tokio::test]
    async fn anthropic_extractor_parses_messages_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            // Messages API shape: content is an array of text blocks; our text block
            // holds the JSON entity array (possibly with surrounding prose).
            let body = r#"{"content":[{"type":"text","text":"[{\"name\":\"jazz\",\"kind\":\"topic\"},{\"name\":\"Portland\",\"kind\":\"place\"}]"}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });

        let x = AnthropicEntityExtractor::new(format!("http://{addr}"), "k", "claude-haiku-4-5");
        let ents = x.extract("I love jazz in Portland").await.unwrap();
        assert_eq!(ents.len(), 2);
        assert_eq!(ents[0].name, "jazz");
        assert_eq!(ents[1].name, "Portland");
        server.await.unwrap();
    }
}
