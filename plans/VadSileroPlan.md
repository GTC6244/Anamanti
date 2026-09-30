# Silero VAD for the Anamanti Core

Design + phasing for adding a **neural (Silero) voice-activity detector** to the
Anamanti Core's end-of-speech pipeline, alongside the existing energy/RMS VAD.

Read with [`architecture.md`](./architecture.md) §4 (the turn pipeline) and
[`Plan.MD`](./Plan.MD) (decision table row *"End-of-speech VAD — pluggable engine"*).

> **STATUS (2026-09-30): Silero is now the committed default.** The `vad-silero`
> build feature is **on by default** and `VadConfig::default().engine == Silero`, so a
> plain `cargo build` + boot runs Silero and **requires the v4 model on disk** (a
> `silero` boot with no model is a hard error). Energy/RMS is the opt-in fallback
> (`vad.engine="energy"` or `--no-default-features`). This was an **owner-directed flip
> ahead of the M3 far-field validation gate** described below (§2, §5/M3) — the driver
> was a live background-noise failure of the energy gate (end-of-speech never firing).
> M3's measurement work remains outstanding and should still be completed to confirm
> the choice; if it regresses, revert by dropping `default = ["vad-silero"]` and
> restoring `engine: Energy` in `VadConfig::default()`.

---

## 1. Why

The Core today decides end-of-speech with a hand-rolled **energy/RMS gate**
(`orchestrator.rs::stream_to_transcript`, `rms_i16_le(&pcm) > voice_rms_threshold`).
It works but has a structural weakness the code comments already admit
(`settings.rs:70-79`): an amplitude threshold cannot tell far-field speech from a
room noise floor. The consequences are real and recurring:

- **Per-device babysitting.** `voice_rms_threshold` (default 180) must be tuned
  per unit — too low and an Echo's ~100–200 idle noise floor reads as perpetual
  speech (end-of-speech never fires, the turn stalls); too high and quiet
  far-field speech is dropped as silence (turn returns nothing). See the field
  war-story in `TODO.md` (device AEC dropped mic level ~15 dB → VAD missed
  end-of-speech).
- **No robustness to non-speech energy.** TV/music/background chatter above the
  threshold reads as speech; the 250 ms onset debounce (`MIN_SPEECH_ONSET`) only
  partially mitigates it.

A Silero VAD returns a calibrated **speech probability** per frame that is far
more robust to noise, which (a) improves accuracy — fewer stalls, fewer dropped
quiet utterances, fewer TV-noise false-accepts — and (b) lets us **safely shorten
the `end_silence` hangover** (the dominant end-of-turn latency term), because a
confident "not speech" is trustworthy in a way an energy dip is not.

**Non-goal / honest scoping.** Silero is *latency-neutral by itself* — the per-frame
inference is ~sub-ms to ~1 ms on the M4 (negligible vs the 32 ms frame and dwarfed
by the post-`audio-stop` Whisper decode), and the model loads once at boot. The
latency *win* comes only from retuning the hangover on top of the better
confidence signal (Phase M2), not from the detector swap itself. This plan treats
**accuracy** as the primary payoff and **latency** as a follow-on from retuning.

## 2. Locked-decision impact

This **generalizes** the locked decision *"End-of-speech (VAD) — Anamanti
Core-side energy VAD"* to *"Anamanti Core-side, pluggable VAD engine; energy is
the committed default; Silero is opt-in."* It does **not** touch the two things
that decision actually locks: VAD stays **off-device** (the device never runs its
own VAD) and stays **on the Core**. Silero runs on the Mac. No new device
protocol, no second audio path.

Merge-safety originally mirrored the `stt.engine` / `system1.backend` precedents
(committed default **energy**, byte-for-byte unchanged until opt-in). **Superseded
2026-09-30:** the committed default was flipped to **Silero** (feature on by default;
`VadConfig::default().engine == Silero`) — an owner-directed decision ahead of the M3
gate, driven by a live background-noise failure of the energy gate. Consequences: a
default build now pulls onnxruntime and a default boot **requires the v4 model on disk**
(hard error otherwise). The M3 far-field validation below is **still worth completing**
to confirm the flip; energy remains one config key away (`vad.engine="energy"`) or an
`--no-default-features` build.

