# Weather Plan: Forecast on the Display

**Targets:** M4 Mac Mini (fetch) + Echo Show 8 (the weather screen + ambient indicator)
**Feature:** ask about the weather → a full-screen forecast appears (today's conditions
with big imagery + a 7-day row along the bottom); and **always**, on the idle/home
screen, a small weather icon + the current temperature sit beside the clock.

> **Status:** **Implemented (2026-09-25), pending on-device QA.** Built and tested: the
> `weather_lookup` / `close_weather` rig tools + the Open-Meteo provider (Anamanti Core,
> new unit tests), the `anamanti-weather` Wyoming frame (round-trip tested in both
> crates), the ambient push (`WeatherService` + periodic broadcast over the persistent
> `role=weather` channel), the FRB `ShowWeather`/`WeatherCurrent`/`DismissWeather`
> events + the `WeatherPush` sidecar stream, and the `WeatherView` + the `_AmbientClock`
> indicator (new Dart tests). Suites green: Anamanti Core 273, device-rust 90, Flutter
> 93; clippy + `dart analyze` clean; FRB codegen clean. Reads together with
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
| **Data source** | **Open-Meteo** — keyless, structured current + 7-day forecast with WMO weather codes, plus its free geocoding API to resolve `home_location`. Behind a `WeatherProvider` trait (injected, offline fixture tests), mirroring `DirectionsProvider`. |
| **Trigger (full screen)** | **Voice.** Any weather question → the LLM calls `weather_lookup` (defaulting the place to the household `home_location`); the tool pushes the forecast and returns a short spoken confirmation. Dismiss by voice (`close_weather`), an on-screen close control, or **automatically after 60 s** (unlike recipe mode, which has no timeout — you glance at weather, you cook along with a recipe). The auto-close timer resets when a fresh forecast is shown and leaves the ambient clock chip untouched (`AssistantController._weatherAutoClose`, default 60 s). |
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
                          WeatherReport { location, units, current, daily[7] }
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
| Fetch + parse | Core | `anamanti-core/src/weather/mod.rs`: `WeatherProvider` trait + `OpenMeteoWeather` (geocode + forecast, WMO codes), `WeatherReport`/`CurrentConditions`/`DailyForecast` (integer temps so it's `Eq` inside `DeviceAction`). |
| The tool | Core | `WeatherLookup` / `close_weather` in `llm/rig.rs` (over `WeatherConfig{provider, live home_location, imperial}`), registered in `Tools`/`dispatch`/`tools_from_config`; guidance clause. |
| Config | Core | `weather { enabled, refresh_interval_secs }` in `config.rs` + `anamanti.example.json`. Reuses `home_location`/`weather_units`. |
| Ambient push | Core | `weather::WeatherService` (registry, twin of `NotificationService`) + `service::spawn_periodic`, wired in `main.rs`; served by the `role=weather` arm in `server.rs`. |
| Frame | both crates | `anamanti-weather` + `weather_show/current/dismiss` constructors + `weather_command()` decoder + `hello_weather`/`hello_role`, byte-identical, round-trip tested. |
| Device decode | Device Rust | `TurnUpdate::Weather` (`client.rs`) → `WakeWordEvent::{show,current,dismiss}_weather` (`net.rs`); the persistent channel `wyoming/weather.rs` → `WeatherPush` FRB stream (`start_weather_channel`). |
| UI | Flutter | `weather_data.dart` (parse) + `weather_icons.dart` (WMO→icon) + `WeatherView` (today panel + 7-day row) + `_AmbientClock` chip; `AssistantState.weather`/`weatherActive`/`weatherCurrent`; `WeatherChannelController` wired in `main.dart`. |

---

## 3. Non-goals (v1)

- Hourly forecast, radar/precipitation maps, severe-weather alerts.
- Per-device targeting of the ambient push (broadcasts to all, like notify).
- A config-page "push weather now" test button (the periodic task + a voice ask cover it).
- Spoken multi-day read-out (the model speaks a one-line confirmation only).

## 4. On-device validation (pending)

Release APK per `agents.md`; set `home_location` + `weather_units`:
- "what's the weather" → spoken confirmation + full-screen forecast with the 7-day row;
  "close the weather" and the on-screen close both dismiss.
- The small icon + temperature appear beside the idle clock and refresh on the interval
  (and reappear after a Mac restart — the channel reconnects with backoff).

## 5. Docs updated on landing

`agents.md` (feature-plan list + the `anamanti-weather` frame), `architecture.md` §4
(frame catalog + weather device mode), `Plan.MD` (decision-table rows), `TODO.md §7`
(the dedicated-weather-tool / render-on-display items).
