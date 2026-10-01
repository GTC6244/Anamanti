//! Text embeddings for the GraphRAG memory (memory_plan.md Goal 3).
//!
//! Every backend implements one trait — [`Embedder`] — so the ingester and the
//! query path never name a concrete provider. Three backends:
//! - [`LocalNomicEmbedder`] (feature `embed-local`, the default) — nomic-embed-text-v1.5
//!   run in-process via `fastembed`/onnxruntime; **offline, no API key**, 768 dims.
//! - [`OpenAiEmbedder`] (`text-embedding-3-small`, 1536 dims) — opt-in cloud backend
//!   (`graphrag.embed_backend = "openai"`) for those who prefer OpenAI's vectors.
//! - [`MockEmbedder`] — a deterministic hash-based embedder that needs no network or
//!   model, so the whole memory pipeline is verifiable offline in tests.
//!
//! **Privacy note (memory_plan.md):** the local nomic embedder is the default and
//! keeps all memory text on-device. The OpenAI embedder (opt-in) sends text to OpenAI.
//!
//! nomic is an *asymmetric* embedder: stored documents and search queries use different
//! instruction prefixes, so the ingester calls [`Embedder::embed`] (documents) while the
//! recall path calls [`Embedder::embed_query`] (queries). See those methods.

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

    /// Short provider label for diagnostics/UI: `"local"`, `"openai"`, `"mock"`.
    fn label(&self) -> &'static str {
        "embedding"
    }

    /// Convenience: embed a single text.
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed(&[text.to_string()]).await?;
        v.pop()
            .context("embedder returned no vector for a single input")
    }

    /// Embed a single text as a **search query** (recall time) rather than a stored
    /// document (ingest time). Asymmetric-embedding models — e.g.
    /// nomic-embed-text-v1.5, which prepends `search_query:` vs `search_document:` —
    /// must embed queries differently from documents, so the recall path calls this
    /// while the ingester calls [`Embedder::embed`]. Symmetric embedders (OpenAI,
    /// the mock) keep the default, which is identical to [`Embedder::embed_one`].
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_one(text).await
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
        // Reuse one keep-alive connection across the many embedding calls (per-turn
        // recall + the background ingester share this client). The shared, keep-alive-
        // tuned client keeps the pooled TLS connection warm between turns, saving the
        // DNS+TCP+TLS handshake (~hundreds of ms) on the next request. See `crate::http`.
        let client = crate::http::shared_client();
        Self {
            client,
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

    fn label(&self) -> &'static str {
        "openai"
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Request base64-encoded vectors: OpenAI returns each embedding as
        // little-endian f32 bytes in base64, which is far smaller on the wire than a
        // JSON array of decimal floats (~1536 numbers), so the response transfers and
        // parses faster. We decode it back to `Vec<f32>` below.
        let body = json!({
            "model": self.model,
            "input": texts,
            "dimensions": self.dimensions,
            "encoding_format": "base64",
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
        data.into_iter().map(|d| d.embedding.into_vec()).collect()
    }
}

#[derive(serde::Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(serde::Deserialize)]
struct EmbeddingDatum {
    index: usize,
    embedding: EmbeddingData,
}

/// One embedding as returned by the API. We request `encoding_format="base64"`
/// (the [`EmbeddingData::Base64`] arm), but the untagged enum also accepts a plain
/// float array so a `float`-format response (or an API that ignores the param)
/// still parses.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum EmbeddingData {
    /// base64 of the raw little-endian `f32` bytes.
    Base64(String),
    /// A JSON array of floats (`encoding_format="float"`).
    Floats(Vec<f32>),
}

impl EmbeddingData {
    /// Materialize the vector, decoding the base64 arm from little-endian f32 bytes.
    fn into_vec(self) -> Result<Vec<f32>> {
        match self {
            EmbeddingData::Floats(v) => Ok(v),
            EmbeddingData::Base64(s) => decode_base64_f32(&s),
        }
    }
}

/// Decode an OpenAI base64 embedding: standard base64 → raw bytes → little-endian
/// `f32`s. Errors if the payload isn't valid base64 or isn't a whole number of
/// 4-byte floats.
fn decode_base64_f32(s: &str) -> Result<Vec<f32>> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .context("decoding base64 embedding")?;
    anyhow::ensure!(
        bytes.len() % 4 == 0,
        "base64 embedding byte length {} is not a multiple of 4",
        bytes.len()
    );
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// nomic's native embedding width. Matryoshka lets callers truncate to a smaller
/// `dimensions()` (then re-normalize); the full vector is 768-d.
#[cfg(feature = "embed-local")]
pub const NOMIC_FULL_DIMS: usize = 768;