## 3. Architecture — a `SpeechGate` seam

The end-of-speech **state machine stays exactly as it is** — onset debounce
(`MIN_SPEECH_ONSET`), `speech_started` latch, `end_silence` hangover,
`no_speech_finalize` fallback, the silence hallucination guard, the voiced-PCM
accumulation for speaker-ID. We only swap the **per-frame voiced/not-voiced
decision** behind a trait.

```rust
// anamanti-core/src/vad/mod.rs (new module)

/// Per-chunk speech decision for the turn's end-of-speech state machine. The
/// caller owns the onset debounce, hangover, and finalize logic; a gate only
/// answers "is this chunk speech?" and may carry model state across chunks.
pub trait SpeechGate: Send {
    /// Feed the next chunk of little-endian PCM16 mono audio at `sample_rate` Hz
    /// (any length; the gate buffers internally to its own frame size). Returns
    /// whether the most recent audio is speech. Taking raw `&[u8]` keeps the energy
    /// path zero-copy; the Silero gate decodes to f32 as it re-frames.
    fn push(&mut self, pcm: &[u8], sample_rate: u32) -> bool;
    fn prob(&self) -> f32;      // 0.0..=1.0 (energy gate: 1.0/0.0)
    fn reset(&mut self);        // called at turn start
}
```

- **`EnergyGate`** (`vad/energy.rs`) — wraps today's exact logic:
  `rms_i16_le(frame) > voice_rms_threshold`. Zero behavior change; this is the
  default. `prob()` returns 1.0/0.0.
- **`SileroGate`** (`vad/silero.rs`, feature `vad-silero`) — **onnxruntime via the
  `ort` crate** running the Silero **v4** model (`silero_vad.onnx`). Two findings drove
  this:
  1. **Not tract.** Pure-Rust `tract` (the crate's ONNX runtime for the speaker
     embedder) **cannot** load stock Silero — both v5 and v4 embed an ONNX `If`
     control-flow op tract's typed translation rejects (v5: sample-rate branch → a
     decoder `Squeeze` on a non-unit axis; v4: `If_25`/`If_69` with mismatched branch
     facts). So the neural gate runs on onnxruntime (`ort` fetches a prebuilt binary at
     build time). Core/Mac-side only; ~15 MB native lib, acceptable there. `ort` is
     pinned (`=2.0.0-rc.13`), gated behind `vad-silero`, so a default build pulls
     neither the dep nor the binary.
  2. **v4, not v5 (on-device finding).** The **v5** export scores a **near-constant
     ~0 probability under `ort`** regardless of input — it never fired even on clean,
     loud TTS (max ~0.003 at natural gain), so every utterance was discarded as
     no-speech. The **v4** model scores clean speech ~0.999 and real far-field device
     speech correctly at natural gain. Verified with an offline harness against clean
     `say` TTS **and** a captured far-field mic dump, then live on-device. So the gate
     uses v4; `fetch-vad-model.sh` pins the v4 model.
  - **Framing.** Silero v4 expects fixed **1536-sample (96 ms @ 16 kHz)** windows. A
    small carry buffer re-frames the device's variable-size chunks; leftover samples
    carry to the next `push`; a chunk that completes no frame returns the sticky last
    decision. Contract: inputs `input`[1,1536] f32, `sr` scalar i64, `h`/`c`[2,1,64]
    f32; outputs `output`[1,1] (prob), `hn`/`cn`[2,1,64].
  - **Recurrent state.** Silero v4 is functional — the LSTM state is **explicit tensor
    I/O** (`h`/`c` in → `hn`/`cn` out), so the `ort::Session` is stateless between runs.
    Design: **one shared `SileroModel` (`Arc<Mutex<Session>>`) loaded once at boot**,
    cloned into each per-turn `SileroGate` that carries its **own** `h`/`c` + framing
    buffer, zeroed on `reset()`. Correct for the one-Pipeline-serves-many-devices case
    and avoids reloading the model per turn.
  - **Decision.** `prob >= silero.threshold` (default 0.5). No amplitude term.

Wiring: `stream_to_transcript` takes a `&mut dyn SpeechGate` instead of the
`voice_rms_threshold: f64` param; `run_turn` builds the gate from the per-turn
settings snapshot (`build_speech_gate()`, mirroring `build_llm`/`build_system1`)
and calls `reset()` at turn start. The line

