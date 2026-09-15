# Ambient Smart Display Voice Assistant

An always-on, ambient voice assistant that turns a jailbroken **Echo Show 8**
(running LineageOS) into a private smart display, backed by an **M4 Mac Mini**
that does the heavy lifting (speech-to-text, an LLM brain, and text-to-speech).

Say a wake word → the display streams your voice to the Mac → your words appear
live on screen → the assistant thinks, answers on screen token-by-token, and
speaks the reply back through the Echo Show's speakers.

---

## What it does

- 🎙️ **On-device wake word** — fully offline detection via an openWakeWord
  `.onnx` model, so no audio leaves the device until you summon it.
- 🗣️ **Streaming STT** — audio streams over the [Wyoming Protocol](https://github.com/rhasspy/wyoming)
  to a Whisper/CoreML server on the Mac; transcripts render as you speak.
- 🧠 **Pluggable LLM brain** — swap between a local model (Ollama / llama.cpp) and
  a cloud API (Claude / OpenAI) behind one interface.
- 🔊 **Spoken replies** — Piper (Wyoming TTS) synthesizes the answer; the Echo
  Show plays it back through the same Rust audio engine that captured you.
- 💬 **Full-duplex + memory** — interrupt mid-reply with the wake word, and the
  assistant remembers facts/preferences across sessions (stored on the Mac).
- 📺 **Ambient display** — a landscape Flutter UI tuned for the 8-inch screen,
  with an idle photo slideshow from a Google Photos/Drive folder.

## Architecture at a glance

```
Echo Show 8 (LineageOS)                     M4 Mac Mini
┌──────────────────────┐                    ┌────────────────────────┐
│ Flutter UI  (Dart)   │                    │ Wyoming STT (Whisper)  │
│   ▲ FRB v2 streams   │                    │        ▼               │
│ Rust engine          │── TCP (Wyoming) ──►│ LLM orchestrator       │
│  • capture (cpal)    │◄── transcript ─────│  (Ollama or cloud)     │
│  • wake word (tract) │◄── TTS audio ──────│        ▼               │
│  • Wyoming client    │                    │ Wyoming TTS (Piper)    │
│  • playback (cpal)   │                    └────────────────────────┘
└──────────────────────┘
   mDNS: discovers _wyoming._tcp on the LAN
```

See [`architecture.md`](./architecture.md) for the full design and
[`agents.md`](./agents.md) for AI-agent / contributor build guidance.

## Tech stack

| Layer | Technology |
| --- | --- |
| UI | Flutter (always-on landscape) |
| Systems | Rust, cross-compiled for `aarch64-linux-android` |
| Interop | `flutter_rust_bridge` v2 (zero-copy Dart↔Rust) |
| Audio | `cpal` on the Android `oboe` backend (capture + playback) |
| Wake word | `tract-onnx` running an openWakeWord model |
| Transport | Wyoming Protocol over `tokio` TCP |
| Discovery | mDNS / Zeroconf (`_wyoming._tcp`) |
| Mac services | Wyoming STT (Whisper/CoreML) · LLM · Wyoming TTS (Piper) |

## Repository layout

```
/lib      Flutter application (Dart)
/rust     Rust systems engine (audio, wake word, Wyoming client)
Plan.MD           Living project plan + confirmed decisions
architecture.md   Detailed technical design
agents.md         Build guidance for AI agents & contributors
README.md         You are here
```

## Getting started

> ⚠️ Pre-implementation. These are the intended bootstrap steps; see `Plan.MD`
> Phase 1 for the current state.

### Prerequisites

- Flutter SDK + Android SDK/NDK
- Rust toolchain with the Android target:
  ```bash
  rustup target add aarch64-linux-android
  ```
- `flutter_rust_bridge_codegen`
- An Echo Show 8 running LineageOS with `adb` access
- An M4 Mac Mini on the same LAN running Wyoming STT + Piper TTS

### Build & run (target)

```bash
# 1. Generate FRB bindings
flutter_rust_bridge_codegen generate

# 2. Build the Rust engine for the device
cargo build --release --target aarch64-linux-android

# 3. Deploy the Flutter app to the Echo Show
flutter run -d <echo-show-device>
```

On first launch the device discovers the Mac's Wyoming service via mDNS. No
static IP configuration is required.

## Status

Planning / pre-implementation. Scope, discovery, wake-word, LLM, TTS, and
playback decisions are locked in — see the decision table in `Plan.MD`.

## License

TBD.
