// The always-on ambient display screen (Plan.MD §3, Phase 5; §5 proximity).
//
// Layered, landscape-first layout tuned for the 8-inch Echo Show:
//  * Background: the idle photo slideshow (always running).
//  * A scrim + live conversation panel fades in while a turn is active.
//  * A subtle clock (idle) and connection-status chip sit in the corners.
//  * **Away / "off" mode:** when the camera proximity sensor reports nobody is in
//    front of the display (and no turn is active), everything hides and only a
//    large, dimmed, centered clock remains — a calm at-a-glance face. Someone
//    approaching flips `userPresent` back and the full ambient screen returns.
//
// The screen is driven by two [ChangeNotifier]s — [AssistantController] for the
// voice turn (and proximity presence) and [SlideshowController] for the idle
// imagery — so the photo cycle is fully decoupled from connectivity and keeps
// running when the Mac is offline.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/engine/notification_controller.dart';
import 'package:anamanti_display/src/engine/weather_data.dart';
import 'package:anamanti_display/src/slideshow/photo_source.dart';
import 'package:anamanti_display/src/ui/conversation_view.dart';
import 'package:anamanti_display/src/ui/listening_overlay.dart';
import 'package:anamanti_display/src/ui/music_control_overlay.dart';
import 'package:anamanti_display/src/ui/next_up_view.dart';
import 'package:anamanti_display/src/ui/notification_banner.dart';
import 'package:anamanti_display/src/ui/now_playing_view.dart';
import 'package:anamanti_display/src/ui/recipe_view.dart';
import 'package:anamanti_display/src/ui/slideshow_view.dart';
import 'package:anamanti_display/src/ui/status_indicator.dart';
import 'package:anamanti_display/src/ui/timers_overlay.dart';
import 'package:anamanti_display/src/ui/weather_icons.dart';
import 'package:anamanti_display/src/ui/place_view.dart';
import 'package:anamanti_display/src/ui/seven_day_view.dart';
import 'package:anamanti_display/src/ui/weather_view.dart';

class AmbientScreen extends StatelessWidget {
  const AmbientScreen({
    super.key,
    required this.assistant,
    required this.slideshow,
    this.notifications,
    this.onOpenSettings,
    this.listeningRingEnabled = true,
    this.ringReactivity = 1.0,
    this.ringAttack = 0.65,
    this.ringRelease = 0.08,
    this.ringDecay = 0.99,
  });

  final AssistantController assistant;
  final SlideshowController slideshow;

  /// Whether the glowing "listening" ring pops up while listening, and its tuning
  /// (see [ListeningOverlay]) — user-adjustable in Speech Processing settings.
  /// Purely presentational; defaults keep tests/other callers unchanged.
  final bool listeningRingEnabled;
  final double ringReactivity;
  final double ringAttack;
  final double ringRelease;
  final double ringDecay;

  /// Proactive notifications pushed by the orchestrator (Approach A). When null (as
  /// in widget tests that only exercise the turn UI) no banner is shown.
  final NotificationController? notifications;

