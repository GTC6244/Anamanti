// Widget tests for the Phase-6 settings screen.
//
// The screen mixes device-local settings (persisted via [SettingsStore]) with
// orchestrator-managed settings (a [FakeOrchestratorClient], no native library).
// An in-memory store keeps `pumpAndSettle` reliable (fake-async can't drive real
// file IO); the real file round trip is covered by `settings_store_test.dart`.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:ambient_display/src/settings/app_settings.dart';
import 'package:ambient_display/src/settings/orchestrator_client.dart';
import 'package:ambient_display/src/ui/settings_screen.dart';

import 'support/fake_orchestrator_client.dart';
import 'support/in_memory_settings_store.dart';

void main() {
  testWidgets('loads remote settings and applies both halves on Save',
      (tester) async {
    final store = InMemorySettingsStore();
    final client = FakeOrchestratorClient();
    AppSettings? applied;

    await tester.pumpWidget(MaterialApp(
      home: SettingsScreen(
        initial: const AppSettings(),
        store: store,
        client: client,
        onApplied: (s) => applied = s,
      ),
    ));
    await tester.pumpAndSettle();

    // Remote settings were fetched and the backend dropdown reflects them.
    expect(client.fetchCount, 1);
    expect(find.text('Local (Ollama)'), findsOneWidget);

    // Change the wake word away from the default (hey_jarvis).
    await tester.tap(find.byKey(const Key('settings-wakeword')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('alexa').last);
    await tester.pumpAndSettle();

    // Save.
    await tester.tap(find.byKey(const Key('settings-save')));
    await tester.pumpAndSettle();

    // Device-local settings persisted + surfaced to the parent.
    expect(applied, isNotNull);
    expect(applied!.wakeWord, 'alexa');
    expect(store.value.wakeWord, 'alexa');

    // Orchestrator settings applied once (with the loaded backend + a voice write).
    expect(client.applyCalls.length, 1);
    expect(client.applyCalls.single['llmBackend'], 'ollama');
    expect(client.applyCalls.single['setTtsVoice'], true);
  });

  testWidgets('changing the backend is sent to the orchestrator', (tester) async {
    final store = InMemorySettingsStore();
    final client = FakeOrchestratorClient();

    await tester.pumpWidget(MaterialApp(
      home: SettingsScreen(
        initial: const AppSettings(),
        store: store,
        client: client,
        onApplied: (_) {},
      ),
    ));
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('settings-backend')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Cloud (Claude)').last);
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('settings-save')));
    await tester.pumpAndSettle();

    expect(client.applyCalls.single['llmBackend'], 'anthropic');
  });

  testWidgets('cloud backend shows a model dropdown and sends the picked model',
      (tester) async {
    final store = InMemorySettingsStore();
    final client = FakeOrchestratorClient(models: const [
      ModelOption(provider: 'anthropic', id: 'claude-opus-5', label: 'Claude Opus 5'),
      ModelOption(provider: 'anthropic', id: 'claude-sonnet-5', label: 'Claude Sonnet 5'),
      ModelOption(provider: 'openai', id: 'gpt-4o-mini', label: 'gpt-4o-mini'),
    ]);

    await tester.pumpWidget(MaterialApp(
      home: SettingsScreen(
        initial: const AppSettings(),
        store: store,
        client: client,
        onApplied: (_) {},
      ),
    ));
    await tester.pumpAndSettle();

    // Switch to the Claude backend → the model becomes a dropdown of Anthropic
    // models (the OpenAI entry is filtered out).
    await tester.tap(find.byKey(const Key('settings-backend')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Cloud (Claude)').last);
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('settings-model')));
    await tester.pumpAndSettle();
    expect(find.text('Claude Sonnet 5').last, findsOneWidget);
    expect(find.text('gpt-4o-mini'), findsNothing);
    await tester.tap(find.text('Claude Sonnet 5').last);
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('settings-save')));
    await tester.pumpAndSettle();

    expect(client.applyCalls.single['llmBackend'], 'anthropic');
    expect(client.applyCalls.single['llmModel'], 'claude-sonnet-5');
  });

  testWidgets('voice dropdown lists installed voices and sends the picked voice',
      (tester) async {
    final store = InMemorySettingsStore();
    final client = FakeOrchestratorClient(voices: const [
      VoiceOption(name: 'en_US-amy-medium', label: 'amy (medium)', language: 'en_US'),
      VoiceOption(name: 'en_US-lessac-medium', label: 'lessac (medium)', language: 'en_US'),
    ]);

    await tester.pumpWidget(MaterialApp(
      home: SettingsScreen(
        initial: const AppSettings(),
        store: store,
        client: client,
        onApplied: (_) {},
      ),
    ));
    await tester.pumpAndSettle();

    // The voice control is a dropdown of installed voices (labels carry locale).
    await tester.tap(find.byKey(const Key('settings-voice')));
    await tester.pumpAndSettle();
    expect(find.text('Server default').last, findsOneWidget);
    expect(find.text('amy (medium) · en_US').last, findsOneWidget);
    await tester.tap(find.text('amy (medium) · en_US').last);
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('settings-save')));
    await tester.pumpAndSettle();

    expect(client.applyCalls.single['setTtsVoice'], true);
    expect(client.applyCalls.single['ttsVoice'], 'en_US-amy-medium');
  });

  testWidgets('voice field falls back to free text when no voices are available',
      (tester) async {
    final store = InMemorySettingsStore();
    final client = FakeOrchestratorClient(voices: const <VoiceOption>[]);

    await tester.pumpWidget(MaterialApp(
      home: SettingsScreen(
        initial: const AppSettings(),
        store: store,
        client: client,
        onApplied: (_) {},
      ),
    ));
    await tester.pumpAndSettle();

    // With no installed voices, the control is a text field the user can type into.
    await tester.enterText(
        find.byKey(const Key('settings-voice')), 'en_GB-alan-medium');
    await tester.tap(find.byKey(const Key('settings-save')));
    await tester.pumpAndSettle();

    expect(client.applyCalls.single['ttsVoice'], 'en_GB-alan-medium');
  });

  testWidgets('anthropic backend exposes the auth selector and sends subscription',
      (tester) async {
    final store = InMemorySettingsStore();
    final client = FakeOrchestratorClient();

    await tester.pumpWidget(MaterialApp(
      home: SettingsScreen(
        initial: const AppSettings(),
        store: store,
        client: client,
        onApplied: (_) {},
      ),
    ));
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('settings-backend')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Cloud (Claude)').last);
    await tester.pumpAndSettle();

    // The auth selector appears for Anthropic; pick Subscription.
    expect(find.byKey(const Key('settings-anthropic-auth')), findsOneWidget);
    await tester.tap(find.byKey(const Key('settings-anthropic-auth')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Subscription').last);
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('settings-save')));
    await tester.pumpAndSettle();

    expect(client.applyCalls.single['llmBackend'], 'anthropic');
    expect(client.applyCalls.single['anthropicAuth'], 'subscription');
  });

  testWidgets('offline assistant still saves device-local settings',
      (tester) async {
    final store = InMemorySettingsStore();
    final client = FakeOrchestratorClient(throwOnFetch: true);
    AppSettings? applied;

    await tester.pumpWidget(MaterialApp(
      home: SettingsScreen(
        initial: const AppSettings(),
        store: store,
        client: client,
        onApplied: (s) => applied = s,
      ),
    ));
    await tester.pumpAndSettle();

    expect(find.text('Assistant offline'), findsOneWidget);

    await tester.tap(find.byKey(const Key('settings-save')));
    await tester.pumpAndSettle();

    // Local settings saved; no remote apply attempted while offline.
    expect(applied, isNotNull);
    expect(client.applyCalls, isEmpty);
    expect(store.value, isNotNull);
  });

  testWidgets('orchestrator dropdown lists discovered orchestrators and persists the selection',
      (tester) async {
    final store = InMemorySettingsStore();
    final client = FakeOrchestratorClient(orchestrators: const [
      OrchestratorOption(
          key: 'mac-mini', name: 'Mac Mini', host: '192.168.1.10', port: 10700),
      OrchestratorOption(
          key: 'test-mac', name: 'Test Mac', host: '192.168.1.11', port: 10700),
    ]);
    AppSettings? applied;

    await tester.pumpWidget(MaterialApp(
      home: SettingsScreen(
        initial: const AppSettings(),
        store: store,
        client: client,
        onApplied: (s) => applied = s,
      ),
    ));
    await tester.pumpAndSettle();

    // The dropdown offers Auto plus each discovered orchestrator.
    await tester.tap(find.byKey(const Key('settings-orchestrator')));
    await tester.pumpAndSettle();
    expect(find.text('Auto (first available)').last, findsOneWidget);
    expect(find.text('Test Mac · 192.168.1.11').last, findsOneWidget);
    await tester.tap(find.text('Test Mac · 192.168.1.11').last);
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('settings-save')));
    await tester.pumpAndSettle();

    // The selection is device-local: persisted + surfaced to the parent.
    expect(applied, isNotNull);
    expect(applied!.orchestratorKey, 'test-mac');
    expect(store.value.orchestratorKey, 'test-mac');
  });

  testWidgets('orchestrator dropdown stays usable (Auto) when the assistant is offline',
      (tester) async {
    final store = InMemorySettingsStore();
    // Offline: fetchSettings/listModels throw, but discovery (listOrchestrators)
    // is a separate path, so the device-local Orchestrator tile still shows.
    final client = FakeOrchestratorClient(throwOnFetch: true);

    await tester.pumpWidget(MaterialApp(
      home: SettingsScreen(
        initial: const AppSettings(),
        store: store,
        client: client,
        onApplied: (_) {},
      ),
    ));
    await tester.pumpAndSettle();

    expect(find.text('Assistant offline'), findsOneWidget);
    // The Orchestrator picker (outside the offline-gated assistant tiles) is present.
    expect(find.byKey(const Key('settings-orchestrator')), findsOneWidget);
    await tester.tap(find.byKey(const Key('settings-orchestrator')));
    await tester.pumpAndSettle();
    expect(find.text('Auto (first available)').last, findsOneWidget);
  });

  testWidgets('dim-delay slider renders the current value and Save preserves it',
      (tester) async {
    final store = InMemorySettingsStore();
    final client = FakeOrchestratorClient();
    AppSettings? applied;

    await tester.pumpWidget(MaterialApp(
      home: SettingsScreen(
        // A non-default dim delay (2 minutes) so we can see it rendered + saved.
        initial: const AppSettings(dimDelaySecs: 120),
        store: store,
        client: client,
        onApplied: (s) => applied = s,
      ),
    ));
    await tester.pumpAndSettle();

    // The Display section exposes the dim-delay slider, labelled with the current
    // value formatted as minutes.
    final scrollable = find.byType(Scrollable).first;
    await tester.scrollUntilVisible(
      find.byKey(const Key('settings-dim-delay')),
      200,
      scrollable: scrollable,
    );
    await tester.ensureVisible(find.byKey(const Key('settings-dim-delay')));
    await tester.pumpAndSettle();
    expect(find.text('2m'), findsOneWidget);

    // Saving persists the device-local dim delay and surfaces it to the parent so
    // the engine restarts with the new proximity release window.
    await tester.scrollUntilVisible(
      find.byKey(const Key('settings-save')),
      -200,
      scrollable: scrollable,
    );
    await tester.ensureVisible(find.byKey(const Key('settings-save')));
    await tester.tap(find.byKey(const Key('settings-save')));
    await tester.pumpAndSettle();

    expect(applied, isNotNull);
    expect(applied!.dimDelaySecs, 120);
    expect(store.value.dimDelaySecs, 120);
  });

  testWidgets('memory tile navigates to the memory screen', (tester) async {
    final store = InMemorySettingsStore();
    final client = FakeOrchestratorClient();
    await tester.pumpWidget(MaterialApp(
      home: SettingsScreen(
        initial: const AppSettings(),
        store: store,
        client: client,
        onApplied: (_) {},
      ),
    ));
    await tester.pumpAndSettle();

    // The memory tile sits at the bottom of the settings list; scroll it fully into
    // view before tapping (the list is long enough that a partial reveal can leave
    // the tile's center off-screen).
    await tester.scrollUntilVisible(
      find.byKey(const Key('settings-memory')),
      200,
      scrollable: find.byType(Scrollable).first,
    );
    await tester.ensureVisible(find.byKey(const Key('settings-memory')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('settings-memory')));
    await tester.pumpAndSettle();

    // The memory screen's app bar title is shown.
    expect(find.text('Memory'), findsOneWidget);
  });

  testWidgets('syncs Drive from the orchestrator, enabling folder pick + Save',
      (tester) async {
    final store = InMemorySettingsStore();
    final client = FakeOrchestratorClient(
      driveToken: const DriveTokenView(
        linked: true,
        configured: true,
        clientId: 'cid.apps',
        clientSecret: 'gocspx-secret',
        refreshToken: '1//refresh',
        folderIds: <String>['1AbC', '1XyZ'],
        scope: 'drive.readonly',
      ),
    );

    await tester.pumpWidget(MaterialApp(
      home: SettingsScreen(
        // Start on the Drive source so the Drive controls render.
        initial: const AppSettings(photoSource: PhotoSourceKind.drive),
        store: store,
        client: client,
        onApplied: (_) {},
      ),
    ));
    await tester.pumpAndSettle();

    final scrollable = find.byType(Scrollable).first;

    // Before syncing there are no Drive credentials, so "Choose folders" is disabled.
    await tester.scrollUntilVisible(
      find.byKey(const Key('settings-drive-pick')),
      200,
      scrollable: scrollable,
    );
    await tester.ensureVisible(find.byKey(const Key('settings-drive-pick')));
    await tester.pumpAndSettle();
    expect(
      tester.widget<FilledButton>(find.byKey(const Key('settings-drive-pick'))).onPressed,
      isNull,
    );

    // Sync pulls the bundle from the (fake) orchestrator.
    await tester.ensureVisible(find.byKey(const Key('settings-drive-sync')));
    await tester.tap(find.byKey(const Key('settings-drive-sync')));
    await tester.pumpAndSettle();

    // Folder field populated from the synced bundle, and folder-pick now enabled.
    expect(find.text('1AbC, 1XyZ'), findsOneWidget);
    await tester.ensureVisible(find.byKey(const Key('settings-drive-pick')));
    expect(
      tester.widget<FilledButton>(find.byKey(const Key('settings-drive-pick'))).onPressed,
      isNotNull,
    );

    // Save persists the synced Drive credentials device-locally (no rebuild needed).
    await tester.scrollUntilVisible(
      find.byKey(const Key('settings-save')),
      -200,
      scrollable: scrollable,
    );
    await tester.ensureVisible(find.byKey(const Key('settings-save')));
    await tester.tap(find.byKey(const Key('settings-save')));
    await tester.pumpAndSettle();

    expect(store.value.driveClientId, 'cid.apps');
    expect(store.value.driveClientSecret, 'gocspx-secret');
    expect(store.value.driveRefreshToken, '1//refresh');
    expect(store.value.driveLinked, isTrue);
    expect(store.value.driveConfigured, isTrue);
    expect(store.value.driveFolderIds, <String>['1AbC', '1XyZ']);
  });
}
