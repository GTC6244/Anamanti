# DisplayUI.md — Anamanti Display UI catalog

The canonical, category-by-category inventory of **everything that can appear on
the Anamanti Display screen** (the Flutter app on the Echo Show 8). Use it to see
at a glance what UI already exists, where each element lives in the code, how it
is triggered, and how the layers stack.

> **Keep this doc updated.** When you add, remove, or restructure any on-screen UI
> — a new overlay, widget, full-screen view, banner, background source, status
> state, settings section, etc. — update the matching section here in the **same
> change**. This file is referenced from [`agents.md`](../agents.md),
> [`architecture.md`](./architecture.md), and the feature plans
> ([`RecipePlan.md`](./RecipePlan.md), [`WeatherPlan.md`](./WeatherPlan.md),
> [`PlacesPlan.md`](./PlacesPlan.md)) so it stays the single source of truth for
> the Display's UI surface.

All paths are relative to `anamanti-display/` unless noted. Boundary reminder
(see `agents.md`): **Flutter owns presentation only** — every reactive element
below is driven by an immutable `AssistantState` folded from Rust FRB stream
events; nothing here touches audio buffers or sockets directly.

---

## Categories at a glance

| Category | Elements |
| --- | --- |
| **App shell** | `AmbientDisplayApp` / `AmbientHome`, `AmbientScreen` (compositor) |
| **Backgrounds / idle** | `SlideshowView`, `SlideshowController`, `PhotoSource` backends (local / Ambient / Drive) |
| **Overlays (always-on)** | idle `_AmbientClock` + weather-beside-clock chip, `StatusIndicator`, settings gear |
| **Overlays (timers)** | `TimersOverlay` — big (idle) + compact (in-turn) |
| **Overlays (away/off)** | away-mode blackout + large centered `_AmbientClock`, backlight dim |
| **Banners** | `NotificationBanner` (proactive push from Core) |
| **Conversation UI** | `ConversationView` (user + assistant bubbles) |
| **Full-screen views** (pushed by Core) | `RecipeView`, `WeatherView`, `SevenDayView`, `PlaceView` |
| **Settings (route)** | `SettingsScreen` (Assistant / Device Config / Audio Diagnostics / Speech Processing / Speech Detection / Background), `AudioDiagnosticsView` |
| **Settings sub-screens** (built, currently **unwired**) | `MemoryScreen`, `PeopleScreen` |
| **Shared visual helpers** | `weatherIcon` / `weatherIconColor` |

---

## App shell

**`AmbientDisplayApp` / `AmbientHome` / `_AmbientHomeState`** — `lib/main.dart`
- Kiosk root. Boots in `immersiveSticky` full-screen (hides Android status/nav
  bars). Owns the long-lived state: `AppSettings`, `AssistantController`,
  `SlideshowController`, `NotificationController`, `WeatherChannelController`,
  `ScreenBrightnessController`, and the `OrchestratorClient`. A translucent
  `Listener` reports every touch as user activity (resets the dim countdown /
  brightens the screen).
- Before the engine config resolves it shows a bare black `Scaffold` +
  `SlideshowView` (screen is never blank); once the assistant controller exists it
  swaps to `AmbientScreen`. Opens `SettingsScreen` via a `MaterialPageRoute` — the
  **only** navigation push in the app. A 30-min timer re-mints Google tokens and
  re-lists photos.

**`AmbientScreen`** — `lib/src/ui/ambient_screen.dart`
- The compositor for the whole idle experience: one black `Scaffold` with a
  `Stack(fit: expand)` driven by an `AnimatedBuilder` on `AssistantController`.
  Composes the background, away face, full-screen modes, conversation panel, and
  all always-on overlays, cross-fading them by state.
- Derived flags: `active = state.displayActive` (turn in flight **or** reply audio
  still playing); `recipeActive` / `weatherActive` / `placeActive`;
  `modeActive = any full-screen mode`; `offMode = !userPresent && !active &&
  !modeActive` (away/off face).

### Z-order (bottom → top)

1. `SlideshowView` — idle imagery, always cycling underneath.
2. Away-mode blackout — `IgnorePointer` + `AnimatedContainer` (500 ms) fading to
   opaque black in off mode so only the big clock shows.
3. Dim scrim — `IgnorePointer` + `AnimatedContainer` (400 ms), alpha 0.55 during
   an active turn / 0.15 otherwise; pointer-transparent so idle swipes reach the
   slideshow.
4. Big `TimersOverlay` (`compact: false`) — screen-filling idle timers; fades out
   during a turn, in away mode, or in any full-screen mode.
