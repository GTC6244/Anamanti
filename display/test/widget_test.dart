// Widget-level tests for the Phase 5 ambient UI.
//
// The native RustLib is not loaded in the plain `flutter test` host VM, so these
// tests drive the reactive UI with a synthetic engine stream (injected via
// [AssistantController]'s `startEngine` factory) and the offline
// [LocalPhotoSource] slideshow — no device or `.so` required. The on-device path
// (real engine) is covered by `integration_test/`.

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:ambient_display/src/engine/assistant_controller.dart';
import 'package:ambient_display/src/slideshow/photo_source.dart';
import 'package:ambient_display/src/ui/ambient_screen.dart';
import 'package:ambient_display/src/rust/api/engine.dart';

WakeWordConfig _testConfig() => WakeWordConfig(
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
      cameraProximity: false,
      proximityMotionThreshold: 0,
      proximityReleaseSecs: 0,
    );

WakeWordEvent _event(
  WakeWordEventKind kind, {
  String message = '',
  String transcript = '',
  String reply = '',
  String model = '',
  int timerId = 0,
  String timerLabel = '',
  int timerRemainingSecs = 0,
  bool present = false,
}) {
  return WakeWordEvent(
    kind: kind,
    message: message,
    device: '',
    deviceSampleRate: 0,
    channels: 0,
    rms: 0,
    score: 0,
    model: model,
    transcript: transcript,
    reply: reply,
    timerId: timerId,
    timerLabel: timerLabel,
    timerRemainingSecs: timerRemainingSecs,
    present: present,
  );
}

/// A harness that wires an [AmbientScreen] to a synthetic engine stream, and
/// tears everything down (unmount + dispose + cancel timers) so no pending timer
/// trips the end-of-test invariant.
class _Harness {
  _Harness()
      : controller = StreamController<WakeWordEvent>.broadcast(),
        slideshow = SlideshowController();

  final StreamController<WakeWordEvent> controller;
  final SlideshowController slideshow;
  late final AssistantController assistant = AssistantController(
    config: _testConfig(),
    startEngine: (_) => controller.stream,
  );

  Future<void> pump(WidgetTester tester) async {
    assistant.start();
    await slideshow.start();
    await tester.pumpWidget(MaterialApp(
      home: AmbientScreen(assistant: assistant, slideshow: slideshow),
    ));
    await tester.pump();
  }

  void add(WakeWordEvent e) => controller.add(e);

  /// Deliver queued stream events and rebuild. Two pumps are needed: the first
  /// runs the stream-delivery microtask (updating state + marking dirty), the
  /// second rebuilds the widget tree with the new state.
  Future<void> settle(WidgetTester tester) async {
    await tester.pump();
    await tester.pump();
  }

  Future<void> dispose(WidgetTester tester) async {
    // Unmount first so widget-owned timers (clock, animations) are cancelled.
    await tester.pumpWidget(const SizedBox.shrink());
    assistant.dispose();
    slideshow.dispose();
    await controller.close();
  }
}

