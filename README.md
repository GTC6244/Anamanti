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
  after the first sentence instead of waiting for the whole reply (end of speech → spoken reply:
  ~0.5 s median when System-1 answers, ~2 s median when the full LLM does; live install, 2026-10).
- 💬 **Barge-in + memory** — say the wake word again mid-reply to interrupt: playback
  stops instantly and a fresh turn begins (the Anamanti Core also aborts the in-flight
  LLM + TTS via an `anamanti-interrupt` frame). The assistant remembers
  facts/preferences across sessions (stored on the Mac).
- 🗣️ **Knows who's talking** — with speaker ID on, each person's voiceprint scopes
  what the assistant remembers and recalls: "I like jazz" is remembered for the person
  who said it, "the wifi is FastNet" for the whole household, and something said by a
  voice it doesn't recognise is kept for review on the Mac but never treated as true for
  anyone. "Who am I?" is answered from the voice, not from memory. Enroll on the
  display under Settings → People → Add a voice.
- 📺 **Anamanti Display** — a landscape Flutter UI tuned for the 8-inch screen,
  with an idle photo slideshow from a Google Photos/Drive folder (how often the
  photo changes is set per display, 5 s to 10 min). The front camera doubles as a
  presence sensor (brightens when you walk up, dims when the room is empty) and as a
  **light sensor**: when the lights go off at night the screen dims right down so it
  doesn't light up a dark room (Settings → Display → Night). Close the privacy shutter
  and you choose what that means: a dim clock, or awake by day and dim during your
  night hours. Nothing from the camera is stored or leaves the device.
- 🔔 **Proactive notifications** — the Mac can push a visual notification to the
  display *without* you asking (a reminder, an alert), over a persistent connection
  the device holds open to its pinned Anamanti Core. Visual-only today (no spoken
  output); send a test one from the config page's **Music & Devices** page.
- 🎵 **House music, by voice or touch** — "play some Radiohead" drives Spotify on the
  house speakers (Snapcast + librespot on the Mac; no music audio passes through the
  Echo Show). While a track plays, a compact control bar rides the idle screen; tap it
  for a full now-playing screen (artwork, progress, transport, volume) and the up-next
  queue (tap a queued track to jump straight to it) — or just say "show me what's playing" / "what's up next" / "hide the music".
  The display you asked shows the music **full screen**; every other display shows the
  small bar. Each display can switch on its own — the expand / minimize button, or say
  "make the music full screen" / "use the small music player" to it — and Music & Devices
  can pin any display to always full screen or always the small bar.
- 📻 **Internet radio** — curate stations on the Core's **Music & Devices** page (paste a
  stream URL, **Look up** fills the name + logo from the Radio Browser directory), then
  say "play Dave Rocks" / "next station" / "stop the radio". It plays on the same
  speakers as Spotify (mpv → Snapcast; starting one stops the other), and the display's
  now-playing screen shows the station logo, a LIVE badge and the current song.
- 🇧🇷 **Bilingual: English + Brazilian Portuguese** — turn it on under the config
  page's **Voice → Languages** (or the `language` block in `anamanti.json`) and anyone
  can speak either language on any turn: the assistant answers in the language that
  turn was spoken in, with a Portuguese Piper voice for Portuguese
  (`pt_BR-faber-medium`). Timers, the clock, "lembra que…", "esquece isso", "desliga a
  luz do escritório" and "fala como um gângster" all work in Portuguese; English is
  unchanged. The Display's own screens stay English (the live transcript and reply show
  whatever was said). See [`plans/BilingualPlan.md`](plans/BilingualPlan.md).
- 📹 **Front-door camera** — say "show me the front door" or "who's at the door?" and
  the display you're talking to shows the live view from the Reolink NVR (video only,
  ~1–3 s behind real time); "close it" or 60 s closes it. The NVR password stays on the
  Mac (`REOLINK_PASSWORD`); the video goes straight from the NVR to the display and never
  leaves the house. Set it up in the `camera` block of `anamanti.json`.
