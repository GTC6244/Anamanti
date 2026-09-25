// Verifies the offline reachability poll (AssistantController): while offline and
// idle it probes the orchestrator on an interval and flips to online on success,
// without needing a wake word.

import 'dart:async';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
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
      cameraProximity: false,
      proximityMotionThreshold: 0,
      proximityReleaseSecs: 0,
    );

void main() {
  test('offline poll flips online once the orchestrator becomes reachable', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    var probeCalls = 0;
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream, // never emits, never closes → stays idle
      offlinePollInterval: const Duration(milliseconds: 25),
      // Unreachable for the first two ticks, then reachable.
      probeOrchestrator: () async {
        probeCalls++;
        return probeCalls >= 3;
      },
    );

    addTearDown(() async {
      controller.dispose();
      await engine.close();
    });

    controller.start();
    expect(controller.state.online, isFalse, reason: 'starts offline');

    // Give the poll several intervals to succeed.
    await Future<void>.delayed(const Duration(milliseconds: 200));

    expect(controller.state.online, isTrue, reason: 'poll should recover online');
    expect(probeCalls, greaterThanOrEqualTo(3));
  });

  test('no probe injected → no polling (feature disabled, e.g. tests)', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      offlinePollInterval: const Duration(milliseconds: 25),
      // probeOrchestrator omitted
    );
    addTearDown(() async {
      controller.dispose();
      await engine.close();
    });

    controller.start();
    await Future<void>.delayed(const Duration(milliseconds: 100));
    expect(controller.state.online, isFalse);
  });

  test('poll stops probing once online', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    var probeCalls = 0;
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      offlinePollInterval: const Duration(milliseconds: 25),
      probeOrchestrator: () async {
        probeCalls++;
        return true; // reachable immediately
      },
    );
    addTearDown(() async {
      controller.dispose();
      await engine.close();
    });

    controller.start();
    await Future<void>.delayed(const Duration(milliseconds: 120));
    final callsAfterOnline = probeCalls;
    expect(controller.state.online, isTrue);
    // Let more intervals elapse; probing must have stopped.
    await Future<void>.delayed(const Duration(milliseconds: 120));
    expect(probeCalls, callsAfterOnline, reason: 'should stop probing once online');
  });
}
