# Porting the device client to Android TV

Feasibility notes for moving the ambient voice-assistant **client** (currently
running on an Echo Show 8 under LineageOS) to an Android smart TV.

**Bottom line:** mechanically portable — the client is a standard Flutter + Rust
Android app and Android TV is just Android — but it is more than a recompile, and
the microphone is a likely hardware showstopper. The Mac Anamanti Core
(STT/LLM/TTS/memory) is untouched by any of this.

## What ports for free

- **LAN architecture is client-agnostic.** The device only discovers
  `_wyoming._tcp` via mDNS and streams to the Mac Anamanti Core. Nothing on the Mac
  side changes.
- **The stack is portable.** Flutter UI + the Rust engine (`cpal`/`oboe` audio,
  `tract-onnx` wake word, `tokio` Wyoming client, mDNS) all run on Android TV's
  Android runtime. Nothing is LineageOS-specific at the code level.
- **Landscape-first UI** is already the correct orientation for a TV.

## What needs real work

### 1. ABI (easy)
The build guidance pins `--target-platform android-arm` (32-bit armeabi-v7a)
*specifically because the crown Echo Show 8 is 32-bit*. Most Android TVs are 64-bit
(arm64), some x86. Build `android-arm64` (or a multi-ABI APK). This is a build-flag
change, not a locked-decision issue — the arm restriction is a device fact, not a
project decision.

### 2. TV app conventions (moderate)
Stock Android TV expects:
- `CATEGORY_LEANBACK_LAUNCHER` intent filter + a TV banner to appear on the home
  screen.
- `uses-feature android.hardware.touchscreen required="false"` and
  `android.software.leanback`.
- **D-pad / focus navigation.** The current UI (settings, memory management, the
  OAuth photo-source flow) is touch-driven; a TV is driven by a remote's D-pad, so
  every interactive screen needs focus traversal. The always-on
  conversation/slideshow view is fine; settings and OAuth are where the work is.

## The likely blocker: the microphone

The whole premise is **ambient, always-on, far-field wake-word capture**
(openWakeWord scoring continuously in the Rust ring buffer). Most Android TVs:
- **Have no built-in far-field mic** — voice lives in the *remote*, which only
  opens the mic on a button press. That is fundamentally incompatible with an
  always-listening wake word.
- Even TVs with a built-in mic often hard-gate `RECORD_AUDIO` or expose no
  continuous capture path on stock firmware.

So the real question is not Flutter/Rust — it is **does the target TV have a
continuously-accessible far-field mic (or can a USB mic array be attached)?** If
not, the ambient assistant degrades to push-to-talk, which changes the product,
not just the port.

## Smaller notes

- Stock TV firmware is more locked down than LineageOS: sideloading, setting the
  app as launcher, and background/Doze restrictions on always-running capture may
  fight you. LineageOS grants freedoms you would lose on stock firmware.
- **AEC** stays exactly as deferred — the raised-wake-word-threshold-during-playback
  mitigation carries over unchanged.

## Summary

Software ports with an ABI flag change + a TV manifest + D-pad focus work, and the
Mac side is untouched. Whether it is *worth* porting comes down entirely to the
microphone: pick a TV with a real always-on far-field mic (or plan a USB mic
array), or the "ambient" part does not survive the move.
