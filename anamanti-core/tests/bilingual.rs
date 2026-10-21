//! Bilingual (English + Brazilian Portuguese) turns through the real [`Pipeline`]
//! (`plans/BilingualPlan.md`): mock STT reports a language like the live ElevenLabs /
//! faster-whisper engines do, a capturing LLM records the exact prompt, and mock Piper
//! records which **voice** each sentence was synthesized with.
//!
//! The §3 acceptance gates these pin: English is unchanged in single-language mode
//! *and* in bilingual mode; a Portuguese turn is answered in Portuguese with the
//! Portuguese voice; the fast paths work in either language; a phrase that matched
//! decides the language; System-1 only answers Portuguese for validated intents.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::io::{split, BufReader, DuplexStream};

use anamanti_core::lang::{Lang, LanguageSettings};
use anamanti_core::llm::{LlmBackend, LlmTurn, ReplyStream};
use anamanti_core::memory::MemoryStore;
use anamanti_core::orchestrator::{Pipeline, ServiceConnector, TurnEvent, TurnOutcome, TurnStart};
use anamanti_core::server;
use anamanti_core::wyoming::protocol::{read_event, types, write_event, AudioFormat, WyomingEvent};
use anamanti_core::wyoming::DynConnection;

const EN_VOICE: &str = "en_US-amy-medium";
const PT_VOICE: &str = "pt_BR-faber-medium";

/// Each synthesized sentence and the Piper voice it was requested with.
type Synthesized = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// Mock STT/TTS: the STT returns `transcript` (with an optional `language`, as newer
/// wyoming-faster-whisper servers do); the TTS records `(text, voice)` per sentence.
struct Mocks {
    transcript: String,
    stt_lang: Option<String>,
    synthesized: Synthesized,
    /// The language each TTS connection was requested for (per-language Piper routing).
    tts_langs: Arc<Mutex<Vec<Option<Lang>>>>,
}

