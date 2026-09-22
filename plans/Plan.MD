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
| **Idle screen** | **Photo slideshow** — two selectable backends: **Google Photos** (Ambient API; ideal, but **partner-program gated**) and **Google Drive** folder (interim, works now); plus local gradients |
| **Photo auth** | **Ambient API:** on-device device-code + QR (`photosambient.mediaitems`) — blocked by Partner Program (`createDevice` 403). **Drive:** `drive.readonly` via one-time Mac consent (Desktop client) → token synced to device. Dual OAuth clients |
| **AEC** | **Ship full-duplex**; raise wake-word confidence **threshold during playback** to suppress self-triggers; real AEC only if still bad |
| **Settings screen** | User-configurable: **LLM backend**, **TTS voice**, **wake word**, **photo source** |
| **Camera proximity (2026-09-21)** | Front camera doubles as a **proximity sensor**: a cheap **frame-motion** detector in **Rust** (no ML, no face recognition) brightens the idle screen on approach and dims it after a quiet spell. Sensing lives in Rust (`camera/presence.rs` + Kotlin `CameraBridge` capture shim, twin of `MicBridge`); **brightness actuation stays in Flutter/Android** (`ScreenBrightnessController` → `MainActivity` `MethodChannel`). Degrades silently if the camera is unavailable (permission denied / **privacy shutter** closed / no camera) — the screen just stays bright. `CAMERA` is granted out-of-band (`pm grant`), like `RECORD_AUDIO` |
| **Tool calling / actions** | **rig-core is the default engine** (tool-calling loop); info tools run on the Mac, device actions (timers/alarms) emit `ambient-*` frames and execute **on-device**. Weather is grounded by a configured **home location** (`AMBIENT_HOME_LOCATION`). Calendar/reminders designed but deferred. |
| **Doc intent** | Onboarding + AI codegen — docs guide AI agents and humans to build from scratch |
| **Multi-device / multi-orchestrator (2026-09-22)** | (1) **Many displays → one orchestrator**: already correct — the accept loop handles each connection independently and replies on that socket; memory + settings are one shared **household** pool (speaker ID scopes per person, not per device). (2) **Many orchestrators → device picks one**: orchestrators advertise TXT `role`/`name`/`instance_id`; the device filters by `role=orchestrator` (excludes raw STT/TTS), enumerates all, and pins a chosen `instance_id` from a settings dropdown. `instance_id` resolves env → git **branch code** → sanitized service name, so a copy run from a test worktree self-names by its branch. "Local production" is a copied release binary at `/Volumes/External/DeveloperSupport/Ambient Orchestrator/` that sets `AMBIENT_INSTANCE_ID` in `~/.zshenv`; test copies run from their worktree on distinct ports. **Strict**: a pinned display stays offline if its orchestrator is unreachable (never switches Macs); `"Auto"` = first responder. (3) **Shared backend data**: SQLite in **WAL** mode for cross-process sharing; embedded HelixDB is single-process, so a test instance sharing prod data runs `AMBIENT_MEMORY_BACKEND=sqlite`. |

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

### Phase 4 — Assistant Pipeline (Mac Mini) ✅ (implemented 2026-09-15)
Implemented as a new standalone Rust crate in `/mac` (`ambient_orchestrator`,
binary `ambient-orchestrator`) — the "brain". It is a Wyoming **server** to the
Echo Show and a Wyoming **client** to the downstream Whisper (STT) and Piper (TTS)
services, inserting the pluggable LLM + persistent memory between them.
- Wyoming STT (Whisper/CoreML) with **server-side VAD** emits the final transcript.
  **Done** — `mac/src/wyoming/stt.rs` (`SttSession`) opens a `transcribe` +
  `audio-start` stream to the Whisper service, forwards the device's PCM, and waits
  for the server's `transcript` (server-side VAD end-of-speech) before sending
  `audio-stop`. The orchestrator relays that transcript back to the device.
- **Pluggable LLM orchestrator** consumes the transcript, streams reply tokens.
  **Done** — `mac/src/llm/` defines the `LlmBackend` trait
  (`respond(turn) -> ReplyStream` of reply-token fragments) with three config-
  selected implementations: local `ollama` (streaming NDJSON `/api/chat`), cloud
  `anthropic` (Claude Messages API over SSE via raw HTTP — Rust has no official
  Anthropic SDK), and an offline `mock` echo backend. The pipeline only ever holds
  a `dyn LlmBackend`, so backends swap without touching the turn logic.
