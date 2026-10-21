//! Stage 2 end-to-end check for the in-process Whisper STT engine
//! (`plans/python-to-rust-whisper.md`). Compiled only with the
//! `stt-whisper-local` feature.
//!
//! The decode needs a real ggml model and a spoken WAV, which are large binary
//! assets we deliberately do NOT vendor. The test therefore reads their paths from
//! the environment and **skips** (passes as a no-op) when they are absent, so
//! `cargo test --features stt-whisper-local` stays green on a machine without the
//! assets. To actually exercise the decode:
//!
//! ```bash
//! export ANAMANTI_TEST_WHISPER_MODEL=/path/to/ggml-tiny.en.bin   # or base/small
//! export ANAMANTI_TEST_WHISPER_WAV=/path/to/jfk.wav              # 16 kHz mono
//! # optional: assert the transcript contains this (case-insensitive) substring
//! export ANAMANTI_TEST_WHISPER_EXPECT="fellow americans"
//! cargo test --features stt-whisper-local --test whisper_local -- --nocapture
//! ```

#![cfg(feature = "stt-whisper-local")]

use anamanti_core::stt::{SttEvent, Transcriber, WhisperEngine};

/// Read a 16 kHz mono 16-bit PCM WAV into little-endian byte chunks, exactly as the
/// device streams PCM to the pipeline.
fn wav_to_le_bytes(path: &str) -> Vec<u8> {
    let mut reader = hound::WavReader::open(path).expect("open WAV fixture");
    let spec = reader.spec();
    assert_eq!(spec.channels, 1, "fixture must be mono");
    assert_eq!(spec.sample_rate, 16_000, "fixture must be 16 kHz");
    let mut bytes = Vec::new();
    for s in reader.samples::<i16>() {
        bytes.extend_from_slice(&s.expect("read sample").to_le_bytes());
    }
    bytes
}

#[tokio::test]
async fn decodes_wav_through_the_transcriber_seam() {
    let (Ok(model), Ok(wav)) = (
        std::env::var("ANAMANTI_TEST_WHISPER_MODEL"),
        std::env::var("ANAMANTI_TEST_WHISPER_WAV"),
    ) else {
        eprintln!(
            "skipping: set ANAMANTI_TEST_WHISPER_MODEL and ANAMANTI_TEST_WHISPER_WAV \
             to run the in-process Whisper decode test"
        );
        return;
    };

    let engine = WhisperEngine::open(&model, Some("en".to_string()), 0).expect("load model");
    let mut stt = engine.begin();

    // Drive the `Transcriber` the way `stream_to_transcript` does: stream the PCM in
    // ~20 ms chunks (640 bytes = 320 samples @ 16 kHz), then finalize and read.
    let pcm = wav_to_le_bytes(&wav);
    for chunk in pcm.chunks(640) {
        stt.forward_pcm(chunk.to_vec()).await.expect("forward_pcm");
    }
    stt.finish().await.expect("finish");

    let text = match stt.read_event().await.expect("read_event") {
        Some(SttEvent::Transcript(t)) => t.text,
        other => panic!("expected a transcript, got {other:?}"),
    };
    eprintln!("whisper transcript: {text:?}");

    assert!(!text.trim().is_empty(), "transcript should not be empty");
    if let Ok(expect) = std::env::var("ANAMANTI_TEST_WHISPER_EXPECT") {
        assert!(
            text.to_lowercase().contains(&expect.to_lowercase()),
            "transcript {text:?} should contain {expect:?}"
        );
    }
}