5. `RecipeView` (recipeActive) — 250 ms fade.
6. `WeatherView` **or** `SevenDayView` (weatherActive) — chosen by
   `state.weather!.isWeek`.
7. `PlaceView` (placeActive).
8. `ConversationView` — live transcript + reply, 300 ms fade, padded 28.
9. Idle clock (`_AmbientClock`, small) — `Positioned(left: 28, bottom: 24)`;
   hidden during a turn, off mode, any full-screen mode, or when timers exist.
10. Away-mode face (`_AmbientClock`, large) — centered dimmed clock, 500 ms fade.
11. `StatusIndicator` — `Positioned(right: 20, top: 18)`; hidden in away mode.
12. Compact `TimersOverlay` (`compact: true`) — top-center chip row, only while a
    turn is active.
13. Settings gear `IconButton` — `Positioned(left: 12, top: 10)`, key
    `open-settings`; fades out during a turn and in away mode.
14. `NotificationBanner` — top layer; shows even in away mode.

---

## 1. Backgrounds / idle screens

**`SlideshowView` / `_Slide`** — `lib/src/ui/slideshow_view.dart`
- The always-on idle photo slideshow and the bottom-most layer. Cross-fades
  (1200 ms `AnimatedSwitcher`) between slides; each `_Slide` renders a remote
  `Image.network` (per-item auth `headers`) over a `LinearGradient`, or a pure
  gradient when there is no image (also the load/error fallback, 600 ms fade-in).
- Started in `initState` (`_slideshow.start()`), independent of settings /
  connectivity. Horizontal swipe → next/previous photo (resets the 10 s
  auto-advance). `HitTestBehavior.opaque`.

**`SlideshowController`** — `lib/src/slideshow/photo_source.dart`
- Cycles a `PhotoSource`'s photos on a 10 s timer, decoupled from the assistant.
  Silently falls back to `LocalPhotoSource` if the configured source throws or
  returns empty.

**`PhotoSource` backends** — `lib/src/slideshow/photo_source.dart`
- `LocalPhotoSource` — always-available offline fallback: 5 curated ambient
  gradient slides ("Dusk", "Aurora", "Deep", "Twilight", "Slate"). What a fresh /
  unlinked / offline device shows.
- `AmbientPhotoSource` — Google Photos via the Ambient API (device-code + QR);
  scope `photosambient.mediaitems`. Backend: `lib/src/slideshow/ambient_photos.dart`.
- `DrivePhotoSource` — Google Drive folders (interim source), Bearer-auth image
  listing. Backend: `lib/src/slideshow/drive_photos.dart` (`listDrivePhotos`).
- `photoSourceFromSettings(...)` picks by `PhotoSourceKind` (`local` / `ambient` /
  `drive`), falling back to local when unlinked or tokenless. OAuth client config:
  `lib/src/slideshow/google_oauth_config.dart`.

See `agents.md` (idle-screen decision) and `TODO.md §3` for the two-backend
rationale (Ambient API gated behind the Partner Program; Drive is the interim,
Core-owned path).

---

## 2. Overlays — always-on

**`_AmbientClock` / `_AmbientClockState`** — `lib/src/ui/ambient_screen.dart` (file-private)
- A 1 s-ticking 12-hour clock (`h:mm AM/PM`) in two variants:
  - **Idle** (bottom-left): 44px time, the weather chip beside it, and the hint
    "Say the wake word to begin".
  - **Away/off** (`large: true`): centered 168px dimmed (white α 0.5) time only,
    `FittedBox` to guard overflow.
- **`_weatherChip(WeatherData)`** (key `idle-weather`): the weather-beside-clock
  indicator — a condition `Icon` (30px) + current temperature with unit suffix
  (28px), sourced from `state.weatherCurrent`; hidden when null.

**`StatusIndicator`** — `lib/src/ui/status_indicator.dart`
- Top-right pill (icon + label + color) mapping `AssistantState`:
  `error` → "Reconnecting" (salmon); offline+idle → "Offline" (amber, the
  disconnected indicator); `listening` → "Listening"; `processing` →
  "Processing"; `connecting` → "Connecting"; `thinking` → "Thinking";
  `speaking` → "Speaking"; `idle` → "Ready". Hidden only in away mode.

**Settings gear** — `lib/src/ui/ambient_screen.dart`
- Top-left `IconButton` (key `open-settings`, opacity 0.7) → `onOpenSettings`;
  fades out during a turn and in away mode. The only entry to `SettingsScreen`.

