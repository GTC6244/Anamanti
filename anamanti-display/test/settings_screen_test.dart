// Widget tests for the Settings screen (plans/SettingsReorgPlan.md).
//
// The screen mixes device-local settings (persisted via [SettingsStore]) with
// Core-managed settings (a [FakeOrchestratorClient], no native library). An
// in-memory store keeps `pumpAndSettle` reliable (fake-async can't drive real file
// IO); the real file round trip is covered by `settings_store_test.dart`.
//
// Layout: a side rail of sections (`settings-rail-<section>`), each with tabs
// (`settings-tab-<id>`). Changes apply immediately after a short debounce — there
// is no Save button — so tests change a control, then `settle` past the debounce.
// Every test runs at the Echo Show's real 960×600 dp size.

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:anamanti_display/src/engine/night_schedule.dart';
import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:anamanti_display/src/settings/app_settings.dart';
import 'package:anamanti_display/src/settings/orchestrator_client.dart';
import 'package:anamanti_display/src/ui/settings_screen.dart';

import 'support/display_size.dart';
import 'support/fake_orchestrator_client.dart';
import 'support/in_memory_settings_store.dart';
import 'support/real_fonts.dart';

/// Records what Settings hands the app.
class _Harness {
  _Harness({FakeOrchestratorClient? client, InMemorySettingsStore? store})
    : client = client ?? FakeOrchestratorClient(),
      store = store ?? InMemorySettingsStore();

  final FakeOrchestratorClient client;
  final InMemorySettingsStore store;

  /// Replaces the default "record it" apply (e.g. to make an apply fail).
  FutureOr<void> Function(AppSettings next)? onApplied;

  /// Clients for re-pinned Cores, by key (Settings → Device → Connection).
  final Map<String, FakeOrchestratorClient> coreClients =
      <String, FakeOrchestratorClient>{};
  final List<AppSettings> applied = <AppSettings>[];
  final List<List<double>> liveTunes = <List<double>>[];

  AppSettings? get last => applied.isEmpty ? null : applied.last;

  Future<void> pump(
    WidgetTester tester, {
    AppSettings initial = const AppSettings(),
    Duration localDebounce = const Duration(milliseconds: 600),
  }) async {
    useEchoShowDisplay(tester);
    await tester.pumpWidget(
      MaterialApp(
        home: SettingsScreen(
          initial: initial,
          store: store,
          client: client,
          onApplied: (next) {
            applied.add(next);
            return onApplied?.call(next);
          },
          clientForCore: (key) =>
              coreClients.putIfAbsent(key, () => FakeOrchestratorClient()),
          onLiveTune: (g, t, a) => liveTunes.add([g, t, a]),
          localDebounce: localDebounce,
        ),
      ),
    );
    await tester.pumpAndSettle();
  }
}

/// Select a rail section and (optionally) one of its tabs.
Future<void> open(WidgetTester tester, String section, [String? tab]) async {
  await tester.tap(find.byKey(Key('settings-rail-$section')));
  await tester.pumpAndSettle();
  if (tab != null) {
    await tester.tap(find.byKey(Key('settings-tab-$tab')));
    await tester.pumpAndSettle();
  }
}

/// Let pending debounced applies fire and complete.
Future<void> settle(WidgetTester tester) async {
  await tester.pump(const Duration(seconds: 1));
  await tester.pumpAndSettle();
}

/// Pick [item] from the dropdown with key [key].
Future<void> pickDropdown(WidgetTester tester, String key, String item) async {
  await tester.tap(find.byKey(Key(key)));
  await tester.pumpAndSettle();
  await tester.tap(find.text(item).last);
  await tester.pumpAndSettle();
}