- **Persistent memory store** (**SQLite** on the Mac) supplies context and records
  facts/preferences across sessions; feeds the LLM prompt. Policy: **explicit +
  inferred**. **Done** — `mac/src/memory/` is a `rusqlite` (bundled SQLite) store
  with an **FTS5** index. `parse_command` handles voice "remember…"/"forget…"/
  "forget that" (applied directly with a spoken confirmation, skipping the LLM);
  `infer_memories` auto-extracts facts/prefs from ordinary turns with deterministic
  heuristics (no embedding model, no extra LLM pass in v1). Relevant entries are
  FTS-searched each turn and injected into the LLM system prompt. `list`/`delete`/
  `forget_*`/`clear` back the settings UI (Phase 6).
- Wyoming TTS (Piper) synthesizes the reply into audio frames. **Done** —
  `mac/src/wyoming/tts.rs` (`TtsSession`) sends one `synthesize` request and reads
  back the `audio-start` → `audio-chunk`… → `audio-stop` stream.
- Frames stream back to the Echo Show over the Wyoming socket. **Done** —
  `mac/src/orchestrator.rs` (`Pipeline::run_turn`) drives the full turn and relays
  each TTS frame to the device (a device that closed early — Phase-3 clients end on
  the transcript — is tolerated, not an error). `mac/src/server.rs` accepts device
  connections and `mac/src/discovery.rs` advertises `_wyoming._tcp` over mDNS so the
  device discovers the host with no hardcoded IP (symmetric with the Phase-3 browse).
- Verified: `cargo test` (28 tests: Wyoming codec round-trips, STT/TTS session
  framing, Ollama-NDJSON + Claude-SSE stream parsing against loopback HTTP servers,
  memory FTS/commands/inference, and three full-pipeline integration tests that
  drive a mock device + mock Whisper/Piper through the real `Pipeline`),
  `cargo clippy --all-targets -- -D warnings` (clean), `cargo fmt --check` (clean),
  and a `--release` build of the `ambient-orchestrator` binary.

> **Config note (Phase 4):** the orchestrator is environment-driven
> (`mac/src/config.rs`). `AMBIENT_LLM_BACKEND=ollama|anthropic|openai|mock` selects
> the backend (default `ollama`, model `llama3.2`; `anthropic` needs
> `ANTHROPIC_API_KEY`, default model `claude-opus-5`; `openai` needs `OPENAI_API_KEY`,
> default model `gpt-4o-mini`, via `AMBIENT_OPENAI_MODEL`/`AMBIENT_OPENAI_MAX_TOKENS`;
> small `max_tokens` for spoken replies).
> `AMBIENT_STT_ADDR` / `AMBIENT_TTS_ADDR` point at the Whisper (10300) and Piper
> (10200) Wyoming servers, and the host advertises `_wyoming._tcp` on
> `AMBIENT_BIND_ADDR` (default `0.0.0.0:10700`). Whisper and Piper themselves are
> **off-the-shelf Wyoming servers** (rhasspy ecosystem); the orchestrator is a
> client to them, not a reimplementation.

