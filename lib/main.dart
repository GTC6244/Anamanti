// Ambient Smart Display — app entry point (Plan.MD §3, Phase 5).
//
// Phase 1's hello-world bridge check is replaced by the real ambient UI: an
// always-on landscape screen showing an idle photo slideshow that gives way to a
// live transcript + streamed reply during a voice turn, with returned TTS audio
// played by the Rust engine. The engine is started here and its event stream is
// folded into the reactive UI by [AssistantController].

import 'package:flutter/material.dart';

import 'package:ambient_display/src/engine/assistant_controller.dart';
import 'package:ambient_display/src/engine/wakeword_config.dart';
import 'package:ambient_display/src/slideshow/photo_source.dart';
import 'package:ambient_display/src/ui/ambient_screen.dart';
import 'package:ambient_display/src/ui/slideshow_view.dart';
import 'package:ambient_display/src/rust/frb_generated.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
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
      home: const AmbientHome(),
    );
  }
}

/// Owns the two controllers (voice turn + slideshow) for the app's lifetime and
/// starts the native engine once the wake-word config is resolved.
class AmbientHome extends StatefulWidget {
  const AmbientHome({super.key});

  @override
  State<AmbientHome> createState() => _AmbientHomeState();
}

class _AmbientHomeState extends State<AmbientHome> {
  final SlideshowController _slideshow = SlideshowController();
  AssistantController? _assistant;

  @override
  void initState() {
    super.initState();
    // The slideshow runs immediately and independently of connectivity.
    _slideshow.start();
    _startEngine();
  }

  Future<void> _startEngine() async {
    final config = await buildWakeWordConfig();
    if (!mounted) return;
    final assistant = AssistantController(config: config)..start();
    setState(() => _assistant = assistant);
  }

  @override
  void dispose() {
    _assistant?.dispose();
    _slideshow.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final assistant = _assistant;
    if (assistant == null) {
      // Before the engine config resolves, still show the ambient slideshow so the
      // screen is never blank on startup.
      return Scaffold(
        backgroundColor: Colors.black,
        body: SlideshowView(controller: _slideshow),
      );
    }
    return AmbientScreen(assistant: assistant, slideshow: _slideshow);
  }
}
