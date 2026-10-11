// Tests for the Settings → Permissions page and its PermissionsController.
//
// The native `anamanti_display/permissions` channel (MainActivity) is faked via the
// test binary messenger, so these run on the host with no device: the controller's
// status/request/degrade behavior, and the page's grant rows + Grant → request →
// engine-restart flow.

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:anamanti_display/src/engine/permissions.dart';
import 'package:anamanti_display/src/settings/app_settings.dart';
import 'package:anamanti_display/src/ui/settings_screen.dart';

import 'support/fake_orchestrator_client.dart';
import 'support/in_memory_settings_store.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  const channel = MethodChannel('anamanti_display/permissions');
  final messenger =
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger;

  group('PermissionsController', () {
    tearDown(() => messenger.setMockMethodCallHandler(channel, null));

    test('status returns the platform grant map', () async {
      messenger.setMockMethodCallHandler(channel, (call) async {
        expect(call.method, 'status');
        return <String, bool>{
          'android.permission.CAMERA': false,
          'android.permission.RECORD_AUDIO': true,
        };
      });
      final status = await PermissionsController().status();
      expect(status['android.permission.CAMERA'], false);
      expect(status['android.permission.RECORD_AUDIO'], true);
    });

    test('request forwards the names and returns refreshed status', () async {
      List<Object?>? requested;
      messenger.setMockMethodCallHandler(channel, (call) async {
        expect(call.method, 'request');
        requested = (call.arguments as Map)['permissions'] as List<Object?>;
        return <String, bool>{kCameraPermission: true};
      });
      final status = await PermissionsController().request([kCameraPermission]);
      expect(requested, [kCameraPermission]);
      expect(status[kCameraPermission], true);
    });

    test('an absent channel degrades to an empty map (no throw)', () async {
      // No mock handler installed → MissingPluginException, swallowed → empty.
      final status = await PermissionsController().status();
      expect(status, isEmpty);
    });
  });

  group('Permissions page', () {
    tearDown(() => messenger.setMockMethodCallHandler(channel, null));

    Future<void> openPermissions(WidgetTester tester) async {
      final tile = find.byKey(const Key('settings-menu-permissions'));
      await tester.ensureVisible(tile);
      await tester.pumpAndSettle();
      await tester.tap(tile);
      await tester.pumpAndSettle();
    }

    testWidgets('lists grants; a missing runtime permission shows Grant',
        (tester) async {
      messenger.setMockMethodCallHandler(channel, (call) async {
        if (call.method == 'status') {
          return <String, bool>{
            'android.permission.CAMERA': false,
            'android.permission.RECORD_AUDIO': true,
            'android.permission.INTERNET': true,
          };
        }
        return null;
      });

      await tester.pumpWidget(MaterialApp(
        home: SettingsScreen(
          initial: const AppSettings(),
          store: InMemorySettingsStore(),
          client: FakeOrchestratorClient(),
          onApplied: (_) {},
        ),
      ));
      await tester.pumpAndSettle();
      await openPermissions(tester);

      // Camera missing → Grant button; Microphone granted → no button.
      expect(
        find.byKey(Key('settings-permission-grant-$kCameraPermission')),
        findsOneWidget,
      );
      expect(
        find.byKey(const Key(
            'settings-permission-grant-android.permission.RECORD_AUDIO')),
        findsNothing,
      );
      // Install-time INTERNET is shown read-only (row present, no Grant button).
      expect(
        find.byKey(const Key('settings-permission-android.permission.INTERNET')),
        findsOneWidget,
      );
    });

    testWidgets('Grant requests the permission and restarts the engine on camera',
        (tester) async {
      var engineRestarts = 0;
      var requestCalls = 0;
      final state = <String, bool>{
        'android.permission.CAMERA': false,
        'android.permission.RECORD_AUDIO': true,
      };
      messenger.setMockMethodCallHandler(channel, (call) async {
        switch (call.method) {
          case 'status':
            return Map<String, bool>.from(state);
          case 'request':
            requestCalls++;
            final names =
                ((call.arguments as Map)['permissions'] as List).cast<String>();
            for (final n in names) {
              state[n] = true;
            }
            return Map<String, bool>.from(state);
        }
        return null;
      });

      await tester.pumpWidget(MaterialApp(
        home: SettingsScreen(
          initial: const AppSettings(),
          store: InMemorySettingsStore(),
          client: FakeOrchestratorClient(),
          onApplied: (_) {},
          onRequestEngineRestart: () => engineRestarts++,
        ),
      ));
      await tester.pumpAndSettle();
      await openPermissions(tester);

      await tester.tap(
        find.byKey(Key('settings-permission-grant-$kCameraPermission')),
      );
      await tester.pumpAndSettle();

      expect(requestCalls, 1);
      expect(engineRestarts, 1); // a camera grant restarts the engine
      // The Grant button is gone now the permission is granted.
      expect(
        find.byKey(Key('settings-permission-grant-$kCameraPermission')),
        findsNothing,
      );
    });
  });
}