**`weatherIcon(code, isDay)` / `weatherIconColor(code, isDay)`** — `lib/src/ui/weather_icons.dart`
- Shared helper (not a widget). Maps WMO codes → bundled Material icons + tints
  (sun/cloud/fog/drizzle/rain/snow/thunderstorm, day vs night). Drives the idle
  clock chip, the big weather panel, hourly cells, and the 7-day columns.

---

## 3. Overlays — timers

**`TimersOverlay` / `_TimersOverlayState`** — `lib/src/ui/timers_overlay.dart`
- On-device countdown timers; repaints twice a second to interpolate remaining
  time and ring fill (no per-second Rust events needed). Two presentations:
  - **Big (`compact: false`)** — `_FullTimers`: idle, screen-filling. 1 fills the
    screen; 2–3 side-by-side; 4+ in a `GridView`. Each `_TimerCard`: name, a
    draining circular ring (`_RingPainter`, sweeps clockwise from 12 o'clock,
    fill = remaining/total), and a centered `mm:ss` / `h:mm:ss` readout — or
    "Time's up" + "Tap to dismiss" when ringing.
  - **Compact (`compact: true`)** — `_CompactTimers`: a horizontal scrolling row of
    `_TimerChip`s over a live conversation (icon + name + `m:ss`).
- Fed by `AssistantState.timers` (Rust `timerStarted` / `timerFinished` /
  `timerCancelled`). Only **ringing** timers are tappable → `onDismiss(id)` →
  `AssistantController.dismissTimer`; running timers are not dismissible.
  `resolveTimerNames` labels unnamed timers "Timer" / "Timer N".

---

## 4. Overlays — away / off mode

- When the camera proximity sensor reports nobody present and nothing else is
  active (`offMode`), the blackout layer fades in and only the large dimmed
  centered `_AmbientClock` remains; `StatusIndicator`, idle clock, and gear fade
  out (the `NotificationBanner` still shows).
- `ScreenBrightnessController` (`lib/src/engine/screen_brightness.dart`) actuates the
  backlight via a platform `MethodChannel` (`anamanti_display/brightness`), driven by
  `AssistantState.screenAwake` — the exact inverse of `offMode`. It dims to
  `brightnessAway` (0.25) only in away mode and returns to full `brightnessNear` (1.0)
  on **any** wake: a camera approach, a touch/voice turn, or a full-screen mode. Wiring
  it to `screenAwake` (not `userPresent` alone) keeps the backlight and the blackout in
  lockstep, so the screen can never "wake" visually while the backlight stays dim. The
  "Dim screen after" setting controls the camera's release window (how long with no
  motion before away mode); it is not a separate dimmer.

---

## 5. Banners

**`NotificationBanner`** — `lib/src/ui/notification_banner.dart`
- Top-center dismissible card over the slideshow for a pushed `NotifyEvent`.
  Left color-bar + icon keyed to `priority`: `alert` → red / `warning_amber`,
  `reminder` → amber / `schedule`, `info` → blue / `notifications_none`. Title
  (bold) + body + close (`x`). `maxWidth 560`, dark translucent bg, drop shadow.
- Pushed from **Anamanti Core** over the persistent Rust "notify" channel
  (Approach A; `anamanti-hello` → `anamanti-notify`). Managed by
  `NotificationController` (`lib/src/engine/notification_controller.dart`):
  subscribes to `startNotifyChannel(config)`, auto-dismisses after 20 s or on tap;
  a `ChangeNotifier` independent of the voice-turn lifecycle. Shows even in away
  mode. Design: `architecture.md §4`; follow-ups: `TODO.md §6a`.

---

## 6. Conversation UI

**`ConversationView` / `_Bubble`** — `lib/src/ui/conversation_view.dart`
- Live turn panel over the slideshow, capped at 720px reading width. Two bubbles:
  the user transcript (right, dark blue `0xFF2A3350`) when `state.transcript` is
  non-empty; the assistant reply (left, light `0xFFEAF2FF`) when `state.reply` is
  non-empty, with a streaming caret `▌` while `phase == thinking`. Bubbles animate
  size (`AnimatedSize` 120 ms) as tokens stream. Phase text lives **only** in
  `StatusIndicator`, never here.
- Faded in by `AmbientScreen` while `state.displayActive`.

**`AssistantController` / `AssistantState` / `TurnPhase` / `TimerModel`** —
`lib/src/engine/assistant_controller.dart`
- The state engine driving all reactive UI. Folds the Rust
  `Stream<WakeWordEvent>` into an immutable `AssistantState`. Owns turn phases
  (`idle/listening/processing/connecting/thinking/speaking/error`), online/offline
  resilience (backoff reconnect + 3 s offline poll), the local end-of-speech cue,
  timers, camera `userPresent` presence, follow-up turns, and the three
  full-screen modes with auto-close timers (weather 60 s, place 5 min). Pushes
  recipe/weather/place screen context back to Core so voice can drive the screen.
  Loading any full-screen mode unloads the previous one so they never stack.

