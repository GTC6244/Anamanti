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
│  Wyoming STT (Whisper / CoreML) + server-side VAD                  │
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
- **Persistent Memory** — a **SQLite** DB on the Mac holding long-term facts and
  user preferences across sessions, with full-text search (no embedding model in
  v1). Policy is **explicit + inferred**: entries are added on request
  ("remember…") and auto-extracted from conversation turns. Managed via a
  settings list (view/delete) and voice ("forget that"); kept until cleared.
  Private to the LAN; survives device reflashes.
- **Server-side VAD** — the STT server owns end-of-speech detection and signals
  when the device should stop streaming; the device does no VAD of its own.
- **Wyoming TTS (Piper)** — synthesizes the reply into audio frames streamed back
  to the device.

---

## 3. Interop boundary (flutter_rust_bridge v2)

- FRB v2 generates the JNI bindings and the Dart API from Rust signatures.
- Data flows **Rust → Dart** primarily via generated `StreamSink`s:
  - `wake_word_stream` — engine events surfaced by `start_wake_word_engine` as a
    returned `Stream<WakeWordEvent>`: capture started, status, input level,
    wake-word detected, stopped/error (Phase 2) plus the Phase-3 Wyoming turn
    lifecycle (connecting, streaming, transcript, disconnected). Modeled as a flat
    struct tagged by a unit-only `WakeWordEventKind` enum so the boundary needs no
    `freezed` codegen. The dedicated `transcript_stream` / `reply_token_stream` /
    `state_stream` below are the Phase-5 UI split built on top of this.
  - `transcript_stream` — live/partial + final transcripts.
  - `reply_token_stream` — LLM reply tokens for on-screen rendering.
  - `state_stream` — assistant state transitions (idle/listening/thinking/speaking).
- Control flows **Dart → Rust** via generated function calls (e.g. start engine,
  cancel/reset, select LLM backend).
- Zero-copy is preferred for audio-adjacent buffers; UI text uses ordinary
  generated types.

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
  `transcript` events on the same socket. **End-of-speech is server-side**: the
  STT server's VAD signals completion, then the device sends `audio-stop`.
- **THINKING** — STT final transcript handed to the LLM orchestrator (which
  consults persistent memory); reply tokens stream back and render.
- **SPEAKING** — Piper TTS audio frames arrive and are played via `cpal`/`oboe`.
- **Full-duplex barge-in:** wake-word scoring keeps running during THINKING and
  SPEAKING; a wake word interrupts playback and starts a new turn. AEC is
  deferred for v1; the interim mitigation is to **raise the wake-word confidence
  threshold while in SPEAKING** so the device's own speaker is less likely to
  self-trigger (see `Plan.MD` §4).
- Return to **IDLE** on playback completion, timeout, or reset.

### Wire format

- Newline-delimited (`\n`) JSON control frames for metadata and events.
- Raw audio chunks streamed immediately after the setup/metadata frame.
- The same socket carries outbound audio and inbound `transcript` + synthesized
  audio frames.

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
- **Latency:** wake word runs on-device; audio streams (not batched) so STT can
  emit partials early; reply tokens render as they arrive; TTS streams back.
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
| Full-duplex barge-in | Natural interruption; AEC deferred, threshold-tuned in v1 |
| Server-side VAD | Less device work; STT server owns end-of-speech |
| Persistent memory in SQLite | Simple, debuggable; FTS covers explicit+inferred facts |
| On-device OAuth for photos | Device displays directly; no Mac proxy needed |
| Auto-reconnect + status | Robust to Mac downtime; slideshow stays up |
| Idle photo slideshow (Google) | Ambient value when idle; user picks the folder |

---

## 8. Cross-references

- Delivery phases & open questions → [`Plan.MD`](./Plan.MD)
- Contributor / AI-agent build guidance → [`agents.md`](./agents.md)
- Product overview & setup → [`README.md`](./README.md)
