# Weather Plan: Forecast on the Display

**Targets:** M4 Mac Mini (fetch) + Echo Show 8 (the weather screen + ambient indicator)
**Feature:** ask about the weather → a full-screen forecast appears (a big conditions
panel + a **10-hour hourly row** along the bottom); and **always**, on the idle/home
screen, a small weather icon + the current temperature sit beside the clock.

> The `WeatherView` / `SevenDayView` full-screen modes and the weather-beside-clock
> chip are catalogued alongside the rest of the Display's UI in
> [`DisplayUI.md`](./DisplayUI.md) — update it there when this screen changes.

> **Status:** **Implemented (2026-09-25), pending on-device QA.** Built and tested: the
> `weather_lookup` / `close_weather` rig tools + the Open-Meteo provider (Anamanti Core,
> new unit tests), the `anamanti-weather` Wyoming frame (round-trip tested in both
> crates), the ambient push (`WeatherService` + periodic broadcast over the persistent
> `role=weather` channel), the FRB `ShowWeather`/`WeatherCurrent`/`DismissWeather`
> events + the `WeatherPush` sidecar stream, and the `WeatherView` + the `_AmbientClock`
> indicator (new Dart tests). Suites green: Anamanti Core 273, device-rust 90, Flutter
> 93; clippy + `dart analyze` clean; FRB codegen clean.
>
> **Update (2026-09-27):** added a **Visual Crossing** provider behind the same
> `WeatherProvider` trait and made it the **default** (`weather.provider="visualcrossing"`,
> keyed by the `VISUALCROSSING_API_KEY` secret; keyless Open-Meteo is the fallback). The
> provider label + key are runtime-settable on the config-page **Tools tab** — a change
> rebuilds `weather_lookup` and retargets the ambient push live (no restart), mirroring the
> Mapbox pattern. Anamanti Core lib suite now 293 (green); clippy `-D warnings` clean.
>
> **Update (2026-09-28):** the full-screen card now shows a **10-hour hourly row**
> instead of the 7-day daily row (`WeatherReport.hourly` + `when_label` replace `daily`;
> both providers fetch hourly data). For a right-now request the row starts at the current
> hour (truncated); `weather_lookup` gained an optional **`when`** argument (`today`/
> `tomorrow`/a weekday/`YYYY-MM-DD`, resolved by `weather::resolve_when`) so a future-day
> request shows that day's hourly row starting at **08:00** with the big panel showing that
> day's summary. Hourly is no longer a non-goal (§3). No FRB/frame change — the report
> still travels as JSON. Suites green: Anamanti Core lib 324, Flutter 101; clippy
> `-D warnings` + `dart analyze` clean.
>
> **Update (2026-09-29):** revived the 2026-09-28 hourly work (it had been left on an
> unmerged branch) and added a **separate 7-day forecast widget** alongside the hourly
> view. `WeatherReport` regains **`daily[7]`** (always populated — both providers already
> parsed it internally) plus a **`layout`** hint (`""`/`"hourly"` default, or `"week"`).
> `weather_lookup` gains an optional **`layout`** argument the LLM sets to `"week"` for
> weekly/multi-day phrasings ("7-day forecast", "what's the week look like"); the report
> then renders as the new **`SevenDayView`** — the screen divided into 7 vertical columns,
> each with the weekday, condition icon, daily **high/low**, and precip chance. Still one
> `anamanti-weather` frame + one `AssistantState.weather` slot (the display picks the view
> by `WeatherData.isWeek`), so **no FRB/frame change** — the report still travels as JSON.
> The ambient push stays hourly (`layout` empty). Suites green: Anamanti Core lib 345,
> Flutter 116; clippy `-D warnings` + `flutter analyze` clean.
>
> Reads together with
> [`RecipePlan.md`](./RecipePlan.md) (the near-identical device-push template) and
> [`architecture.md`](./architecture.md) §4 (frame catalog).

Weather is **three coordinated pieces** split along the locked Rust/Flutter boundary:

1. a `weather_lookup` **rig tool** on the Anamanti Core (fetch a structured forecast),
2. a new **`anamanti-weather` Wyoming frame** carrying the report to the device, over
   two transports — the per-turn socket (voice-triggered full screen) and a persistent
   `role=weather` channel (the always-on ambient push), and