```rust
let voiced = rms_i16_le(&pcm) > voice_rms_threshold;   // today
let voiced = gate.push(&pcm, mic_rate);                 // after
```

is the only change to the hot loop. Everything downstream is untouched.

## 4. Configuration (`vad` block)

New optional block, `deny_unknown_fields`, defaults preserving today's behavior —
following the `SttConfig` pattern (`config.rs`). Energy's existing knobs
(`voice_rms_threshold`, `end_silence_ms`) stay where they are (top-level settings)
so existing configs keep working untouched.

```jsonc
"vad": {
  "engine": "energy",              // "energy" (default) | "silero"
  "silero": {
    "model_path": "models/silero_vad.onnx",  // resolved rel. to cwd
    "threshold": 0.5               // speech-probability gate, 0.0..1.0
    // M2 will add: "min_speech_ms" (→ MIN_SPEECH_ONSET) and a Silero-tuned
    // "min_silence_ms" hangover. Not implemented in M1.
  }
}
```

**Implemented in M1:** `engine` + `silero.{model_path, threshold}` (see
`config.rs::VadConfig`). Energy's existing knobs (`voice_rms_threshold`,
`end_silence_ms`) are unchanged and still top-level.

- `VadEngineKind::from_label` (`"energy"` | `"silero"`), mirroring
  `SttEngineKind::from_label`.
- **Boot-time model check:** `engine=silero` with a missing/unloadable
  `model_path` is a **hard error at boot** (consistent with the config policy:
  fail loud, don't silently fall back to energy — that would mask a
  misconfiguration in production).
- **Model provisioning:** add `anamanti-core/scripts/fetch-vad-model.sh
  <dir>` (twin of `fetch-whisper-models.sh`) to download `silero_vad.onnx`
  (~1.8 MB, MIT-licensed) into the model dir. Not committed (binary asset), like
  the ggml Whisper models.

## 5. Phasing

Each phase is independently landable; the default stays energy throughout M0–M2.

- **M0 — Seam, no behavior change. ✅ DONE.** Extracted `vad/` module + `SpeechGate`
  trait; moved current logic into `EnergyGate`; threaded `&mut dyn SpeechGate` through
  `stream_to_transcript`/`run_turn`; `build_speech_gate()` defaults to energy.
  `EnergyGate` reproduces `rms_i16_le > threshold` exactly (parity test). Pure refactor,
  verified behavior-preserving (clippy clean; full lib + pipeline suites unchanged).

- **M1 — SileroGate. ✅ DONE + validated on-device.** `vad/silero.rs` (feature
  `vad-silero`): `ort`/onnxruntime load of the **v4** model, 1536-sample framing buffer,
  stateful inference (shared `Session` + per-gate `h`/`c`), `reset`. `vad` config block +
  `VadEngineKind` (+ config tests); boot-time model check with feature-off /
  missing-model **hard errors** (both verified); `ort` optional dep + feature;
  `scripts/fetch-vad-model.sh` (pins v4) + `/anamanti-core/models/` gitignored. Unit
  tests load the real model and assert probs in range + framing/reset. **Validated live
  on the Echo Show (2026-09-28):** "Hey Jarvis / what is the weather" → `speech_started=
  true`, transcript kept, weather spoken + shown. The v5→v4 switch was the key fix (see
  §3 and §6). **Not yet done:** accuracy fixtures over recorded speech/TV-noise +
  systematic far-field/latency numbers — M3.