> **Dep note (Phase 4):** a host-only crate, independent of the device `/rust`
> crate (which is an Android `cdylib` linking `cpal`/`tract`/JNI and cannot be
> shared as a library on the Mac). Uses `tokio`, `serde`/`serde_json`, `reqwest`
> (rustls-tls; NDJSON + SSE streaming), `rusqlite` (bundled SQLite + FTS5),
> `mdns-sd` (advertise), `async-trait`/`async-stream`/`futures-util`, and
> `env_logger`. The Wyoming wire codec is re-implemented in `mac/src/wyoming/
> protocol.rs` (kept byte-identical to the device's, guarded by round-trip tests)
> rather than shared across the two crates.

### Phase 5 — Reactive UI & Playback (Flutter + Rust) ✅ (implemented 2026-09-15)
- Dart screen state consumes FRB `StreamSink` events (transcript + reply tokens).
  **Done** — `lib/src/engine/assistant_controller.dart` folds the single
  `Stream<WakeWordEvent>` into an observable `AssistantState` (phase, transcript,
  streamed reply, connection status). The engine stream is **injected** so
  widget tests drive synthetic events with no native `.so`.
- Modular responsive 8-inch layout; smooth token-by-token rendering. **Done** —
  `lib/src/ui/`: `ambient_screen.dart` (layered slideshow + scrim + conversation +
  clock + status), `conversation_view.dart` (transcript/reply bubbles with a
  streaming caret, reading-column capped for the 8-inch display),
  `status_indicator.dart`. Reply tokens accumulate into the reply bubble as they
  arrive (`WakeWordEventKind::ReplyToken`). The reply text stays on screen for as
  long as the audio is being read aloud: the device relays audio faster than
  real-time, so the turn returns to idle while the playback ring is still draining;
  Rust tracks ring occupancy (`PlaybackSink::pending()`) and emits a UI-only
  `SpeakingDone` when it empties (or a barge-in flushes it), which is when the UI
  removes the text (`AssistantState.audioPlaying` / `displayActive`).
- **Idle ambient screen:** photo slideshow from a chosen Google Photos/Drive folder
  (**on-device OAuth**). Slideshow keeps running when disconnected. **Done** —
  `lib/src/slideshow/photo_source.dart`: a pluggable `PhotoSource`
  (`LocalPhotoSource` offline ambient gradients + `GooglePhotoSource` seam for the
  Phase-6 on-device OAuth folder picker) driven by a timer-based
  `SlideshowController` that is fully decoupled from connectivity and falls back to
  local imagery if the source fails. *(Per Plan §4, the real Google consent flow +
  folder picker land in the Phase-6 settings screen; the slideshow talks to the
  `GooglePhotoSource` seam.)*
- **Resilience:** auto-reconnect with backoff via mDNS + a subtle **disconnected**
  status indicator; wake words queue until reconnected. **Done** — the controller
  restarts capture with exponential backoff on a fatal engine error/stop (the
  engine-side half); the network-side half is the Rust turn's per-attempt mDNS
  re-discovery + cached fallback. A `disconnected` reason other than "turn
  complete" flags the subtle **Offline** chip while the slideshow keeps running.
- Rust (`cpal`) plays returned TTS audio frames through the speakers. **Done** —
  `rust/src/audio/playback.rs` opens the default output device (AAudio on-device),
  resamples each TTS `audio-chunk` to the output rate, fans mono → N channels, and
  feeds a lock-free ring the RT output callback drains (silence on underrun). The
  `!Send` output stream lives on the engine thread beside capture; the `Send`
  `PlaybackSink` goes to the turn task.
- **Full-duplex barge-in:** a wake word during playback interrupts and starts a new
  turn; **raise the wake-word threshold during playback** to suppress self-triggers.
  **Done** — the device turn state machine gained a `Speaking` state (kept
  `is_active()`, so the higher `active_threshold` stays in force during playback).
  A mid-turn detection calls `Network::on_wake_word`, which flushes playback,
  interrupts the running turn over a channel, and the finishing turn spawns a fresh
  one (`pending_restart`).
- Verified: `cargo test` (device 32 tests incl. playback resample/channel-fan-out,
  the SPEAKING transition, a full transcript→reply-token→TTS-playback turn, and a
  barge-in interrupt; mac 28 tests incl. the reply-token relay), `cargo clippy
  --all-targets -D warnings` + `cargo fmt --check` (both crates clean), FRB
  regenerated, `flutter analyze` clean, `flutter test` (3 reactive-UI widget
  tests), and a release `android-arm` APK cross-compile (26.3 MB) — so cpal's
  AAudio **output** and `path_provider` link on the 32-bit device target.

> **Wire note (Phase 5):** the device↔Mac hop gained one project-local event,
> `reply-token` (`data.text`), so the orchestrator streams LLM reply fragments to
> the device for token-by-token rendering. It rides the existing Wyoming framing
> and is only on the device↔orchestrator hop (no off-the-shelf server sees it);
> the codec is kept byte-identical on both sides (`rust/` and `mac/`). TTS audio
> continues to use standard `audio-start`/`audio-chunk`/`audio-stop`.

### Phase 6 — Settings & Persistence ✅ (implemented 2026-09-15)
- Settings screen exposing: **LLM backend**, **TTS voice**, **wake word**, and
  **photo source** (with on-device Google auth + folder picker). **Done** —
  `lib/src/ui/settings_screen.dart` groups the four areas. Wake word + detection
  thresholds and photo source are **device-local** (persisted); LLM backend/model
  + TTS voice are **orchestrator-managed** (read/changed over the network). Device
  settings persist to a JSON file via `lib/src/settings/settings_store.dart`
  (`AppSettings`, `path_provider`; injectable file path for tests — no
  `shared_preferences` plugin). The Google photo source is wired as a testable
  **seam** (`GoogleAuthenticator` + `photoSourceFromSettings` in
  `photo_source.dart`): the default `StubGoogleAuthenticator` reports honestly that
  real OAuth needs a registered client ID (per Plan §4, the environment can't
  provision one), while the folder-name/link UI and the `GooglePhotoSource` path
  are in place for it.
- **Memory management** in settings: view/delete stored items; voice commands
  ("remember…", "forget that"). **Done** — `lib/src/ui/memory_screen.dart` lists
  the orchestrator's stored facts/preferences and deletes individual entries or
  clears all, over the control protocol. The **voice** half already lands on the
  Mac during a turn (Phase 4 `parse_command`); this is the settings-list half.
- Wire settings through FRB to the Rust engine and the Mac orchestrator. **Done** —
  two wiring paths:
  - *Device-local → Rust engine:* changing the wake word/thresholds rebuilds the
    `WakeWordConfig` (`buildWakeWordConfigFrom`) and restarts the engine from
    `main.dart`; the photo source change refreshes the slideshow.
  - *Device → Mac orchestrator:* a **project-local Wyoming control protocol** on
    the device↔Mac hop (symmetric with the Phase-5 `reply-token` extension). New
    `ambient-*` frames (`ambient-describe-settings` / `ambient-set-settings` /
    `ambient-list-memories` / `ambient-delete-memory` / `ambient-clear-memories`
    and their responses) ride the existing framing. The orchestrator's
    `Pipeline` now reads a per-turn snapshot of runtime-swappable
    `SharedSettings` (`mac/src/settings.rs`), so the LLM backend + Piper voice
    change between turns with **no restart**; `mac/src/control.rs` answers the
    frames from the memory store + settings, and `mac/src/server.rs` routes control
    vs. turn frames on one accept loop. The device speaks the protocol from
    `rust/src/wyoming/control.rs`, exposed to Flutter as async FRB functions
    (`rust/src/api/settings.rs`: `fetch_orchestrator_settings`,
    `update_orchestrator_settings`, `list_memories`, `delete_memory`,
    `clear_memories`).
- Verified: `cargo test` (mac 42 tests incl. `SharedSettings` swap/atomic-reject,
  the `control::respond` surface, and a loopback control-protocol integration test;
  device 39 tests incl. control round-trips against a loopback orchestrator and a
  real bundled-model load/score on `tract`),
  `cargo clippy --all-targets -- -D warnings` + `cargo fmt --check` (both crates
  clean), FRB regenerated, `flutter analyze` clean (app code), `flutter test`
  (15 tests: settings store round-trip, settings-screen apply/offline/navigation,
  memory view/delete, plus the Phase-5 reactive-UI tests), and a release
  `android-arm` APK cross-compile (31.6 MB, incl. the bundled wake-word models) —
  so the new FRB control functions + settings UI build and link on the 32-bit
  device target.

> **Wake-word models (Phase 6):** the openWakeWord ONNX chain is now **bundled**
> in the app (`assets/models/`: `melspectrogram.onnx`, `embedding_model.onnx`, and
> the `hey_jarvis.onnx` classifier — from the dscripka/openWakeWord v0.5.1 release).
> `lib/src/engine/model_assets.dart` unpacks them to the on-device model dir on
> first run (preserving any user-dropped models), so a fresh install detects the
> wake word with no manual `adb push`. The default wake word is now `hey_jarvis`
> (the bundled classifier); the other names in the settings list work once their
> `<name>.onnx` is dropped in. A device-crate test loads the real bundled chain on
> `tract` and scores a stream, guarding that the shipped models parse and their
> shapes line up. Server-side VAD is used, so `silero_vad.onnx` is not needed.

> **Model selector (Phase 6 follow-up):** the assistant's model is chosen from a
> **drop-down of specific Anthropic / OpenAI models released in the last 12 months**
> (superseding the earlier free-text field). A new **OpenAI chat backend**
> (`mac/src/llm/openai.rs`, streaming `/v1/chat/completions`, `OPENAI_API_KEY`) sits
> behind the same `LlmBackend` trait next to `ollama`/`anthropic`/`mock`, so a picked
> GPT/o-series model drives the real turn. `mac/src/llm/catalog.rs`
> (`ModelCatalog`) builds the list: it live-queries each provider's `GET /v1/models`,
> filters to the trailing 12 months (Anthropic `created_at`, OpenAI `created` + a
> chat-model filter), caches ~1h, and falls back to a curated built-in list when a
> key is missing/offline. It is exposed to the device via `ambient-list-models` →
> `ambient-models` (new FRB `list_models` → `listModels`) and to the browser via
> `GET /models` on the config page. The chosen model flows through the existing
> `SharedSettings::apply` → `LlmFactory::build` path and persists to
> `ambient_settings.json`; the settings screen shows a dropdown for cloud backends
> and keeps free text for local Ollama tags. Verified: mac `cargo test` (native +
> helix) + clippy/fmt + `--features rig` check, device `cargo test`
> (`list_models` round trip), FRB regen, `flutter analyze` + `flutter test`.

> **TTS voice dropdown (Phase 6 follow-up):** the settings TTS voice field is a
> dropdown of **installed** Piper voices, mirroring the model dropdown. On
> `ambient-list-voices`, the orchestrator queries Piper's advertised catalog (a
> downstream Wyoming `describe` → `info`, parsed in `mac/src/wyoming/tts.rs`
> `describe_voices`/`parse_voice_catalog`) and, when `AMBIENT_TTS_VOICES_DIR` is set
> (Piper co-located on the Mac), keeps only voices whose `<name>.onnx` is present on
> disk; unset → the full advertised list. Exposed via `ambient-list-voices` →
> `ambient-voices` (new FRB `list_voices` → `listVoices`, `VoiceInfo`/`VoiceOption`)
> and to the browser via `GET /voices` on the config page. Both the device screen
> and the config page show a "Server default" entry, preserve a hand-set voice not
> in the list, and fall back to a free-text field when the list is unavailable.
> Verified: mac
> `cargo test` + clippy, device `cargo test` (`list_voices` round trip) + clippy,
> FRB regen, `flutter analyze` + `flutter test` (voice dropdown + free-text fallback).

