// Proactive notifications (Approach A, visual-only phase).
//
// The Rust engine holds a persistent "notify" channel open to the pinned
// orchestrator and streams pushed `NotifyEvent`s (see `start_notify_channel` in
// `rust/src/api/engine.rs`). This controller subscribes to that stream, exposes the
// currently-displayed notification to the UI, and auto-dismisses it after a while
// (or on tap). It is deliberately independent of [AssistantController]: the notify
// channel is a sidecar, unrelated to the voice-turn lifecycle, so a problem on one
// never disturbs the other.

import 'dart:async';

import 'package:flutter/foundation.dart';

import 'package:anamanti_display/src/rust/api/engine.dart';

/// Injectable stream factory so widget tests can drive notifications without the
/// native channel. Defaults to [startNotifyChannel].
typedef NotifyStreamFactory = Stream<NotifyEvent> Function(NotifyConfig);

/// Owns the notify channel subscription and the current on-screen notification.
class NotificationController extends ChangeNotifier {
  NotificationController({
    required NotifyConfig config,
    NotifyStreamFactory? startChannel,
    Duration autoDismiss = const Duration(seconds: 20),
    // `config`/`autoDismiss` are public named params; a `this._config` initializing
    // formal would be an unusable private named parameter, so assign them here.
  })  : _config = config, // ignore: prefer_initializing_formals
        _startChannel = startChannel ?? _defaultChannel,
        _autoDismiss = autoDismiss; // ignore: prefer_initializing_formals

  static Stream<NotifyEvent> _defaultChannel(NotifyConfig config) =>
      startNotifyChannel(config: config);

  final NotifyConfig _config;
  final NotifyStreamFactory _startChannel;
  final Duration _autoDismiss;

  StreamSubscription<NotifyEvent>? _sub;
  Timer? _dismissTimer;
  NotifyEvent? _current;

  /// The notification currently shown, or null when nothing is showing.
  NotifyEvent? get current => _current;

  /// Subscribe to the notify channel. Safe to call once; a second call is a no-op.
  void start() {
    if (_sub != null) return;
    _sub = _startChannel(_config).listen(
      _onEvent,
      onError: (Object e, StackTrace _) =>
          debugPrint('notify channel error: $e'),
      onDone: () => debugPrint('notify channel closed'),
      cancelOnError: false,
    );
  }

  void _onEvent(NotifyEvent event) {
    _current = event;
    _dismissTimer?.cancel();
    if (_autoDismiss > Duration.zero) {
      _dismissTimer = Timer(_autoDismiss, dismiss);
    }
    notifyListeners();
  }

  /// Dismiss the current notification (auto-expiry or a user tap).
  void dismiss() {
    _dismissTimer?.cancel();
    _dismissTimer = null;
    if (_current != null) {
      _current = null;
      notifyListeners();
    }
  }

  @override
  void dispose() {
    _dismissTimer?.cancel();
    _sub?.cancel();
    super.dispose();
  }
}
