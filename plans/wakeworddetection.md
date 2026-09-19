# Wake Word Detection: VACA vs. Our Phase 2

A focused comparison of how the **View Assist Companion App (VACA)** implements
on-device wake-word detection versus our current **Phase 2** Rust engine, and a
concrete list of improvements we should pull across.

VACA is a partial reference for us: its "brain" is Home Assistant, not our custom
Mac Mini orchestrator, and its device is any Android 8+ phone/tablet rather than
the 32-bit Echo Show specifically. But the *device-side* audio + wake-word code is
directly comparable and battle-tested on LineageOS-class hardware, so it is the
best cross-reference we have.

## Sources

- **VACA HA integration** (Wyoming glue, HA side):
  `github.com/msp1974/ViewAssist_Companion_App` → `custom_components/vaca/*`
- **VACA Android app** (the real device implementation, open source, branch `dev`):
  `github.com/msp1974/ViewAssistCompanionApp` → `app/src/main/java/com/msp1974/vacompanion/`
- **Standalone openWakeWord-for-Android reference** (Java/ONNX, fork of
  `hasanatlodhi/OpenwakewordforAndroid`):
  `github.com/msp1974/OpenwakewordforAndroid`
- **Our engine:** `display/rust/src/wakeword/detector.rs`, `display/rust/src/engine/mod.rs`,
  `display/rust/src/audio/{capture,resample,ring_buffer}.rs`, `display/rust/src/api/engine.rs`

> License note: VACA ships `LICENSE` + `NOTICE` (Apache-2.0 family). We can
> reference the design freely; verify the license before copying code verbatim.

---

## 1. How VACA does it

### 1.1 Pluggable engines

VACA supports **three** wake-word engines behind a `WakeWordEngineProvider`
abstraction (`wakeword/WakeWordEngine.kt`):

| `WakeWordEngineModel` | Algorithm | Classifier runtime |
| --- | --- | --- |
| `OPENWAKEWORD` | openWakeWord (dscripka) | **ONNX Runtime** (`OnnxModelRunner`) |
| `OPENWAKEWORD_RT` | openWakeWord | **TFLite/LiteRT** (`TfliteModelRunner`) |
| `MICROWAKEWORD` | microWakeWord | **TFLite**, streaming, with "stop word" models |

Assets ship both formats per wake word
(`assets/openwakeword/wakeWords/{alexa,hey_jarvis,ok_nabu,…}.{onnx,tflite}`,
`assets/microwakeword/wakeWords/*.tflite`) plus the shared front-end models
(`openwakeword/melspectrogram.tflite`, `openwakeword/embedding_model.tflite`).

### 1.2 openWakeWord pipeline (`wakeword/openwakeword/`)

Identical three-stage chain to upstream openWakeWord:

1. **Melspectrogram** (`ml/MelSpectrogram.kt`, TFLite): raw 16 kHz audio → 32-bin
   mel frames, then the affine transform `x / 10.0f + 2.0f`.
2. **Embedding** (`ml/EmbeddingModel.kt`, TFLite): sliding 76-frame window,
   8-frame hop → 96-d embedding.
3. **Classifier** (`ml/OnnxModelRunner.kt` or `ml/TfliteModelRunner.kt`, per wake
   word): last 16 embeddings → single confidence in [0, 1].

`audio/AudioProcessor.kt` owns the streaming buffering. Its constants:

```kotlin
N_PREPARED_SAMPLES   = 1280      // 80 ms @ 16 kHz per melspec step
SAMPLE_RATE          = 16000
MEL_SPECTROGRAM_MAX_LEN = 10 * 97
FEATURE_BUFFER_MAX_LEN  = 120
WINDOW_SIZE          = 76        // mel frames per embedding window
STEP_SIZE            = 8         // embedding hop
MEL_SPEC_FRAMES      = 32        // mel bins
// classifier reads getFeatures(16, -1) → last 16 embeddings
```

### 1.3 Detection smoothing (`openwakeword/OpenWakeWordEngine.kt`)

VACA does **not** trigger on a single frame over threshold. It keeps a 3-frame
sliding window of scores and averages:

