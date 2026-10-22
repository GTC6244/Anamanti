import 'package:anamanti_companion/src/ui/status_screen.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/fakes.dart';
import 'support/host.dart';

void main() {
  testWidgets('renders health, pipeline stages with tiers, displays, latency', (
    tester,
  ) async {
    final api = FakeCompanionApi();
    await tester.pumpWidget(
      host(StatusScreen(api: api, core: sampleCore(), active: true)),
    );
    await tester.pump();
    await tester.pump();

    // First paint skips probes, then probes for real health.
    expect(api.overviewProbes, [false, true]);
    expect(find.text('1 stage unreachable'), findsOneWidget);
    expect(find.text('Speech-to-text'), findsOneWidget);
    expect(find.text('Wyoming Whisper'), findsOneWidget);
    expect(find.text('unreachable'), findsOneWidget);
    expect(find.text('LAN'), findsOneWidget);
    expect(find.text('Local'), findsOneWidget);
    expect(find.text('Cloud'), findsOneWidget);
    // An "off" stage shows its tier but no health pill.
    expect(find.text('Off'), findsWidgets);
    await tester.scrollUntilVisible(find.text('Kitchen Show'), 200);
    expect(find.text('Kitchen Show'), findsOneWidget);
    await tester.scrollUntilVisible(find.text('p50 1.3 s · p95 2.9 s'), 200);
    expect(find.text('p50 1.3 s · p95 2.9 s'), findsOneWidget);
  });

  testWidgets('periodic refresh uses probe=0 and only while active', (
    tester,
  ) async {
    final api = FakeCompanionApi();
    await tester.pumpWidget(
      host(
        StatusScreen(
          api: api,
          core: sampleCore(),
          active: true,
          pollInterval: const Duration(seconds: 10),
        ),
      ),
    );
    await tester.pump();
    await tester.pump();
    api.overviewProbes.clear();

    await tester.pump(const Duration(seconds: 10));
    expect(api.overviewProbes, [false]);

    await tester.pumpWidget(
      host(
        StatusScreen(
          api: api,
          core: sampleCore(),
          active: false,
          pollInterval: const Duration(seconds: 10),
        ),
      ),
    );
    await tester.pump(const Duration(seconds: 30));
    expect(api.overviewProbes, [false], reason: 'hidden tab must not poll');
  });

  testWidgets('all-healthy header', (tester) async {
    final api = FakeCompanionApi()
      ..overviewResult = sampleOverviewJson(
        stages: [
          {
            'key': 'tts',
            'label': 'Text-to-speech',
            'engine': 'Piper',
            'tier': 'lan',
            'status': 'ok',
          },
        ],
      );
    await tester.pumpWidget(
      host(StatusScreen(api: api, core: sampleCore(), active: true)),
    );
    await tester.pump();
    await tester.pump();
    expect(find.text('All stages healthy'), findsOneWidget);
    expect(find.text('healthy'), findsOneWidget);
  });

  testWidgets('error state with retry', (tester) async {
    final api = FakeCompanionApi()
      ..overviewResult = Exception('could not reach the Core');
    final reach = <bool>[];
    await tester.pumpWidget(
      host(
        StatusScreen(
          api: api,
          core: sampleCore(),
          active: true,
          onReachability: reach.add,
        ),
      ),
    );
    await tester.pump();
    await tester.pump();
    expect(find.text("Couldn't load the Core's status"), findsOneWidget);
    expect(reach, contains(false));

    api.overviewResult = sampleOverviewJson();
    await tester.tap(find.text('Retry'));
    await tester.pump();
    expect(find.text('Speech-to-text'), findsOneWidget);
    expect(reach.last, isTrue);
  });

  testWidgets('no companion_port → clear message, no requests', (tester) async {
    final api = FakeCompanionApi();
    await tester.pumpWidget(
      host(
        StatusScreen(
          api: api,
          core: sampleCore(companionPort: null),
          active: true,
        ),
      ),
    );
    await tester.pump();
    expect(
      find.text("This Core doesn't have the companion API turned on"),
      findsOneWidget,
    );
    expect(api.overviewProbes, isEmpty);
  });
}
