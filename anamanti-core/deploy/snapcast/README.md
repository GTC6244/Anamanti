# Anamanti music routing — deployment runbook

House-wide music from the Mac Anamanti Core host to Snapcast speakers. Design:
[`plans/snapcast_routing_plan.md`](../../../plans/snapcast_routing_plan.md).
Companion source/control plan: `plans/MusicPlan.md` (on the
`automate-spotify-app-playback-*` branch).

```
Mac (hub)                                              Speakers
  librespot "Ambient" ──PCM──▶ snap-spotify ┐
  mpv (web URLs)      ──PCM──▶ snap-web      ┤─▶ snapserver ──▶ snapclients:
  Anamanti Core ──JSON-RPC :1705 (duck/select)┘        (Linux main, rooms, Echo Show*)
```

The Anamanti Core **never carries music PCM** — it only ducks/selects over
JSON-RPC and (later) drives mpv over its IPC socket. `*` Echo Show is opt-in (P6).

## 0. Easiest path: the Anamanti Core's Music tab

After `./setup-mac.sh` (below), start the Anamanti Core with `"music": {"enabled":
true}` in its `anamanti.json` and open the config page (default
`http://127.0.0.1:8730/`) → **Music** tab. From there
you can **start/stop snapserver, librespot, and mpv**, watch live snapserver status
(groups/clients/volumes), and **play a web URL** — no terminal needed. The sections
below are the manual/launchd equivalents.

## 1. Mac host bring-up (P1–P3)

```bash
./setup-mac.sh          # installs snapcast+librespot+mpv, makes FIFOs, installs snapserver.conf
```

Then start the pieces (validate manually before installing the launchd agents):

```bash
snapserver -c /opt/homebrew/etc/snapserver.conf         # the hub + control :1705
librespot --name Ambient --backend pipe \
  --device /opt/homebrew/var/run/ambient/snap-spotify --bitrate 320   # Spotify Connect "Ambient"
mpv --idle=yes --no-video --input-ipc-server=/tmp/ambient-mpv.sock \
  --ao=pcm --ao-pcm-waveheader=no \
  --ao-pcm-file=/opt/homebrew/var/run/ambient/snap-web \
  --audio-samplerate=48000 --audio-channels=stereo --audio-format=s16    # web-URL player
```

Validate:
- **Spotify:** open Spotify on a phone on the same LAN → pick the **Ambient** device
  → play. You should hear it on the connected snapclients, in sync.
- **Web:** `echo '{"command":["loadfile","https://stream-url"]}' | socat - /tmp/ambient-mpv.sock`
  (or let the Anamanti Core's future `url_play` tool do it) → audio on the speakers.
- **Format:** if snapserver logs a sampleformat mismatch on the web stream, adjust
  the mpv `--audio-*` flags until it emits raw `48000:16:2` s16le (no WAV header).

Once validated, install the always-on agents:

```bash
cp launchd/com.ambient.*.plist ~/Library/LaunchAgents/
launchctl load ~/Library/LaunchAgents/com.ambient.snapserver.plist
launchctl load ~/Library/LaunchAgents/com.ambient.librespot.plist
launchctl load ~/Library/LaunchAgents/com.ambient.mpv-web.plist
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
  "web_ipc": "/tmp/ambient-mpv.sock"   // for the (future) url_play tool
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
