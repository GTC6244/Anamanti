# Camera Plan: the front-door camera on the display

**Targets:** M4 Mac Mini (Anamanti Core — NVR access, credentials, events) + Echo Show 8
(the camera view)
**Feature:** show the Reolink **"Front door"** camera full screen on the Anamanti Display
the user is talking to (or touching), on demand. An automatic "someone's at the door"
pop-up is **deferred** and will be wired later.

> **Status:** **Implemented (2026-10-20), pending on-device QA.** Shipped = voice-triggered
> **live video** (option B) through the vendored+patched `video_player_android` (option A).
> §7 ("As built") is authoritative wherever it differs from the §2–§4 proposal text. Tests:
> Core lib 712, protocol 34, device-rust 122, Flutter 282, all green; clippy `-D warnings` +
> `dart analyze lib test` clean; release APK builds. The Core was verified end to end with a
> test copy (speech → fast path → `anamanti-camera` show frame → stream reply). Catalogued
> in [`DisplayUI.md`](./DisplayUI.md), [`Plan.MD`](./Plan.MD) and
> [`architecture.md`](./architecture.md) §4.
>
> Read together with [`PlacesPlan.md`](./PlacesPlan.md) / [`WeatherPlan.md`](./WeatherPlan.md)
> (the full-screen view + frame template), [`HomeAssistantPlan.md`](./HomeAssistantPlan.md)
> (Part A's zero-model fast path; Part B's "device pulls a credential from the Core, keeps it
> in memory only" pattern and its on-device spike), and architecture.md §4 (the persistent
> push channels: `ChannelRegistry` / `push_channel::run`).

---

## 0. Confirmed by the owner (2026-10-20)

| Question | Answer |
| --- | --- |
| Dedicated view-only NVR user | **OK.** The owner creates it on the RLN8-410; its password is the `REOLINK_PASSWORD` Core secret. |
| Camera type | "Front door" is a **regular camera**, not a doorbell. There is no button-press event and no doorbell audio. |
| Home Assistant | Reolink is **not** in Home Assistant. Camera access is direct to the NVR. |
| Event polling | **Do not poll the NVR for events** (no `GetMdState`/`GetAiState` loop). The automatic pop-up is deferred; its trigger will be designed later. |
| Which display | **The display the user is talking to** (or touching) gets the camera view. It is never broadcast to every display. This falls out of the design: the voice fast path / tool replies on *that* display's turn socket, and a touch open is local to that display. |

## 1. What we know about the hardware (probed 2026-10-20)

| Item | Value |
| --- | --- |
| NVR | Reolink **RLN8-410** (8-channel PoE NVR) |
| Address | `192.168.1.29` (MAC `ec:71:db:…`, a Reolink prefix). It goes in Core config, like `home_assistant.base_url`. The NVR is a configured endpoint, not something Anamanti discovers, so this is **not** an IP fallback for finding the Core. |
| Open ports | 80 (302 → HTTPS), 443 (HTTPS API, nginx, self-signed cert), **554 (RTSP, answering)**, 1935 (RTMP/FLV), 8000, 9000 (Reolink "Baichuan" native protocol) |
| Camera | **"Front door", NVR channel 5.** The HTTP API counts channels from 0, so it is **`channel=4`** there. RTSP counts from 1 with two digits, so it is **`h264Preview_05_*`**. |
| Login lockout | The API reports `remain_times: 10` on a failed login. Bad credentials lock the account, so the Core must **not** retry a failing login in a loop (back off and raise an error on the config page). |

Endpoints for channel 5:

```
Snapshot (JPEG):  https://192.168.1.29/cgi-bin/api.cgi?cmd=Snap&channel=4&rs=<random>&token=<token>
Login (token):    POST https://192.168.1.29/cgi-bin/api.cgi?cmd=Login   (token lasts ~3600 s)
RTSP sub-stream:  rtsp://<user>:<pass>@192.168.1.29:554/h264Preview_05_sub
RTSP main:        rtsp://<user>:<pass>@192.168.1.29:554/h264Preview_05_main
```