> **Anthropic auth mode (Phase 6 follow-up):** cloud Claude can authenticate with an
> **API key** *or* a **Claude subscription (OAuth)**. `mac/src/llm/anthropic_auth.rs`
> adds `AnthropicAuth` + an `AnthropicTokenProvider` that resolves a subscription
> Bearer token from `ANTHROPIC_OAUTH_TOKEN` (from `claude setup-token`) or a
> configurable token-printing command (`AMBIENT_ANTHROPIC_TOKEN_CMD`, default the
> `ant` CLI), cached with a short TTL and refreshed on a 401; the subscription path
> sends `Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20` (no `x-api-key`).
> The mode is a per-turn `RuntimeSettings` field, rides the `ambient-set-settings`
> frame + config page as `anthropic_auth`, persists to `ambient_settings.json`, and
> is surfaced in the device settings screen (a selector shown for the Anthropic
> backend) and the config page. OpenAI stays API-key-only (no subscription→API path).
> `AMBIENT_ANTHROPIC_AUTH=apikey|subscription` seeds the initial mode.

> **Debug GUI pages (Phase 6 follow-up):** the loopback config server
> (`mac/src/webconfig.rs`) now also serves four read-only inspection pages —
> `/chatlog` (recent turns), `/prompts` (the exact assembled LLM prompt per turn,
> captured to a separate `ambient_promptlog.jsonl` via `memory::PromptLog`, env
> `AMBIENT_PROMPTLOG_PATH`), `/sqlite` (the memory store rows), and `/helix`
> (HelixDB GraphRAG node counts + a sample of nodes, behind the read-only
> `memory::GraphView` seam; reports "disabled" on the SQLite backend). Each has a
> `*.json` data endpoint and a shared nav bar. Still no auth — loopback only.
> Verified: mac `cargo test` (native + helix) + clippy `-D warnings`, plus an
> end-to-end curl of every page/endpoint against a running mock-backend orchestrator.

