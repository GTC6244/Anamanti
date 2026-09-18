// Widget tests for the Phase-C "People" management screen.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:ambient_display/src/settings/orchestrator_client.dart';
import 'package:ambient_display/src/ui/people_screen.dart';

import 'support/fake_orchestrator_client.dart';

SpeakerView _spk(String id, {String? name, int samples = 3}) => SpeakerView(
      id: id,
      name: name,
      labeled: name != null,
      samples: samples,
      createdAt: 0,
    );

void main() {
  testWidgets('shows named people and anonymous "Speaker N" placeholders',
      (tester) async {
    final client = FakeOrchestratorClient(speakers: [
      _spk('spk-1', name: 'Sam'),
      _spk('spk-2'), // anonymous
    ]);
    await tester.pumpWidget(MaterialApp(home: PeopleScreen(client: client)));
    await tester.pumpAndSettle();

    expect(find.text('Sam'), findsOneWidget);
    expect(find.text('Speaker 2'), findsOneWidget);
  });

  testWidgets('names an anonymous speaker', (tester) async {
    final client = FakeOrchestratorClient(speakers: [_spk('spk-2')]);
    await tester.pumpWidget(MaterialApp(home: PeopleScreen(client: client)));
    await tester.pumpAndSettle();

    // Open the row menu → rename.
    await tester.tap(find.byKey(const ValueKey('speaker-menu-spk-2')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Name / rename'));
    await tester.pumpAndSettle();

    await tester.enterText(find.byType(TextField), 'Dana');
    await tester.tap(find.text('Save'));
    await tester.pumpAndSettle();

    expect(client.namedCalls, [
      {'id': 'spk-2', 'name': 'Dana'}
    ]);
    expect(find.text('Dana'), findsOneWidget);
  });

  testWidgets('deletes a speaker', (tester) async {
    final client = FakeOrchestratorClient(speakers: [
      _spk('spk-1', name: 'Sam'),
      _spk('spk-2', name: 'Dana'),
    ]);
    await tester.pumpWidget(MaterialApp(home: PeopleScreen(client: client)));
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const ValueKey('speaker-menu-spk-1')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Forget'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Forget').last); // confirm dialog
    await tester.pumpAndSettle();

    expect(client.deletedSpeakers, ['spk-1']);
    expect(find.text('Sam'), findsNothing);
    expect(find.text('Dana'), findsOneWidget);
  });

  testWidgets('empty state shows a friendly hint', (tester) async {
    final client = FakeOrchestratorClient(speakers: []);
    await tester.pumpWidget(MaterialApp(home: PeopleScreen(client: client)));
    await tester.pumpAndSettle();

    expect(find.text('No one recognized yet'), findsOneWidget);
  });

  testWidgets('offline assistant shows an error with retry', (tester) async {
    final client = _ThrowingClient();
    await tester.pumpWidget(MaterialApp(home: PeopleScreen(client: client)));
    await tester.pumpAndSettle();

    expect(find.text('People unavailable'), findsOneWidget);
    expect(find.text('Retry'), findsOneWidget);
  });
}

/// Always throws on list, to exercise the error state.
class _ThrowingClient extends FakeOrchestratorClient {
  @override
  Future<List<SpeakerView>> listSpeakers() async => throw Exception('offline');
}