Full state-machine flow: `agents.md` cheat-sheet + `architecture.md §4`.

---

## 7. Full-screen views (pushed from Anamanti Core)

These are **not** navigation routes — they are `AnimatedOpacity` layers inside
`AmbientScreen`'s Stack, shown when the matching `AssistantState` field is
non-null (set from Wyoming events pushed by Core) and dismissed by voice, their
close (`x`) control, or an auto-close timer. Each replaces any other full-screen
mode; a live turn's `ConversationView` always draws above them.

**`RecipeView` / `_RecipeViewState`** — `lib/src/ui/recipe_view.dart`
- Dark cooking screen (`bg 0xFF14120E`, accent amber `0xFFE8A33D`). Header:
  title + facts (servings · total time) + close (key `recipe-close`). Body is an
  `IndexedStack` of 3 tabs with a bottom `_tabBar` (keys `recipe-tab-0/1/2`):
  **Overview** (`_OverviewTab` — hero image, summary, fact chips, source URL),
  **Ingredients** (`_ListTab`, key `recipe-ingredients`), **Steps** (`_ListTab`,
  key `recipe-steps`, numbered).
- Triggered by `WakeWordEventKind.showRecipe` (Core `recipe_lookup`) →
  `AssistantState.recipe`; persists across turns while cooking. Tabs switch by
  touch **or** voice (`recipeNavigate`); voice scroll (`recipeScroll`) animates
  the active pane; scroll position + tab reported back to Core as screen context.
  See [`RecipePlan.md`](./RecipePlan.md).

**`WeatherView` / `_HourCell`** — `lib/src/ui/weather_view.dart`
- Blue gradient (`0xFF0B1220`→`0xFF13233B`). Header = location (+ forecast day for
  a future-day request, key `weather-close`). Big "today" panel: 150px condition
  icon (key `weather-today-icon`) beside a 108px temperature, description, `H/L`
  (+ `Feels` for right-now). Bottom: a 10-hour row of `_HourCell`s (time, icon,
  temp, precip%). Triggered by `showWeather` (non-week) → `AssistantState.weather`;
  auto-closes after 60 s. See [`WeatherPlan.md`](./WeatherPlan.md).

**`SevenDayView` / `_DayColumn`** — `lib/src/ui/seven_day_view.dart`
- Same blue gradient; header "7-Day Forecast · <location>" (close key
  `weather-week-close`). Seven `_DayColumn`s: weekday ("Today" for i=0,
  highlighted), 44px icon, high/low, precip% with drop. Selected instead of
  `WeatherView` when `WeatherData.isWeek` (`layout == "week"`).

**`PlaceView`** — `lib/src/ui/place_view.dart`
- Blue gradient, accent `0xFF7FB2FF`. Header = place name + close (key
  `place-close`). Body: 320×320 hero photo (place-icon fallback) beside a
  scrollable details column — category + open-now chip, rating (★ + review count)
  + price level, address / phone / website lines, weekly hours. Triggered by
  `showPlace` (Core `places_lookup`) → `AssistantState.place`; auto-closes after
  5 min. See [`PlacesPlan.md`](./PlacesPlan.md).

**Data models** (tolerant `tryParse(json)` mirrors of Core structs; a malformed
payload is ignored, not crashed): `RecipeData` (`lib/src/engine/recipe_data.dart`),
`WeatherData` / `CurrentConditions` / `WeatherHour` / `WeatherDay`
(`lib/src/engine/weather_data.dart`), `PlaceData` (`lib/src/engine/place_data.dart`).

---

## 8. Settings (navigation route)

