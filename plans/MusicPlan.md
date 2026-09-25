# Music Playback Plan: Spotify via the Anamanti Core

**Targets:** M4 Mac Mini (control + audio source) + Snapcast speakers on the LAN
**Feature:** hands-free Spotify — *"play some Radiohead"* → music on the house speakers.

> **Status:** Voice control **+ config-page consent UI implemented** (2026-09-22).
> The `spotify_control` rig tool + Spotify Web API client
> (`anamanti-core/src/music/spotify.rs`), settings-backed credentials, and a
> config-page "Connect Spotify" consent flow (`spotify_consent.rs`, Music tab) all
> ship on top of PR 38's Snapcast transport + `librespot` supervisor. Remaining:
> just a Premium account + a one-time click-through consent. No Spotify audio flows
> through the Wyoming TTS pipeline or the device — the tool is control-only.

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
| **Control plane** | **Anamanti Core (Mac).** A `spotify_control` rig `PortableTool` in `anamanti-core/src/llm/rig.rs` calls the **Spotify Web API** (search, play, pause, skip, queue, volume) targeting the librespot `device_id`. |
| **librespot placement** | **Mac host, as a sibling process** to the Anamanti Core — **not** compiled into the `anamanti_core` binary. The Anamanti Core only issues Web API calls; it never touches the audio. |
| **Multi-device sync** | Owned by **snapserver** (the Snapcast PR), recommended to run **on the Mac** so it is the always-on hub. Every speaker is a snapclient. |
| **Echo Show as output** | **Default: NO** — the Echo Show stays voice-control + display only (respects the ~1 GB RAM budget and the "Flutter owns presentation only" boundary). **Optional, off by default:** run a snapclient on the device and add it to the group (`ANAMANTI_SPOTIFY_INCLUDE_DEVICE`). |
| **Ducking** | Lower music volume while the assistant speaks so replies stay audible. Built as its own toggle (`ANAMANTI_SPOTIFY_DUCK_ON_SPEECH`), likely on; more important if the device-as-snapclient option is enabled. |
| **OAuth** | One-time **Authorization-Code** consent → stored refresh token, scopes `user-modify-playback-state` + `user-read-playback-state`. Same shape as the existing Google OAuth flow; secrets live on the Mac. |

If a task seems to require changing one of these, stop and confirm first.

---

## 1. Topology

