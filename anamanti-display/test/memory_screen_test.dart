// Widget tests for the Phase-6 memory management screen.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:anamanti_display/src/settings/orchestrator_client.dart';
import 'package:anamanti_display/src/ui/memory_screen.dart';

import 'support/fake_orchestrator_client.dart';

MemoryView _mem(int id, String content, {String kind = 'fact'}) => MemoryView(
      id: id,
      kind: kind,
      content: content,
      source: 'explicit',
      createdAt: 0,
    );

void main() {
  testWidgets('lists entries and deletes one', (tester) async {
    final client = FakeOrchestratorClient(memories: [
      _mem(1, 'the user likes tea'),
      _mem(2, 'likes jazz', kind: 'preference'),
    ]);
    await tester.pumpWidget(MaterialApp(home: MemoryScreen(client: client)));
    await tester.pumpAndSettle();

    expect(find.text('the user likes tea'), findsOneWidget);
    expect(find.text('likes jazz'), findsOneWidget);

    // Delete the first entry.
    await tester.tap(find.descendant(
      of: find.byKey(const ValueKey('memory-1')),
      matching: find.byIcon(Icons.close),
    ));
    await tester.pumpAndSettle();

    expect(client.deleted, [1]);
    expect(find.text('the user likes tea'), findsNothing);
    expect(find.text('likes jazz'), findsOneWidget);
  });

  testWidgets('empty store shows a friendly hint', (tester) async {
    final client = FakeOrchestratorClient(memories: []);
    await tester.pumpWidget(MaterialApp(home: MemoryScreen(client: client)));
    await tester.pumpAndSettle();

    expect(find.text('Nothing remembered yet'), findsOneWidget);
  });

  testWidgets('offline assistant shows an error with retry', (tester) async {
    final client = _ThrowingClient();
    await tester.pumpWidget(MaterialApp(home: MemoryScreen(client: client)));
    await tester.pumpAndSettle();

    expect(find.text('Memory unavailable'), findsOneWidget);
    expect(find.text('Retry'), findsOneWidget);
  });
}

/// Always throws on list, to exercise the error state.
class _ThrowingClient extends FakeOrchestratorClient {
  @override
  Future<List<MemoryView>> listMemories() async => throw Exception('offline');
}
