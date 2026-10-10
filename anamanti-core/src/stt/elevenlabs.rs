//! **ElevenLabs Scribe v2 Realtime** STT engine — a WebSocket client behind the
//! [`Transcriber`] / [`SttEngine`] seam (see `plans/ElevenLabsSttPlan.md`).
//!
//! Always compiled (no cargo feature): a pure-Rust network client with no native
//! toolchain dependency, like the cloud LLM backends. It activates only when
//! `stt.engine = "elevenlabs"` is selected and the `ELEVENLABS_API_KEY` secret is set.
//!
//! The engine connects with `commit_strategy=manual`, so **the Core's VAD stays the
//! end-of-speech authority**: the pump loop forwards PCM via [`forward_pcm`], and when
//! the VAD fires it calls [`finish`], which sends a `commit:true` chunk. ElevenLabs
//! streams `partial_transcript` (interim) events during speech — which we **discard**,
//! final-only — and emits exactly one `committed_transcript` after the commit, which
//! [`read_event`] surfaces as the single [`SttEvent::Transcript`]. This mirrors the
//! in-process whisper engine's "pending until finish, then yield once" contract, so it
//! drops into the existing two-arm `select!` loop unchanged.
//!
//! [`forward_pcm`]: Transcriber::forward_pcm
//! [`finish`]: Transcriber::finish
//! [`read_event`]: Transcriber::read_event

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::wyoming::protocol::AudioFormat;

use super::{SttEngine, SttEvent, Transcriber};

/// The ElevenLabs realtime model id (the only one this endpoint accepts today).
pub const DEFAULT_MODEL_ID: &str = "scribe_v2_realtime";
/// The global production WebSocket host (regional residency hosts also exist).
pub const DEFAULT_BASE_URL: &str = "wss://api.elevenlabs.io";

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// [`SttEngine`] adapter for ElevenLabs realtime STT, held by the pipeline. Holds the
/// connection config + API key; each turn's [`begin`](SttEngine::begin) dials a fresh
/// WebSocket.
pub struct ElevenLabsEngine {
    base_url: String,
    api_key: String,
    model_id: String,
    /// Decode language (ISO 639); `None` ⇒ let the model auto-detect.
    language: Option<String>,
}

impl ElevenLabsEngine {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model_id: impl Into<String>,
        language: Option<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model_id: model_id.into(),
            language,
        }
    }
}

#[async_trait]
impl SttEngine for ElevenLabsEngine {
    async fn begin(&self, format: AudioFormat) -> Result<Box<dyn Transcriber>> {
        let session = ElevenLabsTranscriber::connect(
            &self.base_url,
            &self.api_key,
            &self.model_id,
            self.language.as_deref(),
            format.rate,
        )
        .await?;
        Ok(Box::new(session))
    }
}

/// Map a PCM sample rate to the ElevenLabs `audio_format` query value. The device
/// streams 16 kHz, which matches `pcm_16000`; other supported rates are mapped
/// defensively, falling back to 16 kHz.
fn audio_format_param(rate: u32) -> &'static str {
    match rate {
        8_000 => "pcm_8000",
        16_000 => "pcm_16000",
        22_050 => "pcm_22050",
        24_000 => "pcm_24000",
        44_100 => "pcm_44100",
        48_000 => "pcm_48000",
        _ => "pcm_16000",
    }
}

/// Is this server `message_type` one of the error variants that should fail the turn?
fn is_error_message_type(mt: &str) -> bool {
    matches!(
        mt,
        "error"
            | "auth_error"
            | "quota_exceeded"
            | "unaccepted_terms"
            | "rate_limited"
            | "queue_overflow"
            | "resource_exhausted"
            | "session_time_limit_exceeded"
            | "input_error"
            | "invalid_request"
            | "chunk_size_exceeded"
            | "transcriber_error"
    )
}

/// One in-flight realtime transcription over an open WebSocket. `forward_pcm` streams
/// `input_audio_chunk` frames; `finish` commits; `read_event` reads frames, silently
/// consuming partials/`insufficient_audio_activity` until the committed transcript (or
/// a hard error / close) arrives.
pub struct ElevenLabsTranscriber {
    ws: Ws,
    sample_rate: u32,
    /// Latched by the first `finish` so the commit is sent exactly once (idempotent).
    finished: bool,
}

