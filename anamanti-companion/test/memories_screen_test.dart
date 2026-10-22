import 'package:anamanti_companion/src/ui/memories_screen.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/fakes.dart';
import 'support/host.dart';

void main() {
  FakeCompanionApi apiWith() => FakeCompanionApi()
    ..memoriesResult = [
      sampleMemory(2, 'likes jazz', kind: 'preference'),
      sampleMemory(1, 'dog is Rex'),
    ];

  testWidgets('lists entries with kind and source', (tester) async {
    final api = apiWith();
    await tester.pumpWidget(
      host(MemoriesScreen(api: api, core: sampleCore(), active: true)),
    );
    await tester.pump();
    expect(find.text('likes jazz'), findsOneWidget);
    expect(find.text('dog is Rex'), findsOneWidget);
    expect(find.textContaining('Preference · Explicit'), findsOneWidget);
    expect(find.text('2 remembered'), findsOneWidget);
  });

  testWidgets('works without the companion API (Wyoming only)', (tester) async {
    final api = apiWith();
    await tester.pumpWidget(
      host(
        MemoriesScreen(
          api: api,
          core: sampleCore(companionPort: null),
          active: true,
        ),
      ),
    );
    await tester.pump();
    expect(find.text('likes jazz'), findsOneWidget);
  });

  testWidgets('does not load until the tab is shown', (tester) async {
    final api = apiWith();
    await tester.pumpWidget(
      host(MemoriesScreen(api: api, core: sampleCore(), active: false)),
    );
    await tester.pump();
    expect(api.listCalls, 0);
  });

  testWidgets('swipe to delete asks first; cancel keeps it', (tester) async {
    final api = apiWith();
    await tester.pumpWidget(
      host(MemoriesScreen(api: api, core: sampleCore(), active: true)),
    );
    await tester.pump();

    await tester.drag(find.text('dog is Rex'), const Offset(-600, 0));
    await tester.pumpAndSettle();
    expect(find.text('Forget this?'), findsOneWidget);
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(api.deletedIds, isEmpty);
    expect(find.text('dog is Rex'), findsOneWidget);

    await tester.drag(find.text('dog is Rex'), const Offset(-600, 0));
    await tester.pumpAndSettle();
    await tester.tap(find.widgetWithText(FilledButton, 'Forget'));
    await tester.pumpAndSettle();
    expect(api.deletedIds, [1]);
    expect(find.text('dog is Rex'), findsNothing);
    expect(find.text('1 remembered'), findsOneWidget);
  });

  testWidgets('delete button also confirms', (tester) async {
    final api = apiWith();
    await tester.pumpWidget(
      host(MemoriesScreen(api: api, core: sampleCore(), active: true)),
    );
    await tester.pump();
    await tester.tap(find.byTooltip('Forget this').first);
    await tester.pumpAndSettle();
    await tester.tap(find.widgetWithText(FilledButton, 'Forget'));
    await tester.pumpAndSettle();
    expect(api.deletedIds, [2]);
    expect(find.text('likes jazz'), findsNothing);
  });

  testWidgets('clear all confirms, then empties the list', (tester) async {
    final api = apiWith()..clearResult = 2;
    await tester.pumpWidget(
      host(MemoriesScreen(api: api, core: sampleCore(), active: true)),
    );
    await tester.pump();
    await tester.tap(find.byKey(const Key('memories-clear-all')));
    await tester.pumpAndSettle();
    expect(find.text('Forget everything?'), findsOneWidget);
    await tester.tap(find.widgetWithText(FilledButton, 'Forget all'));
    await tester.pumpAndSettle();
    expect(api.clearCalls, 1);
    expect(find.text('Nothing remembered yet'), findsOneWidget);
    expect(find.text('Forgot 2 memories'), findsOneWidget);
  });

  testWidgets('error state with retry', (tester) async {
    final api = FakeCompanionApi()
      ..memoriesResult = Exception('connection refused');
    await tester.pumpWidget(
      host(MemoriesScreen(api: api, core: sampleCore(), active: true)),
    );
    await tester.pump();
    expect(find.text('Memory unavailable'), findsOneWidget);
    api.memoriesResult = [sampleMemory(3, 'new')];
    await tester.tap(find.text('Retry'));
    await tester.pump();
    expect(find.text('new'), findsOneWidget);
  });
}
