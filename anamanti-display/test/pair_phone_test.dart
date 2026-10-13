// Widget tests for the "Link a phone" screen: enter a name + 4-char AppSaid
// pairing code, submit, and show the Core's confirmation or the failure reason.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:anamanti_display/src/ui/pair_phone_screen.dart';

import 'support/fake_orchestrator_client.dart';

void main() {
  testWidgets('submit is disabled until a name and a full code are entered',
      (tester) async {
    final client = FakeOrchestratorClient();
    await tester.pumpWidget(MaterialApp(home: PairPhoneScreen(client: client)));
    await tester.pumpAndSettle();

    FilledButton submit() =>
        tester.widget<FilledButton>(find.byKey(const Key('pair-submit')));
    expect(submit().onPressed, isNull, reason: 'disabled with empty fields');

    await tester.enterText(find.byKey(const Key('pair-name')), 'Mom');
    await tester.pump();
    expect(submit().onPressed, isNull, reason: 'still disabled without a code');

    // A 3-char code is too short.
    await tester.enterText(find.byKey(const Key('pair-code')), 'ab2');
    await tester.pump();
    expect(submit().onPressed, isNull, reason: 'disabled until 4 chars');

    await tester.enterText(find.byKey(const Key('pair-code')), 'ab2k');
    await tester.pump();
    expect(submit().onPressed, isNotNull, reason: 'enabled with name + 4 chars');
  });

  testWidgets('links a phone: sends normalized code, shows confirmation',
      (tester) async {
    final client = FakeOrchestratorClient();
    await tester.pumpWidget(MaterialApp(home: PairPhoneScreen(client: client)));
    await tester.pumpAndSettle();

    await tester.enterText(find.byKey(const Key('pair-name')), '  Mom ');
    // Lower-case with a dash — the field upper-cases, and the normalized code
    // sent to the Core strips the dash.
    await tester.enterText(find.byKey(const Key('pair-code')), 'ab-2k');
    await tester.pump();

    await tester.tap(find.byKey(const Key('pair-submit')));
    await tester.pumpAndSettle();

    expect(client.pairPhoneCalls, hasLength(1));
    expect(client.pairPhoneCalls.single['name'], 'Mom');
    expect(client.pairPhoneCalls.single['code'], 'AB2K');

    expect(find.byKey(const Key('pair-done-title')), findsOneWidget);
    expect(find.text("Linked Mom's phone."), findsOneWidget);
  });

  testWidgets('a rejected code surfaces the reason inline and does not navigate',
      (tester) async {
    final client = FakeOrchestratorClient()
      ..pairPhoneError =
          'that pairing code is invalid, already used, or expired';
    await tester.pumpWidget(MaterialApp(home: PairPhoneScreen(client: client)));
    await tester.pumpAndSettle();

    await tester.enterText(find.byKey(const Key('pair-name')), 'Dad');
    await tester.enterText(find.byKey(const Key('pair-code')), 'ZZZZ');
    await tester.pump();
    await tester.tap(find.byKey(const Key('pair-submit')));
    await tester.pumpAndSettle();

    // Stays on the form (no done step) and shows the stripped reason.
    expect(find.byKey(const Key('pair-done-title')), findsNothing);
    expect(
      find.text('that pairing code is invalid, already used, or expired'),
      findsOneWidget,
    );
  });
}
