# Architecture

Detailed technical design for **Anamanti** (the ambient smart-display voice
assistant). This document is the source of truth for how the system is
structured; `Plan.MD` tracks phased delivery and open questions.

The two components: **Anamanti Core** (the Mac-side brain — STT ↔ LLM + memory ↔
TTS; crate `anamanti_core`, binary `anamanti-core`) and **Anamanti Display** (the
Echo Show app; package `com.anamanti.anamanti_display`).

---

## 1. System overview

Two cooperating nodes on a trusted LAN:

- **Echo Show 8 (LineageOS)** — the *edge* device. Captures audio, detects the
  wake word offline, streams audio to the Mac, renders the conversation, and
  plays back the spoken reply. Constrained: ~1 GB RAM, mobile-class CPU.
- **M4 Mac Mini** — the *brain* (**Anamanti Core**). Runs the Wyoming STT server,
  a pluggable LLM, and the Wyoming TTS (Piper) server.

They communicate over a single Wyoming Protocol TCP connection. The Mac's
service is located via mDNS, so neither node hardcodes an IP.

```
┌─────────────────────────── Echo Show 8 ───────────────────────────┐
│                                                                    │
│  Flutter UI (Dart)                                                 │
│   ├─ Ambient/idle dashboard                                        │
│   ├─ Live transcript view                                          │
│   └─ Streaming reply view                                          │
│        ▲                                                           │
│        │  flutter_rust_bridge v2 (StreamSink events, callbacks)    │
│        ▼                                                           │
│  Rust System Engine                                                │
│   ├─ Audio Capture   (cpal → oboe, 16 kHz mono i16)                │
│   ├─ Ring Buffer     (pre-allocated, lock-light)                   │
│   ├─ Wake Word       (tract-onnx + openWakeWord)                   │
│   ├─ Wyoming Client  (tokio TCP state machine)                     │
│   ├─ mDNS Resolver   (_wyoming._tcp)                               │
│   └─ Audio Playback  (cpal → oboe, TTS frames)                     │
│                                                                    │
└──────────────────────────────┬─────────────────────────────────────┘
                               │ TCP · Wyoming Protocol · newline JSON + PCM
                               ▼
┌─────────────────────────── M4 Mac Mini ───────────────────────────┐
│  Wyoming STT (Whisper / CoreML)  · Anamanti Core VAD (silero dflt) │
│        │ final transcript                                          │
│        ▼                                                           │
│  LLM backend  (trait-based, pluggable)  ◄─► Persistent Memory      │
│   ├─ Local backend  (Ollama / llama.cpp)          (local DB:       │
│   └─ Cloud backend  (Claude / OpenAI)              facts + prefs)  │
│        │ streamed reply tokens                                     │
│        ▼                                                           │
│  Wyoming TTS (Piper)  → synthesized audio frames                   │
└────────────────────────────────────────────────────────────────────┘
```

---

## 2. Component responsibilities

### 2.1 Rust System Engine (Echo Show)

The single owner of all real-time, resource-sensitive work. Chosen for
predictable memory use and no GC pauses under the 1 GB limit.

- **Audio Capture** — `cpal` pulls raw mono 16-bit PCM blocks from the mic array
  at the device-native rate (downmixed to mono in the real-time callback), then a
  linear resampler converts to 16 kHz off the RT path. On Android, `cpal` 0.18
  drives the NDK's **AAudio** input backend (the earlier `oboe` assumption is
  superseded — see `Plan.MD` Phase 2); on the macOS host it uses coreaudio.
- **Ring Buffer** — a pre-allocated single-producer/single-consumer circular
  buffer (`ringbuf::HeapRb`, allocated once) decouples the capture callback from
  consumers (wake word + network) with lock-free atomic index updates and no
  per-frame heap churn.
- **Wake Word** — a low-overhead thread continuously scores buffer windows with
  `tract-onnx` running the openWakeWord `.onnx` model chain (melspectrogram →
  feature/embedding → classifier). Fully offline; no audio leaves the device
  before a trigger.
- **Wyoming Client** — a `tokio` TCP state machine (see §4) that frames outgoing
  audio and parses incoming events.
- **mDNS Resolver** — browses `_wyoming._tcp`, resolves host/port, caches the
  last-known endpoint for fast reconnect.
- **Audio Playback** — `cpal`/`oboe` output stream that plays returned TTS audio
  frames. Symmetric with capture; keeps all audio in one layer.
- **Camera proximity sensor** (Android) — the front camera doubles as a proximity
  sensor. A thin Kotlin `CameraBridge` shim (twin of the `AudioRecord` `MicBridge`)
  opens the camera via Camera2 at 176×144 / ~5 fps and pushes packed luma frames over
  JNI; the Rust `camera::presence::PresenceDetector` measures frame-to-frame motion
  (mean absolute luma delta) and emits a `Presence` event when someone approaches or
  the room goes quiet. No ML / no face recognition / no frames leave the device. The
  brightness *actuation* is presentation and lives in Flutter (§2.2), not here.
  The same frames make the camera a **lux sensor**: each push also carries the
  capture's exposure time / ISO / aperture (from the Camera2 `CaptureResult` — frame
  brightness alone is useless because auto-exposure normalizes it), and
  `camera::ambient` estimates illuminance (`E ≈ 2.5·N²/(t·S/100)` scaled by the frame's
  distance from mid-grey), smooths it, and applies hysteresis (dark < `night_dark_lux`,
  lit again > 3×) + dwell (10 s to go dark, 2 s to come back) before emitting an
  `AmbientLight` event (`dark`, `lux`, `covered`) on a flip, plus a reading every 5 s.
  `covered` is a separate debounced check for a **covered lens** (privacy shutter /
  tape): frames with mean luma < 12 and stddev < 6 (0..255) held 10 s, cleared after
  2 s of normal frames (a pitch-black room can read as covered too).

### 2.2 Flutter UI (Echo Show)

> **Full on-screen UI inventory:** [`DisplayUI.md`](./DisplayUI.md) is the canonical
> catalog of every element that can appear on the Display — backgrounds, overlays,
> widgets, banners, full-screen views, status indicators, and settings — grouped by
> category with file paths and triggers. Keep it updated when you add or change UI.

- Always-on landscape layout tuned for the 8-inch display.
- Consumes FRB-generated `StreamSink` events; no polling.
- Primary states: **idle/ambient**, **live transcript**, **thinking**,
  **streaming reply**, **speaking**.
- **Idle/ambient screen:** photo slideshow from a chosen Google Photos/Drive
  folder via **on-device OAuth** (keeps running when the assistant is
  disconnected).
- **Settings screen:** LLM backend, TTS voice, wake word, photo source (on-device
  Google auth + folder picker), and **memory management** (view/delete entries).
