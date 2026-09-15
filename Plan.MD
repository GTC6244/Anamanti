# Project Blueprint: Ambient Smart Display Voice Assistant

**Targets:** Echo Show 8 (LineageOS) + M4 Mac Mini

> **Status:** Planning / pre-implementation. Scope confirmed as a **full voice
> assistant** (STT → LLM → TTS) with local wake-word detection on-device.

---

## 0. Confirmed Decisions (2026-09-15)

These answers override any earlier ambiguity in this document:

| Question | Decision |
| --- | --- |
| **Scope** | **Full voice assistant** — wake word → stream audio → STT → LLM → TTS → spoken reply + on-screen text |
| **Service discovery** | **mDNS / Zeroconf** — the Echo Show auto-discovers the Mac Mini's Wyoming service on the LAN |
| **Wake word** | **openWakeWord `.onnx`** model executed on-device via `tract-onnx` (no custom training for v1) |
| **LLM runtime** | **Pluggable** — abstracted behind an interface; local (Ollama / llama.cpp) or cloud (Claude / OpenAI) are swappable |
| **TTS** | **Piper** via the Wyoming TTS protocol (local, Wyoming-native) |
| **Playback** | **Rust (cpal / oboe)** plays the returned TTS audio frames — symmetric with capture |
| **Barge-in** | **Full-duplex** — wake word stays active during playback so the user can interrupt at any time |
| **Echo cancellation** | **Deferred for v1** — ship full-duplex without robust AEC; accept some self-triggering and revisit (see §4) |
| **End-of-speech (VAD)** | **Server-side** — the Wyoming STT server decides end-of-speech; the device streams until told to stop |
| **Conversation memory** | **Persistent**, stored **on the Mac Mini** in **SQLite** (structured facts + preferences across sessions) |
| **Memory policy** | **Explicit + inferred** — remember on request *and* auto-extract facts/prefs from conversation |
| **Memory UX** | Manage via **settings list (view/delete)** + **voice** ("remember…", "forget that") |
| **Resilience** | **Auto-reconnect** with backoff via mDNS + a subtle **disconnected** status; slideshow keeps running |
| **Idle screen** | **Photo slideshow** sourced from a chosen **Google Photos / Drive** folder |
| **Photo auth** | **On-device OAuth** — the Echo Show runs the flow and calls Google directly |
| **AEC** | **Ship full-duplex**; raise wake-word confidence **threshold during playback** to suppress self-triggers; real AEC only if still bad |
| **Settings screen** | User-configurable: **LLM backend**, **TTS voice**, **wake word**, **photo source** |
| **Doc intent** | Onboarding + AI codegen — docs guide AI agents and humans to build from scratch |

---

## 1. System Architecture

```
[ Echo Show 8 (LineageOS) ]                         [ M4 Mac Mini (Wyoming host) ]
┌──────────────────────────┐                        ┌─────────────────────────────┐
│  Flutter UI Layer        │                        │  Wyoming STT (Whisper/CoreML)│
│  (Ambient screen, live   │                        │            │                │
│   transcript, reply text)│                        │            ▼                │
└───────────▲──────────────┘                        │  LLM Orchestrator           │
            │ FRB v2 streams / callbacks            │  (pluggable: Ollama / API)  │
┌───────────▼──────────────┐                        │            │                │
│  Rust System Engine      │ ── TCP (Wyoming) ────► │            ▼                │
│  • cpal/oboe capture     │ ◄── transcript ─────── │  Wyoming TTS (Piper)        │
│  • openWakeWord (tract)  │ ◄── synthesize audio ─ │                             │
│  • Wyoming client        │                        └─────────────────────────────┘
│  • cpal/oboe playback    │
└──────────────────────────┘
        ▲ mDNS discovery of _wyoming._tcp on the LAN
```

