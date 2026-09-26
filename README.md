# Anamanti

An always-on, ambient voice assistant that turns a jailbroken **Echo Show 8**
(running LineageOS) into a private smart display, backed by an **M4 Mac Mini**
that does the heavy lifting (speech-to-text, an LLM brain, and text-to-speech).

Say a wake word → the display streams your voice to the Mac → your words appear
live on screen → the assistant thinks, answers on screen token-by-token, and
speaks the reply back through the Echo Show's speakers.

> **Naming.** The project is **Anamanti** — from the Irish *anam an tí*, roughly
> "soul of the home" (pronounced **"AN-um un TEE"**). Its two halves:
> **Anamanti Core** is the Mac-side brain (STT ↔ LLM + memory ↔ TTS; the crate
> `anamanti_core`, binary `anamanti-core`), and **Anamanti Display** is the app
> that runs on the Echo Show (package `com.anamanti.anamanti_display`).

---

## What it does

- 🎙️ **On-device wake word** — fully offline detection via an openWakeWord
  `.onnx` model, so no audio leaves the device until you summon it.
- 🗣️ **Streaming STT** — audio streams over the [Wyoming Protocol](https://github.com/rhasspy/wyoming)
  to a Whisper/CoreML server on the Mac; transcripts render as you speak.
- 🧠 **Pluggable LLM brain** — swap between a local model (Ollama / llama.cpp) and
  a cloud API (Claude / OpenAI) behind one interface.
- 🔊 **Streaming spoken replies** — Piper (Wyoming TTS) synthesizes the answer
  sentence-by-sentence *as the LLM generates it*, so the Echo Show starts speaking
  after the first sentence (~2 s on-device) instead of waiting for the whole reply.
- 💬 **Barge-in + memory** — say the wake word again mid-reply to interrupt: playback
  stops instantly and a fresh turn begins (the Anamanti Core also aborts the in-flight
  LLM + TTS via an `anamanti-interrupt` frame). The assistant remembers
  facts/preferences across sessions (stored on the Mac).
- 📺 **Anamanti Display** — a landscape Flutter UI tuned for the 8-inch screen,
  with an idle photo slideshow from a Google Photos/Drive folder.
- 🔔 **Proactive notifications** — the Mac can push a visual notification to the
  display *without* you asking (a reminder, an alert), over a persistent connection
  the device holds open to its pinned Anamanti Core. Visual-only today (no spoken
  output); send a test one from the config page's **Notify** tab.

## Architecture at a glance

```
Echo Show 8 (LineageOS)                     M4 Mac Mini
┌──────────────────────┐                    ┌────────────────────────┐
│ Flutter UI  (Dart)   │                    │ Wyoming STT (Whisper)  │
│   ▲ FRB v2 streams   │                    │        ▼               │
│ Rust engine          │── TCP (Wyoming) ──►│ Anamanti Core (LLM)    │
│  • capture (cpal)    │◄── transcript ─────│  (Ollama or cloud)     │
│  • wake word (tract) │◄── TTS audio ──────│        ▼               │
│  • Wyoming client    │                    │ Wyoming TTS (Piper)    │
│  • playback (cpal)   │                    └────────────────────────┘
└──────────────────────┘
   mDNS: discovers _wyoming._tcp on the LAN
```

See [`architecture.md`](./plans/architecture.md) for the full design and
[`agents.md`](./agents.md) for AI-agent / contributor build guidance.

## Tech stack

| Layer | Technology |
| --- | --- |
| UI | Flutter (always-on landscape) |
| Systems | Rust, cross-compiled for `aarch64-linux-android` |
| Interop | `flutter_rust_bridge` v2 (zero-copy Dart↔Rust) |
| Audio | `cpal` (capture + playback); Android backend is NDK AAudio |
| Wake word | `tract-onnx` running an openWakeWord model |
| Transport | Wyoming Protocol over `tokio` TCP |
| Discovery | mDNS / Zeroconf (`_wyoming._tcp`) |
| Mac services | STT — Whisper via Wyoming **or** in-process whisper.cpp (`stt.engine`) · LLM · Wyoming TTS (Piper) |

## Repository layout

```
anamanti-display/   Anamanti Display — the Echo Show app: Flutter UI (`lib/`) +
                    Rust device engine (`rust/`: audio, wake word, Wyoming client)
anamanti-core/      Anamanti Core — the Mac Mini brain (STT ↔ LLM + memory ↔ TTS)
plans/              Design + planning docs (architecture.md, Plan.MD, TODO.md, …)
agents.md           Build guidance for AI agents & contributors
README.md           You are here
```

## Getting started

> Phases 1–4 are implemented (device engine + Mac Anamanti Core); the reactive UI
> and settings phases are still in progress. See `Plan.MD` for current state.

### Prerequisites

- Flutter SDK + Android SDK/NDK
- Rust toolchain with the Android target. The Echo Show 8 (`crown`) LineageOS
  build is **32-bit** (`armeabi-v7a`), so use the armv7 target:
  ```bash
  rustup target add armv7-linux-androideabi   # 32-bit Echo Show 8 (crown)
  # rustup target add aarch64-linux-android   # only for a 64-bit device
  ```
- `flutter_rust_bridge_codegen`
- An Echo Show 8 running LineageOS with `adb` access
- An M4 Mac Mini on the same LAN running Wyoming STT + Piper TTS

### Build & run (target)

```bash
# 1. (Re)generate FRB bindings after changing Rust signatures
flutter_rust_bridge_codegen generate

# 2. Build a device APK — cargokit cross-compiles the Rust engine into it.
#    Echo Show 8 (crown) is 32-bit, so target android-arm (armeabi-v7a).
flutter build apk --release --target-platform android-arm

# 3. Deploy to the Echo Show (LineageOS) over adb
adb install build/app/outputs/flutter-apk/app-release.apk
# ...or, for live development:
flutter run -d <echo-show-device>
```

On first launch the device discovers the Mac's Wyoming service via mDNS. No
static IP configuration is required.

### Run the Mac Mini Anamanti Core (the brain)

The `/mac` crate (`anamanti_core`) is the Wyoming host the device
discovers. It relays audio to a Whisper (STT) server, runs the pluggable LLM with
persistent memory, and streams a Piper (TTS) reply back — advertising
`_wyoming._tcp` over mDNS so the device finds it automatically.

```bash
# Point at your local Whisper + Piper Wyoming servers and pick an LLM backend.
ANAMANTI_LLM_BACKEND=ollama \
ANAMANTI_STT_ADDR=127.0.0.1:10300 \
ANAMANTI_TTS_ADDR=127.0.0.1:10200 \
cargo run --manifest-path anamanti-core/Cargo.toml --release
```

- `ANAMANTI_LLM_BACKEND` — `ollama` (default, local), `anthropic` (Claude; needs
  `ANTHROPIC_API_KEY`), `openai` (GPT / o-series; needs `OPENAI_API_KEY`), or
  `mock` (offline echo, no servers needed).
- **Provider API keys at runtime:** you don't have to set the cloud key before
  launch. The config page (`http://127.0.0.1:8730/`) has Anthropic / OpenAI API-key
  fields — paste a key, pick the backend, and it applies **without a restart** (the
  key is saved 0600 in `anamanti_settings.json`, so it survives reboots too). The env
  vars are just the boot seed. For safety the key fields live only on the loopback
  config page, not on the device settings screen.
- **Model selection:** the settings screen and the config page
  (`http://127.0.0.1:8730/`) show a **drop-down of specific Anthropic / OpenAI
  models from the last 12 months** (fetched live from each provider's `/v1/models`,
  with a curated built-in fallback). Picking one is saved on the Anamanti Core
  (`anamanti_settings.json`) and used for every subsequent chat turn. Pin an initial
  model with `ANAMANTI_ANTHROPIC_MODEL` / `ANAMANTI_OPENAI_MODEL`.
- **Anthropic auth — API key or subscription:** a per-provider toggle chooses how
  Claude authenticates. `ANAMANTI_ANTHROPIC_AUTH=apikey` (default) uses
  `ANTHROPIC_API_KEY` (`x-api-key`). `ANAMANTI_ANTHROPIC_AUTH=subscription` uses a
  Claude **subscription OAuth** token (`Authorization: Bearer` + the
  `anthropic-beta: oauth-2025-04-20` header) — provide it via `ANTHROPIC_OAUTH_TOKEN`
  (run **`claude setup-token`** once), or via `ANAMANTI_ANTHROPIC_TOKEN_CMD` (a command
  that prints a fresh token, default `ant auth print-credentials --access-token`).
  OpenAI is API-key-only (`OPENAI_API_KEY`) — its ChatGPT subscription does not grant
  API access.
- Piper (and, by default, Whisper) are off-the-shelf Wyoming servers the Anamanti
  Core is a client to. STT can instead run **in-process** (whisper.cpp,
  `stt.engine=whisper-rs`) with no separate server — see
  `plans/python-to-rust-whisper.md`. See `anamanti-core/src/config.rs` for config.
- **Multiple displays, one Anamanti Core:** N Echo Shows can share a single
  Anamanti Core — each connection is handled independently and every reply is
  routed back to the display that asked. Memory + settings are one shared
  household pool (speaker ID scopes per person, not per device).
- **Multiple Anamanti Cores (prod + test):** each Anamanti Core advertises a
  friendly `name` and a stable `instance_id` over mDNS. The device settings screen
  has an **Anamanti Core** dropdown to pick one; `"Auto"` uses the first available.
  The pick is **strict** — a display pinned to one Anamanti Core stays offline if
  it's unreachable rather than silently connecting to another.
  - **Local production** is a copied release binary installed at
    `/Volumes/External/DeveloperSupport/Anamanti Core/`. It runs outside any
    git checkout and pins its identity via `ANAMANTI_INSTANCE_ID` (set in
    `~/.zshenv`), using the default ports (10700 / config 8730) and the shared
    runtime data under `.../anamanti-core/`.
  - **Test copies** run straight from a git branch/worktree. `ANAMANTI_INSTANCE_ID`
    is resolved from that env → the working dir's **git branch code** → the
    sanitized service name, so a worktree copy is auto-named by its branch with no
    extra config. Run one alongside production with distinct ports (and a distinct
    service name to avoid an mDNS name clash); sharing production's memory means
    `ANAMANTI_MEMORY_BACKEND=sqlite` (two processes can't share one embedded HelixDB
    graph; SQLite is shared safely via WAL):

    ```bash
    # ANAMANTI_INSTANCE_ID is intentionally unset here → defaults to the branch code.
    env -u ANAMANTI_INSTANCE_ID \
      ANAMANTI_SERVICE_NAME="Anamanti Core (test)" \
      ANAMANTI_BIND_ADDR=0.0.0.0:10701 ANAMANTI_CONFIG_ADDR=127.0.0.1:8731 \
      ANAMANTI_MEMORY_BACKEND=sqlite \
      cargo run --manifest-path anamanti-core/Cargo.toml --release
    ```

> **Android toolchain note:** the project pins **AGP 8.7.3 / Kotlin 2.1.0 /
> Gradle 8.11.1** because the bundled cargokit Gradle plugin does not yet support
> Gradle 9 / AGP 9. See `Plan.MD` Phase 1.

## Status

**Phase 1 complete — verified on real hardware.** The monorepo is scaffolded
(`/lib` Flutter + `/rust` engine), `flutter_rust_bridge` v2 is wired, and a
hello-world Rust API cross-compiles into an `armeabi-v7a` APK that installs, runs,
and renders engine text over the FRB bridge on the physical Echo Show 8
(LineageOS, Android 11).

**Phase 2 complete — native audio + on-device wake word.** The Rust engine now
captures mic audio via `cpal`, decouples the real-time callback from inference
through a pre-allocated lock-free ring buffer, resamples to 16 kHz, and scores
wake-word confidence continuously and offline with the openWakeWord model chain on
`tract-onnx` — all on one low-overhead background thread. Flutter starts/stops the
engine over FRB and consumes a `Stream<WakeWordEvent>` (capture status, input
level, detections).

**Phase 3 complete — Wyoming client + mDNS + turn state machine.** The device now
discovers the Mac's `_wyoming._tcp` service, opens a `tokio` TCP connection on
wake-word detection, and streams PCM through a pure, unit-tested turn state machine
(`Idle → Triggered → Streaming → Closing`), rendering the server's transcript.
Wake-word scoring keeps running during a turn (full-duplex), with a configurable
raised confidence threshold as the interim self-trigger mitigation.

**Phase 4 complete — Mac Mini assistant pipeline.** The new `/mac` Anamanti Core
(`anamanti_core`) ties the brain together: a Wyoming server to the device
and a Wyoming client to Whisper (STT; Core-side energy VAD) and Piper (TTS), with a
**pluggable LLM** trait (Ollama / Claude / mock) and a **persistent SQLite + FTS5
memory** store (explicit "remember…"/"forget…" commands plus inferred fact/pref
extraction) in the middle. It advertises `_wyoming._tcp` over mDNS and streams the
synthesized reply back to the device. Remaining phases (reactive UI + playback,
settings) are planned — see the decision table and phases in `Plan.MD`.

## License

TBD.
