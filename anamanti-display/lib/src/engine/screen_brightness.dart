// Screen-brightness actuation for the camera proximity sensor (Plan.MD §5).
//
// The Rust engine owns the *sensing* (front-camera frame-motion presence) and
// reports it as `userPresent` on the assistant state. Changing the actual backlight
// is presentation, so it stays here in Flutter: this controller crosses a platform
// MethodChannel to `MainActivity`, which sets the Android window brightness.
//
// It is deliberately tiny and injectable (the channel can be faked) so unit tests
// can assert the present→bright / absent→dim mapping with no device.

import 'package:flutter/services.dart';

import 'package:anamanti_display/src/engine/wakeword_config.dart';

/// Drives the window backlight from the proximity sensor's present/absent state.
class ScreenBrightnessController {
  ScreenBrightnessController({
    MethodChannel? channel,
    this.nearBrightness = WakeWordDefaults.brightnessNear,
    this.awayBrightness = WakeWordDefaults.brightnessAway,
  }) : _channel = channel ?? const MethodChannel('anamanti_display/brightness');

  final MethodChannel _channel;

  /// Absolute window brightness [0,1] when someone is present.
  final double nearBrightness;

  /// Absolute window brightness [0,1] when the room has been quiet.
  final double awayBrightness;

  /// Last state actuated, so we only cross the channel on a real change.
  bool? _lastPresent;

  /// Apply the brightness for [present]. No-ops if unchanged since the last call.
  Future<void> apply(bool present) async {
    if (_lastPresent == present) return;
    _lastPresent = present;
    final level = present ? nearBrightness : awayBrightness;
    try {
      await _channel.invokeMethod<void>('setBrightness', level);
    } catch (_) {
      // Best-effort: on a platform without the channel (host tests, no camera) this
      // is a harmless no-op — the screen simply keeps its current brightness.
    }
  }

  /// Hand brightness back to the system/user default (e.g. on shutdown).
  Future<void> reset() async {
    _lastPresent = null;
    try {
      await _channel.invokeMethod<void>('setBrightness', -1.0);
    } catch (_) {
      // Ignore — see [apply].
    }
  }
}