  /// Opens the settings screen (Phase 6). When null, no settings control is shown
  /// (e.g. in widget tests that only exercise the reactive turn UI).
  final VoidCallback? onOpenSettings;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: Colors.black,
      body: AnimatedBuilder(
        animation: assistant,
        builder: (context, _) {
          final state = assistant.state;
          // Keep the panel (and dimmed scrim) up not just while the turn is active
          // but for as long as the reply audio is still playing, so the text stays
          // on screen until it stops being read aloud.
          final active = state.displayActive;
          // Recipe mode: a parsed recipe is on screen (persists across turns while
          // cooking). It takes the screen over the idle presentation but yields to an
          // active voice turn (the conversation panel draws above it).
          final recipeActive = state.recipeActive;
          // Weather mode: the full-screen forecast is on screen. Like recipe mode it
          // takes over the idle presentation but yields to an active voice turn.
          final weatherActive = state.weatherActive;
          final placeActive = state.placeActive;
          // Core-pushed full-screen modes (recipe/weather/place). These win over the
          // device-toggled music screen, which is suppressed while one is up.
          final coreMode = recipeActive || weatherActive || placeActive;
          // Music: `musicActive` means a track is playing (the compact control overlay
          // rides the ambient screen); a full music screen (now-playing / up-next) opens
          // only when the user taps the overlay, and yields to a Core-pushed mode.
          final showNowPlaying = state.musicScreen == MusicScreen.nowPlaying &&
              state.musicActive &&
              !coreMode;
          final showNextUp = state.musicScreen == MusicScreen.nextUp &&
              state.musicActive &&
              !coreMode;
          final musicFullScreen = showNowPlaying || showNextUp;
          // Any full-screen mode that overlays the idle presentation.
          final modeActive = coreMode || musicFullScreen;
          // Away / "off" mode: nobody in front of the display and no active turn.
          // Only the big centered clock shows; everything else fades away. A turn
          // always wins (saying the wake word implies you're here), so off mode is
          // strictly the idle-and-absent case. Recipe/weather mode also imply
          // engagement, so they suppress the away face.
          // The inverse of [AssistantState.screenAwake] — the single source of truth
          // shared with the backlight actuator so the away-face and the physical
          // brightness always agree (equivalent to `!userPresent && !active &&
          // !modeActive`).
          final offMode = !state.screenAwake;
          // The compact music control overlay rides the ambient screen when a track is
          // playing, no full music screen is up, no Core mode is up, no turn is active,
          // and someone is present.
          final musicOverlayVisible = state.musicActive &&
              !musicFullScreen &&
              !coreMode &&
              !active &&
              !offMode;
          // "Listening" cue: the device is actively listening to the user (wake word
          // or follow-up listen fired, through end-of-speech). Shows the big glowing
          // blue ring, which reacts to the live mic level. Suppressed when the user
          // has turned the ring off in settings.
          final listening = state.listening && listeningRingEnabled;
          return Stack(
            fit: StackFit.expand,
            children: [
              // Idle imagery, always cycling underneath.
              SlideshowView(controller: slideshow),

              // Away mode blacks out the slideshow so only the big clock remains.
              IgnorePointer(
                child: AnimatedContainer(
                  duration: const Duration(milliseconds: 500),
                  color: Colors.black.withValues(alpha: offMode ? 1.0 : 0.0),
                ),
              ),

              // Dim scrim that deepens while a turn is active so text stays legible.
              // Pointer-transparent so idle swipes reach the slideshow below.
              IgnorePointer(
                child: AnimatedContainer(
                  duration: const Duration(milliseconds: 400),
                  color: Colors.black.withValues(alpha: active ? 0.55 : 0.15),
                ),
              ),

              // Big, screen-filling timers — the idle presentation. Fades out while a
              // turn is active (the conversation wins the screen) and in away mode
              // (only the clock shows); survives as the compact badge below. Ignores
              // pointers when hidden so it never steals taps meant for the conversation.
              AnimatedOpacity(
                opacity: (active || offMode || modeActive) ? 0 : 1,
                duration: const Duration(milliseconds: 300),
                child: IgnorePointer(
                  ignoring: active || offMode || modeActive,
                  child: TimersOverlay(
                    timers: state.timers,
                    onDismiss: assistant.dismissTimer,
                  ),
                ),
              ),

              // Recipe mode: a full-screen 3-tab cooking view. Sits above the idle
              // presentation but below the conversation panel, so a voice turn mid-cook
              // ("next step", a follow-up) still overlays it. Dismissed by voice
              // ("done cooking") or the view's own close control.
              AnimatedOpacity(
                opacity: recipeActive ? 1 : 0,
                duration: const Duration(milliseconds: 250),
                child: IgnorePointer(
                  ignoring: !recipeActive,
                  child: recipeActive
                      ? RecipeView(
                          key: ValueKey(
                            state.recipe!.sourceUrl.isNotEmpty
                                ? state.recipe!.sourceUrl
                                : state.recipe!.title,
                          ),
                          recipe: state.recipe!,
                          onClose: assistant.dismissRecipe,
                          tab: state.recipeTab,
                          onTabSelected: assistant.setRecipeTab,
                          scrollSeq: state.recipeScrollSeq,
                          scrollDir: state.recipeScrollDir,
                          onScrollPositionChanged:
                              assistant.reportRecipeScrollPosition,
                        )
                      : const SizedBox.shrink(),
                ),
              ),

              // Weather mode: a full-screen forecast. The default hourly view (today's
              // big conditions panel + a 10-hour row) or, when the report's layout is
              // "week", the separate 7-day forecast widget (7 columns of highs/lows).
              // Same precedence as recipe mode — above the idle presentation, below the
              // conversation panel, so a voice turn still overlays it. Dismissed by
              // voice or the view's own close control.
              AnimatedOpacity(
                opacity: weatherActive ? 1 : 0,
                duration: const Duration(milliseconds: 250),
                child: IgnorePointer(
                  ignoring: !weatherActive,
                  child: weatherActive
                      ? (state.weather!.isWeek
                            ? SevenDayView(
                                key: ValueKey(
                                  'week:${state.weather!.locationLabel}',
                                ),
                                weather: state.weather!,
                                onClose: assistant.dismissWeather,
                              )
                            : WeatherView(
                                key: ValueKey(state.weather!.locationLabel),
                                weather: state.weather!,
                                onClose: assistant.dismissWeather,
                              ))
                      : const SizedBox.shrink(),
                ),
              ),

              // Place mode: a full-screen place card (photo + details). Same precedence
              // as recipe/weather mode — above the idle presentation, below the
              // conversation panel. Dismissed by voice or the view's own close control.
              AnimatedOpacity(
                opacity: placeActive ? 1 : 0,
                duration: const Duration(milliseconds: 250),
                child: IgnorePointer(
                  ignoring: !placeActive,
                  child: placeActive
                      ? PlaceView(
                          key: ValueKey(state.place!.name),
                          place: state.place!,
                          onClose: assistant.dismissPlace,
                        )
                      : const SizedBox.shrink(),
                ),
              ),

              // Music now-playing: a full-screen view (artwork + transport + volume),
              // opened by tapping the compact control overlay. Same precedence as the
              // Core-pushed modes — above the idle presentation, below the conversation
              // panel — and suppressed while a Core mode is up. The close control returns
              // to the overlay (music keeps playing).
              AnimatedOpacity(
                opacity: showNowPlaying ? 1 : 0,
                duration: const Duration(milliseconds: 250),
                child: IgnorePointer(
                  ignoring: !showNowPlaying,
                  child: showNowPlaying
                      ? NowPlayingView(
                          key: ValueKey('now-playing:${state.music!.trackTitle}'),
                          music: state.music!,
                          onClose: assistant.closeMusicScreen,
                          onPlayPause: assistant.playPauseMusic,
                          onNext: assistant.nextTrack,
                          onPrevious: assistant.previousTrack,
                          onVolume: assistant.setMusicVolume,
                          onShowQueue: assistant.showMusicQueue,
                        )
                      : const SizedBox.shrink(),
                ),
              ),

              // Music up-next: the full-screen queue list, reached from the now-playing
              // view's "Up next" control. Back returns to now-playing; close returns to
              // the overlay.
              AnimatedOpacity(
                opacity: showNextUp ? 1 : 0,
                duration: const Duration(milliseconds: 250),
                child: IgnorePointer(
                  ignoring: !showNextUp,
                  child: showNextUp
                      ? NextUpView(
                          key: const ValueKey('next-up'),
                          music: state.music!,
                          onClose: assistant.closeMusicScreen,
                          onBack: assistant.backToNowPlaying,
                        )
                      : const SizedBox.shrink(),
                ),
              ),

              // The live conversation panel fades in for the duration of a turn.
              AnimatedOpacity(
                opacity: active ? 1 : 0,
                duration: const Duration(milliseconds: 300),
                child: IgnorePointer(
                  ignoring: !active,
                  child: Padding(
                    padding: const EdgeInsets.all(28),
                    child: ConversationView(state: state),
                  ),
                ),
              ),

              // "Listening" cue: a large glowing blue ring centered on screen while
              // the device is listening to the user — from the wake word until
              // end-of-speech — reacting to the live mic level. Draws above the
              // conversation panel and below the status chip / settings / banner.
              // Pointer-transparent — a touch should still reach the slideshow /
              // dismiss controls beneath it. The ring widget only exists while shown
              // so its pulse controller isn't spinning during the whole idle day.
              IgnorePointer(
                child: AnimatedOpacity(
                  key: const Key('listening-cue'),
                  opacity: listening ? 1 : 0,
                  duration: const Duration(milliseconds: 250),
                  child: listening
                      ? ListeningOverlay(
                          level: state.micLevel,
                          reactivity: ringReactivity,
                          attack: ringAttack,
                          release: ringRelease,
                          decay: ringDecay,
                        )
                      : const SizedBox.shrink(),
                ),
              ),

              // Compact music control overlay, bottom-center — the lightweight transport
              // (artwork + title + prev/play-pause/next + volume) that rides the ambient
              // screen while a track plays. Tapping it opens the full now-playing screen.
              // Hidden during a turn, in away mode, and while any full screen is up.
              Positioned(
                left: 120,
                right: 120,
                bottom: 18,
                child: AnimatedOpacity(
                  key: const Key('music-overlay'),
                  opacity: musicOverlayVisible ? 1 : 0,
                  duration: const Duration(milliseconds: 250),
                  child: IgnorePointer(
                    ignoring: !musicOverlayVisible,
                    child: musicOverlayVisible
                        ? Align(
                            alignment: Alignment.bottomCenter,
                            child: MusicControlOverlay(
                              music: state.music!,
                              onPlayPause: assistant.playPauseMusic,
                              onNext: assistant.nextTrack,
                              onPrevious: assistant.previousTrack,
                              onVolume: assistant.setMusicVolume,
                              onTap: assistant.openNowPlaying,
                            ),
                          )
                        : const SizedBox.shrink(),
                  ),
                ),
              ),

              // Idle clock, bottom-left. Yields to the big timer display, to a turn,
              // and to away mode (where the large centered clock takes over).
              Positioned(
                left: 28,
                bottom: 24,
                child: AnimatedOpacity(
                  key: const Key('idle-clock'),
                  opacity:
                      (active ||
                          offMode ||
                          modeActive ||
                          state.timers.isNotEmpty)
                      ? 0
                      : 1,
                  duration: const Duration(milliseconds: 300),
                  child: _AmbientClock(weather: state.weatherCurrent),
                ),
              ),

              // Away-mode face: a large, dimmed, centered clock. Fades in when the
              // proximity sensor reports the room is empty; the low opacity (on top
              // of the already-dimmed backlight) keeps it calm and glare-free.
              IgnorePointer(
                child: AnimatedOpacity(
                  key: const Key('away-face'),
                  opacity: offMode ? 1 : 0,
                  duration: const Duration(milliseconds: 500),
                  child: const Center(child: _AmbientClock(large: true)),
                ),
              ),

              // Connection-status chip, top-right. Hidden in away mode.
              Positioned(
                right: 20,
                top: 18,
                child: AnimatedOpacity(
                  opacity: offMode ? 0 : 1,
                  duration: const Duration(milliseconds: 300),
                  child: StatusIndicator(state: state),
                ),
              ),

              // Compact timer badge, top-center — the presentation while a turn is
              // active, so the timers yield the screen to the live conversation but
              // stay glanceable. Padded clear of the settings button (top-left) and
              // status chip (top-right).
              Positioned(
                top: 16,
                left: 64,
                right: 120,
                child: AnimatedOpacity(
                  opacity: active ? 1 : 0,
                  duration: const Duration(milliseconds: 300),
                  child: IgnorePointer(
                    ignoring: !active,
                    child: Align(
                      alignment: Alignment.topCenter,
                      child: TimersOverlay(
                        compact: true,
                        timers: state.timers,
                        onDismiss: assistant.dismissTimer,
                      ),
                    ),
                  ),
                ),
              ),

              // Discreet settings control, top-left. Fades out during a turn and in
              // away mode so it never competes with the conversation or the face.
              if (onOpenSettings != null)
                Positioned(
                  left: 12,
                  top: 10,
                  child: AnimatedOpacity(
                    opacity: (active || offMode) ? 0 : 0.7,
                    duration: const Duration(milliseconds: 300),
                    child: IconButton(
                      key: const Key('open-settings'),
                      tooltip: 'Settings',
                      onPressed: (active || offMode) ? null : onOpenSettings,
                      icon: Icon(
                        Icons.settings,
                        color: Colors.white.withValues(alpha: 0.9),
                      ),
                    ),
                  ),
                ),

              // Proactive notification banner, top layer so it sits above the
              // conversation and clock. Rebuilds independently on its own controller,
              // and shows even in away mode (an alert should draw attention).
              if (notifications != null)
                ListenableBuilder(
                  listenable: notifications!,
                  builder: (context, _) {
                    final note = notifications!.current;
                    return AnimatedSwitcher(
                      duration: const Duration(milliseconds: 250),
                      child: note == null
                          ? const SizedBox.shrink()
                          : NotificationBanner(
                              key: ValueKey(note.id),
                              notification: note,
                              onDismiss: notifications!.dismiss,
                            ),
                    );
                  },
                ),
            ],
          );
        },
      ),
    );
  }
}

