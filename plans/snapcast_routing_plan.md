# Snapcast Routing Plan: house-wide music from the Anamanti Core host

**Targets:** M4 Mac Mini (always-on hub) + Snapcast speakers on the LAN
(main speaker = a Linux box; other rooms; optional Echo Show).
**Feature:** route music PCM from Mac-hosted sources (librespot Spotify Connect +
a web-URL/stream player) to synchronized house speakers via **Snapcast**.

> **Status:** Planning / pre-implementation (2026-09-22). This is the **"Snapcast
> routing PR"** that [`MusicPlan.md`](./MusicPlan.md) (the Spotify source +
> control-plane plan, currently on branch `automate-spotify-app-playback-*`)
> repeatedly defers to. The two plans are complementary: `MusicPlan.md` owns the
> **source + control plane**; this plan owns the **transport / routing**.

Read together with [`MusicPlan.md`](./MusicPlan.md), [`Plan.MD`](./Plan.MD)
(decision table), and [`architecture.md`](./architecture.md) (design).

---

## 0. Confirmed decisions (2026-09-22)

| Question | Decision |
| --- | --- |
| **Where does music originate** | On the **Mac Anamanti Core host**, not the Echo Show — offloads app/power from the ~1 GB device. Answers earlier questions about Android system-audio capture (`MediaProjection`) as **n/a**: no on-device capture, no second device audio path. |
| **Transport** | **Snapcast** — synchronized multi-room, wire-compatible clients, Rust crates exist. ~1 s buffered latency accepted (confirmed "fine"). |
| **Sources (v1)** | Two, both **external OS processes** (never inside `anamanti_core`): (1) **librespot** (Spotify Connect, the *same* instance MusicPlan.md drives) and (2) a **web-URL/stream player** (mpv/ffmpeg). |
| **PCM never enters the Anamanti Core** | Consistent with MusicPlan.md L23–25/§7: each source writes raw PCM to a **snapfifo**; **snapserver** reads the fifo. The Anamanti Core only issues **control** (Web API / player IPC / snapserver JSON-RPC), never audio bytes. |
| **snapserver placement** | **On the Mac** (confirmed 2026-09-22) — always-on hub, and the snapfifos are local to the Mac-hosted sources. |
| **Daemon / routing code location** | **On the Anamanti Core host** (the Mac): snapserver + source processes + a thin supervisor/control shim. Snapclients are stock `snapclient`. |
| **Snapclient endpoints** | The **Linux box** (main speaker) **and the Echo Show(s)** (confirmed 2026-09-22), plus any other rooms. |
| **Echo Show snapclient form** | **Bundle the prebuilt `snapclient` native binary in the existing app, run it as a *separate OS process*** (a Kotlin foreground service `exec`s it) — **not** linked into the Rust engine, **not** a separate APK. Rationale: keeps the app's single cpal audio path (capture + TTS) exactly as-locked, isolates crashes/RAM, and Android's AudioFlinger mixes the two processes' output. Per-device **toggle** (`ANAMANTI_MUSIC_INCLUDE_DEVICE`, default **off**) because of the wake-word-over-music risk (§7). |
| **Ducking** | Lower the **Snapcast group volume** via JSON-RPC while the assistant is THINKING/SPEAKING; restore on IDLE. This is the mechanism MusicPlan.md §6/P4 leaves open. |

If a task seems to require changing one of these, stop and confirm first.

---

## 1. Topology

```
                 ┌───────────────── Mac Mini (always-on host) ─────────────────┐
                 │  Anamanti Core (anamanti_core)                              │
   "play X" ───▶ │   ├─ spotify_control tool ──Web API──▶ Spotify  (MusicPlan) │
                 │   └─ url_play tool ──IPC──▶ url-player                       │
                 │            control only — NEVER PCM                         │
                 │                                                             │
                 │  librespot ("Ambient")      ──PCM──▶ /tmp/snap-spotify ┐    │
                 │  url-player (mpv/ffmpeg)     ──PCM──▶ /tmp/snap-web     ┤    │
                 │                                                        ▼    │
                 │                                        snapserver (2 streams,│
                 │                                         sync hub, JSON-RPC   │
                 │                                         :1705)  ◀── ducking / │
                 │                                                   stream-select│
                 └───────────────────────────────┬─────────────────────────────┘
                                                  │  Snapcast (TCP, mDNS _snapcast._tcp)
                        ┌─────────────────────────┼─────────────────────────┐
                        ▼                         ▼                         ▼
                 Linux box (main)          Room speaker            Echo Show(s)
                 snapclient                snapclient              bundled snapclient
                 (systemd)                 (systemd)               (separate process,
                                                                    per-device toggle)
```