```kotlin
private val slidingWindowSize = 3
private val probabilities = ArrayDeque<Float>(slidingWindowSize)

private fun isWakeWordDetected(probability: Float): Boolean {
    if (probabilities.size == slidingWindowSize) probabilities.removeFirst()
    probabilities.add(probability)
    return probabilities.size == slidingWindowSize
        && probabilities.average() > config.wakeWordThreshold
}
```

Plus a per-model `detectionCooldownMs = 1500L` guard against repeat fires, and it
can run multiple wake-word models concurrently (`modelProcessors` map).

### 1.4 Audio front-end (`audio/MicrophoneInput.kt`) — the valuable part

This is where the real device wisdom lives, and it directly addresses our
deferred-AEC decision:

- **Hardware effects with software fallback.** Attaches Android's
  `AcousticEchoCanceler`, `AutomaticGainControl`, and `NoiseSuppressor` to the
  `AudioRecord.audioSessionId`. If a platform effect is unavailable, a software
  `AudioEnhancer` covers AGC/NS. **AEC has no software fallback** — it is a pure
  hardware capability check (`aecSource`).
- **Self-trigger suppression that preserves barge-in.** Rather than muting or
  raising thresholds during playback, it blanks the mic to *digital silence* only
  for the device's own short chime/error sounds:

  ```kotlin
  fun suppressMicFor(durationMs: Long) { suppressUntilMs = now + durationMs }
  // readShort(): if (isMicSuppressed()) return ShortArray(frame.size)  // silence, same cadence
  ```

  It returns **silence frames rather than dropping them** (keeps the model's frame
  cadence) and deliberately does **not** suppress during TTS/alarm playback, so
  barge-in stays live. Blanking to silence also keeps the AGC envelope/noise-floor
  from being polluted by the leak-back.
- **Mic source + gain tuning.** `AudioInRouter` picks the preferred mic + audio
  source (raw vs. the `VOICE_RECOGNITION`-tuned source), applies `config.micGain`,
  and there is a per-device quirk list (`UnsupportedFunctionsDevice.isIssueDevice`)
  to skip hardware effects on known-bad OS builds.

### 1.5 Concurrency + event model

The engine is a Kotlin coroutine `Flow` emitting
`WakeDetected / StopDetected / Audio / AudioLevel / EngineStatus`. When
`isStreaming` is set it also emits raw PCM bytes onward to Home Assistant (the
equivalent of our capture → Wyoming handoff). Muting is reactive via
`MutableStateFlow` + `flatMapLatest`.

---

## 2. What our current code looks like

Our Phase 2 engine lives in Rust and is functionally the same openWakeWord chain,
built independently.

### 2.1 Detector (`display/rust/src/wakeword/detector.rs`)

`tract-onnx` runs all three models. Constants match VACA exactly:

```rust
const MELSPEC_CHUNK_SAMPLES: usize = 1280;   // == N_PREPARED_SAMPLES
const MEL_BINS: usize = 32;                  // == MEL_SPEC_FRAMES
const EMBED_WINDOW_FRAMES: usize = 76;       // == WINDOW_SIZE
const EMBED_STEP_FRAMES: usize = 8;          // == STEP_SIZE
const EMBED_DIM: usize = 96;
const WAKEWORD_WINDOW_EMBEDDINGS: usize = 16;
const MEL_SCALE: f32 = 10.0;  const MEL_BIAS: f32 = 2.0;  // mel/10 + 2
```

`push_audio()` accumulates 16 kHz `f32` samples, runs full 1280-sample chunks
through mel → embedding → classifier, and returns the **max** score seen in the
block. Rolling state (`audio_accum`, `mel`, `embeddings`) is carried across calls.

### 2.2 Engine loop (`display/rust/src/engine/mod.rs`)

One background thread owns the `!Send` `cpal` stream and runs
drain → resample → score. Trigger logic is a **single-frame threshold**:

```rust
Ok(Some(score)) if score >= config.threshold => {
    sink.add(WakeWordEvent::detected(config.model_name.clone(), score))
}
```

There is **no moving-average smoothing, no cooldown, and only one model at a
time.** If models are absent it degrades to capture-only RMS levels.

