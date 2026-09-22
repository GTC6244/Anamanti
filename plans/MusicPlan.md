# Music Playback Plan: Spotify via the Orchestrator

**Targets:** M4 Mac Mini (control + audio source) + Snapcast speakers on the LAN
**Feature:** hands-free Spotify — *"play some Radiohead"* → music on the house speakers.

> **Status:** Planning / pre-implementation (2026-09-22). Scope confirmed as a
> **control-plane LLM tool on the orchestrator** driving a **librespot** Spotify
> Connect endpoint, whose raw PCM is routed by **Snapcast** (separate PR). No
> Spotify audio flows through the Wyoming TTS pipeline or the device.

This plan reads together with [`Plan.MD`](./Plan.MD) (the tool-calling / rig
engine decisions), [`architecture.md`](./architecture.md) (the design), and the
in-flight **Snapcast routing PR** (not yet in this tree).

---

## 0. Confirmed Decisions (2026-09-22)

| Question | Decision |
| --- | --- |
| **What is a "skill" here** | Not an Alexa skill and not a gstack dev skill — it is an **LLM tool** in the rig engine, alongside `internet_search` / `calendar_lookup`. |
| **Account tier** | **Spotify Premium** (confirmed available). Required: both librespot streaming *and* the Web API playback-control endpoints are Premium-only. Free is not supported and no hacky fallback will be built. |
| **Audio path** | **Not through our pipeline.** Spotify audio is DRM'd — it cannot be synthesized/streamed as PCM by Piper. Music is produced by **librespot** (Spotify Connect client) → raw PCM → Snapcast. |
| **Control plane** | **Orchestrator (Mac).** A `spotify_control` rig `PortableTool` in `orchestrator/src/llm/rig.rs` calls the **Spotify Web API** (search, play, pause, skip, queue, volume) targeting the librespot `device_id`. |
| **librespot placement** | **Mac host, as a sibling process** to the orchestrator — **not** compiled into the `ambient_orchestrator` binary. The orchestrator only issues Web API calls; it never touches the audio. |
| **Multi-device sync** | Owned by **snapserver** (the Snapcast PR), recommended to run **on the Mac** so it is the always-on hub. Every speaker is a snapclient. |
| **Echo Show as output** | **Default: NO** — the Echo Show stays voice-control + display only (respects the ~1 GB RAM budget and the "Flutter owns presentation only" boundary). **Optional, off by default:** run a snapclient on the device and add it to the group (`AMBIENT_SPOTIFY_INCLUDE_DEVICE`). |
| **Ducking** | Lower music volume while the assistant speaks so replies stay audible. Built as its own toggle (`AMBIENT_SPOTIFY_DUCK_ON_SPEECH`), likely on; more important if the device-as-snapclient option is enabled. |
| **OAuth** | One-time **Authorization-Code** consent → stored refresh token, scopes `user-modify-playback-state` + `user-read-playback-state`. Same shape as the existing Google OAuth flow; secrets live on the Mac. |

If a task seems to require changing one of these, stop and confirm first.

---

## 1. Topology

```
                 ┌───────────────── Mac Mini (always-on host) ─────────────────┐
  "play X" ──▶ orchestrator (LLM + spotify_control tool) ──Web API──▶ Spotify
                        │ targets device_id                              │
                        ▼                                                ▼
                 librespot (Connect device "Ambient") ──raw PCM──▶ snapfifo
                                                                         │
                                                              snapserver (sync hub)
                                                               │        │        │
                                                            Speaker  Speaker  [Echo Show]
                                                          (snapclient) ...     (optional)
                        └─────────────────────────────────────────────────────┘

  Echo Show 8 (default): voice control + on-screen now-playing only — NOT a music output.
```

**End-to-end flow:** wake word → STT → LLM recognizes intent → `spotify_control`
call → Web API starts playback on the librespot device → librespot PCM → snapfifo
→ snapserver → house speakers. The LLM speaks a short confirmation via Piper
(ducking the music briefly if enabled).

**Decoupling:** the two halves are independent. librespot + snapserver can land
and be tested (queue a track from a phone onto the "Ambient" Connect device)
*before* the voice tool exists. The tool only adds hands-free control on top.

---

## 2. Components & Ownership

| Piece | Where | Form |
| --- | --- | --- |
| Intent + Web API calls | Orchestrator (Mac) | New rig `PortableTool` in `llm/rig.rs` (`definition()` + guidance text, twin of `InternetSearch`) |
| OAuth / refresh token / secrets | Orchestrator (Mac) | `AMBIENT_SPOTIFY_*` env/config, mirroring the Google consent flow |
| librespot (PCM producer) | Mac, **beside** orchestrator | Separate process — Snapcast's `librespot`/`spotify` stream source, or launchd |
| snapserver (multi-room sync) | Mac (recommended) | **Snapcast PR** (external to this plan) |
| Echo Show | Device | snapclient + optional now-playing UI via a state stream — **opt-in** |