/// A minimal ticking clock for the idle screen. In [large] mode it renders as the
/// big, dimmed, centered away-mode face (time only); otherwise it's the small
/// bottom-left idle clock with the wake-word hint.
class _AmbientClock extends StatefulWidget {
  const _AmbientClock({this.large = false, this.weather});

  /// Render the large centered away-mode variant (dimmed, time only).
  final bool large;

  /// Current ambient conditions for the small icon + temperature beside the time
  /// (idle variant only). `null` hides the indicator (weather off / not yet fetched).
  final WeatherData? weather;

  @override
  State<_AmbientClock> createState() => _AmbientClockState();
}

class _AmbientClockState extends State<_AmbientClock> {
  late final Stream<DateTime> _ticks = Stream<DateTime>.periodic(
    const Duration(seconds: 1),
    (_) => DateTime.now(),
  );

  String _fmt(DateTime t) {
    final h = t.hour % 12 == 0 ? 12 : t.hour % 12;
    final m = t.minute.toString().padLeft(2, '0');
    final ampm = t.hour < 12 ? 'AM' : 'PM';
    return '$h:$m $ampm';
  }

  @override
  Widget build(BuildContext context) {
    return StreamBuilder<DateTime>(
      stream: _ticks,
      builder: (context, snap) {
        final now = snap.data ?? DateTime.now();
        if (widget.large) {
          // Away-mode face: big and dimmed. FittedBox guards the widest times
          // ("12:00 PM") against overflow on the 8-inch panel.
          return FittedBox(
            fit: BoxFit.scaleDown,
            child: Padding(
              padding: const EdgeInsets.symmetric(horizontal: 24),
              child: Text(
                _fmt(now),
                style: TextStyle(
                  color: Colors.white.withValues(alpha: 0.5),
                  fontSize: 168,
                  fontWeight: FontWeight.w200,
                  letterSpacing: 2.0,
                ),
              ),
            ),
          );
        }
        final weather = widget.weather;
        return Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.center,
              children: [
                Text(
                  _fmt(now),
                  style: TextStyle(
                    color: Colors.white.withValues(alpha: 0.92),
                    fontSize: 44,
                    fontWeight: FontWeight.w300,
                    letterSpacing: 1.0,
                  ),
                ),
                if (weather != null) _weatherChip(weather),
              ],
            ),
            Text(
              'Say the wake word to begin',
              style: TextStyle(
                color: Colors.white.withValues(alpha: 0.6),
                fontSize: 14,
              ),
            ),
          ],
        );
      },
    );
  }

  /// The small weather indicator beside the time: a condition icon + current
  /// temperature, sourced from the periodic ambient push.
  Widget _weatherChip(WeatherData weather) {
    final c = weather.current;
    return Padding(
      padding: const EdgeInsets.only(left: 16),
      child: Row(
        key: const Key('idle-weather'),
        mainAxisSize: MainAxisSize.min,
        children: [
          Icon(
            weatherIcon(c.weatherCode, isDay: c.isDay),
            size: 30,
            color: weatherIconColor(c.weatherCode, isDay: c.isDay),
          ),
          const SizedBox(width: 6),
          Text(
            '${c.temp}${weather.unitSuffix}',
            style: TextStyle(
              color: Colors.white.withValues(alpha: 0.92),
              fontSize: 28,
              fontWeight: FontWeight.w400,
            ),
          ),
        ],
      ),
    );
  }
}
