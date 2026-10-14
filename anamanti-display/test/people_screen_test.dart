// Widget tests for the Phase-C "People" management screen.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:anamanti_display/src/settings/orchestrator_client.dart';
import 'package:anamanti_display/src/ui/people_screen.dart';

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

  testWidgets('"Add voice samples" is hidden without the live engine',
      (tester) async {
    final client = FakeOrchestratorClient(speakers: [_spk('spk-1', name: 'Sam')]);
    await tester.pumpWidget(MaterialApp(home: PeopleScreen(client: client)));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const ValueKey('speaker-menu-spk-1')));
    await tester.pumpAndSettle();
    expect(find.text('Name / rename'), findsOneWidget);
    expect(find.text('Add voice samples'), findsNothing);
  });

  Future<void> openLink(WidgetTester tester, String id) async {
    await tester.tap(find.byKey(ValueKey('speaker-menu-$id')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Link to household member…'));
    await tester.pumpAndSettle();
  }

  testWidgets('household badges show which voices are linked', (tester) async {
    final client = FakeOrchestratorClient(
      householdMembers: ['Christina'],
      speakers: [
        _spk('spk-1', name: 'Christina'),
        _spk('spk-2', name: 'Bob'),
        _spk('spk-3'),
      ],
    );
    await tester.pumpWidget(MaterialApp(home: PeopleScreen(client: client)));
    await tester.pumpAndSettle();
    expect(find.text('Household member · 3 voice clips'), findsOneWidget);
    expect(find.text('Not linked to household · 3 voice clips'), findsOneWidget);
    expect(find.text('Not named yet · 3 voice clips'), findsOneWidget);
  });

  testWidgets('links an unnamed voice to a household member with no voiceprint',
      (tester) async {
    final client = FakeOrchestratorClient(
      householdMembers: ['Christina', 'Paul'],
      speakers: [_spk('spk-1', name: 'Christina'), _spk('spk-2')],
    );
    await tester.pumpWidget(MaterialApp(home: PeopleScreen(client: client)));
    await tester.pumpAndSettle();

    await openLink(tester, 'spk-2');
    // Paul has no voiceprint → "becomes"; Christina has one → "merge".
    expect(find.text('This voice becomes Paul'), findsOneWidget);
    expect(find.text("Merge into Christina's voiceprint"), findsOneWidget);
    await tester.tap(find.byKey(const ValueKey('link-member-Paul')));
    await tester.pumpAndSettle();

    expect(client.linkCalls, [
      {'id': 'spk-2', 'member': 'Paul'}
    ]);
    expect(client.mergeCalls, isEmpty);
    expect(find.text('Paul'), findsOneWidget);
    expect(find.text('Household member · 3 voice clips'), findsNWidgets(2));
  });

  testWidgets('merges a voice into a household member who already has one',
      (tester) async {
    final client = FakeOrchestratorClient(
      householdMembers: ['Christina'],
      speakers: [_spk('spk-1', name: 'Christina', samples: 4), _spk('spk-2')],
    );
    await tester.pumpWidget(MaterialApp(home: PeopleScreen(client: client)));
    await tester.pumpAndSettle();

    // Cancelling the confirm changes nothing.
    await openLink(tester, 'spk-2');
    await tester.tap(find.byKey(const ValueKey('link-member-Christina')));
    await tester.pumpAndSettle();
    expect(find.text('Merge Speaker 2 into Christina?'), findsOneWidget);
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(client.linkCalls, isEmpty);

    // Confirmed → merged; one voiceprint for Christina remains.
    await openLink(tester, 'spk-2');
    await tester.tap(find.byKey(const ValueKey('link-member-Christina')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('link-merge-confirm')));
    await tester.pumpAndSettle();
    expect(client.linkCalls.single, {'id': 'spk-2', 'member': 'Christina'});
    expect(find.text('Speaker 2'), findsNothing);
    expect(find.text('Household member · 7 voice clips'), findsOneWidget);
  });

  testWidgets('an already-linked member is shown but disabled', (tester) async {
    final client = FakeOrchestratorClient(
      householdMembers: ['Christina'],
      speakers: [_spk('spk-1', name: 'Christina')],
    );
    await tester.pumpWidget(MaterialApp(home: PeopleScreen(client: client)));
    await tester.pumpAndSettle();
    await openLink(tester, 'spk-1');
    expect(find.text('Already linked'), findsOneWidget);
    await tester.tap(find.byKey(const ValueKey('link-member-Christina')));
    await tester.pumpAndSettle();
    expect(client.linkCalls, isEmpty);
  });

  testWidgets('with no household roster, linking explains where to add people',
      (tester) async {
    final client = FakeOrchestratorClient(speakers: [_spk('spk-1')]);
    await tester.pumpWidget(MaterialApp(home: PeopleScreen(client: client)));
    await tester.pumpAndSettle();
    await openLink(tester, 'spk-1');
    expect(find.textContaining('No household members yet'), findsOneWidget);
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

  testWidgets('play button plays a person\'s voice clips', (tester) async {
    final client = FakeOrchestratorClient(
      speakers: [_spk('spk-2')],
      clipsBySpeaker: {
        'spk-2': const [
          SpeakerClipView(
              name: '100-0', createdMs: 100, samples: 16000, sampleRate: 16000),
          SpeakerClipView(
              name: '200-1', createdMs: 200, samples: 16000, sampleRate: 16000),
        ],
      },
    );
    await tester.pumpWidget(MaterialApp(home: PeopleScreen(client: client)));
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const ValueKey('speaker-play-spk-2')));
    await tester.pump(); // let the future resolve + snackbar appear
    await tester.pump();

    expect(client.playedClips, ['spk-2']);
    expect(find.textContaining('Playing 2 clips'), findsOneWidget);
  });

  testWidgets('play button reports when a person has no clips', (tester) async {
    final client = FakeOrchestratorClient(speakers: [_spk('spk-2')]);
    await tester.pumpWidget(MaterialApp(home: PeopleScreen(client: client)));
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const ValueKey('speaker-play-spk-2')));
    await tester.pump();
    await tester.pump();

    expect(client.playedClips, ['spk-2']);
    expect(find.textContaining('No voice clips saved yet'), findsOneWidget);
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
