// Ambient weather channel (the twin of the notify channel).
//
// The Rust engine holds a persistent "weather" channel open to the pinned
// orchestrator and streams periodic current-conditions pushes (see
// `start_weather_channel` in `rust/src/api/engine.rs`). This controller subscribes to
// that stream and forwards each report's JSON to [onReport] — which the app shell wires
// to `AssistantController.applyWeatherPush`, so the small icon + temperature beside the
// idle clock stay fresh. Independent of the voice-turn lifecycle, like the notify
// channel.

import 'dart:async';

import 'package:flutter/foundation.dart';

import 'package:anamanti_display/src/rust/api/engine.dart';

/// Injectable stream factory so widget tests can drive weather pushes without the
/// native channel. Defaults to [startWeatherChannel].
typedef WeatherStreamFactory = Stream<WeatherPush> Function(WeatherConfig);

/// Owns the weather channel subscription and forwards pushes to [onReport].
class WeatherChannelController {
  WeatherChannelController({
    required WeatherConfig config,
    required this.onReport,
    WeatherStreamFactory? startChannel,
    // A `this._config` initializing formal would be an unusable private named
    // parameter, so assign it here.
  })  : _config = config, // ignore: prefer_initializing_formals
        _startChannel = startChannel ?? _defaultChannel;

  static Stream<WeatherPush> _defaultChannel(WeatherConfig config) =>
      startWeatherChannel(config: config);

  final WeatherConfig _config;
  final WeatherStreamFactory _startChannel;

  /// Called with each pushed report's JSON string.
  final void Function(String reportJson) onReport;

  StreamSubscription<WeatherPush>? _sub;

  /// Subscribe to the weather channel. Safe to call once; a second call is a no-op.
  void start() {
    if (_sub != null) return;
    _sub = _startChannel(_config).listen(
      (push) => onReport(push.reportJson),
      onError: (Object e, StackTrace _) =>
          debugPrint('weather channel error: $e'),
      onDone: () => debugPrint('weather channel closed'),
      cancelOnError: false,
    );
  }

  void dispose() {
    _sub?.cancel();
  }
}
