# Architecture

Detailed technical design for the Ambient Smart Display Voice Assistant. This
document is the source of truth for how the system is structured; `Plan.MD`
tracks phased delivery and open questions.

---

## 1. System overview

Two cooperating nodes on a trusted LAN:

- **Echo Show 8 (LineageOS)** — the *edge* device. Captures audio, detects the
  wake word offline, streams audio to the Mac, renders the conversation, and
  plays back the spoken reply. Constrained: ~1 GB RAM, mobile-class CPU.
- **M4 Mac Mini** — the *brain*. Runs the Wyoming STT server, a pluggable LLM
  orchestrator, and the Wyoming TTS (Piper) server.

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
│  Wyoming STT (Whisper / CoreML)  · orchestrator energy VAD         │
│        │ final transcript                                          │
│        ▼                                                           │
│  LLM Orchestrator  (trait-based, pluggable)  ◄─► Persistent Memory │
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

### 2.2 Flutter UI (Echo Show)

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

### 2.3 Mac Mini services

- **Wyoming STT** — Whisper (CoreML-accelerated on the M4) exposed via the
  Wyoming Protocol; emits partial and final transcripts.
- **LLM Orchestrator** — a trait/interface with a streaming
  `respond(transcript) -> token stream`. Two interchangeable implementations:
  local (Ollama/llama.cpp) and cloud (Claude/OpenAI). Selection is config-driven.
  Reads/writes the persistent memory store to build context and record new facts.
- **Persistent Memory** — a **SQLite** DB on the Mac is the store of record for
  long-term facts and user preferences across sessions. Policy is **explicit +
  inferred**: entries are added on request ("remember…") and auto-extracted from
  conversation turns. Managed via a settings list (view/delete) and voice
  ("forget that"); kept until cleared. Private to the LAN; survives device
  reflashes. **Retrieval/recall runs behind a `memory::Recall` seam and defaults
  to the embedded HelixDB GraphRAG backend** (`mac/src/memory/helix.rs`;
  in-process, no server/Docker): each completed turn is appended to
  `ambient_chatlog.jsonl` and a background ingester (`memory/ingester.rs`) embeds
  it (OpenAI `text-embedding-3-small`, `OPENAI_API_KEY`) into a graph of
  `User/Turn/Memory/Entity` nodes, so recall does vector KNN + a graph hop rather
  than keyword FTS. If `OPENAI_API_KEY` is absent or the graph fails to open,
  recall **falls back to SQLite FTS** (writes are unaffected); `AMBIENT_MEMORY_BACKEND=sqlite`
  forces FTS. **Per-person:** an opt-in local voiceprint embedder
  (`mac/src/speaker/`) identifies who is speaking from the utterance PCM and scopes
  memory writes/recall and the prompt to that person (a shared "household" scope is
  the floor); see `speaker_id_plan.md`.
- **End-of-speech / VAD** — runs in the **orchestrator**, not the STT server:
  `wyoming-faster-whisper` has no streaming VAD and only transcribes once it
  receives `audio-stop`, so the orchestrator scores per-chunk RMS energy over the
  incoming PCM and, after speech followed by ~900 ms of trailing silence (or a 6 s
  no-speech fallback), sends `audio-stop` to STT to finalize (`mac/src/orchestrator.rs`,
  `stream_to_transcript`). The Echo Show device still runs **no VAD of its own** —
  it streams continuously and waits for the transcript.
- **Wyoming TTS (Piper)** — synthesizes the reply into audio frames streamed back
  to the device.

> **Implementation (Phase 4, `/mac`):** the STT, LLM, memory, and TTS pieces above
> are wired by a standalone Rust crate `ambient_orchestrator` (`/mac`). It is a
> Wyoming **server** to the Echo Show and a Wyoming **client** to the off-the-shelf
> Whisper (STT) and Piper (TTS) Wyoming servers, with the pluggable `LlmBackend`
> trait (Ollama / Claude / mock) and the SQLite+FTS5 memory store in the middle
> (`mac/src/{orchestrator,server,discovery,llm,memory,wyoming}.rs`). It advertises
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
    (Phase 3 Wyoming turn); `replyToken`, `speaking` (Phase 5); and `stopped` /
    `error`. Modeled as a flat struct tagged by a unit-only `WakeWordEventKind` enum
    (payload fields carry neutral defaults when not relevant), so the boundary needs
    no `freezed` codegen and there is exactly one stream to manage.
  - The UI split (transcript vs. reply vs. phase) happens **Dart-side**, not on the
    boundary. `AssistantController` (`lib/src/engine/assistant_controller.dart`)
    folds this single event stream into an observable `AssistantState` / `TurnPhase`
    (`idle → listening → connecting → thinking → speaking`, plus `error`) that
    widgets watch. There are no separate `transcript_stream` / `reply_token_stream`
    / `state_stream` sinks on the FRB boundary.
