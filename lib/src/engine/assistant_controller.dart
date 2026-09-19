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

import 'package:ambient_display/src/rust/api/engine.dart';

/// Opens the native engine event stream for a given config. Production passes
/// `startWakeWordEngine`; tests pass a fake.
typedef EngineStreamFactory = Stream<WakeWordEvent> Function(WakeWordConfig config);

/// Probes whether the Mac orchestrator is currently reachable. Returns `true` if a
/// connection/handshake succeeded. Production wires this to a control-protocol
/// round-trip (mDNS discover + connect); tests inject a fake. When left null the
/// offline poll is disabled.
typedef OrchestratorProbe = Future<bool> Function();

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
    this.finished = false,
  });

  /// Stable device-assigned id (used to update/remove the right chip).
  final int id;

  /// Spoken label ("pasta"), or empty for an unlabeled timer.
  final String label;

  /// Wall-clock instant the timer fires, derived on start from the reported
  /// remaining seconds so the UI can count down smoothly without per-second events.
  final DateTime deadline;

  /// True once the timer reached zero (the device alarm is sounding); the chip stays
  /// until acknowledged.
  final bool finished;

  TimerModel copyWith({bool? finished}) => TimerModel(
        id: id,
        label: label,
        deadline: deadline,
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
    this.captureReady = false,
    this.timers = const [],
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

  /// Input RMS level (~0..1), surfaced in capture-only mode as a liveness cue.
  final double micLevel;

  /// True once the mic capture stream has started at least once.
  final bool captureReady;

  /// Active on-device timers (Phase 2), in start order. Independent of [phase] —
  /// they run and display during idle and during a turn alike.
  final List<TimerModel> timers;

  /// Whether a turn is currently in flight (anything but idle/error).
  bool get turnActive => phase != TurnPhase.idle && phase != TurnPhase.error;

  AssistantState copyWith({
    TurnPhase? phase,
    bool? online,
    String? transcript,
    String? reply,
    String? statusMessage,
    String? wakeWord,
    double? micLevel,
    bool? captureReady,
    List<TimerModel>? timers,
  }) {
    return AssistantState(
      phase: phase ?? this.phase,
      online: online ?? this.online,
      transcript: transcript ?? this.transcript,
      reply: reply ?? this.reply,
      statusMessage: statusMessage ?? this.statusMessage,
      wakeWord: wakeWord ?? this.wakeWord,
      micLevel: micLevel ?? this.micLevel,
      captureReady: captureReady ?? this.captureReady,
      timers: timers ?? this.timers,
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
  })  : _config = config,
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

  final WakeWordConfig _config;
  final EngineStreamFactory _startEngine;

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

  /// True once we've seen speech-level audio in the current turn (so trailing
  /// silence means "done speaking" rather than "hasn't started yet").
  bool _speechSeen = false;

  /// When the mic last carried speech-level audio in the current turn.
  DateTime? _lastVoiceAt;

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
    // Start probing immediately if we're offline (don't wait for the first event).
    _syncOfflinePoll();
  }

  void _listen() {
    if (_disposed) return;
    try {
      _sub = _startEngine(_config).listen(
        _onEvent,
        onError: (Object e, StackTrace _) => _scheduleReconnect('engine error: $e'),
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
    _emit(_state.copyWith(
      phase: TurnPhase.error,
      online: false,
      statusMessage: '$reason — reconnecting…',
    ));
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

  void _onEvent(WakeWordEvent e) {
    // A healthy event stream resets the backoff.
    _backoff = _minBackoff;

    switch (e.kind) {
      case WakeWordEventKind.started:
        _emit(_state.copyWith(
          captureReady: true,
          statusMessage: 'Listening on ${e.device}',
        ));
      case WakeWordEventKind.status:
        _emit(_state.copyWith(statusMessage: e.message));
      case WakeWordEventKind.level:
        _onLevel(e.rms);
      case WakeWordEventKind.detected:
        // A wake word starts a fresh turn: clear the previous exchange and reset the
        // local end-of-speech tracker.
        _speechSeen = false;
        _lastVoiceAt = _clock();
        _emit(_state.copyWith(
          phase: TurnPhase.listening,
          wakeWord: e.model,
          transcript: '',
          reply: '',
        ));
      case WakeWordEventKind.connecting:
        _emit(_state.copyWith(
          phase: TurnPhase.connecting,
          online: true,
          statusMessage: e.message,
        ));
      case WakeWordEventKind.streaming:
        _emit(_state.copyWith(phase: TurnPhase.listening, online: true));
      case WakeWordEventKind.transcript:
        _emit(_state.copyWith(
          phase: TurnPhase.thinking,
          online: true,
          transcript: e.transcript,
        ));
      case WakeWordEventKind.replyToken:
        _emit(_state.copyWith(
          phase: TurnPhase.thinking,
          online: true,
          reply: _state.reply + e.reply,
        ));
      case WakeWordEventKind.speaking:
        _emit(_state.copyWith(phase: TurnPhase.speaking, online: true));
      case WakeWordEventKind.disconnected:
        _onDisconnected(e.message);
      case WakeWordEventKind.stopped:
        _scheduleReconnect('capture stopped');
      case WakeWordEventKind.error:
        _scheduleReconnect(e.message);
      case WakeWordEventKind.timerStarted:
        // Anchor a wall-clock deadline so the overlay can count down smoothly with
        // no per-second events from Rust. Replace any existing timer with this id.
        final deadline = _clock().add(Duration(seconds: e.timerRemainingSecs));
        _emit(_state.copyWith(timers: [
          ..._state.timers.where((t) => t.id != e.timerId),
          TimerModel(id: e.timerId, label: e.timerLabel, deadline: deadline),
        ]));
      case WakeWordEventKind.timerFinished:
        // Mark it finished (its alarm is sounding); the chip stays until dismissed.
        _emit(_state.copyWith(
          timers: _state.timers
              .map((t) => t.id == e.timerId ? t.copyWith(finished: true) : t)
              .toList(),
        ));
      case WakeWordEventKind.timerCancelled:
        _emit(_state.copyWith(
          timers: _state.timers.where((t) => t.id != e.timerId).toList(),
        ));
    }
  }

  /// Dismiss a timer from the UI (e.g. the user taps a finished/ringing chip).
  /// Device-side the timer has already fired or been cancelled; this only clears the
  /// chip.
  void dismissTimer(int id) {
    if (_state.timers.any((t) => t.id == id)) {
      _emit(_state.copyWith(timers: _state.timers.where((t) => t.id != id).toList()));
    }
  }

  /// Fold a mic-level event into the state, and — while we're actively listening —
  /// run the local end-of-speech detector so the UI flips to [TurnPhase.processing]
  /// the moment the user stops talking, ahead of the Mac's VAD + transcript.
  void _onLevel(double rms) {
    final now = _clock();
    var next = _state.copyWith(micLevel: rms);
    if (_endpointCueEnabled && _state.phase == TurnPhase.listening) {
      if (rms >= _endpointRmsThreshold) {
        _speechSeen = true;
        _lastVoiceAt = now;
      } else if (_speechSeen &&
          _lastVoiceAt != null &&
          now.difference(_lastVoiceAt!) >= _endpointSilence) {
        next = next.copyWith(phase: TurnPhase.processing);
      }
    }
    _emit(next);
  }

  void _onDisconnected(String message) {
    // "turn complete" is the normal end of a successful turn — stay online and
    // return to idle. Anything else (no host / connect failed / turn error) means
    // the Mac was unreachable, so flag the disconnected indicator.
    final normalEnd = message.contains('turn complete');
    _emit(_state.copyWith(
      phase: TurnPhase.idle,
      online: normalEnd,
      statusMessage: normalEnd ? 'Ready' : message,
    ));
  }

  void _emit(AssistantState next) {
    if (_disposed) return;
    _state = next;
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
      _offlinePollTimer ??=
          Timer.periodic(_offlinePollInterval, (_) => _probeOnce());
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
    _disposed = true;
    _reconnectTimer?.cancel();
    _offlinePollTimer?.cancel();
    _sub?.cancel();
    super.dispose();
  }
}