> **Wire note (Phase 6):** the `ambient-*` control frames are project-local to the
> device↔orchestrator hop (no off-the-shelf Wyoming server sees them), kept
> byte-identical in `rust/` and `mac/` `wyoming::protocol::types` and guarded by
> round-trip tests on both sides. Selecting the cloud (anthropic) backend at
> runtime still requires an API key on the orchestrator, but that key no longer has
> to be present at boot (see the runtime-key note below); if none is available the
> change is rejected **in-band** (the response's `ok:false` + message) and the
> previous backend stays in force, so a control error never drops the device
> connection or the turn.

### Camera proximity → screen brightness (post-Phase-6, 2026-09-21)

The Echo Show's **front camera is used as a proximity sensor** to brighten the idle
screen when someone approaches and dim it when the room is quiet — no ML, no face
recognition, no frames ever leave the device.

- **Boundary:** sensing in Rust, actuation in Flutter (per `agents.md`). A Kotlin
  `CameraBridge` (the visual twin of `MicBridge`) opens the front camera via Camera2
  at a tiny resolution (176×144, ~5 fps), pulls the packed **Y (luma)** plane from
  each frame, and pushes it over JNI to `rust/src/camera/bridge.rs`. The pure,
  unit-tested `camera/presence.rs` `PresenceDetector` measures the **mean absolute
  per-pixel luma delta** (frame-to-frame motion); it flips to *present* above a
  motion threshold and back to *absent* after a quiet **release window** (defaults
  2.5 / 20 s, both config-tunable via `WakeWordConfig`). Transitions emit a new
  `WakeWordEventKind::Presence` (`present: bool`) on the existing engine event stream.
- **Actuation (presentation):** `AssistantController` folds `Presence` into
  `AssistantState.userPresent`; `ScreenBrightnessController` crosses a
  `ambient_display/brightness` `MethodChannel` to `MainActivity`, which sets the
  window `screenBrightness` (near 1.0 / away 0.25, tunable). Starts **bright** and
  stays bright if the camera is unavailable.
- **Away-mode UI:** `userPresent` also drives the ambient screen's presentation, not
  just the backlight — when nobody's in front (and no turn is active), everything
  fades out and only a **large, dimmed, centered clock** remains (`AmbientScreen`
  `offMode = !userPresent && !displayActive`); the slideshow, status chip, settings,
  timers, and wake-word hint all hide. Someone approaching flips it back to the full
  ambient screen. This makes presence visible independent of the (subtle, panel- and
  ambient-light-dependent) backlight change.
- **Lifecycle & failure modes:** the camera is opened alongside the engine and
  released on teardown; it is fully independent of the audio pipeline. It **degrades
  silently** — `CAMERA` permission denied, the **physical privacy shutter** closed
  (frames go black → no motion → treated as absent/quiet), or no front camera — never
  breaks capture, wake-word detection, or a turn. Known v1 limitation: a perfectly
  **still** person eventually reads as absent and the screen dims until the next
  movement (motion-only signal, by design/choice).
- **Permission:** `CAMERA` is a runtime-dangerous permission declared in the
  manifest (`uses-feature` `required=false`) and, like `RECORD_AUDIO`, granted
  out-of-band on the kiosk device: `adb shell pm grant com.ambientdisplay.ambient_display android.permission.CAMERA`.
- **Follow-ups (not done):** no settings-screen toggle yet (on by default via
  `WakeWordDefaults.cameraProximity`); brightness levels + motion/release tuning are
  compile-time defaults. Could later add an ambient-light (mean-luma) auto-brightness
  curve and/or a settings toggle.

### Tool calling & actions (post-Phase-6)

The assistant can call **tools** to fetch info and take actions. Mechanism: the
LLM is handed tool definitions (name + description + JSON-schema args); if it asks
for one, the orchestrator executes it, threads the result back, and the model
answers (`mac/src/llm/rig.rs`, up to `MAX_TOOL_ROUNDS`). Adding a capability = one
more tool in `Tools`.

- **Engine:** rig-core is **always compiled in** and is the **default** engine
  (`AMBIENT_LLM_ENGINE=rig`); `native` opts back out to the tool-less HTTP backends.
  Tool turns answer **non-streaming** (rig 0.42's ollama streaming parser drops
  non-final tool calls) — fine, since the reply is spoken whole.
- **Info tools (Mac-side):** `internet_search` (DuckDuckGo keyless / Tavily with
  `TAVILY_API_KEY`), on by default (`AMBIENT_WEB_SEARCH`). **Weather works** via
  this tool; it is grounded by a configured home location + units
  (`AMBIENT_HOME_LOCATION` / `AMBIENT_WEATHER_UNITS`), injected into the system
  prompt next to the wall-clock and speaker-identity lines (`location_line`).
- **Device actions (implemented, on-device):** the `set_timer` / `cancel_timer`
  tools emit a `DeviceAction` onto a per-turn `ActionSink` (`LlmTurn.actions`); the
  orchestrator's reply loop drains it (`drain_device_actions`) and relays an
  `ambient-timer` frame on the turn socket. The device decodes it
  (`TurnUpdate::Timer`) and its long-lived `TimerManager`
  (`rust/src/engine/timer.rs`) owns each countdown (unlimited, outliving the turn
  socket + surviving a Mac disconnect), sounds a synthesized chime via the shared
  `PlaybackSink`, and emits `WakeWordEventKind.timer{Started,Finished,Cancelled}`.
  Flutter renders a countdown-chip overlay visible during idle + turns. The full
  add-a-capability process is documented in `architecture.md` §8.
- **Calendar / reminders:** designed, **deferred** — backend (macOS EventKit vs
  Google vs CalDAV) undecided.

> **Runtime provider keys (Phase 6 follow-up):** a cloud API key no longer has to be
> in the environment at launch. `SettingsUpdate` gains optional `anthropic_api_key` /
> `openai_api_key` fields (tri-state like the Tavily search key: absent = keep,
> `null` = clear, value = set); `SharedSettings::apply` builds the backend with a
> factory clone whose keys reflect the target, so a key entered at runtime enables the
> cloud backend **without a restart**. Keys live in `RuntimeSettings` (the env var is
> only the boot seed) and persist to `ambient_settings.json` (0600); on load a
> persisted key overlays the env key only when present, so a pre-feature settings file
> can't wipe a working env key. Key **entry** is exposed only on the loopback config
> page (`webconfig.rs` — password fields shown per backend); the device/Wyoming
> control path never accepts a provider key, and both surfaces report only
> `anthropic_key_set` / `openai_key_set` (a boolean), never the key value. Endpoints
> (base URLs) stay boot-time env only — deferred. Verified: mac `cargo test`
> (107 unit + integration, incl. runtime-key enable + persist tests) + clippy `-D
> warnings` + fmt.

---

## 4. Resolved Details & Implementation Notes

All prior open questions are now decided (see the decision table). Notes for
implementers:

- **AEC / full-duplex:** ship full-duplex without dedicated AEC. Interim
  mitigation: **raise the wake-word confidence threshold while in SPEAKING** to
  suppress self-triggers from the device's own speaker. Only build real AEC
  (Rust-side WebRTC/speexdsp with the playback signal as reference, or Android
  hardware `AcousticEchoCanceler`) if self-triggering remains a problem.
- **Photo auth (two backends, selectable in settings):**
  - **Google Photos Ambient API** (the ideal): OAuth 2.0 for TVs and Limited Input
    devices (device-code + QR), fully on-device — sign-in QR → `createDevice` →
    album-picker QR → poll `mediaSourcesSet` → `listMediaItems` (`baseUrl` +
    `=w…-h…` + Bearer). Scope `photosambient.mediaitems` (the flow rejects
    `photosambient.tv`). **Gated: the Ambient API requires Google Photos Partner
    Program acceptance** — `createDevice` returns 403 until the project is accepted
    (a valid token/scope is not enough). Built + on-device; works once accepted.
  - **Google Drive** (interim): `drive.readonly` folder listing. The device-code
    flow rejects Drive scopes, so consent runs **once on the Mac**
    (`tools/google_photo_consent.py`, loopback+PKCE, a **"Desktop app"** client) and
    the refresh token is adb-pushed to the device (Settings → Import Drive token).
    Reads a folder (owned or "Shared with me"); `drive.file` is unusable here (can't
    list a folder without an interactive Picker).
  - Both clients' creds are injected at build via
    `--dart-define-from-file=google_oauth.json` (gitignored): `GOOGLE_OAUTH_*` (TV) +
    `GOOGLE_DRIVE_*` (Desktop). Ruled out: the Photos Library API (no library read
    since March 2025). Implemented in `slideshow/ambient_photos.dart`,
    `drive_photos.dart`, `photo_source.dart`, settings, `main.dart`. Consent screens
    stay in **Testing** mode with household test users.
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
