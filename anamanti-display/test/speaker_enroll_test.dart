// Guided voice-enrollment screen (SpeakerEnrollmentPlan.md Phase 2): drives the
// name → capture → commit → verify → done flow with a fake orchestrator client and
// a controller fed synthetic `enrollResult` engine events.

import 'dart:async';
import 'dart:convert';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:anamanti_display/src/settings/orchestrator_client.dart';
import 'package:anamanti_display/src/ui/people_screen.dart';
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

WakeWordEvent _enroll(Map<String, dynamic> data) => WakeWordEvent(
  kind: WakeWordEventKind.enrollResult,
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
  recipeAction: '',
  enrollJson: jsonEncode(data),
);

void main() {
  testWidgets('enrollment: name → samples → commit → verify → done', (
    tester,
  ) async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
    )..start();
    addTearDown(() {
      controller.dispose();
      engine.close();
    });
    final client = FakeOrchestratorClient();
    final contextCalls = <String>[];

    await tester.pumpWidget(
      MaterialApp(
        home: SpeakerEnrollScreen(
          client: client,
          controller: controller,
          wakeWord: 'hey jarvis',
          setEnrollContext:
              ({
                required bool active,
                required String session,
                required String name,
                required int sampleIndex,
                required String phase,
              }) => contextCalls.add('$active/$phase/$sampleIndex'),
        ),
      ),
    );

    // Name step → Start.
    expect(find.byKey(const Key('enroll-start')), findsOneWidget);
    await tester.enterText(find.byKey(const Key('enroll-name')), 'Dana');
    await tester.pump();
    await tester.tap(find.byKey(const Key('enroll-start')));
    await tester.pump();

    // Capture step: a phrase is shown and the context was stamped (sample phase).
    expect(find.byKey(const Key('enroll-phrase')), findsOneWidget);
    expect(
      contextCalls.where((c) => c.startsWith('true/sample')).isNotEmpty,
      true,
    );

    // Finish is disabled until enough samples.
    final finishBtn = tester.widget<FilledButton>(
      find.byKey(const Key('enroll-finish')),
    );
    expect(finishBtn.onPressed, isNull);

    // Three accepted samples arrive from the engine.
    for (var n = 1; n <= 3; n++) {
      engine.add(
        _enroll({
          'ok': true,
          'phase': 'sample',
          'accepted': true,
          'samples': n,
        }),
      );
      await tester.pump();
    }
    final finishReady = tester.widget<FilledButton>(
      find.byKey(const Key('enroll-finish')),
    );
    expect(finishReady.onPressed, isNotNull);

    // Commit.
    await tester.tap(find.byKey(const Key('enroll-finish')));
    await tester.pump(); // committing → enrollFinish resolves
    await tester.pump();
    expect(client.enrollFinishCalls.single['name'], 'Dana');

    // Verify step is up; a verify result completes the flow.
    engine.add(
      _enroll({
        'ok': true,
        'phase': 'verify',
        'confidence': 0.82,
        'name': 'Dana',
      }),
    );
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 20));
    expect(find.byKey(const Key('enroll-done-title')), findsOneWidget);
    expect(find.textContaining('Recognized Dana'), findsOneWidget);
  });

  testWidgets('a rejected sample shows the reason and keeps capturing', (
    tester,
  ) async {
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
        ),
      ),
    );
    await tester.enterText(find.byKey(const Key('enroll-name')), 'Sam');
    await tester.pump();
    await tester.tap(find.byKey(const Key('enroll-start')));
    await tester.pump();

    engine.add(
      _enroll({
        'ok': true,
        'phase': 'sample',
        'accepted': false,
        'samples': 0,
        'message': 'too short — please speak a little longer',
      }),
    );
    await tester.pump();
    expect(
      find.textContaining('too short — please speak a little longer'),
      findsOneWidget,
    );
    // Still on the capture step (finish disabled).
    final finish = tester.widget<FilledButton>(
      find.byKey(const Key('enroll-finish')),
    );
    expect(finish.onPressed, isNull);
  });

  testWidgets(
    'an existing household member can be picked instead of typing a name',
    (tester) async {
      final engine = StreamController<WakeWordEvent>.broadcast();
      final controller = AssistantController(
        config: _cfg(),
        startEngine: (_) => engine.stream,
      )..start();
      addTearDown(() {
        controller.dispose();
        engine.close();
      });
      final client = FakeOrchestratorClient(
        householdMembers: ['Dana', 'Sam'],
        speakers: const [
          SpeakerView(
            id: 'spk-1',
            name: 'Sam',
            labeled: true,
            samples: 4,
            createdAt: 0,
          ),
        ],
      );
      final contextNames = <String>[];

      await tester.pumpWidget(
        MaterialApp(
          home: SpeakerEnrollScreen(
            client: client,
            controller: controller,
            setEnrollContext:
                ({
                  required bool active,
                  required String session,
                  required String name,
                  required int sampleIndex,
                  required String phase,
                }) {
                  if (active) contextNames.add(name);
                },
          ),
        ),
      );
      await tester.pumpAndSettle();

      // Both members are offered; Start is disabled until someone is chosen.
      expect(find.byKey(const Key('enroll-member-Dana')), findsOneWidget);
      expect(find.byKey(const Key('enroll-member-Sam')), findsOneWidget);
      expect(
        tester
            .widget<FilledButton>(find.byKey(const Key('enroll-start')))
            .onPressed,
        isNull,
      );

      // Sam already has a voiceprint → the hint says samples will be added to it.
      await tester.tap(find.byKey(const Key('enroll-member-Sam')));
      await tester.pump();
      expect(find.textContaining('already has a voiceprint'), findsOneWidget);

      // Dana has none → linked to the household member.
      await tester.tap(find.byKey(const Key('enroll-member-Dana')));
      await tester.pump();
      expect(find.textContaining('linked to Dana'), findsOneWidget);

      await tester.tap(find.byKey(const Key('enroll-start')));
      await tester.pump();
      expect(find.text('Enrolling Dana'), findsOneWidget);
      expect(contextNames.last, 'Dana');

      for (var n = 1; n <= 3; n++) {
        engine.add(
          _enroll({
            'ok': true,
            'phase': 'sample',
            'accepted': true,
            'samples': n,
          }),
        );
        await tester.pump();
      }
      await tester.tap(find.byKey(const Key('enroll-finish')));
      await tester.pump();
      await tester.pump();
      expect(client.enrollFinishCalls.single['name'], 'Dana');
    },
  );

  testWidgets('a typed name matching a member uses the member\'s spelling', (
    tester,
  ) async {
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
          client: FakeOrchestratorClient(householdMembers: ['Dana Smith']),
          controller: controller,
        ),
      ),
    );
    await tester.pumpAndSettle();

    await tester.enterText(find.byKey(const Key('enroll-name')), 'dana smith');
    await tester.pump();
    expect(find.textContaining('linked to Dana Smith'), findsOneWidget);

    // A brand-new name is flagged as a new household member.
    await tester.enterText(find.byKey(const Key('enroll-name')), 'Alex');
    await tester.pump();
    expect(
      find.textContaining('added to the household as a new person'),
      findsOneWidget,
    );

    await tester.enterText(find.byKey(const Key('enroll-name')), 'dana smith');
    await tester.pump();
    await tester.tap(find.byKey(const Key('enroll-start')));
    await tester.pump();
    expect(find.text('Enrolling Dana Smith'), findsOneWidget);
  });

  testWidgets(
    'People ⋮ → "Add voice samples" guides phrases and saves into that person',
    (tester) async {
      final engine = StreamController<WakeWordEvent>.broadcast();
      final controller = AssistantController(
        config: _cfg(),
        startEngine: (_) => engine.stream,
      )..start();
      addTearDown(() {
        controller.dispose();
        engine.close();
      });
      final client = FakeOrchestratorClient(
        speakers: const [
          SpeakerView(
            id: 'spk-1',
            name: 'Sam',
            labeled: true,
            samples: 4,
            createdAt: 0,
          ),
          SpeakerView(id: 'spk-2', labeled: false, samples: 2, createdAt: 1),
        ],
      );
      final contextCalls = <String>[];

      await tester.pumpWidget(
        MaterialApp(
          home: PeopleScreen(
            client: client,
            controller: controller,
            wakeWord: 'hey jarvis',
            setEnrollContext:
                ({
                  required bool active,
                  required String session,
                  required String name,
                  required int sampleIndex,
                  required String phase,
                }) => contextCalls.add('$active/$phase/$name'),
          ),
        ),
      );
      await tester.pumpAndSettle();

      await tester.tap(find.byKey(const ValueKey('speaker-menu-spk-1')));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Add voice samples'));
      await tester.pumpAndSettle();

      // Straight to capture (no name step), with the full phrase guide shown.
      expect(find.text('Add voice samples'), findsOneWidget); // app bar
      expect(find.text('More samples for Sam'), findsOneWidget);
      expect(find.byKey(const Key('enroll-name')), findsNothing);
      expect(contextCalls.first, 'true/sample/Sam');
      for (var i = 0; i < kMinEnrollSamples; i++) {
        expect(find.byKey(Key('enroll-guide-$i')), findsOneWidget);
      }
      // A re-train reads different sentences than a first enrollment.
      expect(
        tester.widget<Text>(find.byKey(const Key('enroll-phrase'))).data,
        kEnrollPhrases[kMinEnrollSamples],
      );

      for (var n = 1; n <= 3; n++) {
        engine.add(
          _enroll({
            'ok': true,
            'phase': 'sample',
            'accepted': true,
            'samples': n,
          }),
        );
        await tester.pump();
      }
      // The current phrase advanced with each capture.
      expect(
        tester.widget<Text>(find.byKey(const Key('enroll-phrase'))).data,
        kEnrollPhrases[(2 * kMinEnrollSamples) % kEnrollPhrases.length],
      );

      await tester.tap(find.byKey(const Key('enroll-finish')));
      await tester.pump();
      await tester.pump();
      expect(client.enrollFinishCalls.single['speakerId'], 'spk-1');

      // Skip verify → back on People, refreshed with the extra samples.
      await tester.tap(find.byKey(const Key('enroll-skip-verify')));
      await tester.pumpAndSettle();
      expect(find.text('7 voice clips'), findsOneWidget);
    },
  );

  testWidgets('an unnamed speaker can get samples too', (tester) async {
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
          speakerId: 'spk-9',
          personLabel: 'Speaker 2',
        ),
      ),
    );
    expect(find.text('More samples for Speaker 2'), findsOneWidget);
  });
}
