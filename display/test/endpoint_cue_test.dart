// Verifies the local end-of-speech cue in AssistantController: once the mic level
// has stayed below the silence threshold for the configured window after speech,
// the phase flips to `processing` immediately — without waiting for the Mac's VAD +
// transcript. This is the on-device visual cue for "done speaking, now thinking".

import 'dart:async';

import 'package:ambient_display/src/engine/assistant_controller.dart';
import 'package:ambient_display/src/rust/api/engine.dart';
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

WakeWordEvent _ev(WakeWordEventKind kind, {double rms = 0, String model = ''}) =>
    WakeWordEvent(
      kind: kind,
      message: '',
      device: '',
      deviceSampleRate: 0,
      channels: 0,
      rms: rms,
      score: 0,
      model: model,
      transcript: '',
      reply: '',
      timerId: 0,
      timerLabel: '',
      timerRemainingSecs: 0,
    );

void main() {
  test('flips to processing after trailing silence once speech was seen', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    var now = DateTime(2026, 1, 1, 12, 0, 0);
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      endpointCueEnabled: true,
      endpointSilence: const Duration(milliseconds: 600),
      endpointRmsThreshold: 0.012,
      clock: () => now,
    )..start();
    addTearDown(controller.dispose);

    // Wake word → listening; then the mic is streaming.
    engine.add(_ev(WakeWordEventKind.detected, model: 'hey_jarvis'));
    engine.add(_ev(WakeWordEventKind.streaming));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.phase, TurnPhase.listening);

    // Speech-level audio arrives → still listening (speech seen).
    engine.add(_ev(WakeWordEventKind.level, rms: 0.05));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.phase, TurnPhase.listening);

    // A short silence (< window) does not yet endpoint.
    now = now.add(const Duration(milliseconds: 300));
    engine.add(_ev(WakeWordEventKind.level, rms: 0.001));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.phase, TurnPhase.listening);

    // Enough trailing silence elapses → flip to processing (the local cue).
    now = now.add(const Duration(milliseconds: 400));
    engine.add(_ev(WakeWordEventKind.level, rms: 0.001));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.phase, TurnPhase.processing);

    // The authoritative transcript still advances to thinking.
    engine.add(_ev(WakeWordEventKind.transcript));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.phase, TurnPhase.thinking);
  });

  test('does not endpoint before any speech is seen', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    var now = DateTime(2026, 1, 1, 12, 0, 0);
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      endpointSilence: const Duration(milliseconds: 600),
      endpointRmsThreshold: 0.012,
      clock: () => now,
    )..start();
    addTearDown(controller.dispose);

    engine.add(_ev(WakeWordEventKind.detected, model: 'hey_jarvis'));
    engine.add(_ev(WakeWordEventKind.streaming));
    // Only silence, never any speech-level audio: must stay listening (matches the
    // Mac's no-speech fallback rather than falsely claiming we're processing).
    for (var i = 0; i < 5; i++) {
      now = now.add(const Duration(milliseconds: 300));
      engine.add(_ev(WakeWordEventKind.level, rms: 0.001));
      await Future<void>.delayed(Duration.zero);
    }
    expect(controller.state.phase, TurnPhase.listening);
  });

  test('cue can be disabled', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    var now = DateTime(2026, 1, 1, 12, 0, 0);
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      endpointCueEnabled: false,
      endpointSilence: const Duration(milliseconds: 600),
      endpointRmsThreshold: 0.012,
      clock: () => now,
    )..start();
    addTearDown(controller.dispose);

    engine.add(_ev(WakeWordEventKind.detected, model: 'hey_jarvis'));
    engine.add(_ev(WakeWordEventKind.streaming));
    engine.add(_ev(WakeWordEventKind.level, rms: 0.05)); // speech
    now = now.add(const Duration(milliseconds: 1000));
    engine.add(_ev(WakeWordEventKind.level, rms: 0.001)); // long silence
    await Future<void>.delayed(Duration.zero);
    // With the cue off, we wait for the transcript instead of flipping locally.
    expect(controller.state.phase, TurnPhase.listening);
  });
}