- Control flows **Dart → Rust** via generated function calls: `start_wake_word_engine`
  / `stop_wake_word_engine` (Phase 2–3) plus the Phase-6 settings functions
  (`fetch_orchestrator_settings`, `update_orchestrator_settings`, `list_memories`,
  `delete_memory`, `clear_memories` in `rust/src/api/settings.rs`). The settings
  functions are async: each stands up a small current-thread `tokio` runtime and
  drives one Wyoming control round trip, so FRB returns a `Future` off the UI
  isolate.
- Zero-copy is preferred for audio-adjacent buffers; UI text uses ordinary
  generated types.

### Phase 6 — settings + memory control protocol

- The **wake word, thresholds, and photo source** are device-local: the Flutter
  settings screen persists them (`SettingsStore` → JSON) and applies them by
  rebuilding the `WakeWordConfig` and restarting the engine / refreshing the
  slideshow.
- The **LLM backend + model and TTS voice** live on the Mac and are read/changed
  over a **project-local control protocol** on the device↔orchestrator hop —
  `ambient-*` Wyoming frames (`describe`/`set` settings; `list`/`delete`/`clear`
  memories; `list-models`) that ride the existing framing (byte-identical `types`
  in both crates, no off-the-shelf server sees them). The orchestrator's `Pipeline`
  reads a per-turn snapshot of runtime-swappable `SharedSettings`, so a
  backend/voice change takes effect on the next turn with no restart; the accept
  loop routes control frames to `control::handle_control` and audio-start frames to
  a turn. Backends are `ollama` (local), `anthropic` and `openai` (cloud), and
  `mock`; selecting a cloud backend needs its API key (`ANTHROPIC_API_KEY` /
  `OPENAI_API_KEY`) on the Mac, else the change is rejected in-band (never dropping
  the connection).
- **Model selection** is a drop-down of **specific Anthropic / OpenAI models from
  the last 12 months**, produced by `llm::catalog::ModelCatalog`: it live-queries
  each provider's `GET /v1/models` (Anthropic `created_at`, OpenAI `created`),
  filters to the trailing 12 months (and OpenAI to chat models), caches the result,
  and falls back to a curated built-in list when a key is missing/offline. The list
  is exposed to the device via `ambient-list-models` → `ambient-models` and to the
  browser via `GET /models` on the config page. The chosen `llm_model` flows through
  the same `SharedSettings::apply` → `LlmFactory::build` path into the concrete
  backend's request, and persists to `ambient_settings.json`.
- **Anthropic auth mode** (`llm::anthropic_auth`): a per-provider toggle selects
  **API key** (`x-api-key` from `ANTHROPIC_API_KEY`) or **subscription OAuth**
  (`Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20`). Subscription tokens
  come from `AnthropicTokenProvider` — `ANTHROPIC_OAUTH_TOKEN` (from
  `claude setup-token`) or a token-printing command (`AMBIENT_ANTHROPIC_TOKEN_CMD`,
  default the `ant` CLI), cached with a short TTL and refreshed on a 401. Both the
  chat backend and the catalog share the provider so listing + turns authenticate
  identically. The mode rides the settings protocol (`anthropic_auth` field) and the
  config page toggle; OpenAI is API-key-only.
- **Debug/inspection pages** (`webconfig.rs`, same loopback HTTP server as the
  config page, strictly read-only): `GET /chatlog`, `/prompts`, `/sqlite`, `/helix`
  render the recent chat log, the exact assembled LLM prompt per turn (captured to
  a separate `ambient_promptlog.jsonl` via `PromptLog`), the SQLite memory rows, and
  the HelixDB GraphRAG node stats + a sample of nodes (behind the read-only
  `memory::GraphView` seam; reports "disabled" on the SQLite backend). Each has a
  `*.json` data endpoint the page fetches. No auth — keep the config address on a
  trusted network.
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
  no VAD; **the orchestrator detects end-of-speech** (energy VAD over the PCM) and
  sends `audio-stop` to the STT server, which then returns the final transcript.
