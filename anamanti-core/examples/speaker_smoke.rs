//! Smoke test for a real speaker-embedding ONNX model (speaker_id_plan.md Phase E
//! / SpeakerEnrollmentPlan.md Phase 1). Proves whether `tract-onnx` can load + run
//! the chosen ECAPA-TDNN export, and prints the output embedding dimensionality so
//! `speaker.embed_dims` can be confirmed.
//!
//!   cargo run --example speaker_smoke --manifest-path anamanti-core/Cargo.toml -- <model.onnx>
//!
//! Exits non-zero (with the tract error) if the model cannot be loaded — that is
//! the signal to fall back to the `ort`/onnxruntime backend (the VAD/embeddings
//! pin) instead of pure-Rust tract.

use anamanti_core::speaker::embed::{OnnxSpeakerEmbedder, SpeakerEmbedder};
use anamanti_core::speaker::features::FbankConfig;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: speaker_smoke <model.onnx>");
    eprintln!("loading {path} via tract-onnx …");
    // dims is only a label on the embedder; the true output length is measured below.
    let embedder = OnnxSpeakerEmbedder::open(&path, 192, FbankConfig::default())?;
    eprintln!("✓ tract loaded + optimized the model");

    // ~2 s of a synthetic "voice": a fundamental + harmonics, with vibrato and an
    // amplitude envelope so frames vary over time (a steady tone collapses under
    // the per-utterance mean normalization the fbank now applies).
    let emb = embedder.embed(&voice(120.0))?;
    let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
    println!("output embedding dims = {}", emb.len());
    println!("L2 norm = {norm:.4} (should be ~1.0)");
    println!("first 8 = {:?}", &emb[..emb.len().min(8)]);

    // A different "voice" (higher pitch) should produce a measurably different vector.
    let emb2 = embedder.embed(&voice(240.0))?;
    let cos: f32 = emb.iter().zip(&emb2).map(|(a, b)| a * b).sum();
    println!("cosine(120Hz voice, 240Hz voice) = {cos:.4}");
    Ok(())
}

/// A crude but time-varying synthetic voice at fundamental `f0` (Hz): three
/// harmonics, 5 Hz vibrato, and a slow tremolo envelope.
fn voice(f0: f32) -> Vec<i16> {
    let sr = 16_000.0f32;
    (0..32_000)
        .map(|t| {
            let s = t as f32 / sr;
            let vib = 1.0 + 0.01 * (std::f32::consts::TAU * 5.0 * s).sin();
            let env = 0.6 + 0.4 * (std::f32::consts::TAU * 2.0 * s).sin();
            let sample = (std::f32::consts::TAU * f0 * vib * s).sin()
                + 0.5 * (std::f32::consts::TAU * 2.0 * f0 * vib * s).sin()
                + 0.25 * (std::f32::consts::TAU * 3.0 * f0 * vib * s).sin();
            (sample * env * 4000.0) as i16
        })
        .collect()
}
