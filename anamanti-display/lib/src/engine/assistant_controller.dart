// Reactive bridge between the native Rust engine's event stream and the Flutter
// UI (Plan.MD §3, Phase 5).
//
// The Rust engine emits a single `Stream<WakeWordEvent>` covering the whole
// lifecycle: capture start, wake-word detection, the Wyoming turn (connecting →
// streaming → transcript → reply tokens → speaking) and disconnects. This
// controller folds that stream into a small, observable [AssistantState] the
// widgets render, and owns **resilience**: if the engine stream ends or errors it
// restarts capture with exponential backoff, surfacing a subtle disconnected
// status in the meantime (the idle slideshow keeps running regardless).
//
// It is deliberately decoupled from `RustLib`: the engine stream is injected via
// [EngineStreamFactory], so widget/unit tests can drive synthetic events with no
// native library loaded.

// The private controller fields (`_config`, `_minBackoff`, `_maxBackoff`) are
// assigned in the initializer list rather than via initializing formals because
// Dart forbids private names on *named* constructor parameters.
// ignore_for_file: prefer_initializing_formals

import 'dart:async';

import 'package:flutter/foundation.dart';

import 'package:anamanti_display/src/engine/camera_session.dart';
import 'package:anamanti_display/src/engine/recipe_data.dart';
import 'package:anamanti_display/src/engine/place_data.dart';
import 'package:anamanti_display/src/engine/music_data.dart';
import 'package:anamanti_display/src/engine/night_schedule.dart';
import 'package:anamanti_display/src/engine/weather_data.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:anamanti_display/src/rust/api/settings.dart' show CameraStream;
import 'package:anamanti_display/src/settings/app_settings.dart'
    show kFontScaleMin, kFontScaleMax;

/// Opens the native engine event stream for a given config. Production passes
/// `startWakeWordEngine`; tests pass a fake.
typedef EngineStreamFactory =
    Stream<WakeWordEvent> Function(WakeWordConfig config);

/// Pushes the current recipe-screen state down to the native engine, so the next
/// voice turn's `audio-start` tells the orchestrator what the display is showing (and
/// which tab / scroll position) and it can drive the screen by voice. Production wires
/// the native [setRecipeContext]; null (the default, for tests) disables it.
typedef RecipeContextSink =
    void Function({
      required bool active,
      required String title,
      required String tab,
      required bool atTop,
      required bool atBottom,
      required int ingredientCount,
      required int stepCount,
      required int highlightedStep,
      required List<String> ingredients,
      required List<String> steps,
    });

/// Pushes the current weather-screen state down to the native engine, so the next voice
/// turn's `audio-start` tells the orchestrator the forecast is up (and what it shows) —
/// letting the LLM answer follow-ups in context or close it. Production wires the native
/// [setWeatherContext]; null (the default, for tests) disables it.
typedef WeatherContextSink =
    void Function({
      required bool active,
      required String location,
      required String units,
      required int temp,
      required String description,
    });

/// Pushes the current place-card state down to the native engine, so the next voice
/// turn's `audio-start` tells the orchestrator a place card is up (and which place) —
/// letting the LLM answer follow-ups in context or close it. Production wires the native
/// [setPlaceContext]; null (the default, for tests) disables it.
typedef PlaceContextSink =
    void Function({
      required bool active,
      required String name,
      required String address,
    });

/// Pushes the live camera view's state down to the native engine, so the next voice
/// turn's `audio-start` tells the orchestrator a camera view is up (and which camera),
/// letting "stop" / "close it" close it. Carries only the camera's name — never the
/// stream URL. Production wires the native [setCameraContext]; null (the default, for
/// tests) disables it.
typedef CameraContextSink =
    void Function({required bool active, required String name});

/// Fetches the live stream for a camera (by display name) from the Core when the
/// camera view opens. Production wires the FRB `getCameraStream`; tests inject a fake.
/// The returned URL embeds the NVR credentials: callers keep it in memory only and
/// never log it. Null (the default) skips the fetch, leaving the view loading.
typedef CameraStreamFetcher = Future<CameraStream> Function(String camera);

/// Pushes the current font-scaling context down to the native engine, so the next voice
/// turn's `audio-start` tells the orchestrator whether a resizable text surface is on
/// screen and what the current global scale is. Lets "increase font" / "decrease font"
/// resolve only when there is something to resize (and lets the assistant say when it is
/// already at the maximum/minimum). Production wires the native [setFontContext]; null
/// (the default, for tests) disables it.
typedef FontContextSink =
    void Function({
      required bool scalable,
      required double scale,
      required bool atMin,
      required bool atMax,
    });

/// Probes whether the Mac orchestrator is currently reachable. Returns `true` if a
/// connection/handshake succeeded. Production wires this to a control-protocol
/// round-trip (mDNS discover + connect); tests inject a fake. When left null the
/// offline poll is disabled.
typedef OrchestratorProbe = Future<bool> Function();

/// Sends a music transport/volume command from the on-screen controls to the Core.
/// `action` ∈ `pause`/`resume`/`next`/`previous`/`volume`; `value` is the 0–100 percent
/// for `volume`. Production wires the native [musicControl] (a short-lived control
/// round-trip); null (the default, for tests) disables it so taps are no-ops.
typedef MusicControlSink = void Function({required String action, int? value});

/// Pushes the current music-screen state down to the native engine, so the next voice
/// turn's `audio-start` tells the orchestrator a music screen is up (what's playing + which
/// view) and the LLM can drive it by voice (`music_screen` show/queue/close) or answer
/// "what's this song?". Production wires the native [setMusicContext]; null (the default,
/// for tests) disables it. Only a full music screen counts as context — the compact overlay
/// does not, so it never steals the context from a recipe/weather/place screen.
typedef MusicContextSink =
    void Function({
      required bool active,
      required bool playing,
      required String title,
      required String artist,
      required String screen,
    });

/// Which music surface is currently shown, independent of whether music is playing
/// (`music != null`). While playing, the compact [MusicControlOverlay] rides the ambient
/// screen; tapping it opens the full now-playing screen, from which the up-next list is
/// reachable. Closing a full screen returns to [hidden] (the overlay) — it never stops
/// playback.
enum MusicScreen {
  /// No full music screen — only the compact control overlay (when music is playing).
  hidden,

  /// The full-screen now-playing view (artwork, transport, volume).
  nowPlaying,

  /// The full-screen up-next queue list.
  nextUp,
}

/// Where the current voice turn is, from the UI's point of view.
enum TurnPhase {
  /// No active turn — the ambient/idle screen (slideshow) is showing.
  idle,

  /// Wake word fired / mic streaming up: we are listening to the user.
  listening,

  /// The user has stopped speaking (detected locally on-device from the mic level)
  /// and the assistant is finalizing/transcribing — shown immediately as a cue so
  /// the screen isn't stuck on "Listening…" during the Mac's VAD + STT round trip.
  processing,

  /// Contacting the Mac orchestrator over the discovered Wyoming socket.
  connecting,

  /// Transcript received; the assistant is composing its reply.
  thinking,

  /// The reply's TTS audio is playing back through the speakers.
  speaking,

  /// A fatal engine error — capture is being restarted.
  error,
}

/// One on-device countdown timer, as the UI sees it (Phase 2 device actions). The
/// device owns the authoritative countdown + alarm; the UI just renders a live
/// countdown to [deadline] and highlights [finished] ones (their alarm is sounding).
@immutable
class TimerModel {
  const TimerModel({
    required this.id,
    required this.label,
    required this.deadline,
    required this.total,
    this.finished = false,
  });

  /// Stable device-assigned id (used to update/remove the right chip).
  final int id;

  /// Spoken label ("pasta"), or empty for an unlabeled timer.
  final String label;

  /// Wall-clock instant the timer fires, derived on start from the reported
  /// remaining seconds so the UI can count down smoothly without per-second events.
  final DateTime deadline;

  /// The timer's full requested duration (from the reported remaining seconds at
  /// start). Fixed for the timer's life; used to draw the progress ring, whose fill
  /// is `remaining / total`.
  final Duration total;

  /// True once the timer reached zero (the device alarm is sounding); the chip stays
  /// until acknowledged.
  final bool finished;

  TimerModel copyWith({bool? finished}) => TimerModel(
    id: id,
    label: label,
    deadline: deadline,
    total: total,
    finished: finished ?? this.finished,
  );
}

/// An immutable snapshot of everything the UI needs to render one frame.
@immutable
class AssistantState {
  const AssistantState({
    this.phase = TurnPhase.idle,
    this.online = false,
    this.transcript = '',
    this.reply = '',
    this.statusMessage = 'Starting…',
    this.wakeWord = '',
    this.micLevel = 0.0,
    this.wakeScore = 0.0,
    this.wakeAvgScore = 0.0,
    this.wakeThreshold = 0.0,
    this.captureGainDb = 0.0,
    this.captureDevice = '',
    this.captureSampleRate = 0,
    this.captureChannels = 0,
    this.detectionSeq = 0,
    this.lastDetectionScore = 0.0,
    this.captureReady = false,
    this.timers = const [],
    this.timerSpotlight = false,
    this.audioPlaying = false,
    this.userPresent = true,
    this.roomDark = false,
    this.ambientLux,
    this.cameraCovered = false,
    this.coveredMode = CoveredCameraMode.clock,
    this.scheduledNight = false,
    this.followUp = false,
    this.recipe,
    this.weather,
    this.weatherCurrent,
    this.place,
    this.camera,
    this.music,
    this.musicScreen = MusicScreen.hidden,
    this.musicDisplayStyle = MusicDisplayStyle.compact,
    this.recipeTab = 0,
    this.recipeScrollSeq = 0,
    this.recipeScrollDir = '',
    this.recipeStep = 0,
    this.recipeStepSeq = 0,
    this.enrollSeq = 0,
    this.enrollResultJson = '',
  });

