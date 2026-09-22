// Camera proximity → screen brightness (Plan.MD §5).
//
// Two seams are covered here without any device: (1) the AssistantController folds
// a `Presence` engine event into `AssistantState.userPresent`, and (2) the
// ScreenBrightnessController maps present→near / absent→away and only crosses the
// platform channel when the target actually changes.

import 'dart:async';

import 'package:ambient_display/src/engine/assistant_controller.dart';
import 'package:ambient_display/src/engine/screen_brightness.dart';
import 'package:ambient_display/src/rust/api/engine.dart';
import 'package:flutter/services.dart';
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
      useAudiorecord: false,
      micSource: 6,
      platformAec: false,
      platformAgc: true,
      platformNs: true,
      cameraProximity: true,
      proximityMotionThreshold: 0,
      proximityReleaseSecs: 0,
    );

WakeWordEvent _presence(bool present) => WakeWordEvent(
      kind: WakeWordEventKind.presence,
      message: '',
      device: '',
      deviceSampleRate: 0,
      channels: 0,
      rms: 0,
      score: 0,
      model: '',
      transcript: '',
      reply: '',
      timerId: 0,
      timerLabel: '',
      timerRemainingSecs: 0,
      present: present,
    );

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  test('presence events fold into AssistantState.userPresent', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
    )..start();
    addTearDown(controller.dispose);

    // Starts bright (so an unavailable camera never leaves the screen dim).
    expect(controller.state.userPresent, isTrue);

    engine.add(_presence(false)); // room went quiet → dim
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.userPresent, isFalse);

    engine.add(_presence(true)); // someone approached → bright
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.userPresent, isTrue);
  });

  test('brightness controller maps presence and de-dupes channel calls', () async {
    final calls = <double>[];
    const channel = MethodChannel('test/ambient_brightness');
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(channel, (call) async {
      if (call.method == 'setBrightness') calls.add(call.arguments as double);
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
    );

    await c.apply(true); // bright
    await c.apply(true); // unchanged → no channel call
    await c.apply(false); // dim
    await c.apply(false); // unchanged → no channel call
    await c.apply(true); // bright again
    expect(calls, [1.0, 0.25, 1.0]);

    await c.reset(); // hands brightness back to the system default
    expect(calls.last, -1.0);
  });
}
