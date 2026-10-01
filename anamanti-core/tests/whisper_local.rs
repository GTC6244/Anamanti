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
        Some(SttEvent::Transcript(t)) => t,
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
