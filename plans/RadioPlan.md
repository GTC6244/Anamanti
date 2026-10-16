# Radio Plan: internet radio stations via the Anamanti Core

**Targets:** M4 Mac Mini (control + mpv source) + the existing Snapcast speakers.
**Feature:** a curated list of internet radio stations (e.g. CJDV 107.5 Dave Rocks,
`https://corus.leanstream.co/CJDVFM-MP3`) managed in the Core web UI, played
house-wide on the **same Snapcast output as Spotify**, with a radio now-playing
screen on the display (station logo + station name + current song) and voice control
("play Dave Rocks", "next station", "stop the radio").

Reads together with [`MusicPlan.md`](./MusicPlan.md) (Spotify source + control plane +
the now-playing push) and [`snapcast_routing_plan.md`](./snapcast_routing_plan.md) (the
transport; this feature is its "P3 web-URL source" + "P5 `url_play` tool", specialised
to a curated station list).

> **Status:** implemented (2026-10-14). Needs a live speaker check on hardware.

---

## 0. Confirmed decisions (2026-10-14)

| Question | Decision |
| --- | --- |
| **Audio path** | The **existing** `mpv` web player (`--ao=pcm`, resampled to 48000:16:2) → snapserver's **`Web`** stream. PCM never enters the Core (unchanged invariant). **Amended 2026-10-15:** the `snap-web` named FIFO never played on macOS (snapserver's FIFO reader stalls; mpv then blocks writing the full pipe), so snapserver now launches mpv itself as a `process://` source reading its stdout, and the Core only drives it over IPC (`music.players_in_snapserver`). |
| **Output destination** | The same Snapcast group(s) Spotify plays on. Starting a station points **every** snapserver group at the `Web` stream (`Group.SetStream`); Spotify taking over (voice, display controls, or a phone/Connect start seen by the now-playing poll) stops the radio and points them back at `Spotify`. One audible source at a time. |
| **Spotify ↔ radio hand-off** | Radio start **pauses Spotify** (best-effort). Any Spotify play/resume/next/previous (tool or display) **stops the radio first** (a `SpotifyController` decorator). The now-playing poll, when it sees Spotify actually playing while radio is on, stops the radio (covers starts from a phone). |
| **Station ids** | Each station has a short stable **`id`** (the call sign, e.g. `CJDV`, unique, case-insensitive) plus a display **name** and optional **aliases** for voice ("Dave", "Dave Rocks", "one oh seven five"). The id is what the display, tool and web UI key on. |
| **Logos** | **Auto-lookup + override.** The web UI's "Look up" queries the free, keyless **Radio Browser** directory (`api.radio-browser.info`) by stream URL, then by name/call sign derived from the URL, and offers matches; picking one fills the name + logo URL. The logo URL field is always editable (override). The display loads the logo URL the same way it loads Spotify album art. |
| **Current song** | From the stream's **ICY metadata** (`icy-title`, read from mpv's `metadata` property over IPC on each now-playing tick), cleaned up and split on `" - "` into artist / title. Absent → just the station. |
| **Storage** | The catalog lives in the settings overlay (`anamanti_settings.json`, `radio.stations`) — pure runtime/GUI state with no `anamanti.json` seed, like the personality catalog. A save rebuilds the LLM so the tool's station list stays current. |
| **Wire format** | **No new frame.** Radio rides the existing `anamanti-music` `now_playing` push with additive fields: `source: "radio"`, `station_id`, `station_name`. `track_title` is always non-empty (song title, else the station name) so older displays still render it. Display controls reuse `anamanti-music-control`; for radio `pause` = stop, `next`/`previous` = cycle stations, `volume` = mpv volume. |
| **Voice** | One `radio_control` rig tool (`play` a station / `stop` / `next` / `previous` / `list`), advertised when music routing is on and the catalog is non-empty. |

---

## 1. Components

| Piece | Where |
| --- | --- |
| `RadioStation` / `RadioCatalog` (+ sanitize/resolve) | `anamanti-core/src/music/radio.rs` |
| `RadioPlayer` (mpv load/stop/volume/metadata, `Group.SetStream`, Spotify pause, now-playing snapshot) + `RadioController` trait | `anamanti-core/src/music/radio.rs` |
| `RoutedSpotify` decorator (stop radio before Spotify plays) | `anamanti-core/src/music/radio.rs` |
| Radio Browser lookup | `anamanti-core/src/music/radio_browser.rs` (via `crate::http`) |
| Catalog persistence | `settings.rs` `RuntimeSettings.radio` / `PersistedSettings.radio`, `apply_radio` |
| `radio_control` tool | `anamanti-core/src/llm/rig/tools/services.rs` |
| Now-playing push (radio first, else Spotify) | `music/nowplaying_service.rs` |
| Display control routing | `control.rs` `music_control_response` |
| Web UI: station list, add/edit/delete, look up, play/stop | `/devices` → **Radio stations** card; `GET /radio/stations.json`, `POST /radio/save`, `/radio/lookup`, `/radio/play`, `/radio/stop` |
| Display radio now-playing | `anamanti-display/lib/src/engine/music_data.dart` (`isRadio`, `stationName`…), `now_playing_view.dart` / `music_control_overlay.dart` (no seek/duration/queue, stop instead of pause, logo `contain`) |

## 2. Non-goals (v1)

- Station discovery/browsing on the device (stations are curated on the Core).
- Recording, rewind/time-shift, podcasts.
- Uploading logo files to the Core (URL only; auto-lookup fills it).
- Playing radio on a subset of rooms (it follows the whole house, like Spotify).
