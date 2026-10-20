# Music display style — compact bar or full screen, per display

Status: **implemented (2026-10-15); per-display (2026-10-20)**. Branches off
[`MusicPlan.md`](./MusicPlan.md) (P6, the now-playing UI) and [`RadioPlan.md`](./RadioPlan.md);
UI catalog in [`DisplayUI.md`](./DisplayUI.md) §4b.

## What

While audio plays (Spotify **or** internet radio — they render through the same widgets),
each display shows one of two music widgets:

| Style | What shows |
| --- | --- |
| `compact` | The `MusicControlOverlay` transport bar riding the ambient screen. |
| `fullscreen` | The `NowPlayingView` full-screen player, automatically, for as long as music plays. |

**Each display has its own style.** By default the music is **full screen on the display the
play request came from** (the display you asked) and the **compact bar on every other
display**. Music started outside any display (the Spotify app, the config page) is compact
everywhere.

A display's style is resolved in this order (first match wins):

1. **Its choice for the current music** — the display's own **expand** button
   (`music-overlay-fullscreen`) / **minimize** button (`music-close`, `close_fullscreen` icon;
   the up-next view's close does the same), or voice spoken *to that display* via the
   `music_screen` tool's `fullscreen` / `compact` actions ("make the music full screen", "use
   the small music player"). "Hide the music" while that display is full screen also drops it
   to compact (otherwise it would just come back). Only that display changes. The choice lasts
   until the music stops or a new play request starts new music.
2. **Its pin** — config page → Music & Devices → Displays → "Music player on each display":
   **Automatic** (default) / **Small bar** / **Full screen**, per display. Persisted.
3. **Automatic** — full screen if the current music was asked for from this display, else
   compact.

## Decisions

| Topic | Decision |
| --- | --- |
| **Owner** | The **Core**. Pins live in the settings overlay `music_display_pins` (`device_id` → style, persisted in `anamanti_settings.json`, no `anamanti.json` seed — pure runtime/GUI state like the radio catalog). Session state (who started the music + each display's choice) lives in `MusicDisplayRouter`, one per Core, held by `LlmFactory` (like the radio player) so the voice tools, the control handler and the now-playing push share it. A device-local style was rejected: the config page could not edit it, and only the Core knows which display a play request came from. |
| **Session** | A *session* is one stretch of music. A voice `play` (Spotify `play`, radio `play`) starts a new one: the speaking display becomes the **initiator** and every display's choice is forgotten. A `resume` starts one only when nothing is loaded (resuming paused music keeps the session). Music going from loaded to nothing (a `dismiss` push) ends it. A dismiss while nothing was loaded changes nothing, so the idle push between "play X" and Spotify actually starting can't wipe the initiator. |
| **Identity on the wire** | The display stamps its stable `device_id` (the one it already sends in `anamanti-hello`) as a top-level `data.device_id` on every turn's `audio-start` and on every `anamanti-music-control` (`WyomingEvent::with_device_id`). The Core reads it into `DeviceContext.device_id` → `LlmTurn.device_id` → `ToolCtx.device_id`, and via `music_control_device_id()` for the button. Music channels now also record the identity from their hello. **No new frames, no FRB change** (the device's Rust records the id when a push channel starts). |
| **Per-display push** | `NowPlayingService` registers each music channel with its `ConnectedDevice` and pushes with `ChannelRegistry::broadcast_with`, stamping each display's own `display_style` into its `now_playing` snapshot. |
| **Ordering** | The Spotify/radio tools record the play request **before** issuing the play, so the first push of the new music already carries the right styles. Every router change signals `MusicDisplayRouter::changed`, which wakes the push loop to re-send the cached snapshot at once (no Spotify call; the next tick stays on schedule). |
| **Voice style switch** | `music_screen` `fullscreen`/`compact` records the speaking display's choice in the router and emits `DeviceAction::MusicDisplayStyle`, relayed as the per-turn `anamanti-music` `screen` frame (`screen:"fullscreen"`/`"compact"` → `WakeWordEventKind.musicScreen`) so that display switches at once. Nothing is persisted. |
| **Display button** | `anamanti-music-control` `display_fullscreen` / `display_compact` (intercepted in `control::music_control_response` before transport, so it works with Spotify unlinked) records the sending display's choice and re-pushes. A request with no `device_id` (an older display) is answered `ok:false` in-band and changes nothing for others. |
| **Config page** | `GET /music/display.json` → `{ ok, displays: [{ device_id, name, connected, pin, style, chosen, initiator }] }` (connected displays plus offline pinned ones, so a pin can be cleared). `POST /music/display { device_id, style: auto\|compact\|fullscreen }` sets/clears the pin, forgets that display's session choice so the pin shows at once, and re-pushes. |
| **Device state** | Unchanged: `AssistantState.musicDisplayStyle` mirrors this display's pushed value; `effectiveMusicScreen` resolves what is on screen: a Core mode (recipe/weather/place) always wins → an explicitly opened view (tap/voice) → full screen when the style says so. A Core mode closing therefore brings the full screen back with no extra code. |
| **Away mode** | The style-derived full screen shows only while someone is there (`userPresent` or a turn on screen); an unattended display still falls to the dim away clock instead of holding the backlight up all night. An explicitly opened view keeps its old "counts as awake" behaviour. |
| **Races** | A local change (button/voice) is applied optimistically; a push carrying a *different* style within 8 s is treated as stale (stamped before the Core applied it) and ignored until a push agrees. |
| **Robustness** | Pins read leniently (garbled entries dropped, never fail the settings file). The old household-wide `music_display_style` key in an existing settings file is ignored — the automatic per-display rule replaces it. An older Core sends no `display_style` and the display keeps its current style. |
| **Music context** | The `DisplayContext::Music` push is derived centrally in `AssistantController._emit` from `effectiveMusicScreen` (deduped), so style-driven, presence-driven and mode-driven screen changes all keep the LLM's view of the screen correct. |

## Files

- Protocol: `anamanti-protocol/src/lib.rs` (`with_device_id`, `music_control_device_id`).
- Core: `music/display_style.rs` (`MusicDisplayStyle`, `MusicDisplayPins`,
  `MusicDisplayRouter`, `PlayRequest`), `music/nowplaying_service.rs` (per-display stamp,
  `repush`, `display_styles`, restyle wake), `notify.rs` (`broadcast_with`), `settings.rs`
  (`music_display_pins`, `set_music_display_pin`, `music_display_router`, `LlmFactory.music_display`),
  `wyoming/protocol.rs` (`DeviceContext.device_id`), `orchestrator/mod.rs` + `llm/mod.rs` +
  `llm/rig/mod.rs` (device id → `ToolCtx`), `llm/rig/tools/services.rs` (play → initiator),
  `llm/rig/tools/display.rs` (`music_screen` choice), `control.rs` (display toggle),
  `server.rs` (music channel identity), `webconfig/music.rs` + `routes.rs` + `devices.html`.
- Display Rust: `engine/mod.rs` (`set_device_id` / `device_id`), `api/engine.rs` (recorded when
  a push channel starts), `engine/net.rs` + `wyoming/client.rs` (`audio-start` stamp),
  `wyoming/control.rs` + `api/settings.rs` (music-control stamp).
- Display Dart (behaviour unchanged): `engine/music_data.dart`,
  `engine/assistant_controller.dart`, `ui/ambient_screen.dart`, `ui/music_control_overlay.dart`,
  `ui/now_playing_view.dart` / `ui/next_up_view.dart`.

Core and Display should be redeployed together. Either side alone degrades gracefully: an old
display sends no `device_id`, so it is never the initiator (compact unless pinned) and its
expand/minimize button switches only its own screen locally; an old Core ignores the extra
`device_id` fields and keeps the household-wide style.