/// Bilingual detect-then-pin (`plans/BilingualPlan.md` §2.3) with a **multilingual**
/// model: every clip under `<dir>/en/*.wav` must come back tagged `en`, every clip
/// under `<dir>/pt/*.wav` tagged `pt`. Skips without the assets:
///
/// ```bash
/// export ANAMANTI_TEST_WHISPER_MULTI_MODEL=/path/to/ggml-small.bin
/// export ANAMANTI_TEST_BILINGUAL_CLIPS=/path/to/clips   # en/*.wav + pt/*.wav, 16 kHz mono
/// cargo test --features stt-whisper-metal --test whisper_local -- --nocapture
/// ```
#[tokio::test]
async fn bilingual_decode_detects_among_the_enabled_languages() {
    use anamanti_core::lang::{Lang, LanguageSettings};
    use anamanti_core::stt::{SttEngine, WhisperSttEngine};
    use anamanti_core::wyoming::protocol::AudioFormat;

    let (Ok(model), Ok(dir)) = (
        std::env::var("ANAMANTI_TEST_WHISPER_MULTI_MODEL"),
        std::env::var("ANAMANTI_TEST_BILINGUAL_CLIPS"),
    ) else {
        eprintln!(
            "skipping: set ANAMANTI_TEST_WHISPER_MULTI_MODEL and ANAMANTI_TEST_BILINGUAL_CLIPS"
        );
        return;
    };
    let engine = WhisperEngine::open(&model, Some("en".to_string()), 0).expect("load model");
    assert!(
        engine.is_multilingual(),
        "needs a multilingual (non-.en) model"
    );
    let stt = WhisperSttEngine::new(engine);
    let langs = LanguageSettings {
        enabled: vec![Lang::En, Lang::PtBr],
        default: Lang::En,
        voices: Default::default(),
    };
    let mut failures = Vec::new();
    for (sub, want) in [("en", "en"), ("pt", "pt")] {
        let mut wavs: Vec<_> = std::fs::read_dir(format!("{dir}/{sub}"))
            .expect("clip dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "wav"))
            .collect();
        wavs.sort();
        for wav in wavs {
            let mut t = stt
                .begin_with(AudioFormat::PCM_16K_MONO, &langs)
                .await
                .unwrap();
            // Like a live turn: the pump forwards the VAD's trailing silence too, so a
            // short command ("para") still clears whisper.cpp's 1 s minimum input.
            let mut pcm = wav_to_le_bytes(wav.to_str().unwrap());
            pcm.extend(std::iter::repeat_n(0u8, 16_000 * 2 * 6 / 10));
            for chunk in pcm.chunks(640) {
                t.forward_pcm(chunk.to_vec()).await.unwrap();
            }
            let t0 = std::time::Instant::now();
            t.finish().await.unwrap();
            let Some(SttEvent::Transcript(tr)) = t.read_event().await.unwrap() else {
                panic!("no transcript for {}", wav.display());
            };
            let bilingual_ms = t0.elapsed().as_millis();
            // The same clip decoded the pre-bilingual way (pinned, no detection), for the
            // latency comparison.
            let mut single = stt
                .begin_with(AudioFormat::PCM_16K_MONO, &LanguageSettings::default())
                .await
                .unwrap();
            single.forward_pcm(pcm.clone()).await.unwrap();
            let t1 = std::time::Instant::now();
            single.finish().await.unwrap();
            let _ = single.read_event().await.unwrap();
            let single_ms = t1.elapsed().as_millis();
            eprintln!(
                "[{sub}] {} → {:?} lang={:?} ({bilingual_ms} ms bilingual vs {single_ms} ms single)",
                wav.display(),
                tr.text,
                tr.lang
            );
            // One word carries too little audio for language ID (a lone "para" comes back
            // labelled English); the pipeline resolves those with its short-phrase table
            // (`lang::detect::short_phrase`), so only multi-word clips are asserted here.
            let multi_word = tr.text.split_whitespace().count() >= 2;
            if multi_word && tr.lang.as_deref() != Some(want) {
                failures.push(format!("{}: got {:?}", wav.display(), tr.lang));
            }
            if !multi_word {
                let resolved = anamanti_core::lang::detect::short_phrase(&tr.text, &langs.enabled)
                    .map(|l| l.iso639_1());
                eprintln!("      one word: short-phrase table resolves it to {resolved:?}");
                assert_eq!(resolved, Some(want), "{}", wav.display());
            }
        }
    }
    assert!(failures.is_empty(), "wrong language: {failures:#?}");
}
