//! Phase 0 host-side AEC spike (`cargo run --features aec --example aec_spike`).
//!
//! Runs the [`ambient_orchestrator::aec::AecProcessor`] over recorded device
//! audio so we can measure echo/noise reduction on real hardware captures before
//! wiring the APM into the live turn path (Phase 2).
//!
//! Inputs are **raw little-endian `i16`, 16 kHz, mono** PCM dumps (the format the
//! device already streams as Wyoming `audio-chunk`s):
//!   - `mic.pcm` — the near-end: what the Echo Show microphone heard (with echo).
//!   - `ref.pcm` — the far-end render reference: what was played out the speaker,
//!     tapped at the DAC callback (Phase 1). Must be time-aligned to `mic.pcm`.
//!
//! Usage:
//!   cargo run --features aec --example aec_spike -- mic.pcm ref.pcm clean.pcm
//!
//! Then transcribe `clean.pcm` vs `mic.pcm` through the Whisper path
//! (`src/wyoming/stt.rs`) to confirm the transcript actually improves.

use std::fs;
use std::path::Path;

use ambient_orchestrator::aec::AecProcessor;
use anyhow::{Context, Result};

fn read_pcm_i16(path: &Path) -> Result<Vec<i16>> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect())
}

fn write_pcm_i16(path: &Path, pcm: &[i16]) -> Result<()> {
    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for &s in pcm {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn energy(pcm: &[i16]) -> f64 {
    pcm.iter().map(|&s| (s as f64) * (s as f64)).sum()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!(
            "usage: {} <mic.pcm> <ref.pcm> <clean.pcm>  (raw i16 LE, 16 kHz mono)",
            args[0]
        );
        std::process::exit(2);
    }
    let mic = read_pcm_i16(Path::new(&args[1]))?;
    let reference = read_pcm_i16(Path::new(&args[2]))?;

    // AEC_AGC=1 additionally enables the digital AGC (level normalization) so the
    // same harness can A/B the mic-gain fix on real far-field clips.
    let agc = std::env::var("AEC_AGC").ok().as_deref() == Some("1");
    let mut apm = if agc {
        eprintln!("(AGC enabled)");
        AecProcessor::new_16k_mono_with_agc()?
    } else {
        AecProcessor::new_16k_mono()?
    };
    let n = apm.frame_len();

    let frames = mic.len() / n;
    let mut cleaned = Vec::with_capacity(frames * n);
    for f in 0..frames {
        let lo = f * n;
        // Far-end reference for this frame (silence once the reference runs out —
        // e.g. the reply finished but the mic keeps recording the user).
        let ref_frame: Vec<i16> = if lo + n <= reference.len() {
            reference[lo..lo + n].to_vec()
        } else {
            let mut v = vec![0i16; n];
            if lo < reference.len() {
                let avail = &reference[lo..];
                v[..avail.len()].copy_from_slice(avail);
            }
            v
        };
        apm.process_render(&ref_frame)?;

        let mut mic_frame = mic[lo..lo + n].to_vec();
        apm.process_capture(&mut mic_frame)?;
        cleaned.extend_from_slice(&mic_frame);
    }

    write_pcm_i16(Path::new(&args[3]), &cleaned)?;

    let e_in = energy(&mic[..frames * n]);
    let e_out = energy(&cleaned);
    let reduction_db = 10.0 * (e_in / e_out.max(1e-9)).log10();
    println!(
        "processed {frames} frames ({:.1}s @ {} Hz)",
        (frames * n) as f32 / 16_000.0,
        16_000
    );
    println!("mic energy in={e_in:.1}  cleaned out={e_out:.1}  reduction≈{reduction_db:.1} dB");
    println!("wrote cleaned mic -> {}", args[3]);
    Ok(())
}