impl Mocks {
    fn new(transcript: &str, stt_lang: Option<&str>) -> Self {
        Self {
            transcript: transcript.to_string(),
            stt_lang: stt_lang.map(str::to_string),
            synthesized: Arc::new(Mutex::new(Vec::new())),
            tts_langs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn voices(&self) -> Vec<Option<String>> {
        self.synthesized
            .lock()
            .unwrap()
            .iter()
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn texts(&self) -> Vec<String> {
        self.synthesized
            .lock()
            .unwrap()
            .iter()
            .map(|(t, _)| t.clone())
            .collect()
    }
}

#[async_trait]
impl ServiceConnector for Mocks {
    async fn connect_stt(&self) -> Result<DynConnection> {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let mut ev = WyomingEvent::transcript(&self.transcript);
        if let Some(l) = &self.stt_lang {
            ev.data["language"] = serde_json::json!(l);
        }
        tokio::spawn(async move {
            let (r, w) = split(server);
            let mut reader = BufReader::new(r);
            let mut writer = w;
            while let Ok(Some(e)) = read_event(&mut reader).await {
                if e.event_type == types::AUDIO_CHUNK {
                    let _ = write_event(&mut writer, &ev).await;
                    break;
                }
            }
            while let Ok(Some(_)) = read_event(&mut reader).await {}
        });
        let (r, w) = split(client);
        Ok(DynConnection::from_io(r, w))
    }

    async fn connect_tts_for(&self, lang: Option<Lang>) -> Result<DynConnection> {
        self.tts_langs.lock().unwrap().push(lang);
        self.connect_tts().await
    }

    async fn connect_tts(&self) -> Result<DynConnection> {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let captured = self.synthesized.clone();
        tokio::spawn(async move {
            let (r, w) = split(server);
            let mut reader = BufReader::new(r);
            let mut writer = w;
            if let Ok(Some(ev)) = read_event(&mut reader).await {
                if ev.event_type == types::SYNTHESIZE {
                    let text = ev.data["text"].as_str().unwrap_or_default().to_string();
                    let voice = ev.data["voice"]["name"].as_str().map(str::to_string);
                    captured.lock().unwrap().push((text, voice));
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
        });
        let (r, w) = split(client);
        Ok(DynConnection::from_io(r, w))
    }
}

/// Records the turn handed to the LLM and replies with a fixed text.
struct CapturingLlm {
    seen: Arc<Mutex<Option<LlmTurn>>>,
    reply: &'static str,
}

#[async_trait]
impl LlmBackend for CapturingLlm {
    fn name(&self) -> &str {
        "capturing"
    }
    async fn respond(&self, turn: LlmTurn) -> Result<ReplyStream> {
        *self.seen.lock().unwrap() = Some(turn);
        let reply = self.reply.to_string();
        Ok(Box::pin(futures_util::stream::once(
            async move { Ok(reply) },
        )))
    }
}

fn voiced_chunk_bytes(hz: f32, ms: usize) -> Vec<u8> {
    let n = 16_000usize * ms / 1000;
    let mut out = Vec::with_capacity(n * 2);
    for t in 0..n {
        let x = (std::f32::consts::TAU * hz * t as f32 / 16_000.0).sin();
        out.extend_from_slice(&((x * 8000.0) as i16).to_le_bytes());
    }
    out
}

/// Device side: stream voiced audio, read the transcript, then the reply tokens until
/// the closing `audio-stop`. Returns `(transcript, reply)`.
async fn drive_device(io: DuplexStream) -> (String, String) {
    let (r, w) = split(io);
    let mut reader = BufReader::new(r);
    let mut writer = w;
    let fmt = AudioFormat::PCM_16K_MONO;
    write_event(&mut writer, &WyomingEvent::audio_start(fmt, 0))
        .await
        .unwrap();
    write_event(
        &mut writer,
        &WyomingEvent::audio_chunk(fmt, 0, voiced_chunk_bytes(180.0, 1500)),
    )
    .await
    .unwrap();
    let ev = read_event(&mut reader).await.unwrap().unwrap();
    let transcript = ev.transcript_text().unwrap_or_default().to_string();
    write_event(&mut writer, &WyomingEvent::audio_stop(40))
        .await
        .unwrap();
    let mut reply = String::new();
    while let Some(ev) = read_event(&mut reader).await.unwrap() {
        if let Some(tok) = ev.reply_token_text() {
            reply.push_str(tok);
            continue;
        }
        if ev.event_type == types::AUDIO_STOP {
            break;
        }
    }
    (transcript, reply)
}

async fn run(pipeline: &Pipeline, mocks: &Mocks) -> (String, Vec<TurnEvent>) {
    let (dev_pipeline, dev_test) = tokio::io::duplex(64 * 1024);
    let (pr, pw) = split(dev_pipeline);
    let mut device = DynConnection::from_io(pr, pw);
    let task = tokio::spawn(drive_device(dev_test));
    let mut events = Vec::new();
    {
        let mut on_event = |e: TurnEvent| events.push(e);
        loop {
            let ev = device.read().await.unwrap().unwrap();
            if let Some(start) = TurnStart::from_event(&ev) {
                let out =
                    server::drive_turn(pipeline, &mut device, mocks, start, None, &mut on_event)
                        .await
                        .expect("turn runs");
                assert_eq!(out, TurnOutcome::Completed);
                break;
            }
        }
    }
    let (_transcript, reply) = task.await.unwrap();
    (reply, events)
}

fn bilingual() -> LanguageSettings {
    LanguageSettings {
        enabled: vec![Lang::En, Lang::PtBr],
        default: Lang::En,
        voices: [(Lang::PtBr, PT_VOICE.to_string())].into(),
    }
}

/// A pipeline with a capturing LLM, the English global voice, and `language` applied.
fn pipeline(
    language: Option<LanguageSettings>,
    reply: &'static str,
) -> (Pipeline, Arc<Mutex<Option<LlmTurn>>>, Arc<MemoryStore>) {
    let seen = Arc::new(Mutex::new(None));
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let p = Pipeline::new(
        Arc::new(CapturingLlm {
            seen: seen.clone(),
            reply,
        }),
        memory.clone(),
        "test persona",
        Some(EN_VOICE.to_string()),
        Duration::from_secs(5),
    );
    if let Some(l) = language {
        p.settings().apply_language(&l);
    }
    (p, seen, memory)
}

fn system_prompt(seen: &Arc<Mutex<Option<LlmTurn>>>) -> String {
    seen.lock()
        .unwrap()
        .as_ref()
        .expect("the LLM was called")
        .system_prompt
        .clone()
}

// ---------------------------------------------------------------- English stays English

#[tokio::test]
async fn single_language_mode_is_unchanged() {
    // No language settings at all (the default) — even a Portuguese transcript with a
    // Portuguese STT report takes exactly the pre-bilingual English path.
    let (p, seen, _) = pipeline(None, "Got it.");
    let mocks = Mocks::new("Lembra que a minha cor favorita é azul.", Some("pt"));
    let (reply, _) = run(&p, &mocks).await;
    assert_eq!(reply, "Got it.", "the pt command isn't recognized → LLM");
    assert!(!system_prompt(&seen).contains("# Language"));
    assert_eq!(mocks.voices(), vec![Some(EN_VOICE.to_string())]);
    // Single-language mode never asks for a per-language Piper.
    assert_eq!(*mocks.tts_langs.lock().unwrap(), vec![None]);
}

#[tokio::test]
async fn english_command_in_bilingual_mode_is_byte_identical() {
    let (p, _, memory) = pipeline(Some(bilingual()), "unused");
    let mocks = Mocks::new("remember I like my coffee black", Some("en"));
    let (reply, events) = run(&p, &mocks).await;
    assert_eq!(reply, "Okay, I'll remember that.");
    assert!(events.contains(&TurnEvent::Reply("Okay, I'll remember that.".to_string())));
    assert_eq!(mocks.voices(), vec![Some(EN_VOICE.to_string())]);
    assert_eq!(memory.list().unwrap()[0].content, "I like my coffee black");
}

#[tokio::test]
async fn english_llm_turn_in_bilingual_mode_names_english_and_keeps_the_voice() {
    let (p, seen, _) = pipeline(Some(bilingual()), "Here is a joke about cats.");
    let mocks = Mocks::new("tell me a short joke about cats", Some("en"));
    let (reply, _) = run(&p, &mocks).await;
    assert_eq!(reply, "Here is a joke about cats.");
    assert!(system_prompt(&seen).contains("The person spoke English. Reply in English"));
    assert_eq!(mocks.voices(), vec![Some(EN_VOICE.to_string())]);
}

// ---------------------------------------------------------------- Portuguese

#[tokio::test]
async fn portuguese_llm_turn_names_portuguese_and_uses_the_pt_voice() {
    let (p, seen, _) = pipeline(
        Some(bilingual()),
        "Claro! Por que o gato sentou no computador?",
    );
    let mocks = Mocks::new("Me conta uma piada curta sobre gatos.", Some("pt"));
    let (reply, _) = run(&p, &mocks).await;
    assert!(reply.starts_with("Claro!"));
    assert!(system_prompt(&seen).contains("Reply in Brazilian Portuguese (pt-BR)"));
    assert!(mocks
        .voices()
        .iter()
        .all(|v| v.as_deref() == Some(PT_VOICE)));
    // Routed to the Portuguese Piper (a `tts_addrs` entry, when configured).
    assert!(mocks
        .tts_langs
        .lock()
        .unwrap()
        .iter()
        .all(|l| *l == Some(Lang::PtBr)));
}

#[tokio::test]
async fn portuguese_memory_command_replies_in_portuguese_and_stores_as_spoken() {
    let (p, seen, memory) = pipeline(Some(bilingual()), "unused");
    let mocks = Mocks::new("Lembra que a minha cor favorita é azul.", Some("pt"));
    let (reply, _) = run(&p, &mocks).await;
    assert_eq!(reply, "Tudo bem, vou lembrar disso.");
    assert!(seen.lock().unwrap().is_none(), "the LLM was skipped");
    assert_eq!(mocks.voices(), vec![Some(PT_VOICE.to_string())]);
    let stored = memory.list().unwrap();
    assert_eq!(stored[0].content, "a minha cor favorita é azul");
}

#[tokio::test]
async fn a_matching_phrase_decides_the_language_over_the_stt_report() {
    // The engine guessed English for a two-word utterance; the Portuguese command
    // table matched, so the reply (and voice) follow what was actually said.
    let (p, _, _) = pipeline(Some(bilingual()), "unused");
    let mocks = Mocks::new("Esquece isso.", Some("en"));
    let (reply, _) = run(&p, &mocks).await;
    assert_eq!(reply, "Não havia nada para esquecer.");
    assert_eq!(mocks.voices(), vec![Some(PT_VOICE.to_string())]);
}

#[tokio::test]
async fn text_id_decides_when_the_engine_reports_no_language() {
    let (p, seen, _) = pipeline(Some(bilingual()), "Claro, aqui vai uma piada.");
    let mocks = Mocks::new("me conta uma piada curta sobre gatos", None);
    run(&p, &mocks).await;
    assert!(system_prompt(&seen).contains("Brazilian Portuguese"));
    assert_eq!(mocks.voices(), vec![Some(PT_VOICE.to_string())]);
}

#[tokio::test]
async fn the_voice_follows_a_reply_that_switched_language() {
    // A pt turn ("responde em inglês") whose reply comes back in English: the
    // first-sentence check moves the voice to English.
    let (p, _, _) = pipeline(
        Some(bilingual()),
        "Sure, here is the answer in English. It is raining today.",
    );
    let mocks = Mocks::new("Responde em inglês: vai chover hoje?", Some("pt"));
    run(&p, &mocks).await;
    assert!(
        mocks
            .voices()
            .iter()
            .all(|v| v.as_deref() == Some(EN_VOICE)),
        "voices: {:?}",
        mocks.voices()
    );
    // The switched voice is routed to the English Piper.
    assert!(mocks
        .tts_langs
        .lock()
        .unwrap()
        .iter()
        .all(|l| *l == Some(Lang::En)));
}

// ---------------------------------------------------------------- System-1 gating

/// System-1 answers a Portuguese turn only for intents validated in Portuguese
/// (`system1::PT_BR_READY`); otherwise it defers to System-2, which answers in
/// Portuguese. English is unaffected either way.
#[tokio::test]
async fn system1_answers_portuguese_only_for_validated_intents() {
    use anamanti_core::system1::{mock::MockDecider, ready_for, Intent};

    let (p, seen, _) = pipeline(Some(bilingual()), "São três da tarde.");
    let p = p.with_system1(Arc::new(MockDecider::resolve("time", 0.99)));
    let mocks = Mocks::new("Que horas são?", Some("pt"));
    let (reply, _) = run(&p, &mocks).await;
    if ready_for(Lang::PtBr, Intent::Time) {
        assert!(
            reply.starts_with("São ") || reply.starts_with("É "),
            "{reply:?}"
        );
        assert!(seen.lock().unwrap().is_none(), "System-1 answered");
    } else {
        assert_eq!(reply, "São três da tarde.", "deferred to System-2");
    }
    assert_eq!(mocks.voices(), vec![Some(PT_VOICE.to_string())]);

    // English on the same pipeline: System-1 answers, in English.
    let mocks = Mocks::new("what time is it", Some("en"));
    let (reply, _) = run(&p, &mocks).await;
    assert!(reply.starts_with("It's "), "{reply:?}");
    assert_eq!(mocks.voices(), vec![Some(EN_VOICE.to_string())]);
}

#[tokio::test]
async fn system1_timer_in_portuguese_parses_portuguese_numbers() {
    use anamanti_core::system1::{mock::MockDecider, ready_for, Intent};
    if !ready_for(Lang::PtBr, Intent::Timer) {
        return; // covered by the gating test until the timer intent is validated
    }
    let (p, seen, _) = pipeline(Some(bilingual()), "unused");
    let p = p.with_system1(Arc::new(MockDecider::resolve("timer", 0.99)));
    let mocks = Mocks::new("Coloca um timer de dez minutos.", Some("pt"));
    let (reply, _) = run(&p, &mocks).await;
    assert_eq!(reply, "Timer de 10 minutos iniciado.");
    assert!(seen.lock().unwrap().is_none());
    assert_eq!(
        mocks.texts(),
        vec!["Timer de 10 minutos iniciado.".to_string()]
    );
}