- 🎭 **Personalities** — give the assistant a speaking voice (overly enthusiastic,
  heavy sarcasm, depressed, 1940s gangster, Donald Trump, or normal). Pure *output
  tuning* — it changes only how replies sound, never what the assistant knows or does.
  Switch it on/off and pick one **by voice** ("talk like a 1940s gangster", "go back
  to normal"); edit the catalog of personalities on the config page's **Personality** page.
- 🖥️ **Admin console** — a loopback web console on the Mac (`http://127.0.0.1:8730/`)
  laid out for a 1920×1080 desktop: an **Overview** of the whole voice pipeline that
  marks every stage **On this Mac / Your network / Cloud** and lists exactly what leaves
  the house, plus pages for Voice, Intelligence, Personality, Household, Integrations,
  Music & Devices, Memory (including a switch for cloud entity extraction),
  Conversations (per-turn latency + the exact prompt), Keys & Accounts and System.

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
anamanti-companion/ Anamanti Companion — Android + iOS phone app (Flutter + Rust):
                    a scaled-back display showing system status, now playing, every
                    display's timers, and memories (with delete). Home network only
anamanti-protocol/  Shared Wyoming protocol crate used by Core, Display, and Companion
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
#    A `distribution` flavor dimension exists (selfUpdate / fdroid, see below), so
#    a flavor MUST be named. `selfUpdate` is the normal build.
flutter build apk --release --flavor selfUpdate --target-platform android-arm

# 3. Deploy to the Echo Show (LineageOS) over adb
adb install build/app/outputs/flutter-apk/app-selfUpdate-release.apk
# ...or, for live development:
flutter run --flavor selfUpdate -d <echo-show-device>
```

On first launch the device discovers the Mac's Wyoming service via mDNS. No
static IP configuration is required.

### Releases & in-app updates

The Display can update **itself** over the air: a built-in updater fetches a
signed APK + a `latest.json` manifest from a **Cloudflare R2** bucket, verifies a
SHA-256, and installs it via Android's `PackageInstaller`. (A GitHub Releases +
Obtainium path also still runs — see `.github/workflows/release.yml`.) Full design:
[`plans/UpdaterPlan.md`](plans/UpdaterPlan.md).

Build flavors (same signing key + app id across both):
- **`selfUpdate`** — ships the in-app updater (the normal build).
- **`fdroid`** — ships *without* it (no install permission / UI), for F-Droid.

To cut a release to R2:

```bash
# One-time: a release keystore (android/key.properties or ANDROID_* env — never
# commit it; the key must stay constant forever), an R2 bucket + custom domain with
# a ~60s cache rule on latest.json, and `npx wrangler login`.

# Bump `version:` in anamanti-display/pubspec.yaml first (X.Y.Z+N — the +N build
# number is the Android versionCode and MUST increase every release). Then:
R2_BUCKET=anamanti-dl \
UPDATE_BASE_URL=https://dl.example.com \
NOTES="Bug fixes" \
anamanti-display/scripts/release-r2.sh
```

Point each device at your domain in **Settings → Device → Updates** (the Update URL
field shows in Advanced mode; the default is a
placeholder). On the LineageOS kiosk, grant the app "install unknown apps" once.

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
- **Bilingual (English + Brazilian Portuguese):** add to `anamanti.json` (or set it live
  on the config page, Voice → Languages):

  ```json
  "language":   { "enabled": ["en", "pt-BR"], "default": "en" },
  "tts_voices": { "pt-BR": "pt_BR-faber-medium" },
  "tts_addrs":  { "pt-BR": "127.0.0.1:10201" }
  ```

  Install the `pt_BR` Piper voice where your Piper server keeps voices
  (`pt_BR-faber-medium.onnx` + `.onnx.json` from `rhasspy/piper-voices`), and run a
  **second Piper** for Portuguese on the `tts_addrs` port with the same `--data-dir` and
  `--voice pt_BR-faber-medium`: wyoming-piper keeps one voice loaded, so a single shared
  server reloads the model on every language switch (~0.5–1 s). `tts_addrs` is optional. The STT engine
  stays whatever you use: ElevenLabs needs nothing more; whisper-rs needs a
  multilingual model (`scripts/fetch-whisper-models.sh --multilingual`, then
  `"stt": { "model": "small", "multilingual": true }`); a Wyoming faster-whisper server
  must run with a multilingual model and no `--language`. Remove the `language` block to
  go back to English-only. Design + measurements: `plans/BilingualPlan.md`.
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
and a Wyoming client to Whisper (STT; Core-side VAD — neural Silero by default, energy/RMS fallback) and Piper (TTS), with a
**pluggable LLM** trait (Ollama / Claude / mock) and a **persistent SQLite + FTS5
memory** store (explicit "remember…"/"forget…" commands plus inferred fact/pref
extraction) in the middle. It advertises `_wyoming._tcp` over mDNS and streams the
synthesized reply back to the device. Remaining phases (reactive UI + playback,
settings) are planned — see the decision table and phases in `Plan.MD`.

## License

TBD.