3. a new **display "weather mode"** — the full-screen `WeatherView` plus the small
   icon + temperature beside the idle clock.

---

## 0. Confirmed decisions (2026-09-25)

| Question | Decision |
| --- | --- |
| **Data source** | Behind a `WeatherProvider` trait (injected, offline fixture tests), mirroring `DirectionsProvider`, with two backends selectable by `weather.provider`: **Visual Crossing** (default, `visualcrossing`) — its Timeline API resolves the place *and* returns current + 7-day forecast in one keyed request (needs the `VISUALCROSSING_API_KEY` secret; its text icons are mapped to WMO codes in `vc_icon_to_wmo`); and **Open-Meteo** (`openmeteo`) — keyless, structured current + 7-day forecast with WMO codes + its free geocoding API. Open-Meteo is also the automatic fallback when the Visual Crossing key is absent. **Runtime-settable:** the provider label + Visual Crossing key live on the config page **Tools tab** (persisted to `anamanti_settings.json`, `0600`), and a change rebuilds the `weather_lookup` tool + retargets the ambient push live — mirroring the Mapbox token pattern. |
| **Trigger (full screen)** | **Voice.** Any weather question → the LLM calls `weather_lookup` (defaulting the place to the household `home_location`); the tool pushes the forecast and returns a short spoken confirmation. Dismiss by voice (`close_weather`), an on-screen close control, or **automatically after 60 s** (unlike recipe mode, which has no timeout — you glance at weather, you cook along with a recipe). The auto-close timer resets when a fresh forecast is shown and leaves the ambient clock chip untouched (`AssistantController._weatherAutoClose`, default 60 s). |
| **Layout (hourly vs 7-day)** | **Voice, one tool.** `weather_lookup` takes an optional **`layout`** arg (`"hourly"` default, `"week"`); the LLM sets `"week"` for weekly/multi-day phrasings ("7-day forecast", "what's the week look like"). The report carries `layout` + `daily[7]`, and the display renders the hourly `WeatherView` or the separate `SevenDayView` (7 vertical columns of daily high/low + icon + precip) by `WeatherData.isWeek`. Same frame/slot/auto-close as the hourly view. |
| **Ambient indicator** | **Always-on periodic push.** A Core `WeatherService` background task fetches current conditions every `weather.refresh_interval_secs` (default 30 min) and broadcasts an `anamanti-weather` `current` frame down the persistent channel, so the icon + temperature stay fresh with no voice turn. |
| **Imagery** | **Bundled icon set.** Flutter's built-in Material icons, chosen by WMO code + day/night (`weather_icons.dart`) — offline, scalable to the big today panel and the small chip, and free of raster assets (respects the ~1 GB memory budget). |
| **Transport** | A new `anamanti-weather` frame (mirrored byte-for-byte in both `protocol.rs`, `anamanti-recipe` as the template) with `action` = `show` / `current` / `dismiss`. The ambient channel reuses the notify channel's `anamanti-hello`, discriminated by `role=weather`. |
| **Units** | From the household `weather_units` (imperial → °F, else °C), seeded at boot like `directions_imperial`. |
| **Display context (2026-09-25)** | While the full-screen forecast is up, the device stamps a `{kind:"weather", weather:{location, units, temp, description}}` block on the turn's `audio-start` (the **general display-context channel** merged from main; `set_weather_context` FRB → `engine::set_display_context` → `send_audio_start`). The Core parses it (`DisplayContext::Weather` / `WeatherScreen`) and appends a prompt line (`orchestrator::weather_screen_line`), so a voice turn taken *while looking at the weather* knows the forecast is up and for where — "close it" → `close_weather`, "what about tomorrow" → `weather_lookup` for the same place. The ambient clock chip is **not** a screen and never sets context. See `architecture.md` §4 "Display context on `audio-start`". |

If a task seems to require changing one of these, stop and confirm first.

---

## 1. Topology

