import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'package:anamanti_companion/src/data/companion_api.dart';
import 'package:anamanti_companion/src/data/selection_store.dart';
import 'package:anamanti_companion/src/rust/frb_generated.dart';
import 'package:anamanti_companion/src/state/connection_controller.dart';
import 'package:anamanti_companion/src/ui/home_shell.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await RustLib.init();
  await SystemChrome.setPreferredOrientations([DeviceOrientation.portraitUp]);
  const api = RustCompanionApi();
  final controller = ConnectionController(
    api: api,
    store: PrefsSelectionStore(),
  )..start();
  runApp(CompanionApp(controller: controller, api: api));
}

/// Anamanti Companion — the phone app for the Anamanti Core: system status and
/// the live pipeline, what's playing, every display's timers, and memories.
class CompanionApp extends StatelessWidget {
  const CompanionApp({super.key, required this.controller, required this.api});

  final ConnectionController controller;
  final CompanionApi api;

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Anamanti Companion',
      theme: ThemeData(colorSchemeSeed: Colors.teal, useMaterial3: true),
      darkTheme: ThemeData(
        colorSchemeSeed: Colors.teal,
        brightness: Brightness.dark,
        useMaterial3: true,
      ),
      home: HomeShell(controller: controller, api: api),
    );
  }
}