/// Local, offline text embedder: **nomic-embed-text-v1.5** run in-process via
/// `fastembed` (onnxruntime through `ort`). The default memory embedder — no network,
/// no API key, so all memory text stays on-device.
///
/// nomic is asymmetric: [`Embedder::embed`] prefixes each input with `search_document: `
/// (stored memory) and [`Embedder::embed_query`] with `search_query: ` (recall). fastembed
/// mean-pools and L2-normalizes; with a `dimensions()` below [`NOMIC_FULL_DIMS`] we apply
/// Matryoshka truncation and re-normalize.
///
/// The model (`onnx/model.onnx`) and the four tokenizer files (`tokenizer.json`,
/// `config.json`, `special_tokens_map.json`, `tokenizer_config.json`) are loaded from
/// local disk, provisioned by `scripts/fetch-embed-model.sh`.
#[cfg(feature = "embed-local")]
pub struct LocalNomicEmbedder {
    // fastembed's `TextEmbedding::embed` takes `&mut self`; a blocking Mutex guards it,
    // and all inference runs inside `spawn_blocking` (onnxruntime is CPU-bound, so it
    // must stay off the async executor threads).
    inner: std::sync::Arc<std::sync::Mutex<fastembed::TextEmbedding>>,
    dims: usize,
}

#[cfg(feature = "embed-local")]
impl LocalNomicEmbedder {
    const DOC_PREFIX: &'static str = "search_document: ";
    const QUERY_PREFIX: &'static str = "search_query: ";
    /// Truncation cap on tokenizer input length (nomic supports long context; memory
    /// chunks are short, and fastembed pads only to each batch's max, not to this).
    const MAX_LENGTH: usize = 2048;

    /// Load the nomic ONNX model from `model_path` and the four tokenizer JSON files
    /// from `tokenizer_dir`, producing `dims`-length vectors (<= [`NOMIC_FULL_DIMS`]).
    pub fn open(
        model_path: impl AsRef<std::path::Path>,
        tokenizer_dir: impl AsRef<std::path::Path>,
        dims: usize,
    ) -> Result<Self> {
        use fastembed::{
            InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles,
            UserDefinedEmbeddingModel,
        };
        anyhow::ensure!(
            dims > 0 && dims <= NOMIC_FULL_DIMS,
            "embed_dims {dims} out of range for nomic-embed-text-v1.5 (1..={NOMIC_FULL_DIMS})"
        );
        let model_path = model_path.as_ref();
        let dir = tokenizer_dir.as_ref();
        let onnx_file = std::fs::read(model_path)
            .with_context(|| format!("reading embedding model {}", model_path.display()))?;
        let read = |name: &str| -> Result<Vec<u8>> {
            let p = dir.join(name);
            std::fs::read(&p).with_context(|| format!("reading tokenizer file {}", p.display()))
        };
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: read("tokenizer.json")?,
            config_file: read("config.json")?,
            special_tokens_map_file: read("special_tokens_map.json")?,
            tokenizer_config_file: read("tokenizer_config.json")?,
        };
        let model = UserDefinedEmbeddingModel::new(onnx_file, tokenizer_files)
            .with_pooling(Pooling::Mean);
        let options = InitOptionsUserDefined::new().with_max_length(Self::MAX_LENGTH);
        let embedder = TextEmbedding::try_new_from_user_defined(model, options)
            .map_err(|e| anyhow::anyhow!("initializing nomic embedder: {e}"))?;
        Ok(Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(embedder)),
            dims,
        })
    }

    /// Prefix every input, run fastembed (mean-pool + L2-normalize), then apply
    /// Matryoshka truncation + re-normalization if `dims < NOMIC_FULL_DIMS`.
    async fn embed_prefixed(&self, prefix: &'static str, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let inputs: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        let inner = self.inner.clone();
        let dims = self.dims;
        tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>> {
            let mut guard = inner
                .lock()
                .map_err(|_| anyhow::anyhow!("nomic embedder mutex poisoned"))?;
            let mut vectors = guard
                .embed(inputs, None)
                .map_err(|e| anyhow::anyhow!("nomic embed: {e}"))?;
            if dims < NOMIC_FULL_DIMS {
                for v in &mut vectors {
                    v.truncate(dims);
                    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if norm > 0.0 {
                        for x in v.iter_mut() {
                            *x /= norm;
                        }
                    }
                }
            }
            Ok(vectors)
        })
        .await
        .context("joining nomic embed task")?
    }
}

#[cfg(feature = "embed-local")]
#[async_trait]
impl Embedder for LocalNomicEmbedder {
    fn dimensions(&self) -> usize {
        self.dims
    }

