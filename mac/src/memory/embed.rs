//! Text embeddings for the GraphRAG memory (memory_plan.md Goal 3).
//!
//! Every backend implements one trait — [`Embedder`] — so the ingester and the
//! query path never name a concrete provider. Production uses
//! [`OpenAiEmbedder`] (`text-embedding-3-small`, 1536 dims); tests use
//! [`MockEmbedder`], a deterministic hash-based embedder that needs no network or
//! API key, so the whole memory pipeline is verifiable offline.
//!
//! **Privacy note (memory_plan.md):** the OpenAI embedder sends text to OpenAI.
//! The owner accepted this for v1; there is no local fallback yet.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::json;

/// `text-embedding-3-small` native dimensionality.
pub const OPENAI_SMALL_DIMS: usize = 1536;

/// A swappable text embedder. `Send + Sync` so one instance is shared behind an
/// `Arc` across the background ingester and per-turn query embedding.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed a batch of texts, returning one vector per input in order.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// The dimensionality every returned vector has. Used to declare the HelixDB
    /// vector index, so it must be stable for a given embedder instance.
    fn dimensions(&self) -> usize;

    /// Convenience: embed a single text.
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed(&[text.to_string()]).await?;
        v.pop()
            .context("embedder returned no vector for a single input")
    }
}

/// Cloud embedder backed by OpenAI's `POST /v1/embeddings`. Talks raw HTTP with
/// `reqwest` (same pattern as the Anthropic LLM backend); the API key comes from
/// the caller (config reads `OPENAI_API_KEY`).
pub struct OpenAiEmbedder {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    dimensions: usize,
}

impl OpenAiEmbedder {
    /// `model` defaults to `text-embedding-3-small` at the config layer.
    /// `dimensions` lets us request OpenAI's dimension-reduction (<= native);
    /// pass [`OPENAI_SMALL_DIMS`] for the full-size vectors.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        dimensions: usize,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            dimensions,
        }
    }
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let body = json!({
            "model": self.model,
            "input": texts,
            "dimensions": self.dimensions,
        });
        let resp = self
            .client
            .post(format!("{}/v1/embeddings", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("sending OpenAI embeddings request")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("OpenAI embeddings HTTP {status}: {text}");
        }
        let parsed: EmbeddingsResponse =
            serde_json::from_str(&text).context("parsing OpenAI embeddings response")?;
        // The API preserves input order and echoes each `index`; sort defensively.
        let mut data = parsed.data;
        data.sort_by_key(|d| d.index);
        Ok(data.into_iter().map(|d| d.embedding).collect())
    }
}

#[derive(serde::Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(serde::Deserialize)]
struct EmbeddingDatum {
    index: usize,
    embedding: Vec<f32>,
}

/// Deterministic, network-free embedder for tests. Maps text to a fixed-dim vector
/// via a simple token-hash bag-of-words, then L2-normalizes it. Not semantically
/// meaningful in general, but *stable* and *similarity-preserving for shared
/// tokens*, which is enough to exercise vector search end-to-end offline.
pub struct MockEmbedder {
    dims: usize,
}

impl MockEmbedder {
    pub fn new(dims: usize) -> Self {
        Self { dims }
    }
}

impl Default for MockEmbedder {
    fn default() -> Self {
        // Small dims keep test indexes cheap.
        Self { dims: 16 }
    }
}

#[async_trait]
impl Embedder for MockEmbedder {
    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| embed_mock(t, self.dims)).collect())
    }
}

fn embed_mock(text: &str, dims: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dims];
    for token in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let mut h: u64 = 1469598103934665603; // FNV-1a offset basis
        for b in token.to_lowercase().bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        let idx = (h % dims as u64) as usize;
        v[idx] += 1.0;
    }
    // L2-normalize so cosine similarity behaves; empty text → tiny nonzero vector
    // (a zero vector has undefined direction for cosine).
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    } else {
        v[0] = 1.0;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_embedder_is_deterministic_and_normalized() {
        let e = MockEmbedder::new(16);
        let a = e.embed_one("I love jazz music").await.unwrap();
        let b = e.embed_one("I love jazz music").await.unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        let norm = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn shared_tokens_are_more_similar() {
        let e = MockEmbedder::new(64);
        let dot = |x: &[f32], y: &[f32]| x.iter().zip(y).map(|(a, b)| a * b).sum::<f32>();
        let jazz = e.embed_one("i love jazz music").await.unwrap();
        let jazz2 = e.embed_one("jazz music is great").await.unwrap();
        let weather = e.embed_one("what is the weather today").await.unwrap();
        assert!(
            dot(&jazz, &jazz2) > dot(&jazz, &weather),
            "shared-token texts should score higher"
        );
    }

    /// Exercise the real HTTP client + response parsing against a canned OpenAI
    /// response served from a local socket (no network, no API key). Also checks
    /// out-of-order `index` fields are sorted back into input order.
    #[tokio::test]
    async fn openai_embedder_parses_batched_response_in_order() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            // Two vectors, deliberately returned index 1 before index 0.
            let body =
                r#"{"data":[{"index":1,"embedding":[0.0,1.0]},{"index":0,"embedding":[1.0,0.0]}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });

        let e = OpenAiEmbedder::new(format!("http://{addr}"), "k", "text-embedding-3-small", 2);
        let vecs = e
            .embed(&["first".to_string(), "second".to_string()])
            .await
            .unwrap();
        assert_eq!(vecs.len(), 2);
        assert_eq!(vecs[0], vec![1.0, 0.0], "index 0 must come first");
        assert_eq!(vecs[1], vec![0.0, 1.0]);
        server.await.unwrap();
    }
}
