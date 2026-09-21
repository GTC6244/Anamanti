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
- [ ] Run the orchestrator: `cargo run --manifest-path orchestrator/Cargo.toml --release`
      (advertises `_wyoming._tcp`; env in `orchestrator/src/config.rs`).
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
WebRTC APM (`orchestrator/src/aec/`, `aec` feature) and the AudioRecord capture shim landed for
this effort still build and are useful (clean STT path / true-16 kHz capture), but the
on-device shim is now the primary AEC. Decide later whether to invest in the host APM
live-path wiring (Plan Phases 1-2) or retire it.

- Note: **on-device perf/audio testing must use `--release` APKs** (debug Rust makes
      inference ~3.6× slower on the 32-bit device and masks the real behavior).

## 3. Google Photos for the slideshow — two selectable backends

The device offers two Google photo backends (plus local gradients), chosen in
Settings → Idle photos:

**(a) Google Photos Ambient API** — the ideal, purpose-built API
(https://developers.google.com/photos/ambient): on-device device-code/QR, user
picks albums in the Google Photos app. Fully built + on-device. **BLOCKED: the
Ambient API is gated behind the Google Photos Partner Program** — `devices.create`
returns `403 PERMISSION_DENIED` ("refer to the partner program") even with a valid
`photosambient.mediaitems` token. Owner is applying; the moment the project is
accepted, this option works with no code change.

**(b) Google Drive** (interim, works today) — `drive.readonly` folder listing.
Consent can't happen on-device (the device-code flow rejects Drive scopes), so it
uses **one-time consent on the Mac** (`tools/google_photo_consent.py`, loopback+PKCE,
a **"Desktop app"** OAuth client) → refresh token adb-pushed → Settings "Import Drive
token". Reads a Drive folder (owned or "Shared with me").

Dead ends ruled out (keep, so we don't relitigate):
- **Photos Library API** — since March 2025 can't read a user's existing library.
- **Device-code/QR + Drive** — `/device/code` rejects `drive.readonly`/`drive.photos.readonly`
  (`invalid_scope`); only `drive.file` passes but it can't list a folder (Picker-only).
- **Ambient scope gotcha** — the device flow rejects `photosambient.tv`; the real
  accepted scope is `photosambient.mediaitems`.

Done:
- [x] `AmbientApiClient` (`display/lib/src/slideshow/ambient_photos.dart`): device-code
      (`requestDeviceCode`/`pollForTokens`), `refresh`, `createDevice` (v4-UUID
      `requestId`), `getDevice` (poll `mediaSourcesSet`), `listMediaItems` (maps each
      `mediaFile.baseUrl` → `PhotoItem` with `=w…-h…` size + Bearer header,
      paginated). Injectable http; covered by `test/ambient_photos_test.dart`.
- [x] Settings two-QR link flow (`qr_flutter`): sign-in QR → create device →
      album-picker QR → poll until `mediaSourcesSet`; stores refresh token +
      `ambientDeviceId` in `AppSettings`.
- [x] Boot refresh + slideshow source (`main.dart` → `AmbientPhotoSource` via
      `photoSourceFromSettings`); falls back to local gradients when unlinked/offline.
      Covered by `test/photo_source_test.dart`.
- [x] Drive backend restored: `listDrivePhotos` (`drive_photos.dart`), Mac helper
      `tools/google_photo_consent.py`, `google_token_import.dart` + Settings "Import
      Drive token", `DrivePhotoSource`. Boot refresh uses the Desktop client.
- [x] Both clients' creds injected at build via `--dart-define-from-file=google_oauth.json`
      (gitignored; `.example` committed): TV client (`GOOGLE_OAUTH_CLIENT_ID/SECRET`,
      Ambient) + Desktop client (`GOOGLE_DRIVE_CLIENT_ID/SECRET`, Drive).
- [x] `AppSettings` carries per-backend state (ambient*/drive*); 60 tests green.

Frame UX (done, verified on-device):
- [x] **Drive works end-to-end on the Echo Show**: Desktop client + Mac consent →
      token synced → Settings **folder picker** (`listDriveFolders`, checkboxes) →
      slideshow of the chosen folders.
- [x] Images use Drive's **`thumbnailLink` sized to ~1600px** (not the full-res
      `alt=media` originals, which were too heavy for the 1 GB device — slow + OOM).
- [x] **Periodic refresh** (`main.dart`, every 30 min): re-mint token + re-list so an
      always-on frame never goes stale when tokens/URLs expire (~1 h). Keeps the
      current photos on a transient refresh failure.
- [x] **Immersive full-screen** (`SystemUiMode.immersiveSticky`) — no status/nav bars.
- [x] **Swipe** left/right to navigate photos (wraps; resets auto-advance; disabled
      mid-conversation via the pointer-transparent scrim).

Remaining:
- [ ] **Ambient:** apply to the Google Photos Partner Program; once accepted it works.
- [ ] Follow-up: refresh tokens are device-local **plaintext** (`shared_preferences`)
      — move to platform secure storage.
- [ ] Polish (optional): lighten the idle scrim further; investigate the device's
      "Offline"/clock-drift (device lost the Mac link + NTP, not a photo bug).
- [ ] Nice-to-have (Ambient): streamlined single-QR flow via the `state` param.

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
      `OnnxSpeakerEmbedder` + `FbankConfig` (`orchestrator/src/speaker/{embed,features}.rs`)
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

Tool calling runs on the **rig** engine, now the default (`orchestrator/src/llm/rig.rs`).
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
      (`display/rust/src/engine/timer.rs`) owns each countdown on the long-lived Network runtime
      (outlives the turn socket), fires a synthesized chime via the shared `PlaybackSink`,
      and emits `WakeWordEventKind.timer{Started,Finished,Cancelled}`. Flutter renders a
      countdown-chip overlay (`display/lib/src/ui/timers_overlay.dart`) visible in idle + turns.
- [ ] **Verify timers on hardware**: "set a 5-minute pasta timer" → chip counts down →
      chime + "time's up" at zero; "cancel the pasta timer" / "cancel all timers"; two
      concurrent timers; a timer keeps running after the Mac disconnects mid-countdown.
- [ ] Consider a dedicated **weather tool** if web-search summaries prove too coarse
      (structured forecast vs. a search snippet).
- [ ] **Query/list timers** by voice ("how long left?") — needs a device→Mac timer-state
      report so the model can answer; today the countdown UI answers visually.
- [ ] **Calendar / reminders** (deferred): pick a backend — macOS EventKit (local,
      no OAuth), Google Calendar (OAuth, needs a client ID), or generic CalDAV.