(The NVR also has motion/person state commands, `GetMdState`/`GetAiState`. They are **not
used**: per §0, nothing polls the NVR for events.)

**Verified with the `anamanti` user (2026-10-20):**

| Check | Result |
| --- | --- |
| Login as `anamanti` | OK, token lease 3600 s. Firmware `v3.6.0.384_25051916`. The API reports 12 channel slots, of which 5 are online. |
| Channel | API index 4 = **"Front Door"** (online). Snapshot confirmed it shows the porch. |
| Main stream | **H.265**, 3840×2160, 25 fps, 6 Mbps. Not used on the display. |
| Sub-stream | **H.264 High, 640×360, 10 fps, 256 kbps**, plus an AAC 16 kHz audio track, which the display ignores. `ffprobe` over RTSP/TCP on `h264Preview_05_sub` works. |
| Snapshot | ~600 KB JPEG at full 4K. If the fallback uses it, request a smaller size or downscale on the Core. |
| Echo Show reach | All three Echo Shows (`.32`, `.54`, `.63`, LineageOS on Android 11) get `RTSP/1.0 200 OK` from `192.168.1.29:554`. |
| Device decoders | Hardware `OMX.MTK.VIDEO.DECODER.AVC` (and HEVC) are present, so ExoPlayer can decode the sub-stream in hardware. |

**The display uses only the sub-stream.** It's small and H.264. HEVC hardware decode is
listed, but 4K is too heavy for this device.

### W0 spike results (2026-10-20) — PASSED

A throwaway Flutter app (outside the repo; uninstalled afterwards) played
`h264Preview_05_sub` full screen on the Echo Show at `192.168.1.32`.

| Check | Result |
| --- | --- |
| Toolchain | `video_player` 2.14.1 / `video_player_android` 2.12.2 (media3 ExoPlayer 1.9.2, which ships RTSP support) **builds on the locked AGP 8.7.3 / Kotlin 2.1.0 / Gradle 8.14.3 / Flutter 3.44.1**, release, `android-arm`. The only output is Flutter's "support for this AGP/Kotlin version will soon be dropped" warnings, which the main app gets too. The spike APK was 13 MB in total. |
| **Texture view: FAILS** | With the default `VideoViewType.textureView`, the MTK hardware decoder (`OMX.MTK.VIDEO.DECODER.AVC`) can't allocate output buffers on Flutter's texture surface (`Failed to allocate buffers after transitioning to IDLE`, then `MediaCodec.start failed`). **No video.** |
| **Platform view: WORKS** | With **`viewType: VideoViewType.platformView`** (a native SurfaceView), the hardware decoder starts and the video plays live and smoothly. Two screenshots 5 s apart showed the camera's clock advancing. **This is a hard requirement for `CameraView`.** |
| Time to first frame | UDP default: ~8.2 s, because ExoPlayer waits its 8 s `DEFAULT_TIMEOUT_MS` for UDP before falling back to TCP. **With `RtspMediaSource.Factory().setForceUseRtpTcp(true)`: 5.0–6.6 s** (3 runs). The remainder is ExoPlayer's default start buffer (~2.5 s) plus waiting for a keyframe. |
| Latency | **~1–3 s** behind real time (the camera's on-screen clock against a host-timestamped screenshot; the NVR clock matches the Mac). |
| CPU / RAM | Spike app ~10–13 % CPU, `media.codec` ~3 %, surfaceflinger ~3–8 %. App PSS ~85 MB (Graphics ~37 MB). The device had ~1 GB `MemAvailable` of 1.9 GB with the main Anamanti app (~35 % CPU, ~220 MB RSS) running alongside. Comfortable. |
| Live position | `VideoPlayerValue.position` stays at ~0 for a live RTSP stream, so the UI must not use it as a "stalled" signal. Use `isPlaying`/`isBuffering`/errors. |

**Implications for the build:**
1. `CameraView` must use `VideoViewType.platformView`.
2. Forcing RTP-over-TCP and a low-latency start buffer are **not exposed** by
   `video_player`. Options: (a) a small vendored patch of `video_player_android`
   (`setForceUseRtpTcp(true)`, a smaller `DefaultLoadControl` buffer for RTSP); (b) a minimal
   in-app Kotlin ExoPlayer platform view (it's presentation-only, so the FRB boundary rule
   isn't affected, but it's more code to own); (c) accept ~8 s with the stock plugin. **To decide.**
3. On the NVR, setting the Front Door sub-stream's **I-frame interval to 1×** (currently
   `gop: 4`) should cut the keyframe wait. It's a free change with no code.