void main() {
  testWidgets('idle ambient screen shows the wake-word hint', (tester) async {
    final h = _Harness();
    await h.pump(tester);

    expect(find.text('Say the wake word to begin'), findsOneWidget);

    await h.dispose(tester);
  });

  testWidgets('away mode shows the big centered clock and hides the rest',
      (tester) async {
    final h = _Harness();
    await h.pump(tester);

    double opacity(String key) =>
        tester.widget<AnimatedOpacity>(find.byKey(Key(key))).opacity;

    // Starts present (default): the small idle clock shows, the big away face is hidden.
    expect(opacity('idle-clock'), 1);
    expect(opacity('away-face'), 0);

    // Proximity reports the room is empty → away/off mode: only the big clock.
    h.add(_event(WakeWordEventKind.presence, present: false));
    await h.settle(tester);
    expect(opacity('away-face'), 1);
    expect(opacity('idle-clock'), 0);

    // Someone approaches → back to the full idle screen.
    h.add(_event(WakeWordEventKind.presence, present: true));
    await h.settle(tester);
    expect(opacity('away-face'), 0);
    expect(opacity('idle-clock'), 1);

    await h.dispose(tester);
  });

  testWidgets('a turn renders transcript then streams the reply token-by-token',
      (tester) async {
    final h = _Harness();
    await h.pump(tester);

    // Wake word → streaming → transcript.
    h.add(_event(WakeWordEventKind.detected, model: 'alexa'));
    h.add(_event(WakeWordEventKind.streaming));
    h.add(_event(WakeWordEventKind.transcript, transcript: 'what time is it'));
    await h.settle(tester);
    expect(find.text('what time is it'), findsOneWidget);

    // Reply tokens accumulate with a streaming caret.
    h.add(_event(WakeWordEventKind.replyToken, reply: 'It is '));
    h.add(_event(WakeWordEventKind.replyToken, reply: 'noon'));
    await h.settle(tester);
    expect(find.text('It is noon▌'), findsOneWidget);

    // Speaking phase, then the turn completes back to idle.
    h.add(_event(WakeWordEventKind.speaking));
    await h.settle(tester);
    expect(h.assistant.state.phase, TurnPhase.speaking);

    h.add(_event(WakeWordEventKind.disconnected, message: 'turn complete'));
    await h.settle(tester);
    expect(h.assistant.state.phase, TurnPhase.idle);
    expect(h.assistant.state.online, isTrue);

    await h.dispose(tester);
  });

  testWidgets(
      'reply text stays on screen while audio plays, then clears on speakingDone',
      (tester) async {
    final h = _Harness();
    await h.pump(tester);

    h.add(_event(WakeWordEventKind.detected, model: 'alexa'));
    h.add(_event(WakeWordEventKind.transcript, transcript: 'what time is it'));
    h.add(_event(WakeWordEventKind.replyToken, reply: 'It is noon'));
    h.add(_event(WakeWordEventKind.speaking));
    await h.settle(tester);
    expect(h.assistant.state.audioPlaying, isTrue);

    // The turn returns to idle while the reply audio is still draining from the
    // playback ring — the text must remain on screen and the panel stay open.
    h.add(_event(WakeWordEventKind.disconnected, message: 'turn complete'));
    await h.settle(tester);
    expect(h.assistant.state.phase, TurnPhase.idle);
    expect(h.assistant.state.displayActive, isTrue);
    expect(h.assistant.state.reply, 'It is noon');
    expect(find.text('It is noon'), findsOneWidget);
    expect(find.text('Speaking…'), findsOneWidget);

    // Audio finishes playing: only now is the text removed and the panel closed.
    h.add(_event(WakeWordEventKind.speakingDone));
    await h.settle(tester);
    expect(h.assistant.state.audioPlaying, isFalse);
    expect(h.assistant.state.displayActive, isFalse);
    expect(h.assistant.state.reply, isEmpty);
    expect(find.text('It is noon'), findsNothing);

    await h.dispose(tester);
  });

  testWidgets("a stale speakingDone does not wipe the next turn's text",
      (tester) async {
    final h = _Harness();
    await h.pump(tester);

    // A first reply starts speaking and the turn returns to idle (audio still
    // draining), then the user barges in with a fresh wake word.
    h.add(_event(WakeWordEventKind.speaking));
    h.add(_event(WakeWordEventKind.disconnected, message: 'turn complete'));
    await h.settle(tester);

    h.add(_event(WakeWordEventKind.detected, model: 'alexa'));
    h.add(_event(WakeWordEventKind.transcript, transcript: 'new question'));
    h.add(_event(WakeWordEventKind.replyToken, reply: 'fresh answer'));
    await h.settle(tester);
    expect(h.assistant.state.reply, 'fresh answer');

    // The previous reply's drain watcher fires late — it must not clear the new
    // turn's text.
    h.add(_event(WakeWordEventKind.speakingDone));
    await h.settle(tester);
    expect(h.assistant.state.reply, 'fresh answer');
    expect(h.assistant.state.audioPlaying, isFalse);

    await h.dispose(tester);
  });

  testWidgets('an unreachable host flags the offline indicator', (tester) async {
    final h = _Harness();
    await h.pump(tester);

    h.add(_event(WakeWordEventKind.started, message: 'mic'));
    h.add(_event(WakeWordEventKind.detected, model: 'alexa'));
    h.add(_event(WakeWordEventKind.disconnected,
        message: 'no Wyoming host: browse timed out'));
    await h.settle(tester);

    expect(h.assistant.state.online, isFalse);
    expect(find.text('Offline'), findsOneWidget);

    await h.dispose(tester);
  });
}