    fn label(&self) -> &'static str {
        "local"
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_prefixed(Self::DOC_PREFIX, texts).await
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self
            .embed_prefixed(Self::QUERY_PREFIX, &[text.to_string()])
            .await?;
        v.pop()
            .context("nomic embedder returned no vector for a single query")
    }
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

    fn label(&self) -> &'static str {
        "mock"
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

    #[test]
    fn decode_base64_f32_round_trips_little_endian() {
        use base64::Engine;
        let vals: Vec<f32> = vec![1.0, 0.0, -2.5, 42.25];
        let mut bytes = Vec::new();
        for v in &vals {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        assert_eq!(decode_base64_f32(&b64).unwrap(), vals);
        // A non-multiple-of-4 byte length is rejected.
        let bad = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]);
        assert!(decode_base64_f32(&bad).is_err());
    }

    /// The real client + parser against a canned **base64** response (the format we
    /// now request), confirming we decode little-endian f32 bytes back to the vector.
    #[tokio::test]
    async fn openai_embedder_decodes_base64_response() {
        use base64::Engine;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let b64 = |v: &[f32]| {
            let mut bytes = Vec::new();
            for x in v {
                bytes.extend_from_slice(&x.to_le_bytes());
            }
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };
        let body = format!(
            r#"{{"data":[{{"index":0,"embedding":"{}"}}]}}"#,
            b64(&[1.0, 0.0, -2.5])
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });

        let e = OpenAiEmbedder::new(format!("http://{addr}"), "k", "text-embedding-3-small", 3);
        let vecs = e.embed(&["hello".to_string()]).await.unwrap();
        assert_eq!(vecs, vec![vec![1.0, 0.0, -2.5]]);
        server.await.unwrap();
    }

    /// Symmetric embedders inherit the default `embed_query`, which must be identical
    /// to `embed_one` (same vector for document and query). Only asymmetric backends
    /// (nomic) override it.
    #[tokio::test]
    async fn default_embed_query_matches_embed_one() {
        let e = MockEmbedder::new(32);
        let q = e.embed_query("I love jazz music").await.unwrap();
        let d = e.embed_one("I love jazz music").await.unwrap();
        assert_eq!(q, d);
    }

    // ---- Local nomic embedder (feature `embed-local`) ----------------------
    // These need the provisioned model on disk (scripts/fetch-embed-model.sh); they
    // skip cleanly when it's absent so CI without the model stays green.
    #[cfg(feature = "embed-local")]
    mod local_nomic {
        use super::*;

        fn model_paths() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
            let model = std::path::PathBuf::from("models/nomic-embed-text-v1.5.onnx");
            let tok = std::path::PathBuf::from("models/nomic-tokenizer");
            (model.exists() && tok.join("tokenizer.json").exists()).then_some((model, tok))
        }

        fn cosine(a: &[f32], b: &[f32]) -> f32 {
            a.iter().zip(b).map(|(x, y)| x * y).sum()
        }

        #[tokio::test]
        async fn embeds_full_dim_normalized_and_semantic() {
            let Some((model, tok)) = model_paths() else {
                eprintln!("skip: nomic model not provisioned (scripts/fetch-embed-model.sh)");
                return;
            };
            let e = LocalNomicEmbedder::open(&model, &tok, NOMIC_FULL_DIMS).unwrap();
            let docs = e
                .embed(&[
                    "I really love listening to jazz music".to_string(),
                    "the train to the airport leaves at noon".to_string(),
                ])
                .await
                .unwrap();
            assert_eq!(docs.len(), 2);
            assert_eq!(docs[0].len(), NOMIC_FULL_DIMS);
            let norm = docs[0].iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-3, "expected unit length, got {norm}");

            // A jazz query sits closer to the jazz document than the travel one.
            let q = e.embed_query("jazz records are my favorite").await.unwrap();
            assert!(
                cosine(&q, &docs[0]) > cosine(&q, &docs[1]),
                "jazz query should be nearer the jazz document"
            );
        }

        #[tokio::test]
        async fn document_and_query_prefixes_differ() {
            let Some((model, tok)) = model_paths() else {
                eprintln!("skip: nomic model not provisioned");
                return;
            };
            let e = LocalNomicEmbedder::open(&model, &tok, NOMIC_FULL_DIMS).unwrap();
            // Same text embedded as a document vs a query must differ (different prefix),
            // confirming the ingester/recall asymmetry is actually applied.
            let as_doc = e.embed(&["hello world".to_string()]).await.unwrap();
            let as_query = e.embed_query("hello world").await.unwrap();
            assert!(
                cosine(&as_doc[0], &as_query) < 0.999,
                "document and query embeddings should differ"
            );
        }

        #[tokio::test]
        async fn matryoshka_truncates_and_renormalizes() {
            let Some((model, tok)) = model_paths() else {
                eprintln!("skip: nomic model not provisioned");
                return;
            };
            let e = LocalNomicEmbedder::open(&model, &tok, 256).unwrap();
            assert_eq!(e.dimensions(), 256);
            let v = e.embed_one("jazz music").await.unwrap();
            assert_eq!(v.len(), 256, "Matryoshka should truncate to the configured dims");
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-3, "truncated vector must be re-normalized");
        }

        #[test]
        fn rejects_out_of_range_dims() {
            let Some((model, tok)) = model_paths() else {
                return;
            };
            assert!(LocalNomicEmbedder::open(&model, &tok, 0).is_err());
            assert!(LocalNomicEmbedder::open(&model, &tok, NOMIC_FULL_DIMS + 1).is_err());
        }
    }
}