```
                 ┌───────────────── Mac Mini (always-on host) ─────────────────┐
  "play X" ──▶ Anamanti Core (LLM + spotify_control tool) ──Web API──▶ Spotify
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
| Intent + Web API calls | Anamanti Core (Mac) | New rig `PortableTool` in `llm/rig.rs` (`definition()` + guidance text, twin of `InternetSearch`) |
| OAuth / refresh token / secrets | Anamanti Core (Mac) | `ANAMANTI_SPOTIFY_*` env/config, mirroring the Google consent flow |
| librespot (PCM producer) | Mac, **beside** Anamanti Core | Separate process — Snapcast's `librespot`/`spotify` stream source, or launchd |
| snapserver (multi-room sync) | Mac (recommended) | **Snapcast PR** (external to this plan) |
| Echo Show | Device | snapclient + optional now-playing UI via a state stream — **opt-in** |

---

## 3. `spotify_control` tool (control plane) — **implemented**

A single tool with an `action` enum keeps the LLM surface small and the guidance
tight (same pattern as `internet_search`). Lives in `anamanti-core/src/llm/rig.rs`
(`SpotifyControl`, a real `PortableTool`) over the Web API client in
`anamanti-core/src/music/spotify.rs` (the `SpotifyController` trait — injected, so
it is unit-tested with a fake, no network).

- **Args:** `action` ∈ `{play, pause, resume, next, previous, queue, volume}`
  (aliases accepted: `skip`→next, `back`→previous, `stop`→pause…), optional
  `query`, optional `kind` ∈ `{track, artist, album, playlist}` (default
  `track`), optional `volume_percent` (0–100, clamped).
- **Resolution:** `play`/`queue` hit the Web API **search** endpoint (limit 1),
  then issue playback — a `track` plays as `uris`, an artist/album/playlist as a
  `context_uri`. `play` with no `query` resumes.
- **Target device:** resolved by name from `ANAMANTI_SPOTIFY_DEVICE_NAME` (default
  `"Ambient"`) via `GET /v1/me/player/devices`; every control call passes that
  `device_id`. The access token is refreshed from the refresh token and cached
  until ~30 s before expiry.
- **Errors surface as speech:** device not present ("is it powered on?"), nothing
  found, or a Web API error become the tool result, so the model apologizes aloud
  rather than failing silently. The error type implements `std::error::Error`
  (rig requirement), mirroring `SearchError`.
- **Guidance text:** a `tool_guidance` clause (like the calendar/search ones)
  nudges the model to call `spotify_control` for any play/pause/skip/queue/volume
  request instead of answering in prose.

---

## 4. Configuration keys

**Spotify control plane** (this feature; read by `music::spotify::from_env`):

```bash
ANAMANTI_SPOTIFY_CLIENT_ID=…         # Spotify developer app (Premium account)
ANAMANTI_SPOTIFY_CLIENT_SECRET=…
ANAMANTI_SPOTIFY_REFRESH_TOKEN=…     # from the one-time consent in §8
ANAMANTI_SPOTIFY_DEVICE_NAME=Ambient # librespot Connect device to target (shared with librespot)
```

The `spotify_control` tool is advertised **only when all three of ID/secret/
refresh-token are set** (exactly like `calendar_lookup` needs `ANAMANTI_CALENDARS`).
Secrets never leave the Mac. Requires **Spotify Premium** (the Web API player
endpoints are Premium-only).

**Music transport + ducking** (PR 38 — `snapcast_routing_plan.md`, `config.rs`):

```bash
ANAMANTI_MUSIC=on                    # master switch for the whole Snapcast/music feature
ANAMANTI_MUSIC_DUCK_ON_SPEECH=on     # lower the music group while the assistant speaks
ANAMANTI_MUSIC_DUCK_PERCENT=30       # duck-to level
ANAMANTI_MUSIC_STREAM=Spotify        # which snapserver stream/group to duck
ANAMANTI_MUSIC_INCLUDE_DEVICE=off    # add the Echo Show snapclient to the playback group
```

Ducking is already wired to the turn lifecycle in PR 38 and works regardless of
who started playback — no extra work here.

---

## 5. Phases / status

1. **P1 — librespot + Snapcast bring-up.** ✅ **Done (PR 38).** librespot Connect
   device "Ambient" → snapfifo → snapserver, supervised by the Anamanti Core.
2. **P2 — OAuth consent + token.** ✅ **Done (config-page UI + manual runbook).**
   The **Music tab** of the config page has a "Connect Spotify" flow
   (`spotify_consent.rs`, loopback Authorization-Code + PKCE) that stores the
   refresh token in settings and activates the tool live — no restart, no `curl`.
   The env/manual path (§8) still works for headless setup.
3. **P3 — `spotify_control` tool.** ✅ **Done (this change).** `PortableTool` +
   `definition()` + guidance in `llm/rig.rs` over `music/spotify.rs`; all actions
   (`play`/`pause`/`resume`/`next`/`previous`/`queue`/`volume`); unit + end-to-end
   rig tests with a mocked controller (no network).
4. **P4 — Ducking.** ✅ **Done (PR 38).** Turn lifecycle ducks the music group; no
   Spotify-specific work needed (the group volume covers the librespot stream).
5. **P5 (optional) — Echo Show as snapclient.** ⏳ PR 38 gates it behind
   `ANAMANTI_MUSIC_INCLUDE_DEVICE` (default off). No control-plane change.
6. **P6 (nice-to-have) — Now-playing UI.** Not started. Push current track/artist
   to the device via a state stream and render on the idle/active screen.

---

## 6. Open questions / dependencies

- **One librespot instance, shared.** `spotify_control` targets the device named
  by `ANAMANTI_SPOTIFY_DEVICE_NAME`, which **must equal** the `--name` PR 38's
  supervisor launches librespot with (default `"Ambient"` on both — keep them in
  sync if either changes).
- **Token refresh lifetime / launchd:** the client refreshes the access token
  from the stored refresh token and caches it; confirm this coexists with the
  "run under launchd" open item in `TODO.md §4`. The refresh token itself does not
  expire unless revoked or the app's scopes change.
- **Search quality:** `kind` defaults to `track`; the model chooses `artist`/
  `playlist` for vibes via guidance. Watch whether real turns pick the right kind;
  if not, tighten the tool description.

---

## 7. Non-goals (v1)

- Streaming Spotify audio through Piper/Wyoming or the device's cpal/oboe path.
- Spotify **Free** support or any UI-automation/screen-scraping workaround.
- Playlisting / library management beyond play/pause/skip/queue/volume.
- Multi-service music (Apple Music, YouTube Music) — design leaves room for a
  second provider tool later, but out of scope now.

---

## 8. One-time consent runbook (get the refresh token)

The `spotify_control` tool needs a long-lived **refresh token** for the Premium
account. This is a one-time step on the Mac; the token then lives in the
Anamanti Core's environment. (A config-page consent UI is the §9 follow-up; until
then, do this by hand.)

1. **Create a Spotify app** at <https://developer.spotify.com/dashboard> → note
   the **Client ID** and **Client Secret**. Add a Redirect URI of
   `http://127.0.0.1:8888/callback` (Settings → Redirect URIs).