  final TurnPhase phase;

  /// Whether the Mac orchestrator was reachable on the most recent attempt. Drives
  /// the subtle "disconnected" indicator; the slideshow ignores it.
  final bool online;

  /// The recognized speech for the current turn (empty between turns).
  final String transcript;

  /// The assistant's reply, accumulated token-by-token as it streams.
  final String reply;

  /// Latest human-readable engine status (capture device, model load, errors).
  final String statusMessage;

  /// Name of the wake word that last fired.
  final String wakeWord;

  /// Input RMS level (~0..1), surfaced in capture-only mode as a liveness cue and
  /// as the live mic meter on the audio-diagnostics screen.
  final double micLevel;

  /// The raw current-block wake-word confidence (~0..1), carried on every mic-level
  /// event while a model is loaded. Drives the diagnostics score meter; 0 when no
  /// model is loaded (capture-only).
  final double wakeScore;

  /// The smoothed wake-word score the detection gate actually tests (average, or peak
  /// in peak mode). Shown alongside [wakeScore] on the diagnostics meter.
  final double wakeAvgScore;

  /// The wake-word detection threshold currently in effect on the engine (idle, or the
  /// raised active threshold during a turn). Drawn as the firing line on the meter.
  final double wakeThreshold;

  /// The software capture gain (dB) currently applied by the engine. Reflects live
  /// diagnostics-screen adjustments; shown in the numeric readouts.
  final double captureGainDb;

  /// Capture device name reported when the mic started (diagnostics readout).
  final String captureDevice;

  /// Capture device's native sample rate in Hz (diagnostics readout).
  final int captureSampleRate;

  /// Capture device's channel count before mono downmix (diagnostics readout).
  final int captureChannels;

  /// Monotonic counter bumped each time the wake word fires. The diagnostics screen
  /// watches it to flash + append to its detection history (a counter, so repeated
  /// detections still register). 0 = none yet.
  final int detectionSeq;

  /// The smoothed score of the most recent wake-word detection (paired with
  /// [detectionSeq]).
  final double lastDetectionScore;

  /// True once the mic capture stream has started at least once.
  final bool captureReady;

  /// Active on-device timers (Phase 2), in start order. Independent of [phase] —
  /// they run and display during idle and during a turn alike.
  final List<TimerModel> timers;

  /// Whether a timer was just started in the current exchange, so the big
  /// screen-filling timers should take the screen over the conversation panel right
  /// away (instead of waiting for the turn to end). Set when a **new** timer id starts
  /// (an `adjust_timer` re-emit of an existing id doesn't count); cleared when the
  /// user speaks again (a wake word or a new transcript), so the next exchange gets
  /// the conversation panel back. Full-screen modes (recipe/weather/music…) still win.
  final bool timerSpotlight;

  /// Whether the reply's TTS audio is still playing out of the speaker. Set when
  /// SPEAKING begins and stays true past the turn's return to idle — because the
  /// device relays audio faster than real-time, so the ring keeps playing after the
  /// turn ends — until the engine reports the playback drained (`speakingDone`).
  /// Keeps the on-screen reply text up for exactly as long as it is being read.
  final bool audioPlaying;

  /// Whether the camera proximity sensor currently sees someone in front of the
  /// display. Drives the screen-brightness actuation (bright when present, dimmed
  /// when the room's been quiet). Defaults to `true` so the screen starts bright and
  /// stays bright if the camera is unavailable (permission denied / privacy shutter),
  /// rather than sitting dim (Plan.MD §5).
  final bool userPresent;

  /// Whether the camera-as-lux-sensor judges the room's lights to be off
  /// (`camera/ambient.rs`). The UI dims the backlight to its night levels while this
  /// is set (and night dimming is enabled). Defaults to `false` so an unavailable or
  /// still-settling camera never dims the screen.
  final bool roomDark;

  /// The lux sensor's latest smoothed illuminance estimate, or `null` before the
  /// first reading (or with no camera). A live readout for tuning in Settings.
  final double? ambientLux;

  /// Whether the camera lens looks covered (privacy shutter / tape) — near-black,
  /// near-uniform frames, reported by the lux sensor. A covered camera can't see the
  /// room, so its presence and darkness readings are resolved per [coveredMode].
  final bool cameraCovered;

  /// What a covered camera means (the user's Settings → Display → Night choice).
  final CoveredCameraMode coveredMode;

  /// Whether the current time is inside the user's night window. Only consulted
  /// while the camera is covered in [CoveredCameraMode.schedule].
  final bool scheduledNight;

  /// The covered camera is being overridden by the day/night schedule.
  bool get _scheduleOverrides =>
      cameraCovered && coveredMode == CoveredCameraMode.schedule;

  /// Presence as the screen should treat it: the camera's [userPresent], except that a
  /// covered camera on the day/night schedule counts as "someone's here" by day (the
  /// camera can't see anyone, so the screen stays awake) and defers to [userPresent]
  /// (voice/touch activity) at night.
  bool get effectivePresent =>
      userPresent || (_scheduleOverrides && !scheduledNight);

  /// Whether the room should be treated as dark for night dimming: the lux sensor's
  /// [roomDark], or — for a covered camera on the schedule — the night window.
  bool get effectiveRoomDark => _scheduleOverrides ? scheduledNight : roomDark;

  /// Whether the current turn was auto-opened as a **follow-up** to a question the
  /// assistant just asked (no wake word). Lets the UI show a "listening for your
  /// reply" cue. Set when the engine reports `ListeningFollowup`, cleared when a wake
  /// word starts an ordinary turn or the turn ends. See `plans/Plan.MD`.
  final bool followUp;

  /// The recipe currently shown on the recipe-mode screen, or `null` when the
  /// screen is closed. Pushed by the orchestrator's `recipe_lookup` tool and
  /// dismissed by voice ("done cooking") or a touch of the close control. Outlives
  /// the voice turn — it stays up while the user cooks, through follow-up turns and
  /// idle — so it lives on the state snapshot, not tied to [phase].
  final RecipeData? recipe;

  /// The active recipe tab: 0 = Overview, 1 = Ingredients, 2 = Steps. Driven by voice
  /// (`recipe_control`) and by touch of the bottom tab bar. Only meaningful while
  /// [recipeActive].
  final int recipeTab;

  /// A monotonically increasing counter bumped on each voice scroll command, paired
  /// with [recipeScrollDir]. The [RecipeView] scrolls the active pane whenever this
  /// changes (a counter, so repeating the same direction still fires). 0 = no command
  /// yet.
  final int recipeScrollSeq;

  /// The direction of the most recent voice scroll command: `'up'` / `'down'` (a page)
  /// or `'top'` / `'bottom'`. Read together with [recipeScrollSeq].
  final String recipeScrollDir;

  /// The highlighted recipe step, 1-based (0 = none). Set by a voice "go to step N"
  /// (`recipeStep` event) or a tap on a step; the [RecipeView] draws it as a bubble and
  /// the pushed screen context reports it, so "next step" can resolve.
  final int recipeStep;

  /// A counter bumped on each voice step jump (paired with [recipeStep]). The
  /// [RecipeView] scrolls the highlighted step into view whenever this changes — a
  /// counter, so asking for the same step again still brings it back on screen after
  /// the user scrolled away. 0 = no jump yet.
  final int recipeStepSeq;

  /// Monotonic counter bumped each time a voice-enrollment capture result arrives
  /// (SpeakerEnrollmentPlan.md Phase 2). The enroll screen watches it (a counter, so a
  /// repeated/rejected sample still registers) and reads [enrollResultJson]. 0 = none.
  final int enrollSeq;

  /// The latest `anamanti-enroll-result` data block as a raw JSON string (paired with
  /// [enrollSeq]): the orchestrator's reply to the most recent enrollment sample/verify
  /// capture. Empty until the first result.
  final String enrollResultJson;

  /// Whether recipe mode is currently on screen.
  bool get recipeActive => recipe != null;

  /// The forecast currently shown full-screen on the weather screen, or `null` when
  /// the screen is closed. Pushed by the `weather_lookup` tool ("show" action) and
  /// dismissed by voice or the close control. Outlives the voice turn.
  final WeatherData? weather;

  /// Whether the full-screen weather view is currently on screen.
  bool get weatherActive => weather != null;

  /// The latest ambient current conditions for the small indicator beside the clock,
  /// kept fresh by the orchestrator's periodic push (independent of [weather]). `null`
  /// until the first push arrives (or weather is disabled / no home location).
  final WeatherData? weatherCurrent;

  /// The place currently shown full-screen on the place card, or `null` when the card is
  /// closed. Pushed by the `places_lookup` tool ("show" action) and dismissed by voice or
  /// the close control. Outlives the voice turn.
  final PlaceData? place;

  /// Whether the full-screen place card is currently on screen.
  bool get placeActive => place != null;

  /// The live camera view, or `null` when it's closed (plans/CameraPlan.md). Opened by
  /// voice (the Core's camera fast path / `show_camera` tool); closed by the ✕, voice
  /// "stop" / "close it", or the auto-close timeout. Outlives the voice turn. Its
  /// [CameraSession.url] embeds credentials: memory only, never logged.
  final CameraSession? camera;

  /// Whether the full-screen live camera view is currently on screen.
  bool get cameraActive => camera != null;

  /// The current music now-playing snapshot pushed by the Core over the persistent music
  /// channel, or `null` when nothing is playing (a `dismiss` push). Independent of the
  /// voice turn — music plays and updates during idle. Drives the compact control overlay
  /// and the full music screens.
  final MusicData? music;

  /// Which music surface the user explicitly opened (tap / voice — see [MusicScreen]).
  /// Only meaningful while [musicActive]. What is actually on screen also depends on
  /// [musicDisplayStyle]; read [effectiveMusicScreen] for that.
  final MusicScreen musicScreen;