### 2.3 Capture + resample (`display/rust/src/audio/{capture,resample}.rs`)

- `capture.rs`: `cpal` (AAudio on device) opens the default input, downmixes
  interleaved frames to mono `i16` in the RT callback, `try_push` into the ring.
  **No AEC/AGC/NS, no mic-source selection, no gain, no self-trigger suppression.**
- `resample.rs`: linear interpolator, device-native rate → 16 kHz, off the RT path.

### 2.4 FRB surface (`display/rust/src/api/engine.rs`)

`WakeWordConfig { melspec/embedding/wakeword paths, model_name, threshold }` and a
flat `WakeWordEvent { kind, message, device, device_sample_rate, channels, rms,
score, model }` streamed to Dart. Lifecycle: `start_wake_word_engine`,
`stop_wake_word_engine`, `is_wake_word_engine_running`.

---

## 3. Side-by-side

Current state of **our engine** (as validated end-to-end on an Echo Show 8 —
wake word → STT → LLM → TTS → spoken reply) versus **VACA**. Legend: ✅ working /
present · ⚠️ partial or a different-but-working approach · ❌ not implemented.

| Dimension | VACA (Android/Kotlin) | Our engine (current) |
| --- | --- | --- |
| openWakeWord chain + constants | ✅ mel→embed→classifier, 1280/76/8/32/16 | ✅ identical (`wakeword/detector.rs`) |
| Inference runtime | TFLite (front-end) + ONNX/TFLite (classifier) | ✅ tract-onnx (all three, pure-Rust, offline) |
| End-to-end voice turn on device | ✅ (Home Assistant brain) | ✅ wake→STT→LLM→TTS→playback via Wyoming to the Mac orchestrator |
| Detection smoothing | ✅ 3-frame moving average | ✅ 3-frame moving average (`engine/gate.rs`, unit-tested) |
| Repeat-fire cooldown | ✅ 1500 ms | ✅ 1500 ms (same `DetectionGate`) |
| Streams raw audio onward | ✅ when `isStreaming` | ✅ streams 16 kHz PCM to STT during a turn |
| Speaker playback of the reply | ✅ | ✅ cpal AAudio output (needs `ndk_context`, now initialized via `JNI_OnLoad`) |
| Self-trigger handling | ✅ mic-blank window, barge-in preserved | ⚠️ raises the wake-word threshold while a turn is active (`active_threshold`); mic-blank not ported |
| Barge-in (wake word during playback) | ✅ | ✅ new detection interrupts + restarts the turn (`engine/net.rs`) |
| Live mic level for tuning | ✅ | ✅ RMS `Level` event emitted always (not just capture-only) |
| Capture sample-rate handling | ✅ (AudioRecord reports the real rate) | ✅ **runtime rate calibration** — cpal misreports 48 kHz on this HAL; we measure the true ~24 kHz and resample from that (§6) |
| Hardware AEC/AGC/NS | ✅ platform effects + SW fallback | ❌ none (not reachable through cpal 0.18; see §4.2) |
| Mic source / input gain | ✅ VOICE_RECOGNITION source + gain slider | ❌ default source; cpal exposes no input-preset hook. Digital gain intentionally skipped — the model is scale-invariant (§4.4) |
| Multiple concurrent wake words | ✅ `modelProcessors` map | ❌ single classifier at a time (front-end is shared, so cheap to add — §4.5) |
| microWakeWord engine | ✅ separate TFLite streaming engine | ❌ none (§4.6) |
| Per-device quirk handling | ✅ issue-device list | ⚠️ none as a list, but the rate calibration self-corrects the one HAL quirk we hit |
| Concurrency model | coroutine `Flow` | dedicated capture thread (`!Send` cpal) + tokio net runtime + FRB event stream |

**Takeaway (updated):** the core inference math *and* the full turn loop now work
on the target hardware — the robustness gaps VACA highlighted are largely closed
(smoothing, cooldown, barge-in, playback, live levels). What remains genuinely
missing is the **analog mic front-end**: hardware AEC/AGC/NS and the
`VOICE_RECOGNITION` input source (the real lever for faint far-field pickup),
both blocked on cpal not exposing those Android hooks. Multi-word / microWakeWord
are the remaining engine features, deferred as "more models" work.

