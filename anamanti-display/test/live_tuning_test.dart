// Live-tuned capture gain + wake-word thresholds must survive an engine reconnect:
// the controller reuses its config when it restarts the engine stream, so live
// tuning has to be folded into that config (plans/SettingsReorgPlan.md §3.5).

import 'dart:async';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:flutter_test/flutter_test.dart';

WakeWordConfig _cfg() => WakeWordConfig(
  melspecModelPath: 'm',
  embeddingModelPath: 'e',
  wakewordModelPath: 'w',
  modelName: 'test',
  threshold: 0.5,
  activeThreshold: 0.7,
  orchestratorKey: 'core-1',
  discoveryTimeoutSecs: BigInt.from(3),
  turnTimeoutSecs: BigInt.from(30),
  smoothingWindow: 3,
  fireOnPeak: true,
  playbackBufferSecs: 30,
  captureGainDb: 0,
  useAudiorecord: true,
  micSource: 6,
  platformAec: false,
  platformAgc: true,
  platformNs: true,
  cameraProximity: true,
  proximityMotionThreshold: 0.1,
  proximityReleaseSecs: 300,
);

void main() {
  test(
    'a reconnect re-seeds the live-tuned values, not the original ones',
    () async {
      final started = <WakeWordConfig>[];
      final streams = <StreamController<WakeWordEvent>>[];
      final controller = AssistantController(
        config: _cfg(),
        minBackoff: const Duration(milliseconds: 10),
        startEngine: (config) {
          started.add(config);
          final s = StreamController<WakeWordEvent>();
          streams.add(s);
          return s.stream;
        },
      );
      addTearDown(() async {
        controller.dispose();
        for (final s in streams) {
          await s.close();
        }
      });

      controller.start();
      expect(started, hasLength(1));

      controller.updateLiveTuning(
        gainDb: 12,
        threshold: 0.35,
        activeThreshold: 0.8,
      );

      // The engine stream ends → the controller reconnects with its config.
      await streams.first.close();
      await Future<void>.delayed(const Duration(milliseconds: 100));

      expect(started.length, greaterThanOrEqualTo(2));
      final re = started.last;
      expect(re.captureGainDb, 12);
      expect(re.threshold, 0.35);
      expect(re.activeThreshold, 0.8);
      // Everything else is carried over unchanged.
      expect(re.modelName, 'test');
      expect(re.orchestratorKey, 'core-1');
      expect(re.smoothingWindow, 3);
      expect(re.fireOnPeak, isTrue);
      expect(re.useAudiorecord, isTrue);
      expect(re.proximityReleaseSecs, 300);
      expect(re.discoveryTimeoutSecs, BigInt.from(3));
    },
  );
}