  /// This display's music widget (compact bar / full screen), chosen per display by the
  /// Core and mirrored here from this display's now-playing pushes, its on-screen
  /// expand/minimize button, and the voice `music_screen` tool. Shared by Spotify and
  /// radio. By default full screen where the play request came from, compact elsewhere.
  final MusicDisplayStyle musicDisplayStyle;

  /// Whether music is currently playing (there is a now-playing snapshot). The compact
  /// [MusicControlOverlay] rides the ambient screen whenever this is true and no full
  /// music screen / mode / turn is up.
  bool get musicActive => music != null;

  /// The music surface actually on screen. A Core-pushed mode (recipe/weather/place)
  /// always wins; then an explicitly opened view (tap/voice); otherwise, with the
  /// [MusicDisplayStyle.fullscreen] preference, the now-playing view — but only while
  /// someone is there (present or mid-turn), so an unattended display still falls to the
  /// dim away clock rather than holding the backlight up all night.
  MusicScreen get effectiveMusicScreen {
    if (!musicActive ||
        recipeActive ||
        weatherActive ||
        placeActive ||
        cameraActive) {
      return MusicScreen.hidden;
    }
    if (musicScreen != MusicScreen.hidden) return musicScreen;
    if (musicDisplayStyle == MusicDisplayStyle.fullscreen &&
        (effectivePresent || displayActive)) {
      return MusicScreen.nowPlaying;
    }
    return MusicScreen.hidden;
  }

  /// Whether a full-screen music view (now-playing or up-next) is currently on screen.
  bool get musicScreenActive => effectiveMusicScreen != MusicScreen.hidden;

  /// Whether a turn is currently in flight (anything but idle/error).
  bool get turnActive => phase != TurnPhase.idle && phase != TurnPhase.error;

  /// Whether the device is actively listening to the user: from the wake word (or a
  /// follow-up listen opening the mic), across the brief connect/stream hop, until
  /// end-of-speech moves the turn on to processing/thinking. Drives the live
  /// listening ring ([ListeningOverlay]), which reacts to [micLevel]. Excludes the
  /// TTS `speaking` phase (that's the assistant talking, not the user).
  bool get listening =>
      phase == TurnPhase.listening || phase == TurnPhase.connecting;

  /// Whether the conversation panel (transcript + reply text) should stay on
  /// screen: while a turn is active, and afterwards for as long as the reply audio
  /// is still playing. The audio outlives the turn, so this is the visibility gate
  /// the UI keys off — not [turnActive] alone.
  bool get displayActive => turnActive || audioPlaying;

  /// Whether the screen is "awake" — i.e. showing its full presentation rather than
  /// the dimmed away-mode clock. True when the camera reports someone present, while
  /// a voice turn / reply audio is on screen, or while any full-screen mode
  /// (recipe / weather / place) is up. This is the exact inverse of the UI's
  /// `offMode`, and the **single source of truth** for both blanking the slideshow
  /// ([AmbientScreen]) and driving the backlight ([ScreenBrightnessController]): a
  /// wake from *any* of these causes must brighten the screen to full, not just a
  /// camera-presence flip. (Historically the backlight tracked only [userPresent],
  /// so a turn/mode that woke the display left the backlight stuck dim.)
  bool get screenAwake =>
      effectivePresent ||
      displayActive ||
      recipeActive ||
      weatherActive ||
      placeActive ||
      cameraActive ||
      (musicActive && musicScreen != MusicScreen.hidden);

  AssistantState copyWith({
    TurnPhase? phase,
    bool? online,
    String? transcript,
    String? reply,
    String? statusMessage,
    String? wakeWord,
    double? micLevel,
    double? wakeScore,
    double? wakeAvgScore,
    double? wakeThreshold,
    double? captureGainDb,
    String? captureDevice,
    int? captureSampleRate,
    int? captureChannels,
    int? detectionSeq,
    double? lastDetectionScore,
    bool? captureReady,
    List<TimerModel>? timers,
    bool? timerSpotlight,
    bool? audioPlaying,
    bool? userPresent,
    bool? roomDark,
    double? ambientLux,
    bool? cameraCovered,
    CoveredCameraMode? coveredMode,
    bool? scheduledNight,
    bool? followUp,
    RecipeData? recipe,
    bool clearRecipe = false,
    WeatherData? weather,
    bool clearWeather = false,
    WeatherData? weatherCurrent,
    PlaceData? place,
    bool clearPlace = false,
    CameraSession? camera,
    bool clearCamera = false,
    MusicData? music,
    bool clearMusic = false,
    MusicScreen? musicScreen,
    MusicDisplayStyle? musicDisplayStyle,
    int? recipeTab,
    int? recipeScrollSeq,
    String? recipeScrollDir,
    int? recipeStep,
    int? recipeStepSeq,
    int? enrollSeq,
    String? enrollResultJson,
  }) {
    return AssistantState(
      phase: phase ?? this.phase,
      online: online ?? this.online,
      transcript: transcript ?? this.transcript,
      reply: reply ?? this.reply,
      statusMessage: statusMessage ?? this.statusMessage,
      wakeWord: wakeWord ?? this.wakeWord,
      micLevel: micLevel ?? this.micLevel,
      wakeScore: wakeScore ?? this.wakeScore,
      wakeAvgScore: wakeAvgScore ?? this.wakeAvgScore,
      wakeThreshold: wakeThreshold ?? this.wakeThreshold,
      captureGainDb: captureGainDb ?? this.captureGainDb,
      captureDevice: captureDevice ?? this.captureDevice,
      captureSampleRate: captureSampleRate ?? this.captureSampleRate,
      captureChannels: captureChannels ?? this.captureChannels,
      detectionSeq: detectionSeq ?? this.detectionSeq,
      lastDetectionScore: lastDetectionScore ?? this.lastDetectionScore,
      captureReady: captureReady ?? this.captureReady,
      timers: timers ?? this.timers,
      timerSpotlight: timerSpotlight ?? this.timerSpotlight,
      audioPlaying: audioPlaying ?? this.audioPlaying,
      userPresent: userPresent ?? this.userPresent,
      roomDark: roomDark ?? this.roomDark,
      ambientLux: ambientLux ?? this.ambientLux,
      cameraCovered: cameraCovered ?? this.cameraCovered,
      coveredMode: coveredMode ?? this.coveredMode,
      scheduledNight: scheduledNight ?? this.scheduledNight,
      followUp: followUp ?? this.followUp,
      recipe: clearRecipe ? null : (recipe ?? this.recipe),
      weather: clearWeather ? null : (weather ?? this.weather),
      weatherCurrent: weatherCurrent ?? this.weatherCurrent,
      place: clearPlace ? null : (place ?? this.place),
      camera: clearCamera ? null : (camera ?? this.camera),
      music: clearMusic ? null : (music ?? this.music),
      musicScreen: musicScreen ?? this.musicScreen,
      musicDisplayStyle: musicDisplayStyle ?? this.musicDisplayStyle,
      recipeTab: recipeTab ?? this.recipeTab,
      recipeScrollSeq: recipeScrollSeq ?? this.recipeScrollSeq,
      recipeScrollDir: recipeScrollDir ?? this.recipeScrollDir,
      recipeStep: recipeStep ?? this.recipeStep,
      recipeStepSeq: recipeStepSeq ?? this.recipeStepSeq,
      enrollSeq: enrollSeq ?? this.enrollSeq,
      enrollResultJson: enrollResultJson ?? this.enrollResultJson,
    );
  }
}

/// Consumes the engine event stream and exposes an [AssistantState] to the UI.
class AssistantController extends ChangeNotifier {
  AssistantController({
    required WakeWordConfig config,
    EngineStreamFactory? startEngine,
    OrchestratorProbe? probeOrchestrator,
    Duration minBackoff = const Duration(seconds: 1),
    Duration maxBackoff = const Duration(seconds: 30),
    Duration offlinePollInterval = const Duration(seconds: 3),
    bool endpointCueEnabled = true,
    Duration endpointSilence = const Duration(milliseconds: 600),
    double endpointRmsThreshold = 0.012,
    DateTime Function()? clock,
    VoidCallback? onUserActivity,
    Duration weatherAutoClose = const Duration(seconds: 60),
    Duration placeAutoClose = const Duration(minutes: 5),
    Duration cameraAutoClose = const Duration(seconds: 60),
    RecipeContextSink? setRecipeContext,
    WeatherContextSink? setWeatherContext,
    PlaceContextSink? setPlaceContext,
    CameraContextSink? setCameraContext,
    CameraStreamFetcher? fetchCameraStream,
    MusicControlSink? musicControl,
    MusicContextSink? setMusicContext,
    FontContextSink? setFontContext,
    void Function(int id)? dismissNativeTimer,
    void Function(String direction)? onFontAdjust,
    double fontScale = 1.0,
  }) : _config = config,
       _onUserActivity = onUserActivity,
       _weatherAutoClose = weatherAutoClose,
       _placeAutoClose = placeAutoClose,
       _cameraAutoClose = cameraAutoClose,
       _setRecipeContext = setRecipeContext,
       _setWeatherContext = setWeatherContext,
       _setPlaceContext = setPlaceContext,
       _setCameraContext = setCameraContext,
       _fetchCameraStream = fetchCameraStream,
       _musicControl = musicControl,
       _setMusicContext = setMusicContext,
       _setFontContext = setFontContext,
       _dismissNativeTimer = dismissNativeTimer,
       _onFontAdjust = onFontAdjust,
       _fontScale = fontScale,
       // `startWakeWordEngine` takes a named `config:`; adapt it to the positional
       // [EngineStreamFactory] shape (tests inject their own factory).
       _startEngine = startEngine ?? _defaultEngineStream,
       _probe = probeOrchestrator,
       _minBackoff = minBackoff,
       _maxBackoff = maxBackoff,
       _offlinePollInterval = offlinePollInterval,
       _endpointCueEnabled = endpointCueEnabled,
       _endpointSilence = endpointSilence,
       _endpointRmsThreshold = endpointRmsThreshold,
       _clock = clock ?? DateTime.now;

