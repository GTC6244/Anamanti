# agents.md

Build guidance for AI coding agents (and humans) working in this repository.
Read this together with [`architecture.md`](./plans/architecture.md) (the design) and
[`Plan.MD`](./plans/Plan.MD) (phases, confirmed decisions, open questions).
Feature-specific plans branch off these — e.g.
[`MusicPlan.md`](./plans/MusicPlan.md) (Spotify playback via the orchestrator).

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
  filters browse results by TXT `role=orchestrator` (so raw Whisper/Piper Wyoming
  servers are never picked) and can **pin a specific orchestrator** by its stable
  TXT `instance_id` (settings screen "Orchestrator" dropdown; strict — stays
  offline rather than switching Macs; `"Auto"` = first responder).
  **Deployment:** the orchestrator is configured by a **per-instance JSON file**
  (`ambient.json` in the working directory by default; `--config <path>` overrides).
  Only provider API keys/tokens remain environment variables (secrets); everything
  else — identity, endpoints, feature toggles — lives in the JSON file. The "local
  production" orchestrator is installed at
  `/Volumes/External/DeveloperSupport/Ambient Orchestrator/` (a copied release
  binary, run outside any git checkout) and pins its identity via `instance_id` in
  that folder's `ambient.json`; its provider keys still come from `~/.zshenv`. Test
  copies run from their git branch/worktree, where `instance_id` is left unset so it
  defaults to the branch code; give each a distinct `bind_addr` + `config_addr` (and
  `service_name`) in its own `ambient.json` to run alongside production.
- **Wake word:** openWakeWord `.onnx` via `tract-onnx`. No custom training in v1.
- **LLM:** pluggable behind a trait (local Ollama/llama.cpp **or** cloud API).
- **TTS:** Piper via Wyoming.
- **Playback:** Rust (`cpal`/`oboe`), symmetric with capture.
- **Barge-in:** wake word stays active during playback; saying it again flushes
  playback immediately and starts a fresh turn, and sends an `ambient-interrupt`
  frame so the orchestrator aborts the in-flight LLM + TTS. **AEC is not shipped**
  (investigated on hardware — see below; self-triggering during loud playback is a
  known, accepted limitation).
- **VAD:** off-device — the **orchestrator** decides end-of-speech (energy VAD;
  faster-whisper has no streaming VAD, so the Mac sends `audio-stop`). The device
  never runs its own VAD.
- **Memory:** persistent **SQLite** on the Mac is the store of record for
  **explicit + inferred** facts (writes + the settings list + voice
  "remember…"/"forget that"). **Retrieval/recall defaults to the embedded HelixDB
  GraphRAG backend** (in-process, no server/Docker) — every completed turn is
  appended to `ambient_chatlog.jsonl` and a background ingester embeds it into the
  graph. GraphRAG needs `OPENAI_API_KEY` (embeddings); if it's absent or init
  fails, recall **falls back to SQLite FTS** (writes are unaffected). Override with
  `memory_backend: "sqlite"` in `ambient.json` for pure FTS recall. The HelixDB
  engine, the rig agent framework, and the ECAPA-TDNN speaker embedder are **always
  compiled in** (no longer feature-gated); speaker ID is selected purely at runtime
  via `speaker.enabled` / `speaker.model_path`.
- **Idle screen:** photo slideshow from a Google Photos/Drive folder; keeps running
  when disconnected. Google Photos (Ambient) links via **on-device OAuth**
  (device-code/QR); **Google Drive links on the orchestrator** (consent on the Mac,
  config page → Photos tab) and the device pulls the token over Wyoming.
- **Resilience:** **auto-reconnect** with backoff via mDNS + a subtle
  disconnected indicator; wake words queue until reconnected.
- **AEC interim:** raise the wake-word confidence **threshold during playback** to
  suppress self-triggers. Real AEC was attempted on hardware and deferred — the
  `VOICE_COMMUNICATION` preset doesn't cancel on this device, and software AEC is
  net-negative for flush-on-wake barge-in (no simultaneous echo to cancel). See the
  risk section below.
- **Settings:** LLM backend, TTS voice, wake word, photo source, and memory
  management are configurable.
