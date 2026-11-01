import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/engine/wake_trace.dart';
import 'package:anamanti_display/src/ui/wake_log_view.dart';

void main() {
  group('wakeTraceStateLine', () {
    test('describes what the screen was doing', () {
      const state = AssistantState(userPresent: false, online: true);
      expect(
        wakeTraceStateLine(state, dark: true, brightness: 0.02),
        'state phase=idle awake=false present=false dark=true '
        'brightness=0.02 online=true',
      );
    });

    test('changes when the screen wakes, so the change gets logged', () {
      const away = AssistantState(userPresent: false);
      const near = AssistantState(userPresent: true);
      expect(
        wakeTraceStateLine(away, dark: false, brightness: 0.25),
        isNot(wakeTraceStateLine(near, dark: false, brightness: 1)),
      );
    });
  });

  group('WakeLogView', () {
    Future<void> pump(
      WidgetTester tester,
      Future<String> Function() load,
    ) async {
      await tester.pumpWidget(MaterialApp(home: WakeLogView(load: load)));
      await tester.pumpAndSettle();
    }

    testWidgets('shows the log and where the full files are', (tester) async {
      await pump(tester, () async => 'line one\nline two');

      expect(find.textContaining('line two'), findsOneWidget);
      expect(find.textContaining('adb pull'), findsOneWidget);
    });

    testWidgets('says so when nothing has been logged', (tester) async {
      await pump(tester, () async => '');

      expect(find.text('Nothing logged yet'), findsOneWidget);
    });

    testWidgets('opens at the newest line', (tester) async {
      final log = List.generate(400, (i) => 'line $i').join('\n');
      await pump(tester, () async => log);

      final scroll = tester.widget<SingleChildScrollView>(
        find.byType(SingleChildScrollView),
      );
      final position = scroll.controller!.position;
      expect(position.maxScrollExtent, greaterThan(0));
      expect(position.pixels, position.maxScrollExtent);
    });

    testWidgets('refresh loads the newer lines', (tester) async {
      var calls = 0;
      await pump(tester, () async => 'load ${++calls}');
      expect(find.textContaining('load 1'), findsOneWidget);

      await tester.tap(find.byKey(const Key('wake-log-refresh')));
      await tester.pumpAndSettle();

      expect(find.textContaining('load 2'), findsOneWidget);
    });
  });
}
