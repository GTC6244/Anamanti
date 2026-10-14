// Live capture cues on the voice-enrollment screen: the user should always see
// whether the wake word was heard, whether the mic is recording, whether the sample
// is being checked, and how it went — driven by synthetic engine events.

import 'dart:async';
import 'dart:convert';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:anamanti_display/src/ui/enroll_capture_status.dart';
import 'package:anamanti_display/src/ui/speaker_enroll_screen.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/fake_orchestrator_client.dart';

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
  String message = '',
  double rms = 0,
  String model = '',
  String enrollJson = '',
}) => WakeWordEvent(
  kind: kind,
  message: message,
  device: 'mic',
  deviceSampleRate: 16000,
  channels: 1,
  rms: rms,
  score: 0.9,
  avgScore: 0,
  threshold: 0.5,
  gainDb: 0,
  model: model,
  transcript: '',
  reply: '',
  timerId: 0,
  timerLabel: '',
  timerRemainingSecs: 0,
  present: false,
  recipeJson: '',
  weatherJson: '',
  placeJson: '',
  recipeAction: '',
  enrollJson: enrollJson,
);

/// Deliver queued engine events, then render the frame they scheduled.
Future<void> _flush(WidgetTester tester) async {
  await tester.pump();
  await tester.pump();
}

String _title(WidgetTester tester) =>
    tester.widget<Text>(find.byKey(const Key('enroll-cue-title'))).data!;

void main() {
  group('enrollCueFor', () {
    const ready = AssistantState(captureReady: true, online: true);

    test('maps the turn phase to a cue', () {
      expect(enrollCueFor(const AssistantState()), EnrollCue.micStarting);
      expect(
        enrollCueFor(const AssistantState(captureReady: true)),
        EnrollCue.offline,
      );
      expect(enrollCueFor(ready), EnrollCue.waiting);
      expect(
        enrollCueFor(ready.copyWith(phase: TurnPhase.listening)),
        EnrollCue.listening,
      );
      expect(
        enrollCueFor(ready.copyWith(phase: TurnPhase.connecting)),
        EnrollCue.listening,
        reason: 'the mic still buffers while dialing — keep reading',
      );
      expect(
        enrollCueFor(ready.copyWith(phase: TurnPhase.processing)),
        EnrollCue.checking,
      );
      expect(
        enrollCueFor(ready.copyWith(phase: TurnPhase.error)),
        EnrollCue.restarting,
      );
    });

    test('a result flash shows until a new capture starts', () {
      expect(enrollCueFor(ready, lastResultAccepted: true), EnrollCue.accepted);
      expect(
        enrollCueFor(ready, lastResultAccepted: false),
        EnrollCue.rejected,
      );
      expect(
        enrollCueFor(
          ready.copyWith(phase: TurnPhase.listening),
          lastResultAccepted: true,
        ),
        EnrollCue.listening,
      );
    });
  });

  testWidgets('capture shows wake → recording → checking → result → waiting', (
    tester,
  ) async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    var now = DateTime(2026, 10, 14, 12);
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      clock: () => now,
    )..start();
    addTearDown(() {
      controller.dispose();
      engine.close();
    });

    await tester.pumpWidget(
      MaterialApp(
        home: SpeakerEnrollScreen(
          client: FakeOrchestratorClient(),
          controller: controller,
          wakeWord: 'hey jarvis',
          speakerId: 'spk-1',
          personLabel: 'Sam',
        ),
      ),
    );
    await _flush(tester);
    expect(_title(tester), 'Starting microphone…');

    // Mic up + a completed turn earlier means the Mac is reachable.
    engine.add(_ev(WakeWordEventKind.started));
    engine.add(_ev(WakeWordEventKind.disconnected, message: 'turn complete'));
    await _flush(tester);
    expect(_title(tester), 'Waiting for "hey jarvis"');
    expect(find.byKey(const Key('enroll-mic-meter')), findsNothing);

    // Wake word heard → recording, with the live meter.
    engine.add(_ev(WakeWordEventKind.detected, model: 'hey_jarvis'));
    await _flush(tester);
    expect(_title(tester), 'Listening — read the phrase now');
    expect(find.textContaining('Heard "hey jarvis"'), findsOneWidget);
    expect(find.byKey(const Key('enroll-mic-meter')), findsOneWidget);
    engine.add(_ev(WakeWordEventKind.level, rms: 0.05));
    await _flush(tester);
    expect(find.byKey(const Key('enroll-mic-meter')), findsOneWidget);

    // Still recording while the socket dials, and once it streams.
    engine.add(_ev(WakeWordEventKind.connecting));
    await _flush(tester);
    expect(_title(tester), 'Listening — read the phrase now');
    engine.add(_ev(WakeWordEventKind.streaming));
    engine.add(_ev(WakeWordEventKind.level, rms: 0.05)); // speaking
    await _flush(tester);

    // Trailing silence → the controller's on-device end-of-speech cue → checking.
    now = now.add(const Duration(seconds: 1));
    engine.add(_ev(WakeWordEventKind.level, rms: 0.0));
    await _flush(tester);
    expect(_title(tester), 'Checking that sample…');

    // The Core accepts it, then the turn ends.
    engine.add(
      _ev(
        WakeWordEventKind.enrollResult,
        enrollJson: jsonEncode({
          'ok': true,
          'phase': 'sample',
          'accepted': true,
          'samples': 1,
        }),
      ),
    );
    engine.add(_ev(WakeWordEventKind.disconnected, message: 'turn complete'));
    await _flush(tester);
    expect(_title(tester), 'Sample captured');

    // The result flash fades back to waiting.
    await tester.pump(kEnrollResultFlash + const Duration(milliseconds: 50));
    expect(_title(tester), 'Waiting for "hey jarvis"');

    // A rejected sample says so; the next wake word clears it immediately.
    engine.add(
      _ev(
        WakeWordEventKind.enrollResult,
        enrollJson: jsonEncode({
          'ok': true,
          'phase': 'sample',
          'accepted': false,
          'samples': 1,
          'message': 'too short — please speak a little longer',
        }),
      ),
    );
    await _flush(tester);
    expect(_title(tester), 'Sample not used');
    expect(
      find.textContaining('too short — please speak a little longer'),
      findsOneWidget,
    );
    engine.add(_ev(WakeWordEventKind.detected, model: 'hey_jarvis'));
    await _flush(tester);
    expect(_title(tester), 'Listening — read the phrase now');
  });

  testWidgets('an unreachable Mac is called out', (tester) async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
    )..start();
    addTearDown(() {
      controller.dispose();
      engine.close();
    });
    await tester.pumpWidget(
      MaterialApp(
        home: SpeakerEnrollScreen(
          client: FakeOrchestratorClient(),
          controller: controller,
          speakerId: 'spk-1',
          personLabel: 'Sam',
        ),
      ),
    );
    engine.add(_ev(WakeWordEventKind.started));
    engine.add(_ev(WakeWordEventKind.disconnected, message: 'no host found'));
    await _flush(tester);
    expect(_title(tester), "Can't reach the assistant");
  });
}