**Decoupling (matches MusicPlan.md L58–60):** the routing layer can land and be
tested — queue a track from a phone onto the "Ambient" Connect device and hear it
in sync across speakers — **before** any Anamanti Core tool exists. The tools only
add hands-free control on top.

---

## 2. Components & ownership

| Piece | Where | Form | Owner |
| --- | --- | --- | --- |
| snapserver (sync hub) | Mac | stock `snapserver` + config + launchd | **this plan** |
| Stream source: Spotify | Mac | `librespot` → `/tmp/snap-spotify` fifo (the **one shared** instance) | this plan (process) + MusicPlan (control) |
| Stream source: web URL | Mac | `mpv`/`ffmpeg` → `/tmp/snap-web` fifo, with an IPC control socket | **this plan** |
| snapclient: main speaker | Linux box | stock `snapclient` + systemd | this plan (config) |
| snapclient: rooms | each speaker | stock `snapclient` + systemd | this plan (config) |
| snapclient: Echo Show | Device | **prebuilt `snapclient` binary bundled in the existing app, run as a separate OS process** via a Kotlin foreground service; per-device toggle | **this plan** (device-side) |
| Control shim (ducking, stream select) | Mac, in `anamanti_core` | snapserver JSON-RPC client tied to pipeline state | **this plan** |
| `spotify_control` LLM tool | Mac | rig `PortableTool` | **MusicPlan.md** |
| `url_play` LLM tool | Mac | rig `PortableTool` (mpv IPC) | this plan (or a follow-up) |

---

## 3. snapserver configuration (the heart of this plan)

Two stream sources declared in `snapserver.conf`, each reading a named fifo the
source process writes:

```ini
# Spotify (librespot) — the SAME "Ambient" Connect device MusicPlan.md targets.
# Either snapserver's librespot source type (snapserver spawns librespot) OR an
# external librespot writing the fifo. Prefer the external process so MusicPlan's
# Web API device_id and this fifo point at one instance (MusicPlan §6 "one
# librespot instance, shared").
source = pipe:///tmp/snap-spotify?name=Spotify&sampleformat=44100:16:2&codec=flac

# Web URL / radio / podcast — mpv or ffmpeg decodes the URL to raw PCM.
source = pipe:///tmp/snap-web?name=Web&sampleformat=48000:16:2&codec=flac
```

- snapserver **resamples per stream** to a common client format; clients play in
  lockstep. librespot's native 44.1 kHz and the web player's 48 kHz coexist.
- Only one stream is "active" (audible) at a time — the control shim selects the
  group's stream when a source starts.
- JSON-RPC control API on `:1705` (`Group.SetStream`, `Group.SetVolume`,
  `Server.GetStatus`) drives stream-switch + ducking.

**Web-URL player process:** `mpv --no-video --ao=pcm`/`--audio-channels` piping to
`/tmp/snap-web`, launched with an **IPC socket** (`--input-ipc-server`) so the
Anamanti Core's `url_play` tool can `loadfile`/`stop`/`set volume` without
restarting it. Keeps PCM out of the Anamanti Core (mpv owns decode + fifo write).

---

## 4. Control plane (in `anamanti_core`, control only)

A small **snapserver JSON-RPC client** module (e.g. `anamanti-core/src/music/`)
that never touches PCM:

- **Ducking:** hook the existing pipeline state machine — on THINKING/SPEAKING
  lower the music group volume (`Group.SetVolume`), restore on IDLE. Reuses the
  turn lifecycle the Anamanti Core already owns; behind `ANAMANTI_MUSIC_DUCK_ON_SPEECH`.
  This is the concrete mechanism MusicPlan.md §6/P4 leaves undecided.
- **Stream select:** when a source starts (Spotify vs Web), point the group at
  that stream.