4. Ignore the AAC audio track (mute/disable it). Video only, per §2.

## 2. Proposed decisions

| Question | Proposal |
| --- | --- |
| **Credentials** | Create a dedicated **view-only NVR user** (e.g. `anamanti`) on the RLN8-410. Its password is a **Core secret**: env `REOLINK_PASSWORD`, settable from the config page (masked, 0600), never in `anamanti.json`. **The APK stays credential-free** in Phases 1–2, because the Core fetches the frames. |
| **Config** | `camera{ enabled, nvr_url, username, cameras:[{name, channel}] }` in `anamanti.json`, seeded with `{"name":"Front door","channel":5}`. `channel` is the **1-based number the NVR UI shows**, and the Core converts it to the 0-based API index, so users never see the off-by-one. Edited on Integrations → **Camera** (`/integrations#camera`), with a read-only "Test connection" that does one login + one snapshot. |
| **TLS** | The NVR uses a self-signed cert. Use a **dedicated** reqwest client that accepts the self-signed cert for the configured NVR host only. Do **not** weaken the shared `crate::http` pool. A mandatory timeout still applies. |
| **Transport, Phases 1–2** | **The Core proxies snapshots.** A display opens a `role="camera"` push channel only while the view is up. The Core's `CameraService` (a `ChannelRegistry` wrapper, like weather/music) fetches `Snap` at ~2 fps and pushes JPEGs only to subscribed displays. Nothing is polled when nobody is watching. This reuses the existing channel design, keeps credentials on the Mac, and is cheap: ~50 KB × 2 fps over the LAN. |
| **Transport, Phase 3 (live)** | Live RTSP has to go device → NVR directly. The device pulls `{rtsp_url}` (credentials included) from the Core per open (`anamanti-get-camera-stream` → `anamanti-camera-stream`, modelled on the HA dashboard token), holds it **in memory only**, and plays the **sub-stream** through ExoPlayer (hardware MediaCodec). Fall back to Phase 1 snapshots if playback fails. |
| **Audio** | **Video only.** Doorbell/camera audio or two-way talk would be a second audio path, which a locked decision forbids without confirmation. |
| **Voice trigger** | A **zero-model fast path** before System-1 (string match: "show the front door", "who's at the door", "show me the door camera"). It sends an `anamanti-camera` frame (`action: show/dismiss`, `camera: "Front door"`), and the reply is a short fixed phrase. A `show_camera` rig tool (+ `close_camera`) covers wording the fast path misses. |
| **Touch trigger** | An idle-screen camera icon, shown only when the device-local setting `AppSettings.cameraEnabled` is on (default off, Settings → Display). |
| **Dismiss** | ✕ button, "stop"/"close it" through the existing `stop_dismiss` ladder (display context `{kind:"camera"}` on `audio-start`), and an **auto-close timeout** (default 60 s) so a forgotten view doesn't hold the screen awake and the NVR busy. |
| **Display view** | `CameraView`: one more full-screen `AnimatedOpacity` layer in `AmbientScreen`'s Stack, next to Recipe / Weather / Place. A `gaplessPlayback` `Image.memory` swaps frames without flicker. Overlays show the camera name and a "last frame Ns ago" stale indicator. Frames are decoded at the display size so memory stays within the ~1 GB budget. |
| **Locality** | All camera traffic stays on the LAN. It is shown as **Local** on the Overview page's locality chips, and nothing leaves the house. |

