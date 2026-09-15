# agents.md

Build guidance for AI coding agents (and humans) working in this repository.
Read this together with [`architecture.md`](./architecture.md) (the design) and
[`Plan.MD`](./Plan.MD) (phases, confirmed decisions, open questions).

---

## Project in one paragraph

A full ambient **voice assistant**: an Echo Show 8 (LineageOS) captures audio,
detects a wake word offline, streams speech over the Wyoming Protocol to an M4
Mac Mini for STT (Whisper), runs a pluggable LLM, and speaks the reply back via
Piper TTS — with the conversation rendered live on the display. The device side
is Flutter (UI) + Rust (audio, wake word, networking) bridged by
`flutter_rust_bridge` v2.

## Locked decisions (do not relitigate without asking)

- **Scope:** full voice assistant (STT → LLM → TTS), not transcript-only.
- **Discovery:** mDNS / Zeroconf (`_wyoming._tcp`). No hardcoded IPs.
- **Wake word:** openWakeWord `.onnx` via `tract-onnx`. No custom training in v1.
- **LLM:** pluggable behind a trait (local Ollama/llama.cpp **or** cloud API).
- **TTS:** Piper via Wyoming.
- **Playback:** Rust (`cpal`/`oboe`), symmetric with capture.
- **Barge-in:** full-duplex — wake word stays active during playback. **AEC is
  deferred for v1** (self-triggering is a known, accepted risk).
- **VAD:** server-side — the STT server decides end-of-speech; the device does
  not run its own VAD.
- **Memory:** persistent **SQLite** on the Mac; **explicit + inferred** policy;
  managed via settings list + voice ("remember…"/"forget that").
- **Idle screen:** photo slideshow from a Google Photos/Drive folder via
  **on-device OAuth**; keeps running when disconnected.
- **Resilience:** **auto-reconnect** with backoff via mDNS + a subtle
  disconnected indicator; wake words queue until reconnected.
- **AEC interim:** raise the wake-word confidence **threshold during playback**
  to suppress self-triggers; real AEC only if that's insufficient.
- **Settings:** LLM backend, TTS voice, wake word, photo source, and memory
  management are configurable.

If a task seems to require changing one of these, stop and confirm first.

## Repository layout

```
/lib      Flutter app (Dart) — UI, state, FRB Dart API
/rust     Rust engine — audio capture/playback, ring buffer, wake word,
          Wyoming client, mDNS discovery
Plan.MD           Living plan + decision table
architecture.md   Technical design (source of truth)
agents.md         This file
README.md         Product overview + setup
```

## Boundaries & ownership (respect these)

- **Rust owns** all real-time and resource-sensitive work: audio capture,
  playback, ring buffer, wake-word inference, Wyoming client, mDNS. Reason: the
  Echo Show has ~1 GB RAM and no room for GC pauses.
- **Flutter owns** presentation and user-facing state only. It does **not** touch
  audio buffers or sockets directly — it consumes FRB stream events.
- **The FRB v2 boundary** is the contract. Rust → Dart data flows through
  generated `StreamSink`s (`transcript_stream`, `reply_token_stream`,
  `state_stream`); Dart → Rust flows through generated function calls. Do not
  bypass FRB with ad-hoc channels/platform-channels.
- **The Mac Mini** hosts STT, LLM, and TTS. Keep the LLM behind its trait; never
  hardwire a single backend into the pipeline.

## Environment & build

```bash
# Rust Android target (one-time)
rustup target add aarch64-linux-android

# Generate the Dart/JNI bindings from Rust signatures
flutter_rust_bridge_codegen generate

# Build the engine for the device
cargo build --release --target aarch64-linux-android

# Run the app on the Echo Show (LineageOS) via adb
flutter run -d <echo-show-device>
```

- Requires: Flutter SDK, Android SDK + NDK, Rust toolchain, `adb`.
- Mac side: a Wyoming STT server (Whisper/CoreML) and Piper TTS on the LAN.

## Conventions

- **Rust:** async via `tokio`; keep the audio callback allocation-free; use the
  pre-allocated ring buffer rather than per-frame `Vec`s; format with `cargo fmt`;
  lint with `cargo clippy`.
- **Dart/Flutter:** stream-driven widgets (no polling); landscape-first layout for
  the 8-inch screen; `flutter format` / `dart analyze`.
- **FRB:** change Rust signatures, then regenerate — never hand-edit generated
  bindings.
- **Wyoming:** newline-delimited (`\n`) JSON control frames; raw PCM immediately
  after the metadata frame; one socket carries outbound audio and inbound
  transcript + TTS audio.

## Working agreements for agents

- Prefer the smallest change that satisfies the current phase in `Plan.MD`.
- When you make or discover a design decision, record it in `Plan.MD` (decision
  table) and reflect structural changes in `architecture.md`.
- Keep the three docs consistent: `README.md` (overview), `architecture.md`
  (design), `Plan.MD` (delivery). If you change behavior, update all three.
- Do not add a second audio path, a second interop mechanism, or an IP-based
  discovery fallback without confirmation — these violate locked decisions.
- Respect the memory budget: avoid large models, unbounded buffers, or holding
  full audio in memory on the device.

## State machine cheat-sheet (Rust Wyoming client)

`IDLE → TRIGGERED → STREAMING → THINKING → SPEAKING → IDLE`

- IDLE: wake-word scoring only; socket dormant; photo slideshow on screen.
- TRIGGERED: open TCP, send `audio-start`.
- STREAMING: send PCM frames; read `transcript` events; **server-side VAD** signals
  end-of-speech → send `audio-stop`.
- THINKING: LLM (with persistent memory) streams reply tokens (render live).
- SPEAKING: Piper audio frames play via `cpal`/`oboe`.
- Full-duplex: wake-word scoring keeps running through THINKING/SPEAKING; a wake
  word interrupts and starts a new turn (AEC deferred — raise the wake-word
  threshold during SPEAKING to suppress self-triggers).

Full diagram and wire format: [`architecture.md`](./architecture.md) §4.

## Good first tasks (from Plan.MD phases)

1. **Phase 1** — scaffold `/lib` + `/rust`, wire FRB codegen, prove a hello-world
   cross-compile onto the device via `adb`.
2. **Phase 2** — `cpal` capture → ring buffer → `tract-onnx` openWakeWord scoring.
3. **Phase 3** — `tokio` Wyoming client + mDNS discovery + the state machine.
4. **Phase 4** — Mac-side STT (server-side VAD) → pluggable LLM + persistent
   memory → Piper TTS pipeline.
5. **Phase 5** — Flutter reactive UI, idle photo slideshow, Rust TTS playback,
   full-duplex barge-in.
6. **Phase 6** — settings screen (LLM backend, TTS voice, wake word, photo source).

## Remaining risk to watch (see Plan.MD §4)

All design questions are decided (see locked decisions above). One empirical risk
remains: whether **threshold-tuning alone** keeps self-triggering tolerable during
playback — measure on real hardware; **Rust-side AEC** (WebRTC/speexdsp) is the
fallback if not.

Implementation note: the claude.ai Google Drive connector in this environment is
unauthorized and cannot prototype photo access — wire real **on-device OAuth**
into the app.
