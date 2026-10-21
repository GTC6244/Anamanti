// Phase 2 on-device timers: the controller folds timer events into `AssistantState`
// and the timer widgets render a live countdown with tap-to-dismiss.

import 'dart:async';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:anamanti_display/src/ui/timers_overlay.dart';
import 'package:flutter/material.dart';
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
      avgScore: 0,
      threshold: 0,
      gainDb: 0,
      model: '',
      transcript: '',
      reply: '',
      timerId: timerId,
      timerLabel: timerLabel,
      timerRemainingSecs: timerRemainingSecs,
      present: false,
      recipeJson: '',
      weatherJson: '',
      placeJson: '',
      recipeAction: '',
      enrollJson: '',    );

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
    expect(t.total, const Duration(seconds: 300));
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

  test('Cancel timer turns the alarm off in the engine, not just the UI', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final dismissed = <int>[];
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      dismissNativeTimer: dismissed.add,
    )..start();
    addTearDown(controller.dispose);

    engine.add(_ev(WakeWordEventKind.timerStarted, timerId: 7, timerRemainingSecs: 60));
    engine.add(_ev(WakeWordEventKind.timerFinished, timerId: 7));
    await Future<void>.delayed(Duration.zero);
    controller.dismissTimer(7);
    expect(dismissed, [7]);
    expect(controller.state.timers, isEmpty);
  });

  test('a newly set timer spotlights the big view until the user speaks again',
      () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
    )..start();
    addTearDown(controller.dispose);

    engine.add(_ev(WakeWordEventKind.detected));
    engine.add(_ev(WakeWordEventKind.transcript));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.timerSpotlight, isFalse);

    // The turn sets a timer → the big timers take the screen right away.
    engine.add(_ev(WakeWordEventKind.timerStarted, timerId: 1, timerRemainingSecs: 600));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.timerSpotlight, isTrue);

    // The user speaks again → the conversation panel comes back.
    engine.add(_ev(WakeWordEventKind.transcript));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.timerSpotlight, isFalse);

    // An adjust re-emits the same id: not a new timer, so no spotlight.
    engine.add(_ev(WakeWordEventKind.timerStarted, timerId: 1, timerRemainingSecs: 900));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.timerSpotlight, isFalse);
  });

  test('formatRemaining renders m:ss / h:mm:ss and clamps negatives', () {
    expect(formatRemaining(const Duration(seconds: 5)), '0:05');
    expect(formatRemaining(const Duration(minutes: 5)), '5:00');
    expect(formatRemaining(const Duration(minutes: 5, seconds: 3)), '5:03');
    expect(formatRemaining(const Duration(hours: 1, minutes: 2, seconds: 3)), '1:02:03');
    expect(formatRemaining(const Duration(seconds: -5)), '0:00');
  });

  test('resolveTimerNames names labels, numbers only repeated unnamed timers', () {
    final now = DateTime(2026, 1, 1, 12, 0, 0);
    TimerModel mk(int id, String label) => TimerModel(
        id: id, label: label, deadline: now, total: const Duration(minutes: 1));

    // A lone unnamed timer is just "Timer" (no number).
    expect(resolveTimerNames([mk(1, '')]), {1: 'Timer'});

    // Labels are used verbatim; multiple unnamed timers get numbered.
    expect(
      resolveTimerNames([mk(1, 'pasta'), mk(2, ''), mk(3, '  '), mk(4, 'eggs')]),
      {1: 'pasta', 2: 'Timer 1', 3: 'Timer 2', 4: 'eggs'},
    );
  });

  test('effectiveTimerDisplay links the two timer widgets', () {
    final t = [
      TimerModel(
        id: 1,
        label: 'pasta',
        deadline: DateTime(2026, 1, 1, 12, 5),
        total: const Duration(minutes: 5),
      ),
    ];
    // No timers: neither widget.
    expect(const AssistantState().effectiveTimerDisplay, isNull);
    // Idle and someone present: the full-screen timers.
    expect(AssistantState(timers: t).effectiveTimerDisplay, TimerDisplayStyle.fullscreen);
    // A turn (or its reply audio) covers the idle screen: the compact overlay.
    expect(
      AssistantState(timers: t, phase: TurnPhase.listening).effectiveTimerDisplay,
      TimerDisplayStyle.compact,
    );
    expect(
      AssistantState(timers: t, audioPlaying: true).effectiveTimerDisplay,
      TimerDisplayStyle.compact,
    );
    // Away mode (nobody there, nothing up): only the clock shows.
    expect(AssistantState(timers: t, userPresent: false).effectiveTimerDisplay, isNull);
  });

  test('timers default to the big full-screen widget', () {
    expect(TimerDisplayStyle.defaultStyle, TimerDisplayStyle.fullscreen);
    expect(const AssistantState().timerDisplayStyle, TimerDisplayStyle.fullscreen);
    final t = [
      TimerModel(
        id: 1,
        label: '',
        deadline: DateTime(2026, 1, 1, 12, 5),
        total: const Duration(minutes: 5),
      ),
    ];
    // A compact preference shows the chip row while idle too.
    expect(
      AssistantState(timers: t, timerDisplayStyle: TimerDisplayStyle.compact)
          .effectiveTimerDisplay,
      TimerDisplayStyle.compact,
    );
  });

  testWidgets('big display shows name, countdown, and dismisses a ringing timer',
      (tester) async {
    final now = DateTime(2026, 1, 1, 12, 0, 0);
    int? dismissed;
    final timers = [
      TimerModel(
        id: 1,
        label: 'pasta',
        deadline: now.add(const Duration(minutes: 5)),
        total: const Duration(minutes: 10),
      ),
      TimerModel(id: 2, label: '', deadline: now, total: const Duration(minutes: 3), finished: true),
    ];

    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: FullScreenTimersView(
          timers: timers,
          clock: () => now,
          onDismiss: (id) => dismissed = id,
        ),
      ),
    ));

    // Name heading + big readout are separate texts in the big layout.
    expect(find.text('pasta'), findsOneWidget);
    expect(find.text('5:00'), findsOneWidget);
    // The lone unnamed timer reads "Timer" and, being finished, shows "Time's up".
    expect(find.text('Timer'), findsOneWidget);
    expect(find.text("Time's up"), findsOneWidget);

    // A running timer isn't dismissable; tapping the ringing one dismisses it.
    await tester.tap(find.text("Time's up"));
    expect(dismissed, 2);

    // The ringing timer (only) has a "Cancel timer" button that turns it off.
    expect(find.text('Cancel timer'), findsOneWidget);
    dismissed = null;
    await tester.tap(find.byKey(const ValueKey('cancel-timer-2')));
    expect(dismissed, 2);

    // Unmount so the overlay's periodic ticker is cancelled before the test ends.
    await tester.pumpWidget(const SizedBox.shrink());
  });

  testWidgets('compact badge shows name + countdown per timer', (tester) async {
    final now = DateTime(2026, 1, 1, 12, 0, 0);
    final timers = [
      TimerModel(
        id: 1,
        label: 'pasta',
        deadline: now.add(const Duration(minutes: 5)),
        total: const Duration(minutes: 10),
      ),
      TimerModel(id: 2, label: '', deadline: now, total: const Duration(minutes: 3), finished: true),
    ];

    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: CompactTimersOverlay(
          timers: timers,
          clock: () => now,
          onDismiss: (_) {},
        ),
      ),
    ));

    expect(find.text('pasta  5:00'), findsOneWidget);
    expect(find.text("Timer — time's up"), findsOneWidget);

    await tester.pumpWidget(const SizedBox.shrink());
  });
}