- Renders reply tokens smoothly as they arrive.
- **Screen brightness:** the camera proximity sensor's `Presence` events fold into
  `AssistantState.userPresent`; `ScreenBrightnessController` actuates the window
  backlight (bright on approach, dimmed when quiet) via a `MethodChannel` to
  `MainActivity`. Sensing is Rust (§2.1); only the actuation is here. The **dim
  delay** — how long the screen stays bright after the room goes quiet before it
  dims to the away-mode clock — is a device-local setting (`AppSettings.dimDelaySecs`
  → the proximity detector's release window; Settings → *Display*).
  **Night dimming:** `AmbientLight` events fold into `AssistantState.roomDark` /
  `ambientLux`; while the room is dark and `AppSettings.nightDimEnabled` is on, the
  controller uses night levels (awake 0.15 / away 0.02 instead of 1.0 / 0.25). The
  toggle applies live; the darkness threshold (`AppSettings.nightDarkLux` →
  `WakeWordConfig.nightDarkLux`) restarts the engine. **Covered camera:**
  `AppSettings.coveredCameraMode` picks what a covered lens means — `clock` (default:
  take the blind camera's dark + absent reading) or `schedule` (awake by day, dark +
  absent during `nightStartMinutes`→`nightEndMinutes`, default 21:00→07:00, kept current
  by a once-a-minute tick in `AssistantController.updateCoveredCamera`). Resolved in
  `AssistantState.effectivePresent` / `effectiveRoomDark`; applied live.
- **Photo rotation time:** how long each idle background photo stays up before the
  slideshow cross-fades to the next is a **per-device**, device-local setting
  (`AppSettings.slideIntervalSecs`, default 10 s, 3 s…10 min; Settings → *Display →
  Photos*), applied live to `SlideshowController.setInterval` — it never touches the
  Core, so each display in the house can rotate at its own pace.

### 2.3 Mac Mini services

- **STT (Whisper)** — two interchangeable engines behind the
  `anamanti-core/src/stt/` `Transcriber` seam (`plans/python-to-rust-whisper.md`),
  selected by the `stt.engine` config key:
  - **`wyoming`** (default) — the external `wyoming-faster-whisper` Python server
    over the Wyoming Protocol (`stt_addr`, port 10300).
  - **`whisper-rs`** — **in-process** whisper.cpp (`stt-whisper-local` build
    feature; optional `metal`/`coreml` accel). No separate process, no Python: the
    Core loads a ggml model (`base`/`small`) and transcribes in a `spawn_blocking`
    task. This is the deploy-simplification path, not a speed change.
  Either way the transcript is the same Whisper-class text; end-of-speech is the
  Core's VAD (below; Silero by default since 2026-09-30, pluggable), never the engine.
- **LLM backend** — a trait/interface with a streaming
  `respond(transcript) -> token stream`. Two interchangeable implementations:
  local (Ollama/llama.cpp) and cloud (Claude/OpenAI). Selection is config-driven.
  Reads/writes the persistent memory store to build context and record new facts.
- **Persistent Memory** — a **SQLite** DB on the Mac is the store of record for
  long-term facts and user preferences across sessions. Policy is **explicit +
  inferred**: entries are added on request ("remember…") and auto-extracted from
  conversation turns. Managed via a settings list (view/delete) and voice
  ("forget that"); kept until cleared. Private to the LAN; survives device
  reflashes. **Retrieval/recall runs behind a `memory::Recall` seam and defaults
  to the embedded HelixDB GraphRAG backend** (`anamanti-core/src/memory/helix.rs`;
  in-process, no server/Docker): each completed turn is appended to
  `anamanti_chatlog.jsonl` and a background ingester (`memory/ingester.rs`) embeds
  it (OpenAI `text-embedding-3-small`, `OPENAI_API_KEY`) into a graph of
  `User/Turn/Memory/Entity` nodes, so recall does vector KNN + a graph hop rather
  than keyword FTS. **Non-answer turns are excluded from recall:** the ingester's
  `is_non_answer(reply)` gate skips making a declined/"I can't find that"/clarification
  turn a recallable `Turn` node — otherwise a run of them resurfaces under the prompt's
  "What you remember about this person" block and few-shot-primes the model to repeat the
  failure (a self-reinforcing loop on a mis-transcribed proper noun). The turn still lands
  in the chat log (source of truth) and any **facts** it wrote are still ingested as
  `Memory` nodes; only its raw `transcript\nreply` node is withheld. Graphs built before
  the gate are cleaned once per backend start by `HelixMemory::purge_non_answer_turns`
  (drops matching `Turn` nodes + their edges + vector-index entries; `Memory` facts
  untouched). Beyond that automatic prune, the device **Memory** screen (Settings →
  Memory) can **manually browse and delete** the graph: `anamanti-list-turns` /
  `anamanti-list-entities` return `Turn` / `Entity` ("keyword") rows (filterable by
  text, speaker, or mentioned entity), and `anamanti-delete-graph-node` drops a chosen
  node. These are **Helix-index-only** edits (the chat log stays the source of truth, so
  a rebuild/backend-switch can re-import a deleted turn); they ride the existing
  device↔Core control path and need the HelixDB backend (SQLite recall answers
  `ok:false`). The screen's third surface is the durable SQLite facts via the existing
  `anamanti-list-memories`/`-delete-memory`/`-clear-memories` frames. If `OPENAI_API_KEY` is absent or the graph fails to open,
  recall **falls back to SQLite FTS** (writes are unaffected); `ANAMANTI_MEMORY_BACKEND=sqlite`
  forces FTS. **Per-person:** an opt-in local voiceprint embedder
  (`anamanti-core/src/speaker/`) identifies who is speaking from the utterance PCM and scopes
  memory writes/recall and the prompt to that person; see `speaker_id_plan.md`.
  **Speaker-scoped memory** (`SpeakerScopedMemoryPlan.md`): every memory has a **scope**
  — `person` (true for one identified person, recalled only for them), `household`
  (deliberately true for everyone, recalled for every speaker), or `unknown` (said about
  themselves by a voice that could not be confirmed — **kept but never recalled**, shown
  in the admin console's review queue for attribution). An unattributed turn is the
  `unknown` speaker, never the household. Each turn's speaker has a **resolution** —
  `identified` (match ≥ `match_threshold`), `tentative` (between the thresholds, or a
  short follow-up turn inheriting the previous identified speaker), `new` (a fresh
  anonymous cluster) or `unknown`: identified/tentative speakers *read* their own +
  household memories; only an identified speaker *writes* personal facts to themselves
  (others' personal facts go to `unknown` with the best guess as `candidate_speaker_id`).
  Facts are classified personal vs household by `memory::classify_scope` ("I/my/the
  user" → personal; "we/our/the house/the wifi/everyone" → household). Every memory
  records provenance (`said_by_speaker_id`, resolution, confidence, `turn_id`). A
  **read-only short-clip match** (`speaker.min_identify_ms`, default 600 ms, plus
  `short_clip_margin`) identifies short questions like "who am I?" without creating or
  updating voiceprints. Matching is also **household-relative**: each score is compared
  with the closest *other named* voiceprint (anonymous clusters are not rivals — they are
  often fragments of a known voice). A match that barely leads another person
  (`speaker.min_margin`, 0.05) is only tentative and trains nobody; a tentative-band match
  to a named person that leads every other named person by `speaker.clear_margin` (0.20;
  needs ≥ 2 named people; `0` disables) is identified without training the voiceprint. The
  lead is logged per turn (`speaker_margin` in the chat log). The prompt's identity line comes from the **voiceprint name**
  (reconciled to the household roster's spelling — "Christina" → *Kristina*), never from
  memory text; "who am I?" is a deterministic fast path; "my name is X" only names an
  identified, still-anonymous voiceprint with the roster spelling (`speakers.name_source`
  = `enrolled | admin | spoken`; enrollment-by-name never folds into a spoken-named
  profile). Forgetting a voiceprint moves its memories to `unknown`. The embedder is a real ECAPA-TDNN ONNX
  (WeSpeaker `voxceleb_ECAPA512`, 192-dim, pure-Rust `tract-onnx`) selected at runtime
  by `speaker.model_path`. Beyond passive auto-clustering, the device **People** screen
  has a guided **"Add a voice"** enrollment flow: it stamps an `enroll` display context
  on ordinary wake-word turns (same suppress-the-LLM mechanism as the diagnostics screen —
  **no second audio path**), the Core accumulates the captured samples into a named
  voiceprint (`anamanti-enroll-finish`/`-cancel` → `anamanti-enroll-result`), then a verify
  capture confirms recognition; see `SpeakerEnrollmentPlan.md`. The voice is **tied to the
  household roster**: the device picks an existing member (the `anamanti-speakers` reply
  carries the roster's `members` — names only, no contact PII) or enters a new name; the
  Core saves under the member's canonical name, auto-adds a new name to the household, and
  folds the samples into a person's existing same-named voiceprint instead of duplicating it.
  A person's People-row menu also offers **"Add voice samples"**: the same guided phrase
  capture, committed with a `speaker_id` on `anamanti-enroll-finish` so the samples fold into
  that exact profile (keeping its name — an anonymous cluster stays anonymous; the household
  roster is untouched). **Link to household member:** the People row menu sends
  `anamanti-link-speaker {id, member}`; the Core (whose household roster is the authority)
  rejects a non-member, otherwise merges `id` into that member's existing voiceprint via the
  coordinated `SpeakerService::merge` (voiceprint + SQLite memories + clips + GraphRAG
  history) or — if they have none — names `id` as the member, so each household member ends
  up with one voiceprint.
- **End-of-speech / VAD** — runs in the **Anamanti Core**, for **both** STT engines
  (neither does streaming VAD: `wyoming-faster-whisper` transcribes only on
  `audio-stop`, and whisper.cpp transcribes the buffered utterance on finalize). The
  per-chunk speech decision is **pluggable behind a `SpeechGate` seam**
  (`anamanti-core/src/vad/`): the **committed default is the neural Silero gate since
  2026-09-30** (`SileroGate`, `vad.engine="silero"`; the `vad-silero` feature is on by
  default and a default boot requires the v4 model on disk — a `silero` boot with no
  model is a hard error). The **energy/RMS gate** (`EnergyGate` — score per-chunk RMS,
  voiced when above `voice_rms_threshold`) is now the opt-in fallback
  (`vad.engine="energy"` or a `--no-default-features` build) and the auto-fallback when
  no model is loaded — see `plans/VadSileroPlan.md`. The surrounding state machine is
  engine-independent:
  after speech (a 250 ms onset debounce) followed by ~700 ms of trailing silence (or a
  6 s no-speech fallback), it finalizes the transcriber
  (`anamanti-core/src/orchestrator/vad_loop.rs`, `stream_to_transcript`). The Echo Show device
  still runs **no VAD of its own** — it streams continuously and waits for the
  transcript.
- **Wyoming TTS (Piper)** — synthesizes the reply into audio frames streamed back
  to the device.

> **Implementation (Phase 4, `/mac`):** the STT, LLM, memory, and TTS pieces above
> are wired by a standalone Rust crate `anamanti_core` (`/mac`). It is a
> Wyoming **server** to the Echo Show and a Wyoming **client** to the off-the-shelf
> Whisper (STT) and Piper (TTS) Wyoming servers, with the pluggable `LlmBackend`
> trait (Ollama / Claude / mock) and the SQLite+FTS5 memory store in the middle
> (`anamanti-core/src/{Anamanti Core,server,discovery,llm,memory,wyoming}.rs`). It advertises
> `_wyoming._tcp` over mDNS, symmetric with the device's Phase-3 browse. The
> Wyoming wire codec is re-implemented there (byte-identical to the device's) since
> the device crate is an Android `cdylib` and can't be shared as a Mac library.

---

## 3. Interop boundary (flutter_rust_bridge v2)

- FRB v2 generates the JNI bindings and the Dart API from Rust signatures.
- Data flows **Rust → Dart** over a **single** generated `StreamSink`:
  - `start_wake_word_engine` returns one `Stream<WakeWordEvent>` covering the whole
    turn lifecycle. The `WakeWordEventKind` tag spans: `started`, `status`, `level`,
    `detected` (Phase 2); `connecting`, `streaming`, `transcript`, `disconnected`
    (Phase 3 Wyoming turn); `replyToken`, `speaking`, `speakingDone` (Phase 5); and
    `stopped` / `error`. `speakingDone` is UI-only: it fires when the reply audio has
    finished playing out of the ring (drained or barge-in-flushed), so the UI can keep
    the reply text on screen for exactly as long as it is being read, then remove it.
    Modeled as a flat struct tagged by a unit-only `WakeWordEventKind` enum
    (payload fields carry neutral defaults when not relevant), so the boundary needs
    no `freezed` codegen and there is exactly one stream to manage.
  - The periodic `level` event carries the mic **RMS** and, while a wake-word model
    is loaded, the live wake-word diagnostics (`score`, `avg_score`, `threshold`,
    `gain_db`) that drive the **Audio Diagnostics** settings page's meters +
    readouts (`WakeWordEvent::level_diag`). That page also live-tunes the running
    engine's capture gain + idle threshold **without a restart** via the small
    `#[frb(sync)]` `update_diagnostics_tuning(gain_db, threshold)` call (the loop
    re-reads two atomics each block; they re-seed from `WakeWordConfig` on restart).
  - The UI split (transcript vs. reply vs. phase) happens **Dart-side**, not on the
    boundary. `AssistantController` (`anamanti-display/lib/src/engine/assistant_controller.dart`)
    folds this single event stream into an observable `AssistantState` / `TurnPhase`
    (`idle → listening → connecting → thinking → speaking`, plus `error`) that
    widgets watch. There are no separate `transcript_stream` / `reply_token_stream`
    / `state_stream` sinks on the FRB boundary.
- Control flows **Dart → Rust** via generated function calls: `start_wake_word_engine`
  / `stop_wake_word_engine` (Phase 2–3) plus the Phase-6 settings functions
  (`fetch_orchestrator_settings`, `update_orchestrator_settings`, `list_memories`,
  `delete_memory`, `clear_memories` in `anamanti-display/rust/src/api/settings.rs`). The settings
  functions are async: each stands up a small current-thread `tokio` runtime and
  drives one Wyoming control round trip, so FRB returns a `Future` off the UI
  isolate.
- Zero-copy is preferred for audio-adjacent buffers; UI text uses ordinary
  generated types.

### Phase 6 — settings + memory control protocol

- The **wake word, thresholds, and photo source** are device-local: the Flutter
  settings screen persists them (`SettingsStore` → JSON) and applies them by
  rebuilding the `WakeWordConfig` and restarting the engine / refreshing the
  slideshow. Settings **applies changes immediately** (no Save button): device-local
  changes are debounced + serialized so a burst costs at most one engine restart;
  capture gain and both wake-word thresholds are pushed to the running engine live
  (`update_diagnostics_tuning`, no restart); Core-managed changes are debounced and
  revert on failure. Layout: a six-section side rail with two-column tabs that fit the
  960×600 dp screen — see [`SettingsReorgPlan.md`](./SettingsReorgPlan.md) and
  [`DisplayUI.md`](./DisplayUI.md) §9.
- The **LLM backend + model and TTS voice** live on the Mac and are read/changed
  over a **project-local control protocol** on the device↔Anamanti Core hop —
  `anamanti-*` Wyoming frames (`describe`/`set` settings; `list`/`delete`/`clear`
  memories; `list-models`; `list-voices`) that ride the existing framing (byte-identical `types`
  in both crates, no off-the-shelf server sees them). The Anamanti Core's `Pipeline`
  reads a per-turn snapshot of runtime-swappable `SharedSettings`, so a
  backend/voice change takes effect on the next turn with no restart; the accept
  loop routes control frames to `control::handle_control` and audio-start frames to
  a turn. Backends are `ollama` (local), `anthropic` and `openai` (cloud), and
  `mock`; selecting a cloud backend needs its API key (`ANTHROPIC_API_KEY` /
  `OPENAI_API_KEY`), else the change is rejected in-band (never dropping the
  connection). The key can come from the environment at boot **or** be entered at
  runtime on the loopback config page: `SettingsUpdate` carries optional
  `anthropic_api_key` / `openai_api_key` fields (same tri-state as the search key —
  absent = keep, `null` = clear, value = set), so a cloud backend can be enabled
  without a restart. Runtime keys live in `SharedSettings` (env is only the boot
  seed) and persist to `anamanti_settings.json` (0600). Key **entry** is deliberately
  config-page-only — the Wyoming/device control path never accepts a provider key, so
  cloud secrets never live on the shared Echo Show screen; the device is told only
  whether a key is set (`anthropic_key_set` / `openai_key_set`), never its value.
- **Model selection** is a drop-down of **specific Anthropic / OpenAI models from
  the last 12 months**, produced by `llm::catalog::ModelCatalog`: it live-queries
  each provider's `GET /v1/models` (Anthropic `created_at`, OpenAI `created`),
  filters to the trailing 12 months (and OpenAI to chat models), caches the result,
  and falls back to a curated built-in list when a key is missing/offline. The list
  is exposed to the device via `anamanti-list-models` → `anamanti-models` and to the
  browser via `GET /models` on the config page. The chosen `llm_model` flows through
  the same `SharedSettings::apply` → `LlmFactory::build` path into the concrete
  backend's request, and persists to `anamanti_settings.json`.
- **TTS voice selection** is likewise a drop-down. On `anamanti-list-voices`, the
  Anamanti Core asks Piper for its advertised catalog (a downstream Wyoming
  `describe` → `info`) and, when `ANAMANTI_TTS_VOICES_DIR` points at Piper's model
  dir (Piper co-located on the Mac), intersects it with the `<name>.onnx` files
  actually on disk so only installed voices are offered; with no dir set it returns
  the full advertised list. Exposed to the device via `anamanti-list-voices` →
  `anamanti-voices` (FRB `list_voices` → `listVoices`) and to the browser via
  `GET /voices` on the config page. Both the settings screen and the config page
  render a dropdown (with a "Server default" entry) and fall back to a free-text
  field when the list is unavailable; a hand-set voice not in the list stays
  selectable.
- **Functions status snapshot** (device Settings → Assistant → Capabilities): a **read-only**
  aggregate of what the Anamanti Core is configured to use for every capability —
  cloud vs local, the model, and whether each required API key is set — so a user at
  the display can see the assistant's setup (and spot a missing key) without the Mac
  config page. The device fetches it over a dedicated control round trip
  `anamanti-get-functions` → `anamanti-functions` (byte-identical `types` in both
  crates; FRB `get_functions` → `OrchestratorClient.fetchFunctions()`). The Core's
  `control::functions_response` builds one row per capability from the existing
  `SharedSettings` accessors (`view`, `system1_view`, `stt_view`, the weather/places/
  directions/Spotify/Cadora/AppSaid/calendar accessors) plus the live
  `GraphRagController::status` for embeddings (threaded into the device server
  alongside `catalog`/`notify`/`weather`). It covers LLM + web search + TTS (Config),
  speech-to-text (Speech), directions/weather/places + Spotify/shopping list/phone
  messages/calendar (Tools), fast decisions (System1), and embeddings (Memory). Like
  the rest of the control path it never carries secret values — only each key's
  set/unset bit — so the red "API key missing" warning on the device is driven by the
  same `*_key_set` booleans the loopback config page uses.
- **Anthropic auth mode** (`llm::anthropic_auth`): a per-provider toggle selects
  **API key** (`x-api-key` from `ANTHROPIC_API_KEY`) or **subscription OAuth**
  (`Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20`). Subscription tokens
  come from `AnthropicTokenProvider` — `ANTHROPIC_OAUTH_TOKEN` (from
  `claude setup-token`) or a token-printing command (`ANAMANTI_ANTHROPIC_TOKEN_CMD`,
  default the `ant` CLI), cached with a short TTL and refreshed on a 401. Both the
  chat backend and the catalog share the provider so listing + turns authenticate
  identically. The mode rides the settings protocol (`anthropic_auth` field) and the
  config page toggle; OpenAI is API-key-only.
- **Admin-console front-end** (`webconfig.rs` + `src/webconfig/*`): the whole config
  UI is **plain static HTML in a Material Design layout styled with Tailwind CSS**,
  served by the same hand-rolled loopback HTTP server — no page markup is generated in
  Rust. Each page is an authored `.html` body (`overview.html`, `voice.html`, …)
  wrapped by a static shell (`shell_head.html` + `shell_foot.html`: the top app bar
  with status pills + theme toggle, and the collapsible navigation rail, with a
  `{{TITLE}}` token `page()` fills in); shell behavior lives in `shell.js` and the
  component kit (save bar, choice cards, locality chips, secret fields, drawers, tabs,
  toasts) plus the **locality map** in `ui.js`. **Information architecture
  (2026-10-14, `plans/CoreWebUIPlan.md`):** ten task-grouped pages — Overview, Voice,
  Intelligence, Personality, Household, Integrations, Music & Devices, Memory,
  Conversations, Keys & Accounts, System — each laid out on a 12-column grid to fit one
  1920×1080 screen (data pages scroll inside their panes). Every option that can run
  on this Mac, on the LAN or in the cloud carries a **locality chip**; address-backed
  stages (Wyoming STT/TTS, Ollama, laya-serve) are classified from their configured
  address by `webconfig/system.rs` (loopback → local, RFC 1918/`.local` → LAN, else
  cloud), and cloud options show a **"Sends:"** line naming what leaves the house.
  Legacy page routes 302 to their new homes; JSON/POST endpoints are unchanged. Tailwind is **vendored
  precompiled**: `webconfig/tailwind/input.css` (theme + `@layer components`) is built
  by `scripts/build-webui-css.sh` (the `tailwindcss` CLI) into the committed
  `webconfig/panel.css`. All assets (`panel.css`, `shell.js`) are compiled into the
  binary via `include_str!` and served as their own routes (`GET /panel.css`,
  `GET /shell.js`) — so the single copied release binary still serves the entire UI
  offline, with no CDN and nothing beside it. Page data flows through the existing
  `*.json` endpoints the bodies fetch, plus three read-only aggregators:
  `/overview.json` (every pipeline stage with engine + tier + health probe, the
  integrations, connected displays, recent-turn latency), `/keys.json` (every secret /
  OAuth link: set or not, seed env var, what uses it — never a value) and `/system.json`
  (build, runtime, and a curated **redacted** snapshot of the boot config,
  `webconfig::BootInfo`, built once in `main`).
- **Cloud entity extraction toggle** (`memory/graphrag_control.rs`): GraphRAG entity
  extraction is the one memory step that calls a cloud model (Anthropic,
  `graphrag.extract_model`) even when the LLM and embedder are local. It sits behind a
  `GatedExtractor` with a live on/off switch: `graphrag.extract` (`auto` default | `off`)
  seeds it, the Memory page flips it (`POST /extraction/save`), and the choice persists
  in `helix_path/extract_mode` (wins over the seed at boot). Off ⇒ ingested turns get no
  entities and no turn text leaves the Mac for extraction; recall is pure vector KNN.
- **Turn ↔ prompt join:** each turn mints a `turn_id` (`t-{unix_millis}-{n}`,
  `orchestrator::turn::TURN_IDS`) at start and writes it to both its chat-log and
  prompt-log record (`turn_id`, optional for older records), so the Conversations page
  shows the exact prompt for a turn without timestamp guessing.
- **Debug/inspection pages** (`webconfig.rs`, same loopback HTTP server as the
  config page, strictly read-only): `GET /chatlog`, `/prompts`, `/sqlite`, `/helix`
  render the recent chat log, the exact assembled LLM prompt per turn (captured to
  a separate `anamanti_promptlog.jsonl` via `PromptLog`), the SQLite memory rows, and
  the HelixDB GraphRAG node stats + a sample of nodes (behind the read-only
  `memory::GraphView` seam; reports "disabled" on the SQLite backend). Each has a
  `*.json` data endpoint the page fetches. No auth — keep the config address on a
  trusted network. On `/chatlog`, each turn is **click-to-expand**: the orchestrator
  records a per-turn latency breakdown (`memory::TurnTiming` — STT finalize, System-1
  decision + intent/confidence, fast-path fetch, System-2 recall/prompt/LLM-first-token,
  TTS first chunk, and the end-of-speech→reply total) onto the `ChatLogRecord`, and the
  row expands to show the stages that actually ran. Optional + `skip_serializing_if`, so
  pre-feature log lines parse fine and render "no timing recorded".
- **Companion API** (`webconfig::serve_companion`, a *second* listener on `companion_addr`,
  default `0.0.0.0:8740`): the LAN door for the Anamanti Companion phone app. Same HTTP
  plumbing and route table as the console, but a different guard: only
  `COMPANION_ROUTES` (`/overview.json`, `/music/nowplaying.json`, `/timers.json`), `GET`
  only, and any request with an `Origin` header is refused (no browser page can read it).
  No auth (read-only, owner decision 2026-10-22). Its port is advertised in the mDNS TXT
  record as `companion_port`. `/timers.json` is the Core's mirror of every display's
  `anamanti-timers` report (`timer::DisplayTimers`, keyed by `device_id`, dropped when that
  display's notify channel closes; `remaining_secs` recomputed per request). The phone
  lists/deletes memories over the Wyoming memory frames, exactly as the Display does.
- **Household / home context** (`settings::Household`, editable at `GET /household`
  on the config page): a persisted record of the home **location + units** and a
  **roster of people** (name, emails, phones, relationship). Location/units seed from
  `ANAMANTI_HOME_LOCATION`/`ANAMANTI_WEATHER_UNITS` at first boot, then are editable
  (with the people) from the Household tab and persisted to `anamanti_settings.json`
  (0600 — PII). It is read from the **per-turn settings snapshot** and injected into
  the system prompt (`orchestrator::location_line` grounds an unqualified "here";
  `household_line` lists who lives here so the model can address them and has their
  contact details), so page edits take effect with no restart. The **directions
  tool's default origin** reads the same location via a shared `LiveHomeLocation`
  handle (`directions`), so a location edit re-homes routing without rebuilding the
  backend. An **identified speaker** (speaker_id_plan.md) whose name matches a roster
  member is reconciled to it (`Household::member_matching`), so the identity line uses
  the canonical name + relationship.
- **Memory management** is dual: the settings list (this control protocol) plus
  voice ("remember…", "forget that") applied on the Mac during a turn (Phase 4).
- **On-device Google OAuth** for the photo folder is wired as a seam
  (`GoogleAuthenticator`); the default is an honest stub because a real client ID
  can't be provisioned in this environment.

---

## 4. Wyoming client state machine (Rust)

```
        ┌────────────────────────────────────────────────────────┐
        ▼                                                        │
     ┌──────┐  wake word fires   ┌───────────┐  socket open   ┌──────────┐
     │ IDLE │ ─────────────────► │ TRIGGERED │ ─────────────► │STREAMING │
     └──────┘                    └───────────┘  send audio-   └──────────┘
        ▲   socket dormant/closed              start header        │
        │                                                          │  send PCM frames;
        │                                                          │  read transcript events
        │                                                          ▼
        │                        ┌───────────┐   final txt   ┌──────────┐
        │   playback done /      │ SPEAKING  │ ◄──────────── │ THINKING │
        └──── reset ──────────── │ (play TTS)│  TTS frames   │ (LLM)    │
                                 └───────────┘               └──────────┘
```

- **IDLE** — wake word evaluation runs; TCP socket dormant/closed.
- **TRIGGERED** — wake word fires; open TCP, send Wyoming `audio-start` header.
- **STREAMING** — send raw PCM chunks in Wyoming frames; concurrently read
  `transcript` events on the same socket. The device streams continuously and runs
  no VAD; **the Anamanti Core detects end-of-speech** (its `SpeechGate` VAD over the
  PCM — Silero by default since 2026-09-30, energy/RMS opt-in; see `plans/VadSileroPlan.md`) and sends
  `audio-stop` to the STT server, which then returns the final transcript.
- **STREAMING → (optional) System-1 fast decision:** once the transcript is final, an
  **optional pluggable System-1 decision engine** may run on the Anamanti Core *before* THINKING
  (before memory recall and the LLM). It scores a fixed routing question set in a single
  non-autoregressive forward pass and either **Resolves** a common intent or **Defers**:
  - **Resolve** (confident, closed intent): drive the matching **existing `DeviceAction`** and
    speak a short templated line via Piper, then finish the turn — **skipping memory recall and the
    LLM entirely**. Implemented intents: **weather** (Open-Meteo → `DeviceAction::ShowWeather`, so
    the widget opens *before* speech) and **timer** (a conservative duration parser →
    `DeviceAction::StartTimer`; cancels/free-form defer). Resolved replies are **not**
    wake-word-interruptible in v1 (sub-second line). Chat-log write, inferred memory, and the
    follow-up `listen` window still happen (the fast path shares `emit_follow_up_and_stop` with the
    normal reply path). Because the engine is a *typed classifier* (intent label, no free-form
    slots), only intents with defaulted/parseable arguments are eligible; the rest defer.
  - **Defer** (ambiguous, open-ended, low confidence, disabled, or error): fall through to THINKING
    unchanged.
  The engine is pluggable behind a trait (like the LLM) and shares the `/v1/systemone` wire
  contract, so a local **`laya-serve`** sidecar (default) or **Jev/OpenRouter** (cloud fallback)
  are swappable from config. Default is off (`system1.backend = none`), reproducing today's flow.
  Full design: [`system1-fast-decisions.md`](./system1-fast-decisions.md).
- **THINKING** — STT final transcript handed to the LLM backend (which
  consults persistent memory); reply tokens stream back and render.
- **THINKING → SPEAKING (streaming TTS):** the Anamanti Core does **not** buffer the
  whole reply before speaking. It segments the LLM token stream into sentences and
  synthesizes each with Piper as soon as it forms, so the first audio reaches the
  device ~first-sentence latency (~2 s) instead of full-reply latency (~8 s). The
  per-sentence Piper bursts are **coalesced into one device-facing audio stream** (one
  `audio-start`, all chunks, one final `audio-stop`) because the device ends its turn
  on the first `audio-stop`.
- **SPEAKING** — the coalesced Piper audio stream is played via `cpal`/`oboe`. The
  turn returns to IDLE on the first `audio-stop` (above), but the audio keeps
  draining from the ring for seconds after; the device tracks ring occupancy
  (`PlaybackSink::pending()`) and emits a UI-only `speakingDone` once it empties, so
  the on-screen reply text stays up for the whole utterance and is removed only when
  the audio actually stops (a barge-in flush zeroes the ring and triggers it too).
- **Barge-in:** wake-word scoring keeps running during THINKING and SPEAKING. A wake
  word mid-reply **always flushes the playback ring immediately** (silencing the reply
  even after the turn has technically ended — the Anamanti Core relays audio faster than
  real-time, so the turn reaches IDLE while audio is still draining from the buffer),
  sends an `anamanti-interrupt` frame so the Anamanti Core **aborts the in-flight LLM +
  TTS**, and starts a fresh turn.
- **Follow-up listen (reopen the mic after every reply):** after each reply the
  Anamanti Core sends an `anamanti-listen` frame (above) before the final `audio-stop`,
  with a `wait_secs` window (10 s after a question, 5 s otherwise). The device records
  it during SPEAKING, ends the turn normally, and — once the reply audio has drained
  from the playback ring (the same `pending()==0` signal that emits `speakingDone`, so
  the mic never records the tail of the TTS) — **spawns a fresh turn with no wake word**,
  reusing the barge-in restart machinery. The follow-up turn keeps the raised in-turn
  wake-word threshold and stamps its chain depth + `wait_secs` on its `audio-start`; the
  Anamanti Core feeds recent conversation history into that turn's prompt and sizes its
  no-speech window to `wait_secs`. The no-speech window only counts down while `speech_started`
  is false; because the drain gate frees when the *software* playback ring empties (not the
  OS/hardware output buffer), the reopened mic can still catch a brief residual TTS tail or
  room echo, so the Anamanti Core **debounces the speech onset** — `speech_started` latches
  only after `MIN_SPEECH_ONSET` (250 ms) of *consecutive* voiced audio (`voiced_onset_step`),
  and any non-voiced chunk resets the run. This stops a lone transient from collapsing the
  full `wait_secs` window into the short `end_silence` finalize and cutting the user off
  (observed in production as a follow-up that closed after ~1 s with an empty transcript).
  If the user says nothing in that window the
  Anamanti Core ends the turn (empty transcript + `audio-stop`) and the device **sleeps**;
  the chain continues only while the user keeps responding (bounded by the optional
  `follow_up.max_chain`, default unlimited). See `Plan.MD` (Follow-up listening).
- **In-app AEC:** not shipped. Investigated on real hardware (Echo Show 8): the platform
  `VOICE_COMMUNICATION` AEC preset is reachable but doesn't actually cancel on this
  device, and a software NLMS canceller is net-negative for this flush-on-wake barge-in
  (there's no simultaneous echo to cancel once playback is flushed). The interim
  mitigation remains **raising the wake-word confidence threshold while active**. Real
  in-app AEC would require a true keep-playing-while-listening full-duplex redesign — see
  `Plan.MD` §4 / `TODO.md`.
- **On-device AEC shim — REQUIRED on every Echo Show 8 (1st gen, codename `crown`).**
  Echo cancellation is provided **outside this codebase** by a vendor audio-HAL shim
  `LD_PRELOAD`ed into `android.hardware.audio.service`. It taps the FPGA capture stream
  `pcmC0D22c` (6-ch 16 kHz — mics on ch0–3, a **sample-aligned DAC loopback** on ch4–5)
  and runs SpeexDSP linear AEC on the mic before `AudioRecord`/AudioFlinger ever see it,
  giving talker-preserving barge-in (~16 dB, measured). **Any Echo Show 8 gen-1 this app
  is deployed to must have the shim installed and activated** (`persist.vendor.amznaec.enable=1`,
  loaded via the audio-HAL init rc) — it is a **device prerequisite**, not part of this
  build, and survives reboots. Source + reversible adb install/uninstall:
  <https://github.com/Brutus-GTC6245/EchoShow8gen1-aec-shim>.
- Return to **IDLE** on playback completion, timeout, or reset.

### Wire format

- Newline-delimited (`\n`) JSON control frames for metadata and events.
- Raw audio chunks streamed immediately after the setup/metadata frame.
- The same socket carries outbound audio and inbound `transcript`, streamed
  `reply-token` (per-LLM-token text for on-screen rendering), and synthesized audio
  frames.
- **`anamanti-interrupt`** (device → Anamanti Core): a project-local barge-in frame that
  tells the Anamanti Core to abort the in-flight LLM generation + TTS at once, rather
  than only learning of the interruption when the socket drops.
- **`anamanti-timer`** (Anamanti Core → device): a project-local **device-action** frame
  (`data.action` = `start`/`cancel`/`adjust`; `duration_secs` or signed `delta_secs` +
  optional `label`). Emitted when an LLM **tool** the model called on the Mac
  (`set_timer` / `cancel_timer` / `adjust_timer`) asks to act on the device. The device
  owns the resulting state — unlimited concurrent countdowns, the on-screen UI, and the
  alarm — so a timer keeps running after the turn's socket closes and even if the Mac
  disconnects. An `adjust` carries only the signed delta (the device holds the
  authoritative deadline), which the device applies to the running timer's remaining
  time (floored at 1s, capped at the 24h max) and re-emits `timerStarted` to re-anchor
  the UI. When a timer fires it rings a loud
  two-strike **bell** locally, then (if the Mac is reachable) requests the spoken
  announcement over **`anamanti-speak`**; bell-only when offline. The alarm then
  **repeats every 5 s** (bell + the cached voice) until turned off — the display's
  "Cancel timer" button (FRB `dismiss_timer(id)`) or a voice cancel (an unlabeled cancel
  while one rings silences only the ringing timers). A ringing timer stays in the device's
  timer registry (so it counts as running in the turn context), and a repeat is skipped
  while a voice turn is in flight. See §8.
- **`anamanti-speak`** (device → Anamanti Core): a project-local frame (`data.text`)
  asking the Anamanti Core to synthesize the text with Piper and stream the audio
  (`audio-start`/`audio-chunk`…/`audio-stop`) straight back on the same socket. A
  fired timer uses it to voice "Time's up for {name}" in the real assistant voice; the
  device opens a fresh socket (same mDNS path a turn uses), outside any voice turn, and
  plays the returned audio through the shared `PlaybackSink` right after the bell.
- **`anamanti-recipe`** (Anamanti Core → device): a project-local **device-action** frame
  (`data.action` = `show`/`dismiss`; for `show`, `data.recipe` is the structured recipe).
  Emitted by the `recipe_lookup` tool; the device owns the 3-tab recipe screen until
  dismissed by voice (`close_recipe`) or touch. See `RecipePlan.md`.
- **`anamanti-weather`** (Anamanti Core → device): a project-local **device-action** frame
  (`data.action` = `show`/`current`/`dismiss`; for `show`/`current`, `data.weather` is the
  structured `WeatherReport` — `location_label`, `units`, `when_label`, `current{…}`,
  `hourly[10]`, `daily[7]`, `layout`). It
  rides **two transports**: `show`/`dismiss` on the per-turn voice socket, emitted by the
  `weather_lookup` / `close_weather` tools (`DeviceAction::{ShowWeather,DismissWeather}`),
  drive the **full-screen forecast** (a conditions panel + a 10-hour hourly row; the
  optional `weather_lookup` `when` arg targets a future day, whose row starts at 08:00 and
  whose panel shows that day's summary). The optional `weather_lookup` **`layout`** arg
  (`"week"`) instead selects a **separate 7-day forecast widget** (`SevenDayView`: the
  screen split into 7 vertical columns of daily high/low + icon + precip), chosen on the
  device by `WeatherData.isWeek` — same frame and `AssistantState.weather` slot, so no wire
  change. `current` is
  broadcast periodically on the persistent channel (below) by the Anamanti Core's
  `WeatherService` to refresh the **small icon + temperature beside the idle clock**
  without a voice turn. Data comes from **Visual Crossing** (default) or keyless
  **Open-Meteo** behind a `WeatherProvider` trait. Every provider built by
  `weather::from_config` is wrapped in a shared **`cache::ToolCache`** (a generic,
  TTL-bounded tool-response cache keyed on the outgoing call data — here `(location,
  units)`), so a repeated forecast within the TTL is served locally instead of re-hitting
  the API. The cache is process-wide, so the System-1 fast path, the `weather_lookup`
  tool, and the ambient push all share hits. **TTL is per tool type and configurable** via
  the `tool_cache` config block (`{ "<tool>": <seconds> }`, overlaid on defaults; `0`
  disables a tool's cache), installed process-wide at boot via `cache::set_config`;
  `weather_lookup` defaults to **3600 s (60 min)**. `ToolCache` is generic — its entries
  carry their own TTL, so other read-only tools can opt in at their own TTL; mutating tools
  (timers, shopping-list) must not. See `WeatherPlan.md`.
- **`anamanti-place`** (Anamanti Core → device): a project-local **device-action** frame
  (`data.action` = `show`/`dismiss`; for `show`, `data.place` is the structured
  `PlaceReport` — `name`, `address`, `hours[]`, `open_now`, `rating`, `phone`, `website`,
  `category`, `price_level`, `photo_uri`). Rides the per-turn voice socket only (no ambient
  push), emitted by the `places_lookup` / `close_places` tools
  (`DeviceAction::{ShowPlace,DismissPlace}` → `drain_device_actions` → `TurnUpdate::Place`
  → FRB `WakeWordEvent::{show,dismiss}_place`), driving the **full-screen place card**
  (`PlaceView`). Data comes from the **Google Places API (New)** behind a `PlacesProvider`
  trait (Text Search + Place Details; the hero photo is resolved to a **keyless** `photoUri`
  via `skipHttpRedirect=true`, so the device fetches it directly and the APK stays
  credential-free). The `GOOGLE_PLACES_API_KEY` secret is required — **no keyless fallback**,
  so without it the tool is not advertised. A place query is also a **route-only System-1
  `place` intent** (classified, then deferred to System-2 which owns the free-form
  place-name slot). See `PlacesPlan.md`.
- **`anamanti-camera`** (Anamanti Core → device) + **`anamanti-get-camera-stream`** /
  **`anamanti-camera-stream`** (device ⇄ Core control request): the **live NVR camera
  view** (`CameraPlan.md`). `anamanti-camera` is a device-action frame on the per-turn
  voice socket (`data.action` = `show`/`dismiss`; `show` carries only `data.camera`, the
  camera's display name — **never a URL or credential**), emitted by the pre-System-1
  **camera fast path** (`orchestrator/camera.rs`: "show the front door", "who's at the
  door", and — while the view is open — "stop"/"close it") or the `show_camera` /
  `close_camera` tools (`DeviceAction::{ShowCamera,DismissCamera}` → FRB
  `WakeWordEvent::{show,dismiss}_camera`, the name on the `recipe_action` carrier). On show
  the device asks the Core for the stream over the control socket; the reply carries the
  NVR's **RTSP sub-stream URL with the credential embedded** (`REOLINK_PASSWORD`, env-only),
  riding the LAN hop only and held **in memory only** on the device (never logged on either
  side). The device dials the NVR directly and plays the H.264 sub-stream full screen
  (`CameraView`) through a **vendored, patched `video_player_android`**
  (`anamanti-display/third_party/video_player_android`: RTP-over-TCP + a 0.5 s start
  buffer) in **platform-view** mode (the MTK decoder fails on Flutter's texture surface).
  Video only (no audio path); auto-closes after 60 s. Display context while open:
  `screen: {kind: "camera", camera: {name}}`. Because the frame goes down the speaking
  display's own turn socket, the view opens **only on the display the user is talking to**.
  No NVR polling/events (the automatic pop-up is deferred). Config is boot-time only
  (`camera{nvr_host, rtsp_port, username, cameras[{name, channel}]}`).
- **`anamanti-listen`** (Anamanti Core → device): a project-local **follow-up-listen**
  frame (`data.depth` + `data.wait_secs`). After **every** reply (gated by
  `follow_up.enabled`) the Anamanti Core sends this frame **just before** the turn's
  final `audio-stop` — it must precede that stop because the device ends its turn on the
  first `audio-stop`. It tells the device to **reopen the mic and start a fresh turn
  with no wake word** once the reply audio drains (see the follow-up bullet in §4's
  state machine). `wait_secs` is how long to keep the mic open for input before
  sleeping — **10 s after a question (`?`), 5 s otherwise** (`follow_up.question_wait_secs`
  / `reply_wait_secs`). The device **echoes `depth` + `wait_secs`** on the follow-up
  turn's `audio-start` (`followup: true`, `followup_depth`, `wait_secs`); the
  Anamanti Core uses `wait_secs` to size that turn's **no-speech VAD window** (the only
  place silence can be told from a user mid-sentence — the device runs no VAD), and
  `depth` against the optional `follow_up.max_chain` ceiling (**default 0 = unlimited**;
  the loop is normally ended by silence). On silence the Anamanti Core relays an empty
  transcript **and** an `audio-stop` so the device sleeps promptly, and sends no new
  `anamanti-listen`. A follow-up turn's LLM prompt is seeded with the **recent
  conversation history** (the last `follow_up.history_turns` turns within
  `follow_up.history_window_secs`, from the chat log); ordinary wake-word turns stay
  single-turn.
- **`anamanti-recipe`** (Anamanti Core → device): a project-local **device-action** frame
  driving the guided-recipe screen (`data.action` = `show` + `recipe` object /
  `dismiss` / `navigate` + `target` tab / `scroll` + `direction` / `step` + a 1-based
  `step` number). Emitted by the LLM **tools** the model calls on the Mac —
  `recipe_lookup` (fetch + parse → `show`), `close_recipe` (`dismiss`), and
  `recipe_control` (`navigate` to Overview/Ingredients/Steps, `scroll`
  up/down/top/bottom, or `go_to_step` → `step`, which shows + highlights that step; the spoken reply reads the
  step's text from the device-reported `steps[]`) —
  and by the System-1 `recipe_dismiss` / `recipe_step` fast paths. The device owns the resulting screen state —
  it persists across turns and idle while you cook — so, like timers, this is
  fire-and-forget from the Mac. See `plans/RecipePlan.md`.
- **`anamanti-font`** (Anamanti Core → device): a project-local **device-action** frame
  for the global on-screen font scale (`data.action` = `adjust`, `direction` =
  `increase`/`decrease`). Emitted by the LLM `adjust_font` tool (and, optionally later, a
  System-1 `font_increase`/`font_decrease` fast intent). The device owns + persists the
  scale (one device-wide `TextScaler` on all ambient text), so this is fire-and-forget. See
  `plans/FontScalePlan.md`.
- **Display context on `audio-start`** (device → Anamanti Core): a **general,
  extensible** mechanism that tells the Core **what the display is currently showing**, so
  the model can *drive that screen by voice*. The device stamps a `data.screen` block on
  every turn's `audio-start`, discriminated by a `kind` string, with a per-kind payload.
  The kinds that ship today:
  - **recipe:** `{kind:"recipe", recipe:{title, tab, at_top, at_bottom,
    ingredient_count, step_count, highlighted_step, ingredients[], steps[]}}` — the model
    can `recipe_control` (switch tab / scroll / go to a step) or `close_recipe`, and the
    recipe's lines (capped Core-side at 80 lines × 600 chars) are listed in the prompt so
    it answers recipe questions from what's on screen; System-1 `recipe_step` /
    `recipe_ingredient` use the same fields (highlighted step, ingredient lines).
  - **weather:** `{kind:"weather", weather:{location, units, temp, description}}` — the
    model knows the forecast is up and for where, so it answers follow-ups in context
    ("what about tomorrow" → `weather_lookup` for the same place) and closes it on "close
    it" (`close_weather`).
  - **place:** `{kind:"place", place:{name, address}}` — a place card is up, so the model
    answers follow-ups about it and closes it on request.
  - **diagnostics:** `{kind:"diagnostics"}` (no payload) — the **Audio Diagnostics**
    screen is foreground. Unlike the kinds above, this one does not enrich the turn; it
    **suppresses** it. The orchestrator still runs STT and relays the transcript to the
    device (so the recognized words show on the diagnostics screen), but then
    **short-circuits the turn** — no speaker ID, no System-1, no LLM, no TTS, no
    memory/chat-log — and sends a bare `audio-stop`. The screen is a diagnostic surface
    only: a voice tone it hears must be shown but never processed by the assistant. It
    reuses the ordinary wake-word turn (no second audio path).

  Absent on an idle screen (and the small weather clock chip is **not** a screen — only
  the full-screen forecast sets weather context). The orchestrator parses the block
  (`protocol::display_context` → a `DisplayContext` enum) and injects a one-line
  description into that turn's system prompt (`orchestrator::display_context_line`, one
  arm per screen) — except a **suppressing** context like `diagnostics`, which the
  orchestrator handles by ending the turn early (before any LLM work) rather than by
  contributing a prompt line. **Adding a new voice-controllable screen** (music, photos) is
  a `DisplayContext` variant + a prompt-line arm + a device-side `set_<screen>_context`
  setter — the transport is unchanged; an unknown `kind` is ignored, so a newer device
  never breaks an older Core.

  **How context rides with spoken input.** This is the **only device→Core context
  channel** and it *piggybacks on the turn's own `audio-start`* — the first frame the
  device sends when the user speaks — alongside the follow-up `depth`/`wait_secs` markers.
  There is no separate context uplink and no conversation "context window" pushed ahead of
  time: what the user is looking at travels *with* their utterance, so the model always
  sees the exact on-screen state for the turn it is answering, and nothing goes stale
  between turns. (The other half of a turn's context — long-term memory recall and, for a
  follow-up turn, recent chat history — is assembled **Core-side** into the same system
  prompt; the display-context line is appended to it.) On the device, Rust holds the
  current block in a global slot (`engine::display_context` / `set_display_context`) set by
  the FRB layer (`set_recipe_context` / `set_weather_context` / `set_place_context` /
  `set_diagnostics_context`) whenever a screen opens, closes, or changes, and stamps it in
  `WyomingConnection::send_audio_start`.

### Proactive notifications (Approach A — persistent device-dialed channel)

Every frame above rides a socket the **device** opened for a voice turn, so the
Anamanti Core can only ever *reply*. Proactive notifications add the one path where the
Mac reaches the display **unprompted** — a meeting reminder, an alert — without the
user speaking first. The chosen design keeps the device as the dialer (no reverse
connection, no device-side listener, nothing new for NAT/firewalls, and the same
mDNS + `instance_id` pin the voice path uses):

- The device opens a **second, long-lived Wyoming connection** to the pinned
  Anamanti Core and holds it open, reconnecting with capped exponential backoff
  (`rust/src/wyoming/notify.rs` over the shared `push_channel.rs` driver; started by
  `start_notify_channel` on its own thread, independent of the voice engine). This is a *sidecar* — the per-turn voice socket and
  its state machine are untouched.
- **`anamanti-hello`** (device → Anamanti Core): sent right after the notify socket
  opens (`data.role="notify"`, `data.device_id`, `data.name`). It registers the connection
  with the Anamanti Core's `NotificationService`, which parks the read loop and holds the
  socket to push down. `device_id` is a **stable, globally-unique** identifier the display
  mints once from its Wi-Fi MAC (`anamanti-<12 hex>`, e.g. `anamanti-140ac5942aca`; a
  persisted random id is the fallback when the MAC can't be read — `api::engine::device_hardware_id`),
  so two displays on one Core are distinguishable; `name` is the human-friendly label set on
  the device's Settings screen. The Core stores both per connection and lists the connected
  displays (id + name) on the config page's **Notify** tab (`GET /notifications/status.json`
  → `devices[]`). (Older devices that predate the field simply send no `name`.)
- **`anamanti-notify`** (Anamanti Core → device): a proactive notification
  (`data.id`, `data.priority` = `info`|`reminder`|`alert`, `data.title`, `data.body`).
  The device decodes it to a `NotifyEvent` on a dedicated FRB stream; Flutter's
  `NotificationController` shows a banner on the idle screen (top layer, tap or
  auto-expire to dismiss). **Visual-only in this phase** — no spoken output, no
  state-machine interaction.
- **`anamanti-notify-ack`** (device → Anamanti Core): reserved for a later
  delivery-tracking phase (store-and-forward across reconnects); the constructor exists
  in the shared `anamanti-protocol` crate but nothing sends it yet.
- **`anamanti-timers`** (device → Anamanti Core, up this same notify socket): the device's
  **full** timer list (`data.timers[]` = `ReportedTimer {id, label, duration_secs,
  remaining_secs, ringing}`; empty = none), sent right after the hello on every (re)connect
  and again on each start/adjust/cancel/fire/dismiss, so the Core can show every display's
  timers. `engine/timer.rs` signals a `watch` on each mutation and the push driver's optional
  `Outbound` rebuilds the frame at send time; weather/music channels send nothing up.
- The **ambient weather push** reuses this same persistent-channel design: the device
  opens a *third* long-lived connection (`rust/src/wyoming/weather.rs`, started by
  `start_weather_channel`) with an `anamanti-hello` carrying `data.role="weather"`, which
  the server registers with the `WeatherService` instead of the notify registry. The Core
  then pushes `anamanti-weather` `current` frames down it on a timer; the device surfaces
  them as a `WeatherPush` FRB stream that refreshes the clock's weather indicator. Same
  dialer/backoff/pin model as notify — see `WeatherPlan.md`.
- The **music now-playing push** reuses the same design again: the device opens a *fourth*
  long-lived connection (`rust/src/wyoming/music.rs`, started by `start_music_channel`)
  with `anamanti-hello` `data.role="music"`, which the server registers with the
  `NowPlayingService`. A periodic task reads the current Spotify playback + up-next queue
  (`SpotifyController::now_playing`) and pushes `anamanti-music` `now_playing` / `dismiss`
  frames down it; the device surfaces them as a `MusicPush` FRB stream folded into
  `AssistantState.music`, rendering the compact control overlay + the `NowPlayingView` /
  `NextUpView` full screens. On-screen controls go the other way as a short-lived
  **`anamanti-music-control`** request/response (a control frame like the settings/memory
  ones, handled by `control::music_control_response` → the `spotify_control` controller;
  includes `skip_to` + an up-next index from a tapped `NextUpView` row),
  **not** on the persistent channel. Gated behind `music.enabled` + a linked Spotify; see
  `MusicPlan.md` (P6) and `DisplayUI.md` §4b.
- All three persistent channels share one implementation per side: on the device,
  `rust/src/wyoming/push_channel.rs` owns dial → hello → read → backoff (each channel
  module only supplies its hello frame + a frame decoder) and `api/engine.rs` has one
  `start_channel`/`stop_channel` pair behind the per-channel FRB entry points; on the Core,
  each service wraps the generic `notify::ChannelRegistry`, and `server.rs` dispatches the
  hello `role` via `PushChannel`. The frames themselves live once in `anamanti-protocol`.
- The music screen is also **voice-controllable** via the "Display context" mechanism
  below: the device stamps a `DisplayContext::Music` block (`set_music_context`) on
  `audio-start` when a music screen is open, and the Core's `music_screen` rig tool emits a
  `DeviceAction::MusicScreen` relayed as an `anamanti-music` `screen` frame on the per-turn
  socket (show now-playing / up-next / close). Playback transport stays on `spotify_control`.
- **Music display style** ([`MusicDisplayStylePlan.md`](./MusicDisplayStylePlan.md)): which
  widget shows while music plays (compact bar / full screen) is decided **per display** by
  the Core and stamped as that display's `display_style` on its own `now_playing` push
  (`NowPlayingService` records each music channel's `device_id` and pushes with
  `ChannelRegistry::broadcast_with`). Resolution (`MusicDisplayRouter::style_for`): the
  display's choice for the current music (its expand/minimize button → `anamanti-music-control`
  `display_fullscreen`/`display_compact`; or voice to it → `music_screen`
  `fullscreen`/`compact`, also relayed as a `screen` frame) → its persisted pin
  (`SharedSettings::music_display_pins`, config page) → full screen on the session's
  initiator (the display whose voice turn ran `spotify_control`/`radio_control` `play`),
  compact elsewhere. To know who asked, the display stamps its `device_id` on every
  `audio-start` (→ `DeviceContext.device_id` → `LlmTurn` → `ToolCtx`) and every
  `anamanti-music-control`. Music stopping ends the session. The device resolves what's on
  screen in `AssistantState.effectiveMusicScreen` (Core mode > explicit view > style, the
  style only while someone is present).
- **Internet radio** ([`RadioPlan.md`](./RadioPlan.md)) rides the same push. A curated
  station catalog (settings overlay `radio.stations`, edited on Music & Devices) is played
  by the existing mpv web player into the snapserver `Web` stream; the Core's
  `music::radio::RadioPlayer` loads the URL over mpv IPC and points every snapserver group
  at `Web` (`Group.SetStream`), and back to `Spotify` on stop — PCM still never enters the
  Core. While a station is on, `NowPlayingService` pushes a radio snapshot (`source:"radio"`,
  station id/name, logo as `artwork_uri`, the stream's ICY title as the song) and wakes on
  every station change; `anamanti-music-control` transport acts on the radio
  (pause = stop, next/previous = station, volume = mpv volume). A `RoutedSpotify`
  decorator wraps the Spotify controller so the sources hand off: Spotify transport drives
  the radio while it plays, and any Spotify play stops it first; a Spotify start seen by the
  poll (e.g. from a phone) also stops it. Voice is the `radio_control` rig tool.

Producers enqueue via `NotificationService::notify`, which fans a notification out to
every connected device and prunes dead channels. The first producer is the config
page's **Notify** tab (`POST /notifications/test`); if no device holds a channel open
the notification is simply dropped (delivered-count 0). Because the channel is pinned
to the selected `instance_id`, a pinned display only accepts pushes from its Mac.
Deferred (see `TODO.md`): spoken notifications + barge-in, ack/store-and-forward/TTL,
per-device targeting, quiet hours, and a paired/TLS control channel (the LAN hop is
currently unauthenticated, so this widens the same attack surface the voice/control
frames already have).

### In-app APK auto-updater (Cloudflare R2)

Separate from Wyoming entirely: the Display can update **itself** over HTTPS from a
Cloudflare R2 bucket, instead of relying on the third-party Obtainium app. Full
design + release runbook: [`UpdaterPlan.md`](./UpdaterPlan.md).

- **Rust owns the network half** (behind FRB, like all networking):
  `check_for_update(base_url)` GETs `{base_url}/latest.json`
  (`{versionCode, versionName, apkUrl, sha256, notes}`); `download_update(...)`
  streams the APK to the cache dir on a dedicated thread, computing a **SHA-256** as
  bytes arrive and deleting the file on mismatch/cancel, emitting `DownloadProgress`
  over a `StreamSink` (`rust/src/update/`, `rust/src/api/updater.rs`). The client is
  **`ureq` + rustls/`ring` + `webpki-roots`** — this is the project's **one HTTPS
  client**; it does not touch the Wyoming TCP path. This is the only place the device
  reaches a host *other* than its pinned Core, over plain HTTPS to a domain the owner
  controls, integrity-pinned by the SHA-256.
- **The install is native Kotlin**, since `PackageInstaller` /
  `canRequestPackageInstalls` have no Rust or Dart equivalent: a minimal
  `anamanti_display/updater` MethodChannel on `MainActivity` (versionCode,
  unknown-sources redirect, `PackageInstaller` session) + an `InstallReceiver` that
  launches the system prompt and reports the result back. This is the second
  platform channel (alongside `anamanti_display/brightness`); it is kept minimal and
  the networking stays in Rust, consistent with the interop boundary (§3).
- **Flutter owns orchestration + presentation**: `UpdateController` (a
  `ChangeNotifier` sidecar) checks on boot + every 6 h, compares versionCodes,
  drives the `UpdateBanner` and the Settings → Device → Updates tab, and triggers
  download/install.
- **Build flavors** `selfUpdate` (ships it) / `fdroid` (ships without it) keep the
  same signing key + `applicationId`; the key must never change (a self-update needs
  a matching signature). See key design decisions (§7) and `agents.md`.

---

## 5. Discovery & networking

- **mDNS / Zeroconf**: the device browses `_wyoming._tcp`, resolves the Mac's
  host + port, and caches it for fast reconnect.
- No static IP configuration required.
- **Anamanti Core TXT records**: each Anamanti Core advertises TXT
  `role=core`, a friendly `name` (from `ANAMANTI_SERVICE_NAME`), and a
  **stable** `instance_id`. The `instance_id` resolves from `ANAMANTI_INSTANCE_ID` →
  the working directory's **git branch code** (so a copy run from a test
  branch/worktree names itself by its branch) → the sanitized service name. The
  device **filters browse results by `role=core`**, so the raw
  off-the-shelf Whisper (STT) and Piper (TTS) servers — which also advertise
  `_wyoming._tcp` — are never resolved or connected to.
- **Deployment layout**: the "local production" Anamanti Core is a copied release
  binary at `/Volumes/External/Anamanti Core/`, run outside
  any git checkout with `instance_id` (`anamanti-core`) pinned in its `anamanti.json` and
  provider keys persisted in its `anamanti_settings.json` (not the environment); test copies run
  from their git worktree and fall through to the branch-code default.
- **Multiple displays → one Anamanti Core**: the Anamanti Core's accept loop already
  spawns a task per connection and every reply is written back on that
  connection's own socket, so N displays share one Anamanti Core with no
  cross-routing. Memory + settings are a single shared household pool (speaker ID
  scopes per person, not per device).
- **Multiple Anamanti Cores → device picks one**: a display can run a "production"
  Anamanti Core plus short-lived test instances (each launched with a distinct
  `ANAMANTI_SERVICE_NAME` / `ANAMANTI_INSTANCE_ID` / `ANAMANTI_BIND_ADDR` /
  `ANAMANTI_CONFIG_ADDR`; give each a distinct `service_name` — on macOS
  `mDNSResponder` auto-renames a colliding instance, but the device still keys off
  the stable `instance_id` TXT regardless). The settings screen shows a device-local **Anamanti Core** dropdown
  (populated by a full-window mDNS enumeration, `list_orchestrators`), persisted
  by stable `instance_id` key in `AppSettings`. The selection is **strict**: a
  display pinned to one Anamanti Core resolves *only* that `instance_id` and stays
  **offline** if it is unreachable, never silently switching Macs; the `"Auto"`
  entry (empty key) restores first-responder behavior. The key steers **both**
  discovery paths — voice turns (`WakeWordConfig.orchestrator_key` → `Shared` →
  `resolve(preferred)`) and the settings/control calls (`api/settings.rs` →
  `control` → `resolve(preferred)`) — so both hit the same Mac. Shared backend data
  across instances relies on SQLite **WAL** mode; two processes writing the same
  embedded HelixDB graph is unsupported, so a test instance sharing prod data uses
  `ANAMANTI_MEMORY_BACKEND=sqlite` (see `Plan.MD`).
- **Resilience**: **auto-reconnect** with exponential backoff via mDNS when the
  Mac is unreachable. The idle photo slideshow keeps running; a subtle
  **disconnected** indicator reflects status; wake words queue until the socket
  is restored.
- **Advertisement backend — OS `mDNSResponder` on macOS**: the Core registers
  `_wyoming._tcp` through Apple's **system `mDNSResponder`** via `DNSServiceRegister`
  (the `astro-dnssd` wrapper in `anamanti-core/src/discovery.rs`). The OS daemon —
  not the Core process — owns the live advertisement: address records, multicast-
  group membership, sleep/wake recovery, interface-change refresh (Wi-Fi↔Ethernet
  failover, DHCP renew, en0↔en1), and name-collision probing/auto-rename. This
  replaced a hand-rolled in-process pure-Rust responder (`mdns-sd`) that had to pin
  a single routable IPv4 and re-register on `IpAdd`/`IpDel` monitor events within a
  ~30 s interface-poll window, and could not reliably rejoin multicast groups after
  the Mac slept — so an overnight sleep/wake left a zombie record that answered
  nothing until the Core was **restarted** (the recurring "mDNS bites daily" failure).
  Delegating to `mDNSResponder` removes that whole class of staleness. *(Off macOS —
  dev/CI only — the crate falls back to the old `mdns-sd` pinned-IPv4 advertiser.)*
- **Device picks a reachable address**: because `mDNSResponder` advertises *all* of
  the host's addresses (it no longer pins one IPv4), a resolve returns the full set —
  including loopback and IPv6 link-locals (`fe80::…`). The device's browser
  (`endpoint_from` / `choose_address` in `anamanti-display/rust/src/wyoming/discovery.rs`)
  skips loopback/unspecified/link-local entries and prefers a routable IPv4 over
  global IPv6, so it always dials a reachable endpoint instead of an arbitrary set
  member — the correctness the Core-side single-IPv4 pin used to guarantee.

---

## 6. Resource & performance constraints

- **RAM (~1 GB on the Echo Show):** pre-allocated ring buffer; wake-word model
  kept small; avoid per-frame heap churn; single audio engine for capture +
  playback.
- **Latency:** wake word runs on-device; audio streams (not batched); reply tokens
  render as they arrive; **TTS is synthesized and streamed sentence-by-sentence** so
  playback starts after the first sentence, not the whole reply.
- **Privacy:** no audio leaves the device until the wake word fires.

---

## 7. Key design decisions

| Decision | Rationale |
| --- | --- |
| Rust owns all audio + networking | Deterministic memory, no GC, fits 1 GB budget |
| FRB v2 stream sinks over polling | Push-based, smooth real-time UI updates |
| Wyoming Protocol | Standard, streaming-friendly, pairs STT + Piper TTS cleanly |
| mDNS discovery | Robust to IP changes; zero manual config |
| Device-selectable Anamanti Core (TXT `instance_id`, strict offline) | Multi-Mac homes (prod + test instances); pin by stable key so a display never silently switches Macs and survives IP changes; `role=core` filtering also excludes raw STT/TTS servers |
| Multiple displays share one Anamanti Core | Per-connection reply routing already isolates devices; one shared household memory/settings pool (speaker ID scopes per person) |
| openWakeWord via tract-onnx | Pre-trained models, minimal deps, offline |
| Pluggable LLM behind a trait | Swap local/cloud without touching the pipeline |
| Optional System-1 fast-decision stage (pluggable, before recall+LLM) | Resolves common intents in one non-autoregressive forward pass, skipping the blocking embedding recall and the rig+tools full-completion; defers hard turns to System-2. Same trait pattern as the LLM; shared `/v1/systemone` contract serves local `laya-serve` (default) or Jev/OpenRouter (fallback); default off. See [`system1-fast-decisions.md`](./system1-fast-decisions.md) |
| Rust-side playback | One audio layer, symmetric with capture |
| Wake-word barge-in (flush-on-wake + `anamanti-interrupt`) | Natural interruption without full-duplex complexity; in-app AEC deferred, but a **required device-side HAL AEC shim** delivers echo cancellation on Echo Show 8 gen-1 (see §4) |
| Streaming sentence-chunked TTS | First-audio at first-sentence latency, not full-reply; coalesced to one device audio stream |
| VAD in the Anamanti Core (pluggable `SpeechGate`) | Device does no VAD; neither STT engine does streaming VAD, so the Mac runs its own VAD and finalizes the transcriber — **neural Silero gate by default since 2026-09-30** (`vad-silero` feature on, needs the v4 model on disk), energy/RMS gate the opt-in fallback (`plans/VadSileroPlan.md`) |
| STT engine behind a `Transcriber` seam (`wyoming` \| `whisper-rs`) | Default dials `wyoming-faster-whisper`; `whisper-rs` runs whisper.cpp **in-process** (no Python STT server) for deploy simplicity — same Whisper-class text, engine chosen by config (`plans/python-to-rust-whisper.md`) |
| SQLite is the memory store of record | Simple, debuggable; holds explicit+inferred facts and the settings-list/voice management |
| Recall defaults to embedded HelixDB GraphRAG | Vector KNN + graph hop beats keyword FTS for context; in-process (no server/Docker); needs `OPENAI_API_KEY`, falls back to SQLite FTS if absent |
| Per-person speaker ID (local, opt-in) | Local ECAPA voiceprint (passive + auto-cluster) keeps voice on the LAN and scopes memory + prompt per person for better context; no raw audio leaves the device |
| On-device OAuth for photos | Device displays directly; no Mac proxy needed |
| Auto-reconnect + status | Robust to Mac downtime; slideshow stays up |
| Idle photo slideshow (Google) | Ambient value when idle; user picks the folder |
| Proactive notifications via a persistent **device-dialed** channel (Approach A) | Mac reaches the display unprompted while keeping the device the dialer — reuses the existing mDNS + `instance_id` pin and the blessed auto-reconnect model; avoids a reverse connection / device listener / new trust direction. A doorbell (device advertises, Mac nudges) was considered but only wins idle-socket cost, which is free on a mains-powered display, at the price of lossy triggers. Shipped visual-only first (see §4) |
| One shared Wyoming protocol crate (`anamanti-protocol/`) | Core and Display previously each kept a copy of the framing + ~58 frame constants; one crate makes a wire change a single edit, golden-bytes tests pin the encoding, and the framing reader caps header (64 KiB), `data` (4 MiB) and `payload` (8 MiB) sizes so a LAN peer can't force a huge allocation. Both sides re-export it as `crate::wyoming::protocol`; cargokit builds the device crate in place, so the path dependency works on device builds |
| LLM tools are a name-keyed `VoiceTool` registry built from `ToolDeps` (`llm/rig/tools/`) | One place per tool (definition + guidance + invoke); settings rebuild the whole tool set from one function (`LlmFactory::for_settings`) so saving one setting can never drop another tool. Every rig round streams (tool calls are detected mid-stream), and every rig backend — incl. Anthropic subscription OAuth and OpenAI — gets tools + follow-up history; the `native` engine remains as an explicitly capability-limited fallback |
| Admin console is loopback-only (Host/Origin + JSON content-type checks) | Settings, secrets and OAuth flows are reachable from it, so cross-site POSTs and DNS rebinding from a browser on the Mac are rejected (403/415) centrally before routing |
| In-app APK auto-updater over Cloudflare R2 (Rust download/verify + native `PackageInstaller`), with `selfUpdate`/`fdroid` flavors | First-party, hands-off updates for a headless kiosk without depending on the third-party Obtainium app; networking stays in Rust (ureq+rustls, the one HTTPS client) and only the unavoidable install is a native channel; SHA-256 pins integrity; flavors let F-Droid ship without the self-updater. Keep Obtainium in parallel until hardware-proven. See §4 + [`UpdaterPlan.md`](./UpdaterPlan.md) |

---

## 8. Extending the assistant with tools & device actions

The assistant does more than chat: the LLM can call **tools** to fetch information
and to take **actions** on the device. This section is the canonical process for
adding a new capability.

### 8.1 How tool calling works

Tool calling lives in the **rig engine** (`anamanti-core/src/llm/rig/`), the default LLM
engine (`ANAMANTI_LLM_ENGINE=rig`; `native` opts out to the tool-less HTTP backends).
Each turn the Anamanti Core advertises a set of **tool definitions** (name +
description + JSON-schema for the arguments) to the model. If the model asks to call
one, `RigBackend::respond` runs a short **tool-negotiation loop** (up to
`MAX_TOOL_ROUNDS`): it executes the tool, appends the result to the conversation, and
completes again — until the model answers with no further tool calls. Because a live
`ollama` streaming parser drops non-final tool calls, tool turns use a **non-streaming
completion**; the reply is still spoken whole (and rendered) via the sentence-chunked
TTS path, so there is no user-visible regression.

There are **two flavors** of tool:

Every tool implements the crate-local **`VoiceTool`** trait (`llm/rig/tools/mod.rs`): a
`name`, the advertised `definition()`, optional preamble `guidance()`, and an async
`invoke(args, ctx)`. **`Tools`** is a name-keyed, insertion-ordered registry of them —
the advertised definitions, the guidance text and the dispatcher all derive from that
one list. Tools are grouped by module: `device.rs` (timers, personality, font),
`display.rs` (recipe / weather / places screens + their `close_*` tools, and the music
player's `music_screen`), `services.rs` (calendar, directions, Spotify, shopping list,
phone) and `search.rs` (`internet_search`).

- **Info tool** — runs entirely on the Mac and returns text the model speaks
  (e.g. `internet_search`, `calendar_lookup`). Adding one is self-contained in
  `llm/rig/tools/`, though it may lean on a supporting module: `calendar_lookup` (read-only
  web iCalendar) lives in `anamanti-core/src/calendar/`, holds an injected
  `Arc<dyn CalendarSource>` (mirroring how `internet_search` holds a
  `SearchProvider`), and is advertised only when `ANAMANTI_CALENDARS` lists one or
  more `.ics` subscription URLs. It fetches the feeds live per query (with a short
  TTL cache to dedupe fetches inside one tool-negotiation loop), expands recurring
  events (`RRULE`) within the requested window, and filters by time / person
  (fuzzy name match) / free text. A CalDAV or macOS-EventKit source can drop in
  later behind the same `CalendarSource` trait. The **`shopping_list_add`** tool has
  the same shape but acts on an external service: it holds an injected
  `Arc<dyn GroceryController>` (`anamanti-core/src/cadora/`) and, when the household
  shopping list is linked, POSTs the add to **Cadora's voice API** (the shared family
  backend behind NextHaul et al.) and speaks back its confirmation — mirroring how
  `spotify_control` holds a `SpotifyController`. Linking: NextHaul (Settings → Voice
  & Integrations) mints a one-time **6-digit code** (`/voice/links/pair`); enter it on
  the config page (Household tab) and the Anamanti Core redeems it
  (`/voice/links/redeem`) for a durable voice-link token stored in the 0600 settings
  file. It only ever calls Cadora's API (`https://api.getcadora.com/cadora`, on
  Cloudflare Workers) — Cadora owns list creation, member attribution, and dedupe.
  The **`calendar_add`** tool shares that `Arc<dyn CadoraController>` and link: it
  resolves the spoken day/time to RFC 3339 in the Core's local zone
  (`cadora::resolve_event_times`) and sends Cadora's `add_event` voice action, relaying
  the server's confirmation (one-off, household-visible events). The **`saved_recipes`** tool (2026-10-27) reads the family's **saved recipes** from
  Cadora and/or NextHaul and fills the same 3-tab recipe screen (`DeviceAction::ShowRecipe`)
  as `recipe_lookup`. Those reads need a **user bearer token**, not the voice link, so the
  Core holds its **own identity-api user** (`anamanti-core/src/cadora/apps.rs`,
  `HouseholdApps`): linking an app on Integrations → Cadora / NextHaul signs in
  anonymously once (`POST /identity/auth/anonymous`) and redeems the household's 6-digit
  **family invite code** (`POST /identity/households/join`), so the Core becomes a
  household member named "Anamanti". Linking Cadora also mints the Core's own voice link
  (`POST /cadora/voice/links`), so one code covers recipes, shopping and the calendar. The
  15-minute access token is refreshed through `/identity/auth/refresh`; refresh tokens are
  single-use, so **one hub per process** (built at boot, shared by every LLM rebuild like
  the Home Assistant hub) serializes refreshes and persists each rotation to the 0600
  settings file via an identity sink (no LLM rebuild). Recipes come from each app's
  offline-first sync (`GET /{cadora|nexthaul}/sync/pull`, an incremental in-memory cache of
  `recipes`/`recipe_items`/`list`/`list_item`), plus Cadora's community/global tiers
  (`GET /cadora/recipes/discover`) and full Cadora detail (`GET /cadora/recipes/detail`).
  `shopping_list_add` now holds an `Arc<dyn ShoppingList>`: the NextHaul list
  (`POST /nexthaul/sync/push`, the app's oldest list, bumping a live duplicate's quantity)
  or the Cadora list (voice API), per the `cadora.shopping_app` preference (auto =
  NextHaul when linked); `saved_recipes add_ingredients_to_list` uses the same list, and
  **`shopping_list_read`** reads it back (oldest list by default, or a named list). The **`send_phone_message`** tool has the same external
  shape: it holds an injected `Arc<dyn PhoneMessenger>` (`anamanti-core/src/appsaid/`)
  and pushes a message — often a Google Maps link — to a named family member's phone
  via **AppSaid** (a self-hosted HTML-push worker), one `POST {worker_url}/v1/messages`
  per recipient. It resolves the spoken name to the recipient's AppSaid `user_key`
  (from `appsaid.recipients` in the config), builds a Maps link from a `place` when no
  explicit `url` is given, and speaks back a short confirmation. The sender app token is
  a secret (`APPSAID_APP_TOKEN` / config page, 0600), never in the JSON config; the tool
  is advertised only when the worker URL + app token + at least one recipient are all
  present, and relinks/withdraws live like `spotify_control`.
  **Recipients are unified on the household.** Each person lives in the household roster
  (`HouseholdMember`) with an **`appsaid` address** (their `user_key`) alongside their
  emails/phones; the recipient roster is **derived** from members that have one (the
  household is the single source of truth). Set it two ways: paste it per-person on the
  config page → Household tab, or link it from the device.
  **Recipient pairing (TV-style, device).** The family member opens the AppSaid app →
  "Link another device" to show a 4-character code, and on the Echo Show, **Settings →
  Link a phone** (`PairPhoneScreen`) takes that code plus a name. The device sends both to
  the Core over a control round-trip (**`anamanti-pair-phone` → `anamanti-pair-result`**, a
  clone of `anamanti-get-drive-token` handled in the async `control::handle_control`); the
  Core redeems the code (`appsaid::claim_pairing_code` → `POST {worker_url}/v1/pair/claim`),
  learns the phone's `user_key`, and stores it on the matching household member via
  `SharedSettings::add_appsaid_recipient` — **auto-adding the person to the household** if
  no member matches, so a paired contact is always in the roster. A household edit
  re-derives the roster and rebuilds the tool (so `apply_household` now rebuilds the LLM,
  like `apply_appsaid`). The claimed `user_key` never rides back to the device; only an
  `ok`/`message` result does. A legacy `appsaid.recipients` config seed is folded into the
  household at boot.
- **Action tool** — additionally causes an effect on the Echo Show (e.g.
  `set_timer`). The tool cannot reach the device directly (it runs inside the LLM
  loop, which has no socket), so it emits a **`DeviceAction`** onto a per-turn
  channel; the Anamanti Core's reply loop drains that channel and writes a
  project-local **`anamanti-*`** frame to the device on the same turn socket. The
  device owns whatever state results.

### 8.2 Adding an **info** tool (Mac-only)

1. In the matching `anamanti-core/src/llm/rig/tools/<group>.rs`, add a `NAME` const, a
   `Deserialize` args struct (parse with `parse_args`), and a struct implementing
   `VoiceTool`: `definition()` (built with `definition(NAME, description, schema)`),
   optional `guidance()` telling the model when to call it, and `invoke()` returning a
   short, speakable result. Inject any provider (`Arc<dyn …>`) as a field.
2. Register it in `Tools::build` (`tools/mod.rs`), gated on its provider being present
   in `ToolDeps`. That single registration advertises it, adds its guidance, and routes
   calls to it — there is no separate dispatcher. If its guidance should sit at a
   particular place in the preamble, add `NAME` to `GUIDANCE_ORDER`.
3. Test it next to the tool (a unit test calling `invoke` with a `ToolCtx`), and
   regenerate the advertised-tools snapshot deliberately:
   `ANAMANTI_UPDATE_SNAPSHOTS=1 cargo test … advertised_tools`, then review the diff of
   `src/llm/testdata/advertised_tools.json` (it is exactly what the model sees).

No protocol, device, or Flutter changes are needed — build + test with
`cargo test --manifest-path anamanti-core/Cargo.toml`.

### 8.3 Adding a **device action** (spans all layers)

Worked example: **timers** (`set_timer` / `cancel_timer` / `adjust_timer`). The flow is
model → Mac tool → `DeviceAction` → `anamanti-timer` frame → device timer manager →
FRB event → Flutter UI. To add a new action, mirror these steps (note `adjust_timer`
reuses the existing `timerStarted` FRB event to re-anchor the UI rather than adding a
new one — see step 6's "prefer an existing payload" note):

1. **Mac tool** (`anamanti-core/src/llm/rig/tools/`): as in §8.2, but `invoke` emits a new
   `DeviceAction` variant (`anamanti-core/src/llm/mod.rs`) on the per-turn sink with
   `emit(ctx.actions, action, …)` instead of doing the work locally (`ctx.require_sink`
   first if the tool would otherwise fetch for a display that isn't there). Register it
   unconditionally if it needs no config, or alongside the integration it drives (e.g.
   `music_screen` is registered wherever `spotify_control` is).
2. **Relay** (`anamanti-core/src/orchestrator/speech_out.rs`): the reply loop already drains the per-turn
   action channel (`drain_device_actions`) and writes the frame; add a match arm
   mapping the new `DeviceAction` to its `WyomingEvent` constructor.
3. **Wire frame** (the shared `anamanti-protocol` crate, re-exported by both `anamanti-core/src/wyoming/protocol.rs` and `anamanti-display/rust/src/wyoming/protocol.rs`,
   one definition with golden-bytes round-trip tests): add an `anamanti-*`
   `type` const, constructor(s), and — on the device side — a decoder
   (e.g. `timer_command()`).
4. **Device intake** (`anamanti-display/rust/src/wyoming/client.rs`): add a `TurnUpdate` variant and a
   branch in `handle_server_event` that decodes the frame into it. The frame only
   arrives **mid-turn** (the device is always the Wyoming *client*; the socket exists
   only during a turn), which is fine — device actions are triggered by the very turn
   that carries them.
5. **Device state** (`anamanti-display/rust/src/engine/`): own the resulting state in a manager held by
   the long-lived `Network`/`Shared` (so it outlives the turn socket), applied from
   the `on_update` closure in `net.rs`. Reuse the shared `PlaybackSink` for any sound
   (no second audio path). See `anamanti-display/rust/src/engine/timer.rs`.
6. **FRB event** (`anamanti-display/rust/src/api/engine.rs`): add `WakeWordEventKind` variant(s)
   plus `pub(crate)` builders. **Prefer an existing payload field** before adding one: a
   short string argument rides the generic `recipe_action` carrier (as `RecipeScroll`,
   `FontAdjust` and `MusicScreen` do) — every new field on the flat `WakeWordEvent`
   ripples into every Dart test that builds one (build those with
   `test/support/wake_word_events.dart`'s `wakeEvent(kind, …)` so a new field is one
   edit). Add a neutral-default field only for a genuinely new payload (e.g. a JSON blob); the manager emits them via the `StreamSink`. Then **regenerate**:
   `flutter_rust_bridge_codegen generate` (never hand-edit `frb_generated.rs` or
   `anamanti-display/lib/src/rust/`).
7. **Flutter** (`lib/`): fold the new event kind into `AssistantState` in
   `AssistantController._onEvent` (the exhaustive `switch` forces you to handle it),
   and render it. See `anamanti-display/lib/src/ui/timers_overlay.dart`, mounted in the always-visible
   `Stack` in `ambient_screen.dart` so it shows during both idle slideshow and a live
   turn. Timers render in two presentations: a big, screen-filling display on the idle
   screen (single / side-by-side / grid by count, each with a name, large mm:ss readout,
   and a draining circular ring), and a compact chip row top-center while a turn is active,
   so the conversation wins the screen and the timers stay glanceable.

Build/verify each layer independently: `cargo test` for `anamanti-core/` and `anamanti-display/rust/`,
`flutter test` + `flutter analyze` for the UI.

### 8.4 Output personalities (Core-side, pure output tuning)

**Personalities** change only how the assistant's replies *sound* — tone, word choice,
delivery — never what it knows, remembers, or which tools it calls. The feature is an
on/off switch plus a selectable style, with the catalog of styles editable from the Core
GUI. It is deliberately a **Core-only** concern: nothing crosses the Wyoming boundary and
there is no Display UI.

- **State + catalog** live in the settings overlay (`Personality` in
  `anamanti-core/src/settings.rs`): `enabled`, `active` (a key; empty/"normal" = off), and
  `definitions` (the editable `{key,label,description}` catalog, seeded with the built-ins
  — *overly enthusiastic, heavy sarcasm, depressed, 1940s gangster, Donald Trump*). It is
  part of `PersistedSettings`/`RuntimeSettings` (persisted to `anamanti_settings.json`,
  read from the per-turn snapshot) and has **no `anamanti.json` seed** — it's a pure
  runtime/GUI concern. Changing it **never rebuilds the LLM backend** (like `household`).
- **Prompt injection** (`orchestrator/prompt.rs`, `personality_prompt`): during prompt assembly
  the snapshot's active personality is appended as a **trailing, fenced style block** —
  "deliver your reply in this voice; this changes ONLY tone… never facts, memory, or
  tools". A short always-present line lists the available personality names + the current
  one so the model can switch by voice. Both read from the live snapshot, so a config-page
  or voice change takes effect on the next turn. Nothing is written to memory/recall.
- **Voice control, two layers** — both end at the same `SharedSettings::set_active_personality`
  (resolving the name against the live catalog, "normal"/"off" disabling), so a change takes
  effect on the next turn's snapshot with no LLM rebuild and no device frame:
  1. **Deterministic fast path** (`orchestrator::detect_personality_switch`), checked
     *before* the LLM. It fires only on a switch **trigger** phrase (`talk like`, `switch
     to`, `become`, `personality`, `turn off`, …) **and** either a catalog name (key/label,
     case-insensitive, longest-first) or an explicit **off** intent ("normal" / "turn off"),
     with off winning over a name so "turn off the gangster voice" disables. On a match it
     flips the personality in place and returns a canned spoken confirmation as a one-item
     reply stream, **skipping the model entirely** (like the persona fast path). Conservative
     by design: ordinary speech and a bare name without a trigger never match.
  2. **`set_personality` rig tool** (always registered, like the timer tools) — the fallback
     the model invokes for phrasings the fast path doesn't catch. It emits a **Core-side**
     `DeviceAction::SetPersonality { name }` that `drain_device_actions` applies to the same
     setter rather than writing a device frame.
- **GUI** = the config page **Personality** tab (`/personality`, `webconfig/personality.html`
  + `personality_status_json`/`personality_save_json`): toggle on/off, pick the active one,
  and edit/add/remove personalities (label + style description). A full-record save through
  `apply_personality`, mirroring the Household tab.

---


### 8.5 Home Assistant: a zero-model fast path ahead of System-1

Home commands are the clearest case where the model adds latency without adding value,
so they are answered **before** System-1, memory recall and the LLM
(`plans/HomeAssistantPlan.md`). `Pipeline::respond_and_speak` runs, in order:
`parse_command` (memory) → **`run_home_fast_path`** (`orchestrator/home.rs`) →
`run_system1` → System-2.

- **Shape first, no I/O:** `home_assistant::matcher::parse` recognizes an imperative
  command's sentence shape with string code; anything else returns `None` instantly.
- **Resolve against a cache:** `home_assistant::catalog::resolve` maps the target phrase
  to entities in a cached HA catalog (room groups, "all lights", the display's room,
  exact names, a unique trailing-word match) and **refuses to guess** — ambiguity falls
  through. `exact_only` devices (fireplace) are never in a group.
- **Act directly:** one `POST /api/services/…` call, then a fixed `FastReply` (follow-up
  window left open). An HA error is spoken ("couldn't reach Home Assistant"), not handed
  to the LLM.
- **One hub, two callers:** the `HomeAssistant` hub (connection + catalog cache) is built
  once in `Config::llm_factory`, held on `LlmFactory`, and shared by the pipeline and the
  `home_control` tool (`llm/rig/tools/home.rs`), which uses the same resolver.
- **Room:** `DeviceContext.room` (from `audio-start` `data.room`), else
  `home_assistant.default_room`.

### 8.6 Bilingual turns: English + Brazilian Portuguese (`plans/BilingualPlan.md`)

**Replies follow the language spoken on each turn.** With `language.enabled = ["en",
"pt-BR"]` (`anamanti.json`, or Voice → Languages on the config page) the Core resolves
one `TurnLang` per turn right after STT (`orchestrator/language.rs` →
`lang::resolve`) and everything downstream reads it:

```
STT (Transcript{text, lang?}) ─► lang::resolve ─► TurnCtx.lang ─┬─ runtime.tts_voice = voices[lang]
   short phrase ▸ engine report (overruled by decisive function  ├─ prompt "# Language" block
   words) ▸ text ID ▸ previous turn
   ▸ speaker's preferred language ▸ household default            ├─ lang::phrases (canned replies)
                                                                 └─ System-1 gate (system1::ready_for)
```

- **STT seam:** `SttEvent::Transcript(Transcript { text, lang })`. ElevenLabs opens the
  session with `language_code=<default>` + `secondary_languages=<others>` +
  `include_language_detection=true` and reads `language_code` from
  `committed_transcript_with_timestamps` (measured to keep English output identical to a
  pinned `en` session; auto-detect was rejected). whisper-rs (multilingual model) decodes
  pinned to the default, reads the language from one decoder step over the existing
  encoder state, and re-decodes only when another enabled language wins. Wyoming reads
  `transcript.data.language` when the server sends it (wyoming-faster-whisper 3.8.1
  doesn't — text ID decides there). `SttEngine::begin_with(format, languages)` carries
  the live language set to engines loaded at boot.
- **Fast paths try every enabled language** (turn language first) — memory commands
  (`parse_command_in`), Home Assistant (`home_assistant::pt` maps Portuguese room/device
  words onto English-shaped catalog candidates and acts only on one distinct target),
  the personality switch (`detect_personality_switch_in`), inferred capture
  (`infer_memories_in`) — and the language whose phrasing matched becomes the turn's
  language (`LangSource::Phrase`). All still pure string code, no I/O.
- **Canned replies** live in `lang::phrases`; the English arm is byte-for-byte the
  pre-bilingual text (pinned by tests). Facts are stored as spoken.
- **System-1** answers a Portuguese turn only for intents in `system1::PT_BR_READY`
  (calibrated on `jev`, `tests/system1_calibration.rs::calibrate_portuguese`); others
  defer to System-2, which replies in Portuguese.
- **Voice:** `LanguageSettings::voice_for(lang, tts_voice)` — a language's own Piper
  voice, else the global `tts_voice`. The first reply sentence is language-checked
  (function words, then trigram ID); if the model answered in the other language ("answer
  in English") the rest of the reply switches voice (`timing.reply_lang`). The voice's
  language picks its Piper server via `ServiceConnector::connect_tts_for` (`tts_addrs`):
  wyoming-piper keeps one voice loaded, so one Piper per language keeps each resident
  (a switch on a shared server reloads the model, ~0.5–1 s). Boot warms each voice.
- **Unchanged by decision:** the Display (English chrome; streamed text may be
  Portuguese), the weather screen / ambient push (English), the wake word, VAD, speaker
  ID, the memory embedder. No Wyoming wire change.
- **Single-language mode** (no `language` block — the default) short-circuits every
  step: `LangSource::Fixed`, no prompt block, no voice override, English-only phrase
  tables — byte-identical to the pre-bilingual pipeline.

## 9. Cross-references

- Delivery phases & open questions → [`Plan.MD`](./Plan.MD)
- Contributor / AI-agent build guidance → [`agents.md`](../agents.md)
- Product overview & setup → [`README.md`](../README.md)
- **Music (planned, pre-implementation):** house-wide music is a **Mac-hosted,
  Snapcast-routed** capability that is deliberately **separate from the Wyoming/TTS
  audio path and the device's cpal/oboe path** — music PCM never enters
  `anamanti_core`. Sources (librespot + the idle mpv web-URL player) are **launched by snapserver**
  (`librespot://` / `process://` sources reading their stdout — named snapfifos stall
  on macOS, see `deploy/snapcast/snapserver.conf`); snapserver (on the Mac) fans out to
  snapclient speakers;
  the Anamanti Core only controls (Web API / mpv IPC / snapserver JSON-RPC ducking).
  Design → [`snapcast_routing_plan.md`](./snapcast_routing_plan.md) (transport) +
  [`MusicPlan.md`](./MusicPlan.md) (Spotify source + control plane).
