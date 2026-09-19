// Phase 2 on-device timers: the controller folds timer events into `AssistantState`
// and the TimersOverlay renders a live countdown with tap-to-dismiss.

import 'dart:async';

import 'package:ambient_display/src/engine/assistant_controller.dart';
import 'package:ambient_display/src/rust/api/engine.dart';
import 'package:ambient_display/src/ui/timers_overlay.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

WakeWordConfig _cfg() => WakeWordConfig(
      melspecModelPath: '',
      embeddingModelPath: '',
      wakewordModelPath: '',
      modelName: 'test',
      threshold: 0.5,
      activeThreshold: 0.7,
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
    );

WakeWordEvent _ev(
  WakeWordEventKind kind, {
  int timerId = 0,
  String timerLabel = '',
  int timerRemainingSecs = 0,
}) =>
    WakeWordEvent(
      kind: kind,
      message: '',
      device: '',
      deviceSampleRate: 0,
      channels: 0,
      rms: 0,
      score: 0,
      model: '',
      transcript: '',
      reply: '',
      timerId: timerId,
      timerLabel: timerLabel,
      timerRemainingSecs: timerRemainingSecs,
    );

void main() {
  test('controller folds start/finish/cancel/dismiss into timer state', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final now = DateTime(2026, 1, 1, 12, 0, 0);
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      clock: () => now,
    )..start();
    addTearDown(controller.dispose);

    // Start → one running timer with a deadline derived from the reported seconds.
    engine.add(_ev(WakeWordEventKind.timerStarted,
        timerId: 1, timerLabel: 'pasta', timerRemainingSecs: 300));
    await Future<void>.delayed(Duration.zero);
    final t = controller.state.timers.single;
    expect(t.id, 1);
    expect(t.label, 'pasta');
    expect(t.deadline, now.add(const Duration(seconds: 300)));
    expect(t.finished, isFalse);

    // A second concurrent timer (unlimited timers).
    engine.add(_ev(WakeWordEventKind.timerStarted, timerId: 2, timerRemainingSecs: 60));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.timers.length, 2);

    // Finish #1 → marked finished (alarm sounding), still listed.
    engine.add(_ev(WakeWordEventKind.timerFinished, timerId: 1));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.timers.firstWhere((t) => t.id == 1).finished, isTrue);
    expect(controller.state.timers.length, 2);

    // Cancel #2 → removed.
    engine.add(_ev(WakeWordEventKind.timerCancelled, timerId: 2));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.timers.map((t) => t.id), [1]);

    // Dismiss #1 → cleared from the UI.
    controller.dismissTimer(1);
    expect(controller.state.timers, isEmpty);
  });

  test('formatRemaining renders m:ss / h:mm:ss and clamps negatives', () {
    expect(formatRemaining(const Duration(seconds: 5)), '0:05');
    expect(formatRemaining(const Duration(minutes: 5)), '5:00');
    expect(formatRemaining(const Duration(minutes: 5, seconds: 3)), '5:03');
    expect(formatRemaining(const Duration(hours: 1, minutes: 2, seconds: 3)), '1:02:03');
    expect(formatRemaining(const Duration(seconds: -5)), '0:00');
  });

  testWidgets('overlay shows countdown + dismisses a finished chip on tap',
      (tester) async {
    final now = DateTime(2026, 1, 1, 12, 0, 0);
    int? dismissed;
    final timers = [
      TimerModel(id: 1, label: 'pasta', deadline: now.add(const Duration(minutes: 5))),
      TimerModel(id: 2, label: '', deadline: now, finished: true),
    ];

    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: TimersOverlay(
          timers: timers,
          clock: () => now,
          onDismiss: (id) => dismissed = id,
        ),
      ),
    ));

    expect(find.text('pasta  5:00'), findsOneWidget);
    expect(find.text("Time's up"), findsOneWidget);

    await tester.tap(find.text("Time's up"));
    expect(dismissed, 2);

    // Unmount so the overlay's periodic ticker is cancelled before the test ends.
    await tester.pumpWidget(const SizedBox.shrink());
  });
}
