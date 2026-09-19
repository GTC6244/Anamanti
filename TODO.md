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
- [x] Exercise **barge-in** (wake word during playback) on-device — works: a wake word
      mid-reply flushes playback and starts a fresh turn (flush-on-wake + the
      `ambient-interrupt` frame aborts the orchestrator's in-flight LLM+TTS). Detection
      over loud playback takes a try or two without AEC.
- [ ] Exercise **memory** on-device (voice "remember…"/"forget that" + the settings
      memory list).
- [ ] Verify the Phase-6 **settings control protocol** on-device: change LLM
      backend / model / TTS voice from the settings screen and see it take effect on
      the next turn; view/delete memory entries.

## 2. AEC / self-triggering (investigated on hardware — Plan §4)

- [x] Measured on real hardware: the raised `active_threshold` during playback is the
      shipping mitigation; self-triggering / wake-detection over loud playback is an
      accepted limitation (barge-in still works, takes a try or two).
- [x] Tried Android platform AEC (`VOICE_COMMUNICATION` input preset): reachable, does
      **not** break the wake word on release, but does **not cancel** on this device
      (mic RMS unchanged during playback). Dead end without audio-mode coordination.
- [x] Tried software NLMS AEC (playback as reference): works in isolation (>20 dB host
      tests) but **net-negative** for this flush-on-wake design — barge-in flushes
      playback so there's no echo to cancel, and it corrupts the near-end transcript.
      Reverted.
### ✅ SOLVED on-device (2026-09-18): hardware-referenced AEC shim

The Echo Show 8 has a **hardware sample-aligned echo reference** — the FPGA capture
stream `pcmC0D22c` (6-ch `S24_3LE` 16 kHz) carries the 4 mics on ch0-3 and a **DAC
loopback on ch4-5** (confirmed on our unit; source: jxlarrea/lineageos-echo-show-camera
`docs/echo-cancellation.md`). A HAL shim `LD_PRELOAD`ed into the audio service
interposes tinyalsa `pcm_read` and runs AEC on the mic using `avg(ch4,ch5)` as the
far end — transparent to `AudioRecord`/AudioFlinger.

- [x] Built a **self-contained Speex-only AEC shim** (armv7, static libc++, deps
      liblog/libm/libdl/libc) and installed it reversibly via `LD_PRELOAD` in the audio
      HAL init rc (`.orig` backups kept), PGA lowered 80→40.
- [x] **Verified working, supervised:** during story playback the shim logs `ref ~-45
      dBFS` (loopback is real) and `mic out` 8→16 dB below `mic in` as the adaptive
      filter converges (**~16 dB cancellation, talker-preserving**). Barge-in "Hey
      Jarvis, actually tell me about dogs" *over* a playing story produced a clean
      transcript where the pre-shim attempt gave an empty one. Stable, persists across
      reboot.
- Full install/revert state and knowledge are recorded in the plan file
      (`~/.claude/plans/snazzy-hopping-gadget.md`, "ON-DEVICE AEC SHIM" section).

**Remaining AEC follow-ups (device-side; not in this repo's build):**

1. [ ] **Quiet the shim's debug logging** in daily use: `setprop
       persist.vendor.amznaec.log 0` (drops the 5s `ref/mic in/out` lines).
2. [ ] **Confirm doze behavior in normal daily use** — the app's mic is blocked while
       the device "top-sleeps" (`getInputForAttr permission denied`); the ambient UI
       normally keeps the display awake, but verify over a real idle→wake cycle without
       the `svc power stayon true` test crutch.
3. [ ] 🚀 **Build the WebRTC AEC engine variant (38-50 dB)** via the LineageOS 18.1
       `crown` ROM tree (`m libamznaec_shim` against the ROM's `external/webrtc` +
       `libwebrtc_audio_preprocessing.so`). Needs a Linux host + ~150 GB tree. Much
       stronger cancellation than Speex, but its nonlinear suppressor eats the talker
       ~9.5 dB during double-talk — so keep the talker-preserving Speex engine as the
       wake-word/barge-in default and make the engine selectable
       (`persist.vendor.amznaec.engine`). **The exciting one.**
4. [ ] **Optional Speex tuning:** raise `persist.vendor.amznaec.spx_filter_ms` for a
       longer adaptive filter; try mild `spx_echo_suppress` for more cancellation at a
       small talker cost. A/B against the current ~16 dB.
5. [x] **Shim source is in version control** — extracted to its own repo
       **[EchoShow8gen1-aec-shim](https://github.com/Brutus-GTC6245/EchoShow8gen1-aec-shim)**
       (private): `src/amznaec_speex_shim.cpp`, a standalone NDK build script (no ROM
       tree), reversible adb install/uninstall, the `cap6`/`play6` probes, a
       built-from-source prebuilt `.so`, and vendored SpeexDSP. The WebRTC-engine build
       (#3 above) belongs there too when it happens.
6. [ ] **Turn off Rooted debugging + wifi-adb persistence** on the device when the AEC
       work is finished.

**Superseded by the on-device shim (kept in this repo, gated/dark):** the host-side
WebRTC APM (`mac/src/aec/`, `aec` feature) and the AudioRecord capture shim landed for
this effort still build and are useful (clean STT path / true-16 kHz capture), but the
on-device shim is now the primary AEC. Decide later whether to invest in the host APM
live-path wiring (Plan Phases 1-2) or retire it.

- Note: **on-device perf/audio testing must use `--release` APKs** (debug Rust makes
      inference ~3.6× slower on the 32-bit device and masks the real behavior).

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

## 5. Per-person speaker identification (this branch — finish to production)

Phases A–D **and** the device "People" UI are implemented, tested, and merged
(see [`speaker_id_plan.md`](./speaker_id_plan.md)): passive voiceprint +
auto-clustering, per-person SQLite + GraphRAG memory, the prompt identity line,
voice naming ("my name is …"), and the `ambient-*speaker*` control frames. It is
**not running yet** — `AMBIENT_SPEAKER_ID` is unset (household), the release binary
is built without `--features speaker`, and there is no real embedding model on the
Mac. To take it from dormant code to a real feature:

### 6a. Real embedding model (Phase E — the blocker for accuracy)

- [ ] Obtain/export an **ECAPA-TDNN** (or WeSpeaker) speaker-embedding model to
      **ONNX** and place it on the Mac.
- [ ] Confirm the model's **input contract** (raw waveform vs. precomputed log-mel;
      tensor layout `[1, frames, mels]` vs `[1, mels, frames]`) and align
      `OnnxSpeakerEmbedder` + `FbankConfig` (`mac/src/speaker/{embed,features}.rs`)
      to it; set `AMBIENT_SPEAKER_EMBED_DIMS` (192 for ECAPA).
- [ ] Build/ship the orchestrator with **`--features speaker`** and set
      `AMBIENT_SPEAKER_MODEL_PATH` (else it falls back to the dev-only mock embedder).

### 6b. Enable + calibrate on hardware

- [ ] Launch with **`AMBIENT_SPEAKER_ID=on`** and fold it (plus `--features speaker`
      + the model path) into the launchd/run script from §4.
- [ ] **Calibrate** `AMBIENT_SPEAKER_MATCH_THRESHOLD` / `_NEW_THRESHOLD` /
      `_MIN_SPEECH_MS` against real Echo Show far-field captures; measure EER and
      record the chosen operating point.
- [ ] Verify per-person **memory scoping** + the prompt identity line with 2+ real
      speakers (Sam vs. Dana get separate facts; the reply greets by name).
- [ ] Verify the **People** settings screen end-to-end on the device (list, name,
      merge, forget) and that anonymous clusters show as "Speaker N".
- [ ] Verify **GraphRAG per-`User`** attribution with `AMBIENT_MEMORY_BACKEND=helix`
      (+ `OPENAI_API_KEY`): turns/memories attach to the right user node.

### 6c. Robustness & deferred cuts

- [ ] **Diarization**: multiple speakers within one utterance/turn (v1 attributes the
      whole turn to a single utterance-level embedding).
- [ ] **Cross-session merge suggestions**: propose merging clusters whose centroids
      are close ("Speaker 3 sounds like Dana — merge?").
- [ ] **Voiceprint drift / re-enrollment** as voices age or the mic/AEC path changes.
- [ ] **Retention/pruning** of stale, never-named anonymous clusters.
- [ ] Explicit **"who am I?"** handling (today it relies on the prompt identity line).
- [ ] Privacy: confirm the People "forget" path fully removes the voiceprint centroid;
      document that voiceprints never leave the LAN.

## 6. Nice-to-haves / follow-ups

- [ ] Bundle additional wake-word classifiers (only `hey_jarvis` ships today; others
      in the settings list need their `<name>.onnx` dropped into the model dir).
- [ ] Show wake-word availability in settings (grey out names whose `.onnx` is
      absent) instead of silently degrading to capture-only.
- [ ] Persist a last-known copy of orchestrator settings on the device for display
      while the Mac is offline.

## 7. Tools & actions (agent capabilities)

Tool calling runs on the **rig** engine, now the default (`mac/src/llm/rig.rs`).
Adding a capability = registering one tool in `Tools`. See Plan §3 "Tool calling &
actions".

- [x] **Weather context**: inject a configured home location + units
      (`AMBIENT_HOME_LOCATION` / `AMBIENT_WEATHER_UNITS`) into the system prompt so
      "what's the weather" resolves "here" (`orchestrator::location_line`).
- [x] Make **rig the default engine** + web search on by default (was opt-in).
- [ ] Verify on hardware: ask "what's the weather" with `AMBIENT_HOME_LOCATION` set
      and a Tavily key — confirm the model calls `internet_search` and speaks a
      location-correct answer.
- [x] **Device-action framework**: the `ambient-timer` frame + `set_timer`/`cancel_timer`
      tools emit a `DeviceAction` onto a per-turn channel that the orchestrator drains
      and relays on the turn socket (`orchestrator::drain_device_actions`). The seam is
      `LlmTurn.actions` (an `ActionSink`).
- [x] **Timers/alarms on-device** (unlimited concurrent): Rust `TimerManager`
      (`rust/src/engine/timer.rs`) owns each countdown on the long-lived Network runtime
      (outlives the turn socket), fires a synthesized chime via the shared `PlaybackSink`,
      and emits `WakeWordEventKind.timer{Started,Finished,Cancelled}`. Flutter renders a
      countdown-chip overlay (`lib/src/ui/timers_overlay.dart`) visible in idle + turns.
- [ ] **Verify timers on hardware**: "set a 5-minute pasta timer" → chip counts down →
      chime + "time's up" at zero; "cancel the pasta timer" / "cancel all timers"; two
      concurrent timers; a timer keeps running after the Mac disconnects mid-countdown.
- [ ] Consider a dedicated **weather tool** if web-search summaries prove too coarse
      (structured forecast vs. a search snippet).
- [ ] **Query/list timers** by voice ("how long left?") — needs a device→Mac timer-state
      report so the model can answer; today the countdown UI answers visually.
- [ ] **Calendar / reminders** (deferred): pick a backend — macOS EventKit (local,
      no OAuth), Google Calendar (OAuth, needs a client ID), or generic CalDAV.