---

## 4. How we can improve

Ordered by value-to-effort.

### 4.1 Add moving-average smoothing + cooldown (small, high value) — ✅ DONE

Implemented in `engine/gate.rs` as a pure, unit-tested `DetectionGate`: it fires on
the **moving average** of the last 3 block scores clearing `config.threshold`, with a
1500 ms cooldown so one utterance fires once. The engine loop feeds it each block and
the raised `active_threshold` while a turn is live. (The clock is injected into
`observe(..)` so the smoothing + debounce is tested without a live pipeline.)

### 4.2 Use Android hardware AEC/AGC/NS before writing our own (medium, high value) — ⏸ DEFERRED

Not yet needed for wake-word capture (human "hey jarvis" is now reliable) and its
self-triggering payoff is moot until TTS playback works — playback currently degrades
to silent on the device (the `ndk_context` panic; see §6 follow-up). Revisit together
with §4.4's mic-source once playback is restored and we can validate on hardware.

Our Plan defers AEC. VACA shows the pragmatic first step is the platform effects,
not custom DSP. Investigate whether we can attach `AcousticEchoCanceler`,
`AutomaticGainControl`, and `NoiseSuppressor` to our capture session:

- `cpal` 0.18 on Android drives AAudio and does **not** expose the `AudioRecord`
  `audioSessionId` or the `android.media.audiofx.*` effects. Options:
  1. Add a thin JNI/native shim to attach the effects to the capture session, or
  2. Capture via a small Kotlin `AudioRecord` layer (like VACA) and hand PCM to
     Rust over FRB, or
  3. Add a software AGC/NS stage in Rust (`resample.rs` neighbor) as a fallback.
- Track availability per source (hardware vs. software vs. unavailable) and
  surface it as a diagnostic event, like VACA's `agcSource/nsSource/aecSource`.

### 4.3 Port the mic-blanking self-trigger trick (small, high value for full-duplex) — ⏸ DEFERRED

Correct and cheap, but there is **no caller yet**: the device plays no local
confirmation chime, and TTS playback is currently silent (§6 follow-up). Adding the
hook now would be dead code. Land it alongside the first device-side sound (chime or
restored TTS), where it can be exercised end-to-end.

Implement `suppress_mic_for(duration)` in the capture/engine path: while active,
the drain loop substitutes **silence of the same length** instead of real samples,
used only around the device's own confirmation chime/error sounds — never during
TTS playback (so barge-in survives). Cheaper and more surgical than the Plan's
"raise threshold during SPEAKING," and keeps the AGC envelope clean.

### 4.4 Expose gain + mic-source selection (medium) — ⚠️ PARTIAL

- **Always-on level: ✅ done.** The engine now emits the RMS `Level` event even while
  a model is loaded (previously capture-only), so the UI meter and on-hardware tuning
  stay live during detection, plus a `wake-word diag: rms=… peak_score=…` logcat line.
