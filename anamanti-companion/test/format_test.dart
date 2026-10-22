import 'package:anamanti_companion/src/data/overview.dart';
import 'package:anamanti_companion/src/util/format.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/fakes.dart';

void main() {
  test('clock and duration formatting', () {
    expect(formatClock(0), '0:00');
    expect(formatClock(125), '2:05');
    expect(formatClock(3725), '1:02:05');
    expect(formatClock(-4), '0:00');
    expect(formatDuration(45), '45s');
    expect(formatDuration(600), '10m');
    expect(formatDuration(7260), '2h 1m');
    expect(formatDuration(90000), '1d 1h');
    expect(formatMs(null), '—');
    expect(formatMs(850), '850 ms');
    expect(formatMs(1250), '1.3 s');
  });

  test('overview parses the Core shape and tolerates gaps', () {
    final o = Overview.parse(sampleOverviewJson());
    expect(o.version, '0.9.0');
    expect(o.stages.length, 4);
    expect(o.stages[1].tier, 'lan');
    expect(o.unreachable.single.key, 'stt');
    expect(o.displays.single.name, 'Kitchen Show');
    expect(o.p95Ms, 2900);

    final sparse = Overview.parse('{"ok":true,"stages":[{"key":"llm"}]}');
    expect(sparse.stages.single.status, 'unknown');
    expect(sparse.stages.single.tier, 'off');
    expect(sparse.latencyCount, 0);
    expect(() => Overview.parse('[]'), throwsFormatException);
  });
}
