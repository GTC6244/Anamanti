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
| **Overlays (listening cue)** | `ListeningOverlay` — big glowing blue ring (hollow) that reacts to mic level while listening |
| **Overlays (timers)** | `FullScreenTimersView` (idle) + `CompactTimersOverlay` (in-turn / over a mode) — linked by `TimerDisplayStyle` |
| **Overlays (music)** | `MusicControlOverlay` — compact transport bar while a track plays (or `NowPlayingView` full screen, per this display's music display style) |
| **Overlays (away/off)** | away-mode blackout + large centered `_AmbientClock`, backlight dim |
| **Banners** | `NotificationBanner` (proactive push from Core), `UpdateBanner` (in-app updater, selfUpdate flavor) |
| **Conversation UI** | `ConversationView` (user + assistant bubbles) |
| **Full-screen views** (pushed by Core) | `RecipeView`, `WeatherView`, `SevenDayView`, `PlaceView`, `CameraView`, `NowPlayingView`, `NextUpView` |
| **Settings (route)** | `SettingsScreen`: a side rail of six sections — Assistant (Brain · Capabilities) / Listening (Wake word · Microphone · End of speech) / Audio Diagnostics / Display (Screen · Photos) / Household / Device (Connection · Updates · Maintenance) — plus `AudioDiagnosticsView` |
| **People / enrollment** (pushed from Settings → Household → People) | `PeopleScreen`, `SpeakerEnrollScreen` |
| **Memory** (pushed from Settings → Household → Memory) | `MemoryScreen` (tabs: Facts / Conversations / Keywords) |
| **Link a phone** (pushed from Settings → Household → Link a phone) | `PairPhoneScreen` (name + 4-char AppSaid pairing code) |
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
- **Global font scale.** `main.dart` wraps `AmbientScreen` in a single
  `MediaQuery` whose `textScaler` is `TextScaler.linear(AppSettings.fontScale)`, so
  the voice commands *"increase font"* / *"decrease font"* scale **all** ambient text
  at once (every `Text` reads the ambient `textScaler`; no per-widget sizes change).
  The scale is device-local + persisted, clamped `[0.85, 1.6]`, stepped `0.1`. The
  pushed `SettingsScreen` route is **outside** this subtree, so it stays at the
  default size. See [`FontScalePlan.md`](./FontScalePlan.md).
- Derived flags: `active = state.displayActive` (turn in flight **or** reply audio
  still playing); `recipeActive` / `weatherActive` / `placeActive` / `cameraActive`;
  `modeActive = any full-screen mode`; `offMode = !userPresent && !active &&
  !modeActive` (away/off face).

### Z-order (bottom → top)

1. `SlideshowView` — idle imagery, always cycling underneath.
2. Away-mode blackout — `IgnorePointer` + `AnimatedContainer` (500 ms) fading to
   opaque black in off mode so only the big clock shows.
3. Dim scrim — `IgnorePointer` + `AnimatedContainer` (400 ms), alpha 0.55 during
   an active turn / 0.15 otherwise; pointer-transparent so idle swipes reach the
   slideshow.
4. `FullScreenTimersView` — screen-filling idle timers, shown while
   `effectiveTimerDisplay == TimerDisplayStyle.fullscreen`; fades out during a turn,
   in away mode, or in any full-screen mode.
5. `RecipeView` (recipeActive) — 250 ms fade.
6. `WeatherView` **or** `SevenDayView` (weatherActive) — chosen by
   `state.weather!.isWeek`.
7. `PlaceView` (placeActive).
7b. `CameraView` (cameraActive) — the live camera view, 250 ms fade, keyed on the
    open sequence so each show builds a fresh player.
7a. `NowPlayingView` (`showNowPlaying`) / `NextUpView` (`showNextUp`) — the music
    full screens, 250 ms fade, driven by `state.effectiveMusicScreen` (a Core-pushed
    mode suppresses them; otherwise a tapped/voiced view, or the full-screen music
    display style while someone is present).
8. `ConversationView` — live transcript + reply, 300 ms fade, padded 28.
9. `ListeningOverlay` — big glowing blue ring (hollow), centered, 250 ms fade;
   `IgnorePointer`, shown only while `state.listening` (wake word / follow-up listen
   → end-of-speech). Reacts to `state.micLevel`. Mounted only while shown so its
   pulse controller isn't spinning during idle.
9a. **Bottom strip** — one `Positioned(left: 28, right: 20, bottom: 24)` `Row`:
    the idle clock (item 10) on the left, then an `Expanded` **bottom-widget slot**
    right-aligned in line with it (see *Bottom-edge layout rule* below). Today the
    slot holds `MusicControlOverlay` — the compact transport bar, 250 ms fade; shown while `musicOverlayVisible` (a track is playing and
    no full music screen / Core mode / turn is up and someone is present). Tappable
    (opens `NowPlayingView`); its expand button switches this display's music display
    style to full screen (other displays keep theirs).
10. Idle clock (`_AmbientClock`, small) — the left child of the bottom strip;
    hidden during a turn, off mode, any full-screen mode, or when timers exist.
11. Away-mode face (`_AmbientClock`, large) — centered dimmed clock, 500 ms fade.
12. `StatusIndicator` — `Positioned(right: 20, top: 18)`; hidden in away mode.
13. Settings gear `IconButton` — `Positioned(left: 12, top: 10)`, key
    `open-settings`; **hidden by default** — shown only for 5 s after a touch, and only
    when no full-screen widget is displayed and not in away mode (see §2).
14. `NotificationBanner` — shows even in away mode.
15. `UpdateBanner` — in-app updater prompt/progress (selfUpdate flavor).
16. `CompactTimersOverlay` — top-center chip row, the **top-most
    layer** so a running timer always has z-order precedence over every other overlay
    and banner. Shown (`effectiveTimerDisplay == TimerDisplayStyle.compact`) whenever timers run and the big
    screen-filling presentation is hidden — i.e. while a turn is active **or** any
    full-screen mode (recipe/weather/place/music) is up — so the timer stays glanceable
    instead of hiding behind those views; suppressed in away mode.

---

## 1. Backgrounds / idle screens

**`SlideshowView` / `_Slide`** — `lib/src/ui/slideshow_view.dart`
- The always-on idle photo slideshow and the bottom-most layer. Cross-fades
  (1200 ms `AnimatedSwitcher`) between slides; each `_Slide` renders a remote
  `Image.network` (per-item auth `headers`) over a `LinearGradient`, or a pure
  gradient when there is no image (also the load/error fallback, 600 ms fade-in).
- Started in `initState` (`_slideshow.start()`), independent of settings /
  connectivity. Horizontal swipe → next/previous photo (resets the
  auto-advance timer to a full interval). `HitTestBehavior.opaque`.

**`SlideshowController`** — `lib/src/slideshow/photo_source.dart`
- Cycles a `PhotoSource`'s photos on a timer, decoupled from the assistant. The
  rotation time is **per device** — `AppSettings.slideIntervalSecs` (default 10 s,
  clamped 3 s…10 min), set on Settings → Display → Photos ("Change photo every") and
  applied live via `setInterval` (restarts the timer; no engine restart).
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
  - **Idle** (bottom-left): 50px time with the weather chip beside it (no hint
    text — the "Say the wake word to begin" line was removed 2026-10-19).
  - **Away/off** (`large: true`): centered 168px dimmed (white α 0.5) time only,
    `FittedBox` to guard overflow.
- **`_weatherChip(WeatherData)`** (key `idle-weather`): the weather-beside-clock
  indicator — a condition `Icon` (34px) + current temperature with unit suffix
  (32px), sourced from `state.weatherCurrent`; hidden when null.

**`StatusIndicator`** — `lib/src/ui/status_indicator.dart`
- Top-right status. **Healthy idle is a bare 14px green dot** (key
  `status-ready-dot`, no label, no pill). Every other state is a pill (icon + label +
  color) mapping `AssistantState`:
  `error` → "Reconnecting" (salmon); offline+idle → "Offline" (amber, the
  disconnected indicator); `listening` → "Listening"; `processing` →
  "Processing"; `connecting` → "Connecting"; `thinking` → "Thinking";
  `speaking` → "Speaking"; `idle` → the green dot. Hidden only in away mode.

**Settings gear** — `lib/src/ui/ambient_screen.dart`
- Top-left `IconButton` (key `open-settings`, opacity 0.7) → `onOpenSettings`. The
  only entry to `SettingsScreen`. **Hidden unless the screen was touched AND no
  full-screen widget is displayed**: a translucent `Listener` (`_TouchReveal`, doesn't
  consume the touch) reveals it for 5 s after each pointer-down. Only full-screen
  widgets block it — the turn's conversation panel, a full-screen mode
  (recipe/weather/place/music now-playing/up-next), or the big screen-filling idle
  timers — plus away mode. Partial overlays (the compact music bar, the listening ring,
  banners) never block it. Hidden, it is disabled — the first touch only reveals it; a
  second tap opens settings.

### Bottom-edge layout rule (all current and upcoming bottom widgets)

**No bottom widget may ever cover the time/weather (the idle clock).** The bottom edge
is a single `Row` in `ambient_screen.dart`: the idle clock on the left, then an
`Expanded` bottom-widget slot whose contents are **right-aligned, vertically in line
with the time/weather**. A bottom widget only gets the width the clock leaves (its own
content must shrink/ellipsize, as `MusicControlOverlay`'s title does). When adding a
new bottom widget (e.g. a Home Assistant strip, a calendar "next up" chip), put it in
that slot — **never** as its own `Positioned(bottom: …)` layered over the clock. If
several bottom widgets can be visible at once, stack/arrange them inside the slot.
Full-screen views are exempt (they hide the clock anyway).

**`weatherIcon(code, isDay)` / `weatherIconColor(code, isDay)`** — `lib/src/ui/weather_icons.dart`
- Shared helper (not a widget). Maps WMO codes → bundled Material icons + tints
  (sun/cloud/fog/drizzle/rain/snow/thunderstorm, day vs night). Drives the idle
  clock chip, the big weather panel, hourly cells, and the 7-day columns.

---

## 3. Overlays — listening cue

**`ListeningOverlay` / `_ListeningOverlayState`** — `lib/src/ui/listening_overlay.dart`
- A large (260 px) glowing blue **ring** (hollow — no fill, so the slideshow shows
  through the centre), centered on screen, that gives an unmistakable "I heard you —
  I'm listening" cue and doubles as a **live voice visualizer**. A bright blue-white
  stroke with a soft blue glow (two stacked `BoxShadow`s — a wide halo + a tight edge
  bloom). Its **stroke width, scale, and glow react to the microphone amplitude**
  (`AssistantState.micLevel`, the input RMS streamed by `WakeWordEventKind.level`),
  **auto-ranged against a slowly-decaying peak** (so it stays visibly dynamic despite
  the small, gain/AEC/distance-dependent absolute RMS) with asymmetric attack/release
  smoothing so each syllable pumps the ring and it settles cleanly; under a baseline
  it keeps a slow "breathing" pulse (a single
  `AnimationController`, 1.4 s, `repeat(reverse: true)`) so it feels alive before you
  speak. `IgnorePointer` — touches pass through to the layers beneath.
- Shown while `AssistantState.listening` (`phase == listening || connecting`): it
  appears on `WakeWordEventKind.detected` (wake word) **and** `listeningFollowup` (a
  follow-up listen reopens the mic), stays up through the `connecting` → `streaming`
  hop and while the user speaks, and hides when end-of-speech moves the turn on
  (local cue → `processing`, or the authoritative `transcript` → `thinking`) or the
  turn ends. The local cue is only a guess (the Core's VAD is authoritative and keeps
  listening through a pause the cue mistook for end-of-speech), so if sustained
  speech-level audio (≥150 ms above the cue's level threshold) resumes while in
  `processing` and before the transcript arrives, the phase reverts to `listening`
  and the ring comes back. Excludes the TTS `speaking` phase (that's the assistant talking).
  `AmbientScreen` wraps it in an `AnimatedOpacity` (key `listening-cue`, 250 ms) and
  mounts the widget only while shown, so the pulse controller isn't spinning during
  the idle day.
- **User-configurable** in Settings → Display → Screen: a master on/off
  (`AppSettings.listeningRingEnabled` — when off the overlay never appears) plus the
  reactivity / attack / release / auto-range-decay dials (`ringReactivity`,
  `ringAttack`, `ringRelease`, `ringDecay`; attack / release / auto-range are
  Advanced-mode only). `AmbientScreen` receives these from `main.dart` and passes
  them to `ListeningOverlay`; changes apply as soon as they're made (a `setState` in
  `_onSettingsApplied`), with no engine restart or Mac round-trip.

## 4. Overlays — timers

**`FullScreenTimersView` + `CompactTimersOverlay`** — `lib/src/ui/timers_overlay.dart`
- On-device countdown timers as a **linked dual-form pair** (the `agents.md` dual-form
  widget convention): two widgets linked by `TimerDisplayStyle { compact, fullscreen }`.
  Which one shows is resolved only by `AssistantState.effectiveTimerDisplay`: `null`
  with no timers or in away mode, `compact` while a turn/reply audio, a Core-pushed mode,
  or a full music screen covers the idle screen, otherwise `AssistantState.timerDisplayStyle`.
  That field defaults to `TimerDisplayStyle.defaultStyle` = **`fullscreen`** (the big
  widget). Nothing changes it yet; it's the hook for a future setting or voice command. Both share
  `_TickingTimersState`, which repaints twice a second to interpolate remaining time and
  ring fill (no per-second Rust events needed).
  - **`FullScreenTimersView`** (`fullscreen`) — `_FullTimers`: idle, screen-filling. 1 fills the
    screen; 2–3 side-by-side; 4+ in a `GridView`. Each `_TimerCard`: name, a
    draining circular ring (`_RingPainter`, sweeps clockwise from 12 o'clock,
    fill = remaining/total), and a centered `mm:ss` / `h:mm:ss` readout — or
    "Time's up" + "Tap to dismiss" when ringing.
  - **`CompactTimersOverlay`** (`compact`) — `_CompactTimers`: a horizontal scrolling row of
    `_TimerChip`s (icon + name + `m:ss`). Mounted as the **top-most** Stack layer in
    `ambient_screen.dart` (z-order precedence over every overlay/banner) and shown
    whenever timers run and the full-screen form is hidden (a
    turn is active or a full-screen mode is up), so it stays glanceable over the
    conversation, music, recipe, weather, or place views.
- **Shown full screen as soon as a timer is set (2026-10-21):** a `timerStarted` for a
  **new** id sets `AssistantState.timerSpotlight` (folded into `effectiveTimerDisplay`), so
  `FullScreenTimersView` replaces the
  conversation panel immediately, mid-turn (the listening ring still overlays it); the
  user speaking again (wake word / new transcript) clears it and brings the panel back.
  Full-screen modes (recipe/weather/place/camera/music) still win. An adjust re-emit of
  an existing id does not spotlight.
- Fed by `AssistantState.timers` (Rust `timerStarted` / `timerFinished` /
  `timerCancelled`). A **ringing** card shows a big **"Cancel timer"** button (key
  `cancel-timer-<id>`; the card/chip is tappable too) → `onDismiss(id)` →
  `AssistantController.dismissTimer`, which calls the native `dismiss_timer(id)` to stop
  the engine's repeating alarm; running timers are not dismissible.
  `resolveTimerNames` shows the spoken label (from either the Core's System-1 timer
  fast path or the `set_timer` tool) and labels only unnamed timers "Timer" / "Timer N".
- **Add/remove time:** the Core's `adjust_timer` tool sends an `anamanti-timer`
  `adjust` frame carrying a signed `delta_secs`; the device applies it to the running
  timer's own deadline (floored at 1s, capped at 24h) and re-emits `timerStarted` for
  the same id, which re-anchors the countdown + ring to the new deadline.

---

## 4b. Music — now-playing screens + control overlay

The music UI (see [`MusicPlan.md`](./MusicPlan.md) P6) is driven by a now-playing
snapshot the **Anamanti Core** pushes over a **separate persistent channel**
(`role="music"`, frame `anamanti-music`) — the twin of the ambient weather channel,
**not** the per-turn `WakeWordEvent` stream. `MusicChannelController`
(`lib/src/engine/music_channel_controller.dart`) subscribes to `startMusicChannel` and
forwards each push's JSON to `AssistantController.applyMusicPush`, which folds it into
`AssistantState.music` (a tolerant `MusicData`, `lib/src/engine/music_data.dart`); an
empty payload is a `dismiss` (nothing playing). Because "Flutter owns presentation,"
**the device decides what's on screen** — a push only provides data (including the
display's own `display_style`); the full screens open on a tap, by voice, or because this
display's style is full screen — never on a bare Core command.

Presentation (`AssistantState.effectiveMusicScreen` ∈ `hidden` / `nowPlaying` / `nextUp`,
resolved from a Core mode > the explicitly opened `musicScreen` > the **music display
style**):

**Display style** ([`MusicDisplayStylePlan.md`](./MusicDisplayStylePlan.md)): chosen **per
display** by the Core, `AssistantState.musicDisplayStyle` ∈ `compact` (the bar) /
`fullscreen` (the `NowPlayingView` shows automatically while Spotify or radio plays and
someone is present; it yields to Core modes and returns when they close, and falls to the
away clock when nobody is there). By default it is full screen on the display the play
request came from and compact on the others. Mirrored from the `display_style` field on this
display's now-playing pushes; this display alone switches with the bar's expand button, the
full view's minimize button, or by voice to it ("make the music full screen" / "use the
small music player" → `music_screen` `fullscreen`/`compact`) for the current music; the
config page (Music & Devices → Displays → "Music player on each display") pins a display to
Automatic / Small bar / Full screen.
- **`MusicControlOverlay`** — `lib/src/ui/music_control_overlay.dart`. The compact
  transport bar (artwork + title/artist + prev / play-pause / next + volume) that rides
  the ambient screen whenever a track is playing and no full music screen / Core mode /
  turn is up. Keys: `music-overlay-open` / `-previous` / `-play-pause` / `-next` /
  `-volume` (+ `-volume-up`), `-fullscreen` (expand → full-screen style). Tapping it →
  `openNowPlaying`.
- **`NowPlayingView`** — `lib/src/ui/now_playing_view.dart`. Full-screen now-playing:
  album artwork (graceful network fallback), title/artist/album, a progress bar, the
  transport row, and a volume slider. Keys: `music-close`, `music-previous`,
  `music-play-pause`, `music-next`, `music-volume`, `music-show-queue`. Opened by tapping
  the overlay or by the full-screen style; the minimize control (`music-close`,
  `close_fullscreen` icon) returns to the overlay and, with the full-screen style set,
  switches it to compact (**never stops playback**).
- **`NextUpView`** — `lib/src/ui/next_up_view.dart`. Full-screen up-next queue list
  (`music.nextUp`), reached from the now-playing view's "Up next" control. **Tapping a
  row jumps to that track** (`onSelect` → `AssistantController.skipToQueueTrack(i)` →
  `anamanti-music-control` `skip_to` with the row's 0-based index; the Core jumps there —
  one offset play within an album/playlist, else skipping forward `i + 1` tracks) and returns to the now-playing view. Keys: `next-up-close`,
  `next-up-back`, `next-up-row-<i>`. Empty queue shows "Nothing queued".
- **`AlbumArt`** — `lib/src/ui/album_art.dart`. The shared artwork tile all three surfaces
  use (overlay 40 px, up-next rows 56 px, now-playing 280 px): a rounded network image
  with a music-note fallback while loading, on error, or when there's no artwork URL.
  `fit` is `cover` for album art and `contain` for radio station logos.

**Live radio** ([`RadioPlan.md`](./RadioPlan.md)): the same surfaces render a radio
snapshot (`MusicData.isRadio`, `source:"radio"` + `station_id`/`station_name`). The
now-playing header is the **station name with a red LIVE badge** (`music-live-badge`), the
logo is shown uncropped, the stream's current song is the headline (`music-radio-song`,
artist under it; "Live radio" when the stream sends no song), there is **no progress bar
and no Up next**, and the transport is previous station / **stop** (`Icons.stop_circle`)
/ next station. The overlay shows a stop control and "artist · station". The controls
send the same `pause`/`next`/`previous`/`volume` actions; the Core applies them to the
radio. Up next (touch or voice) falls back to now-playing for radio, and the view is keyed
on the station so a new song doesn't rebuild it.

Controls route back to the Core device→Core: `AssistantController` holds a
`MusicControlSink` (`main.dart` wires it to the FRB `musicControl`), which issues a
short-lived `anamanti-music-control` request/response round-trip (play/pause→the device
sends the explicit `pause`/`resume`, plus `next`/`previous`/`volume`) that the Core maps
to the `spotify_control` controller. See `agents.md` + `MusicPlan.md`.

**Voice control** (mirrors the recipe voice-nav pattern): the music screen is drivable by
voice. Whenever the music full screen is open, `AssistantController` pushes a
`DisplayContext::Music` block on the turn's `audio-start` via the FRB `setMusicContext`
(playing / title / artist / which view), so the LLM knows what's on screen. The Core's
**`music_screen`** `VoiceTool` (advertised alongside `spotify_control`) emits
`DeviceAction::MusicScreen` → an `anamanti-music` `screen` frame on the per-turn socket →
`WakeWordEventKind.musicScreen` (target screen in the event's `recipeAction` string, the
same carrier `fontAdjust` uses) → `AssistantController._applyMusicScreen`, which routes
"show me what's playing" → `openNowPlaying`, "what's up next" → `showMusicQueue`, and "hide
the music" → `closeMusicScreen` (the same methods touch uses), and "make the music full
screen" / "use the small music player" (`screen` = `fullscreen` / `compact`) →
`setMusicDisplayStyle` (already recorded for this display by the Core, so not sent back). A Core-pushed mode
(recipe/weather/place/camera) sets `musicScreen: hidden` so the music full screen yields to it.
Playback-by-voice (play/pause/skip/volume) stays on the existing `spotify_control` tool.

## 5. Overlays — away / off mode

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
- **Night dimming:** when the camera-as-lux-sensor reports the room's lights are off
  (`AssistantState.roomDark`, from `AmbientLight` events) and "Dim at night" is on,
  both levels drop to `nightBrightnessNear` (0.15) / `nightBrightnessAway` (0.02). No
  new on-screen element — only the backlight changes; turning a light on restores the
  normal levels within ~2 s.
- **Covered camera** (privacy shutter / tape — near-black, near-uniform frames held
  10 s, `AssistantState.cameraCovered`): **Clock mode** (default) keeps the blind
  camera's reading — dark + nobody there, so the dim away clock; **Day & night times**
  treats the room as occupied and lit by day (full ambient screen, normal levels) and
  as empty and dark during the night window (dim away clock at night levels; a touch or
  voice turn wakes it at 0.15). Resolved by `effectivePresent` / `effectiveRoomDark`,
  which feed `screenAwake` and the backlight.

---

## 6. Banners

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

**`UpdateBanner`** — `lib/src/ui/update_banner.dart`
- Top-center card (same styling as `NotificationBanner`, blue `system_update_alt`
  icon) for the in-app updater. Shows when a newer build is **available** (title +
  version + an **Update** button) and through the user-initiated download (a
  `LinearProgressIndicator` + percentage), **Ready to install** (an **Install**
  button), **Installing…**, and download/install **errors** (a **Retry** button);
  `x` dismisses. Silent background *check* failures never raise it.
- Driven by `UpdateController` (`lib/src/engine/update_controller.dart`): a
  `ChangeNotifier` sidecar that calls the Rust updater FRB
  (`check_for_update`/`download_update`) and the native `anamanti_display/updater`
  channel (`PackageInstaller` install). Rendered as the top Stack layer in
  `AmbientScreen`; only present on the `selfUpdate` flavor (null → hidden on
  `fdroid`). Design: `plans/UpdaterPlan.md`, `architecture.md §4`.

---

## 7. Conversation UI

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
  timers, camera `userPresent` presence, follow-up turns, and the Core-pushed
  full-screen modes with auto-close timers (weather 60 s, place 5 min, live camera
  60 s). Pushes recipe/weather/place/camera screen context back to Core so voice can
  drive (or close) the screen.
  Loading any full-screen mode unloads the previous one so they never stack.

Full state-machine flow: `agents.md` cheat-sheet + `architecture.md §4`.

---

## 8. Full-screen views (pushed from Anamanti Core)

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
  key `recipe-steps`, numbered). Rows are built eagerly (`SingleChildScrollView` +
  `Column`, a `GlobalKey` per step) so any step can be scrolled to; the highlighted
  step is drawn as an amber **bubble** (tinted rounded card + accent outline + filled
  number circle, key `recipe-step-highlighted`).
- Triggered by `WakeWordEventKind.showRecipe` (Core `recipe_lookup`, or `saved_recipes`
  `show` for a recipe saved in Cadora / NextHaul — same payload, no Display change) →
  `AssistantState.recipe`; persists across turns while cooking. Tabs switch by
  touch **or** voice (`recipeNavigate`); voice scroll (`recipeScroll`) animates
  the active pane; voice "go to step N" / "next step" (`recipeStep` →
  `AssistantState.recipeStep` + `recipeStepSeq`) switches to Steps, scrolls the step
  into view and highlights it; tapping a step highlights it too. Scroll position, tab,
  highlighted step and the recipe's ingredient/step lines are reported back to Core as
  screen context (so "how many tomatoes?" is answered from the screen).
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

**`CameraView`** — `lib/src/ui/camera_view.dart` (live front-door camera,
[`CameraPlan.md`](./CameraPlan.md))
- Full-screen black background with the NVR's live RTSP **sub-stream** (H.264,
  640×360) centered at its aspect ratio, **video only** (volume 0; no audio path; it
  doesn't take audio focus). A top gradient bar holds a camera icon + the camera name
  (key `camera-name`) and a ✕ close (key `camera-close`). A spinner (key
  `camera-loading`) shows until the player first reports ready; an error state (key
  `camera-error`, "Couldn't reach the camera" + the Core's reason when it gave one)
  shows when the stream fetch fails or the player errors.
- Played with `video_player` (`VideoPlayerController.networkUrl`) using
  **`VideoViewType.platformView`** — mandatory: the Echo Show's MTK hardware decoder
  fails on the default texture view. The Android implementation is the vendored,
  patched `third_party/video_player_android` (RTP over TCP + a low-latency start
  buffer for RTSP; see its `ANAMANTI_PATCHES.md`). `VideoPlayerValue.position` is not
  used as a liveness signal (it stays ~0 for live RTSP). The player is disposed as soon
  as the view closes.
- **Triggers:** voice only for now — the Core's camera fast path ("show the front
  door", "who's at the door") or its `show_camera` tool sends `anamanti-camera`
  (`show`, camera name) on that display's turn socket → `WakeWordEventKind.showCamera`
  (name in the `recipeAction` carrier) → `AssistantState.camera` (`CameraSession`,
  `lib/src/engine/camera_session.dart`). The controller then pulls the stream URL from
  the Core with the FRB `getCameraStream` (`anamanti-get-camera-stream` →
  `anamanti-camera-stream`). The URL embeds the NVR credentials: it is held in memory
  only, never persisted or logged. A newer show replaces the camera (fresh fetch,
  restarted timer).
- **Dismiss:** the ✕, voice "stop" / "close it" (the display stamps
  `screen: {kind: "camera", camera: {name}}` on `audio-start` via `set_camera_context`,
  so the Core's stop ladder sends `anamanti-camera` `dismiss` → `dismissCamera`), or the
  **60 s auto-close**. Opening any other Core mode closes it, and vice versa.

**Data models** (tolerant `tryParse(json)` mirrors of Core structs; a malformed
payload is ignored, not crashed): `RecipeData` (`lib/src/engine/recipe_data.dart`),
`WeatherData` / `CurrentConditions` / `WeatherHour` / `WeatherDay`
(`lib/src/engine/weather_data.dart`), `PlaceData` (`lib/src/engine/place_data.dart`).

---

## 9. Settings (navigation route)

**`SettingsScreen`** — `lib/src/ui/settings_screen.dart` (shell) +
`lib/src/ui/settings/` (one file per section, the shared `settings_widgets.dart` rows,
and `settings_controller.dart`). Layout and rationale:
[`SettingsReorgPlan.md`](./SettingsReorgPlan.md).

- **Navigation.** Pushed from the gear on `AmbientScreen` (the only `Navigator.push` of
  Settings). Opens directly on **Assistant → Brain** — there is no menu page. A left
  **side rail** (key `settings-rail-<section>`) switches between six sections; a
  section with more than one tab shows a **tab strip** (key `settings-tab-<id>`).
  Every setting is at most two touches away. Each tab body is keyed
  `settings-page-<section>-<tab>`.
- **Fits one screen.** Every tab is laid out in **two columns** sized for the Echo
  Show 8's **960×600 dp** (1280×800 px at 213 dpi) and does not scroll; the outer
  scroll view is only a safety net. A widget test (with the real Material fonts)
  asserts no tab scrolls, with Advanced mode on and off. Long help text lives behind
  ⓘ buttons; small choices are segmented buttons / chips.
- **Changes apply immediately — no Save button.** `SettingsController`:
  - device-local settings are persisted (`SettingsStore`) and handed to `main.dart`'s
    `_onSettingsApplied` after a ~600 ms debounce; applies are **serialized**, and a
    burst of changes costs one apply (one engine restart at most). Sliders commit on
    release; text fields (device name, Ollama model, free-text voice, Update URL, Drive
    folder IDs) commit on submit / focus loss.
  - **capture gain and both wake-word thresholds** are pushed to the running engine
    **live while the slider moves** (FRB `updateDiagnosticsTuning(gainDb, threshold,
    activeThreshold)`), with no engine restart.
  - Core-managed settings (LLM, model, voice, VAD) go to the Mac via `applySettings`
    after a ~500 ms debounce; a rejected or failed apply **reverts** the control to the
    Core's last confirmed value and shows why in the AppBar.
  - anything still pending (a debounce, a slider mid-drag, a text field mid-edit) is
    **flushed when Settings closes**. Text-field drafts are held apart from the other
    settings, so an unrelated change never applies a half-typed value; a failed apply
    isn't marked applied, so it's retried; an edit made while an apply is still in
    flight (including an undo) is always sent.
  - **Re-pinning the Core** (Device → Connection) switches Settings itself to the new
    Mac once the change applies (`SettingsScreen.clientForCore`), reloading the Core
    settings and capabilities; People / Memory / Link a phone open against it too.
  - Live-tuned gain + thresholds are also folded into the engine controller's config
    (`AssistantController.updateLiveTuning`), so an engine reconnect re-seeds them
    instead of reverting.
  - The AppBar shows the apply status (key `settings-apply-status`: "Saving…" /
    "Saved" / the last error) and, in Advanced mode, an **Advanced** chip
    (`settings-advanced-chip`, jumps to Device → Maintenance).
- **Advanced mode** (`AppSettings.advancedMode`, default **on**, switch on Device →
  Maintenance, key `settings-advanced-mode`). Off hides the expert controls (marked
  *Adv* below) and the Listening → Microphone tab. Presentational only.
- **Core-dependent controls** (Brain, and the Core column of End of speech) show a
  "Contacting the assistant…" / "Assistant offline · Retry" banner and are disabled
  (not hidden) until the Core is reachable.

**Sections and tabs**

- **Assistant**
  - **Brain** — LLM backend (segmented: Local · Claude · OpenAI, + Mock *Adv*; key
    `settings-backend`), Anthropic auth (segmented API key / Subscription,
    `settings-anthropic-auth`, Claude only), Model (dropdown of the Core's recent models
    for cloud backends, free text for Ollama; `settings-model`), TTS voice (dropdown of
    installed Piper voices or free text; `settings-voice`), and a one-line
    **Capabilities summary** ("11 on · 1 key missing", `settings-capabilities-summary`)
    that taps through to the next tab.
  - **Capabilities** — a **read-only** grid (3 across) of what the Core is configured to
    use per capability, mirroring the Core config page's Config, Tools, System1, Speech
    and Memory→Embedding tabs: name, provider · model, and a **Cloud / Local / Off** chip
    — or a red **"API key missing"** warning (`Icons.key_off`) when a required key isn't
    set on the Core (card key `settings-function-<cat>-<name>`). Fetched via
    `anamanti-get-functions` → `anamanti-functions` (FRB `get_functions`) when Settings
    opens, with its own offline guard + Retry and a Refresh button. Never carries secret
    values — only each key's set/unset bit.
- **Listening**
  - **Wake word** — wake word (`settings-wakeword`), Sensitivity (idle)
    (`settings-threshold`), Sensitivity while speaking (`settings-active-threshold`),
    Capture gain (`settings-capture-gain`) — all three sliders live-tuned; *Adv*:
    Smoothing window (`settings-smoothing`), Fire on peak (`settings-fire-on-peak`); a
    "Test it live in Audio Diagnostics" button.
  - **Microphone** (*Adv* tab) — AudioRecord capture (far-field)
    (`settings-use-audiorecord`), Playback buffer (`settings-playback-buffer`); platform
    Noise suppression / Automatic gain control / Echo cancellation
    (`settings-platform-ns|agc|aec`, disabled unless AudioRecord is on). These restart
    the engine (debounced).
  - **End of speech** — two labelled columns so the two "end of speech" settings aren't
    confused. **Assistant (Core)**, which decides when you've finished talking:
    End-of-speech wait (`settings-vad-silence`); *Adv*: VAD engine (segmented Silero /
    Energy, Silero is the default; `settings-vad-engine`), Silero threshold
    (`settings-vad-silero-threshold`, Silero only), Voice level (`settings-vad-level`)
    — see [`VadSileroPlan.md`](./VadSileroPlan.md). **On-screen cue**, which only
    changes when the "thinking" indicator appears: Instant processing cue
    (`settings-endpoint-cue`); *Adv*: cue silence (`settings-endpoint-silence`) and cue
    silence level (`settings-endpoint-level`).
- **Audio Diagnostics** — the full `AudioDiagnosticsView` (below), kept as a top-level
  section because it's used often. Follows the engine controller across restarts.
- **Display**
  - **Screen** — Dim screen after (chips 30s…1h, `settings-dim-delay`, each
    `settings-dim-delay-<secs>`; an off-preset stored value shows the nearest), Font
    size (`settings-font-scale`, 85 %…160 %, the manual override of the voice font
    scale) with a live preview line; Listening ring on/off (`settings-listening-ring`)
    + Reactivity (`settings-ring-reactivity`); *Adv*: Attack / Release / Auto-range
    (`settings-ring-attack|release|decay`). Ring dials are disabled while the ring is off.
  - **Night** — *Lights off*: Dim at night (`settings-night-dim`, ⓘ explains the camera
    is a light sensor) + "Lights off means" chips Pitch dark / Dark / Dim
    (`settings-night-dark`, each `settings-night-dark-<3|8|20>`, disabled while night
    dimming is off) with a live `~N lux (lit|dark|camera covered)` readout when the
    engine runs. *When the camera is covered*: chips Clock mode / Day & night times
    (`settings-covered-clock|schedule`) with a one-line explanation of the selected
    mode, and Night hours `From – To` time pickers (`settings-night-start|end`, default
    21:00 – 07:00; enabled only for Day & night times).
  - **Photos** — idle photo source (segmented Gradients · Google Photos · Drive,
    `settings-photo-source`); Google Photos (Ambient API) link via QR
    (`settings-ambient-link`); Google Drive: Sync from Core (`settings-drive-sync`) +
    Choose folders (`settings-drive-pick`); *Adv*: raw folder-ID field
    (`settings-drive-folders`). Links apply as soon as they complete. **Change photo
    every** (chips 5s…10m, `settings-slide-interval`, each
    `settings-slide-interval-<secs>`; an off-preset stored value shows the nearest) —
    the per-device background-photo rotation time, applied live.
- **Household** — three large buttons that push the existing full screens: **People**
  (`settings-menu-people`), **Link a phone** (`settings-menu-pair-phone`), **Memory**
  (`settings-menu-memory`). See §10.
- **Device**
  - **Connection** — Device name (`settings-device-name`; the helper shows this
    display's stable MAC-derived `deviceId`, e.g. `anamanti-140ac5942aca`; the name is
    sent to the Core in `anamanti-hello` and appears in the Core config page's
    "Connected devices" list), the **Anamanti Core** picker (`settings-orchestrator`:
    Auto or a discovered Core — a strict pin), and a connection status line with Retry.
    Both controls are device-local and usable offline.
  - **Updates** (only on the `selfUpdate` flavor, i.e. when `SettingsScreen.updates`
    is non-null) — "Check for updates automatically" (`settings-auto-update`), the
    installed versionCode + live status, **Check now** (`settings-check-update`) and
    **Download & install / Install / Retry** (`settings-update-action`), driven by
    `UpdateController`; *Adv*: Update URL (`settings-update-url`, the Cloudflare R2
    base). See [`UpdaterPlan.md`](./UpdaterPlan.md).
  - **Maintenance** — **Permissions**: every Android permission the app declares (read
    live from the platform on open / on app resume), each row keyed
    `settings-permission-<name>` with a status chip; the two **runtime** permissions —
    **Camera** (presence detection + auto-brightness) and **Microphone** (wake word +
    voice) — show a **Grant** button (`settings-permission-grant-<name>`) when missing;
    a Refresh button (`settings-permissions-refresh`). A newly granted **Camera**
    triggers `SettingsScreen.onRequestEngineRestart` (wired to `main.dart`'s
    `_startEngine`). It exists because the app is the device Home/launcher, so there's
    no onboarding moment to prompt. Then the **Advanced mode** switch, and **Android**:
    Open app settings (`settings-permissions-app-settings`, for a permanently denied
    grant), **Switch Home app** (`settings-switch-home` →
    `Settings.ACTION_HOME_SETTINGS`) and **Open Android settings**
    (`settings-open-android-settings` → `Settings.ACTION_SETTINGS`) — the **kiosk escape
    hatch**, since the app is the device **Home/launcher** (manifest `category.HOME` +
    `DEFAULT`) and the OS relaunches it after a native crash. Channels:
    `anamanti_display/permissions` and `anamanti_display/maintenance` → `MainActivity`.
    adb fallback: `adb shell cmd package set-home-activity`.

**`AudioDiagnosticsView`** — `lib/src/ui/audio_diagnostics_view.dart`
- The **Audio Diagnostics** section — a live, visual mic-tuning surface. Rendered by
  `SettingsScreen` with the live `AssistantController` from
  `SettingsScreen.assistant` (a `ValueListenable`, wired from `main.dart`). After an
  engine restart the same view swaps to the new controller (`didUpdateWidget`) without
  re-announcing the transcription-only context, so it stays set while the page is up.
  People is likewise rebuilt with the live controller. Driven entirely by the always-on wake-word
  engine's event stream folded into `AssistantState` — no second audio path.
- Shows: a **mic input-level (RMS) meter** on a dBFS scale whose header line carries
  the **Microphone monitor** switch (`Key('diag-monitor-toggle')`, pauses/resumes the
  visualization only — the wake-word mic is always on); a **wake-word score meter**
  with the firing **threshold** drawn as a bright line and the smoothed score as a
  faint tick, flashing a **DETECTED** chip when the wake word fires; **live tuning**
  sliders (**Capture gain**, **Sensitivity (idle)**); a compact **recent detections**
  list (time + score, the last 4, two per row); a **numeric readouts** grid (three
  across: RMS, dBFS, score, smoothed, threshold, gain, device / rate / channels) whose
  header carries a **copy-config** button (`Key('diag-copy-config')`); and a **Heard
  words** card.
- Layout: **two columns** (meters, tuning, detections on the left; readouts and heard
  words on the right) that fit the 864 dp-wide pane beside the rail without scrolling,
  even with a full detection list and a two-line transcript (covered by a widget test).
- **Heard words** (`Key('diag-transcript')`, up to two lines) shows what the server
  transcribed from the last thing said *while this screen is up*. Entering the section
  calls FRB `setDiagnosticsContext(active: true)` (and `false` on leave; injectable as
  `SettingsScreen.onDiagnosticsActive` for tests), which stamps a
  **transcription-only** display context (`screen.kind == "diagnostics"`) on each
  voice turn's `audio-start`; the Core then transcribes and relays the words here but
  **never sends them to the LLM** (no reply, no TTS, no memory). Reuses the normal
  wake-word turn (no second audio path).
- The tuning sliders write the same settings as Listening → Wake word, apply to the
  **running engine instantly** (no restart), and are saved automatically.
- Data path: the Rust `run_loop` enriches its periodic `WakeWordEventKind.level`
  event with the live `score` / `avgScore` / `threshold` / `gainDb` (see
  `rust/src/engine/mod.rs`, `WakeWordEvent::level_diag`); the controller stores them
  on `AssistantState` (`wakeScore`, `wakeAvgScore`, `wakeThreshold`, `captureGainDb`,
  `captureDevice` / `captureSampleRate` / `captureChannels`, `detectionSeq`,
  `lastDetectionScore`). Live tuning: `update_diagnostics_tuning` stores the gain, idle
  threshold and active threshold in atomics the loop re-reads every block (re-seeded
  from `WakeWordConfig` on each engine start); the in-turn threshold is
  `max(active, idle)`.

---

## 10. People, voice enrollment & other sub-screens

`PeopleScreen` + `SpeakerEnrollScreen`, `MemoryScreen` and `PairPhoneScreen` are all
**reachable** from the large buttons on **Settings → Household** (→ People, → Link a
phone, → Memory).

**`MemoryScreen`** — `lib/src/ui/memory_screen.dart`
- Reached from **Settings → Household → Memory** (`settings-menu-memory`, pushed as its own route
  like People). `Scaffold` + `AppBar("Memory")` with a 3-tab `TabBar`, each tab its own
  list + delete, all constructed with an `OrchestratorClient`:
  - **Facts** — the durable SQLite facts/preferences (`MemoryView`: facts vs
    preferences, explicit vs inferred) as `ListTile`s with per-entry delete + a
    "Forget all" action (confirm dialog). Uses `listMemories`/`deleteMemory`/
    `clearMemories` (unchanged frames).
  - **Conversations** — stored GraphRAG `Turn` nodes (`TurnView`): transcript + reply,
    speaker + relative time, and entity ("keyword") chips. A search field + a person
    dropdown (`listSpeakers`) + an optional entity filter chip (set by tapping a
    keyword) drive `listTurns(query, speakerId, entity)`. Per-turn delete (confirm) →
    `deleteGraphNode(turn.id)`.
  - **Keywords** — stored GraphRAG `Entity` nodes (`EntityView`: name + kind) via
    `listEntities(query)`; tapping one jumps to Conversations filtered to it; per-entity
    delete (confirm) → `deleteGraphNode(entity.id)`.
- Conversations/Keywords deletes are **Helix-index-only** (prune recall; the chat-log
  source of truth is untouched, so a rebuild can restore them) — surfaced as a caption
  at the bottom of those lists. Needs the Core on the HelixDB backend; the SQLite recall
  backend answers these frames `ok:false` and the tab shows an "unavailable" state.

**`PeopleScreen`** — `lib/src/ui/people_screen.dart`
- Reached from **Settings → Household → People** (`settings-menu-people`). `Scaffold` +
  `AppBar("People")`. Lists voice-identified speakers (`SpeakerView`; named people vs
  anonymous "Speaker N", voice-clip counts). Each row has a **play button**
  (`speaker-play-<id>`, ▶) that replays that person's recorded voice clips through the
  assistant's speaker so a human can listen and recognize who it is before naming them
  (`OrchestratorClient.playSpeakerClips` → the Core streams the clips back; a snackbar
  reports how many played, or that none are saved yet). Rows are badged against the Mac's
  household roster (`OrchestratorClient.listHouseholdMembers`): a home avatar + "Household
  member" when the voice's name matches a member, "Not linked to household" for a named
  voice outside the roster. Per-row menu: **Link to household member…**
  (`speaker-link-<id>` → a member picker `link-member-<name>`: "This voice becomes X", or
  "Merge into X's voiceprint" (→ confirm `link-merge-confirm`) when X already has one;
  "Already linked" disabled; empty roster → a snackbar pointing to the Mac's Household
  page; calls `OrchestratorClient.linkSpeaker`), Name/rename,
  **Add voice samples** (`speaker-add-samples-<id>`, only when the live engine controller is
  wired — opens `SpeakerEnrollScreen` in add-samples mode for that person), Merge with…, Forget. An **"Add voice"** FAB (`people-add`, shown when the live engine
  controller is wired) opens the guided enrollment flow below. Constructed with an
  `OrchestratorClient` (+ the `AssistantController` and native `setEnrollContext` for
  enrollment). See [`speaker_id_plan.md`](./speaker_id_plan.md).

**`SpeakerEnrollScreen`** — `lib/src/ui/speaker_enroll_screen.dart`
- `Scaffold` + `AppBar("Add a voice")`. The guided "train a voice" flow
  (SpeakerEnrollmentPlan.md Phase 2): a "Whose voice is this?" step — **household members**
  as `ChoiceChip`s (`enroll-member-<name>`; a voice icon marks members who already have a
  voiceprint) fetched via `OrchestratorClient.listHouseholdMembers`, **or** a new name in the
  `enroll-name` field (a typed name matching a member resolves to that member's spelling); an
  `enroll-name-hint` line says whether the voice will be linked to an existing member, added to
  their existing voiceprint, or added to the household as a new person — → a capture step
  with a **phrase guide** (`enroll-guide-<i>`: the session's phrases, captured ones ticked off,
  the current one highlighted; beside the controls on landscape ≥700 px, stacked below on
  narrow screens). Cancel / Save are **pinned** under the scrolling content so they stay
  on the ~960×600 screen. Both the capture and verify steps show a live **capture-status
  panel** (`EnrollCaptureStatus`, `lib/src/ui/enroll_capture_status.dart`, key
  `enroll-capture-status`) mapped by `enrollCueFor` from `AssistantState.phase` /
  `captureReady` / `online` + the last sample result: *Starting microphone…* → *Waiting for
  "<wake word>"* → **Listening — read the phrase now** (red, recording dot, "Heard
  "<wake word>"", an auto-ranged `MicLevelMeter` fed `micLevel`) → *Checking that sample…*
  (spinner, from the on-device end-of-speech cue) → *Sample captured* / *Sample not used* +
  the Core's reason (flashed for `kEnrollResultFlash` = 3 s, cleared early by the next wake
  word); plus *Can't reach the assistant* and *Microphone restarting…*. Presentation only —
  no new engine events; implicit animations only.
  **Add-samples mode** (`speakerId` + `personLabel`, from the People ⋮ menu): `AppBar("Add
  voice samples")`, skips the name step, starts the guide later in `kEnrollPhrases` so a
  re-train reads fresh sentences, and commits via `enrollFinish(…, speakerId:)` into that exact
  voiceprint. Then the shared flow: a capture step that prompts the user to
  say the wake word and read short phrases (progress dots, per-sample accept/retry feedback)
  → commit → a verify step that confirms recognition ("Recognized Dana!", with confidence).
  Captures reuse the ordinary wake-word turn via an `enroll` display context (stamped by the
  native `setEnrollContext`), so there is **no second audio path**; each capture's result
  arrives as a `WakeWordEvent.enrollResult` folded into `AssistantState`. Commit/cancel are
  `OrchestratorClient.enrollFinish`/`enrollCancel`. See
  [`SpeakerEnrollmentPlan.md`](./SpeakerEnrollmentPlan.md).

**`PairPhoneScreen`** — `lib/src/ui/pair_phone_screen.dart`
- Reached from **Settings → Household → Link a phone** (`settings-menu-pair-phone`, pushed as its own
  route like People/Memory). `Scaffold` + `AppBar("Link a phone")`. Matches a 4-character
  **AppSaid pairing code** — shown on a family member's phone in the AppSaid app ("Link
  another device") — to a **person**: a name field (`pair-name`) + a 4-char code field
  (`pair-code`, upper-cased, alphanumeric), then a **Link phone** button (`pair-submit`,
  disabled until both are valid). Submitting calls
  `OrchestratorClient.pairPhone(name, code)`, which round-trips **`anamanti-pair-phone` →
  `anamanti-pair-result`** to the Core; the Core redeems the code (`POST /v1/pair/claim`),
  learns the phone's `user_key`, and stores it on the matching **household member's**
  AppSaid address — **auto-adding the person to the household** if new — so recipients stay
  unified on the household (the key never returns to the device). On success a done step shows the Core's
  confirmation + **Done** (`pair-done`); a bad/expired code or unreachable Mac shows the
  reason inline (`pair-error`). See [`architecture.md`](./architecture.md) §8.4.

---

## Related docs

- [`agents.md`](../agents.md) — build guidance, boundaries, state-machine cheat-sheet.
- [`architecture.md`](./architecture.md) — the design (§4 state machine + wire format).
- [`Plan.MD`](./Plan.MD) — phases, decisions, open questions.
- Feature plans: [`RecipePlan.md`](./RecipePlan.md), [`WeatherPlan.md`](./WeatherPlan.md),
  [`PlacesPlan.md`](./PlacesPlan.md), [`MusicPlan.md`](./MusicPlan.md).
</content>
</invoke>
