import 'package:anamanti_companion/main.dart';
import 'package:anamanti_companion/src/state/connection_controller.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/fakes.dart';

void main() {
  testWidgets('connected: four tabs, switching activates the tab', (
    tester,
  ) async {
    final api = FakeCompanionApi()
      ..discoverResult = [sampleCore()]
      ..memoriesResult = [sampleMemory(1, 'dog is Rex')];
    final c = ConnectionController(api: api, store: MemorySelectionStore());
    await c.start();
    await tester.pumpWidget(CompanionApp(controller: c, api: api));
    await tester.pump();
    await tester.pump();

    expect(find.text('Kitchen Mac'), findsOneWidget);
    for (final label in ['Status', 'Now playing', 'Timers', 'Memories']) {
      expect(find.widgetWithText(NavigationDestination, label), findsOneWidget);
    }
    expect(find.byKey(const Key('health-ok')), findsOneWidget);
    expect(api.listCalls, 0, reason: 'Memories loads only when shown');

    await tester.tap(find.text('Memories'));
    await tester.pump();
    await tester.pump();
    expect(api.listCalls, 1);
    expect(find.text('dog is Rex'), findsOneWidget);

    await tester.tap(find.text('Timers'));
    await tester.pump();
    await tester.pump();
    expect(find.text('No timers running'), findsOneWidget);
  });

  testWidgets('picker lists Cores and connects on tap', (tester) async {
    final api = FakeCompanionApi()
      ..discoverResult = [
        sampleCore(key: 'a', name: 'Mac A'),
        sampleCore(key: 'b', name: 'Mac B', companionPort: null),
      ];
    final store = MemorySelectionStore();
    final c = ConnectionController(api: api, store: store);
    await c.start();
    await tester.pumpWidget(CompanionApp(controller: c, api: api));
    expect(find.text('Choose your Core'), findsOneWidget);
    expect(find.textContaining('companion API off'), findsOneWidget);

    await tester.tap(find.text('Mac B'));
    await tester.pump();
    await tester.pump();
    expect(store.value, 'b');
    expect(
      find.text("This Core doesn't have the companion API turned on"),
      findsOneWidget,
    );
  });

  testWidgets('not found offers a fresh search', (tester) async {
    final api = FakeCompanionApi();
    final c = ConnectionController(api: api, store: MemorySelectionStore());
    await c.start();
    await tester.pumpWidget(CompanionApp(controller: c, api: api));
    expect(find.text('No Anamanti Core found'), findsOneWidget);

    api.discoverResult = [sampleCore()];
    await tester.tap(find.text('Search again'));
    await tester.pump();
    await tester.pump();
    expect(find.text('Kitchen Mac'), findsOneWidget);
  });
}
