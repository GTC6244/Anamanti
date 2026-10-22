import 'package:anamanti_companion/src/state/connection_controller.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/fakes.dart';

void main() {
  test('one Core and nothing saved → auto-select (not persisted)', () async {
    final api = FakeCompanionApi()..discoverResult = [sampleCore()];
    final store = MemorySelectionStore();
    final c = ConnectionController(api: api, store: store);
    await c.start();
    expect(c.phase, ConnectionPhase.connected);
    expect(c.core!.key, 'paul-family');
    expect(store.value, isNull);
  });

  test('several Cores → choose; pick is persisted', () async {
    final a = sampleCore(key: 'a', name: 'A');
    final b = sampleCore(key: 'b', name: 'B');
    final api = FakeCompanionApi()..discoverResult = [a, b];
    final store = MemorySelectionStore();
    final c = ConnectionController(api: api, store: store);
    await c.start();
    expect(c.phase, ConnectionPhase.choosing);
    await c.select(b);
    expect(c.phase, ConnectionPhase.connected);
    expect(store.value, 'b');
  });

  test('saved Core is preferred and passed to the browse', () async {
    final a = sampleCore(key: 'a');
    final b = sampleCore(key: 'b');
    final api = FakeCompanionApi()..discoverResult = [a, b];
    final c = ConnectionController(api: api, store: MemorySelectionStore('b'));
    await c.start();
    expect(api.discoverCalls, ['b']);
    expect(c.core!.key, 'b');
  });

  test(
    'saved Core missing → picker with a note, even with one other',
    () async {
      final api = FakeCompanionApi()..discoverResult = [sampleCore(key: 'a')];
      final c = ConnectionController(
        api: api,
        store: MemorySelectionStore('gone'),
      );
      await c.start();
      expect(c.phase, ConnectionPhase.choosing);
      expect(c.missingSavedKey, 'gone');
    },
  );

  test('nothing found / browse error → notFound', () async {
    final api = FakeCompanionApi();
    final c = ConnectionController(api: api, store: MemorySelectionStore());
    await c.start();
    expect(c.phase, ConnectionPhase.notFound);

    api.discoverResult = Exception('mDNS daemon failed');
    await c.discover();
    expect(c.phase, ConnectionPhase.notFound);
    expect(c.error, contains('mDNS daemon failed'));
  });

  test('repeated failures re-browse and pick up a moved Core', () async {
    final api = FakeCompanionApi()..discoverResult = [sampleCore()];
    var now = DateTime(2026);
    final c = ConnectionController(
      api: api,
      store: MemorySelectionStore(),
      now: () => now,
    );
    await c.start();
    api.discoverCalls.clear();

    api.discoverResult = [sampleCore(address: '192.168.1.77')];
    c.reportFailure();
    expect(api.discoverCalls, isEmpty, reason: 'one failure is tolerated');
    c.reportFailure();
    expect(c.health, ConnectionHealth.reconnecting);
    await Future<void>.delayed(Duration.zero);
    expect(api.discoverCalls, ['paul-family']);
    expect(c.health, ConnectionHealth.ok);
    expect(c.core!.address, '192.168.1.77');

    // Core gone: offline, and the cooldown rate-limits re-browses.
    api.discoverResult = [];
    c.reportFailure();
    c.reportFailure();
    await Future<void>.delayed(Duration.zero);
    expect(
      c.health,
      ConnectionHealth.ok,
      reason: 'within cooldown, no re-browse',
    );
    now = now.add(const Duration(seconds: 11));
    c.reportFailure();
    await Future<void>.delayed(Duration.zero);
    expect(c.health, ConnectionHealth.offline);
    expect(c.phase, ConnectionPhase.connected, reason: 'tabs stay up');

    c.reportSuccess();
    expect(c.health, ConnectionHealth.ok);
  });
}