---

## 3. `spotify_control` tool (control plane)

A single tool with an `action` enum keeps the LLM surface small and the guidance
tight (same pattern as `internet_search`):

- **Actions:** `play` (query = track/artist/album/playlist, free-text),
  `pause`, `resume`, `next`, `previous`, `queue`, `set_volume`.
- **Resolution:** `play` first hits the Web API **search** endpoint, picks the
  top match for the requested type, then issues **start/resume playback** on the
  target `device_id` (the librespot device).
- **Target device:** resolved once from `AMBIENT_SPOTIFY_DEVICE_NAME` (default
  `"Ambient"`) via `GET /me/player/devices`; cached, re-resolved on 404 (device
  asleep/renamed).
- **Errors surface as speech:** no active device, not Premium, nothing found →
  a short spoken explanation, never a silent failure.
- **Guidance text:** a `tool_guidance` clause (like the calendar/search ones)
  telling the model to use `spotify_control` for any "play / pause / skip / next
  / put on / queue" music request rather than answering in prose.

Errors implement `std::error::Error` (rig requirement), mirroring `SearchError`.

---

## 4. Configuration keys (env-driven, per `config.rs` convention)

```bash
AMBIENT_SPOTIFY=on|off              # master toggle; off → tool not advertised (default off until shipped)
AMBIENT_SPOTIFY_CLIENT_ID=…         # Spotify developer app
AMBIENT_SPOTIFY_CLIENT_SECRET=…
AMBIENT_SPOTIFY_REFRESH_TOKEN=…     # from one-time Authorization-Code consent
AMBIENT_SPOTIFY_DEVICE_NAME=Ambient # librespot Connect device to target
AMBIENT_SPOTIFY_INCLUDE_DEVICE=off  # add the Echo Show snapclient to the playback group
AMBIENT_SPOTIFY_DUCK_ON_SPEECH=on   # lower music volume while the assistant speaks
```

Secrets never leave the Mac. When `AMBIENT_SPOTIFY` is unset/off, the tool is not
advertised to the model (exactly like `calendar_lookup` when no calendars are set).

---

## 5. Phases (smallest coherent first)

1. **P1 — librespot + Snapcast bring-up (depends on Snapcast PR).** Stand up
   librespot as a Connect device "Ambient" feeding the snapfifo; verify audio on
   the house speakers by queuing from a phone. No orchestrator changes. Confirms
   the audio path end-to-end before any code.
2. **P2 — OAuth consent + token.** One-time Authorization-Code flow (small Mac
   helper, twin of `tools/google_photo_consent.py`) → refresh token stored for
   the orchestrator. Verify a raw `play` call against the device with `curl`.
3. **P3 — `spotify_control` tool.** Implement the `PortableTool` + `definition()`
   + guidance in `llm/rig.rs`; wire config in `config.rs`; `play`/`pause`/`next`
   first, then `queue`/`set_volume`. Host tests with a mocked Web API client
   (inject like `SearchProvider`).
4. **P4 — Ducking.** On THINKING/SPEAKING, lower Spotify/Snapcast volume; restore
   on return to IDLE. Behind `AMBIENT_SPOTIFY_DUCK_ON_SPEECH`.
5. **P5 (optional) — Echo Show as snapclient.** Provision snapclient on the
   device + group membership behind `AMBIENT_SPOTIFY_INCLUDE_DEVICE`; expose as a
   settings toggle. No control-plane change.
6. **P6 (nice-to-have) — Now-playing UI.** Push current track/artist to the
   device via a state stream and render on the idle/active screen.

---

## 6. Open questions / dependencies

- **Where does the Snapcast PR run snapserver?** Recommended: the Mac (always-on
  hub). If it runs snapserver *on the Echo Show*, revisit librespot placement —
  the Mac-hosted source assumes a Mac-hosted (or LAN-reachable) snapfifo.
- **One librespot instance, shared.** The Snapcast PR and this feature should use
  the **same** librespot device, not stand up two. Confirm ownership/naming.
- **Ducking mechanism:** Spotify Web API volume vs. Snapcast group volume — pick
  whichever the Snapcast setup exposes most reliably (decide in P4).
- **Token refresh lifetime / launchd:** the orchestrator refreshes the access
  token from the stored refresh token; confirm this coexists with the "run under
  launchd" open item in `TODO.md §4`.

---

## 7. Non-goals (v1)

- Streaming Spotify audio through Piper/Wyoming or the device's cpal/oboe path.
- Spotify **Free** support or any UI-automation/screen-scraping workaround.
- Playlisting / library management beyond play/pause/skip/queue/volume.
- Multi-service music (Apple Music, YouTube Music) — design leaves room for a
  second provider tool later, but out of scope now.