- **Proactive notifications (Approach A):** the orchestrator can push **visual**
  notifications to the display *unprompted* (no voice turn) over a **persistent,
  device-dialed** Wyoming channel (`ambient-hello` → `ambient-notify`), separate from
  the per-turn voice socket. The device is still the dialer (reuses mDNS + the
  `instance_id` pin + auto-reconnect); the Mac only pushes down the open socket. Rust
  owns the socket + reconnect (`display/rust/src/wyoming/notify.rs`,
  `start_notify_channel`); Flutter shows a dismissible banner
  (`NotificationController`); the orchestrator keeps a `NotificationService` registry
  (config-page **Notify** tab pushes a test). Visual-only for now — no spoken output,
  no state-machine interaction. Design: architecture.md §4; follow-ups: TODO.md §6a.

If a task seems to require changing one of these, stop and confirm first.

## Repository layout

```
display/       Everything installed on the Android device (the Echo Show). The
               Flutter project root: Dart UI in `lib/`, the Rust engine (audio
               capture/playback, ring buffer, wake word, Wyoming client, mDNS) in
               `rust/`, `android/`, cargokit `rust_builder/`, bundled openWakeWord
               models in `assets/`, and the Flutter `test/` + `integration_test/`.
orchestrator/  Everything that runs on the Mac. Rust orchestrator (crate
               `ambient_orchestrator`) — Wyoming server to the device + Wyoming
               client to Whisper/Piper, pluggable LLM, HelixDB/SQLite memory, mDNS.
plans/         Design + planning docs: architecture.md (design, source of truth),
               Plan.MD (phases + decision table), TODO.md, the *_plan / rollout
               notes, and MusicPlan.md (Spotify playback via the orchestrator).
agents.md      This file (repo root).
CLAUDE.md      Harness entry point; points here (repo root).
README.md      Product overview + setup (repo root).
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

# All Flutter / FRB commands run from the display/ project root:
cd display

# Generate the Dart/JNI bindings from Rust signatures (reads display/flutter_rust_bridge.yaml)
flutter_rust_bridge_codegen generate

# Build a device APK (cargokit cross-compiles the Rust engine into it).
# Echo Show 8 (crown) is 32-bit armeabi-v7a — use android-arm, NOT android-arm64
# (arm64 fails with INSTALL_FAILED_NO_MATCHING_ABIS on this device).
flutter build apk --release --target-platform android-arm

# Run the app on the Echo Show (LineageOS) via adb (path relative to display/)
adb install build/app/outputs/flutter-apk/app-release.apk   # or: flutter run -d <echo-show-device>
```

- Requires: Flutter SDK, Android SDK + NDK, Rust toolchain, `adb`.
- Mac side: a Wyoming STT server (Whisper/CoreML) and Piper TTS on the LAN.