**`SettingsScreen` / `_SettingsScreenState`** — `lib/src/ui/settings_screen.dart`
- The only `Navigator.push` in the app (tap the gear on `AmbientScreen`). A
  two-level master/detail: `_menu()` lists 6 category tiles; selecting one opens
  `_categoryPage`. `enum _SettingsCategory`:
  - **Assistant** — orchestrator (which Mac) + LLM backend, Anthropic auth
    (API key vs subscription/OAuth), model, voice. Orchestrator-managed; shows
    "Contacting…" / "Assistant offline" guard tiles when the Mac is unreachable.
  - **Device Config** — Wake word (word, threshold slider, smoothing window,
    capture gain, "Fire on peak", "AudioRecord capture (far-field)", noise
    suppression / AGC / echo cancellation switches) + Display ("Dim screen after"
    preset slider, 30 s…1 h).
  - **Audio Diagnostics** — a full custom page (`AudioDiagnosticsView`, not a tile
    list) for visually tuning the mic. See its own section below.
  - **Speech Processing** — playback buffer, "Instant processing cue", endpoint
    silence ms, endpoint RMS threshold.
  - **Speech Detection** — VAD engine (Energy vs Silero), Silero threshold,
    end-silence ms, voice RMS threshold (applied on the Mac; see
    [`VadSileroPlan.md`](./VadSileroPlan.md)).
  - **Background** — photo source; Google Photos (Ambient) link via QR; Google
    Drive link via the orchestrator config page + Sync; folder-id field.
- On Save → `onApplied(next)` in `main.dart` restarts the engine (wake/threshold
  changes) and/or refreshes the slideshow (photo-source changes); assistant / VAD
  settings apply on the Mac.

**`AudioDiagnosticsView`** — `lib/src/ui/audio_diagnostics_view.dart`
- The **Audio Diagnostics** settings category — a live, visual mic-tuning surface.
  Rendered full-page by `SettingsScreen._audioDiagnosticsPage()` (passed the live
  `AssistantController` via the new `SettingsScreen.assistant` field, wired from
  `main.dart`'s `_openSettings`). Driven entirely by the always-on wake-word
  engine's event stream folded into `AssistantState` — no second audio path.
- Shows: a **Microphone monitor** toggle (pauses/resumes the visualization only —
  the wake-word mic is always on); a **mic input-level (RMS) meter** on a dBFS
  scale (so quiet far-field audio is visible); a **wake-word score meter** with the
  firing **threshold** drawn as a bright line and the smoothed score as a faint
  tick, flashing a **DETECTED** chip when the wake word fires; a **numeric
  readouts** grid (RMS, dBFS, score, smoothed, threshold, gain, device / rate /
  channels); a **recent-detections** history (time + score); and two **live-tuning**
  sliders (**Capture gain**, **Sensitivity (idle)**).
- The tuning sliders apply to the **running engine instantly** via the new FRB
  `updateDiagnosticsTuning(gainDb, threshold)` (no restart), and mirror into the
  parent's editable `AppSettings` via `onChanged` so the shared **Save** button
  persists them across restarts.
- Data path: the Rust `run_loop` enriches its periodic `WakeWordEventKind.level`
  event with the live `score` / `avgScore` / `threshold` / `gainDb` (see
  `rust/src/engine/mod.rs`, `WakeWordEvent::level_diag`); the controller stores them
  on `AssistantState` (`wakeScore`, `wakeAvgScore`, `wakeThreshold`, `captureGainDb`,
  `captureDevice` / `captureSampleRate` / `captureChannels`, `detectionSeq`,
  `lastDetectionScore`).

---

## 9. Settings sub-screens — built but currently UNWIRED

These widgets exist and are tested but are **not reachable** from the running app
in this version (referenced only from `test/`). Listed so they are not
re-implemented by mistake; wire them into `SettingsScreen` navigation when needed.

**`MemoryScreen`** — `lib/src/ui/memory_screen.dart`
- `Scaffold` + `AppBar("Memory")`. Lists Core memory entries (`MemoryView`: facts
  vs preferences, explicit vs inferred) as `ListTile`s with per-entry delete;
  app-bar refresh + "Forget all" (confirm dialog). Constructed with an
  `OrchestratorClient`.

**`PeopleScreen`** — `lib/src/ui/people_screen.dart`
- `Scaffold` + `AppBar("People")`. Lists voice-identified speakers (`SpeakerView`;
  named people vs anonymous "Speaker N", voice-clip counts). Per-row menu:
  Name/rename, Merge with…, Forget. Constructed with an `OrchestratorClient`.
  See [`speaker_id_plan.md`](./speaker_id_plan.md).

---

## Related docs

- [`agents.md`](../agents.md) — build guidance, boundaries, state-machine cheat-sheet.
- [`architecture.md`](./architecture.md) — the design (§4 state machine + wire format).
- [`Plan.MD`](./Plan.MD) — phases, decisions, open questions.
- Feature plans: [`RecipePlan.md`](./RecipePlan.md), [`WeatherPlan.md`](./WeatherPlan.md),
  [`PlacesPlan.md`](./PlacesPlan.md), [`MusicPlan.md`](./MusicPlan.md).
</content>
</invoke>