**End-to-end flow:** idle listening → wake word fires on-device → open Wyoming
socket → stream 16 kHz PCM → receive live `transcript` events (render on screen)
→ silence detected → LLM produces a reply (streamed tokens rendered as text) →
Piper synthesizes speech → audio frames stream back → Rust plays them through the
Echo Show speakers → return to idle.

---

## 2. Technical Stack

- **Frontend:** Flutter — always-on landscape layout, smooth live text rendering,
  simple image/status grids, tuned for the 8-inch display.
- **Systems layer:** Rust cross-compiled for Android. **The Echo Show 8 (`crown`)
  LineageOS build is 32-bit**, so the device target is `armv7-linux-androideabi`
  (armeabi-v7a), not `aarch64` — see Phase 1 ABI note in §3.
- **Interop:** `flutter_rust_bridge` v2 (FRB) — zero-copy Dart↔Rust data flow,
  generated stream sinks and callbacks.
- **Audio capture + playback:** `cpal` crate on the native Android `oboe-shared`
  backend (16 kHz, mono, 16-bit PCM for capture; playback of returned TTS frames).
- **Local wake word:** `tract-onnx` executing an **openWakeWord** `.onnx` model,
  fully offline in a low-overhead Rust thread.
- **Transport:** **Wyoming Protocol** over async `tokio` TCP.
- **Discovery:** mDNS / Zeroconf (`_wyoming._tcp`) to locate the Mac Mini.
- **Mac Mini services:** Wyoming STT (Whisper / CoreML) → pluggable LLM
  orchestrator → Wyoming TTS (Piper).

---

## 3. Implementation Phases

### Phase 1 — Workspace Setup & Cross-Compilation ✅ (implemented 2026-09-15)
- Unified monorepo: `/lib` (Flutter) and `/rust` (systems layer). **Done** —
  Flutter app scaffolded (`ambient_display`, Android-only), FRB v2 integrated via
  `flutter_rust_bridge_codegen integrate` (adds `/rust` cdylib + `rust_builder`
  cargokit plugin).
- Configure `flutter_rust_bridge_codegen` to watch and generate JNI bindings.
  **Done** — `flutter_rust_bridge.yaml` (`rust_input: crate::api`, output
  `lib/src/rust`). Hello-world API in `rust/src/api/engine.rs` (`engine_greeting`,
  `engine_version`); regenerate with `flutter_rust_bridge_codegen generate`.
- Install the NDK target: `rustup target add aarch64-linux-android`. **Done.**
- Validate a hello-world cross-compile onto the LineageOS target via `adb`.
  **Done and verified on real hardware.** Built, installed, and launched on the
  physical Echo Show 8; the on-screen greeting text is returned by the Rust engine
  over the FRB bridge (`android/arm`). Deploy command:
  `adb install -r build/app/outputs/flutter-apk/app-release.apk`.

> **⚠️ ABI correction (measured on-device):** the plan originally assumed
> **`aarch64-linux-android`**, but the real Echo Show 8 (`crown`) LineageOS build
> is **32-bit only** — `ro.product.cpu.abilist = armeabi-v7a,armeabi`, empty
> `abilist64`, **Android 11 (SDK 30)**. An arm64 APK fails to install with
> `INSTALL_FAILED_NO_MATCHING_ABIS`. **The real device target is
> `armv7-linux-androideabi` (armeabi-v7a).** Build with `flutter build apk
> --release --target-platform android-arm` (`rustup target add
> armv7-linux-androideabi`). Keep arm64 only for any future 64-bit device.

> **Toolchain pin (decision):** `flutter create` (Flutter 3.44) scaffolded
> **Gradle 9.1 / AGP 9.0**, but the bundled cargokit Gradle plugin uses the
> legacy AGP variant API + `project.exec` that Gradle 9 removed. Pinned
> `android/settings.gradle.kts` to **AGP 8.7.3 / Kotlin 2.1.0** and the wrapper to
> **Gradle 8.11.1**. Revisit if/when cargokit ships AGP-9 support. NDK in use:
> `28.2.13676358`; Rust `1.94.0`; FRB `2.11.1`.

