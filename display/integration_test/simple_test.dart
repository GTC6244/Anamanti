// On-device smoke test for the Phase 5 ambient app.
//
// Runs the real app with the native RustLib loaded (unlike `flutter test`), so it
// exercises `RustLib.init()`, the FRB bridge, and engine startup on the device.
// It verifies the ambient screen comes up in its idle state; the full voice-turn
// path (wake word → STT → LLM → TTS playback) needs a live Wyoming host and is
// validated separately.

import 'package:flutter_test/flutter_test.dart';
import 'package:ambient_display/main.dart';
import 'package:ambient_display/src/rust/frb_generated.dart';
import 'package:integration_test/integration_test.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  setUpAll(() async => await RustLib.init());

  testWidgets('ambient app boots into its idle screen', (tester) async {
    await tester.pumpWidget(const AmbientDisplayApp());
    // Let the async wake-word config resolve and the first engine events arrive.
    await tester.pump(const Duration(seconds: 1));
    await tester.pump(const Duration(seconds: 1));

    // The idle ambient screen shows the wake-word hint (no turn is active).
    expect(find.text('Say the wake word to begin'), findsOneWidget);
  });
}