impl ElevenLabsTranscriber {
    /// Open a realtime session: dial the WebSocket (with the `xi-api-key` header and
    /// the connection config as query params) and await the `session_started` ack.
    async fn connect(
        base_url: &str,
        api_key: &str,
        model_id: &str,
        language: Option<&str>,
        sample_rate: u32,
    ) -> Result<Self> {
        let mut url = format!(
            "{base}/v1/speech-to-text/realtime?model_id={model}&audio_format={fmt}&commit_strategy=manual",
            base = base_url.trim_end_matches('/'),
            model = model_id,
            fmt = audio_format_param(sample_rate),
        );
        if let Some(l) = language {
            if !l.is_empty() {
                url.push_str("&language_code=");
                url.push_str(l);
            }
        }

        let mut request = url
            .into_client_request()
            .context("building ElevenLabs realtime STT request")?;
        request.headers_mut().insert(
            "xi-api-key",
            HeaderValue::from_str(api_key).context("ELEVENLABS_API_KEY has invalid characters")?,
        );

        let (mut ws, _resp) = connect_async(request)
            .await
            .context("connecting to ElevenLabs realtime STT")?;

        // Await the session handshake so a bad key / quota fails at begin() rather
        // than mid-turn.
        loop {
            match ws.next().await {
                None => bail!("ElevenLabs STT closed before session_started"),
                Some(Err(e)) => {
                    return Err(e).context("ElevenLabs STT handshake read failed");
                }
                Some(Ok(Message::Text(t))) => {
                    let v: serde_json::Value = serde_json::from_str(&t)
                        .context("parsing ElevenLabs STT handshake frame")?;
                    match v.get("message_type").and_then(|m| m.as_str()) {
                        Some("session_started") => break,
                        Some(mt) if is_error_message_type(mt) => {
                            let err = v.get("error").and_then(|e| e.as_str()).unwrap_or(mt);
                            bail!("ElevenLabs STT error on connect ({mt}): {err}");
                        }
                        // Ignore any pre-session chatter (warnings, etc.).
                        _ => continue,
                    }
                }
                // Ignore non-text control frames during the handshake.
                Some(Ok(_)) => continue,
            }
        }

        Ok(Self {
            ws,
            sample_rate,
            finished: false,
        })
    }

    /// Send one `input_audio_chunk` frame.
    async fn send_chunk(&mut self, audio_base_64: &str, commit: bool) -> Result<()> {
        let frame = serde_json::json!({
            "message_type": "input_audio_chunk",
            "audio_base_64": audio_base_64,
            "commit": commit,
            "sample_rate": self.sample_rate,
        });
        self.ws
            .send(Message::Text(frame.to_string()))
            .await
            .context("sending audio chunk to ElevenLabs STT")?;
        Ok(())
    }
}