## 3. Phases

**W0 — spike (no production code).** Create the NVR user. With it: check the sub-stream
codec/resolution with `DESCRIBE`, time `Snap` latency for channel 4, and confirm the Echo Show
can reach `192.168.1.29` (same subnet). For Phase 3 also check: does a pinned
`video_player` / media3 RTSP setup build on the locked AGP 8.7.3 / Gradle 8.14.3 / Flutter
3.44.1, and does it play `h264Preview_05_sub` smoothly on the armeabi-v7a device? (APK
size, RAM, and CPU while playing.) An AGP-9-only plugin blocks Phase 3 but not Phase 1.

**Phase 1 — on-demand snapshot view.** Core: `camera` config, `CameraService` (login/token
refresh with lockout-safe backoff, `Snap` fetch), the `role="camera"` push channel, the
voice fast path + `show_camera` tool, and the config-page drawer. Shared protocol:
`anamanti-camera` frame + the JPEG push payload, defined once in `anamanti-protocol`
with round-trip tests. Device: `CameraView`, the idle icon + setting, display context, and
auto-close. Update `DisplayUI.md`, `architecture.md`, `Plan.MD`, and `README.md`.

**Phase 2 — automatic pop-up: DEFERRED.** The owner will wire this later. Constraints
already known: it must **not** poll the NVR every second (§0), and there is no HA or
doorbell event to use. Candidates for later: the NVR's push channel on port 9000
(Baichuan), ONVIF event subscription, or an external trigger. The Phase 1 `show` path
(the `anamanti-camera` frame) is what the eventual trigger will call.

**Phase 3 — live video.** The sub-stream RTSP path from §2, if W0 shows it plays well.
Snapshots stay as the fallback.

## 4. Pieces (Phase 1)

| Piece | Side | Notes |
| --- | --- | --- |
| `camera` config + `REOLINK_PASSWORD` secret | Core | `config.rs`, `anamanti.example.json`, settings overlay; channel is 1-based in config |
| `CameraService` / `ReolinkClient` behind a `CameraSource` trait | Core | Fixture-tested offline (canned Login / Snap / error responses, including the lockout reply) |
| Fast path + `show_camera` tool | Core | Fast path goes before System-1 (pure string reject, no I/O); tool registered in `Tools::build`; update the advertised-tools snapshot |
| `anamanti-camera` frame, `role="camera"` channel payload | protocol | One definition, both crates re-export |
| Push channel client | device Rust | `push_channel::run` + `start_channel("camera")` while the view is open; FRB stream of JPEG bytes |
| `CameraView`, setting, idle icon | Flutter | `lib/src/ui/camera_view.dart`; Dart tests for open/close/auto-close/stale state |
| Config-page drawer | Core web UI | Integrations tile + "Test connection"; regenerate `panel.css` |

## 5. Risks

- **NVR load / lockout:** polling only while a view is open, a single shared token, and
  backoff on auth failure keep this safe. A misconfigured password must never burn through
  the 10 attempts.
- **Device decode (Phase 3):** an old 32-bit SoC plus a WebView/ExoPlayer stack is the main
  unknown, which is why the W0 spike comes first and the Phase 1 snapshots stay as a fallback.
- **Snapshot cadence:** ~2 fps is enough to see who's at the door but it isn't video.
  That's acceptable for Phases 1–2.

## 6. Open questions

1. The automatic pop-up's trigger (deferred, §3 Phase 2).

**Decided 2026-10-20 — Option B: live video is the primary view.** Live RTSP of the
sub-stream (`h264Preview_05_sub`, played on the device through ExoPlayer, with the URL pulled
from the Core per open and held in memory only) becomes the shipped view as soon as the W0
spike passes. Core-proxied snapshots are kept only as the fallback when playback fails. That
means Phase 3 is built straight after W0, together with Phase 1, rather than later.

Resolved 2026-10-20 (§0): doorbell vs camera, Home Assistant, which display, the NVR user.

