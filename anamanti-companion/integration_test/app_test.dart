import 'package:flutter_test/flutter_test.dart';
import 'package:anamanti_companion/main.dart';
import 'package:anamanti_companion/src/data/companion_api.dart';
import 'package:anamanti_companion/src/rust/api/companion.dart';
import 'package:anamanti_companion/src/rust/frb_generated.dart';
import 'package:anamanti_companion/src/state/connection_controller.dart';
import 'package:integration_test/integration_test.dart';

import '../test/support/fakes.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  setUpAll(() async => await RustLib.init());

  testWidgets('Rust engine is reachable and the shell boots', (tester) async {
    expect(engineVersion(), isNotEmpty);
    // A real (short) mDNS browse through Rust: it must complete, Core or not.
    final controller = ConnectionController(
      api: const RustCompanionApi(),
      store: MemorySelectionStore(),
      browseWindow: const Duration(seconds: 1),
    );
    await tester.pumpWidget(
      CompanionApp(controller: controller, api: const RustCompanionApi()),
    );
    await controller.start();
    await tester.pump();
    expect(controller.phase, isNot(ConnectionPhase.searching));
  });
}
