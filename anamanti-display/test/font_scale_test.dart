// Global voice-controlled font scale: the AssistantController folding a `fontAdjust`
// event into the onFontAdjust callback + pushing font context, and AppSettings
// persisting/clamping the scale.

import 'dart:async';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:anamanti_display/src/settings/app_settings.dart';
import 'package:flutter_test/flutter_test.dart';

WakeWordConfig _cfg() => WakeWordConfig(
  melspecModelPath: '',
  embeddingModelPath: '',
  wakewordModelPath: '',
  modelName: 'test',
  threshold: 0.5,
  activeThreshold: 0.7,
  orchestratorKey: '',
  discoveryTimeoutSecs: BigInt.zero,
  turnTimeoutSecs: BigInt.zero,
  smoothingWindow: 2,
  fireOnPeak: false,
  playbackBufferSecs: 30,
  captureGainDb: 0,
  useAudiorecord: false,
  micSource: 6,
  platformAec: false,
  platformAgc: true,
  platformNs: true,
  cameraProximity: false,
  proximityMotionThreshold: 0,
  proximityReleaseSecs: 0,
);

WakeWordEvent _ev(WakeWordEventKind kind, {String recipeAction = ''}) =>
    WakeWordEvent(
      kind: kind,
      message: '',
      device: '',
      deviceSampleRate: 0,
      channels: 0,
      rms: 0,
      score: 0,
      avgScore: 0,
      threshold: 0,
      gainDb: 0,
      model: '',
      transcript: '',
      reply: '',
      timerId: 0,
      timerLabel: '',
      timerRemainingSecs: 0,
      present: false,
      recipeJson: '',
      weatherJson: '',
      placeJson: '',
      recipeAction: recipeAction,
    );

/// Captures the arguments of each [FontContextSink] push.
class _FontCtx {
  _FontCtx(this.scalable, this.scale, this.atMin, this.atMax);
  final bool scalable;
  final double scale;
  final bool atMin;
  final bool atMax;
}

void main() {
  test('controller routes a fontAdjust event to onFontAdjust', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final directions = <String>[];
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      onFontAdjust: directions.add,
    )..start();
    addTearDown(controller.dispose);

    engine.add(_ev(WakeWordEventKind.fontAdjust, recipeAction: 'increase'));
    await Future<void>.delayed(Duration.zero);
    engine.add(_ev(WakeWordEventKind.fontAdjust, recipeAction: 'decrease'));
    await Future<void>.delayed(Duration.zero);

    expect(directions, ['increase', 'decrease']);
  });

  test('controller pushes font context at start and on scale change', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final pushes = <_FontCtx>[];
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      fontScale: kFontScaleMin, // start at the minimum bound
      setFontContext:
          ({
            required bool scalable,
            required double scale,
            required bool atMin,
            required bool atMax,
          }) => pushes.add(_FontCtx(scalable, scale, atMin, atMax)),
    )..start();
    addTearDown(controller.dispose);

    // start() seeds the context: scalable, at the min bound.
    expect(pushes, isNotEmpty);
    expect(pushes.last.scalable, isTrue);
    expect(pushes.last.scale, kFontScaleMin);
    expect(pushes.last.atMin, isTrue);
    expect(pushes.last.atMax, isFalse);

    // Raising to the max bound re-pushes with atMax set.
    controller.updateFontScale(kFontScaleMax);
    expect(pushes.last.scale, kFontScaleMax);
    expect(pushes.last.atMin, isFalse);
    expect(pushes.last.atMax, isTrue);
  });

  group('AppSettings.fontScale', () {
    test('defaults to 1.0 and round-trips through JSON', () {
      expect(const AppSettings().fontScale, 1.0);
      final s = const AppSettings().copyWith(fontScale: 1.3);
      final back = AppSettings.fromJson(s.toJson());
      expect(back.fontScale, 1.3);
    });

    test('clamps out-of-range persisted values', () {
      final tooBig = AppSettings.fromJson({'fontScale': 9.0});
      expect(tooBig.fontScale, kFontScaleMax);
      final tooSmall = AppSettings.fromJson({'fontScale': 0.1});
      expect(tooSmall.fontScale, kFontScaleMin);
    });

    test('falls back to 1.0 when the key is missing or invalid', () {
      expect(AppSettings.fromJson(<String, dynamic>{}).fontScale, 1.0);
      expect(AppSettings.fromJson({'fontScale': 'big'}).fontScale, 1.0);
    });
  });
}