### Phase 2 — Native Audio & Local Inference (Rust) ✅ (implemented 2026-09-15)
- Initialize `cpal` to pull raw 16 kHz / mono / 16-bit PCM from the mic array.
  **Done** — `rust/src/audio/capture.rs` opens the default input device, downmixes
  interleaved frames to mono `i16` in the (allocation-free) real-time callback,
  and pushes into the ring buffer. Device-native rate is preserved on capture and
  converted to 16 kHz downstream by a linear resampler (`audio/resample.rs`), since
  mic arrays rarely deliver exactly 16 kHz. Requires `RECORD_AUDIO` (added to the
  Android manifest) + the microphone feature.
- Feed the buffer stream through a pre-allocated ring buffer. **Done** —
  `rust/src/audio/ring_buffer.rs` wraps a single-producer/single-consumer
  `ringbuf::HeapRb` (allocated once, ~2 s at 48 kHz) so the RT capture callback and
  the inference loop exchange samples with only atomic index updates — no locks, no
  per-frame heap churn.
- Integrate `tract-onnx` + openWakeWord to score wake-word confidence continuously.
  **Done** — `rust/src/wakeword/detector.rs` runs the full openWakeWord 3-model
  chain on `tract-onnx`: melspectrogram (fixed 1280-sample chunks → 32-bin mel
  frames, `mel/10 + 2`) → feature/embedding (sliding 76-frame window, 8-frame hop →
  96-d embedding) → classifier (last 16 embeddings → confidence). Feeding fixed
  chunks gives every model a concrete input shape so `tract` fully optimizes the
  graph; rolling state is carried across calls for continuous streaming scoring.
- Keep evaluation fully offline in a low-overhead thread loop to respect the 1 GB
  RAM limit. **Done** — `rust/src/engine/mod.rs` owns one background thread that
  holds the `cpal` stream (which is `!Send`) and runs the drain→resample→score
  loop off the RT path. A process-wide start/stop lifecycle is exposed over FRB
  (`start_wake_word_engine` returns a `Stream<WakeWordEvent>`; `stop_wake_word_engine`,
  `is_wake_word_engine_running`). If the model files are absent the engine degrades
  to a capture-only mode that emits RMS levels, so the capture→ring→resample path
  stays observable. Verified on the host via `cargo check`/`cargo clippy` (clean)
  and a release `android-arm` APK cross-compile.

> **Backend note (measured):** the locked decision was "`cpal` on the `oboe`
> backend". The pinned `cpal` 0.18 no longer uses `oboe` — on Android it drives
> the NDK's **AAudio** input directly (via the `ndk` crate). This still honors the
> locked decision's intent (Rust `cpal` is the single audio layer that owns
> capture + playback); only the underlying Android backend changed from oboe →
> AAudio. No app-visible difference. **Consequence:** `cpal` links the NDK
> `-laaudio` library, which exists only at **API level ≥ 26**, so the app's
> `minSdk` was raised to **26** (`android/app/build.gradle.kts`); cargokit
> cross-compiles the Rust `.so` at `minSdk`, and a lower value fails to link with
> `unable to find library -laaudio`. The Echo Show 8 runs Android 11 (SDK 30), so
> 26 is a safe floor.

> **Dep note:** bumped `cc` (build-dependency, transitive via `tract-linalg`) from
> the Phase-1-locked `1.0.83` to `1.4.6`; `tract-linalg` 0.23.7's build script
> calls `cc`'s `cargo_warnings`, which the older `cc` predates. FRB event types use
> a flat struct + unit-only-enum tag (not a data-carrying enum) to avoid pulling
> the Dart `freezed` codegen dependency into the app.

