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

TurnView _turn(
  int id,
  String transcript,
  String reply, {
  String speakerId = 'household',
  List<EntityRefView> entities = const [],
}) =>
    TurnView(
      id: id,
      extId: 'r$id',
      session: 's',
      ts: 0,
      transcript: transcript,
      reply: reply,
      speakerId: speakerId,
      entities: entities,
    );

EntityView _entity(int id, String name, {String kind = 'topic'}) =>
    EntityView(id: id, name: name, kind: kind);

/// Confirm the delete dialog's action button (FilledButton labelled [label]).
Future<void> _confirmDialog(WidgetTester tester, String label) async {
  await tester.tap(find.widgetWithText(FilledButton, label));
  await tester.pumpAndSettle();
}

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

  testWidgets('Conversations tab lists turns and deletes one', (tester) async {
    final client = FakeOrchestratorClient(turns: [
      _turn(10, 'where is the library?', 'On Main Street.',
          entities: [const EntityRefView(name: 'library', kind: 'place')]),
      _turn(11, 'what time is it?', 'It is 3pm.'),
    ]);
    await tester.pumpWidget(MaterialApp(home: MemoryScreen(client: client)));
    await tester.pumpAndSettle();

    await tester.tap(find.text('Conversations'));
    await tester.pumpAndSettle();

    expect(find.text('where is the library?'), findsOneWidget);
    expect(find.text('On Main Street.'), findsOneWidget);
    // The mentioned entity renders as a chip.
    expect(find.widgetWithText(Chip, 'library'), findsOneWidget);

    await tester.tap(find.descendant(
      of: find.byKey(const ValueKey('memory-turn-10')),
      matching: find.byIcon(Icons.close),
    ));
    await tester.pumpAndSettle();
    await _confirmDialog(tester, 'Delete');

    expect(client.deletedGraphNodes, [10]);
    expect(find.text('where is the library?'), findsNothing);
  });

  testWidgets('person filter scopes the turn query', (tester) async {
    final client = FakeOrchestratorClient(
      speakers: [
        const SpeakerView(id: 'spk-dana', name: 'Dana', labeled: true, samples: 3, createdAt: 0),
      ],
      turns: [
        _turn(1, 'hi', 'hello', speakerId: 'household'),
        _turn(2, 'remember milk', 'ok', speakerId: 'spk-dana'),
      ],
    );
    await tester.pumpWidget(MaterialApp(home: MemoryScreen(client: client)));
    await tester.pumpAndSettle();

    await tester.tap(find.text('Conversations'));
    await tester.pumpAndSettle();

    // Pick "Dana" in the person dropdown.
    await tester.tap(find.byKey(const Key('memory-turns-speaker')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Dana').last);
    await tester.pumpAndSettle();

    expect(client.listTurnsCalls.last['speakerId'], 'spk-dana');
    expect(find.text('remember milk'), findsOneWidget);
    expect(find.text('hi'), findsNothing);
  });

  testWidgets('Keywords tab lists entities, tapping filters conversations', (tester) async {
    final client = FakeOrchestratorClient(
      entities: [_entity(5, 'jazz'), _entity(6, 'Portland', kind: 'place')],
      turns: [
        _turn(1, 'play jazz', 'ok',
            entities: [const EntityRefView(name: 'jazz', kind: 'topic')]),
      ],
    );
    await tester.pumpWidget(MaterialApp(home: MemoryScreen(client: client)));
    await tester.pumpAndSettle();

    await tester.tap(find.text('Keywords'));
    await tester.pumpAndSettle();
    expect(find.text('jazz'), findsOneWidget);
    expect(find.text('Portland'), findsOneWidget);

    // Tapping a keyword jumps to Conversations filtered to that entity.
    await tester.tap(find.byKey(const ValueKey('memory-entity-5')));
    await tester.pumpAndSettle();
    expect(client.listTurnsCalls.last['entity'], 'jazz');
    expect(find.byKey(const Key('memory-turns-entity-chip')), findsOneWidget);
    expect(find.text('play jazz'), findsOneWidget);
  });

  testWidgets('Keywords tab deletes an entity after confirm', (tester) async {
    final client = FakeOrchestratorClient(
      entities: [_entity(5, 'jazz'), _entity(6, 'Portland', kind: 'place')],
    );
    await tester.pumpWidget(MaterialApp(home: MemoryScreen(client: client)));
    await tester.pumpAndSettle();

    await tester.tap(find.text('Keywords'));
    await tester.pumpAndSettle();

    await tester.tap(find.descendant(
      of: find.byKey(const ValueKey('memory-entity-5')),
      matching: find.byIcon(Icons.close),
    ));
    await tester.pumpAndSettle();
    await _confirmDialog(tester, 'Delete');

    expect(client.deletedGraphNodes, [5]);
    expect(find.text('jazz'), findsNothing);
    expect(find.text('Portland'), findsOneWidget);
  });
}

/// Always throws on list, to exercise the error state.
class _ThrowingClient extends FakeOrchestratorClient {
  @override
  Future<List<MemoryView>> listMemories() async => throw Exception('offline');
}