```
  "what's the weather?"      ┌──────────── Mac Mini (Anamanti Core) ────────────┐
        ──STT──▶ LLM intent ──▶ weather_lookup tool
                                      │  Open-Meteo geocode + forecast
                                      ▼
                          WeatherReport { location, units, when_label, current, hourly[10], daily[7], layout }
                                      │
                    tool returns a spoken confirmation to the model, and
                    emits DeviceAction::ShowWeather(report)
                                      │  drain_device_actions
                                      ▼  anamanti-weather {action:"show"} (voice socket)
  Echo Show:  client.rs ─▶ TurnUpdate::Weather ─▶ WakeWordEvent::ShowWeather
                     ─▶ AssistantState.weather ─▶ WeatherView (full screen)

  ── every 30 min ──▶ WeatherService.broadcast(report)
                          anamanti-weather {action:"current"} (role=weather channel)
  Echo Show:  weather.rs ─▶ WeatherPush ─▶ AssistantController.applyWeatherPush
                     ─▶ AssistantState.weatherCurrent ─▶ _AmbientClock (icon + temp)
```

---

## 2. Components & ownership

| Piece | Where | Form |
| --- | --- | --- |
| Fetch + parse | Core | `anamanti-core/src/weather/mod.rs`: `WeatherProvider` trait + `VisualCrossingWeather` (Timeline API, one keyed request; `vc_icon_to_wmo` icon→WMO mapping) and `OpenMeteoWeather` (geocode + forecast, WMO codes); `from_config(enabled, provider, api_key)` selects the backend (Visual Crossing default, Open-Meteo fallback). `WeatherReport`/`CurrentConditions`/`DailyForecast` (integer temps so it's `Eq` inside `DeviceAction`). |
| The tool | Core | `WeatherLookup` / `close_weather` in `llm/rig.rs` (over `WeatherConfig{provider, live home_location, imperial}`), registered in `Tools`/`dispatch`/`tools_from_config`; guidance clause. |
| Config | Core | `weather { enabled, provider, refresh_interval_secs }` in `config.rs` + `anamanti.example.json`; the `VISUALCROSSING_API_KEY` secret from env. Reuses `home_location`/`weather_units`. Provider + key are runtime-settable via `SharedSettings::apply_weather_tool` (persisted, best-effort rebuild) and surfaced on the config-page Tools tab (`/tools/weather/status.json` + `/tools/weather/save`, `tools.html`). |
| Ambient push | Core | `weather::WeatherService` (registry, twin of `NotificationService`) + `service::spawn_periodic`, wired in `main.rs`; served by the `role=weather` arm in `server.rs`. |
| Frame | both crates | `anamanti-weather` + `weather_show/current/dismiss` constructors + `weather_command()` decoder + `hello_weather`/`hello_role`, byte-identical, round-trip tested. |
| Device decode | Device Rust | `TurnUpdate::Weather` (`client.rs`) → `WakeWordEvent::{show,current,dismiss}_weather` (`net.rs`); the persistent channel `wyoming/weather.rs` → `WeatherPush` FRB stream (`start_weather_channel`). |
| UI | Flutter | `weather_data.dart` (parse; `WeatherHour` + `WeatherDay` + `whenLabel`/`layout`/`isWeek`) + `weather_icons.dart` (WMO→icon) + `WeatherView` (conditions panel + 10-hour hourly row, day label + no "Feels" for a future day) + `SevenDayView` (7 vertical day columns of high/low + icon + precip, selected by `isWeek`) + `_AmbientClock` chip; `AssistantState.weather`/`weatherActive`/`weatherCurrent`; `WeatherChannelController` wired in `main.dart`. |

---

## 3. Non-goals (v1)

- ~~Hourly forecast~~ (shipped 2026-09-28; see the update note above). Radar/
  precipitation maps, severe-weather alerts remain out of scope.
- Per-device targeting of the ambient push (broadcasts to all, like notify).
- A config-page "push weather now" test button (the periodic task + a voice ask cover it).
- Spoken multi-day read-out (the model speaks a one-line confirmation only).

## 4. On-device validation (pending)

Release APK per `agents.md`; set `home_location` + `weather_units`:
- "what's the weather" → spoken confirmation + full-screen forecast with the 10-hour
  hourly row (starting at the current hour); "what's the weather Saturday" → the hourly
  row starts at 8am with that day's summary in the panel;
  "close the weather" and the on-screen close both dismiss.
- The small icon + temperature appear beside the idle clock and refresh on the interval
  (and reappear after a Mac restart — the channel reconnects with backoff).

## 5. Docs updated on landing

`agents.md` (feature-plan list + the `anamanti-weather` frame), `architecture.md` §4
(frame catalog + weather device mode), `Plan.MD` (decision-table rows), `TODO.md §7`
(the dedicated-weather-tool / render-on-display items).