void main() {
  testWidgets('opens on Assistant with the Core settings loaded', (
    tester,
  ) async {
    final h = _Harness();
    await h.pump(tester);

    expect(h.client.fetchCount, 1);
    expect(
      find.byKey(const Key('settings-page-assistant-brain')),
      findsOneWidget,
    );
    final backend = tester.widget<SegmentedButton<String>>(
      find.byKey(const Key('settings-backend')),
    );
    expect(backend.selected, {'ollama'});
    // No Save button any more.
    expect(find.byKey(const Key('settings-save')), findsNothing);
  });

  testWidgets('a device-local change applies + persists without Save', (
    tester,
  ) async {
    final h = _Harness();
    await h.pump(tester);

    await open(tester, 'listening');
    await pickDropdown(tester, 'settings-wakeword', 'alexa');
    await settle(tester);

    expect(h.applied, hasLength(1));
    expect(h.last!.wakeWord, 'alexa');
    expect(h.store.value.wakeWord, 'alexa');
    // Nothing Core-managed changed, so nothing was sent to the Mac.
    expect(h.client.applyCalls, isEmpty);
    expect(find.text('Saved'), findsOneWidget);
  });

  testWidgets('changing the backend is sent to the Core', (tester) async {
    final h = _Harness();
    await h.pump(tester);

    await tester.tap(find.text('Claude'));
    await settle(tester);

    expect(h.client.applyCalls.single['llmBackend'], 'anthropic');
    // Device-local settings untouched.
    expect(h.applied, isEmpty);
  });

  testWidgets('quick successive Core changes coalesce into one apply', (
    tester,
  ) async {
    final h = _Harness();
    await h.pump(tester);

    // Pick Claude, then Subscription auth, within the debounce window.
    await tester.tap(find.text('Claude'));
    await tester.pump();
    expect(find.byKey(const Key('settings-anthropic-auth')), findsOneWidget);
    await tester.tap(find.text('Subscription'));
    await settle(tester);

    expect(h.client.applyCalls, hasLength(1));
    expect(h.client.applyCalls.single['llmBackend'], 'anthropic');
    expect(h.client.applyCalls.single['anthropicAuth'], 'subscription');
  });

  testWidgets(
    'cloud backend shows a model dropdown and sends the picked model',
    (tester) async {
      final h = _Harness(
        client: FakeOrchestratorClient(
          models: const [
            ModelOption(
              provider: 'anthropic',
              id: 'claude-opus-5',
              label: 'Claude Opus 5',
            ),
            ModelOption(
              provider: 'anthropic',
              id: 'claude-sonnet-5',
              label: 'Claude Sonnet 5',
            ),
            ModelOption(
              provider: 'openai',
              id: 'gpt-4o-mini',
              label: 'gpt-4o-mini',
            ),
          ],
        ),
      );
      await h.pump(tester);

      await tester.tap(find.text('Claude'));
      await tester.pumpAndSettle();
      await tester.tap(find.byKey(const Key('settings-model')));
      await tester.pumpAndSettle();
      expect(find.text('Claude Sonnet 5').last, findsOneWidget);
      expect(find.text('gpt-4o-mini'), findsNothing);
      await tester.tap(find.text('Claude Sonnet 5').last);
      await settle(tester);

      expect(h.client.applyCalls.last['llmBackend'], 'anthropic');
      expect(h.client.applyCalls.last['llmModel'], 'claude-sonnet-5');
    },
  );

  testWidgets(
    'voice dropdown lists installed voices and sends the picked voice',
    (tester) async {
      final h = _Harness(
        client: FakeOrchestratorClient(
          voices: const [
            VoiceOption(
              name: 'en_US-amy-medium',
              label: 'amy (medium)',
              language: 'en_US',
            ),
            VoiceOption(
              name: 'en_US-lessac-medium',
              label: 'lessac (medium)',
              language: 'en_US',
            ),
          ],
        ),
      );
      await h.pump(tester);

      await tester.tap(find.byKey(const Key('settings-voice')));
      await tester.pumpAndSettle();
      expect(find.text('Server default').last, findsOneWidget);
      await tester.tap(find.text('amy (medium) · en_US').last);
      await settle(tester);

      expect(h.client.applyCalls.single['setTtsVoice'], true);
      expect(h.client.applyCalls.single['ttsVoice'], 'en_US-amy-medium');
    },
  );

  testWidgets(
    'free-text voice applies when editing is done, not per keystroke',
    (tester) async {
      final h = _Harness(
        client: FakeOrchestratorClient(voices: const <VoiceOption>[]),
      );
      await h.pump(tester);

      await tester.enterText(
        find.byKey(const Key('settings-voice')),
        'en_GB-alan-medium',
      );
      await settle(tester);
      expect(h.client.applyCalls, isEmpty); // still editing

      await tester.testTextInput.receiveAction(TextInputAction.done);
      await settle(tester);
      expect(h.client.applyCalls.single['ttsVoice'], 'en_GB-alan-medium');
    },
  );

  testWidgets('a failed Core apply reverts the control and shows why', (
    tester,
  ) async {
    final h = _Harness();
    await h.pump(tester);
    h.client.throwOnApply = true;

    await tester.tap(find.text('Claude'));
    await settle(tester);

    expect(h.client.applyCalls, hasLength(1));
    final backend = tester.widget<SegmentedButton<String>>(
      find.byKey(const Key('settings-backend')),
    );
    expect(backend.selected, {'ollama'}); // reverted
    expect(find.textContaining('change undone'), findsOneWidget);
  });

  testWidgets('a rejected Core apply reverts and surfaces the Core message', (
    tester,
  ) async {
    final h = _Harness();
    await h.pump(tester);
    h.client.rejectApply = 'no such backend';

    await tester.tap(find.text('OpenAI'));
    await settle(tester);

    final backend = tester.widget<SegmentedButton<String>>(
      find.byKey(const Key('settings-backend')),
    );
    expect(backend.selected, {'ollama'});
    expect(find.textContaining('no such backend'), findsOneWidget);
  });

  testWidgets(
    'offline Core: assistant controls disabled, device settings apply',
    (tester) async {
      final h = _Harness(client: FakeOrchestratorClient(throwOnFetch: true));
      await h.pump(tester);

      expect(find.text('Assistant offline'), findsOneWidget);
      final backend = tester.widget<SegmentedButton<String>>(
        find.byKey(const Key('settings-backend')),
      );
      expect(backend.onSelectionChanged, isNull);

      await open(tester, 'display', 'screen');
      await tester.tap(find.byKey(const Key('settings-dim-delay-60')));
      await settle(tester);

      expect(h.last!.dimDelaySecs, 60);
      expect(h.client.applyCalls, isEmpty);
    },
  );

  testWidgets('Core picker lists discovered Cores and persists the selection', (
    tester,
  ) async {
    final h = _Harness(
      client: FakeOrchestratorClient(
        orchestrators: const [
          OrchestratorOption(
            key: 'mac-mini',
            name: 'Mac Mini',
            host: '192.168.1.10',
            port: 10700,
          ),
          OrchestratorOption(
            key: 'test-mac',
            name: 'Test Mac',
            host: '192.168.1.11',
            port: 10700,
          ),
        ],
      ),
    );
    await h.pump(tester);

    await open(tester, 'device', 'connection');
    await tester.tap(find.byKey(const Key('settings-orchestrator')));
    await tester.pumpAndSettle();
    expect(find.text('Auto (first available)').last, findsOneWidget);
    await tester.tap(find.text('Test Mac · 192.168.1.11').last);
    await settle(tester);

    expect(h.last!.orchestratorKey, 'test-mac');
    expect(h.store.value.orchestratorKey, 'test-mac');
  });

  testWidgets('Core picker stays usable (Auto) when the Core is offline', (
    tester,
  ) async {
    final h = _Harness(client: FakeOrchestratorClient(throwOnFetch: true));
    await h.pump(tester);

    await open(tester, 'device', 'connection');
    expect(find.text('Can’t reach the assistant'), findsOneWidget);
    await tester.tap(find.byKey(const Key('settings-orchestrator')));
    await tester.pumpAndSettle();
    expect(find.text('Auto (first available)').last, findsOneWidget);
  });

  testWidgets('device name applies when editing is done', (tester) async {
    final h = _Harness();
    await h.pump(tester, initial: const AppSettings(deviceId: 'anamanti-abc'));

    await open(tester, 'device', 'connection');
    expect(find.text('ID: anamanti-abc'), findsOneWidget);
    await tester.enterText(
      find.byKey(const Key('settings-device-name')),
      'Kitchen',
    );
    await settle(tester);
    expect(h.applied, isEmpty);

    await tester.testTextInput.receiveAction(TextInputAction.done);
    await settle(tester);
    expect(h.last!.deviceName, 'Kitchen');
  });

  testWidgets('dim-delay presets show the current value and apply on tap', (
    tester,
  ) async {
    final h = _Harness();
    await h.pump(tester, initial: const AppSettings(dimDelaySecs: 120));

    await open(tester, 'display', 'screen');
    final chip = tester.widget<ChoiceChip>(
      find.byKey(const Key('settings-dim-delay-120')),
    );
    expect(chip.selected, isTrue);
    expect(find.text('2m'), findsOneWidget);

    await tester.tap(find.byKey(const Key('settings-dim-delay-3600')));
    await settle(tester);
    expect(h.last!.dimDelaySecs, 3600);
    expect(h.store.value.dimDelaySecs, 3600);
  });

  testWidgets(
    'photo rotation presets show the current value and apply on tap',
    (tester) async {
      final h = _Harness();
      await h.pump(tester, initial: const AppSettings(slideIntervalSecs: 30));

      await open(tester, 'display', 'photos');
      expect(
        tester
            .widget<ChoiceChip>(
              find.byKey(const Key('settings-slide-interval-30')),
            )
            .selected,
        isTrue,
      );

      await tester.ensureVisible(
        find.byKey(const Key('settings-slide-interval-300')),
      );
      await tester.tap(find.byKey(const Key('settings-slide-interval-300')));
      await settle(tester);
      expect(h.last!.slideIntervalSecs, 300);
      expect(h.store.value.slideIntervalSecs, 300);
    },
  );

  testWidgets('an off-preset photo rotation shows the nearest preset', (
    tester,
  ) async {
    final h = _Harness();
    await h.pump(tester, initial: const AppSettings(slideIntervalSecs: 40));

    await open(tester, 'display', 'photos');
    expect(
      tester
          .widget<ChoiceChip>(
            find.byKey(const Key('settings-slide-interval-30')),
          )
          .selected,
      isTrue,
    );
  });

  testWidgets('night dimming: toggle + darkness picker apply and persist', (
    tester,
  ) async {
    final h = _Harness();
    await h.pump(tester);

    await open(tester, 'display', 'night');
    expect(find.text('Dim at night'), findsOneWidget);
    expect(find.text('Lights off means:'), findsOneWidget);

    // Pick a less strict darkness threshold.
    await tester.tap(find.byKey(const Key('settings-night-dark-20')));
    await settle(tester);
    expect(h.last!.nightDarkLux, 20.0);
    expect(h.store.value.nightDarkLux, 20.0);

    // Turn night dimming off: persisted, and the picker greys out.
    await tester.tap(find.byKey(const Key('settings-night-dim')));
    await settle(tester);
    expect(h.last!.nightDimEnabled, isFalse);
    expect(h.store.value.nightDimEnabled, isFalse);
    final chip = tester.widget<ChoiceChip>(
      find.byKey(const Key('settings-night-dark-8')),
    );
    expect(chip.onSelected, isNull);
  });

  testWidgets('covered camera: Clock mode by default, schedule enables hours', (
    tester,
  ) async {
    final h = _Harness();
    await h.pump(tester);

    await open(tester, 'display', 'night');
    expect(
      tester
          .widget<ChoiceChip>(find.byKey(const Key('settings-covered-clock')))
          .selected,
      isTrue,
    );
    // Night hours only matter for the schedule → disabled in Clock mode.
    expect(
      tester
          .widget<TextButton>(find.byKey(const Key('settings-night-start')))
          .onPressed,
      isNull,
    );
    expect(find.text('21:00'), findsOneWidget);
    expect(find.text('07:00'), findsOneWidget);

    await tester.tap(find.byKey(const Key('settings-covered-schedule')));
    await settle(tester);
    expect(h.last!.coveredCameraMode, CoveredCameraMode.schedule);
    expect(h.store.value.coveredCameraMode, CoveredCameraMode.schedule);
    expect(
      tester
          .widget<TextButton>(find.byKey(const Key('settings-night-start')))
          .onPressed,
      isNotNull,
    );
  });

  testWidgets('an off-preset dim delay shows the nearest preset', (
    tester,
  ) async {
    final h = _Harness();
    await h.pump(tester, initial: const AppSettings(dimDelaySecs: 280));

    await open(tester, 'display', 'screen');
    expect(
      tester
          .widget<ChoiceChip>(find.byKey(const Key('settings-dim-delay-300')))
          .selected,
      isTrue,
    );
  });

  testWidgets(
    'font-size slider renders as a percentage and applies on release',
    (tester) async {
      final h = _Harness();
      await h.pump(tester, initial: const AppSettings(fontScale: 1.3));

      await open(tester, 'display', 'screen');
      expect(find.text('130%'), findsOneWidget);

      await tester.drag(
        find.byKey(const Key('settings-font-scale')),
        const Offset(-600, 0),
      );
      await settle(tester);
      expect(h.last!.fontScale, kFontScaleMin);
    },
  );

  testWidgets('listening ring: toggle disables the dials and applies', (
    tester,
  ) async {
    final h = _Harness();
    await h.pump(tester, initial: const AppSettings(ringReactivity: 1.5));

    await open(tester, 'display', 'screen');
    expect(find.text('1.50×'), findsOneWidget);

    await tester.tap(find.byKey(const Key('settings-listening-ring')));
    await settle(tester);
    final slider = tester.widget<Slider>(
      find.byKey(const Key('settings-ring-reactivity')),
    );
    expect(slider.onChanged, isNull);

    expect(h.last!.listeningRingEnabled, isFalse);
    expect(h.store.value.ringReactivity, 1.5);
  });

  testWidgets('sensitivity sliders live-tune the engine while dragging', (
    tester,
  ) async {
    final h = _Harness();
    await h.pump(tester);

    await open(tester, 'listening', 'wakeWord');
    final gesture = await tester.startGesture(
      tester.getCenter(find.byKey(const Key('settings-active-threshold'))),
    );
    await gesture.moveBy(const Offset(80, 0));
    await tester.pump();
    // Pushed to the engine mid-drag, before anything is committed.
    expect(h.liveTunes, isNotEmpty);
    expect(h.applied, isEmpty);
    await gesture.up();
    await settle(tester);

    expect(h.applied, hasLength(1));
    expect(h.liveTunes.last[2], h.last!.activeThreshold);
    expect(h.last!.activeThreshold, isNot(const AppSettings().activeThreshold));
  });

  testWidgets('several quick device-local toggles apply once', (tester) async {
    final h = _Harness();
    await h.pump(tester);

    await open(tester, 'listening', 'microphone');
    await tester.tap(find.byKey(const Key('settings-platform-ns')));
    await tester.pump();
    await tester.tap(find.byKey(const Key('settings-platform-agc')));
    await tester.pump();
    await tester.tap(find.byKey(const Key('settings-platform-aec')));
    await settle(tester);

    expect(h.applied, hasLength(1));
    expect(h.last!.platformNs, isFalse);
    expect(h.last!.platformAgc, isFalse);
    expect(h.last!.platformAec, isTrue);
  });

  testWidgets('leaving Settings applies a change still waiting on its debounce', (
    tester,
  ) async {
    final h = _Harness();
    // A long debounce so the change is still pending when Settings closes.
    await h.pump(tester, localDebounce: const Duration(hours: 1));

    await open(tester, 'listening');
    // Fixed pumps: the "Saving…" spinner animates while the change is pending, so
    // pumpAndSettle would never settle.
    await tester.tap(find.byKey(const Key('settings-wakeword')));
    await tester.pump(const Duration(milliseconds: 500));
    await tester.tap(find.text('ok nabu').last);
    await tester.pump(const Duration(milliseconds: 500));
    expect(find.text('Saving…'), findsOneWidget);
    expect(h.applied, isEmpty);

    await tester.pumpWidget(const SizedBox());
    await tester.pump();
    expect(h.last!.wakeWord, 'ok_nabu');
    expect(h.store.value.wakeWord, 'ok_nabu');
  });

  testWidgets(
    'syncs Drive from the Core, enabling folder pick, applied at once',
    (tester) async {
      final h = _Harness(
        client: FakeOrchestratorClient(
          driveToken: const DriveTokenView(
            linked: true,
            configured: true,
            clientId: 'cid.apps',
            clientSecret: 'gocspx-secret',
            refreshToken: '1//refresh',
            folderIds: <String>['1AbC', '1XyZ'],
            scope: 'drive.readonly',
          ),
        ),
      );
      await h.pump(
        tester,
        initial: const AppSettings(photoSource: PhotoSourceKind.drive),
      );

      await open(tester, 'display', 'photos');
      expect(
        tester
            .widget<FilledButton>(find.byKey(const Key('settings-drive-pick')))
            .onPressed,
        isNull,
      );

      await tester.tap(find.byKey(const Key('settings-drive-sync')));
      await settle(tester);

      expect(find.text('1AbC, 1XyZ'), findsOneWidget);
      expect(
        tester
            .widget<FilledButton>(find.byKey(const Key('settings-drive-pick')))
            .onPressed,
        isNotNull,
      );
      expect(h.store.value.driveClientId, 'cid.apps');
      expect(h.store.value.driveClientSecret, 'gocspx-secret');
      expect(h.store.value.driveRefreshToken, '1//refresh');
      expect(h.store.value.driveLinked, isTrue);
      expect(h.store.value.driveFolderIds, <String>['1AbC', '1XyZ']);
    },
  );

  testWidgets('photo source switches by touch and applies', (tester) async {
    final h = _Harness();
    await h.pump(tester);

    await open(tester, 'display', 'photos');
    await tester.tap(find.byKey(const Key('settings-photos-drive')));
    await settle(tester);
    expect(h.last!.photoSource, PhotoSourceKind.drive);
    expect(find.byKey(const Key('settings-drive-sync')), findsOneWidget);
  });

  testWidgets(
    'VAD engine swaps to Silero, revealing the threshold, and applies',
    (tester) async {
      final h = _Harness(
        client: FakeOrchestratorClient(
          settings: const OrchestratorSettingsView(
            ok: true,
            message: 'ok',
            llmBackend: 'ollama',
            vadEngine: 'energy',
          ),
        ),
      );
      await h.pump(tester);

      await open(tester, 'listening', 'endOfSpeech');
      expect(
        find.byKey(const Key('settings-vad-silero-threshold')),
        findsNothing,
      );

      await tester.tap(find.text('Silero'));
      await tester.pumpAndSettle();
      expect(
        find.byKey(const Key('settings-vad-silero-threshold')),
        findsOneWidget,
      );
      await settle(tester);

      expect(h.client.applyCalls.single['vadEngine'], 'silero');
      expect(h.client.applyCalls.single['sileroThreshold'], isA<double>());
    },
  );

  testWidgets('Silero is the default when the Core does not report an engine', (
    tester,
  ) async {
    final h = _Harness(); // fake reports vadEngine ''
    await h.pump(tester);

    await open(tester, 'listening', 'endOfSpeech');
    final engine = tester.widget<SegmentedButton<String>>(
      find.byKey(const Key('settings-vad-engine')),
    );
    expect(engine.selected, {'silero'});
  });

  testWidgets('seeded Silero settings render the threshold slider', (
    tester,
  ) async {
    final h = _Harness(
      client: FakeOrchestratorClient(
        settings: const OrchestratorSettingsView(
          ok: true,
          message: 'ok',
          llmBackend: 'ollama',
          llmModel: 'llama3.2',
          vadEngine: 'silero',
          sileroThreshold: 0.4,
        ),
      ),
    );
    await h.pump(tester);

    await open(tester, 'listening', 'endOfSpeech');
    expect(find.text('0.40'), findsOneWidget);
  });

  testWidgets(
    'Capabilities shows cloud/local chips and a missing-key warning',
    (tester) async {
      final h = _Harness(
        client: FakeOrchestratorClient(
          functions: const [
            FunctionStatusView(
              category: 'Config',
              name: 'LLM',
              mode: 'cloud',
              provider: 'Anthropic (Claude)',
              model: 'claude-opus-5',
              keyRequired: true,
              keySet: true,
              note: '',
            ),
            FunctionStatusView(
              category: 'Tools',
              name: 'Places',
              mode: 'off',
              provider: 'Google Places',
              model: '',
              keyRequired: true,
              keySet: false, // required but unset → red "API key missing"
              note: '',
            ),
            FunctionStatusView(
              category: 'Memory',
              name: 'Embeddings',
              mode: 'local',
              provider: 'nomic-embed-text',
              model: '768-dim',
              keyRequired: false,
              keySet: false,
              note: '',
            ),
          ],
        ),
      );
      await h.pump(tester);

      // The Brain tab summarizes, and taps through to the Capabilities tab.
      expect(find.text('2 on · 1 key missing'), findsOneWidget);
      await tester.tap(find.byKey(const Key('settings-capabilities-summary')));
      await tester.pumpAndSettle();

      expect(
        find.byKey(const Key('settings-function-Config-LLM')),
        findsOneWidget,
      );
      expect(find.textContaining('claude-opus-5'), findsOneWidget);
      expect(find.text('Cloud'), findsOneWidget);
      expect(find.text('Local'), findsOneWidget);
      expect(find.text('API key missing'), findsOneWidget);
      expect(find.byIcon(Icons.key_off), findsOneWidget);
    },
  );

  testWidgets(
    'Capabilities shows an offline guard when the Mac is unreachable',
    (tester) async {
      final h = _Harness(client: FakeOrchestratorClient(throwOnFetch: true));
      await h.pump(tester);

      await open(tester, 'assistant', 'capabilities');
      expect(find.text('Assistant offline'), findsOneWidget);
      expect(find.text('Retry'), findsOneWidget);
    },
  );

  group('Advanced mode', () {
    testWidgets('off hides expert controls and the Microphone tab', (
      tester,
    ) async {
      final h = _Harness();
      await h.pump(tester, initial: const AppSettings(advancedMode: false));

      expect(find.byKey(const Key('settings-advanced-chip')), findsNothing);
      await open(tester, 'listening');
      expect(find.byKey(const Key('settings-tab-microphone')), findsNothing);
      expect(find.byKey(const Key('settings-smoothing')), findsNothing);
      expect(find.byKey(const Key('settings-threshold')), findsOneWidget);

      await open(tester, 'listening', 'endOfSpeech');
      expect(find.byKey(const Key('settings-vad-engine')), findsNothing);
      expect(find.byKey(const Key('settings-vad-silence')), findsOneWidget);

      await open(tester, 'display', 'screen');
      expect(find.byKey(const Key('settings-ring-attack')), findsNothing);
      expect(find.byKey(const Key('settings-ring-reactivity')), findsOneWidget);
    });

    testWidgets('turning it on from Maintenance reveals them', (tester) async {
      final h = _Harness();
      await h.pump(tester, initial: const AppSettings(advancedMode: false));

      await open(tester, 'device', 'maintenance');
      await tester.tap(find.byKey(const Key('settings-advanced-mode')));
      await settle(tester);

      expect(h.last!.advancedMode, isTrue);
      expect(find.byKey(const Key('settings-advanced-chip')), findsOneWidget);
      await open(tester, 'listening', 'microphone');
      expect(find.byKey(const Key('settings-use-audiorecord')), findsOneWidget);
    });

    testWidgets('turning it off while on the Microphone tab falls back', (
      tester,
    ) async {
      final h = _Harness();
      await h.pump(tester);

      await open(tester, 'listening', 'microphone');
      // Jump to Maintenance via the AppBar chip and switch Advanced off.
      await tester.tap(find.byKey(const Key('settings-advanced-chip')));
      await tester.pumpAndSettle();
      await tester.tap(find.byKey(const Key('settings-advanced-mode')));
      await settle(tester);

      await open(tester, 'listening');
      expect(
        find.byKey(const Key('settings-page-listening-wakeWord')),
        findsOneWidget,
      );
    });
  });

  testWidgets('Household buttons open their screens', (tester) async {
    final h = _Harness();
    await h.pump(tester);

    await open(tester, 'household');
    await tester.tap(find.byKey(const Key('settings-menu-pair-phone')));
    await tester.pumpAndSettle();
    expect(find.text('Link a phone'), findsWidgets);
  });

  testWidgets(
    'Audio Diagnostics follows the engine, fits one screen, and toggles its '
    'transcription-only context',
    (tester) async {
      useEchoShowDisplay(tester);
      final realFonts = await loadRealFonts(tester);
      final engine = StreamController<WakeWordEvent>.broadcast();
      final controller = AssistantController(
        config: _cfg(),
        startEngine: (_) => engine.stream,
      )..start();
      addTearDown(() async {
        controller.dispose();
        await engine.close();
      });
      final listenable = ValueNotifier<AssistantController?>(controller);
      addTearDown(listenable.dispose);
      final active = <bool>[];

      await tester.pumpWidget(
        MaterialApp(
          home: SettingsScreen(
            initial: const AppSettings(),
            store: InMemorySettingsStore(),
            client: FakeOrchestratorClient(),
            onApplied: (_) {},
            assistant: listenable,
            onDiagnosticsActive: active.add,
          ),
        ),
      );
      await tester.pumpAndSettle();

      await open(tester, 'diagnostics');
      expect(active, [true]);

      // Worst case for height: a full detection history and a long transcript.
      for (var i = 0; i < 6; i++) {
        engine.add(_event(WakeWordEventKind.detected, score: 0.8));
        await tester.pump();
      }
      engine.add(
        _event(
          WakeWordEventKind.transcript,
          transcript:
              'what is the weather going to be like tomorrow afternoon in '
              'the city, and should I bring an umbrella with me when I go out',
        ),
      );
      await tester.pumpAndSettle();

      // An engine restart replaces the controller while the page is up: the
      // transcription-only context must stay set (not be toggled off and on).
      final engine2 = StreamController<WakeWordEvent>.broadcast();
      final controller2 = AssistantController(
        config: _cfg(),
        startEngine: (_) => engine2.stream,
      )..start();
      addTearDown(() async {
        controller2.dispose();
        await engine2.close();
      });
      listenable.value = controller2;
      await tester.pumpAndSettle();
      expect(active, [true]);
      engine2.add(_event(WakeWordEventKind.detected, score: 0.9));
      await tester.pumpAndSettle();
      expect(find.text('0.900'), findsWidgets); // follows the new engine

      final page = find.byKey(const Key('settings-page-diagnostics-'));
      // Height is only meaningful with real glyph widths (see real_fonts.dart).
      for (final e
          in !realFonts
              ? const <Element>[]
              : find
                    .descendant(of: page, matching: find.byType(Scrollable))
                    .evaluate()) {
        final pos = ((e as StatefulElement).state as ScrollableState).position;
        if (pos.axis == Axis.vertical) expect(pos.maxScrollExtent, 0);
      }

      await open(tester, 'assistant');
      expect(active, [true, false]);
    },
  );

  group('apply-immediately edge cases', () {
    testWidgets(
      'undoing a Core change while its apply is in flight is applied',
      (tester) async {
        final h = _Harness();
        await h.pump(tester);
        await open(tester, 'listening', 'endOfSpeech');
        final gate = Completer<void>();
        h.client.applyGate = gate;

        // Silero (current) → Energy: applied, and held in flight on the Mac…
        await tester.tap(find.text('Energy'));
        await tester.pump(const Duration(seconds: 1));
        // …then straight back to Silero — exactly the Core's confirmed value.
        await tester.tap(find.text('Silero'));
        await tester.pump(const Duration(seconds: 1));
        gate.complete();
        await settle(tester);

        // The undo must still be sent, or the Core is left on Energy.
        expect(h.client.applyCalls.map((c) => c['vadEngine']), [
          'energy',
          'silero',
        ]);
      },
    );

    testWidgets('a half-typed field is not applied by an unrelated change', (
      tester,
    ) async {
      final h = _Harness();
      await h.pump(tester);

      await open(tester, 'device', 'connection');
      await tester.enterText(
        find.byKey(const Key('settings-device-name')),
        'Kit',
      );
      // Change something else while the name is still being typed.
      await open(tester, 'display', 'screen');
      await tester.tap(find.byKey(const Key('settings-dim-delay-60')));
      await settle(tester);

      expect(h.last!.dimDelaySecs, 60);
      expect(h.last!.deviceName, isEmpty);
    });

    testWidgets('a half-typed field is applied when Settings closes', (
      tester,
    ) async {
      final h = _Harness();
      await h.pump(tester);

      await open(tester, 'device', 'connection');
      await tester.enterText(
        find.byKey(const Key('settings-device-name')),
        'Kitchen',
      );
      await tester.pumpWidget(const SizedBox());
      await tester.pump();

      expect(h.last!.deviceName, 'Kitchen');
    });

    testWidgets('a failed device-local apply is retried', (tester) async {
      final h = _Harness();
      await h.pump(tester);
      var fail = true;
      h.onApplied = (_) {
        if (fail) throw Exception('engine failed to start');
      };

      await open(tester, 'listening');
      await pickDropdown(tester, 'settings-wakeword', 'alexa');
      await settle(tester);
      expect(find.textContaining('engine failed to start'), findsOneWidget);

      fail = false;
      await tester.pumpWidget(const SizedBox()); // closing Settings retries
      await tester.pumpAndSettle();
      expect(h.applied.map((s) => s.wakeWord), ['alexa', 'alexa']);
    });

    testWidgets('re-pinning the Core switches Settings to the new Mac', (
      tester,
    ) async {
      final h = _Harness(
        client: FakeOrchestratorClient(
          orchestrators: const [
            OrchestratorOption(
              key: 'test-mac',
              name: 'Test Mac',
              host: '192.168.1.11',
              port: 10700,
            ),
          ],
        ),
      );
      await h.pump(tester);

      await open(tester, 'device', 'connection');
      await pickDropdown(
        tester,
        'settings-orchestrator',
        'Test Mac · 192.168.1.11',
      );
      await settle(tester);

      final newCore = h.coreClients['test-mac']!;
      expect(newCore.fetchCount, 1); // reloaded from the new Core

      await open(tester, 'assistant', 'brain');
      await tester.tap(find.text('OpenAI'));
      await settle(tester);
      expect(newCore.applyCalls.single['llmBackend'], 'openai');
      expect(h.client.applyCalls, isEmpty);
    });
  });

  // Every tab must fit the Echo Show's 960×600 dp screen without scrolling — the
  // core goal of the reorg. Runs with Advanced mode on (the tallest layouts) and
  // off, against a realistic ~15-capability Core.
  for (final advanced in [true, false]) {
    testWidgets('no tab scrolls at 960×600 (advanced: $advanced)', (
      tester,
    ) async {
      // Measured with the real Material fonts, as on the device.
      if (!await loadRealFonts(tester)) {
        markTestSkipped('Flutter SDK Material fonts not found');
        return;
      }
      final functions = [
        for (final (cat, name) in const [
          ('Config', 'LLM'),
          ('Config', 'Web search'),
          ('Config', 'Text to speech'),
          ('Speech', 'Speech to text'),
          ('Tools', 'Directions'),
          ('Tools', 'Weather'),
          ('Tools', 'Places'),
          ('Tools', 'Spotify'),
          ('Tools', 'Shopping list'),
          ('Tools', 'Phone messages'),
          ('Tools', 'Calendar'),
          ('System1', 'Fast decisions'),
          ('Memory', 'Embeddings'),
          ('Memory', 'Graph extraction'),
          ('Memory', 'Recall'),
        ])
          FunctionStatusView(
            category: cat,
            name: name,
            mode: 'cloud',
            provider: 'Some provider name',
            model: 'some-model-id-2026',
            keyRequired: true,
            keySet: name != 'Places',
            note: '',
          ),
      ];
      final h = _Harness(
        client: FakeOrchestratorClient(
          functions: functions,
          settings: const OrchestratorSettingsView(
            ok: true,
            message: 'ok',
            llmBackend: 'anthropic',
            vadEngine: 'silero',
          ),
        ),
      );
      await h.pump(
        tester,
        initial: AppSettings(
          advancedMode: advanced,
          photoSource: PhotoSourceKind.drive,
          deviceId: 'anamanti-140ac5942aca',
        ),
      );

      const pages = <(String, String?)>[
        ('assistant', 'brain'),
        ('assistant', 'capabilities'),
        ('listening', 'wakeWord'),
        ('listening', 'microphone'),
        ('listening', 'endOfSpeech'),
        ('diagnostics', null),
        ('display', 'screen'),
        ('display', 'photos'),
        ('household', null),
        ('device', 'connection'),
        ('device', 'maintenance'),
      ];
      for (final (section, tab) in pages) {
        if (tab == 'microphone' && !advanced) continue;
        await open(tester, section, tab);
        final page = find.byKey(Key('settings-page-$section-${tab ?? ''}'));
        expect(page, findsOneWidget, reason: '$section/$tab');
        final scrollables = find.descendant(
          of: page,
          matching: find.byType(Scrollable),
        );
        for (final e in scrollables.evaluate()) {
          final state = (e as StatefulElement).state as ScrollableState;
          if (state.position.axis != Axis.vertical) continue;
          expect(
            state.position.maxScrollExtent,
            0,
            reason:
                '$section/$tab scrolls by ${state.position.maxScrollExtent}',
          );
        }
      }
    });
  }
}

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

WakeWordEvent _event(
  WakeWordEventKind kind, {
  double score = 0,
  String transcript = '',
}) => WakeWordEvent(
  kind: kind,
  message: '',
  device: '',
  deviceSampleRate: 0,
  channels: 0,
  rms: 0,
  score: score,
  avgScore: 0,
  threshold: 0,
  gainDb: 0,
  model: '',
  transcript: transcript,
  reply: '',
  timerId: 0,
  timerLabel: '',
  timerRemainingSecs: 0,
  present: false,
  recipeJson: '',
  weatherJson: '',
  placeJson: '',
  recipeAction: '',
  enrollJson: '',
);
