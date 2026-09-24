// Tests for the proactive-notification controller (Approach A, visual-only).

import 'dart:async';

import 'package:ambient_display/src/engine/notification_controller.dart';
import 'package:ambient_display/src/rust/api/engine.dart';
import 'package:flutter_test/flutter_test.dart';

NotifyConfig _config() => NotifyConfig(
      orchestratorKey: '',
      discoveryTimeoutSecs: BigInt.zero,
      deviceId: 'test',
    );

NotifyEvent _event(String id) =>
    NotifyEvent(id: id, priority: 'info', title: 'T $id', body: 'B $id');

void main() {
  test('surfaces the latest pushed notification', () {
    final controller = StreamController<NotifyEvent>.broadcast();
    addTearDown(controller.close);
    final c = NotificationController(
      config: _config(),
      startChannel: (_) => controller.stream,
      autoDismiss: Duration.zero, // no auto-dismiss for this test
    )..start();
    addTearDown(c.dispose);

    var notified = 0;
    c.addListener(() => notified++);

    expect(c.current, isNull);
    controller.add(_event('1'));
    // The stream delivers asynchronously; pump the microtask queue.
    return Future<void>.delayed(Duration.zero, () {
      expect(c.current?.id, '1');
      expect(notified, 1);
      controller.add(_event('2'));
      return Future<void>.delayed(Duration.zero, () {
        expect(c.current?.id, '2');
      });
    });
  });

  test('manual dismiss clears the current notification', () async {
    final controller = StreamController<NotifyEvent>.broadcast();
    addTearDown(controller.close);
    final c = NotificationController(
      config: _config(),
      startChannel: (_) => controller.stream,
      autoDismiss: Duration.zero,
    )..start();
    addTearDown(c.dispose);

    controller.add(_event('1'));
    await Future<void>.delayed(Duration.zero);
    expect(c.current, isNotNull);

    c.dismiss();
    expect(c.current, isNull);
  });

  test('auto-dismiss clears after the configured delay', () {
    return fakeAsyncTest();
  });
}

/// Uses a real timer window rather than FakeAsync to keep the dependency surface
/// minimal: a short auto-dismiss and a slightly longer wait.
Future<void> fakeAsyncTest() async {
  final controller = StreamController<NotifyEvent>.broadcast();
  addTearDown(controller.close);
  final c = NotificationController(
    config: _config(),
    startChannel: (_) => controller.stream,
    autoDismiss: const Duration(milliseconds: 20),
  )..start();
  addTearDown(c.dispose);

  controller.add(_event('1'));
  await Future<void>.delayed(Duration.zero);
  expect(c.current, isNotNull);

  await Future<void>.delayed(const Duration(milliseconds: 40));
  expect(c.current, isNull, reason: 'auto-dismiss should have fired');
}