  static Stream<WakeWordEvent> _defaultEngineStream(WakeWordConfig config) =>
      startWakeWordEngine(config: config);

  /// The engine config, reused on every reconnect ([_listen]). Not final: live
  /// tuning ([updateLiveTuning]) folds new values in so a reconnect doesn't revert
  /// them to what the controller was created with.
  WakeWordConfig _config;
  final EngineStreamFactory _startEngine;

  /// The engine config currently in use (reflects live tuning).
  WakeWordConfig get config => _config;

  /// Record live-tuned capture gain + wake-word thresholds (already pushed to the
  /// running engine via `update_diagnostics_tuning`) in the config, so an engine
  /// reconnect re-seeds these values instead of the ones this controller started
  /// with.
  void updateLiveTuning({
    required double gainDb,
    required double threshold,
    required double activeThreshold,
  }) {
    final c = _config;
    _config = WakeWordConfig(
      melspecModelPath: c.melspecModelPath,
      embeddingModelPath: c.embeddingModelPath,
      wakewordModelPath: c.wakewordModelPath,
      modelName: c.modelName,
      threshold: threshold,
      activeThreshold: activeThreshold,
      discoveryTimeoutSecs: c.discoveryTimeoutSecs,
      orchestratorKey: c.orchestratorKey,
      turnTimeoutSecs: c.turnTimeoutSecs,
      smoothingWindow: c.smoothingWindow,
      fireOnPeak: c.fireOnPeak,
      playbackBufferSecs: c.playbackBufferSecs,
      captureGainDb: gainDb,
      useAudiorecord: c.useAudiorecord,
      micSource: c.micSource,
      platformAec: c.platformAec,
      platformAgc: c.platformAgc,
      platformNs: c.platformNs,
      cameraProximity: c.cameraProximity,
      proximityMotionThreshold: c.proximityMotionThreshold,
      proximityReleaseSecs: c.proximityReleaseSecs,
      nightDarkLux: c.nightDarkLux,
    );
  }

  /// Reachability probe used to auto-recover the online status while idle, instead
  /// of waiting for the next wake word. Null disables the poll (e.g. in tests).
  final OrchestratorProbe? _probe;
  final Duration _minBackoff;
  final Duration _maxBackoff;
  final Duration _offlinePollInterval;

  /// Local end-of-speech cue: flip to [TurnPhase.processing] once the mic level has
  /// stayed below [_endpointRmsThreshold] for [_endpointSilence] after speech, so
  /// the UI reacts the instant the user stops rather than waiting on the Mac.
  final bool _endpointCueEnabled;
  final Duration _endpointSilence;
  final double _endpointRmsThreshold;
  final DateTime Function() _clock;

  /// Notifies the engine that the user is actively engaged — by voice or, via
  /// [noteUserActivity] from the UI layer, by touching the screen — so the
  /// screen-dim countdown resets (and a dimmed screen brightens). Production wires
  /// this to the native `noteUserActivity`; null (the default) disables it, so
  /// unit/widget tests run with no native library loaded.
  final VoidCallback? _onUserActivity;

  /// How long the full-screen weather view stays up before it auto-dismisses back to
  /// the idle screen (the ambient clock chip is unaffected). Reset each time a new
  /// forecast is shown; cancelled on an early voice/touch dismiss.
  final Duration _weatherAutoClose;
  Timer? _weatherAutoCloseTimer;

  /// How long the full-screen place card stays up before it auto-dismisses back to the
  /// idle screen. Reset each time a new place is shown; cancelled on an early voice/touch
  /// dismiss or when another full-screen widget replaces it.
  final Duration _placeAutoClose;
  Timer? _placeAutoCloseTimer;

  /// How long the live camera view stays up before it closes on its own (default 60 s),
  /// so a forgotten view doesn't hold the screen awake and the NVR stream open. Reset by
  /// each new show; cancelled on an early voice/touch dismiss or when another
  /// full-screen widget replaces it.
  final Duration _cameraAutoClose;
  Timer? _cameraAutoCloseTimer;

  /// Monotonic open counter for [CameraSession.seq] (drops stale stream fetches).
  int _cameraSeq = 0;

  /// Sink for pushing recipe-screen context to the native engine (see
  /// [RecipeContextSink]); null disables it (tests with no native library).
  final RecipeContextSink? _setRecipeContext;

  /// Sink for pushing weather-screen context to the native engine (see
  /// [WeatherContextSink]); null disables it (tests with no native library).
  final WeatherContextSink? _setWeatherContext;

  /// Sink for pushing place-card context to the native engine (see [PlaceContextSink]);
  /// null disables it (tests with no native library).
  final PlaceContextSink? _setPlaceContext;

  /// Sink for pushing camera-view context to the native engine (see
  /// [CameraContextSink]); null disables it (tests with no native library).
  final CameraContextSink? _setCameraContext;

  /// Fetches a camera's stream URL from the Core (see [CameraStreamFetcher]); null
  /// skips the fetch (tests that don't exercise it).
  final CameraStreamFetcher? _fetchCameraStream;

  /// Sink for sending music transport/volume commands to the Core (see
  /// [MusicControlSink]); null disables it (tests with no native library), so taps are
  /// no-ops.
  final MusicControlSink? _musicControl;

  /// Sink for pushing music-screen display context to the native engine (see
  /// [MusicContextSink]); null disables it (tests with no native library).
  final MusicContextSink? _setMusicContext;

  /// Sink for pushing font-scaling context to the native engine (see [FontContextSink]);
  /// null disables it (tests with no native library).
  final FontContextSink? _setFontContext;

  /// Turns a timer off in the native engine (stopping a ringing alarm's repeats).
  /// Production wires the native `dismissTimer`; null (tests) only clears the UI.
  final void Function(int id)? _dismissNativeTimer;

  /// Called when a voice "increase font" / "decrease font" command arrives, with the
  /// direction `'increase'` / `'decrease'`. The app shell applies + persists the new
  /// global [AppSettings.fontScale] and calls [updateFontScale] back. Null disables it.
  final void Function(String direction)? _onFontAdjust;

  /// The current global font scale, mirrored from [AppSettings.fontScale] so the pushed
  /// font context reports the live value and whether it is at the min/max bound. Updated
  /// by the app shell via [updateFontScale] after each voice adjustment.
  double _fontScale;

  /// The active recipe pane's latest scroll position, tracked so recipe context
  /// pushes carry it. `_recipeAtTop` starts true (a freshly opened tab is at the top);
  /// the [RecipeView] corrects both once it measures the pane.
  bool _recipeAtTop = true;
  bool _recipeAtBottom = false;

  /// True once we've seen speech-level audio in the current turn (so trailing
  /// silence means "done speaking" rather than "hasn't started yet").
  bool _speechSeen = false;

  /// When the mic last carried speech-level audio in the current turn.
  DateTime? _lastVoiceAt;

  /// While in [TurnPhase.processing], when the current unbroken run of speech-level
  /// audio began (null when the mic is quiet). The local cue is only a guess — the
  /// Core's VAD is authoritative and keeps listening through a pause the cue took as
  /// end-of-speech — so speech that resumes before the transcript arrives reverts
  /// the phase to [TurnPhase.listening]. See [_endpointResumeMin].
  DateTime? _resumeSince;

  /// How long speech-level audio must persist in [TurnPhase.processing] before the
  /// cue reverts to [TurnPhase.listening], so a single noise blip (a cough, a door)
  /// while the Mac transcribes doesn't flicker the listening ring back on.
  static const Duration _endpointResumeMin = Duration(milliseconds: 150);

  AssistantState _state = const AssistantState();
  AssistantState get state => _state;

  StreamSubscription<WakeWordEvent>? _sub;
  Timer? _reconnectTimer;
  Timer? _offlinePollTimer;
  bool _probing = false;
  Duration _backoff = const Duration(seconds: 1);
  bool _disposed = false;

  /// Begin (or restart) consuming the engine stream. Safe to call once at startup.
  void start() {
    _reconnectTimer?.cancel();
    _sub?.cancel();
    _backoff = _minBackoff;
    _listen();
    // Seed the native font context before the first turn so its `audio-start` already
    // carries the current global scale (and `scalable`). Pushed here — well ahead of any
    // turn — so the value is read, not raced, when the turn stamps `screen.font`.
    _pushFontContext();
    // Start probing immediately if we're offline (don't wait for the first event).
    _syncOfflinePoll();
  }

  void _listen() {
    if (_disposed) return;
    try {
      _sub = _startEngine(_config).listen(
        _onEvent,
        onError: (Object e, StackTrace _) =>
            _scheduleReconnect('engine error: $e'),
        onDone: () => _scheduleReconnect('engine stream ended'),
        cancelOnError: true,
      );
    } catch (e) {
      _scheduleReconnect('failed to start engine: $e');
    }
  }

  /// Restart capture after a backoff, doubling it up to [_maxBackoff]. This is the
  /// engine-side half of the plan's "auto-reconnect with backoff"; the per-turn
  /// mDNS re-discovery is the network-side half (in Rust).
  void _scheduleReconnect(String reason) {
    if (_disposed) return;
    _sub?.cancel();
    _sub = null;
    _emit(
      _state.copyWith(
        phase: TurnPhase.error,
        online: false,
        statusMessage: '$reason — reconnecting…',
        audioPlaying: false,
      ),
    );
    _reconnectTimer?.cancel();
    _reconnectTimer = Timer(_backoff, () {
      _backoff = _nextBackoff(_backoff);
      _listen();
    });
  }

  Duration _nextBackoff(Duration current) {
    final next = current * 2;
    return next > _maxBackoff ? _maxBackoff : next;
  }

