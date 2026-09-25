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
- [ ] Run the Anamanti Core: `cargo run --manifest-path anamanti-core/Cargo.toml --release`
      (advertises `_wyoming._tcp`; env in `anamanti-core/src/config.rs`).
- [ ] Install the release APK on the Echo Show and run one full turn:
      wake word → STT → LLM → TTS playback, with the transcript/reply rendered live.
- [ ] Confirm **mDNS discovery** works across the real network (no hardcoded IP).
- [x] Exercise **barge-in** (wake word during playback) on-device — works: a wake word
      mid-reply flushes playback and starts a fresh turn (flush-on-wake + the
      `anamanti-interrupt` frame aborts the Anamanti Core's in-flight LLM+TTS). Detection
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

### Reinstalled on the production Echo Show (2026-09-25) + mic-gain tuning

- [x] **Installed the shim on the production device** from the version-controlled
      [EchoShow8gen1-aec-shim](https://github.com/Brutus-GTC6245/EchoShow8gen1-aec-shim)
      repo via `scripts/install.sh --pga 40 --log 1` (prebuilt armv7 `.so`, no build).
      Verified loaded: `LD_PRELOAD=libamznaec_shim.so` in the live
      `android.hardware.audio.service`, the `.so` mapped in, and the `amznaec 5s:
      ref/mic in/out (speex linear)` telemetry ticking once capture opens. Persists
      across reboot (on-disk `/vendor` `.so` + init-rc `setenv` + `persist.*` props;
      `.orig` backups kept, revert via `scripts/uninstall.sh`).
- [x] **Mic-gain tuning — important.** The doc-recommended **PGA 80→40 (by design)**
      dropped the *streamed* mic level ~15 dB (idle wake-word-diag rms ~0.003 → ~0.0005;
      the shim's default `gain_db=20` makeup didn't fully offset the ~20 dB analog cut).
      The wake word still fired, but the **Core's energy VAD intermittently missed
      speech onset** (`speech_started=false` → empty transcript → the device shows
      "processing" then no reply; a retry works). Seen live on a "show me the steps"
      recipe command. **Fix:** raised the shim's makeup gain
      `setprop persist.vendor.amznaec.gain_db 34` (from 20), which restores idle rms
      ~0.003 **without touching the by-design PGA**; since `gain_db` is applied *after*
      cancellation, the echo-suppression ratio is unchanged. Persists (`persist.` prop).
      If very close/loud near speech ever clips, dial to ~30; alternatively lower the
      Core's `voice_rms_threshold` instead of raising device gain.
- [ ] **Set `persist.vendor.amznaec.log 0`** on this device for daily use — telemetry
      is currently on (`log 1`) from the install. (Same as follow-up #1 below.)

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
WebRTC APM (`anamanti-core/src/aec/`, `aec` feature) and the AudioRecord capture shim landed for
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
uses **one-time consent on the Mac** — now run **inside the Anamanti Core**
(`anamanti-core/src/drive_consent.rs`, loopback+PKCE, a **"Desktop app"** OAuth
client) and driven from the config page **Photos tab** (`/drive`). The Anamanti Core
stores the client id/secret + refresh token + folder ids and the device **pulls the
bundle over Wyoming** (`anamanti-get-drive-token`); it mints access tokens on-device.
Reads a Drive folder (owned or "Shared with me"). *(Migrated 2026-09-22 from the
standalone `tools/google_photo_consent.py` + adb-push + Settings "Import" flow.)*

Dead ends ruled out (keep, so we don't relitigate):
- **Photos Library API** — since March 2025 can't read a user's existing library.
- **Device-code/QR + Drive** — `/device/code` rejects `drive.readonly`/`drive.photos.readonly`
  (`invalid_scope`); only `drive.file` passes but it can't list a folder (Picker-only).
- **Ambient scope gotcha** — the device flow rejects `photosambient.tv`; the real
  accepted scope is `photosambient.mediaitems`.

Done:
- [x] `AmbientApiClient` (`anamanti-display/lib/src/slideshow/ambient_photos.dart`): device-code
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
- [x] Drive backend: `listDrivePhotos` (`drive_photos.dart`), `DrivePhotoSource`.
      Boot refresh mints access tokens on-device from the synced client creds.
- [x] **Drive consent moved into the Anamanti Core (2026-09-22).** New
      `anamanti-core/src/drive_consent.rs` (loopback+PKCE) + config-page Photos tab
      (`/drive`, `/drive/save`, `/drive/link`, `/drive/status.json` in
      `webconfig.rs`); creds+token persisted in `DriveConfig` inside
      `anamanti_settings.json`. New Wyoming control `anamanti-get-drive-token`
      (`control.rs` + both `protocol.rs` copies); device `get_drive_token`
      (`anamanti-display/rust/.../control.rs` + FRB `DriveToken`); Dart pulls it on boot /
      photo-refresh (`main.dart`) and via Settings **"Sync Drive from Mac"**. The
      standalone `tools/google_photo_consent.py`, `google_token_import.dart`, and the
      build-time `GOOGLE_DRIVE_*` dart-defines were **removed** — the APK is now
      credential-free for Drive; `AppSettings.driveConfigured` is a runtime check.
- [x] Only the TV (Ambient) client creds are injected at build via
      `--dart-define-from-file=google_oauth.json` (gitignored; `.example` committed):
      `GOOGLE_OAUTH_CLIENT_ID/SECRET`. The Mac's Drive client is set via
      `ANAMANTI_GOOGLE_DRIVE_CLIENT_ID/SECRET` (optional `_FOLDER_IDS`).
- [x] `AppSettings` carries per-backend state (ambient* + drive* incl. synced
      `driveClientId`/`driveClientSecret`); Anamanti Core + device + Dart tests green.

Frame UX (done, verified on-device):
- [x] **Drive works end-to-end on the Echo Show**: Desktop client + Mac consent →
      token synced → Settings **folder picker** (`listDriveFolders`, checkboxes) →
      slideshow of the chosen folders. *(Re-verify on hardware after the 2026-09-22
      migration of consent into the Anamanti Core + Wyoming token delivery.)*
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
- [ ] Follow-up: refresh tokens are **plaintext** on both sides — device-local
      (`shared_preferences`, incl. the synced Drive client secret) and the
      Anamanti Core's `0600 anamanti_settings.json` — and the Drive bundle rides the
      device↔Mac Wyoming hop **unencrypted**. Move device secrets to platform secure
      storage; consider a paired/TLS control channel if the LAN isn't trusted.
- [ ] Polish (optional): lighten the idle scrim further; investigate the device's
      "Offline"/clock-drift (device lost the Mac link + NTP, not a photo bug).
- [ ] Nice-to-have (Ambient): streamlined single-QR flow via the `state` param.

## 3a. Multi-device / multi-Anamanti Core (implemented 2026-09-22 — verify on hardware)

Code + tests landed (Plan.MD decision table 2026-09-22): Anamanti Core advertises
`role`/`name`/`instance_id` TXT; device filters by `role=core`, enumerates
all (`list_orchestrators`), and pins a chosen `instance_id` via the settings
**Anamanti Core** dropdown (strict offline; `"Auto"` = first responder). The key
steers both the voice-turn path and the settings/control path. SQLite opens WAL.

- [ ] **Two displays, one Anamanti Core**: run a real turn from each Echo Show
      concurrently; confirm each reply lands on the display that asked and both
      share the same memory pool.
- [ ] **Pick a test Anamanti Core on-device**: launch prod + a test instance
      (distinct `ANAMANTI_SERVICE_NAME`/`ANAMANTI_INSTANCE_ID`/`ANAMANTI_BIND_ADDR`/
      `ANAMANTI_CONFIG_ADDR`); confirm both appear in the dropdown, selecting the
      test one routes turns there (check its logs), and stopping it makes the
      display go **Offline** (does *not* fall back to prod). Switch to `"Auto"` →
      reconnects.
- [ ] **Validate HelixDB cross-process open** before promising shared Helix data:
      two Anamanti Core processes opening the same `anamanti_helix` graph. If it
      single-locks/errs, keep the test instance on `ANAMANTI_MEMORY_BACKEND=sqlite`
      (shared SQLite/WAL data, FTS recall) as documented; otherwise lift that note.

## 4. Ops & deployment polish

- [ ] Run the Anamanti Core as a managed service on the Mac (launchd/login item) so it
      survives restarts.
- [ ] Document the concrete LAN setup (server versions, ports, Piper voice, model
      choices) in `README.md` from a real deployment.
- [ ] Confirm auto-reconnect/backoff behavior end-to-end when the Mac goes away and
      returns (slideshow keeps running; subtle offline chip; wake words queue).

## 5. Per-person speaker identification (this branch — finish to production)

Phases A–D **and** the device "People" UI are implemented, tested, and merged
(see [`speaker_id_plan.md`](./speaker_id_plan.md)): passive voiceprint +
auto-clustering, per-person SQLite + GraphRAG memory, the prompt identity line,
voice naming ("my name is …"), and the `anamanti-*speaker*` control frames. It is
**not running yet** — `speaker.enabled` is false (household) and there is no real
embedding model on the Mac. (The ONNX embedder is now always compiled in — there is
no longer a `speaker` build feature — so with no `speaker.model_path` set it just
falls back to the dev-only mock embedder.) To take it from dormant code to a real
feature:

### 6a. Real embedding model (Phase E — the blocker for accuracy)

- [ ] Obtain/export an **ECAPA-TDNN** (or WeSpeaker) speaker-embedding model to
      **ONNX** and place it on the Mac.
- [ ] Confirm the model's **input contract** (raw waveform vs. precomputed log-mel;
      tensor layout `[1, frames, mels]` vs `[1, mels, frames]`) and align
      `OnnxSpeakerEmbedder` + `FbankConfig` (`anamanti-core/src/speaker/{embed,features}.rs`)
      to it; set `ANAMANTI_SPEAKER_EMBED_DIMS` (192 for ECAPA).
- [ ] Set **`speaker.model_path`** in `anamanti.json` to the ONNX model (else it
      falls back to the dev-only mock embedder). No build feature is needed — the
      ONNX embedder is always compiled in.

### 6b. Enable + calibrate on hardware

- [ ] Set **`speaker.enabled = true`** (plus the model path) in `anamanti.json` and
      fold it into the launchd/run script from §4.
- [ ] **Calibrate** `ANAMANTI_SPEAKER_MATCH_THRESHOLD` / `_NEW_THRESHOLD` /
      `_MIN_SPEECH_MS` against real Echo Show far-field captures; measure EER and
      record the chosen operating point.
- [ ] Verify per-person **memory scoping** + the prompt identity line with 2+ real
      speakers (Sam vs. Dana get separate facts; the reply greets by name).
- [ ] Verify the **People** settings screen end-to-end on the device (list, name,
      merge, forget) and that anonymous clusters show as "Speaker N".
- [ ] Verify **GraphRAG per-`User`** attribution with `ANAMANTI_MEMORY_BACKEND=helix`
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
- [ ] Persist a last-known copy of Anamanti Core settings on the device for display
      while the Mac is offline.

## 6a. Proactive notifications (Approach A — shipped visual-only 2026-09-23)

The Anamanti Core can push visual notifications to the display over a persistent,
device-dialed Wyoming channel (`anamanti-hello` → `anamanti-notify`). Device: `start_notify_channel`
+ `rust/src/wyoming/notify.rs` (reconnect/backoff) → `NotifyEvent` FRB stream →
`NotificationController` + `NotificationBanner`. Anamanti Core: `NotificationService`
registry + server `anamanti-hello` arm + config-page **Notify** tab (`POST /notifications/test`).
Verified: mac `cargo test` + `clippy -D warnings`; device `cargo test` + `clippy -D warnings`;
`flutter analyze` + `flutter test` (incl. `notification_controller_test.dart`); FRB codegen.

Remaining (next phases, in rough order):
- [ ] **Verify on hardware**: hold the notify channel open, push from the Notify tab,
      confirm the banner appears on the Echo Show and reconnects after a Mac restart.
- [ ] **Spoken notifications + barge-in**: an opt-in `speak` flag that voices a
      notification via the playback path when idle, integrated with the wake-word
      state machine (queue while a turn is active; a wake word cancels it) + quiet hours.
- [ ] **Reliable delivery**: send `anamanti-notify-ack` (constructor already exists in
      both crates), SQLite store-and-forward across reconnects, TTL/dedup — so a
      notification isn't lost if the device is briefly disconnected (today it's dropped).
- [ ] **Per-device targeting**: `NotificationService::notify` currently broadcasts to
      every connected channel; key by `device_id` for multi-display homes.
- [ ] **Real producers**: wire the first non-test producer (calendar reminders from the
      existing `calendar.subscriptions` feed; later timers/alerts).
- [ ] **Security**: the notify channel widens the same unauthenticated-LAN surface the
      voice/control frames have (a push can render arbitrary text on the display).
      Gate behind the planned paired/TLS control channel (see §3/§7 secret-handling).

## 7. Tools & actions (agent capabilities)

Tool calling runs on the **rig** engine, now the default (`anamanti-core/src/llm/rig.rs`).
Adding a capability = registering one tool in `Tools`. See Plan §3 "Tool calling &
actions".

- [x] **Weather context**: inject a configured home location + units
      (`ANAMANTI_HOME_LOCATION` / `ANAMANTI_WEATHER_UNITS`) into the system prompt so
      "what's the weather" resolves "here" (`orchestrator::location_line`).
- [x] **Canonical household record**: the Anamanti Core now holds a persisted
      `Household` (home location + units + a roster of people with emails/phones,
      `settings::Household`). Location/units seed from the env at first boot, then are
      editable — along with the people — from the config page **Household tab**
      (`/household`); the prompt reads location + roster from the per-turn settings
      snapshot (`orchestrator::household_line`), so edits apply with no restart.
- [x] **Directions origin tracks the live household location**: the `directions_lookup`
      default origin now reads a shared `directions::LiveHomeLocation` handle
      (`LlmFactory.home_location`) instead of the startup env, so editing the home
      location on the Household tab changes the route origin with no backend rebuild
      (`SharedSettings::apply_household` calls `set` on the same cell).
- [x] **Speaker↔household reconciliation**: when an identified speaker's name matches a
      household member (case-insensitive, `Household::member_matching`), the prompt's
      identity line uses the roster's **canonical** name and notes their relationship
      ("You are speaking with Alice (parent), who lives here.") —
      `orchestrator::speaker_identity_line`.
- [ ] Still open: expose the roster **to the device** (a Flutter settings screen +
      Wyoming control frames) so it can be edited without the config page — larger,
      cross-crate (FRB codegen + protocol frames in both crates + Flutter UI).
- [x] Make **rig the default engine** + web search on by default (was opt-in).
- [ ] Verify on hardware: ask "what's the weather" with `ANAMANTI_HOME_LOCATION` set
      and a Tavily key — confirm the model calls `internet_search` and speaks a
      location-correct answer.
- [x] **Device-action framework**: the `anamanti-timer` frame + `set_timer`/`cancel_timer`
      tools emit a `DeviceAction` onto a per-turn channel that the Anamanti Core drains
      and relays on the turn socket (`orchestrator::drain_device_actions`). The seam is
      `LlmTurn.actions` (an `ActionSink`).
- [x] **Timers/alarms on-device** (unlimited concurrent): Rust `TimerManager`
      (`anamanti-display/rust/src/engine/timer.rs`) owns each countdown on the long-lived Network runtime
      (outlives the turn socket), fires a synthesized chime via the shared `PlaybackSink`,
      and emits `WakeWordEventKind.timer{Started,Finished,Cancelled}`. Flutter renders them
      (`anamanti-display/lib/src/ui/timers_overlay.dart`) two ways: a big, screen-filling display on
      the idle screen (one timer fills the screen, 2–3 sit side by side, 4+ tile into a grid;
      each shows its name, a large mm:ss readout, and a circular ring that drains full→empty),
      and a compact chip row top-center while a conversation is on screen (the turn wins the
      screen). Unnamed timers read "Timer" (numbered only when several coexist).
- [x] **Timer alarm = bell + Piper voice**: on fire the device rings a two-strike bell
      (`alarm_pcm`), then requests "Time's up for {name}" from the Anamanti Core via the
      project-local `anamanti-speak` frame — the Mac synthesizes it with Piper and streams
      the audio back into the same `PlaybackSink` (plays right after the bell). Bell-only
      when the Mac is unreachable (offline). See `Pipeline::announce` + `server.rs`.
- [x] **Immersive kiosk display**: `SystemUiMode.immersiveSticky` in `main()` hides the
      status + navigation bars (verified on device); the big timer layout also wraps in a
      `SafeArea` as belt-and-suspenders.
- [ ] **Verify timers on hardware (audio)**: "set a 5-minute pasta timer" → ring drains +
      counts down → **bell rings twice then "Time's up for pasta"** at zero; "cancel the
      pasta timer" / "cancel all timers"; two concurrent timers; a timer keeps running (and
      still bells) after the Mac disconnects mid-countdown. (Needs a spoken turn to set the
      timer + the Mac Anamanti Core + Piper running; not drivable headlessly.)
- [x] **Dedicated weather tool + display** (2026-09-25, see `WeatherPlan.md`): the
      `weather_lookup` / `close_weather` rig tools fetch a structured current + 7-day
      forecast from the keyless **Open-Meteo** API (behind a `WeatherProvider` trait) and
      push it over a new **`anamanti-weather`** frame. The device renders a full-screen
      **weather screen** (today's conditions with big imagery + a 7-day row) on a voice
      ask, and shows a **small icon + current temperature beside the idle clock** kept
      fresh by an always-on periodic push (`WeatherService` → the persistent
      `role=weather` channel). Suites green; **pending on-device QA** (§4 of the plan).
- [x] **Directions / traffic (voice-only)** — shipped: the `directions_lookup` rig info
      tool returns real distance, travel time, and **live traffic** between two places
      (`driving`/`walking`/`cycling`). Lives in `anamanti-core/src/directions/` behind a
      `DirectionsProvider` trait; v1 backend is **Mapbox** (`MAPBOX_TOKEN`, provider
      selected by `ANAMANTI_DIRECTIONS_PROVIDER=mapbox`), geocoding v6 + Directions v5
      `driving-traffic` (traffic-aware ETA + typical-time delta → a spoken "traffic is
      heavy/normal/light" note). The origin defaults to `ANAMANTI_HOME_LOCATION`;
      distances follow `ANAMANTI_WEATHER_UNITS`. Absent token → tool not advertised.
- [ ] **Directions — follow-ups** (deferred): (a) alternate providers behind the same
      `DirectionsProvider` trait — Google (best traffic, stricter ToS), HERE/TomTom, or
      keyless OSRM (no live traffic); (b) **render the route/map on the display** (v1 is
      voice-only) — a Flutter map surface fed a polyline over FRB; (c) transit/departure
      times and multi-stop; (d) verify on hardware ("how long to drive downtown?" with a
      `MAPBOX_TOKEN` + `ANAMANTI_HOME_LOCATION` set → model calls `directions_lookup` and
      speaks a traffic-correct ETA).
- [ ] **Query/list timers** by voice ("how long left?") — needs a device→Mac timer-state
      report so the model can answer; today the countdown UI answers visually.
- [x] **Calendar (read-only, web .ics)** — shipped: the `calendar_lookup` rig info tool
      reads one or more web iCalendar subscriptions (`ANAMANTI_CALENDARS`, `webcal://`
      accepted), expands `RRULE` recurrences in the query window, and filters by
      time / person (fuzzy name match) / free text. Lives in `anamanti-core/src/calendar/`
      behind a `CalendarSource` trait.
- [ ] **Calendar — follow-ups** (deferred): (a) authenticated **CalDAV**
      (`REPORT calendar-query`, which unlike a static `.ics` supports server-side
      `time-range` filtering) and/or macOS **EventKit** / Google Calendar OAuth
      sources behind the same `CalendarSource` trait; (b) canonicalize the `person`
      filter against HelixDB `Entity`/`User` nodes (disambiguate "Mike" via the graph)
      instead of matching only the names present in the feed; (c) write actions
      (create/RSVP) — out of scope for the read-only cut; (d) **background refresh +
      stale-while-revalidate** so lookups are always served from a warm cache and a
      slow/failing feed falls back to last-good bodies, plus conditional GET
      (ETag/`Last-Modified`) + gzip to make refreshes cheap. (Today: a synchronous
      TTL cache, `ANAMANTI_CALENDAR_CACHE_TTL`, default 300 s.)
- [ ] **Reminders** (deferred): pick a backend — macOS EventKit (local, no OAuth),
      Google Tasks/Calendar (OAuth), or generic CalDAV.
- [x] **Recipe voice navigation + screen context (2026-09-25)** — the guided-recipe
      screen ([`RecipePlan.md`](./RecipePlan.md)) is drivable by voice: switch tabs and
      scroll the panes (and close it) via a new **`recipe_control`** rig tool →
      `navigate`/`scroll` sub-actions on the `anamanti-recipe` frame (both `protocol.rs`
      files, round-trip tested). The device sends **display context** to the Core — a
      **general, extensible** channel (`protocol::display_context` → a `DisplayContext`
      enum, recipe being the first screen): each turn's `audio-start` carries a `screen`
      block discriminated by `kind` (today `recipe`: title, tab, scroll at-top/at-bottom,
      counts), injected into the turn's system prompt via `display_context_line` (one arm
      per screen). First device→Core context channel; piggybacks on `audio-start`. A new
      voice-controllable screen (music/weather/photos) = a `DisplayContext` variant + a
      prompt-line arm + a device `set_<screen>_context` setter. New
      `RecipeNavigate`/`RecipeScroll` FRB events + a `set_recipe_context` FRB call; touch
      stays in sync via `RecipeController`/`AssistantState`.
- [ ] **Verify recipe voice control on hardware**: with a recipe up, say "show the
      ingredients" / "go to the steps" / "scroll down" / "back to the top" / "close the
      recipe" and confirm the screen reacts and the model reliably calls
      `recipe_control` / `close_recipe` (needs the Mac Anamanti Core + a spoken turn;
      not drivable headlessly).
