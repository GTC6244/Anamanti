import 'package:anamanti_companion/src/data/companion_api.dart';
import 'package:anamanti_companion/src/ui/timers_screen.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/fakes.dart';
import 'support/host.dart';

void main() {
  TimersSnapshot snapshot() => TimersSnapshot(
    nowMs: 0,
    displays: [
      DisplayTimers(
        deviceId: 'anamanti-bb',
        name: 'Living Room',
        reportedAtMs: 0,
        timers: [sampleTimer(id: 1, label: 'pasta', remaining: 125)],
      ),
      DisplayTimers(
        deviceId: 'anamanti-aa',
        name: '',
        reportedAtMs: 0,
        timers: [
          sampleTimer(id: 2, label: 'eggs', remaining: 0, ringing: true),
        ],
      ),
      const DisplayTimers(
        deviceId: 'anamanti-cc',
        name: 'Office',
        reportedAtMs: 0,
        timers: [],
      ),
    ],
  );

  testWidgets(
    'groups by display name (device id fallback), ringing highlighted',
    (tester) async {
      final api = FakeCompanionApi()..timersResult = snapshot();
      await tester.pumpWidget(
        host(TimersScreen(api: api, core: sampleCore(), active: true)),
      );
      await tester.pump();

      expect(find.text('Living Room'), findsOneWidget);
      expect(
        find.text('anamanti-aa'),
        findsOneWidget,
        reason: 'unnamed display falls back to device id',
      );
      expect(
        find.text('Office'),
        findsNothing,
        reason: 'displays without timers are omitted',
      );
      expect(find.text('pasta'), findsOneWidget);
      expect(find.text('2:05'), findsOneWidget);
      expect(find.text('Ringing'), findsOneWidget);
      expect(find.byIcon(Icons.alarm), findsOneWidget);
    },
  );

  testWidgets('counts down locally from remaining_secs between polls', (
    tester,
  ) async {
    final clock = ManualClock();
    final api = FakeCompanionApi()..timersResult = snapshot();
    await tester.pumpWidget(
      host(
        TimersScreen(
          api: api,
          core: sampleCore(),
          active: true,
          clock: clock.call,
          pollInterval: const Duration(minutes: 10),
        ),
      ),
    );
    await tester.pump();
    expect(find.text('2:05'), findsOneWidget);

    clock.advance(const Duration(seconds: 6));
    await tester.pump(const Duration(seconds: 1));
    expect(find.text('1:59'), findsOneWidget);

    clock.advance(const Duration(minutes: 5));
    await tester.pump(const Duration(seconds: 1));
    expect(find.text("Time's up"), findsOneWidget);
  });

  testWidgets('a fresh poll re-anchors the countdown', (tester) async {
    final clock = ManualClock();
    final api = FakeCompanionApi()..timersResult = snapshot();
    await tester.pumpWidget(
      host(
        TimersScreen(
          api: api,
          core: sampleCore(),
          active: true,
          clock: clock.call,
        ),
      ),
    );
    await tester.pump();
    expect(api.timersCalls, 1);

    clock.advance(const Duration(seconds: 5));
    api.timersResult = TimersSnapshot(
      nowMs: 0,
      displays: [
        DisplayTimers(
          deviceId: 'x',
          name: 'Kitchen',
          reportedAtMs: 0,
          timers: [sampleTimer(remaining: 300)],
        ),
      ],
    );
    await tester.pump(const Duration(seconds: 5));
    await tester.pump();
    expect(api.timersCalls, 2);
    expect(find.text('5:00'), findsOneWidget);
  });

  testWidgets('empty state', (tester) async {
    final api = FakeCompanionApi();
    await tester.pumpWidget(
      host(TimersScreen(api: api, core: sampleCore(), active: true)),
    );
    await tester.pump();
    expect(find.text('No timers running'), findsOneWidget);
  });

  testWidgets('a Core without /timers.json gets a friendly message', (
    tester,
  ) async {
    final api = FakeCompanionApi()
      ..timersResult = Exception('HTTP 404: not found');
    await tester.pumpWidget(
      host(TimersScreen(api: api, core: sampleCore(), active: true)),
    );
    await tester.pump();
    expect(find.textContaining("doesn't report timers yet"), findsOneWidget);
  });

  testWidgets('no companion_port → clear message', (tester) async {
    final api = FakeCompanionApi();
    await tester.pumpWidget(
      host(
        TimersScreen(
          api: api,
          core: sampleCore(companionPort: null),
          active: true,
        ),
      ),
    );
    expect(
      find.text("This Core doesn't have the companion API turned on"),
      findsOneWidget,
    );
    expect(api.timersCalls, 0);
  });
}
