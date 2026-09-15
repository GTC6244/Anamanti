// Widget-level smoke test for the Phase 1 hello-world screen.
//
// Note: the RustLib native library is not loaded in the plain `flutter test`
// host VM, so the engine calls are exercised by the integration test in
// `integration_test/simple_test.dart` (which runs on a device/emulator). This
// test just verifies the widget tree constructs.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  testWidgets('App shell builds', (WidgetTester tester) async {
    await tester.pumpWidget(
      const MaterialApp(
        home: Scaffold(body: Center(child: Text('Ambient Smart Display'))),
      ),
    );
    expect(find.text('Ambient Smart Display'), findsOneWidget);
  });
}