- **M2 — Runtime tuning + latency win. ◐ PARTIAL.**
  - **✅ Threshold relay + live engine swap (done).** `silero_threshold` **and**
    `vad_engine` are now runtime settings threaded through `RuntimeSettings` /
    `SettingsView` / `SettingsUpdate` / `PersistedSettings` (seeded from `vad.*`,
    persisted-wins), relayed on both the device Wyoming path (`control.rs`) and the
    config-page JSON (`webconfig.rs`), with a **VAD section on the config page**
    (`config.html`: engine dropdown + threshold input) **and on the device (Echo Show)
    settings screen** — the fields flow over the Wyoming relay through the FRB engine
    (`anamanti-display/rust/src/api/settings.rs` + `wyoming/control.rs`, regenerated
    bridge) into the Dart settings screen (`settings_screen.dart`,
    `orchestrator_client.dart`): a VAD-engine dropdown + a Silero threshold slider shown
    only when Silero is selected, mirroring the existing
    `voice_rms_threshold`/`end_silence_ms` controls. `build_speech_gate` picks the engine
    from the live snapshot each turn; a swap to Silero with no model loaded falls back to
    energy + warns. Boot now loads the Silero model whenever the `vad-silero` build has one
    available (even if the boot engine is energy) so the swap needs no restart; threshold
    is clamped `[0,1]`, engine changes need no LLM rebuild. Verified via the config API
    (swap both ways + threshold clamp + persistence), Core unit tests
    (`apply_sets_and_clamps_silero_threshold`, `apply_swaps_the_vad_engine`), a device
    Rust relay round-trip test, and Flutter widget tests (engine swap reveals the slider +
    Save sends it; seeded Silero renders the threshold).
  - **☐ Latency win (still deferred).** The Silero-specific `min_silence_ms` hangover —
    A/B it **shorter** than the energy 700 ms (target 400–500 ms) to capture the
    end-of-turn latency win the better confidence unlocks; optional adaptive hangover
    that shortens once `prob` collapses. Needs the M3 measurements to tune safely.

- **M3 — On-device validation. ⚠️ Default already flipped (2026-09-30) ahead of this
  gate; validation still owed.** The committed default is now `silero` (feature on by
  default) — an owner-directed flip driven by a live background-noise failure of the
  energy gate, not by the measurements this phase was meant to produce. Still do the M4
  Mac Mini + Echo Show far-field QA to confirm the choice: measure, against energy@180 as
  baseline, false-accepts on TV/music, missed quiet utterances, end-of-turn latency at
  the retuned hangover, and per-frame inference time on the M4. Record the numbers as a
  dated decision-table update; if they regress vs energy, revert the default (drop
  `default = ["vad-silero"]` + restore `engine: Energy` in `VadConfig::default()`).

## 6. Risks & open questions

- **~~tract op coverage.~~ RESOLVED (M1 spike).** tract **cannot** load stock Silero
  v4 or v5 — both embed an ONNX `If` op tract's typed translation rejects. Decision:
  run Silero on **onnxruntime via `ort`** behind the `vad-silero` feature.
- **~~v5 model under ort.~~ RESOLVED → use v4 (on-device finding, 2026-09-28).** The v5
  export scores a **near-constant ~0** under `ort` regardless of input (max ~0.003 on
  clean loud TTS; erratic, sub-0.5 even with heavy gain) — it never fires on real
  speech, so every turn was discarded as no-speech. The **v4** model scores clean TTS
  ~0.999 and captured far-field device speech correctly at natural gain. Root cause of
  the v5 misbehavior under ort not pinned down (not worth it); **v4 is the shipped
  model.** Lesson: the spike only tested a sine (correctly ~0), which could not
  distinguish "correct" from "always ~0" — validate a neural model against **known
  positive** input, not just negatives.
- **Recurrent-state threading.** Silero v4's LSTM state is explicit tensor I/O; the gate
  threads `h`/`c`→`hn`/`cn` and zeroes on `reset()`. Covered by unit tests + the live
  on-device turn; a golden-vector regression test is a nice-to-have (M3).
- **`ort` is a prerelease** (`=2.0.0-rc.13`, pinned) and pulls a prebuilt onnxruntime
  binary at build time. Acceptable behind an opt-in, Core-side feature; revisit when
  `ort` 2.0 goes stable.
- **Model version / frame size.** Shipped model is **v4** = 1536-sample frames @ 16 kHz
  (`WINDOW`), `h`/`c` state; the fetch script pins v4 by SHA. Keep `WINDOW` + state
  shapes in `vad/silero.rs` matched to the pinned model.
- **Licensing.** Silero VAD is MIT — fine to ship; note provenance in the fetch
  script.
- **Memory.** ~2 MB model + session on the Mac — negligible (this is Core-side,
  not the RAM-constrained device).
- **Open:** should Silero's `prob` also feed a smarter follow-up no-speech window
  (`follow_up.*_wait_secs`)? Deferred to after M3 — keep the first cut a drop-in
  gate swap.
