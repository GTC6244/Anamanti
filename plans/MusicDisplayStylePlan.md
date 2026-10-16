# Music display style — compact bar or full screen

Status: **implemented (2026-10-15)**. Branches off [`MusicPlan.md`](./MusicPlan.md) (P6, the
now-playing UI) and [`RadioPlan.md`](./RadioPlan.md); UI catalog in
[`DisplayUI.md`](./DisplayUI.md) §4b.

## What

While audio plays (Spotify **or** internet radio — they render through the same widgets),
the display shows one of two music widgets:

| Style | What shows |
| --- | --- |
| `compact` (default) | The `MusicControlOverlay` transport bar riding the ambient screen (the original behaviour). |
| `fullscreen` | The `NowPlayingView` full-screen player, automatically, for as long as music plays. |

The preference can be changed three ways, and all three edit **one value**:

1. **Config page** — Music & Devices → Displays → "Music player on the display"
   (Small bar / Full screen). `GET /music/display.json`, `POST /music/display {style}`.
2. **On the display** — the bar's **expand** button (`music-overlay-fullscreen`) switches to
   full screen; the full view's **minimize** button (`music-close`, now `close_fullscreen`
   icon) switches back to compact. The up-next view's close does the same.
3. **Voice** — the existing `music_screen` tool gains `fullscreen` / `compact` actions
   ("make the music full screen", "use the small music player"). "Hide the music" while the
   full-screen style is set also drops to compact (otherwise it would just come back).

## Decisions

| Topic | Decision |
| --- | --- |
| **Owner** | The **Core** (settings overlay `music_display_style`, persisted in `anamanti_settings.json`, no `anamanti.json` seed — pure runtime/GUI state like the radio catalog). Household-wide: every display follows it. A device-local setting was rejected because the config page could not edit it. |
| **Wire** | **No new frames, no FRB change.** The Core stamps `display_style` inside every `anamanti-music` `now_playing` snapshot (`NowPlayingService::to_value`); the display button sends `anamanti-music-control` actions `display_fullscreen` / `display_compact` (intercepted in `control::music_control_response` before transport, so it works with Spotify unlinked); voice reuses the per-turn `anamanti-music` `screen` frame with `screen:"fullscreen"`/`"compact"` (→ `WakeWordEventKind.musicScreen`). |
| **Propagation** | Config page / display button → `NowPlayingService::set_display_style` persists **and** re-pushes the cached snapshot at once to every display. Voice → `DeviceAction::MusicDisplayStyle` is applied Core-side (persist) **and** relayed as the screen frame to the speaking display; the other displays pick it up on the next periodic push (≤ `music.nowplaying_refresh_secs`), since each tick re-reads the stored style. |
| **Device state** | `AssistantState.musicDisplayStyle` mirrors the pushed value; `effectiveMusicScreen` resolves what is on screen: a Core mode (recipe/weather/place) always wins → an explicitly opened view (tap/voice) → full screen when the style says so. A Core mode closing therefore brings the full screen back with no extra code. |
| **Away mode** | The style-derived full screen shows only while someone is there (`userPresent` or a turn on screen); an unattended display still falls to the dim away clock instead of holding the backlight up all night. An explicitly opened view keeps its old "counts as awake" behaviour. |
| **Races** | A local change (button/voice) is applied optimistically; a push carrying a *different* style within 8 s is treated as stale (stamped before the Core applied it) and ignored until a push agrees. |
| **Robustness** | Unknown persisted values deserialize to `compact` (never fail the settings file); an older Core sends no `display_style` and the display keeps its current style. |
| **Music context** | The `DisplayContext::Music` push is now derived centrally in `AssistantController._emit` from `effectiveMusicScreen` (deduped), so style-driven, presence-driven and mode-driven screen changes all keep the LLM's view of the screen correct. |

## Files

- Core: `music/display_style.rs` (`MusicDisplayStyle`), `music/nowplaying_service.rs`
  (stamp + `set_display_style`), `settings.rs` (field + get/set), `control.rs` (display
  toggle), `llm/mod.rs` (`DeviceAction::MusicDisplayStyle`), `orchestrator/speech_out.rs`
  (persist + frame), `llm/rig/tools/display.rs` (tool actions), `webconfig/music.rs` +
  `routes.rs` + `devices.html` (config page).
- Display: `engine/music_data.dart` (`MusicDisplayStyle`, `displayStyle`),
  `engine/assistant_controller.dart` (state, `effectiveMusicScreen`, `expandMusic`,
  `setMusicDisplayStyle`, `closeMusicScreen`, context sync), `ui/ambient_screen.dart`,
  `ui/music_control_overlay.dart` (expand), `ui/now_playing_view.dart` /
  `ui/next_up_view.dart` (minimize).

Core and Display should be redeployed together to get all three paths; either side alone
degrades gracefully (old display ignores `display_style`; old Core sends none and rejects
the `display_*` control action in-band).
