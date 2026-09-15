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
| **Idle screen** | **Photo slideshow** sourced from a chosen **Google Photos / Drive** folder |
| **Photo auth** | **On-device OAuth** — the Echo Show runs the flow and calls Google directly |
| **AEC** | **Ship full-duplex**; raise wake-word confidence **threshold during playback** to suppress self-triggers; real AEC only if still bad |
| **Settings screen** | User-configurable: **LLM backend**, **TTS voice**, **wake word**, **photo source** |
| **Doc intent** | Onboarding + AI codegen — docs guide AI agents and humans to build from scratch |

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
- **Systems layer:** Rust cross-compiled for Android (`aarch64-linux-android`).
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

### Phase 1 — Workspace Setup & Cross-Compilation
- Unified monorepo: `/lib` (Flutter) and `/rust` (systems layer).
- Configure `flutter_rust_bridge_codegen` to watch and generate JNI bindings.
- Install the NDK target: `rustup target add aarch64-linux-android`.
- Validate a hello-world cross-compile onto the LineageOS target via `adb`.

### Phase 2 — Native Audio & Local Inference (Rust)
- Initialize `cpal` to pull raw 16 kHz / mono / 16-bit PCM from the mic array.
- Feed the buffer stream through a pre-allocated ring buffer.
- Integrate `tract-onnx` + openWakeWord to score wake-word confidence continuously.
- Keep evaluation fully offline in a low-overhead thread loop to respect the 1 GB
  RAM limit.

### Phase 3 — Wyoming Client Core Logic (Rust)
- mDNS discovery of `_wyoming._tcp`; cache the resolved host/port.
- Connection-state struct over `tokio::net::TcpStream`.
- **State machine:**
  - **Idle:** analyze audio for wake word; socket dormant/closed.
  - **Triggered:** wake word fires → open TCP → send Wyoming `audio-start` header.
  - **Streaming:** send PCM chunks in Wyoming frames; concurrently read `transcript`
    events on the same socket. End-of-speech is decided **server-side** (VAD on the
    STT server), which signals when to stop.
  - **Silence/Timeout:** on the server's end-of-speech signal (or timeout) send
    `audio-stop`; tear down / reset.
- Wake-word evaluation keeps running **during playback** (full-duplex). AEC is
  deferred for v1, so guard against obvious self-triggers pragmatically.

### Phase 4 — Assistant Pipeline (Mac Mini)
- Wyoming STT (Whisper/CoreML) with **server-side VAD** emits the final transcript.
- **Pluggable LLM orchestrator** consumes the transcript, streams reply tokens.
- **Persistent memory store** (**SQLite** on the Mac) supplies context and records
  facts/preferences across sessions; feeds the LLM prompt. Policy: **explicit +
  inferred** (store on request and auto-extract from turns).
- Wyoming TTS (Piper) synthesizes the reply into audio frames.
- Frames stream back to the Echo Show over the Wyoming socket.

### Phase 5 — Reactive UI & Playback (Flutter + Rust)
- Dart screen state consumes FRB `StreamSink` events (transcript + reply tokens).
- Modular responsive 8-inch layout; smooth token-by-token rendering.
- **Idle ambient screen:** photo slideshow from a chosen Google Photos/Drive folder
  (**on-device OAuth**). Slideshow keeps running when disconnected.
- **Resilience:** auto-reconnect with backoff via mDNS + a subtle **disconnected**
  status indicator; wake words queue until reconnected.
- Rust (`cpal`/`oboe`) plays returned TTS audio frames through the speakers.
- **Full-duplex barge-in:** a wake word during playback interrupts and starts a new
  turn; **raise the wake-word threshold during playback** to suppress self-triggers.

### Phase 6 — Settings & Persistence
- Settings screen exposing: **LLM backend**, **TTS voice**, **wake word**, and
  **photo source** (with on-device Google auth + folder picker).
- **Memory management** in settings: view/delete stored items; voice commands
  ("remember…", "forget that").
- Wire settings through FRB to the Rust engine and the Mac orchestrator.

---

## 4. Resolved Details & Implementation Notes

All prior open questions are now decided (see the decision table). Notes for
implementers:

- **AEC / full-duplex:** ship full-duplex without dedicated AEC. Interim
  mitigation: **raise the wake-word confidence threshold while in SPEAKING** to
  suppress self-triggers from the device's own speaker. Only build real AEC
  (Rust-side WebRTC/speexdsp with the playback signal as reference, or Android
  hardware `AcousticEchoCanceler`) if self-triggering remains a problem.
- **Google Photos/Drive auth:** **on-device OAuth** — the Echo Show runs the
  consent flow and calls Google directly; the token is stored on the device.
  Needs a usable consent flow on the 8-inch screen and a folder picker in
  settings. *(Note: the claude.ai Google Drive connector in this environment is
  unauthorized and cannot prototype this — wire real OAuth into the app.)*
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