  /// Report user activity that should keep the screen awake — a screen touch from
  /// the UI layer, or (internally) a voice-turn transition. Resets the native
  /// screen-dim countdown and brightens the screen if it had already dimmed. Safe to
  /// call any time; a no-op when no activity sink is wired (tests).
  void noteUserActivity() => _onUserActivity?.call();

  void _onEvent(WakeWordEvent e) {
    // A healthy event stream resets the backoff.
    _backoff = _minBackoff;

    // A voice turn counts as user activity: reset the screen-dim countdown at each
    // stage of the turn (wake, mic streaming, transcript, TTS, follow-up listen) so
    // a hands-free conversation keeps the screen bright even if the user is too
    // still for the camera to register motion. Excludes the high-frequency `level`
    // (mic RMS) and camera `presence` events, which aren't user-initiated turns.
    switch (e.kind) {
      case WakeWordEventKind.detected:
      case WakeWordEventKind.streaming:
      case WakeWordEventKind.transcript:
      case WakeWordEventKind.speaking:
      case WakeWordEventKind.listeningFollowup:
        noteUserActivity();
      default:
        break;
    }

    switch (e.kind) {
      case WakeWordEventKind.started:
        _emit(
          _state.copyWith(
            captureReady: true,
            statusMessage: 'Listening on ${e.device}',
            captureDevice: e.device,
            captureSampleRate: e.deviceSampleRate,
            captureChannels: e.channels,
          ),
        );
      case WakeWordEventKind.status:
        _emit(_state.copyWith(statusMessage: e.message));
      case WakeWordEventKind.level:
        _onLevel(e);
      case WakeWordEventKind.detected:
        // A wake word starts a fresh turn: clear the previous exchange and reset the
        // local end-of-speech tracker. Also clear any lingering audio-playing flag
        // from a previous reply (a barge-in cuts its playback) so a late drain event
        // can't wipe this new turn's text.
        _speechSeen = false;
        _lastVoiceAt = _clock();
        _resumeSince = null;
        _emit(
          _state.copyWith(
            phase: TurnPhase.listening,
            wakeWord: e.model,
            transcript: '',
            reply: '',
            audioPlaying: false,
            followUp: false,
            timerSpotlight: false,
            // Record the fire for the diagnostics screen's flash + history.
            detectionSeq: _state.detectionSeq + 1,
            lastDetectionScore: e.score,
            wakeScore: e.score,
          ),
        );
      case WakeWordEventKind.connecting:
        _emit(
          _state.copyWith(
            phase: TurnPhase.connecting,
            online: true,
            statusMessage: e.message,
          ),
        );
      case WakeWordEventKind.streaming:
        _emit(_state.copyWith(phase: TurnPhase.listening, online: true));
      case WakeWordEventKind.transcript:
        _emit(
          _state.copyWith(
            phase: TurnPhase.thinking,
            online: true,
            transcript: e.transcript,
            // The user said something new: give the conversation panel back.
            timerSpotlight: false,
          ),
        );
      case WakeWordEventKind.replyToken:
        _emit(
          _state.copyWith(
            phase: TurnPhase.thinking,
            online: true,
            reply: _state.reply + e.reply,
          ),
        );
      case WakeWordEventKind.speaking:
        // TTS playback started: mark audio as playing so the reply text stays on
        // screen through the whole utterance, even after the turn returns to idle.
        _emit(
          _state.copyWith(
            phase: TurnPhase.speaking,
            online: true,
            audioPlaying: true,
          ),
        );
      case WakeWordEventKind.speakingDone:
        _onSpeakingDone();
      case WakeWordEventKind.listeningFollowup:
        // The assistant asked a question and reopened the mic with no wake word.
        // Treat it like the start of a fresh turn (clear the previous exchange, reset
        // the local end-of-speech tracker), but flag it as a follow-up so the UI can
        // show a distinct "listening for your reply" cue. The reply audio has already
        // drained by the time this fires (the engine gates it on drain), so it is safe
        // to clear the text now.
        _speechSeen = false;
        _lastVoiceAt = _clock();
        _resumeSince = null;
        _emit(
          _state.copyWith(
            phase: TurnPhase.listening,
            online: true,
            transcript: '',
            reply: '',
            audioPlaying: false,
            followUp: true,
          ),
        );
      case WakeWordEventKind.disconnected:
        _onDisconnected(e.message);
      case WakeWordEventKind.stopped:
        _scheduleReconnect('capture stopped');
      case WakeWordEventKind.error:
        _scheduleReconnect(e.message);
      case WakeWordEventKind.timerStarted:
        // Anchor a wall-clock deadline so the overlay can count down smoothly with
        // no per-second events from Rust. Replace any existing timer with this id.
        final total = Duration(seconds: e.timerRemainingSecs);
        final deadline = _clock().add(total);
        // A brand-new timer takes the screen right away (see [timerSpotlight]); an
        // adjust re-emits the same id and leaves the current view alone.
        final isNew = !_state.timers.any((t) => t.id == e.timerId);
        _emit(
          _state.copyWith(
            timerSpotlight: isNew ? true : null,
            timers: [
              ..._state.timers.where((t) => t.id != e.timerId),
              TimerModel(
                id: e.timerId,
                label: e.timerLabel,
                deadline: deadline,
                total: total,
              ),
            ],
          ),
        );
      case WakeWordEventKind.timerFinished:
        // Mark it finished (its alarm is sounding); the chip stays until dismissed.
        _emit(
          _state.copyWith(
            timers: _state.timers
                .map((t) => t.id == e.timerId ? t.copyWith(finished: true) : t)
                .toList(),
          ),
        );
      case WakeWordEventKind.timerCancelled:
        _emit(
          _state.copyWith(
            timers: _state.timers.where((t) => t.id != e.timerId).toList(),
          ),
        );
      case WakeWordEventKind.presence:
        // Camera proximity transition (brighten on approach / dim when quiet). The
        // brightness actuation lives in the UI layer, which reads `userPresent`.
        _emit(_state.copyWith(userPresent: e.present));
      case WakeWordEventKind.ambientLight:
        // Camera lux reading: lights off/on (dims the backlight to night levels) plus
        // the live estimate shown in Settings. Not user activity (see above).
        _emit(
          _state.copyWith(
            roomDark: e.dark,
            ambientLux: e.lux,
            cameraCovered: e.covered,
            scheduledNight: _nightWindow.isNight(_clock()),
          ),
        );
      case WakeWordEventKind.showRecipe:
        // The orchestrator pushed a parsed recipe; open recipe mode on the Overview
        // tab. A payload that fails to parse is ignored (recipe stays as it was)
        // rather than crashing. Loading a full-screen widget unloads the previous one
        // (here: the weather screen) so they never stack — the ambient clock chip
        // (weatherCurrent) is not a full-screen widget and stays.
        final recipe = RecipeData.tryParse(e.recipeJson);
        if (recipe != null) {
          _weatherAutoCloseTimer?.cancel();
          _placeAutoCloseTimer?.cancel();
          _cameraAutoCloseTimer?.cancel();
          _recipeAtTop = true;
          _recipeAtBottom = false;
          _emit(
            _state.copyWith(
              recipe: recipe,
              recipeTab: 0,
              recipeStep: 0,
              clearWeather: true,
              clearPlace: true,
              clearCamera: true,
              musicScreen: MusicScreen.hidden,
            ),
          );
          // The display context is a single slot: clear the replaced screens first so
          // the newly opened one's (active) context is the last write and wins.
          _pushPlaceContext();
          _pushCameraContext();
          _pushRecipeContext();
        }
      case WakeWordEventKind.dismissRecipe:
        _clearRecipe();
      case WakeWordEventKind.showWeather:
        // The orchestrator pushed a forecast; open the full-screen weather view and
        // refresh the ambient indicator from the same payload. A payload that fails to
        // parse is ignored rather than crashing.
        final weather = WeatherData.tryParse(e.weatherJson);
        if (weather != null) {
          // Loading a full-screen widget unloads the previous one (here: the recipe
          // screen) so they never stack.
          _placeAutoCloseTimer?.cancel();
          _cameraAutoCloseTimer?.cancel();
          _recipeAtTop = true;
          _recipeAtBottom = false;
          _emit(
            _state.copyWith(
              weather: weather,
              weatherCurrent: weather,
              clearRecipe: true,
              clearPlace: true,
              clearCamera: true,
              recipeTab: 0,
              recipeStep: 0,
              musicScreen: MusicScreen.hidden,
            ),
          );
          _scheduleWeatherAutoClose();
          // Clear the replaced screens' context first; the active one is written last.
          _pushPlaceContext();
          _pushCameraContext();
          _pushWeatherContext();
        }
      case WakeWordEventKind.weatherCurrent:
        // An ambient refresh (from the persistent channel or riding a show): update the
        // small clock indicator only; never opens or closes the full screen.
        final weather = WeatherData.tryParse(e.weatherJson);
        if (weather != null) {
          _emit(_state.copyWith(weatherCurrent: weather));
        }
      case WakeWordEventKind.dismissWeather:
        _weatherAutoCloseTimer?.cancel();
        _emit(_state.copyWith(clearWeather: true));
        _pushWeatherContext();
      case WakeWordEventKind.showPlace:
        // The orchestrator pushed a place; open the full-screen place card. A payload
        // that fails to parse is ignored rather than crashing. Loading a full-screen
        // widget unloads the previous one (recipe / weather) so they never stack.
        final place = PlaceData.tryParse(e.placeJson);
        if (place != null) {
          _weatherAutoCloseTimer?.cancel();
          _cameraAutoCloseTimer?.cancel();
          _recipeAtTop = true;
          _recipeAtBottom = false;
          _emit(
            _state.copyWith(
              place: place,
              clearRecipe: true,
              clearWeather: true,
              clearCamera: true,
              recipeTab: 0,
              recipeStep: 0,
              musicScreen: MusicScreen.hidden,
            ),
          );
          _schedulePlaceAutoClose();
          _pushRecipeContext();
          _pushWeatherContext();
          _pushCameraContext();
          _pushPlaceContext();
        }
      case WakeWordEventKind.dismissPlace:
        _placeAutoCloseTimer?.cancel();
        _emit(_state.copyWith(clearPlace: true));
        _pushPlaceContext();
      case WakeWordEventKind.showCamera:
        // The orchestrator asked for the live camera view. The camera's display name
        // rides the generic [WakeWordEvent.recipeAction] carrier; the stream URL is
        // fetched from the Core here, never carried on the frame.
        _openCamera(e.recipeAction.trim());
      case WakeWordEventKind.dismissCamera:
        _closeCamera();
      case WakeWordEventKind.recipeNavigate:
        // Voice tab switch ("show the ingredients" / "go to the steps").
        if (_state.recipe != null) {
          _setRecipeTab(_tabIndexForTarget(e.recipeAction));
        }
      case WakeWordEventKind.recipeScroll:
        // Voice scroll ("scroll down" / "back to the top"). Bump the seq so the view
        // scrolls the active pane even when the direction repeats; it reports the new
        // position back via [reportRecipeScrollPosition], which refreshes the context.
        if (_state.recipe != null) {
          _emit(
            _state.copyWith(
              recipeScrollSeq: _state.recipeScrollSeq + 1,
              recipeScrollDir: e.recipeAction,
            ),
          );
        }
      case WakeWordEventKind.recipeStep:
        // Voice "go to step 4" / "next step": switch to the Steps tab, highlight the
        // step, and bump the seq so the view scrolls it into view. An unparseable or
        // out-of-range step is ignored.
        final step = int.tryParse(e.recipeAction.trim());
        final r = _state.recipe;
        if (r != null && step != null && step >= 1 && step <= r.steps.length) {
          if (_state.recipeTab != 2) {
            _recipeAtTop = true;
            _recipeAtBottom = false;
          }
          _emit(
            _state.copyWith(
              recipeTab: 2,
              recipeStep: step,
              recipeStepSeq: _state.recipeStepSeq + 1,
            ),
          );
          _pushRecipeContext();
        }
      case WakeWordEventKind.musicScreen:
        // Voice `music_screen` command ("show me what's playing" / "what's up next" /
        // "hide the music"). The target screen rides the generic
        // [WakeWordEvent.recipeAction] string (same carrier as font adjust). Routed
        // through the same methods a touch uses.
        _applyMusicScreen(e.recipeAction);
      case WakeWordEventKind.fontAdjust:
        // Voice "increase font" / "decrease font". The direction rides the generic
        // [WakeWordEvent.recipeAction] string (same carrier as recipe scroll). The app
        // shell applies + persists the new global scale and calls [updateFontScale] back.
        _onFontAdjust?.call(e.recipeAction);
      case WakeWordEventKind.enrollResult:
        // A guided voice-enrollment capture's result (sample accepted/rejected, or the
        // verify outcome). The enroll screen owns the flow; bump the counter + stash the
        // raw JSON for it to parse. Does not touch the turn state machine.
        _emit(
          _state.copyWith(
            enrollSeq: _state.enrollSeq + 1,
            enrollResultJson: e.enrollJson,
          ),
        );
    }
  }

