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
| Audio | `cpal` (capture + playback); Android backend is NDK AAudio |
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
level, detections). Remaining phases (Wyoming client, Mac pipeline, UI, settings)
are planned — see the decision table and phases in `Plan.MD`.

## License

TBD.