- **Software gain: intentionally skipped.** openWakeWord's log-mel front-end is
  effectively **amplitude/scale-invariant** — a clean "hey jarvis" attenuated to
  `0.037×` (the Echo's far-field level) still scores **0.99** on the host. So a digital
  `mic_gain` applied after capture does *nothing* for detection (it only rescales the
  RMS meter and risks clipping). Not worth a config knob.
- **Mic-source (`VOICE_RECOGNITION`): the real far-field lever, still blocked.** This
  changes the *analog* capture path (platform AGC/NS + higher input gain), which is
  what would actually help the faint far-field case (the residual weakness is SNR /
  ADC quantization at ~6 effective bits, not digital amplitude). But `cpal` 0.18's
  AAudio backend opens the stream with `inputPreset = 0` and exposes **no** hook to set
  the preset (`configure_for_device` only sets device id / rate / buffer). Reaching it
  needs a `cpal` fork or the Kotlin `AudioRecord` capture layer from §4.2 — and live
  device validation. Deferred with §4.2.

### 4.5 Support multiple concurrent wake words (medium)

Generalize the detector to hold a map of classifier models sharing one
mel/embedding front-end (the front-end is the expensive part and is wake-word
independent). Emit the model id + score per detection. Matches VACA and is cheap
because only the tiny classifier is per-word.

### 4.6 Evaluate microWakeWord as a second engine (spike)

microWakeWord is lighter and streaming — a good fit for the 32-bit / 1 GB Echo
Show. Prototype a `MicroWakeWord` path alongside tract openWakeWord and compare
CPU/RAM and detection quality on-device. VACA ships ready-made
`hey_jarvis/ok_nabu/alexa/…` TFLite models we can benchmark against.

### 4.7 Keep tract, but note the runtime trade-off

VACA uses TFLite (with likely NNAPI/GPU delegate potential on some devices); we use
tract-onnx (pure Rust, no extra native deps, fully offline, deterministic). tract
is the right call for our single-binary Rust engine and the 32-bit target, but if
CPU headroom becomes a problem on-device, TFLite with a delegate is the escape
hatch VACA validates.

---

## 5. Recommended next actions

**Landed** (this pass): the §6 sample-rate fix that made detection work at all, plus
§4.1 (smoothing + cooldown, now a unit-tested `DetectionGate`) and the §4.4 always-on
level meter. Human "hey jarvis" is reliable on hardware.

**Next, in priority order:**

1. **Restore TTS playback** (`ndk_context` init — §6 follow-up). It gates the whole
   full-duplex story: without it §4.2 (self-trigger AEC) and §4.3 (mic-blanking) have
   nothing to guard against, and a spoken reply is the missing half of a turn.
2. **Mic-source / analog capture path** (§4.4 + §4.2). The one remaining wake-word
   weakness is faint far-field pickup (SNR/quantization-limited, *not* digital gain).
   The lever is the `VOICE_RECOGNITION` input source + platform AGC/NS, which needs a
   `cpal` fork or a Kotlin `AudioRecord` layer — do this as a measured spike on device.
3. **Then** §4.5–4.6 (more wake words / microWakeWord) as quality/perf work.

---

## 6. Field fix: Echo Show capture sample-rate mismatch (resolved)

First working on-hardware run. Wake-word detection scored ~0 on the device even
though the mic was clearly capturing speech (`rms` tracked room audio). Isolation:

- Host scoring of the **bundled models** against a real "hey jarvis" clip → **0.99**
  (model + pipeline correct), including after a 48 kHz→16 kHz pass through the engine
  `Resampler` and after attenuating the clip to the device's quiet far-field level.
- Dumping the exact 16 kHz PCM the engine fed the model and scoring it on the host →
  **~0.0001**, but re-scoring that same dump as if it were **8 kHz** (a 2x slow-down)
  → **0.998**. The captured audio was **exactly 2x too fast / an octave high**.

Root cause: on the Echo Show 8 (MediaTek AAudio HAL), `cpal`'s
`default_input_config()` reports **48 kHz**, but the stream is actually delivered at
**~24 kHz**. The engine trusted 48 kHz, so the linear resampler emitted every block at
double speed — pitch/time-distorted enough that confidence collapsed to ~0. (The mono
downmix was fine; only the rate label was wrong.)

Fix (`engine/mod.rs`): **measure the true input rate at startup** — drain a short
warm-up, count samples into the ring over a fixed wall-clock window, snap to the
nearest standard rate — and build the `Resampler` from that instead of cpal's number.
On device this calibrates `measured 24283 Hz -> 24000 Hz (cpal reported 48000)`; on
the host coreaudio measures its real rate and nothing changes. After the fix, live
scores hit 0.95–0.999 and human "hey jarvis" fires reliably.

Also landed alongside: §4.1 moving-average smoothing (3-frame) + 1500 ms cooldown, and
Info-level diagnostics (`wake-word diag: rms=… peak_score=…`, per-fire logs) that make
this kind of on-hardware debugging tractable from `logcat`.

Follow-ups surfaced: the faint far-field playback of a TTS "hey jarvis" from across the
room still misses occasionally (near the noise floor); a software **mic gain / AGC**
stage (§4.4) would firm that up. Real spoken wake words at conversational distance are
unaffected.