  /// Map a voice tab target (`"overview"` / `"ingredients"` / `"steps"`) to its tab
  /// index; an unknown target falls back to Overview.
  int _tabIndexForTarget(String target) {
    switch (target) {
      case 'ingredients':
        return 1;
      case 'steps':
        return 2;
      default:
        return 0;
    }
  }

  /// (Re)arm the full-screen weather auto-dismiss. A new forecast restarts the clock;
  /// firing clears only the full screen (the ambient chip stays).
  void _scheduleWeatherAutoClose() {
    _weatherAutoCloseTimer?.cancel();
    if (_weatherAutoClose <= Duration.zero) return;
    _weatherAutoCloseTimer = Timer(_weatherAutoClose, () {
      if (_state.weather != null) {
        _emit(_state.copyWith(clearWeather: true));
        _pushWeatherContext();
      }
    });
  }

  /// (Re)arm the full-screen place-card auto-dismiss (default 5 minutes). A new place
  /// restarts the clock; firing closes the card and clears its display context.
  void _schedulePlaceAutoClose() {
    _placeAutoCloseTimer?.cancel();
    if (_placeAutoClose <= Duration.zero) return;
    _placeAutoCloseTimer = Timer(_placeAutoClose, () {
      if (_state.place != null) {
        _emit(_state.copyWith(clearPlace: true));
        _pushPlaceContext();
      }
    });
  }

  /// Push the current weather-screen state (active + what it shows) down to the native
  /// engine so the next voice turn carries it to the orchestrator. A no-op when no sink
  /// is wired (tests). Only the full screen counts as display context — the ambient chip
  /// does not.
  void _pushWeatherContext() {
    final sink = _setWeatherContext;
    if (sink == null) return;
    final w = _state.weather;
    if (w == null) {
      sink(active: false, location: '', units: '', temp: 0, description: '');
      return;
    }
    sink(
      active: true,
      location: w.locationLabel,
      units: w.units,
      temp: w.current.temp,
      description: w.current.description,
    );
  }

  /// Dismiss recipe mode from the UI (the user taps the close control). Voice
  /// dismissal ("done cooking") arrives instead as a `dismissRecipe` event.
  void dismissRecipe() {
    if (_state.recipe != null) {
      _clearRecipe();
    }
  }

  /// Dismiss the full-screen weather view from the UI (the user taps the close
  /// control). Voice dismissal arrives instead as a `dismissWeather` event. The ambient
  /// indicator ([weatherCurrent]) is left untouched — only the full screen closes.
  void dismissWeather() {
    _weatherAutoCloseTimer?.cancel();
    if (_state.weather != null) {
      _emit(_state.copyWith(clearWeather: true));
      _pushWeatherContext();
    }
  }

  /// Push the current place-card state (active + which place) down to the native engine
  /// so the next voice turn carries it to the orchestrator. A no-op when no sink is wired
  /// (tests).
  void _pushPlaceContext() {
    final sink = _setPlaceContext;
    if (sink == null) return;
    final p = _state.place;
    if (p == null) {
      sink(active: false, name: '', address: '');
      return;
    }
    sink(active: true, name: p.name, address: p.address);
  }

  /// Dismiss the full-screen place card from the UI (the user taps the close control).
  /// Voice dismissal arrives instead as a `dismissPlace` event.
  void dismissPlace() {
    _placeAutoCloseTimer?.cancel();
    if (_state.place != null) {
      _emit(_state.copyWith(clearPlace: true));
      _pushPlaceContext();
    }
  }

  /// Open (or retarget) the full-screen live camera view on [name]. Loading a
  /// full-screen widget unloads the previous one (recipe / weather / place) so they
  /// never stack. A newer show while one is open replaces it (new name, fresh fetch,
  /// restarted auto-close).
  void _openCamera(String name) {
    _weatherAutoCloseTimer?.cancel();
    _placeAutoCloseTimer?.cancel();
    _recipeAtTop = true;
    _recipeAtBottom = false;
    final seq = ++_cameraSeq;
    _emit(
      _state.copyWith(
        camera: CameraSession(name: name, seq: seq),
        clearRecipe: true,
        clearWeather: true,
        clearPlace: true,
        recipeTab: 0,
        recipeStep: 0,
        musicScreen: MusicScreen.hidden,
      ),
    );
    _scheduleCameraAutoClose();
    // Clear the replaced screens' context first; the camera's is written last.
    _pushRecipeContext();
    _pushWeatherContext();
    _pushPlaceContext();
    _pushCameraContext();
    unawaited(_loadCameraStream(seq, name));
  }

  /// Fetch the stream URL for the camera session [seq] and fold the result in. A reply
  /// for a session that's no longer current (closed, or replaced by a newer show) is
  /// dropped. Never logs the URL.
  Future<void> _loadCameraStream(int seq, String name) async {
    final fetch = _fetchCameraStream;
    if (fetch == null) return;
    CameraStream? result;
    Object? failure;
    try {
      result = await fetch(name);
    } catch (e) {
      failure = e;
    }
    final current = _state.camera;
    if (_disposed || current == null || current.seq != seq) return;
    if (result != null && result.ok && result.url.isNotEmpty) {
      final resolved = result.camera.trim();
      final renamed = resolved.isNotEmpty && resolved != current.name;
      _emit(
        _state.copyWith(
          camera: current.copyWith(
            name: renamed ? resolved : null,
            url: result.url,
          ),
        ),
      );
      if (renamed) _pushCameraContext();
      return;
    }
    // Only the failure reason is logged — never the URL.
    debugPrint(
      'camera stream unavailable: ${failure ?? (result?.message ?? '')}',
    );
    _emit(
      _state.copyWith(
        camera: current.copyWith(
          error: failure != null ? '' : (result?.message ?? ''),
        ),
      ),
    );
  }

  /// (Re)arm the live camera view's auto-close (default 60 s). A new show restarts the
  /// clock; firing closes the view and clears its display context.
  void _scheduleCameraAutoClose() {
    _cameraAutoCloseTimer?.cancel();
    if (_cameraAutoClose <= Duration.zero) return;
    _cameraAutoCloseTimer = Timer(_cameraAutoClose, _closeCamera);
  }

  /// Close the live camera view (voice dismiss, touch, or auto-close). A no-op when it
  /// isn't open. Dropping the session also drops the in-memory stream URL.
  void _closeCamera() {
    _cameraAutoCloseTimer?.cancel();
    if (_state.camera != null) {
      _emit(_state.copyWith(clearCamera: true));
      _pushCameraContext();
    }
  }

