// Screen-brightness actuation for the ambient display (Plan.MD §5).
//
// The Rust engine owns the *sensing* (front-camera frame-motion presence); Flutter
// folds that — together with active voice turns and full-screen modes — into
// `AssistantState.screenAwake`. Changing the actual backlight is presentation, so it
// stays here in Flutter: this controller crosses a platform MethodChannel to
// `MainActivity`, which sets the Android window brightness.
//
// Driving from `screenAwake` (rather than camera presence alone) is deliberate: the
// screen must return to full brightness on *any* wake — a camera approach, a voice
// turn, or a recipe/weather/place mode — not only a camera-presence flip. That is
// exactly the set of causes that lift the away-face blackout, so the backlight and
// the on-screen presentation stay in lockstep.
//
// Night dimming layers on top: while the camera-as-lux-sensor reports the room's
// lights are off (`AssistantState.roomDark`, gated by the "Dim at night" setting),
// both levels drop to their night values so the display doesn't light up a dark room.
//
// It is deliberately tiny and injectable (the channel can be faked) so unit tests
// can assert the awake→bright / asleep→dim mapping with no device.

import 'package:flutter/services.dart';

import 'package:anamanti_display/src/engine/wakeword_config.dart';

/// Drives the window backlight from the screen's awake/asleep state.
class ScreenBrightnessController {
  ScreenBrightnessController({
    MethodChannel? channel,
    this.nearBrightness = WakeWordDefaults.brightnessNear,
    this.awayBrightness = WakeWordDefaults.brightnessAway,
    this.nightNearBrightness = WakeWordDefaults.nightBrightnessNear,
    this.nightAwayBrightness = WakeWordDefaults.nightBrightnessAway,
  }) : _channel = channel ?? const MethodChannel('anamanti_display/brightness');

  final MethodChannel _channel;

  /// Absolute window brightness [0,1] when the screen is awake (full brightness).
  final double nearBrightness;

  /// Absolute window brightness [0,1] when the screen has dimmed to the away face.
  final double awayBrightness;

  /// Awake brightness while the room's lights are off (night dimming).
  final double nightNearBrightness;

  /// Away brightness while the room's lights are off (night dimming).
  final double nightAwayBrightness;

  /// Last level actuated, so we only cross the channel on a real change.
  double? _lastLevel;

  /// The backlight level for an [awake]/asleep screen in a lit or [dark] room.
  double levelFor(bool awake, {bool dark = false}) => dark
      ? (awake ? nightNearBrightness : nightAwayBrightness)
      : (awake ? nearBrightness : awayBrightness);

  /// Apply the brightness for [awake] (and [dark], the night-dimming state). No-ops
  /// if the target is unchanged since the last call, so it's safe to call on every
  /// state emit (it dedupes the high-frequency stream down to real transitions).
  Future<void> apply(bool awake, {bool dark = false}) async {
    final level = levelFor(awake, dark: dark);
    if (_lastLevel == level) return;
    _lastLevel = level;
    try {
      await _channel.invokeMethod<void>('setBrightness', level);
    } catch (_) {
      // Best-effort: on a platform without the channel (host tests, no camera) this
      // is a harmless no-op — the screen simply keeps its current brightness.
    }
  }

  /// Hand brightness back to the system/user default (e.g. on shutdown).
  Future<void> reset() async {
    _lastLevel = null;
    try {
      await _channel.invokeMethod<void>('setBrightness', -1.0);
    } catch (_) {
      // Ignore — see [apply].
    }
  }
}
