# agents.md

Build guidance for AI coding agents (and humans) working in this repository.
Read this together with [`architecture.md`](./plans/architecture.md) (the design) and
[`Plan.MD`](./plans/Plan.MD) (phases, confirmed decisions, open questions).

> **Display UI inventory.** [`DisplayUI.md`](./plans/DisplayUI.md) is the canonical,
> category-by-category catalog of everything that can appear on the Anamanti Display
> screen — backgrounds/idle, overlays, widgets, banners, full-screen views, status
> indicators, and settings — with file paths and how each is triggered. When you add,
> remove, or restructure any on-screen UI, update `DisplayUI.md` in the same change.

> **Naming.** The project is **Anamanti** (from the Irish *anam an tí*, "soul of
> the home"; pronounced **"AN-um un TEE"** — formerly "Easy Home"/"Ambient"). It
> has two halves: **Anamanti Core**, the Mac-side brain (formerly "the
> orchestrator"), in the **`anamanti-core/`** directory — crate `anamanti_core`,
> binary `anamanti-core` — and **Anamanti Display**, the app on the Echo Show, in
> **`anamanti-display/`** (Dart package `anamanti_display`, Android id
> `com.anamanti.anamanti_display`). The per-instance config file is `anamanti.json`,
> env vars are `ANAMANTI_*`, the mDNS TXT role is `role=core`, runtime data files
> are `anamanti_*` (`anamanti_memory.sqlite`, `anamanti_settings.json`,
> `anamanti_chatlog.jsonl`), and Wyoming wire frames use the `anamanti-*` prefix
> (`anamanti-listen`, `anamanti-notify`, …). Device and Core must be rebuilt/
> redeployed together (the frame names, mDNS role, config filename, env vars, and
> data-file names all changed).
> **Deliberately NOT renamed (leave as-is):** the internal turn-pipeline
> module `orchestrator/` (`orchestrator::…`, split into `mod.rs`/`turn.rs`/`vad_loop.rs`/
> `prompt.rs`/`system1_handlers.rs`/`speech_out.rs`); the FRB native lib crate
> `rust_lib_ambient_display` (kept so cargokit/gradle/podspecs keep building); and
> the Snapcast/music-routing infra ids (`ambient-mpv`, `ambient-snapserver`,
> `ambient-librespot`, `ambient-snapclient`, launchd `com.ambient.*`) — these are
> deployment identifiers, not Wyoming frames.
Feature-specific plans branch off these — e.g.
[`MusicPlan.md`](./plans/MusicPlan.md) (Spotify playback via the Anamanti Core),
[`RecipePlan.md`](./plans/RecipePlan.md) (recipe mode: the `recipe_lookup` tool +
the `anamanti-recipe` frame + the display's 3-tab recipe screen), and
[`WeatherPlan.md`](./plans/WeatherPlan.md) (weather: the `weather_lookup` tool +
the `anamanti-weather` frame + the display's full-screen forecast and the ambient
icon/temperature beside the clock), and
[`PlacesPlan.md`](./plans/PlacesPlan.md) (places: the `places_lookup` tool over the
Google Places API + the `anamanti-place` frame + the display's full-screen place card,
plus the route-only System-1 `place` intent), and
[`UpdaterPlan.md`](./plans/UpdaterPlan.md) (in-app APK auto-updater: Rust fetch/
download/SHA-256-verify of a signed APK + `latest.json` from Cloudflare R2, a native
`PackageInstaller` channel, and the `selfUpdate`/`fdroid` build flavors), and
[`FontScalePlan.md`](./plans/FontScalePlan.md) (voice "increase/decrease font": a global
device-wide `TextScaler` on all ambient text, the `adjust_font` tool + the `anamanti-font`
frame, and the orthogonal `screen.font` display-context sibling that gates it), and
[`ElevenLabsSttPlan.md`](./plans/ElevenLabsSttPlan.md) (a third, selectable STT engine:
ElevenLabs Scribe v2 Realtime over WebSocket, behind the existing `Transcriber`/`SttEngine`
seam alongside `wyoming` and `whisper-rs`; boot-time `stt.engine=elevenlabs`, the
`ELEVENLABS_API_KEY` secret, Core VAD stays the segmenter), and
[`speaker_id_plan.md`](./plans/speaker_id_plan.md) + [`SpeakerEnrollmentPlan.md`](./plans/SpeakerEnrollmentPlan.md)
(per-person voice recognition: an always-compiled ECAPA-TDNN voiceprint embedder
identifies who is speaking on **every** turn's voiced PCM — not the wake word — and
scopes memory/prompt per person; opt-in via `speaker.enabled`+`speaker.model_path`, with
a device **People** screen whose guided "Add a voice" flow (existing household member or new person) actively enrolls + verifies a
voice via an `enroll` display context on the ordinary turn — no second audio path), and
[`RadioPlan.md`](./plans/RadioPlan.md) (internet radio: a curated station list on the
Core's Music & Devices page with Radio Browser logo lookup, played by the existing mpv →
snapserver `Web` stream on the same speakers as Spotify, the `radio_control` tool, and a
radio variant of the display's now-playing screen over the existing `anamanti-music` push), and
[`MusicDisplayStylePlan.md`](./plans/MusicDisplayStylePlan.md) (the music widget while
Spotify/radio plays — compact bar or full screen, **per display**: full screen on the display
the play request came from, compact elsewhere; each display can switch itself for the current
music via its expand/minimize button or the `music_screen` tool, and Music & Devices can pin a
display. Core-owned (`MusicDisplayRouter` + persisted pins), stamped per display on each
now-playing push; the display sends its `device_id` on `audio-start` + music-control), and
[`SpeakerScopedMemoryPlan.md`](./plans/SpeakerScopedMemoryPlan.md) (memory scopes
`person` / `household` / `unknown`: a fact said by an unrecognized voice is kept but never
recalled for anyone; per-turn speaker resolution `identified|tentative|new|unknown`;
"who am I?" answered from the voiceprint; admin-console speaker tags + unknown-fact review), and
[`HomeAssistantPlan.md`](./plans/HomeAssistantPlan.md) (Home Assistant: a **pre-System-1 home-command
fast path** — string-matched against a cached HA device catalog, one direct REST call, a fixed spoken
confirmation, no model — plus the `home_control` rig tool as the LLM fallback; HA's Assist is
deliberately not used. Part B, planned: an opt-in on-screen HA dashboard widget).

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
- **Discovery:** mDNS / Zeroconf (`_wyoming._tcp`). No hardcoded IPs. The device
  filters browse results by TXT `role=core` (so raw Whisper/Piper Wyoming
  servers are never picked) and can **pin a specific Anamanti Core** by its stable
  TXT `instance_id` (settings screen "Anamanti Core" dropdown; strict — stays
  offline rather than switching Macs; `"Auto"` = first responder).
  **Deployment:** the Anamanti Core is configured by a **per-instance JSON file**
  (`anamanti.json` in the working directory by default; `--config <path>` overrides).
  Only provider API keys/tokens remain environment variables (secrets); everything
  else — identity, endpoints, feature toggles — lives in the JSON file. The "local
  production" Anamanti Core is installed at
  `/Volumes/External/DeveloperSupport/Anamanti Core/` (a copied release
  binary, run outside any git checkout) and pins its identity via `instance_id` in
  that folder's `anamanti.json`; its provider keys still come from `~/.zshenv`. Test
  copies run from their git branch/worktree, where `instance_id` is left unset so it
  defaults to the branch code; give each a distinct `bind_addr` + `config_addr` (and
  `service_name`) in its own `anamanti.json` to run alongside production.
- **Wake word:** openWakeWord `.onnx` via `tract-onnx`. No custom training in v1.
- **LLM:** pluggable behind a trait (local Ollama/llama.cpp **or** cloud API).
- **TTS:** Piper via Wyoming.
- **Playback:** Rust (`cpal`/`oboe`), symmetric with capture.
- **Barge-in:** wake word stays active during playback; saying it again flushes
  playback immediately and starts a fresh turn, and sends an `anamanti-interrupt`
  frame so the Anamanti Core aborts the in-flight LLM + TTS. **In-app AEC is not part
  of this build**, but echo cancellation *is* provided at the device layer by a
  required audio-HAL shim (`libamznaec_shim.so` — SpeexDSP linear AEC against the Echo
  Show's DAC loopback, ~16 dB, talker-preserving), so barge-in over playback yields
  clean transcripts. See the AEC-interim decision + risk section below, architecture.md
  §4, and TODO.md §2.
- **VAD:** off-device — the **Anamanti Core** decides end-of-speech (neither STT
  engine has streaming VAD, so the Mac sends `audio-stop`). The device never runs
  its own VAD. The detector is **pluggable behind a `SpeechGate` seam**
  (`anamanti-core/src/vad/`). **As of 2026-09-30 the neural Silero engine is the
  committed default** (`vad.engine="silero"`, `VadConfig::default()`; the `vad-silero`
  build feature is **on by default**) — an energy/RMS gate is more robust to background
  noise's amplitude-vs-speech ambiguity than the old energy default. Consequences of the
  flip: a default `cargo build` pulls onnxruntime and a default boot **requires the
  Silero v4 model on disk** (`anamanti-core/scripts/fetch-vad-model.sh <dir>`) — a
  `silero` boot with no model is a **hard error** (fail-loud). The **energy/RMS gate**
  is now the opt-in fallback (`vad.engine="energy"`, or an energy-only
  `cargo build --no-default-features`) and the automatic fallback when a `vad-silero`
  build has no model loaded. See `plans/VadSileroPlan.md`.
- **Memory:** persistent **SQLite** on the Mac is the store of record for
  **explicit + inferred** facts (writes + the settings list + voice
  "remember…"/"forget that"). **Retrieval/recall defaults to the embedded HelixDB
  GraphRAG backend** (in-process, no server/Docker) — every completed turn is
  appended to `anamanti_chatlog.jsonl` and a background ingester embeds it into the
  graph. **Embeddings default to a local, offline model** — nomic-embed-text-v1.5 run
  in-process via `fastembed`/onnxruntime (`graphrag.embed_backend="local"`, feature
  `embed-local`, ON by default); it needs no network and no API key, only the ONNX +
  tokenizer on disk (`scripts/fetch-embed-model.sh`). `graphrag.embed_backend="openai"`
  is the opt-in cloud path and then needs `OPENAI_API_KEY`. The backend is **hot-swappable
  live** from the config page (Memory → Recall settings, `/memory#settings`) with no restart; each backend keeps its
  **own per-signature store** (`helix_path/<label>-<dims>/`) so switching is lossless (the
  durable SQLite facts + chat log are never touched — only the derived vector index differs).
  If the chosen embedder init
  fails, recall **falls back to SQLite FTS** (writes are unaffected). Override with
  `memory_backend: "sqlite"` in `anamanti.json` for pure FTS recall. (Pure-Rust `tract`
  cannot load the nomic graph — its rotary `Range` op does not lower to a typed model —
  so the local embedder runs on onnxruntime via `ort`, the same pin as `vad-silero`.)
  The HelixDB engine, the rig agent framework, and the ECAPA-TDNN speaker embedder are
  **always compiled in** (no longer feature-gated); speaker ID is selected purely at
  runtime via `speaker.enabled` / `speaker.model_path`.
- **Idle screen:** photo slideshow from a Google Photos/Drive folder; keeps running
  when disconnected. Google Photos (Ambient) links via **on-device OAuth**
  (device-code/QR); **Google Drive links on the Anamanti Core** (consent on the Mac,
  config page → Integrations → Google Drive, `/integrations#drive`) and the device pulls the token over Wyoming.
- **Resilience:** **auto-reconnect** with backoff via mDNS + a subtle
  disconnected indicator; wake words queue until reconnected.
- **AEC interim:** raise the wake-word confidence **threshold during playback** to
  suppress self-triggers. In-app AEC was attempted on hardware and deferred — the
  `VOICE_COMMUNICATION` preset doesn't cancel on this device, and software AEC is
  net-negative for flush-on-wake barge-in (no simultaneous echo to cancel). Echo
  cancellation is instead delivered **outside this build** by a required device-side
  audio-HAL shim (`libamznaec_shim.so`, SpeexDSP, ~16 dB) — see the risk section below,
  architecture.md §4, and TODO.md §2. The raised threshold remains the in-app
  mitigation layered on top.
- **Settings:** LLM backend, TTS voice, wake word, photo source, and memory
  management are configurable.
- **Output personalities:** the assistant can speak in a selectable **personality**
  (overly enthusiastic, heavy sarcasm, depressed, 1940s gangster, Donald Trump; normal =
  off). **Pure output tuning** — it changes only how a reply *sounds*, never memory,
  recall, facts, or tool use. On/off + selection are drivable **by voice** two ways: a
  deterministic **fast path** (`orchestrator::detect_personality_switch`) handled *before*
  the LLM — a "talk like a gangster" / "go back to normal" utterance (switch trigger + a
  catalog name, or an explicit off intent) flips the live personality in place and speaks a
  canned confirmation, skipping the model — and, as a fallback the model can invoke, the
  always-on **`set_personality`** rig tool, which emits a **Core-side**
  `DeviceAction::SetPersonality`. Both apply to `SharedSettings` via the same
  `set_active_personality` (no device frame); the orchestrator appends a fenced style block
  to each turn's prompt from the snapshot. State + the **editable catalog** live in the settings overlay (`Personality`
  in `settings.rs`, persisted to `anamanti_settings.json`, no `anamanti.json` seed) and
  are edited on the Core GUI **Personality page** (`/personality`). Core-only; no Display
  UI. See architecture.md §8.4 and the Plan.MD decision row.
- **Proactive notifications (Approach A):** the Anamanti Core can push **visual**
  notifications to the display *unprompted* (no voice turn) over a **persistent,
  device-dialed** Wyoming channel (`anamanti-hello` → `anamanti-notify`), separate from
  the per-turn voice socket. The device is still the dialer (reuses mDNS + the
  `instance_id` pin + auto-reconnect); the Mac only pushes down the open socket. Rust
  owns the socket + reconnect (`anamanti-display/rust/src/wyoming/notify.rs`,
  `start_notify_channel`); Flutter shows a dismissible banner
  (`NotificationController`); the Anamanti Core keeps a `NotificationService` registry
  (config-page **Notify** tab pushes a test). Visual-only for now — no spoken output,
  no state-machine interaction. Design: architecture.md §4; follow-ups: TODO.md §6a.
  The ambient **weather** (`role=weather`) and **music now-playing** (`role=music`)
  pushes reuse this exact design, and all three share one implementation per side: the
  device's `wyoming/push_channel.rs` driver + `start_channel`/`stop_channel` in
  `api/engine.rs`, and the Core's `notify::ChannelRegistry` + `server.rs` `PushChannel`.

If a task seems to require changing one of these, stop and confirm first.

## Repository layout

```
anamanti-display/   Anamanti Display — everything installed on the Android device
                    (the Echo Show). The Flutter project root: Dart UI in `lib/`,
                    the Rust engine (audio capture/playback, ring buffer, wake
                    word, Wyoming client, mDNS) in `rust/`, `android/`, cargokit
                    `rust_builder/`, bundled openWakeWord models in `assets/`, and
                    the Flutter `test/` + `integration_test/`.
anamanti-core/      Anamanti Core — everything that runs on the Mac (crate
                    `anamanti_core`) — Wyoming server to the device + Wyoming
                    client to Whisper/Piper, pluggable LLM, HelixDB/SQLite memory,
                    mDNS.
anamanti-protocol/  Shared Wyoming protocol crate (`anamanti_protocol`): the frame
                    types, constructors/predicates and the size-capped framing
                    reader. A path dependency of BOTH anamanti-core and
                    anamanti-display/rust (each re-exports it as
                    `crate::wyoming::protocol`) — a wire change is made once, here,
                    and both sides must be redeployed together.
plans/              Design + planning docs: architecture.md (design, source of
                    truth), Plan.MD (phases + decision table), TODO.md,
                    DisplayUI.md (the Display's on-screen UI catalog), the
                    *_plan / rollout notes, and MusicPlan.md.
agents.md           This file (repo root).
CLAUDE.md           Harness entry point; points here (repo root).
README.md           Product overview + setup (repo root).
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
# Rust Android target (one-time). Echo Show 8 (crown) LineageOS is 32-bit:
rustup target add armv7-linux-androideabi

# All Flutter / FRB commands run from the anamanti-display/ project root:
cd display

# Generate the Dart/JNI bindings from Rust signatures (reads anamanti-display/flutter_rust_bridge.yaml)
flutter_rust_bridge_codegen generate

# Build a device APK (cargokit cross-compiles the Rust engine into it).
# Echo Show 8 (crown) is 32-bit armeabi-v7a — use android-arm, NOT android-arm64
# (arm64 fails with INSTALL_FAILED_NO_MATCHING_ABIS on this device).
#
# A `distribution` flavor dimension exists (selfUpdate / fdroid — the in-app R2
# auto-updater, plans/UpdaterPlan.md), so EVERY apk build/run must
# name a flavor. `selfUpdate` is the normal build (ships the updater); `fdroid`
# ships without it. The APK is then named app-<flavor>-release.apk.
flutter build apk --release --flavor selfUpdate --target-platform android-arm

# Run the app on the Echo Show (LineageOS) via adb (path relative to anamanti-display/)
adb install build/app/outputs/flutter-apk/app-selfUpdate-release.apk   # or: flutter run --flavor selfUpdate -d <echo-show-device>
```

- Requires: Flutter SDK, Android SDK + NDK, Rust toolchain, `adb`.
- Mac side: Piper TTS on the LAN, plus **STT** — one of three engines behind the
  `Transcriber`/`SttEngine` seam: the external `wyoming-faster-whisper` server
  (default, `stt.engine=wyoming`), the in-process whisper.cpp engine
  (`stt.engine=whisper-rs`, built with `--features stt-whisper-local`; no Python STT
  server; see `plans/python-to-rust-whisper.md`), **or** ElevenLabs Scribe v2 Realtime
  over WebSocket (`stt.engine=elevenlabs`, needs the `ELEVENLABS_API_KEY` secret and
  network; always compiled, no feature flag; see `plans/ElevenLabsSttPlan.md`). The
  engine is **runtime-swappable** from the config-page **Voice** page (`/voice#stt`) with no
  restart (like the LLM/VAD/weather controls); `stt.engine` is just the boot seed. The
  Core's VAD stays the end-of-speech authority for all three.

```bash
# Mac Mini Anamanti Core (the "brain"). Runs on the Mac, not the device.
# Toolchain: pinned to rustc 1.92 via the repo-root `rust-toolchain.toml` (the embedded
# HelixDB stack — foyer/crc-fast/roaring — requires >= 1.91). rustup auto-installs it on
# first build; `rustup default 1.88` alone will fail dependency resolution.
cargo test  --manifest-path anamanti-core/Cargo.toml           # unit + pipeline integration tests
cargo clippy --manifest-path anamanti-core/Cargo.toml --all-targets -- -D warnings
cargo run   --manifest-path anamanti-core/Cargo.toml --release # advertises _wyoming._tcp, serves turns

# Admin console (loopback config page) web UI: plain static HTML in a Material Design
# layout styled with Tailwind CSS, served by the same loopback HTTP server. Tailwind is
# VENDORED PRECOMPILED — the committed anamanti-core/src/webconfig/panel.css is generated
# from webconfig/tailwind/input.css by the Tailwind CLI, embedded via include_str!, so the
# shipped binary needs no CDN / Node at runtime. After editing any webconfig *.html /
# shell.js or the Tailwind theme, regenerate + commit panel.css (needs the `tailwindcss`
# CLI on PATH, e.g. `brew install tailwindcss`):
anamanti-core/scripts/build-webui-css.sh
#
# The admin console is LOOPBACK-ONLY by design: every request's Host must be
# 127.0.0.1/localhost/[::1] on the server's own port (else 403) and a present Origin must
# be that same loopback origin (CSRF / DNS-rebinding guard); every POST must send
# Content-Type: application/json (else 415) — page scripts use shell.js apiJSON/postJSON.
# Routes live in ONE table (anamanti-core/src/webconfig/routes.rs); the checks run once,
# before routing. Binding config_addr to a LAN address therefore does NOT expose it.
#
# Information architecture (plans/CoreWebUIPlan.md, 2026-10-14): ten pages tuned to fit
# one 1920x1080 screen each — Overview `/` (live pipeline with a Local/LAN/Cloud chip
# per stage, displays, latency, "what leaves the house"), Voice `/voice` (STT, VAD,
# Piper voice, transcript fixes), Intelligence `/intelligence` (LLM + keys, System-1,
# web search, read-only system prompt), Personality, Household (address + people),
# Integrations `/integrations` (tiles + per-service drawers: weather, places,
# directions, calendar, Cadora, AppSaid, Spotify, Drive), Music & Devices `/devices`,
# Memory `/memory` (embeddings, the cloud ENTITY-EXTRACTION toggle, facts, graph),
# Conversations `/conversations` (turns + latency waterfall + the matched prompt, joined
# by `turn_id`), Keys & Accounts `/keys`, System `/system` (build, runtime, redacted
# effective boot config). The pre-2026-10-14 page routes (/stt, /tools, /drive,
# /system1, /music, /notifications, /embeddings, /sqlite, /helix, /chatlog, /prompts,
# /about, /pronunciations) 302 to their new homes; every JSON/POST endpoint is
# unchanged. Shared front-end: shell.js (helpers + shell) and ui.js (component kit +
# the LOCALITY map that holds every option's tier and "Sends:" copy — keep it in step
# with the plan's locality matrix). Aggregators: /overview.json, /keys.json,
# /system.json (webconfig/system.rs, which also holds the address→tier classifier).

# End-to-end check of a running Core (real STT → LLM → TTS) with a fake device:
#   say -o /tmp/q.wav --data-format=LEI16@16000 "what is two plus two"
#   anamanti-core/scripts/e2e_wyoming_client.py 127.0.0.1 <bind port> /tmp/q.wav

# Configuration lives in a PER-INSTANCE JSON FILE (see anamanti-core/src/config.rs and
# the committed anamanti-core/anamanti.example.json). Default path: `anamanti.json` in the
# working directory; override with `--config <path>`. A missing convention file → built-in
# defaults; a missing/malformed --config file (or an unknown key — deny_unknown_fields)
# → a hard error at boot. Every key is optional and falls back to its default.
#
#   cargo run --manifest-path anamanti-core/Cargo.toml --release -- --config ./anamanti.json
#
# ONLY SECRETS remain environment variables (never put these in the JSON file). Each
# is a BOOT SEED: it is also settable at runtime from the loopback config page (masked,
# never echoed back) and then persisted to settings_path, so a headless host needs no
# shell env at all.
#   ANTHROPIC_API_KEY / OPENAI_API_KEY  (anthropic/openai backends; OPENAI_API_KEY is
#     needed for the helix GraphRAG embeddings ONLY when graphrag.embed_backend="openai"
#     — the default "local" nomic embedder is offline and needs no key; UI: Intelligence → `/intelligence#llm`, also Keys & Accounts → `/keys`)
#   TAVILY_API_KEY                       (real web search when llm.search_provider=tavily;
#     UI: Intelligence → `/intelligence#search`)
#   MAPBOX_TOKEN (or MAPBOX_ACCESS_TOKEN) (the directions_lookup provider token;
#     UI: Integrations → `/integrations#directions`)
#   APPSAID_APP_TOKEN                    (the send_phone_message sender app token for
#     AppSaid HTML-push; UI: Integrations → `/integrations#appsaid`)
#   VISUALCROSSING_API_KEY               (the weather provider key when weather.provider=
#     visualcrossing, the default; UI: Integrations → `/integrations#weather`. Absent ⇒ weather falls back to
#     keyless Open-Meteo)
#   GOOGLE_PLACES_API_KEY                (the places_lookup provider key — Google Places API
#     New; UI: Integrations → `/integrations#places`. Absent ⇒ the tool is not advertised, no keyless fallback)
#   ANTHROPIC_OAUTH_TOKEN                (Claude subscription token from `claude setup-token`;
#     UI: Intelligence → `/intelligence#llm` when Anthropic auth = subscription)
#   OPENROUTER_API_KEY                   (the System-1 `jev` decision backend on OpenRouter;
#     UI: Intelligence → `/intelligence#system1`. Only needed when system1.backend=jev)
#   HOME_ASSISTANT_TOKEN                 (Home Assistant long-lived access token for the
#     home-command fast path + the home_control tool; UI: Integrations →
#     `/integrations#home_assistant` (masked, also on Keys & Accounts). Absent ⇒ both off)
#   RUST_LOG                             (standard env_logger filter; env-only — read at
#     process start, so it has no config-page control)
#
# The JSON file holds everything else. Key fields (defaults in anamanti.example.json):
#   bind_addr / config_addr / stt_addr / tts_addr / service_name / instance_id
#     (instance_id resolution: the JSON value → the working dir's git BRANCH CODE → the
#     sanitized service_name; must be stable across restarts. config_addr "off"/"none"
#     disables the loopback config+debug pages. A second/test Anamanti Core on the same
#     Mac needs distinct bind_addr + config_addr + service_name; sharing prod's data
#     files means memory_backend="sqlite" — two processes can't share an embedded Helix graph.)
#   db_path / chatlog_path / promptlog_path / helix_path / settings_path / audio_dump_dir
#     (settings_path is the runtime OVERLAY file — see below; "off"/"none" disables persistence)
#   system_prompt / home_address / weather_units / turn_timeout_secs / memory_backend
#     (home_address is a discrete-field object {address1,address2,city,state,postal,country};
#     directions + nearby-places geocode the full combined address, weather + the prompt's
#     "here" grounding use only City + State/Prov. A legacy single-line "home_location"
#     string is still accepted and folds into City. helix|sqlite; helix embeds locally by
#     default — graphrag.embed_backend="openai"
#     is the opt-in cloud path and needs OPENAI_API_KEY, falling back to sqlite FTS if absent)
#   llm.backend (ollama|anthropic|openai|mock), llm.engine (rig|native, default rig;
#     on rig EVERY backend — incl. anthropic subscription auth and openai — gets tools +
#     follow-up history and streams; `native` has neither and logs a loud warning, and
#     the config view's `llm_capabilities` says so),
#     llm.anthropic_auth (apikey|subscription), llm.anthropic_token_cmd (default `ant`),
#     llm.web_search, llm.search_provider (duckduckgo|tavily, default tavily — needs
#     the TAVILY_API_KEY secret; duckduckgo is keyless), and the per-provider
#     sub-blocks llm.ollama{url,model} / llm.anthropic{model,max_tokens} / llm.openai{…}
#   graphrag{embed_backend (local|openai, default local), embed_model_path,
#     embed_tokenizer_dir, embed_model, embed_dims, extract_model, …}. The default
#     local nomic-embed-text-v1.5 embedder needs its ONNX+tokenizer on disk — provision
#     with anamanti-core/scripts/fetch-embed-model.sh (never committed; see /models/).
#     The backend is hot-swappable live from the config page (Memory → Recall settings); the
#     choice persists in helix_path/active_backend. Each backend keeps its OWN store +
#     ingest offset under helix_path/<label>-<dims>/ (e.g. nomic-768, openai-1536), so
#     switching is lossless and never re-embeds an already-built backend — durable SQLite
#     facts + the chat log (source of truth) are untouched.
#   graphrag{…}, speaker{…}, music{…} (music.enabled defaults to true, so the
#     Anamanti Core ducks the music group while it speaks and — with music.autostart,
#     also default on — supervises snapserver/librespot/mpv at boot. With Spotify
#     linked it also polls now-playing every music.nowplaying_refresh_secs (default 5,
#     floor 1) and pushes it to the display's music screen over the persistent
#     role="music" channel — skipped while no display has that channel open; see
#     anamanti.example.json for the full shape)
#   system1{backend,base_url,openrouter_model,min_confidence,intents} — the
#     pluggable System-1 fast-decision engine (plans/system1-fast-decisions.md). backend
#     default "none" (disabled); "laya-serve" (local Laya-Decision sidecar on base_url,
#     `/v1/systemone`) or "jev" (OpenRouter, needs OPENROUTER_API_KEY). Runtime-swappable
#     + persisted; UI: Intelligence → `/intelligence#system1`. Resolves weather/timer before the LLM.
#     (system1.device / system1.model are deprecated: still accepted, ignored + warned.)
#   calendar.subscriptions=[{name,url}], calendar.cache_ttl_secs (read-only web .ics;
#     `webcal://` accepted; enables calendar_lookup; empty → tool not advertised). The
#     feed cache is the shared ToolCache: calendar.cache_ttl_secs wins when set (0 = no
#     cache), else tool_cache.calendar_lookup (default 300s). A total feed failure is
#     never cached — the tool says it couldn't reach the calendar.
#   directions.provider="mapbox" (+ the MAPBOX_TOKEN secret, set via env or the Tools
#     tab) → the directions_lookup tool
#   weather{enabled,provider,refresh_interval_secs} → the weather_lookup tool + ambient
#     push. provider="visualcrossing" (default; + the VISUALCROSSING_API_KEY secret, set
#     via env or Integrations → Weather) or "openmeteo" (keyless). Provider + key are runtime-
#     settable on Integrations → Weather (persisted, rebuilds the tool + retargets the push live);
#     visualcrossing with no key falls back to keyless Open-Meteo
#   places{enabled,provider} → the places_lookup tool (Google Places API New) + the
#     anamanti-place frame + the display's place card. provider="google" (default/only) +
#     the GOOGLE_PLACES_API_KEY secret, set via env or Integrations → Places (runtime-settable,
#     rebuilds the tool live). No keyless fallback — no key ⇒ the tool is not advertised
#   drive{client_id,client_secret,folder_ids,scope} (Google Drive photo slideshow OAuth
#     CLIENT creds; the refresh token is minted by config-page consent, never seeded)
#   spotify{client_id,client_secret,refresh_token,device_name,redirect_url} (spotify_control
#     tool; Premium; easiest setup is config page → Integrations → Spotify → "Connect Spotify". redirect_url
#     is the OAuth callback the consent flow binds + registers, default
#     http://127.0.0.1:8888/callback)
#   cadora{base_url,link_token} (shopping_list_add tool → the shared Cadora household
#     shopping list; NextHaul + Cadora share one Supabase backend; base_url default
#     https://cadora-server.fly.dev. Link: mint a 6-digit code in NextHaul → Settings →
#     Voice & Integrations, enter it on the config page → Integrations → "Shopping list
#     (Cadora)"; the Anamanti Core redeems it for a vl_ token, stored 0600, never seeded.
#     A pasted vl_ token is also accepted)
#   appsaid{worker_url,recipients{name:user_key},default_recipient} (send_phone_message
#     tool → push a message / Google Maps link to a family member's phone via AppSaid.
#     The sender app token is a SECRET (env APPSAID_APP_TOKEN or config page → Household
#     tab, stored 0600, NEVER in the JSON). The tool is advertised only when worker_url +
#     the app token + at least one recipient are all present. RECIPIENTS ARE UNIFIED ON
#     THE HOUSEHOLD: each person lives in the household roster with an `appsaid` address
#     (their user_key) alongside emails/phones — settable on the config page → Household
#     tab, or linked from the device Settings → "Link a phone" pairing flow (claims a
#     4-char code → fills in the address; auto-adds the person if new). The roster is
#     DERIVED from members with an appsaid address; the legacy `appsaid.recipients` config
#     map is folded into the household at boot as a one-time seed.)
#   home_assistant{base_url,default_room,exact_only,dashboard_path} (plans/HomeAssistantPlan.md)
#     → the pre-System-1 home-command fast path + the home_control tool, both off unless
#     base_url + the HOME_ASSISTANT_TOKEN secret are set. default_room = the room a bare
#     "turn off the lights" means when the display doesn't report one (a device sends
#     `room` on audio-start); exact_only (default ["fireplace"]) = devices never swept up
#     by room-wide/"all" commands. The device catalog is cached (pre-warmed at boot, 60 s
#     stale-while-refresh). base_url + default_room (and the token) are runtime-settable on
#     Integrations → Home Assistant (with a read-only "Test connection") and persisted to
#     settings_path; the shared hub is reconfigured live, no restart.
#   tool_cache{"<tool>": <seconds>, …} — per-tool response-cache TTLs, a flat map keyed by
#     tool name, overlaid on built-in defaults; `0` disables a tool's cache. Generic
#     (crate::cache::ToolCache) but only read-only tools are wired: weather_lookup defaults
#     to 3600s (60 min). Read once at boot (not runtime-settable). Mutating tools uncached.
#
# home_address/weather_units, drive, spotify, cadora, appsaid, the tts_voice, and the llm engine/
# backend/model/web_search/search_provider fields only SEED the live settings at boot:
# they are then editable from the config page and persisted to settings_path
# (anamanti_settings.json), and a PERSISTED value wins over the JSON seed at the next boot.
# Provider API keys must never appear in either JSON file — they stay in the environment.
```
- **Do not bump the Android toolchain past AGP 8 / Gradle 8.** The bundled
  cargokit plugin (`anamanti-display/rust_builder/cargokit`) uses the legacy AGP variant API and
  `project.exec`, which Gradle 9 / AGP 9 removed. Pinned in
  `android/settings.gradle.kts` (AGP 8.7.3, Kotlin 2.1.0) and the Gradle wrapper
  (**8.14.3** — bumped 2026-10-07 from 8.11.1: Flutter stable moved its minimum to
  Gradle 8.14.0, and CI's `release.yml` floats on `channel: stable`. Still Gradle 8.x,
  so the legacy cargokit APIs removed in Gradle 9 are intact; AGP stays on 8.7.3). Do
  not cross into Gradle 9 / AGP 9 — revisit only when cargokit ships AGP-9 support.
  NDK: `28.2.13676358`.

### Build output goes on the external drive (disk is tight)

The main volume runs near-full (often <1 GiB free), which is not enough for Rust
target dirs, a release APK, and Gradle caches. **Route all build output to the
external drive** at `/Volumes/External/DeveloperSupport`, which has plenty of
space. Set these for every Cargo / Flutter / Gradle build in this repo:

```bash
# Host-side Cargo builds/tests (both anamanti-display/rust host tests and Anamanti Core):
export CARGO_TARGET_DIR=/Volumes/External/DeveloperSupport/ambient-build/cargo-target

# Gradle caches + a scratch TMPDIR for the APK build:
export GRADLE_USER_HOME=/Volumes/External/DeveloperSupport/mac-caches/gradle
export TMPDIR=/Volumes/External/DeveloperSupport/ambient-build/tmp

# Flutter/cargokit write to ./build — symlink it onto the external drive:
ln -sfn /Volumes/External/DeveloperSupport/ambient-display-build/build build
```

- `/Volumes/External` root is not user-writable; use the `DeveloperSupport/`
  subtree (owned by the user). Create dirs there as needed.
- `~/.cargo/registry` (cache + src) is a re-downloadable global cache — safe to
  clear to reclaim space; Cargo refetches on the next build.
- Do **not** commit the `build` symlink (it's git-ignored) or these paths — they
  are machine-local.

### Deploying / updating the local production Anamanti Core

"Local production" is a **copied release binary** at
`/Volumes/External/DeveloperSupport/Anamanti Core/anamanti-core`,
run **outside any git checkout**. Its identity, data paths, and ports (10700 /
config 8730) come from `anamanti.json` in that folder (`instance_id="Paul Family"`
etc.; `chmod 0600` it — it may hold OAuth client secrets). Only the provider API
keys/tokens still come from `~/.zshenv` (they're secrets). To push the current
branch's Anamanti Core to production, follow this runbook exactly (it does **not**
touch any test copy running from a worktree):

```bash
export CARGO_TARGET_DIR=/Volumes/External/DeveloperSupport/ambient-build/cargo-target
PROD="/Volumes/External/DeveloperSupport/Anamanti Core"

# 1. Build the release binary from the branch you want to ship.
#    VAD: `vad-silero` is a DEFAULT feature (2026-09-30), so this build compiles in
#    onnxruntime and the Core boots on Silero. That REQUIRES the Silero v4 model in the
#    prod folder — fetch it once (idempotent; skips if present):
#      anamanti-core/scripts/fetch-vad-model.sh "$PROD/models"      # silero_vad.onnx (v4)
#    A `silero` boot with no model is a HARD ERROR. To ship the old energy-only Core
#    instead, build `--no-default-features` (re-add any features you want) and set
#    "vad": { "engine": "energy" } in "$PROD/anamanti.json".
#    Default STT dials the external wyoming-faster-whisper server. To ship the
#    in-process whisper.cpp engine instead (no Python STT server), add the feature
#    and pre-fetch the models (see plans/python-to-rust-whisper.md):
#      anamanti-core/scripts/fetch-whisper-models.sh "$PROD/models"   # base + small
#      cargo build --release --features stt-whisper-metal --manifest-path anamanti-core/Cargo.toml
#    then set "stt": { "engine": "whisper-rs", "model": "base", "model_dir": "models" }
#    in "$PROD/anamanti.json". (Metal = Apple-GPU accel; plain stt-whisper-local = CPU.)
anamanti-core/scripts/fetch-vad-model.sh "$PROD/models"
cargo build --release --manifest-path anamanti-core/Cargo.toml

# 2. Stop the running production copy — the process LISTENING on :10700. Do NOT
#    `pkill -f "$PROD/anamanti-core"`: production is launched as
#    `./anamanti-core` (relative argv, see step 4), so its command line never
#    contains that absolute path and the pkill silently matches nothing. Target the
#    port instead — a worktree/test copy binds 10701+, so :10700 is production alone.
PROD_PID="$(lsof -nP -iTCP:10700 -sTCP:LISTEN -t)"    # empty if it isn't running
if [ -n "$PROD_PID" ]; then
  kill $PROD_PID
  # Wait for the socket to free up before rebinding :10700; escalate if it lingers.
  for _ in $(seq 1 20); do
    lsof -nP -iTCP:10700 -sTCP:LISTEN -t >/dev/null 2>&1 || break
    sleep 0.3
  done
  lsof -nP -iTCP:10700 -sTCP:LISTEN -t >/dev/null 2>&1 && kill -9 $PROD_PID
fi
while lsof -nP -iTCP:10700 -sTCP:LISTEN -t >/dev/null 2>&1; do sleep 0.3; done

# 3. Copy the freshly built binary into the production folder (overwrites).
cp "$CARGO_TARGET_DIR/release/anamanti-core" "$PROD/anamanti-core"
chmod +x "$PROD/anamanti-core"

# 4. Restart from the production folder, sourcing ~/.zshenv for the provider API
#    keys (secrets). Identity/ports/paths come from "$PROD/anamanti.json" (cwd is $PROD,
#    so its convention anamanti.json is picked up). Detached, logging to the folder.
( source ~/.zshenv 2>/dev/null; cd "$PROD"; \
  ./anamanti-core > "$PROD/anamanti-core.log" 2>&1 & )

# 5. Verify: bound on :10700 and advertising instance_id=`Paul Family`.
sleep 2; tail -5 "$PROD/anamanti-core.log"
```

Notes:
- **Only production is stopped.** Step 2 kills whatever process is *listening on
  :10700*, which is production by definition — a worktree/test Anamanti Core binds
  10701+, so it keeps running. (Do not match on the binary path: production runs as
  the relative `./anamanti-core`, so a `pkill -f "$PROD/anamanti-core"`
  matches nothing and leaves the old copy up — the bug this runbook step avoids.)
- The binary is machine-local; do **not** commit it or the `Anamanti Core/`
  folder.
- Persistence: the process is detached but does **not** survive a reboot — running
  it under launchd is the open item in `TODO.md §4`.
- **⚠️ One-time migration for an existing (pre-rename) production install.**
  **Status: DONE for the local production install (2026-09-29)** — the folder, data
  files, config paths, and identity were migrated and the running Core + the Echo Show
  are on the new names (kept here as reference for any other pre-rename install). The
  Anamanti rename changed the on-disk names the binary looks for, so migrate the
  existing prod install **once** or it will start with fresh, empty state:
  ```bash
  cd /Volumes/External/DeveloperSupport
  mv "Ambient Orchestrator" "Anamanti Core"          # folder (skip if already renamed)
  cd "Anamanti Core"
  mv ambient.json          anamanti.json             # per-instance config
  mv ambient_memory.sqlite anamanti_memory.sqlite    # SQLite memory (store of record)
  mv ambient_settings.json anamanti_settings.json    # persisted settings + keys (0600)
  mv ambient_chatlog.jsonl anamanti_chatlog.jsonl    # GraphRAG chat log
  mv ambient_promptlog.jsonl anamanti_promptlog.jsonl 2>/dev/null || true
  mv ambient_helix         anamanti_helix            # embedded HelixDB store
  # also rename the sidecar files if present: anamanti_chatlog.jsonl.offset / -* etc.
  ```
  Then in `~/.zshenv` rename the provider/secret env vars from `AMBIENT_*` to
  `ANAMANTI_*` (e.g. `AMBIENT_INSTANCE_ID`→`ANAMANTI_INSTANCE_ID`). (Spotify creds are
  **not** environment variables — they live in the `spotify` block of `anamanti.json`
  and in `anamanti_settings.json`, migrated by the settings-file rename above.)
  Skipping any of these silently loses the corresponding state (memory/settings)
  because the renamed binary creates new empty files at the new default paths.
  For a **full identity rename** (optional — the `instance_id` is otherwise an opaque
  stable key), also set `service_name`/`instance_id` in `anamanti.json` to
  `"Anamanti Core"`/`"anamanti-core"` **and** re-pin the device to match: the Echo Show
  stores its pinned Core id as `orchestratorKey` in `/data/data/com.anamanti.anamanti_display/files/settings.json`
  (a **strict** pin — it stays offline rather than switching Macs), so update that value
  (or set it to Auto) or the device will stop connecting. This is what the 2026-09-29
  local-prod migration did.
- **Redeploy Core and Display together.** The Wyoming frame names (`anamanti-*`), the
  mDNS TXT role (`role=core`), the config filename, env vars, and data-file names all
  changed, so a new Core will not interoperate with an old Display build (or vice
  versa) — rebuild and reflash both.

### Deploying / updating Anamanti Display (device OTA) — publish from the Mac

This is how you ship an app update to the Echo Show fleet. Read this whole section
before running anything — there is a **signing-key trap** that will silently break
every device in the field if you get it wrong. Design doc: `plans/UpdaterPlan.md`.

**Two independent OTA paths reach the devices (a release should feed both):**
1. **In-app R2 updater — the baked-in default, the one the fleet actually uses.** The
   app checks `<updateBaseUrl>/latest.json` on boot + every 6 h; `updateBaseUrl`
   defaults to **`https://releases.immediacy.app/anamanti`** (`kDefaultUpdateBaseUrl`
   in `lib/src/settings/app_settings.dart`) — the `anamanti/` subpath of the shared
   **`immediacy-releases`** Cloudflare R2 bucket. Published with
   `anamanti-display/scripts/release-r2.sh`.
2. **GitHub Releases + Obtainium.** CI workflow `.github/workflows/release.yml` fires
   on a pushed `v*` tag and attaches `Anamanti-<version>.apk` to a GitHub Release that
   Obtainium polls. Legacy path, kept alive until the in-app path fully supersedes it.

#### ⚠️ Signing key — the thing that will brick your OTA if ignored

LineageOS enforces **signature consistency** on in-place updates: an OTA installs only
if it is signed with the **same key as the build already on the device**. The fielded
devices are signed with **this build host's `~/.android/debug.keystore`** (default
creds: store/key password `android`, alias `androiddebugkey`; cert SHA-256
`6b56aa286ccca32e311799fa8d5d063b359c10fa327ee1abe09e68afe11d97ec`). `build.gradle.kts`
falls back to that debug key for release builds **when no `ANDROID_*` env / `key.properties`
is present** (`hasReleaseSigning` false). So a plain `flutter build apk --release` on this
Mac produces an OTA-installable APK. **Do NOT introduce a new/standalone release key** (or
sign in CI with a different key) without reflashing every device by hand — the mismatch is
rejected silently as "no update."
Verify the signer before publishing (both tools need a JDK on PATH — use Android Studio's
JBR, e.g. `export JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home"`):

```bash
APKSIGNER="$(ls -1 "$HOME"/Library/Android/sdk/build-tools/*/apksigner | sort -V | tail -1)"
"$APKSIGNER" verify --print-certs <apk> | grep -i 'SHA-256'          # must equal 6b56aa…97ec
keytool -list -v -keystore ~/.android/debug.keystore -storepass android -alias androiddebugkey | grep -i SHA256
```

#### Pre-flight (applies to BOTH paths)

- **Bump the version** in `anamanti-display/pubspec.yaml`: `version: X.Y.Z+N`. The `+N`
  build number is the Android **`versionCode`** and **must increase every release** — it
  is what both updaters compare against the running build (a patch deploy is e.g.
  `1.0.3+4` → `1.0.4+5`). Land it on `main` (PR), then build/publish from that commit.
- **Pin the toolchain.** Build with **Flutter 3.44.1** (Dart 3.12.1 — matches pubspec
  `sdk: ^3.12.1`); it is the hardware-verified version that builds on the locked
  AGP 8.7.3 / Gradle 8.14.3. Newer Flutter stable demands Gradle 9 / AGP 9 and breaks
  cargokit (see the locked decision above). `brew`'s floating `flutter` may have drifted
  — check `flutter --version`.
- **Build output must go to the external drive** (see the section above): export
  `CARGO_TARGET_DIR`, `GRADLE_USER_HOME`, `TMPDIR`, and symlink `anamanti-display/build`.
- **32-bit only.** The Echo Show 8 (crown) is armeabi-v7a — build
  `--target-platform android-arm` (arm64 → `INSTALL_FAILED_NO_MATCHING_ABIS`).
- The full build cross-compiles the Rust engine for armv7 (~3–11 min); run it backgrounded.

#### Path 1 — in-app R2 updater (do this one; it reaches the fleet)

`wrangler` auth is cached on this Mac (`npx wrangler whoami` → R2 write; bucket
`immediacy-releases` exists). The script rebuilds the signed APK, computes its SHA-256,
writes `latest.json`, and uploads **both** to R2 (`--remote` is mandatory — without it
wrangler writes to a local simulator store and nothing publishes).

```bash
cd anamanti-display
export CARGO_TARGET_DIR=/Volumes/External/DeveloperSupport/ambient-build/cargo-target
export GRADLE_USER_HOME=/Volumes/External/DeveloperSupport/mac-caches/gradle
export TMPDIR=/Volumes/External/DeveloperSupport/ambient-build/tmp
ln -sfn /Volumes/External/DeveloperSupport/ambient-display-build/build build
R2_BUCKET=immediacy-releases \
UPDATE_BASE_URL=https://releases.immediacy.app/anamanti \
NOTES="<user-facing release notes — shown in the device update banner>" \
WRANGLER="npx -y wrangler" \
./scripts/release-r2.sh
```

**Stale-UI guard (2026-10-14).** The script deletes `build/app` before building and,
before uploading, compares the new APK's compiled Dart UI (`lib/armeabi-v7a/libapp.so`)
with the currently published APK's — **identical ⇒ it refuses to publish** (nothing is
uploaded). This exists because an incremental build shipped 1.0.18 with 1.0.17's UI:
Gradle's `mergeSelfUpdateReleaseJniLibFolders` kept a stale `libapp.so` while Flutter's
fresh AOT output had changed, and only the Rust `.so` updated. If it trips, `flutter
clean` and rebuild; for a deliberate Rust/native-only release with no Dart change, re-run
with `ALLOW_SAME_DART=1`. When verifying an APK by hand, note that Dart stores strings
containing non-Latin-1 characters (e.g. `…`, `—`) as UTF-16, so a plain `grep -a` won't
find those — check an ASCII-only string, or compare `libapp.so` hashes.

Verify it went live (devices pick it up on their next check, within the `latest.json`
edge-cache TTL — purge/short-TTL it if a device lags):

```bash
curl -fsS "https://releases.immediacy.app/anamanti/latest.json"       # versionCode = new +N
curl -fsSI "https://releases.immediacy.app/anamanti/app-<X.Y.Z>.apk"  # HTTP 200
```

#### Path 2 — GitHub Release for Obtainium (CI is currently NON-FUNCTIONAL for signing)

`release.yml` **builds** fine but **cannot sign**: the repo has **no `ANDROID_*` secrets**,
so the "Decode release keystore" step writes an empty keystore and signing throws
`KeytoolException: Tag number over 30 is not supported`. Until that is fixed, **publish
the GitHub Release from the Mac too** (same signed APK → guaranteed key match):

```bash
cd anamanti-display   # on the released commit, with build env exported as above
flutter build apk --release --flavor selfUpdate --target-platform android-arm
cp build/app/outputs/flutter-apk/app-selfUpdate-release.apk "Anamanti-<X.Y.Z>+<N>.apk"
gh release create v<X.Y.Z> "Anamanti-<X.Y.Z>+<N>.apk" --title "v<X.Y.Z>" --generate-notes
rm "Anamanti-<X.Y.Z>+<N>.apk"
```

**To make CI self-sufficient** (optional, needs owner sign-off because it sets the signing
identity for all future updates): add the **existing debug keystore** as repo secrets so CI
signs with the key the fleet already trusts —
`ANDROID_KEYSTORE_BASE64=$(base64 < ~/.android/debug.keystore)`,
`ANDROID_KEYSTORE_PASSWORD=android`, `ANDROID_KEY_ALIAS=androiddebugkey`,
`ANDROID_KEY_PASSWORD=android` (`gh secret set …`). CI also floats on `channel: stable`
and must stay pinned to Flutter 3.44.1 (see `release.yml`), or it drifts past the locked
Android toolchain again.

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
  transcript + TTS audio. Frame types/constructors/decoders are defined **once**, in
  the shared `anamanti-protocol` crate — never copied into either side's `protocol.rs`.
- **Turn order (fast paths first):** `respond_and_speak` tries, in order, the explicit
  memory command (`parse_command`), the **Home Assistant fast path**
  (`orchestrator/home.rs`), **System-1**, then System-2 (recall + LLM). A new zero-model
  fast path belongs before System-1 and must reject non-matching turns with pure string
  code (no I/O) so ordinary turns pay nothing.
- **Reuse the shared seams (DRY):** a new LLM tool is a `VoiceTool` registered in
  `Tools::build` (architecture.md §8); a new persistent push channel wraps
  `ChannelRegistry` (Core) and `push_channel::run` + `start_channel` (device); a new
  `{ok, message}` control response is read with `ok_message`; a short string argument
  on a `WakeWordEvent` rides the existing `recipe_action` carrier rather than a new FRB
  field; Dart tests build events with `test/support/wake_word_events.dart`; outbound
  HTTP goes through `crate::http` (shared pool, mandatory timeout).

## Working agreements for agents

- Prefer the smallest change that satisfies the current phase in `Plan.MD`.
- When you make or discover a design decision, record it in `Plan.MD` (decision
  table) and reflect structural changes in `architecture.md`.
- Keep the three docs consistent: `README.md` (overview), `architecture.md`
  (design), `Plan.MD` (delivery). If you change behavior, update all three. When the
  change touches the Display's on-screen UI, also update
  [`plans/DisplayUI.md`](./plans/DisplayUI.md) (the UI catalog).
- Do not add a second audio path, a second interop mechanism, or an IP-based
  discovery fallback without confirmation — these violate locked decisions.
- Respect the memory budget: avoid large models, unbounded buffers, or holding
  full audio in memory on the device.

## State machine cheat-sheet (Rust Wyoming client)

`IDLE → TRIGGERED → STREAMING → THINKING → SPEAKING → IDLE`

- IDLE: wake-word scoring only; socket dormant; photo slideshow on screen.
- TRIGGERED: open TCP, send `audio-start`.
- STREAMING: send PCM frames; read `transcript` events; the **Anamanti Core's VAD**
  (neural Silero by default since 2026-09-30; pluggable `SpeechGate`, energy/RMS is the
  opt-in fallback — see `plans/VadSileroPlan.md`) detects end-of-speech and sends
  `audio-stop` to STT (device runs no VAD).
- THINKING: LLM (with persistent memory) streams reply tokens (render live).
- THINKING→SPEAKING: the Anamanti Core segments the LLM stream into sentences and
  synthesizes each with Piper as it forms (streaming TTS), coalesced into one
  device-facing audio stream; the device plays it via `cpal`/`oboe`.
- Barge-in: wake-word scoring keeps running through THINKING/SPEAKING; a wake word
  flushes playback immediately + sends `anamanti-interrupt` (Anamanti Core aborts
  LLM+TTS) + starts a new turn. (AEC deferred — raise the wake-word threshold during
  SPEAKING to suppress self-triggers.)
- Follow-up listen: after **every** reply the Anamanti Core sends `anamanti-listen`
  (before the final `audio-stop`) carrying a `wait_secs` window (10 s after a `?` reply,
  5 s otherwise); after its TTS drains the device reopens the mic and starts a fresh turn
  with **no wake word**, and that turn's prompt is fed recent conversation history. The
  Anamanti Core sizes the follow-up turn's no-speech VAD window to `wait_secs` and
  **sleeps on silence**; the chain continues only while the user keeps talking (optional
  `follow_up.max_chain`, default unlimited). See `plans/Plan.MD` (Follow-up listening) +
  §4 of `architecture.md`.

Full diagram and wire format: [`architecture.md`](./plans/architecture.md) §4.

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

**AEC (echo cancellation) — SOLVED at the device layer (2026-09), not in-app.** A
vendor audio-HAL shim (`libamznaec_shim.so`, `LD_PRELOAD`ed into
`android.hardware.audio.service`) runs SpeexDSP linear AEC against the Echo Show's
sample-aligned DAC loopback, giving ~16 dB talker-preserving cancellation before
`AudioRecord` ever sees the mic — so barge-in over playback now produces clean
transcripts. It is a **device prerequisite**, not part of this build (source +
reversible install: <https://github.com/Brutus-GTC6245/EchoShow8gen1-aec-shim>;
architecture.md §4, TODO.md §2). Live-verified present + loaded on hardware
(2026-09-28). The in-app approaches below were investigated and **not** shipped:
- Platform AEC via the AAudio `VOICE_COMMUNICATION` input preset is reachable and does
  **not** break the wake word (on a release build), but it does **not actually cancel**
  the device's own playback here (measured mic RMS ~0.1 during playback vs ~0.003 idle)
  — the preset attaches the effect with no working render reference.
- A dependency-free software NLMS canceller works in isolation (>20 dB on host tests)
  but is **net-negative** once integrated: this design **flushes playback on barge-in**,
  so there's no simultaneous echo to cancel, and the filter subtracts a phantom echo
  from the user's clean speech, corrupting the transcript.
- **Only pursue if we adopt true full-duplex barge-in** (keep playing while listening,
  cancel echo, VAD-detect the user) with a production AEC (AEC3/speexdsp + double-talk
  detector + residual suppressor), or coordinate the platform audio mode
  (`MODE_IN_COMMUNICATION` + routed output) so the hardware AEC references the render
  stream — which is exactly what the device-side HAL shim above now does. The raised
  wake-word threshold during playback remains the in-app mitigation for self-triggering,
  layered on top of the shim's cancellation.
- **On-device testing MUST use `--release` APKs** — debug Rust makes tract-onnx
  inference ~3.6× slower on the 32-bit device, which starves the wake-word loop and
  masquerades as unrelated audio bugs.

Implementation note: photos have **two selectable backends** (see TODO §3):
(1) the **Google Photos Ambient API** (`ambient_photos.dart`; device-code + QR,
scope `photosambient.mediaitems`) — the ideal path, but **gated behind the Google
Photos Partner Program** (`createDevice` → 403 until accepted); and (2) **Google
Drive** (`drive_photos.dart`; `drive.readonly`) as the interim. **Drive is
Anamanti Core-owned:** the Mac runs the one-time OAuth consent itself
(`anamanti-core/src/drive_consent.rs`, a "Desktop app" client, loopback + PKCE),
driven from the config page **Integrations → Google Drive** drawer (`/integrations#drive` on `ANAMANTI_CONFIG_ADDR`; the legacy `/drive` redirects there).
The Anamanti Core holds the Drive client id/secret + refresh token + folder ids (in
its `0600` `anamanti_settings.json`), and the device **pulls the whole bundle over
Wyoming** (`anamanti-get-drive-token`) and mints Drive access tokens on-device — so
the tablet APK ships **credential-free** for Drive (there is no build-time
`GOOGLE_DRIVE_*` any more; `AppSettings.driveConfigured` is a runtime check). Set the
Mac's Drive client via `ANAMANTI_GOOGLE_DRIVE_CLIENT_ID` / `_SECRET` (optional
`_FOLDER_IDS`). Only the **Ambient** (TV) client is still built into the APK, via
`--dart-define-from-file=google_oauth.json` (gitignored): `GOOGLE_OAUTH_*`. Dead
ends: Photos Library API (no library read since 2025); Drive scopes via
device-code/QR (rejected); the old standalone `tools/google_photo_consent.py` +
adb-push (replaced by the Anamanti Core consent + Wyoming delivery).
