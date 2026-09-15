//! End-to-end pipeline integration tests (Plan.MD Phase 4).
//!
//! These wire the orchestrator against in-memory mock Whisper (STT) and Piper
//! (TTS) servers plus a mock Echo Show, and drive a full turn through the real
//! [`Pipeline`] — proving STT relay, memory policy, LLM reply, and TTS playback
//! interoperate over the exact Wyoming wire format the device speaks, without any
//! external services.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::io::{split, BufReader, DuplexStream};

use ambient_orchestrator::llm::mock::MockLlm;
use ambient_orchestrator::memory::{MemoryKind, MemoryStore};
use ambient_orchestrator::orchestrator::{Pipeline, ServiceConnector, TurnEvent};
use ambient_orchestrator::wyoming::protocol::{
    read_event, types, write_event, AudioFormat, WyomingEvent,
};
use ambient_orchestrator::wyoming::DynConnection;

/// A connector backed by in-process mock STT/TTS servers over duplex pipes.
struct MockConnector {
    transcript: String,
    /// Captures the text the TTS server was asked to synthesize.
    synthesized: Arc<Mutex<Option<String>>>,
}

impl MockConnector {
    fn new(transcript: &str) -> Self {
        Self {
            transcript: transcript.to_string(),
            synthesized: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait]
impl ServiceConnector for MockConnector {
    async fn connect_stt(&self) -> Result<DynConnection> {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let transcript = self.transcript.clone();
        tokio::spawn(mock_stt(server, transcript));
        let (r, w) = split(client);
        Ok(DynConnection::from_io(r, w))
    }

    async fn connect_tts(&self) -> Result<DynConnection> {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let captured = self.synthesized.clone();
        tokio::spawn(mock_tts(server, captured));
        let (r, w) = split(client);
        Ok(DynConnection::from_io(r, w))
    }
}

/// Mock Whisper: reads `transcribe`/`audio-start`, and once the first forwarded
/// `audio-chunk` arrives (server-side VAD firing) emits the final `transcript`.
async fn mock_stt(server: DuplexStream, transcript: String) {
    let (r, w) = split(server);
    let mut reader = BufReader::new(r);
    let mut writer = w;
    while let Ok(Some(ev)) = read_event(&mut reader).await {
        if ev.event_type == types::AUDIO_CHUNK {
            let _ = write_event(&mut writer, &WyomingEvent::transcript(&transcript)).await;
            break;
        }
    }
    // Stay alive (draining) until the orchestrator drops the connection.
    while let Ok(Some(_)) = read_event(&mut reader).await {}
}

/// Mock Piper: reads the `synthesize` request, records the text, and emits a short
/// audio stream (`audio-start` → one `audio-chunk` → `audio-stop`).
async fn mock_tts(server: DuplexStream, captured: Arc<Mutex<Option<String>>>) {
    let (r, w) = split(server);
    let mut reader = BufReader::new(r);
    let mut writer = w;
    if let Ok(Some(ev)) = read_event(&mut reader).await {
        if ev.event_type == types::SYNTHESIZE {
            let text = ev
                .data
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string();
            *captured.lock().unwrap() = Some(text);
            let fmt = AudioFormat::PCM_16K_MONO;
            for e in [
                WyomingEvent::audio_start(fmt, 0),
                WyomingEvent::audio_chunk(fmt, 0, vec![0u8; 8]),
                WyomingEvent::audio_stop(20),
            ] {
                let _ = write_event(&mut writer, &e).await;
            }
        }
    }
}

/// Drive the device side of a turn: stream audio, then read the relayed transcript,
/// the streamed reply tokens, and the returned TTS audio frames. Returns
/// `(transcript_seen, reply_text, tts_event_kinds)`.
async fn drive_device(io: DuplexStream) -> (String, String, Vec<String>) {
    let (r, w) = split(io);
    let mut reader = BufReader::new(r);
    let mut writer = w;
    let fmt = AudioFormat::PCM_16K_MONO;

    write_event(&mut writer, &WyomingEvent::audio_start(fmt, 0))
        .await
        .unwrap();
    write_event(
        &mut writer,
        &WyomingEvent::audio_chunk(fmt, 0, vec![1, 0, 2, 0]),
    )
    .await
    .unwrap();
    write_event(
        &mut writer,
        &WyomingEvent::audio_chunk(fmt, 20, vec![3, 0, 4, 0]),
    )
    .await
    .unwrap();

    // The relayed transcript arrives first.
    let ev = read_event(&mut reader).await.unwrap().unwrap();
    let transcript = ev.transcript_text().unwrap_or_default().to_string();
    // Real device ends its input stream once it has the transcript.
    write_event(&mut writer, &WyomingEvent::audio_stop(40))
        .await
        .unwrap();

    // Then the streamed reply tokens (Phase 5) arrive, followed by the synthesized
    // reply audio. Accumulate the reply text and collect the TTS frame kinds.
    let mut reply = String::new();
    let mut kinds = Vec::new();
    while let Some(ev) = read_event(&mut reader).await.unwrap() {
        if let Some(tok) = ev.reply_token_text() {
            reply.push_str(tok);
            continue;
        }
        let t = ev.event_type.clone();
        kinds.push(t.clone());
        if t == types::AUDIO_STOP {
            break;
        }
    }
    (transcript, reply, kinds)
}

fn build_pipeline(memory: Arc<MemoryStore>) -> Pipeline {
    Pipeline::new(
        Arc::new(MockLlm::new("You said: {msg}")),
        memory,
        "test persona",
        None,
        Duration::from_secs(5),
    )
}

async fn run_one_turn(
    pipeline: &Pipeline,
    connector: &MockConnector,
) -> ((String, String, Vec<String>), Vec<TurnEvent>) {
    let (dev_pipeline, dev_test) = tokio::io::duplex(64 * 1024);
    let (pr, pw) = split(dev_pipeline);
    let mut device = DynConnection::from_io(pr, pw);

    let device_task = tokio::spawn(drive_device(dev_test));

    let mut events = Vec::new();
    {
        let mut on_event = |e: TurnEvent| events.push(e);
        pipeline
            .run_turn(&mut device, connector, &mut on_event)
            .await
            .expect("turn runs");
    }

    let device_out = device_task.await.unwrap();
    (device_out, events)
}

#[tokio::test]
async fn full_turn_streams_transcript_reply_and_tts_audio() {
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let pipeline = build_pipeline(memory.clone());
    let connector = MockConnector::new("turn on the lights");

    let ((transcript, reply, tts_kinds), events) = run_one_turn(&pipeline, &connector).await;

    assert_eq!(transcript, "turn on the lights");
    // The reply was relayed to the device token-by-token (Phase 5) and reassembles
    // to the full LLM reply.
    assert_eq!(reply, "You said: turn on the lights");
    assert_eq!(
        tts_kinds,
        vec![
            types::AUDIO_START.to_string(),
            types::AUDIO_CHUNK.to_string(),
            types::AUDIO_STOP.to_string(),
        ],
        "device receives a complete TTS audio stream"
    );

    assert!(events.contains(&TurnEvent::Transcript("turn on the lights".to_string())));
    assert!(events.contains(&TurnEvent::Reply(
        "You said: turn on the lights".to_string()
    )));
    assert!(events.contains(&TurnEvent::Speaking));
    assert_eq!(events.last(), Some(&TurnEvent::Finished));

    // Piper was asked to synthesize exactly the LLM reply.
    assert_eq!(
        connector.synthesized.lock().unwrap().as_deref(),
        Some("You said: turn on the lights")
    );
}

#[tokio::test]
async fn inferred_facts_are_persisted_across_a_turn() {
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let pipeline = build_pipeline(memory.clone());
    let connector = MockConnector::new("my name is Sam");

    let (_, events) = run_one_turn(&pipeline, &connector).await;

    assert!(events
        .iter()
        .any(|e| matches!(e, TurnEvent::MemoryStored(c) if c == "The user's name is Sam")));
    let stored = memory.list().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].content, "The user's name is Sam");
    assert_eq!(stored[0].kind, MemoryKind::Fact);
}

#[tokio::test]
async fn explicit_remember_command_short_circuits_the_llm() {
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let pipeline = build_pipeline(memory.clone());
    let connector = MockConnector::new("remember I like my coffee black");

    let (_, events) = run_one_turn(&pipeline, &connector).await;

    // The reply is the confirmation, not the mock LLM's "You said: …" echo.
    assert!(events.contains(&TurnEvent::Reply("Okay, I'll remember that.".to_string())));
    // The confirmation is what gets synthesized.
    assert_eq!(
        connector.synthesized.lock().unwrap().as_deref(),
        Some("Okay, I'll remember that.")
    );

    let stored = memory.list().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].content, "I like my coffee black");
    assert_eq!(stored[0].kind, MemoryKind::Preference);
}