  /// Push the current camera-view state (active + which camera) down to the native
  /// engine so the next voice turn carries it to the orchestrator. Never the URL. A
  /// no-op when no sink is wired (tests).
  void _pushCameraContext() {
    final sink = _setCameraContext;
    if (sink == null) return;
    final c = _state.camera;
    sink(active: c != null, name: c?.name ?? '');
  }

  /// Dismiss the live camera view from the UI (the user taps ✕). Voice dismissal
  /// arrives instead as a `dismissCamera` event.
  void dismissCamera() => _closeCamera();

  /// Fold a music now-playing push (from the persistent music channel) into state. An
  /// empty/blank payload is a `dismiss` (nothing playing) → clear music and hide any
  /// full music screen; otherwise parse the snapshot and keep whatever music screen the
  /// user has open. A malformed non-empty payload is ignored (keeps the last snapshot)
  /// rather than crashing. Called by the app shell's music-channel subscription.
  ///
  /// The push also carries this display's [MusicDisplayStyle] (`display_style`, chosen
  /// per display by the Core); a change made elsewhere (a new play request, a config-page
  /// pin, voice) is adopted here — except a stale value racing a change this display just
  /// made (see [_pendingStyle]).
  void applyMusicPush(String nowPlayingJson) {
    if (nowPlayingJson.trim().isEmpty) {
      if (_state.music != null) {
        _emit(
          _state.copyWith(clearMusic: true, musicScreen: MusicScreen.hidden),
        );
      }
      return;
    }
    final music = MusicData.tryParse(nowPlayingJson);
    if (music != null) {
      // Radio has no queue: a station starting while the up-next list is open falls
      // back to the now-playing view.
      var screen = music.isRadio && _state.musicScreen == MusicScreen.nextUp
          ? MusicScreen.nowPlaying
          : _state.musicScreen;
      var style = _state.musicDisplayStyle;
      final pushed = _acceptPushedStyle(music.displayStyle);
      if (pushed != null && pushed != style) {
        style = pushed;
        // A style switch resets any explicitly opened view, so the new preference is
        // what shows (full screen ⇒ now-playing, compact ⇒ the bar).
        screen = MusicScreen.hidden;
      }
      _emit(
        _state.copyWith(
          music: music,
          musicScreen: screen,
          musicDisplayStyle: style,
        ),
      );
    }
  }

  /// A style this display just set locally (button / voice) and when, so a push stamped
  /// *before* the Core applied it can't flip the screen back for a moment. Cleared once
  /// a push agrees, or after [_pendingStyleWindow].
  MusicDisplayStyle? _pendingStyle;
  DateTime? _pendingStyleAt;
  static const _pendingStyleWindow = Duration(seconds: 8);

  /// The pushed style to adopt, or `null` to keep the current one (none pushed, or a
  /// stale value racing a fresh local change).
  MusicDisplayStyle? _acceptPushedStyle(MusicDisplayStyle? pushed) {
    if (pushed == null) return null;
    final pending = _pendingStyle;
    if (pending != null) {
      final fresh =
          DateTime.now().difference(_pendingStyleAt!) < _pendingStyleWindow;
      if (pushed == pending || !fresh) {
        _pendingStyle = null;
        _pendingStyleAt = null;
      } else {
        return null;
      }
    }
    return pushed;
  }

  /// Switch this display's music widget at once and (when [persist]) tell the Core, which
  /// records it as this display's choice for the current music (other displays are not
  /// affected). `persist` is false when the change came *from* the Core (the voice tool
  /// already recorded it). Any explicitly opened view is reset so the new style is what
  /// shows.
  void setMusicDisplayStyle(MusicDisplayStyle style, {bool persist = true}) {
    final changed = style != _state.musicDisplayStyle;
    _pendingStyle = style;
    _pendingStyleAt = DateTime.now();
    _emit(
      _state.copyWith(
        musicDisplayStyle: style,
        musicScreen: MusicScreen.hidden,
      ),
    );
    if (persist && changed) {
      _musicControl?.call(action: 'display_${style.name}');
    }
  }

  /// The compact bar's expand button: show the current music full screen on this display.
  void expandMusic() => setMusicDisplayStyle(MusicDisplayStyle.fullscreen);

  /// Open the full-screen now-playing view (the user tapped the compact control overlay,
  /// or said "show me what's playing"). No-op when nothing is playing.
  void openNowPlaying() {
    if (_state.musicActive && _state.musicScreen != MusicScreen.nowPlaying) {
      _emit(_state.copyWith(musicScreen: MusicScreen.nowPlaying));
    }
  }

  /// Show the full-screen up-next queue (from the now-playing view, or "what's up next").
  /// No-op when nothing is playing. Live radio has no queue, so it opens now-playing.
  void showMusicQueue() {
    if (_state.music?.isRadio ?? false) return openNowPlaying();
    if (_state.musicActive && _state.musicScreen != MusicScreen.nextUp) {
      _emit(_state.copyWith(musicScreen: MusicScreen.nextUp));
    }
  }

  /// Return from the up-next list to the now-playing view.
  void backToNowPlaying() => openNowPlaying();

  /// Close any full music screen and return to the compact control overlay (music keeps
  /// playing — this never stops playback). With this display's style full screen this is
  /// the minimize control: it switches this display to compact, since otherwise the full
  /// screen would simply come straight back.
  void closeMusicScreen() {
    if (_state.musicActive &&
        _state.musicDisplayStyle == MusicDisplayStyle.fullscreen) {
      setMusicDisplayStyle(MusicDisplayStyle.compact);
    } else if (_state.musicScreen != MusicScreen.hidden) {
      _emit(_state.copyWith(musicScreen: MusicScreen.hidden));
    }
  }

  /// Apply a voice `music_screen` command (from the engine stream) by routing it through
  /// the same methods a touch uses. `screen` ∈ `now_playing` / `up_next` / `hidden`, or a
  /// style switch `fullscreen` / `compact` (already recorded for this display by the Core).
  void _applyMusicScreen(String screen) {
    final style = MusicDisplayStyle.tryParse(screen);
    if (style != null) {
      setMusicDisplayStyle(style, persist: false);
      return;
    }
    switch (screen) {
      case 'now_playing':
        openNowPlaying();
      case 'up_next':
        showMusicQueue();
      default:
        closeMusicScreen();
    }
  }

  /// The last music context pushed to the engine (see [_syncMusicContext]), so an
  /// unchanged screen isn't re-sent on every state change.
  String _musicContextKey = 'hidden';

  /// Push the current music-screen context to the native engine (see [MusicContextSink])
  /// whenever what's on screen changes — called from [_emit], so every path (tap, voice,
  /// push, style switch, presence, a Core mode opening/closing) stays in sync. Only a
  /// *full* music screen is reported as context — the compact overlay is a background
  /// affordance, so it never claims the display context from a recipe/weather/place
  /// screen. A no-op when no sink is wired (tests).
  void _syncMusicContext() {
    final sink = _setMusicContext;
    if (sink == null) return;
    final m = _state.music;
    final screen = _state.effectiveMusicScreen;
    final key = (m == null || screen == MusicScreen.hidden)
        ? 'hidden'
        : '${screen.name}|${m.playing}|${m.trackTitle}|${m.artist}';
    if (key == _musicContextKey) return;
    _musicContextKey = key;
    if (key == 'hidden') {
      sink(
        active: false,
        playing: false,
        title: '',
        artist: '',
        screen: 'hidden',
      );
      return;
    }
    sink(
      active: true,
      playing: m!.playing,
      title: m.trackTitle,
      artist: m.artist,
      screen: screen == MusicScreen.nextUp ? 'up_next' : 'now_playing',
    );
  }

  /// Toggle play/pause from the on-screen controls. The device knows the current state
  /// from the last push, so it sends the explicit `pause`/`resume` the Core maps 1:1.
  void playPauseMusic() {
    final playing = _state.music?.playing ?? false;
    _musicControl?.call(action: playing ? 'pause' : 'resume');
  }

  /// Skip to the next track.
  void nextTrack() => _musicControl?.call(action: 'next');

  /// Go back to the previous track.
  void previousTrack() => _musicControl?.call(action: 'previous');

  /// Set the music volume (0–100 percent).
  void setMusicVolume(int percent) =>
      _musicControl?.call(action: 'volume', value: percent.clamp(0, 100));

  /// Mirror the latest global font scale (from [AppSettings.fontScale]) after the app
  /// shell applies a voice adjustment, and re-push the font context so the next turn
  /// reports the new value (and whether it is now at the min/max bound).
  /// Live-apply the "when the camera is covered" setting: [mode] and the daily night
  /// [window]. In [CoveredCameraMode.schedule] a once-a-minute tick keeps
  /// [AssistantState.scheduledNight] current so the screen dims/wakes at the window's
  /// edges even with no engine events. Safe to call any time (no engine restart).
  void updateCoveredCamera(CoveredCameraMode mode, NightWindow window) {
    _nightWindow = window;
    _nightTick?.cancel();
    _nightTick = null;
    if (mode == CoveredCameraMode.schedule) {
      _nightTick = Timer.periodic(const Duration(minutes: 1), (_) {
        final night = _nightWindow.isNight(_clock());
        if (night != _state.scheduledNight) {
          _emit(_state.copyWith(scheduledNight: night));
        }
      });
    }
    _emit(
      _state.copyWith(
        coveredMode: mode,
        scheduledNight: window.isNight(_clock()),
      ),
    );
  }

  NightWindow _nightWindow = const NightWindow();
  Timer? _nightTick;

  void updateFontScale(double scale) {
    _fontScale = scale;
    _pushFontContext();
  }

  /// Push the current font-scaling context down to the native engine so the next voice
  /// turn's `audio-start` carries it to the orchestrator. `scalable` is true whenever a
  /// text surface is on screen — which on this always-on display is always the case (the
  /// conversation panel and cards during a turn, the idle clock otherwise, the large away
  /// clock in off mode), per the product decision to allow resizing whenever text is
  /// visible. A no-op when no sink is wired (tests).
  void _pushFontContext() {
    final sink = _setFontContext;
    if (sink == null) return;
    const eps = 1e-6;
    sink(
      scalable: true,
      scale: _fontScale,
      atMin: _fontScale <= kFontScaleMin + eps,
      atMax: _fontScale >= kFontScaleMax - eps,
    );
  }