- **Group membership:** add/remove the Echo Show snapclient behind
  `ANAMANTI_MUSIC_INCLUDE_DEVICE`.
- **`url_play` tool (this plan or follow-up):** a rig `PortableTool` twin of
  `InternetSearch` (`anamanti-core/src/llm/rig.rs`) — `play`/`stop`/`set_volume`
  for radio/stream URLs via the mpv IPC socket. `spotify_control` (MusicPlan.md)
  stays the Spotify tool; both are advertised only when their master toggle is on.

---

## 5. Discovery & config

- **Discovery:** snapclients locate snapserver via Snapcast's own mDNS
  (`_snapcast._tcp`) or an explicit host arg — no hardcoded IP in the app,
  consistent with the project's mDNS-only discovery decision.
- **Config keys** (env-driven, per `config.rs` convention; coordinated with
  MusicPlan.md's `ANAMANTI_SPOTIFY_*`):

```bash
ANAMANTI_MUSIC=on|off                 # master toggle for the routing/control layer (now the JSON key `music.enabled`; default ON)
ANAMANTI_MUSIC_SNAPSERVER=127.0.0.1:1705   # JSON-RPC control endpoint
ANAMANTI_MUSIC_DUCK_ON_SPEECH=on      # duck group volume while the assistant speaks
ANAMANTI_MUSIC_INCLUDE_DEVICE=off     # add the Echo Show snapclient to the group
ANAMANTI_MUSIC_WEB_IPC=/tmp/mpv-web.sock   # mpv IPC socket for url_play
# Spotify source/control keys are owned by MusicPlan.md (ANAMANTI_SPOTIFY_*).
```

---

## 6. Phases (smallest coherent first)

1. **P1 — snapserver + one source on the Mac.** Stand up `snapserver` with the
   Spotify pipe source + a `snapclient` on the Linux main speaker; verify sync by
   queuing to the "Ambient" Connect device from a phone. **Unblocks MusicPlan.md P1.**
2. **P2 — multi-room.** Add room snapclients (+ systemd units); verify lockstep
   playback across all speakers. launchd unit for snapserver on the Mac (coexists
   with the Anamanti Core launchd item in `TODO.md §4`).
3. **P3 — web-URL source.** Add the mpv/ffmpeg → `/tmp/snap-web` source + the
   second snapserver stream; verify a radio URL plays house-wide.
4. **P4 — control shim.** `anamanti-core/src/music/` JSON-RPC client: ducking on
   THINKING/SPEAKING + stream-select. Host tests against a mock JSON-RPC server.
5. **P5 — `url_play` tool.** rig `PortableTool` over the mpv IPC socket.
6. **P6 — Echo Show snapclient.** Cross-compile / obtain a `snapclient` binary for
   the device (armv7 / Android 11), bundle it in the existing app, and start it from
   a Kotlin **foreground service** as a **separate OS process** (not in the Rust
   engine). Per-device toggle `ANAMANTI_MUSIC_INCLUDE_DEVICE` (default **off**) +
   settings switch. Include the wake-word-over-music mitigation: keep the raised
   wake-word threshold in force whenever the device snapclient is in the active
   group (extend the existing SPEAKING-threshold mechanism), and measure
   self-triggering on hardware before defaulting any device on.

---

## 7. Open questions / dependencies

- **Wake-word self-trigger over continuous music (device output).** *Resolved
  approach, needs hardware measurement.* With the Echo Show as a music output and
  **no AEC** (a locked deferral), the mic hears loud continuous music → far more
  self-triggering than short TTS. Mitigation: hold the raised wake-word threshold
  whenever the device snapclient is in the active group; default each device **off**
  and only enable Echo Shows in rooms without a better speaker. Measure before
  enabling. *(This makes the device a second audio output; it is kept a **separate
  OS process** precisely so the app's single locked cpal path is untouched.)*
- **snapserver placement — resolved:** on the **Mac** (2026-09-22).
- **snapclient binary for the device — resolved (available).** An `armeabi-v7a`
  (Android 11) snapclient exists: bundled as `libsnapclient.so` in **Snapdroid**
  (`snapcast/snapdroid`, also on F-Droid as `de.badaix.snapcast`), and buildable
  from source via snapcast's `client/build_android.sh` (targets
  `arm-linux-androideabi`). So P6 is unblocked when we get to it.
- **One shared librespot instance** (MusicPlan.md §6): this plan and
  `spotify_control` must target the same "Ambient" device — coordinate naming/launch
  ownership so we don't stand up two.
- **Ducking lever:** Snapcast **group volume** (this plan's default) vs. Spotify
  Web API volume (MusicPlan). Group volume also covers the web-URL source, so it's
  the general choice.
- **Codec on the wire:** FLAC (default, low CPU, ~1 s latency) vs. Opus/PCM —
  default FLAC; revisit only if latency matters.

---

## 8. Implementation status (2026-09-22, autonomous session)

Executed end-to-end as far as is verifiable without live snapserver/librespot/mpv
hardware and the Linux box:

- **Config-page Music tab + process supervisor: implemented, tested, and
  smoke-verified live.** The Anamanti Core's loopback config UI gained a **Music**
  tab (`webconfig.rs` → `/music`) backed by a `MusicSupervisor`
  (`anamanti-core/src/music/supervisor.rs`) that **starts/stops snapserver,
  librespot, and mpv** as managed child processes (`kill_on_drop`, per-process log
  files) and shows **live snapserver status** (groups/streams/clients/volumes via
  the JSON-RPC client) plus a **play-a-URL** box (mpv IPC). Endpoints:
  `GET /music/status.json`, `POST /music/{proc,play,stopweb}`. Gated behind
  `ANAMANTI_MUSIC` (the tab reports "disabled" otherwise). Launch commands +
  fifo/log/bin paths are env-tunable (`ANAMANTI_MUSIC_{BIN,RUN,LOG}_DIR`,
  `ANAMANTI_MUSIC_CONF`, `ANAMANTI_SPOTIFY_DEVICE_NAME`). Verified with a live
  binary: the page renders, and starting snapserver from the UI actually launched
  it and the control client reported `reachable: true`. This is the
  point-and-click alternative to the P1–P3 runbook / launchd agents.

- **P4 — ducking: implemented & tested.** New `anamanti-core/src/music/` module:
  a `SnapcastClient` (JSON-RPC over TCP :1705, ndjson), a `MusicDucker`
  (per-client attenuate-and-restore, idempotent), and an `MpvControl` (mpv JSON
  IPC). Wired at the `server.rs` `on_event` seam (`Speaking`→duck, `Finished`→
  restore) via `tokio::spawn`, gated behind `music.enabled` (now default **on**;
  set `music.enabled=false` in `anamanti.json` to keep it dormant). Config keys in `config.rs`
  (`ANAMANTI_MUSIC*`). **166 lib unit tests pass** (incl. 6 new music tests against
  a fake snapserver + fake mpv socket); `cargo clippy -D warnings` and `cargo fmt`
  clean on the changed files.
- **P1–P3 — infra: delivered as artifacts + runbook** under
  `anamanti-core/deploy/snapcast/` (snapserver.conf, `setup-mac.sh`, launchd agents
  for snapserver/librespot/mpv, a Linux `snapclient.service`, and README.md). Not
  started/loaded on the live Mac (no Spotify creds, physical speakers, or
  supervision available overnight); the audio-format flags for the mpv web source
  are flagged to validate at bring-up.
- **P5 — `url_play` rig tool: primitive built, registration deferred.** The
  `MpvControl` building block is done + tested; the final rig `PortableTool`
  registration is intentionally **not** added to `llm/rig.rs` tonight to avoid a
  merge conflict with MusicPlan.md's `spotify_control` (same `Tools` plumbing) —
  it should co-land with that branch.
- **P6 — Echo Show snapclient: deferred (as directed).** armv7 dependency
  confirmed available (§7).

## 9. Non-goals (v1)

- Routing music PCM through the Wyoming/Piper pipeline or the device's cpal/oboe
  path (matches MusicPlan.md §7 — DRM'd Spotify can't, and web audio shouldn't).
- On-device Android system-audio capture (`MediaProjection`) — superseded by the
  Mac-sourced design.
- Playlist/library management beyond play/pause/skip/volume + URL playback.
- A second music service beyond Spotify + generic web URLs (design leaves room).