### Phase 3 — Wyoming Client Core Logic (Rust) ✅ (implemented 2026-09-15)
- mDNS discovery of `_wyoming._tcp`; cache the resolved host/port. **Done** —
  `rust/src/wyoming/discovery.rs` browses `_wyoming._tcp.local.` via `mdns-sd`,
  returns the first resolved `WyomingEndpoint` (address/port/hostname) within a
  timeout, and always tears the daemon down. An `EndpointCache` holds the
  last-known host; `resolve()` prefers a fresh browse and falls back to the cache
  when the browse times out (the plan's "cached fallback"; full reconnect/back-off
  is Phase 5). No static IP is ever configured.
- Connection-state struct over `tokio::net::TcpStream`. **Done** —
  `rust/src/wyoming/client.rs` `WyomingConnection` owns the split buffered
  read/write halves plus the PCM format, sets `TCP_NODELAY`, and exposes
  `send_audio_start`/`send_chunk`/`send_audio_stop`/`read_event`. It is generic
  over the IO so the turn driver is testable against an in-memory duplex.
- **State machine.** **Done** — `rust/src/wyoming/state.rs` is a *pure* transition
  function (`Idle → Triggered → Streaming → Closing → Idle`; the plan's
  "Silence/Timeout" is `Closing`). Keeping it side-effect-free makes the whole turn
  lifecycle exhaustively unit-testable. The async driver `client::run_turn` binds
  it to a socket: on connect it sends `audio-start`, streams PCM pulled from an
  mpsc channel as `audio-chunk`s, and reads events back; a server `transcript`
  (server-side VAD's end-of-speech) or an idle timeout drives `audio-stop` +
  teardown.
- **Wire format.** **Done** — `rust/src/wyoming/protocol.rs` encodes/decodes the
  Wyoming framing (newline-delimited JSON header + length-prefixed `data` block +
  binary `payload`), matching the reference `rhasspy/wyoming` implementation so we
  interoperate with off-the-shelf servers (Phase 4).
- **Engine integration.** **Done** — `rust/src/engine/net.rs` bridges the `!Send`
  capture thread to the async client: a single-worker tokio runtime spawns a turn
  task on wake-word detection, and the inference loop forwards resampled 16 kHz PCM
  to it over a bounded channel (drops on overflow — never blocks the RT path).
  Turn progress surfaces to Dart as new `WakeWordEvent` kinds (`Connecting`,
  `Streaming`, `Transcript`, `Disconnected`).
- Wake-word evaluation keeps running **during** a turn (full-duplex). AEC is
  deferred for v1; the interim mitigation is wired here as a configurable
  `active_threshold` — the engine requires a higher wake-word confidence (never
  below the idle `threshold`) whenever a turn is active. **Done.**
- Verified: `cargo test` (25 tests: protocol round-trips, state-machine
  transitions, discovery cache fallback, a duplex mock-server turn, an idle
  timeout, and a real-`TcpStream` loopback dial), `cargo clippy -- -D warnings`
  (clean), `dart analyze` (clean after FRB regen), and a release `android-arm`
  APK cross-compile (25.5 MB) — so `tokio` + `mdns-sd` + `serde` all link on the
  32-bit device target.

> **Dep note (Phase 3):** added `tokio` (rt-multi-thread/net/io-util/sync/time/
> macros), `serde`+`serde_json` (Wyoming JSON framing), and `mdns-sd` 0.11
> (`_wyoming._tcp` discovery). `tokio` was already transitively present via FRB, so
> this only promotes it to a direct dependency. The Wyoming networking uses one
> worker thread to stay light on the ~1 GB budget. `minSdk` is unchanged (still 26,
> set in Phase 2); none of the new crates raise the floor.

### Phase 4 — Assistant Pipeline (Mac Mini)
- Wyoming STT (Whisper/CoreML) with **server-side VAD** emits the final transcript.
- **Pluggable LLM orchestrator** consumes the transcript, streams reply tokens.
- **Persistent memory store** (**SQLite** on the Mac) supplies context and records
  facts/preferences across sessions; feeds the LLM prompt. Policy: **explicit +
  inferred** (store on request and auto-extract from turns).
- Wyoming TTS (Piper) synthesizes the reply into audio frames.
- Frames stream back to the Echo Show over the Wyoming socket.

### Phase 5 — Reactive UI & Playback (Flutter + Rust)
- Dart screen state consumes FRB `StreamSink` events (transcript + reply tokens).
- Modular responsive 8-inch layout; smooth token-by-token rendering.
- **Idle ambient screen:** photo slideshow from a chosen Google Photos/Drive folder
  (**on-device OAuth**). Slideshow keeps running when disconnected.
- **Resilience:** auto-reconnect with backoff via mDNS + a subtle **disconnected**
  status indicator; wake words queue until reconnected.
- Rust (`cpal`/`oboe`) plays returned TTS audio frames through the speakers.
- **Full-duplex barge-in:** a wake word during playback interrupts and starts a new
  turn; **raise the wake-word threshold during playback** to suppress self-triggers.

### Phase 6 — Settings & Persistence
- Settings screen exposing: **LLM backend**, **TTS voice**, **wake word**, and
  **photo source** (with on-device Google auth + folder picker).
- **Memory management** in settings: view/delete stored items; voice commands
  ("remember…", "forget that").
- Wire settings through FRB to the Rust engine and the Mac orchestrator.

---

## 4. Resolved Details & Implementation Notes

All prior open questions are now decided (see the decision table). Notes for
implementers:

- **AEC / full-duplex:** ship full-duplex without dedicated AEC. Interim
  mitigation: **raise the wake-word confidence threshold while in SPEAKING** to
  suppress self-triggers from the device's own speaker. Only build real AEC
  (Rust-side WebRTC/speexdsp with the playback signal as reference, or Android
  hardware `AcousticEchoCanceler`) if self-triggering remains a problem.
- **Google Photos/Drive auth:** **on-device OAuth** — the Echo Show runs the
  consent flow and calls Google directly; the token is stored on the device.
  Needs a usable consent flow on the 8-inch screen and a folder picker in
  settings. *(Note: the claude.ai Google Drive connector in this environment is
  unauthorized and cannot prototype this — wire real OAuth into the app.)*
- **Persistent memory:** **SQLite** on the Mac. Schema holds structured facts +
  preferences with full-text search (no embedding model for v1). Policy is
  **explicit + inferred** — store on request and auto-extract facts/prefs from
  turns. UX: a memory list in **settings** (view/delete) plus **voice** commands
  ("remember…", "forget that"). Kept until cleared.
- **Resilience:** **auto-reconnect** with exponential backoff via mDNS. The idle
  photo slideshow keeps running while disconnected; a subtle **disconnected**
  indicator shows status; wake words queue until the socket is restored.

### Remaining risk to watch

- Whether threshold-tuning alone is enough to keep self-triggering tolerable
  during playback — measure on real hardware; AEC is the fallback.

---

## 5. AI Prompting Targets

- **Cargo setup:** "Generate a `Cargo.toml` for an Android target (aarch64) using
  `flutter_rust_bridge` v2, `cpal` with the oboe backend, `tokio` async, and
  `tract-onnx` for minimal-dependency neural-net execution."
- **Protocol interface:** "Write an async Rust Wyoming-protocol client using
  `tokio::io` that parses newline-delimited JSON frames and streams raw audio
  chunks immediately after the setup metadata, handling both `transcript` and
  synthesized-audio return frames."
- **mDNS discovery:** "Write a Rust routine to browse `_wyoming._tcp` via mDNS and
  resolve the Mac Mini's host/port, with a cached fallback."
- **LLM abstraction:** "Define a Rust/host-side trait for a pluggable LLM backend
  with a streaming `respond(transcript) -> token stream` method, implemented for
  both a local Ollama endpoint and a cloud API."
