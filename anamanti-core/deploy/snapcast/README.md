# Anamanti music routing — deployment runbook

House-wide music from the Mac Anamanti Core host to Snapcast speakers. Design:
[`plans/snapcast_routing_plan.md`](../../../plans/snapcast_routing_plan.md).
Companion source/control plan: `plans/MusicPlan.md` (on the
`automate-spotify-app-playback-*` branch).

```
Mac (hub)                                                   Speakers
  snapserver ─┬─ spawns librespot "Ambient" (stdout PCM) ┐
              └─ spawns mpv (idle, IPC socket)  (stdout) ┴─▶ snapclients:
  Anamanti Core ──JSON-RPC :1705 (duck/select/volume)          (Linux main, rooms, Echo Show*)
               └─ mpv IPC /tmp/ambient-mpv.sock (radio play/stop/volume)
```

The Anamanti Core **never carries music PCM** — it only ducks/selects over
JSON-RPC and drives mpv over its IPC socket. `*` Echo Show is opt-in (P6).

**snapserver owns the players.** Its `snapserver.conf` launches librespot
(`librespot://`) and mpv (`process://`) and reads their stdout. The older layout —
the Core or launchd starting them writing into named FIFOs (`pipe://`) — **does not
work on macOS**: snapserver's FIFO reader stalls (kqueue FIFO bugs), mpv blocks in
`write()` on the full pipe, and nothing plays. The Core's
`music.players_in_snapserver` (default `true`) keeps it from spawning duplicates; the
`com.ambient.librespot` / `com.ambient.mpv-web` launchd agents are obsolete on the Mac.

## 0. Easiest path: the Anamanti Core's Music tab

After `./setup-mac.sh` (below), start the Anamanti Core with `"music": {"enabled":
true}` in its `anamanti.json` and open the config page (default
`http://127.0.0.1:8730/`) → **Music** tab. From there
you can **start/stop snapserver** (which brings librespot + mpv up and down with
it), watch live snapserver status, set each speaker's **volume / mute**, and **play a
web URL** or a radio station — no terminal needed. The sections
below are the manual/launchd equivalents.

## 1. Mac host bring-up (P1–P3)

```bash
./setup-mac.sh          # installs snapcast+librespot+mpv, the librespot cache dir, snapserver.conf
```

Then start the hub — it launches librespot ("Ambient") and the idle mpv itself:

```bash
snapserver -c /opt/homebrew/etc/snapserver.conf         # the hub + control :1705
```

Validate:
- **Spotify:** open Spotify on a phone on the same LAN → pick the **Ambient** device
  → play. You should hear it on the connected snapclients, in sync.
- **Web:** `echo '{"command":["loadfile","https://stream-url"]}' | socat - /tmp/ambient-mpv.sock`
  (or let the Anamanti Core's future `url_play` tool do it) → audio on the speakers.
- **Format:** if snapserver logs a sampleformat mismatch on the web stream, adjust
  the mpv `--audio-*` flags in `snapserver.conf` until it emits raw `48000:16:2` s16le
  (no WAV header). mpv logs to `/opt/homebrew/var/log/ambient-mpv-web.log`.
- **Check:** `Server.GetStatus` on :1705 should show the `Web` stream `playing` a few
  seconds after a load. If it stays `idle` while mpv's IPC stops answering, something is
  writing a FIFO again — that is the macOS stall above.

Normally the Anamanti Core starts snapserver (`music.autostart`). To run it from launchd
instead, install only the snapserver agent (the librespot / mpv-web agents are the old
FIFO layout — don't load them on the Mac):

```bash
cp launchd/com.ambient.snapserver.plist ~/Library/LaunchAgents/
launchctl load ~/Library/LaunchAgents/com.ambient.snapserver.plist
```

## 2. Speakers (snapclients)

- **Linux main speaker:** see [`linux/snapclient.service`](linux/snapclient.service)
  (auto-discovers the Mac via mDNS; or pin `-h`).
- **Other rooms:** the same unit with a distinct `--hostID`.
- **Echo Show (P6, opt-in, low priority):** see below.

## 3. Anamanti Core ducking (P4 — implemented & tested)

The Anamanti Core lowers the music group's volume while the assistant speaks and
restores it when the turn ends. Enable it in `anamanti.json` (all default-off/inert):

```jsonc
"music": {
  "enabled": true,                     // master switch (off ⇒ feature dormant)
  "snapserver_addr": "127.0.0.1:1705",
  "duck_on_speech": true,              // default on
  "duck_percent": 30,                  // duck to 30% while speaking
  "stream": "Spotify",                 // or "group": "<id>", else auto
  "web_ipc": "/tmp/ambient-mpv.sock",  // the mpv snapserver launches (radio, url play)
  "players_in_snapserver": true        // default; false only with a Linux pipe:// conf
}
```

Behavior: on `TurnEvent::Speaking` each client in the target group is attenuated to
`DUCK_PERCENT` (prior volumes remembered); on `TurnEvent::Finished` they are
restored exactly. Best-effort — a missing/unreachable snapserver only logs at debug
and never affects a turn. Implementation: `anamanti-core/src/music/` (unit-tested
against a fake snapserver) + the `server.rs` on_event seam.

## 4. Echo Show snapclient (P6 — opt-in, deferred)

An `armeabi-v7a` (Android 11) snapclient **is available** — bundled as
`libsnapclient.so` in Snapdroid, and buildable via snapcast's
`client/build_android.sh`. Plan: bundle the binary in the existing app and run it
from a Kotlin foreground service as a **separate OS process** (see the plan). Note
the wake-word-over-music risk (no AEC): default each device off. Not built yet.

## Status

- P1–P3: artifacts here (validate on the Mac + Linux box).
- P4 (ducking): **implemented & tested** in `anamanti-core/src/music/`.
- P5 (`url_play` rig tool): `MpvControl` primitive built & tested; final rig-tool
  registration deferred to co-land with MusicPlan.md's `spotify_control` (same
  `llm/rig.rs` plumbing) to avoid a merge conflict.
- P6 (Echo Show): deferred; armv7 dep confirmed available.