  /// Fold an ambient weather push (from the weather channel) into the indicator state.
  /// Called by the app shell's weather-channel subscription. Never opens the full
  /// screen — that's `showWeather` on the engine stream.
  void applyWeatherPush(String reportJson) {
    final weather = WeatherData.tryParse(reportJson);
    if (weather != null) {
      _emit(_state.copyWith(weatherCurrent: weather));
    }
  }

  /// Switch the recipe tab from the UI (a tap of the bottom tab bar). Voice tab
  /// switches arrive instead as a `recipeNavigate` event; both route here.
  void setRecipeTab(int index) {
    if (_state.recipe != null) {
      _setRecipeTab(index);
    }
  }

  /// Report the active recipe pane's scroll position from the [RecipeView], so the
  /// next voice turn's context tells the orchestrator whether a scroll would do
  /// anything. Only pushes context when the position actually changed.
  void reportRecipeScrollPosition(bool atTop, bool atBottom) {
    if (_state.recipe == null) return;
    if (atTop == _recipeAtTop && atBottom == _recipeAtBottom) return;
    _recipeAtTop = atTop;
    _recipeAtBottom = atBottom;
    _pushRecipeContext();
  }

  /// Highlight a recipe step from the UI (a tap on it in the Steps tab), 1-based. The
  /// pushed context then tells the orchestrator which step the user is on, so "next
  /// step" continues from there. Tapping the highlighted step again clears it.
  void selectRecipeStep(int step) {
    final r = _state.recipe;
    if (r == null || step < 1 || step > r.steps.length) return;
    _emit(_state.copyWith(recipeStep: step == _state.recipeStep ? 0 : step));
    _pushRecipeContext();
  }

  void _setRecipeTab(int index) {
    final clamped = index.clamp(0, 2);
    if (clamped == _state.recipeTab) return;
    // A freshly shown tab starts at the top until the view measures it.
    _recipeAtTop = true;
    _recipeAtBottom = false;
    _emit(_state.copyWith(recipeTab: clamped));
    _pushRecipeContext();
  }

  void _clearRecipe() {
    _emit(_state.copyWith(clearRecipe: true, recipeTab: 0, recipeStep: 0));
    _pushRecipeContext();
  }

  /// Push the current recipe-screen state (active/tab/scroll/counts/highlighted step
  /// plus the ingredient and step lines) down to the
  /// native engine so the next voice turn carries it to the orchestrator. A no-op
  /// when no sink is wired (tests).
  void _pushRecipeContext() {
    final sink = _setRecipeContext;
    if (sink == null) return;
    final r = _state.recipe;
    if (r == null) {
      sink(
        active: false,
        title: '',
        tab: 'overview',
        atTop: true,
        atBottom: false,
        ingredientCount: 0,
        stepCount: 0,
        highlightedStep: 0,
        ingredients: const [],
        steps: const [],
      );
      return;
    }
    const tabs = ['overview', 'ingredients', 'steps'];
    sink(
      active: true,
      title: r.title,
      tab: tabs[_state.recipeTab.clamp(0, 2)],
      atTop: _recipeAtTop,
      atBottom: _recipeAtBottom,
      ingredientCount: r.ingredients.length,
      stepCount: r.steps.length,
      // The recipe's lines ride the context so the Core can answer "how many tomatoes?"
      // from what is on screen (System-1 lookup, else the System-2 prompt).
      highlightedStep: _state.recipeStep,
      ingredients: r.ingredients,
      steps: r.steps,
    );
  }

  /// Turn a timer off from the UI — the ringing timer's "Cancel timer" button (or a
  /// tap on its chip). Tells the engine to stop the alarm (which otherwise repeats
  /// every 5 s) and clears it from the screen right away; the engine's
  /// `timerCancelled` that follows is then a no-op.
  void dismissTimer(int id) {
    _dismissNativeTimer?.call(id);
    if (_state.timers.any((t) => t.id == id)) {
      _emit(
        _state.copyWith(
          timers: _state.timers.where((t) => t.id != id).toList(),
        ),
      );
    }
  }

  /// Fold a mic-level event into the state, and — while we're actively listening —
  /// run the local end-of-speech detector so the UI flips to [TurnPhase.processing]
  /// the moment the user stops talking, ahead of the Mac's VAD + transcript.
  void _onLevel(WakeWordEvent e) {
    final rms = e.rms;
    final now = _clock();
    // Fold the enriched level event (mic RMS + live wake-word diagnostics) into the
    // state so the audio-diagnostics screen's meters + readouts update in real time.
    var next = _state.copyWith(
      micLevel: rms,
      wakeScore: e.score,
      wakeAvgScore: e.avgScore,
      wakeThreshold: e.threshold,
      captureGainDb: e.gainDb,
    );
    if (_endpointCueEnabled && _state.phase == TurnPhase.listening) {
      if (rms >= _endpointRmsThreshold) {
        _speechSeen = true;
        _lastVoiceAt = now;
      } else if (_speechSeen &&
          _lastVoiceAt != null &&
          now.difference(_lastVoiceAt!) >= _endpointSilence) {
        _resumeSince = null;
        next = next.copyWith(phase: TurnPhase.processing);
      }
    } else if (_endpointCueEnabled && _state.phase == TurnPhase.processing) {
      // The local cue fired, but the Core hasn't sent its transcript yet, so it is
      // still listening. If the user carries on talking (a pause the cue mistook for
      // end-of-speech), revert to listening so the ring comes back. The transcript
      // moves the phase on to thinking, which ends this window.
      if (rms >= _endpointRmsThreshold) {
        _resumeSince ??= now;
        if (now.difference(_resumeSince!) >= _endpointResumeMin) {
          _resumeSince = null;
          _lastVoiceAt = now;
          next = next.copyWith(phase: TurnPhase.listening);
        }
      } else {
        _resumeSince = null;
      }
    }
    _emit(next);
  }

  void _onDisconnected(String message) {
    // "turn complete" is the normal end of a successful turn — stay online and
    // return to idle. Anything else (no host / connect failed / turn error) means
    // the Mac was unreachable, so flag the disconnected indicator.
    final normalEnd = message.contains('turn complete');
    // On a clean end the reply audio is usually still playing out of the ring, so
    // leave `audioPlaying` untouched — it keeps the reply text on screen until the
    // engine's `speakingDone` fires. On a failed turn nothing is playing, so drop it.
    _emit(
      _state.copyWith(
        phase: TurnPhase.idle,
        online: normalEnd,
        statusMessage: normalEnd ? 'Ready' : message,
        audioPlaying: normalEnd ? null : false,
        // The turn ended; the follow-up cue belongs to a live turn only. If a chained
        // follow-up is coming, the engine's next `listeningFollowup` re-sets it.
        followUp: false,
      ),
    );
  }

  /// The reply's TTS audio has finished playing (drained naturally, or was flushed
  /// by a barge-in). Now — and only now — remove the on-screen text.
  ///
  /// Guard against a stale drain event from a previous reply landing inside a newer
  /// turn: only clear the text if we're still in the speaking/just-finished window
  /// (SPEAKING, or idle with audio that was playing). If a fresh turn has already
  /// moved us on (listening/connecting/thinking), just drop the flag and keep the
  /// new turn's text.
  void _onSpeakingDone() {
    final endingReply =
        _state.phase == TurnPhase.speaking ||
        (_state.phase == TurnPhase.idle && _state.audioPlaying);
    if (endingReply) {
      _emit(_state.copyWith(audioPlaying: false, transcript: '', reply: ''));
    } else if (_state.audioPlaying) {
      _emit(_state.copyWith(audioPlaying: false));
    }
  }

  void _emit(AssistantState next) {
    if (_disposed) return;
    _state = next;
    _syncMusicContext();
    notifyListeners();
    // Keep the offline poll in sync with every state change: run it while we're
    // offline and idle, stop it as soon as we're online or a turn is in flight.
    _syncOfflinePoll();
  }

  /// Start/stop the reachability poll based on current state. While offline and
  /// not mid-turn, probe the orchestrator every [_offlinePollInterval] so the UI
  /// recovers to "online" on its own instead of waiting for the next wake word.
  void _syncOfflinePoll() {
    if (_probe == null) return; // feature disabled (no probe injected)
    final shouldPoll = !_disposed && !_state.online && !_state.turnActive;
    if (shouldPoll) {
      _offlinePollTimer ??= Timer.periodic(
        _offlinePollInterval,
        (_) => _probeOnce(),
      );
    } else {
      _offlinePollTimer?.cancel();
      _offlinePollTimer = null;
    }
  }

  Future<void> _probeOnce() async {
    // Skip if state changed since the tick was scheduled, or a probe is in flight
    // (a slow probe must not stack up behind the periodic timer).
    if (_disposed || _probing || _state.online || _state.turnActive) return;
    _probing = true;
    var reachable = false;
    try {
      reachable = await _probe!();
    } catch (_) {
      reachable = false;
    }
    _probing = false;
    if (_disposed || !reachable) return;
    // Only flip to online if we're still idle+offline (a turn may have started).
    if (!_state.online && !_state.turnActive) {
      _emit(_state.copyWith(online: true, statusMessage: 'Ready'));
    }
  }

  @override
  void dispose() {
    _nightTick?.cancel();
    _disposed = true;
    _reconnectTimer?.cancel();
    _offlinePollTimer?.cancel();
    _weatherAutoCloseTimer?.cancel();
    _placeAutoCloseTimer?.cancel();
    _cameraAutoCloseTimer?.cancel();
    _sub?.cancel();
    super.dispose();
  }
}
