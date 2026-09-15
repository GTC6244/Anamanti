# TODO — remaining work

All six delivery phases in [`Plan.MD`](./Plan.MD) are implemented and merged, and
every locked decision is resolved. What remains is **not new phase work** — it is
real-world integration, one genuine feature stub, and empirical validation on
hardware. Grouped by priority.

## 1. End-to-end hardware run (highest value for a working demo)

Everything below the UI is in place and unit/integration-tested with mocks; the
full loop has not been exercised against real services on the device.

- [ ] Stand up the Mac services on the LAN: **wyoming-faster-whisper** (STT, 10300),
      **wyoming-piper** (TTS, 10200), and **Ollama** (11434) — all off-the-shelf.
- [ ] Run the orchestrator: `cargo run --manifest-path mac/Cargo.toml --release`
      (advertises `_wyoming._tcp`; env in `mac/src/config.rs`).
- [ ] Install the release APK on the Echo Show and run one full turn:
      wake word → STT → LLM → TTS playback, with the transcript/reply rendered live.
- [ ] Confirm **mDNS discovery** works across the real network (no hardcoded IP).
- [ ] Exercise **barge-in** (wake word during playback) and **memory** on-device
      (voice "remember…"/"forget that" + the settings memory list).
- [ ] Verify the Phase-6 **settings control protocol** on-device: change LLM
      backend / model / TTS voice from the settings screen and see it take effect on
      the next turn; view/delete memory entries.

## 2. AEC / self-triggering (the one open empirical risk — Plan §4)

- [ ] Measure on real hardware whether raising the wake-word confidence threshold
      during playback (`active_threshold`) is enough to stop the device
      self-triggering on its own TTS.
- [ ] If not, implement real AEC (Rust-side WebRTC/speexdsp with the playback signal
      as reference, or Android hardware `AcousticEchoCanceler`).

## 3. Real Google OAuth for the photo slideshow (feature stub)

Currently a testable seam: `GoogleAuthenticator` + `StubGoogleAuthenticator` and the
`GooglePhotoSource` path in `lib/src/slideshow/photo_source.dart`. Scopes already
declared (`photoslibrary.readonly`, `drive.readonly`).

- [ ] Decide the flow based on whether the Echo Show's LineageOS build has **Google
      Play Services**:
  - No Play Services (typical): use the **Device Authorization flow**
    (OAuth client type "TVs and Limited Input devices") — show a code + URL, approve
    on a phone. Does not need Play Services or a keyboard.
  - Has Play Services (GApps): standard `google_sign_in` with an **Android** OAuth
    client ID (package `com.ambientdisplay.ambient_display` + release/debug SHA-1).
- [ ] Create the OAuth client ID in Google Cloud Console; enable the Photos Library
      and/or Drive API; configure the consent screen + test users.
- [ ] Implement a real `GoogleAuthenticator.link()` for the chosen flow.
- [ ] Propagate the resulting **access token + photo URLs** from the settings screen
      back to the slideshow: thread a `GoogleLinkResult` through `onApplied` in
      `lib/main.dart` into `photoSourceFromSettings(...)` (today the token isn't
      carried, so the slideshow stays on local ambient gradients even when linked).
- [ ] Implement folder/album listing so the picker shows real folders and resolves
      their image URLs.

## 4. Ops & deployment polish

- [ ] Run the orchestrator as a managed service on the Mac (launchd/login item) so it
      survives restarts.
- [ ] Document the concrete LAN setup (server versions, ports, Piper voice, model
      choices) in `README.md` from a real deployment.
- [ ] Confirm auto-reconnect/backoff behavior end-to-end when the Mac goes away and
      returns (slideshow keeps running; subtle offline chip; wake words queue).

## 5. Nice-to-haves / follow-ups

- [ ] Bundle additional wake-word classifiers (only `hey_jarvis` ships today; others
      in the settings list need their `<name>.onnx` dropped into the model dir).
- [ ] Show wake-word availability in settings (grey out names whose `.onnx` is
      absent) instead of silently degrading to capture-only.
- [ ] Persist a last-known copy of orchestrator settings on the device for display
      while the Mac is offline.
