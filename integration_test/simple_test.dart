import 'package:flutter_test/flutter_test.dart';
import 'package:ambient_display/main.dart';
import 'package:ambient_display/src/rust/frb_generated.dart';
import 'package:integration_test/integration_test.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  setUpAll(() async => await RustLib.init());
  testWidgets('Flutter can call into the Rust engine', (
    WidgetTester tester,
  ) async {
    await tester.pumpWidget(const AmbientDisplayApp());
    // The greeting only renders if the cross-compiled Rust `.so` loaded and the
    // FRB bridge returned a value.
    expect(find.textContaining('Rust engine is alive'), findsOneWidget);
    expect(find.textContaining('ambient-display engine v'), findsOneWidget);
  });
}