#[async_trait]
impl Transcriber for ElevenLabsTranscriber {
    async fn forward_pcm(&mut self, pcm: Vec<u8>) -> Result<()> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&pcm);
        self.send_chunk(&b64, false).await
    }

    async fn read_event(&mut self) -> Result<Option<SttEvent>> {
        // Read until something meaningful: the committed transcript (final), a hard
        // error, or the socket closing. Partials and other non-final frames are
        // consumed and discarded (final-only; see the module doc), which keeps this
        // future "pending" from the pump loop's perspective until end-of-speech —
        // mirroring the whisper engine. `StreamExt::next` is cancellation-safe, so the
        // two-arm `select!` may drop this future between frames without data loss.
        loop {
            match self.ws.next().await {
                None => return Ok(None),
                Some(Err(e)) => return Err(e).context("ElevenLabs STT read failed"),
                Some(Ok(Message::Text(t))) => {
                    let v: serde_json::Value =
                        serde_json::from_str(&t).context("parsing ElevenLabs STT frame")?;
                    match v.get("message_type").and_then(|m| m.as_str()) {
                        Some("committed_transcript") => {
                            let text = v
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or_default()
                                .trim()
                                .to_string();
                            return Ok(Some(SttEvent::Transcript(text)));
                        }
                        // Not enough speech to transcribe after the commit → treat as
                        // an empty utterance, not a hard error. The orchestrator's
                        // no-speech guard already handles the empty transcript.
                        Some("insufficient_audio_activity") => {
                            return Ok(Some(SttEvent::Transcript(String::new())));
                        }
                        Some(mt) if is_error_message_type(mt) => {
                            let err = v.get("error").and_then(|e| e.as_str()).unwrap_or(mt);
                            return Err(anyhow!("ElevenLabs STT error ({mt}): {err}"));
                        }
                        // partial_transcript, session_started, *_with_timestamps,
                        // *_entities, edited_transcript, warning, commit_throttled, … —
                        // discard and keep reading.
                        _ => continue,
                    }
                }
                // Binary frames aren't used by this API; control frames (ping/pong) and
                // close-frame acks are handled by the stream — keep reading.
                Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Ok(_)) => continue,
            }
        }
    }

    async fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(()); // idempotent: the loop and its caller may both finalize
        }
        self.finished = true;
        // Commit the segment. All speech PCM has already been streamed, so the commit
        // carries no new audio.
        self.send_chunk("", true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{
        ErrorResponse, Request as HsRequest, Response as HsResponse,
    };

    /// Drive the engine against a mock ElevenLabs realtime server over a plain `ws://`
    /// socket: asserts the connection request shape (query params + `xi-api-key`), that
    /// `forward_pcm` streams `input_audio_chunk` frames, that `finish` sets the commit
    /// flag, that partial transcripts are discarded, and that the `committed_transcript`
    /// surfaces as the single final [`SttEvent::Transcript`].
    #[tokio::test]
    async fn realtime_roundtrip_commits_and_returns_committed_transcript() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let saw_commit = Arc::new(Mutex::new(false));
        let captured_path = Arc::new(Mutex::new(String::new()));
        let captured_key = Arc::new(Mutex::new(String::new()));
        let chunk_count = Arc::new(Mutex::new(0usize));

        let (saw_commit_s, cp, ck, cc) = (
            saw_commit.clone(),
            captured_path.clone(),
            captured_key.clone(),
            chunk_count.clone(),
        );

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let callback =
                |req: &HsRequest, resp: HsResponse| -> Result<HsResponse, ErrorResponse> {
                    *cp.lock().unwrap() = req.uri().to_string();
                    if let Some(k) = req.headers().get("xi-api-key") {
                        *ck.lock().unwrap() = k.to_str().unwrap_or_default().to_string();
                    }
                    Ok(resp)
                };
            let mut ws = accept_hdr_async(stream, callback).await.unwrap();

            // Session handshake first.
            ws.send(Message::Text(
                r#"{"message_type":"session_started","session_id":"s1","config":{}}"#.into(),
            ))
            .await
            .unwrap();

            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(t) = msg {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    assert_eq!(v["message_type"], "input_audio_chunk");
                    *cc.lock().unwrap() += 1;
                    if v["commit"].as_bool().unwrap_or(false) {
                        *saw_commit_s.lock().unwrap() = true;
                        // A partial (should be discarded) then the committed transcript.
                        ws.send(Message::Text(
                            r#"{"message_type":"partial_transcript","text":"hello"}"#.into(),
                        ))
                        .await
                        .unwrap();
                        ws.send(Message::Text(
                            r#"{"message_type":"committed_transcript","text":" hello world "}"#
                                .into(),
                        ))
                        .await
                        .unwrap();
                        break;
                    }
                }
            }
        });

        let engine = ElevenLabsEngine::new(
            format!("ws://{addr}"),
            "test-key",
            DEFAULT_MODEL_ID,
            Some("en".to_string()),
        );
        let mut t = engine.begin(AudioFormat::PCM_16K_MONO).await.unwrap();
        t.forward_pcm(vec![0u8, 1, 2, 3]).await.unwrap();
        t.finish().await.unwrap();
        // finish is idempotent — a second call is a no-op and sends nothing.
        t.finish().await.unwrap();

        match t.read_event().await.unwrap() {
            Some(SttEvent::Transcript(s)) => assert_eq!(s, "hello world"),
            other => panic!("expected a final transcript, got {other:?}"),
        }

        server.await.unwrap();
        assert!(
            *saw_commit.lock().unwrap(),
            "server never saw a commit:true chunk"
        );
        assert_eq!(
            *chunk_count.lock().unwrap(),
            2,
            "expected one audio chunk + one commit chunk"
        );
        assert_eq!(*captured_key.lock().unwrap(), "test-key");
        let path = captured_path.lock().unwrap().clone();
        assert!(path.contains("model_id=scribe_v2_realtime"), "path: {path}");
        assert!(path.contains("audio_format=pcm_16000"), "path: {path}");
        assert!(path.contains("commit_strategy=manual"), "path: {path}");
        assert!(path.contains("language_code=en"), "path: {path}");
    }
}