## 7. As built (2026-10-20)

This section supersedes the §2–§4 proposal text where they differ.

**Shipped**
- **Trigger:** voice only. The pre-System-1 **camera fast path** (`anamanti-core/src/orchestrator/camera.rs` →
  `CameraHub::match_utterance` in `anamanti-core/src/camera/mod.rs`) runs right after the Home
  Assistant fast path:
  - **Show:** "show (me) the front door (camera)", "bring up / pull up / let me see …",
    "who's at the door", "is someone at the door". "Open/turn on" only counts together
    with the word "camera". **"Open the front door" is deliberately not matched**, because
    it could be a lock command.
  - **Close** (only while the view is open): "stop" / "close it" / "that's enough" /
    "thanks" / "close the camera". A bare "stop" still goes to a running timer first,
    matching the System-1 stop ladder.
  - **Reply:** a fixed spoken "Here's the Front Door." / "Okay." that ends the session.
- **LLM fallback:** the `show_camera` (optional `camera` name; the description lists the
  configured cameras and says the model can't see the video) and `close_camera` tools. They
  are advertised only when the camera is configured.
- **Transport:**
  1. The `anamanti-camera` show/dismiss frame names the camera only.
  2. The device then sends `anamanti-get-camera-stream`.
  3. The Core replies `anamanti-camera-stream {ok, camera, url}` (or `{ok:false, message}`).
  4. The URL is `rtsp://<user>:<pw>@<host>:<port>/h264Preview_<NN>_sub`, with the
     credentials percent-encoded. It is never logged by the Core (only the event type is
     logged) or the device (`CameraStream`/`CameraSession` redact it in debug output).
     The device keeps it in memory only.
- **Which display:** the speaking display only. The frame goes down that display's own turn
  socket.
- **Display:**
  - `CameraView` (`anamanti-display/lib/src/ui/camera_view.dart`) uses
    `VideoViewType.platformView`, volume 0 and `mixWithOthers`. It has a ✕ (key
    `camera-close`), a spinner until the player is initialized, and a "Couldn't reach the
    camera" error state.
  - It shares the full-screen slot with recipe / weather / place: opening one closes the
    others.
  - Auto-closes after 60 s.
  - Display context: `{kind:"camera", camera:{name}}`.
- **Player patches:** `anamanti-display/third_party/video_player_android` (2.12.2 +
  `ANAMANTI_PATCHES.md`):
  1. RTP over TCP.
  2. An RTSP-only `DefaultLoadControl` (min 1 s / max 3 s; playback starts after 0.5 s, or
     1 s after a rebuffer).

  `pubspec.yaml` points `video_player_android` at it with `dependency_overrides`.
- **Config:** boot-time only. `camera{enabled, nvr_host, rtsp_port (554), username,
  cameras:[{name, channel}]}` in `anamanti.json`, plus `REOLINK_PASSWORD` from the
  environment. The camera is off unless the host, username, password and at least one
  camera are all set.
- **Bug fixed along the way:** opening recipe/weather used to send the place card's
  "cleared" context last, which wiped the newly opened screen's context (it's a single
  slot). The display now writes the active screen's context last.

**Deferred** (`TODO.md`, "Camera — follow-ups")
- Touch entry (idle-screen icon + `AppSettings.cameraEnabled`).
- A config-page drawer / runtime-settable camera config and password.
- The Core snapshot fallback (the `role="camera"` push channel in §2).
- The automatic pop-up (§3 Phase 2).

**To deploy**
1. Add the `camera` block to production `anamanti.json`:
   `nvr_host 192.168.1.29`, `username anamanti`, `cameras [{"name":"Front Door","channel":5}]`.
2. Restart the Core with `REOLINK_PASSWORD` in `~/.zshenv`, following the agents.md runbook.
3. Ship the APK (version bump + `release-r2.sh`). Core and Display must go out together:
   new frames.
4. Optional: on the NVR, set the Front Door sub-stream's I-frame interval to 1× to cut the
   wait for a keyframe.