```bash
# Mac Mini orchestrator (the "brain"). Runs on the Mac, not the device.
cargo test  --manifest-path orchestrator/Cargo.toml           # unit + pipeline integration tests
cargo clippy --manifest-path orchestrator/Cargo.toml --all-targets -- -D warnings
cargo run   --manifest-path orchestrator/Cargo.toml --release # advertises _wyoming._tcp, serves turns

# Configuration lives in a PER-INSTANCE JSON FILE (see orchestrator/src/config.rs and
# the committed orchestrator/ambient.example.json). Default path: `ambient.json` in the
# working directory; override with `--config <path>`. A missing convention file → built-in
# defaults; a missing/malformed --config file (or an unknown key — deny_unknown_fields)
# → a hard error at boot. Every key is optional and falls back to its default.
#
#   cargo run --manifest-path orchestrator/Cargo.toml --release -- --config ./ambient.json
#
# ONLY SECRETS remain environment variables (never put these in the JSON file). Each
# is a BOOT SEED: it is also settable at runtime from the loopback config page (masked,
# never echoed back) and then persisted to settings_path, so a headless host needs no
# shell env at all.
#   ANTHROPIC_API_KEY / OPENAI_API_KEY  (anthropic/openai backends and, for
#     OPENAI_API_KEY, the helix GraphRAG embeddings; UI: Config tab)
#   TAVILY_API_KEY                       (real web search when llm.search_provider=tavily;
#     UI: Config tab)
#   MAPBOX_TOKEN (or MAPBOX_ACCESS_TOKEN) (the directions_lookup provider token;
#     UI: Tools tab → /tools)
#   ANTHROPIC_OAUTH_TOKEN                (Claude subscription token from `claude setup-token`;
#     UI: Config tab when Anthropic auth = subscription)
#   RUST_LOG                             (standard env_logger filter; env-only — read at
#     process start, so it has no config-page control)
#
# The JSON file holds everything else. Key fields (defaults in ambient.example.json):
#   bind_addr / config_addr / stt_addr / tts_addr / service_name / instance_id
#     (instance_id resolution: the JSON value → the working dir's git BRANCH CODE → the
#     sanitized service_name; must be stable across restarts. config_addr "off"/"none"
#     disables the loopback config+debug pages. A second/test orchestrator on the same
#     Mac needs distinct bind_addr + config_addr + service_name; sharing prod's data
#     files means memory_backend="sqlite" — two processes can't share an embedded Helix graph.)
#   db_path / chatlog_path / promptlog_path / helix_path / settings_path / audio_dump_dir
#     (settings_path is the runtime OVERLAY file — see below; "off"/"none" disables persistence)
#   system_prompt / home_location / weather_units / turn_timeout_secs / memory_backend
#     (helix|sqlite; helix needs OPENAI_API_KEY and falls back to sqlite FTS if absent)
#   llm.backend (ollama|anthropic|openai|mock), llm.engine (rig|native, default rig),
#     llm.anthropic_auth (apikey|subscription), llm.anthropic_token_cmd (default `ant`),
#     llm.web_search, llm.search_provider (duckduckgo|tavily, default tavily — needs
#     the TAVILY_API_KEY secret; duckduckgo is keyless), and the per-provider
#     sub-blocks llm.ollama{url,model} / llm.anthropic{model,max_tokens} / llm.openai{…}
#   graphrag{…}, speaker{…}, music{…} (music.enabled defaults to true, so the
#     orchestrator ducks the music group while it speaks and — with music.autostart,
#     also default on — supervises snapserver/librespot/mpv at boot; see
#     ambient.example.json for the full shape)
#   calendar.subscriptions=[{name,url}], calendar.cache_ttl_secs (read-only web .ics;
#     `webcal://` accepted; enables calendar_lookup; empty → tool not advertised)
#   directions.provider="mapbox" (+ the MAPBOX_TOKEN secret, set via env or the Tools
#     tab) → the directions_lookup tool
#   drive{client_id,client_secret,folder_ids,scope} (Google Drive photo slideshow OAuth
#     CLIENT creds; the refresh token is minted by config-page consent, never seeded)
#   spotify{client_id,client_secret,refresh_token,device_name} (spotify_control tool;
#     Premium; easiest setup is config page → Music tab → "Connect Spotify")
#
# home_location/weather_units, drive, spotify, the tts_voice, and the llm engine/
# backend/model/web_search/search_provider fields only SEED the live settings at boot:
# they are then editable from the config page and persisted to settings_path
# (ambient_settings.json), and a PERSISTED value wins over the JSON seed at the next boot.
# Provider API keys must never appear in either JSON file — they stay in the environment.
```
- **Do not bump the Android toolchain past AGP 8 / Gradle 8.** The bundled
  cargokit plugin (`display/rust_builder/cargokit`) uses the legacy AGP variant API and
  `project.exec`, which Gradle 9 / AGP 9 removed. Pinned in
  `android/settings.gradle.kts` (AGP 8.7.3, Kotlin 2.1.0) and the Gradle wrapper
  (8.11.1). Revisit only when cargokit ships AGP-9 support. NDK: `28.2.13676358`.

### Build output goes on the external drive (disk is tight)

The main volume runs near-full (often <1 GiB free), which is not enough for Rust
target dirs, a release APK, and Gradle caches. **Route all build output to the
external drive** at `/Volumes/External/DeveloperSupport`, which has plenty of
space. Set these for every Cargo / Flutter / Gradle build in this repo:

```bash
# Host-side Cargo builds/tests (both display/rust host tests and orchestrator):
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

### Deploying / updating the local production orchestrator

"Local production" is a **copied release binary** at
`/Volumes/External/DeveloperSupport/Ambient Orchestrator/ambient-orchestrator`,
run **outside any git checkout**. Its identity, data paths, and ports (10700 /
config 8730) come from `ambient.json` in that folder (`instance_id="Paul Family"`
etc.; `chmod 0600` it — it may hold OAuth client secrets). Only the provider API
keys/tokens still come from `~/.zshenv` (they're secrets). To push the current
branch's orchestrator to production, follow this runbook exactly (it does **not**
touch any test copy running from a worktree):

