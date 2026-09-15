// Phase 1 hello-world app (Plan.MD §3, Phase 1).
//
// This screen exists only to prove the full toolchain end-to-end: Flutter (Dart)
// calling the cross-compiled Rust engine over the flutter_rust_bridge v2 boundary
// on the aarch64-linux-android device. Later phases replace this with the real
// ambient UI (idle slideshow, live transcript, streaming reply).

import 'package:flutter/material.dart';
import 'package:ambient_display/src/rust/api/engine.dart';
import 'package:ambient_display/src/rust/frb_generated.dart';

Future<void> main() async {
  await RustLib.init();
  runApp(const AmbientDisplayApp());
}

class AmbientDisplayApp extends StatelessWidget {
  const AmbientDisplayApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Ambient Display',
      debugShowCheckedModeBanner: false,
      theme: ThemeData.dark(useMaterial3: true),
      home: const BridgeCheckScreen(),
    );
  }
}

/// Landscape-first screen that calls into the native Rust engine and shows the
/// result — confirming the FRB bridge and the cross-compiled `.so` are live.
class BridgeCheckScreen extends StatelessWidget {
  const BridgeCheckScreen({super.key});

  @override
  Widget build(BuildContext context) {
    final greeting = engineGreeting(name: 'Echo Show');
    final version = engineVersion();

    return Scaffold(
      body: Center(
        child: Column(
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            const Text('🎙️', style: TextStyle(fontSize: 56)),
            const SizedBox(height: 16),
            const Text(
              'Ambient Smart Display',
              style: TextStyle(fontSize: 28, fontWeight: FontWeight.bold),
            ),
            const SizedBox(height: 24),
            Text(greeting, style: const TextStyle(fontSize: 18)),
            const SizedBox(height: 8),
            Text(
              version,
              style: const TextStyle(fontSize: 14, color: Colors.tealAccent),
            ),
          ],
        ),
      ),
    );
  }
}
