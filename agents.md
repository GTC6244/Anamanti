# agents.md

Build guidance for AI coding agents (and humans) working in this repository.
Read this together with [`architecture.md`](./plans/architecture.md) (the design) and
[`Plan.MD`](./plans/Plan.MD) (phases, confirmed decisions, open questions).

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
  **Deployment:** the "local production" orchestrator is installed at
  `/Volumes/External/DeveloperSupport/Ambient Orchestrator/` (a copied release
  binary, run outside any git checkout) and pins its identity via
  `AMBIENT_INSTANCE_ID` in `~/.zshenv`. Test copies run from their git
  branch/worktree, where `AMBIENT_INSTANCE_ID` is left unset so it defaults to the
  branch code; give each a distinct `AMBIENT_BIND_ADDR` + `AMBIENT_CONFIG_ADDR`
  (and `AMBIENT_SERVICE_NAME`) to run alongside production.
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
  `AMBIENT_MEMORY_BACKEND=sqlite` for pure FTS recall. The HelixDB engine and the
  rig agent framework are **always compiled in** (no longer feature-gated).
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
               Plan.MD (phases + decision table), TODO.md, and the *_plan / rollout
               notes.
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

# Backend selection + endpoints are env-driven (see orchestrator/src/config.rs), e.g.:
#   AMBIENT_LLM_BACKEND=ollama|anthropic|openai|mock (default ollama; anthropic needs
#     ANTHROPIC_API_KEY, openai needs OPENAI_API_KEY)
#   AMBIENT_ANTHROPIC_AUTH=apikey|subscription (default apikey; subscription uses a
#     Claude OAuth token from ANTHROPIC_OAUTH_TOKEN (`claude setup-token`) or
#     AMBIENT_ANTHROPIC_TOKEN_CMD)
#   AMBIENT_ANTHROPIC_MODEL / AMBIENT_OPENAI_MODEL   (initial pinned model; the
#     settings screen / config page pick a specific model from a last-12-months list)
#   AMBIENT_STT_ADDR=127.0.0.1:10300  AMBIENT_TTS_ADDR=127.0.0.1:10200
#   AMBIENT_BIND_ADDR=0.0.0.0:10700   AMBIENT_TTS_VOICE=en_US-amy-medium
#   AMBIENT_SERVICE_NAME="Ambient Orchestrator"  (mDNS instance name / dropdown label)
#   AMBIENT_INSTANCE_ID=<stable key>  (mDNS TXT instance_id the device pins to; must
#     be stable across restarts. Resolution: this env → the working dir's git BRANCH
#     CODE (so a copy run from a test branch/worktree names itself by its branch) →
#     the sanitized service name. The "local production" install (see below) sets it
#     in ~/.zshenv. A second/test orchestrator on the same Mac needs distinct
#     BIND_ADDR + CONFIG_ADDR (and a distinct SERVICE_NAME to avoid an mDNS name
#     clash); sharing prod's data files means AMBIENT_MEMORY_BACKEND=sqlite — two
#     processes can't share an embedded Helix graph)
#   AMBIENT_LLM_ENGINE=rig|native  (default rig — enables tool calling incl. the
#     internet_search tool; set `native` for the hand-rolled HTTP backends / no tools)
#   AMBIENT_WEB_SEARCH=on|off  (default on with the rig engine) plus
#     AMBIENT_SEARCH_PROVIDER=duckduckgo|tavily and TAVILY_API_KEY for real web search
#   AMBIENT_CALENDARS=Name=URL;Name=URL  (read-only web iCalendar/.ics subscriptions;
#     `webcal://` is accepted. Enables the rig-engine `calendar_lookup` tool — the LLM
#     answers schedule/meeting/"who am I seeing" questions live from these feeds. Unset
#     → the tool isn't advertised. Bare URLs are auto-named "Calendar N".)
#   AMBIENT_CALENDAR_CACHE_TTL=300  (seconds a fetched .ics feed is cached before the
#     next lookup re-fetches; default 300. The fetch dominates cost — parsing a ~1 MB
#     feed is ~20 ms — so a few minutes of staleness makes back-to-back questions
#     instant. `0` disables caching / always re-fetches.)
#   AMBIENT_HOME_LOCATION="Austin, Texas"  AMBIENT_WEATHER_UNITS=imperial|metric
#     (grounds "here" for weather/location questions in the system prompt; also the
#     default origin + distance units for the directions tool below). These now only
#     *seed* the canonical Household record at first boot: location + units (and the
#     people roster) are persisted in ambient_settings.json and edited live from the
#     config page **Household tab** (`/household`), so a persisted value wins over the
#     env at the next boot. The prompt reads location + roster from the per-turn
#     settings snapshot, so edits apply without a restart. The directions tool's
#     default origin also tracks this live household location (a shared
#     `LiveHomeLocation` handle), so editing it re-homes routing with no restart.
#   AMBIENT_DIRECTIONS_PROVIDER=mapbox  MAPBOX_TOKEN=<token>  (enables the rig-engine
#     `directions_lookup` tool — real distance, travel time, and LIVE TRAFFIC between
#     two places via Mapbox Geocoding v6 + Directions v5 `driving-traffic`. Unset token
#     → the tool isn't advertised. Origin defaults to AMBIENT_HOME_LOCATION; voice-only
#     in v1. Only `mapbox` is supported today; MAPBOX_ACCESS_TOKEN is also accepted.)
#   AMBIENT_MEMORY_BACKEND=helix|sqlite (default helix/GraphRAG; needs OPENAI_API_KEY
#     for embeddings and falls back to sqlite FTS if absent. sqlite = pure FTS recall)
#   AMBIENT_CONFIG_ADDR=127.0.0.1:8730 (loopback config + debug pages: /household,
#     /chatlog, /prompts, /sqlite, /helix, /drive — no auth; `off` disables)
#   AMBIENT_GOOGLE_DRIVE_CLIENT_ID / AMBIENT_GOOGLE_DRIVE_CLIENT_SECRET (the Drive
#     "Desktop app" OAuth client the orchestrator uses to run consent from the config
#     page Photos tab; the device pulls the resulting token over Wyoming). Optional
#     AMBIENT_GOOGLE_DRIVE_FOLDER_IDS=id1,id2 seeds the slideshow folders.
#   AMBIENT_TTS_VOICES_DIR=<piper model dir>  (when Piper is co-located: the
#     settings voice dropdown then lists only the `<name>.onnx` voices installed
#     there; unset → the dropdown shows Piper's full advertised catalog)
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
run **outside any git checkout** so its identity comes from `~/.zshenv`
(`AMBIENT_INSTANCE_ID="Paul Family"`, the shared data paths, provider keys) on the
default ports (10700 / config 8730). To push the current branch's orchestrator to
production, follow this runbook exactly (it does **not** touch any test copy running
from a worktree):

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

# 4. Restart from the production folder, sourcing ~/.zshenv so it picks up
#    AMBIENT_INSTANCE_ID etc. Detached, logging to the folder.
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