- **THINKING** — STT final transcript handed to the LLM orchestrator (which
  consults persistent memory); reply tokens stream back and render.
- **THINKING → SPEAKING (streaming TTS):** the orchestrator does **not** buffer the
  whole reply before speaking. It segments the LLM token stream into sentences and
  synthesizes each with Piper as soon as it forms, so the first audio reaches the
  device ~first-sentence latency (~2 s) instead of full-reply latency (~8 s). The
  per-sentence Piper bursts are **coalesced into one device-facing audio stream** (one
  `audio-start`, all chunks, one final `audio-stop`) because the device ends its turn
  on the first `audio-stop`.
- **SPEAKING** — the coalesced Piper audio stream is played via `cpal`/`oboe`.
- **Barge-in:** wake-word scoring keeps running during THINKING and SPEAKING. A wake
  word mid-reply **always flushes the playback ring immediately** (silencing the reply
  even after the turn has technically ended — the orchestrator relays audio faster than
  real-time, so the turn reaches IDLE while audio is still draining from the buffer),
  sends an `ambient-interrupt` frame so the orchestrator **aborts the in-flight LLM +
  TTS**, and starts a fresh turn.
- **AEC:** not shipped. Investigated on real hardware (Echo Show 8): the platform
  `VOICE_COMMUNICATION` AEC preset is reachable but doesn't actually cancel on this
  device, and a software NLMS canceller is net-negative for this flush-on-wake barge-in
  (there's no simultaneous echo to cancel once playback is flushed). The interim
  mitigation remains **raising the wake-word confidence threshold while active**. Real
  AEC would require a true keep-playing-while-listening full-duplex redesign — see
  `Plan.MD` §4 / `TODO.md`.
- Return to **IDLE** on playback completion, timeout, or reset.

### Wire format

- Newline-delimited (`\n`) JSON control frames for metadata and events.
- Raw audio chunks streamed immediately after the setup/metadata frame.
- The same socket carries outbound audio and inbound `transcript`, streamed
  `reply-token` (per-LLM-token text for on-screen rendering), and synthesized audio
  frames.
- **`ambient-interrupt`** (device → orchestrator): a project-local barge-in frame that
  tells the orchestrator to abort the in-flight LLM generation + TTS at once, rather
  than only learning of the interruption when the socket drops.

---

## 5. Discovery & networking

- **mDNS / Zeroconf**: the device browses `_wyoming._tcp`, resolves the Mac's
  host + port, and caches it for fast reconnect.
- No static IP configuration required.
- **Resilience**: **auto-reconnect** with exponential backoff via mDNS when the
  Mac is unreachable. The idle photo slideshow keeps running; a subtle
  **disconnected** indicator reflects status; wake words queue until the socket
  is restored.

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
| openWakeWord via tract-onnx | Pre-trained models, minimal deps, offline |
| Pluggable LLM behind a trait | Swap local/cloud without touching the pipeline |
| Rust-side playback | One audio layer, symmetric with capture |
| Wake-word barge-in (flush-on-wake + `ambient-interrupt`) | Natural interruption without full-duplex complexity; AEC investigated on hardware and deferred (see §4) |
| Streaming sentence-chunked TTS | First-audio at first-sentence latency, not full-reply; coalesced to one device audio stream |
| VAD in the orchestrator | Device does no VAD; faster-whisper has no streaming VAD, so the Mac runs energy VAD and sends `audio-stop` |
| SQLite is the memory store of record | Simple, debuggable; holds explicit+inferred facts and the settings-list/voice management |
| Recall defaults to embedded HelixDB GraphRAG | Vector KNN + graph hop beats keyword FTS for context; in-process (no server/Docker); needs `OPENAI_API_KEY`, falls back to SQLite FTS if absent |
| Per-person speaker ID (local, opt-in) | Local ECAPA voiceprint (passive + auto-cluster) keeps voice on the LAN and scopes memory + prompt per person for better context; no raw audio leaves the device |
| On-device OAuth for photos | Device displays directly; no Mac proxy needed |
| Auto-reconnect + status | Robust to Mac downtime; slideshow stays up |
| Idle photo slideshow (Google) | Ambient value when idle; user picks the folder |

---

## 8. Cross-references

- Delivery phases & open questions → [`Plan.MD`](./Plan.MD)
- Contributor / AI-agent build guidance → [`agents.md`](./agents.md)
- Product overview & setup → [`README.md`](./README.md)
