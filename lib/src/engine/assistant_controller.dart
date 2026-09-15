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

/// Where the current voice turn is, from the UI's point of view.
enum TurnPhase {
  /// No active turn — the ambient/idle screen (slideshow) is showing.
  idle,

  /// Wake word fired / mic streaming up: we are listening to the user.
  listening,

  /// Contacting the Mac orchestrator over the discovered Wyoming socket.
  connecting,

  /// Transcript received; the assistant is composing its reply.
  thinking,

  /// The reply's TTS audio is playing back through the speakers.
  speaking,

  /// A fatal engine error — capture is being restarted.
  error,
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
    );
  }
}

/// Consumes the engine event stream and exposes an [AssistantState] to the UI.
class AssistantController extends ChangeNotifier {
  AssistantController({
    required WakeWordConfig config,
    EngineStreamFactory? startEngine,
    Duration minBackoff = const Duration(seconds: 1),
    Duration maxBackoff = const Duration(seconds: 30),
  })  : _config = config,
        // `startWakeWordEngine` takes a named `config:`; adapt it to the positional
        // [EngineStreamFactory] shape (tests inject their own factory).
        _startEngine = startEngine ?? _defaultEngineStream,
        _minBackoff = minBackoff,
        _maxBackoff = maxBackoff;

  static Stream<WakeWordEvent> _defaultEngineStream(WakeWordConfig config) =>
      startWakeWordEngine(config: config);

  final WakeWordConfig _config;
  final EngineStreamFactory _startEngine;
  final Duration _minBackoff;
  final Duration _maxBackoff;

  AssistantState _state = const AssistantState();
  AssistantState get state => _state;

  StreamSubscription<WakeWordEvent>? _sub;
  Timer? _reconnectTimer;
  Duration _backoff = const Duration(seconds: 1);
  bool _disposed = false;

  /// Begin (or restart) consuming the engine stream. Safe to call once at startup.
  void start() {
    _reconnectTimer?.cancel();
    _sub?.cancel();
    _backoff = _minBackoff;
    _listen();
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
        _emit(_state.copyWith(micLevel: e.rms));
      case WakeWordEventKind.detected:
        // A wake word starts a fresh turn: clear the previous exchange.
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
    }
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
  }

  @override
  void dispose() {
    _disposed = true;
    _reconnectTimer?.cancel();
    _sub?.cancel();
    super.dispose();
  }
}