```bash
export CARGO_TARGET_DIR=/Volumes/External/DeveloperSupport/ambient-build/cargo-target
PROD="/Volumes/External/DeveloperSupport/Ambient Orchestrator"

# 1. Build the release binary from the branch you want to ship.
cargo build --release --manifest-path orchestrator/Cargo.toml

# 2. Stop the running production copy. Match on the PROD binary path so ONLY
#    production is killed — never a test copy (which runs from cargo-target/…).
pkill -f "$PROD/ambient-orchestrator"    # no-op if it isn't running
# Wait for the socket to free up before rebinding :10700.
while lsof -nP -iTCP:10700 -sTCP:LISTEN -t >/dev/null 2>&1; do sleep 0.3; done

# 3. Copy the freshly built binary into the production folder (overwrites).
cp "$CARGO_TARGET_DIR/release/ambient-orchestrator" "$PROD/ambient-orchestrator"
chmod +x "$PROD/ambient-orchestrator"

# 4. Restart from the production folder, sourcing ~/.zshenv for the provider API
#    keys (secrets). Identity/ports/paths come from "$PROD/ambient.json" (cwd is $PROD,
#    so its convention ambient.json is picked up). Detached, logging to the folder.
( source ~/.zshenv 2>/dev/null; cd "$PROD"; \
  ./ambient-orchestrator > "$PROD/orchestrator.log" 2>&1 & )

# 5. Verify: bound on :10700 and advertising instance_id=`Paul Family`.
sleep 2; tail -5 "$PROD/orchestrator.log"
```

Notes:
- **Only production is stopped.** `pkill -f "$PROD/ambient-orchestrator"` matches the
  absolute production path, so a worktree/test orchestrator (launched from
  `…/cargo-target/release/ambient-orchestrator` on ports 10701+) keeps running.
- The binary is machine-local; do **not** commit it or the `Ambient Orchestrator/`
  folder.
- Persistence: the process is detached but does **not** survive a reboot — running
  it under launchd is the open item in `TODO.md §4`.

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
- STREAMING: send PCM frames; read `transcript` events; the **orchestrator's energy
  VAD** detects end-of-speech and sends `audio-stop` to STT (device runs no VAD).
- THINKING: LLM (with persistent memory) streams reply tokens (render live).
- THINKING→SPEAKING: the orchestrator segments the LLM stream into sentences and
  synthesizes each with Piper as it forms (streaming TTS), coalesced into one
  device-facing audio stream; the device plays it via `cpal`/`oboe`.
- Barge-in: wake-word scoring keeps running through THINKING/SPEAKING; a wake word
  flushes playback immediately + sends `ambient-interrupt` (orchestrator aborts
  LLM+TTS) + starts a new turn. (AEC deferred — raise the wake-word threshold during
  SPEAKING to suppress self-triggers.)

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

**AEC (echo cancellation) — investigated on hardware, not shipped.** Findings:
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
  stream. For now the shipping mitigation is the raised wake-word threshold during
  playback, and self-triggering over loud playback is an accepted limitation.
- **On-device testing MUST use `--release` APKs** — debug Rust makes tract-onnx
  inference ~3.6× slower on the 32-bit device, which starves the wake-word loop and
  masquerades as unrelated audio bugs.

Implementation note: photos have **two selectable backends** (see TODO §3):
(1) the **Google Photos Ambient API** (`ambient_photos.dart`; device-code + QR,
scope `photosambient.mediaitems`) — the ideal path, but **gated behind the Google
Photos Partner Program** (`createDevice` → 403 until accepted); and (2) **Google
Drive** (`drive_photos.dart`; `drive.readonly`) as the interim. **Drive is
orchestrator-owned:** the Mac runs the one-time OAuth consent itself
(`orchestrator/src/drive_consent.rs`, a "Desktop app" client, loopback + PKCE),
driven from the config page **Photos tab** (`/drive` on `AMBIENT_CONFIG_ADDR`).
The orchestrator holds the Drive client id/secret + refresh token + folder ids (in
its `0600` `ambient_settings.json`), and the device **pulls the whole bundle over
Wyoming** (`ambient-get-drive-token`) and mints Drive access tokens on-device — so
the tablet APK ships **credential-free** for Drive (there is no build-time
`GOOGLE_DRIVE_*` any more; `AppSettings.driveConfigured` is a runtime check). Set the
Mac's Drive client via `AMBIENT_GOOGLE_DRIVE_CLIENT_ID` / `_SECRET` (optional
`_FOLDER_IDS`). Only the **Ambient** (TV) client is still built into the APK, via
`--dart-define-from-file=google_oauth.json` (gitignored): `GOOGLE_OAUTH_*`. Dead
ends: Photos Library API (no library read since 2025); Drive scopes via
device-code/QR (rejected); the old standalone `tools/google_photo_consent.py` +
adb-push (replaced by the orchestrator consent + Wyoming delivery).