2. **Authorize** — open this URL in a browser signed into the Premium account
   (scopes cover playback control + reading the device list):

   ```
   https://accounts.spotify.com/authorize?client_id=CLIENT_ID&response_type=code&redirect_uri=http://127.0.0.1:8888/callback&scope=user-modify-playback-state%20user-read-playback-state
   ```

   Approve; the browser redirects to `http://127.0.0.1:8888/callback?code=CODE`
   (the page won't load — just copy `CODE` from the address bar).

3. **Exchange the code for a refresh token:**

   ```bash
   curl -s -X POST https://accounts.spotify.com/api/token \
     -u "CLIENT_ID:CLIENT_SECRET" \
     -d grant_type=authorization_code \
     -d code=CODE \
     -d redirect_uri=http://127.0.0.1:8888/callback | python3 -m json.tool
   ```

   Copy the `refresh_token` from the JSON.

4. **Configure the Anamanti Core** (e.g. in `~/.zshenv` for the local-production
   install, alongside `ANAMANTI_INSTANCE_ID`):

   ```bash
   export ANAMANTI_SPOTIFY_CLIENT_ID="…"
   export ANAMANTI_SPOTIFY_CLIENT_SECRET="…"
   export ANAMANTI_SPOTIFY_REFRESH_TOKEN="…"     # from step 3
   # ANAMANTI_SPOTIFY_DEVICE_NAME defaults to "Ambient" (matches librespot)
   ```

   Restart the Anamanti Core. On boot it logs `spotify_control: enabled, targeting
   Connect device "Ambient"`, and the tool is advertised to the rig LLM engine.

5. **Verify by voice:** with `ANAMANTI_MUSIC=on` and librespot running, say
   *"play some Radiohead"* → music on the speakers + a spoken confirmation.

---

## 9. Config-page consent UI — **implemented**

The Google-Drive loopback consent pattern is mirrored for Spotify:

- **`anamanti-core/src/spotify_consent.rs`** — loopback Authorization-Code + PKCE
  flow. **Unlike Drive**, Spotify requires an *exact pre-registered* redirect, so
  it binds a **fixed** port (default 8888) and the operator registers
  `http://127.0.0.1:8888/callback` in their Spotify app (the page tells them so).
- **`SpotifyConfig` in `settings.rs`** — client id/secret/refresh-token/device,
  persisted (0600) and seeded from `ANAMANTI_SPOTIFY_*` at boot. Unlike Drive, a
  change **rebuilds the LLM** (`apply_spotify`) so `spotify_control` is advertised/
  withdrawn live — the tool activates the instant consent completes.
- **Config page → Music tab** — a "Spotify (voice control)" card: client id/secret/
  device inputs, **Save**, and **Connect Spotify** (`POST /spotify/save`,
  `/spotify/link`, `GET /spotify/status.json`). Secrets are never echoed back.

So the end-to-end setup is now: open the config page → Music tab → paste client
id/secret → Connect Spotify → approve in the browser → say "play some Radiohead."
