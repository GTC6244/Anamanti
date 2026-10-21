// Night dimming: the camera-as-lux-sensor (`camera/ambient.rs`) reports when the
// room's lights go off, and the display drops its backlight to night levels.
//
// Covered without a device: (1) the AssistantController folds an `AmbientLight`
// engine event into `AssistantState.roomDark` / `ambientLux` without counting it as
// user activity, (2) ScreenBrightnessController maps awake × dark to the four levels
// and de-dupes channel calls, (3) the settings persist and clamp, and (4) the
// darkness-preset / live-readout helpers. The Settings widgets themselves are
// covered in settings_screen_test.dart.

import 'dart:async';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/engine/night_schedule.dart';
import 'package:anamanti_display/src/engine/screen_brightness.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:anamanti_display/src/settings/app_settings.dart';
import 'package:anamanti_display/src/ui/settings/display_section.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/wake_word_events.dart';

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
  cameraProximity: true,
  proximityMotionThreshold: 0,
  proximityReleaseSecs: 0,
);

WakeWordEvent _light({
  required bool dark,
  required double lux,
  bool covered = false,
}) => wakeEvent(
  WakeWordEventKind.ambientLight,
  dark: dark,
  lux: lux,
  covered: covered,
);

/// A controller on a fake engine whose clock reads [now()].
(AssistantController, StreamController<WakeWordEvent>) _controller(
  DateTime Function() now,
) {
  final engine = StreamController<WakeWordEvent>.broadcast();
  final c = AssistantController(
    config: _cfg(),
    startEngine: (_) => engine.stream,
    clock: now,
  )..start();
  return (c, engine);
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  test('ambient-light events fold into roomDark / ambientLux', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    var activity = 0;
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      onUserActivity: () => activity++,
    )..start();
    addTearDown(controller.dispose);

    // Starts lit with no reading, so a missing camera never dims the screen.
    expect(controller.state.roomDark, isFalse);
    expect(controller.state.ambientLux, isNull);

    engine.add(_light(dark: false, lux: 140));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.roomDark, isFalse);
    expect(controller.state.ambientLux, closeTo(140, 0.01));

    engine.add(_light(dark: true, lux: 0.6)); // lights went off
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.roomDark, isTrue);
    expect(controller.state.ambientLux, closeTo(0.6, 0.01));

    // The camera's own light readings are not user activity (they must not keep
    // resetting the dim countdown).
    expect(activity, 0);
  });

  test('brightness controller maps awake × dark and de-dupes', () async {
    final calls = <double>[];
    const channel = MethodChannel('test/night_brightness');
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(channel, (call) async {
          if (call.method == 'setBrightness') {
            calls.add(call.arguments as double);
          }
          return null;
        });
    addTearDown(() {
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(channel, null);
    });

    final c = ScreenBrightnessController(
      channel: channel,
      nearBrightness: 1.0,
      awayBrightness: 0.25,
      nightNearBrightness: 0.15,
      nightAwayBrightness: 0.02,
    );

    await c.apply(true); // lit room, awake
    await c.apply(true, dark: true); // lights off → night level
    await c.apply(true, dark: true); // unchanged → no call
    await c.apply(false, dark: true); // nobody around in the dark
    await c.apply(false); // lights back on, still away
    await c.apply(true); // someone approaches
    expect(calls, [1.0, 0.15, 0.02, 0.25, 1.0]);
  });

  test('default night levels are dimmer than the day levels', () {
    final c = ScreenBrightnessController();
    expect(c.levelFor(true, dark: true), lessThan(c.levelFor(true)));
    expect(c.levelFor(false, dark: true), lessThan(c.levelFor(false)));
  });

  test('night-dim settings round-trip and clamp', () {
    const d = AppSettings();
    expect(d.nightDimEnabled, isTrue);
    expect(d.nightDarkLux, kDefaultNightDarkLux);

    final s = d.copyWith(nightDimEnabled: false, nightDarkLux: 20);
    final back = AppSettings.fromJson(s.toJson());
    expect(back.nightDimEnabled, isFalse);
    expect(back.nightDarkLux, 20);
    expect(back, s);

    final wild = AppSettings.fromJson({...s.toJson(), 'nightDarkLux': 9999});
    expect(wild.nightDarkLux, kNightDarkLuxMax);
    // Missing keys (a settings file from before this feature) → defaults.
    final old = AppSettings.fromJson(<String, dynamic>{});
    expect(old.nightDimEnabled, isTrue);
    expect(old.nightDarkLux, kDefaultNightDarkLux);
  });

  test('darkness preset + readout helpers', () {
    expect(nearestNightDarkPreset(kDefaultNightDarkLux), kDefaultNightDarkLux);
    expect(nearestNightDarkPreset(1.0), 3.0);
    expect(nearestNightDarkPreset(45), 20.0);
    expect(formatAmbientLux(null, false), contains('No reading'));
    expect(formatAmbientLux(0.62, true), 'Now ~0.6 lux (dark)');
    expect(formatAmbientLux(143.4, false), 'Now ~143 lux (lit)');
    expect(
      formatAmbientLux(0.05, true, covered: true),
      'Now ~0.1 lux (camera covered)',
    );
  });

  group('covered camera', () {
    test('night window wraps past midnight; equal ends = never night', () {
      const w = NightWindow(startMinutes: 21 * 60, endMinutes: 7 * 60);
      expect(w.isNight(DateTime(2026, 1, 1, 20, 59)), isFalse);
      expect(w.isNight(DateTime(2026, 1, 1, 21)), isTrue);
      expect(w.isNight(DateTime(2026, 1, 1, 3)), isTrue);
      expect(w.isNight(DateTime(2026, 1, 1, 7)), isFalse);
      const nap = NightWindow(startMinutes: 13 * 60, endMinutes: 15 * 60);
      expect(nap.isNight(DateTime(2026, 1, 1, 14)), isTrue);
      expect(nap.isNight(DateTime(2026, 1, 1, 16)), isFalse);
      const none = NightWindow(startMinutes: 600, endMinutes: 600);
      expect(none.isNight(DateTime(2026, 1, 1, 10)), isFalse);
      expect(formatMinutesOfDay(7 * 60 + 5), '07:05');
    });

    // Camera covered: black frames → lux sensor says dark, presence says absent.
    const covered = AssistantState(
      userPresent: false,
      roomDark: true,
      cameraCovered: true,
    );

    test('clock mode keeps the blind camera\'s reading (dim away clock)', () {
      expect(covered.screenAwake, isFalse);
      expect(covered.effectiveRoomDark, isTrue);
    });

    test('schedule mode: awake + normal by day, dim clock at night', () {
      final day = covered.copyWith(
        coveredMode: CoveredCameraMode.schedule,
        scheduledNight: false,
      );
      expect(day.screenAwake, isTrue);
      expect(day.effectiveRoomDark, isFalse);

      final night = day.copyWith(scheduledNight: true);
      expect(night.screenAwake, isFalse);
      expect(night.effectiveRoomDark, isTrue);
      // A touch / voice turn at night still wakes it (at night levels).
      expect(night.copyWith(userPresent: true).screenAwake, isTrue);
    });

    test('schedule mode is ignored while the camera can see', () {
      const clear = AssistantState(
        userPresent: false,
        roomDark: false,
        coveredMode: CoveredCameraMode.schedule,
        scheduledNight: true,
      );
      expect(clear.screenAwake, isFalse); // presence still rules
      expect(clear.effectiveRoomDark, isFalse); // the lux sensor still rules
    });

    test('covered flag folds in from AmbientLight events', () async {
      final (c, engine) = _controller(() => DateTime(2026, 1, 1, 12));
      addTearDown(c.dispose);
      engine.add(_light(dark: true, lux: 0.01, covered: true));
      await Future<void>.delayed(Duration.zero);
      expect(c.state.cameraCovered, isTrue);
      engine.add(_light(dark: false, lux: 150));
      await Future<void>.delayed(Duration.zero);
      expect(c.state.cameraCovered, isFalse);
    });

    testWidgets('updateCoveredCamera applies live and ticks across the window edge', (
      tester,
    ) async {
      var now = DateTime(2026, 1, 1, 20, 58);
      final (c, _) = _controller(() => now);
      c.updateCoveredCamera(CoveredCameraMode.schedule, const NightWindow());
      expect(c.state.coveredMode, CoveredCameraMode.schedule);
      expect(c.state.scheduledNight, isFalse);

      now = DateTime(2026, 1, 1, 21, 0, 30);
      await tester.pump(const Duration(minutes: 1));
      expect(c.state.scheduledNight, isTrue);

      // Back to Clock mode: the tick stops (no pending timers left at teardown).
      c.updateCoveredCamera(CoveredCameraMode.clock, const NightWindow());
      expect(c.state.coveredMode, CoveredCameraMode.clock);
      c.dispose();
    });

    test('covered-camera settings round-trip with sane defaults', () {
      const d = AppSettings();
      expect(d.coveredCameraMode, CoveredCameraMode.clock);
      expect(d.nightWindow, const NightWindow());
      final s = d.copyWith(
        coveredCameraMode: CoveredCameraMode.schedule,
        nightStartMinutes: 22 * 60 + 30,
        nightEndMinutes: 6 * 60,
      );
      final back = AppSettings.fromJson(s.toJson());
      expect(back, s);
      expect(back.nightWindow.startMinutes, 22 * 60 + 30);
      final junk = AppSettings.fromJson({
        'coveredCameraMode': 'bogus',
        'nightStartMinutes': 99999,
      });
      expect(junk.coveredCameraMode, CoveredCameraMode.clock);
      expect(junk.nightStartMinutes, 24 * 60 - 1);
    });
  });
}
