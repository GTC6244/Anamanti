// Ambient Smart Display — app entry point (Plan.MD §3, Phases 5–6).
//
// An always-on landscape screen showing an idle photo slideshow that gives way to
// a live transcript + streamed reply during a voice turn, with returned TTS audio
// played by the Rust engine. Phase 6 adds a settings screen (wake word, assistant
// backend + voice, photo source, memory) reachable from a discreet control on the
// ambient screen; changing device-local settings restarts the engine and refreshes
// the slideshow, while assistant/memory settings are applied on the Mac.

import 'package:flutter/material.dart';

import 'package:ambient_display/src/engine/assistant_controller.dart';
import 'package:ambient_display/src/engine/wakeword_config.dart';
import 'package:ambient_display/src/settings/app_settings.dart';
import 'package:ambient_display/src/settings/orchestrator_client.dart';
import 'package:ambient_display/src/settings/settings_store.dart';
import 'package:ambient_display/src/slideshow/photo_source.dart';
import 'package:ambient_display/src/ui/ambient_screen.dart';
import 'package:ambient_display/src/ui/settings_screen.dart';
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

/// Owns the app's long-lived state: the persisted [AppSettings], the two
/// controllers (voice turn + slideshow), and the orchestrator client. Starts the
/// native engine once the wake-word config is resolved from settings, and reacts to
/// settings changes by restarting the engine and refreshing the slideshow source.
class AmbientHome extends StatefulWidget {
  const AmbientHome({super.key});

  @override
  State<AmbientHome> createState() => _AmbientHomeState();
}

class _AmbientHomeState extends State<AmbientHome> {
  final SettingsStore _store = SettingsStore();
  final OrchestratorClient _client = const FrbOrchestratorClient();
  final SlideshowController _slideshow = SlideshowController();

  AppSettings _settings = const AppSettings();
  AssistantController? _assistant;

  @override
  void initState() {
    super.initState();
    // Show the ambient slideshow immediately, independent of settings/connectivity.
    _slideshow.start();
    _boot();
  }

  Future<void> _boot() async {
    _settings = await _store.load();
    await _applyPhotoSource();
    await _startEngine();
  }

  Future<void> _applyPhotoSource() async {
    // The Google access token/URLs come from a completed on-device consent flow.
    // The default authenticator is a stub (real OAuth needs a client ID), so with
    // no token this resolves to the local ambient source — see [photoSourceFromSettings].
    final source = photoSourceFromSettings(_settings);
    await _slideshow.setSource(source);
  }

  Future<void> _startEngine() async {
    final config = await buildWakeWordConfigFrom(_settings);
    if (!mounted) return;
    _assistant?.dispose();
    final assistant = AssistantController(config: config)..start();
    setState(() => _assistant = assistant);
  }

  /// Apply settings changed on the [SettingsScreen] (already persisted there):
  /// refresh the slideshow if the photo source changed and restart the engine if a
  /// wake-word/threshold knob changed. Assistant + memory settings are applied on
  /// the Mac by the settings screen itself.
  Future<void> _onSettingsApplied(AppSettings next) async {
    final engineChanged = next.wakeWord != _settings.wakeWord ||
        next.threshold != _settings.threshold ||
        next.activeThreshold != _settings.activeThreshold;
    final photoChanged = next.photoSource != _settings.photoSource ||
        next.googleFolderName != _settings.googleFolderName ||
        next.googleLinked != _settings.googleLinked;

    _settings = next;
    if (photoChanged) await _applyPhotoSource();
    if (engineChanged) await _startEngine();
  }

  void _openSettings() {
    Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (_) => SettingsScreen(
          initial: _settings,
          store: _store,
          client: _client,
          onApplied: _onSettingsApplied,
        ),
      ),
    );
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
    return AmbientScreen(
      assistant: assistant,
      slideshow: _slideshow,
      onOpenSettings: _openSettings,
    );
  }
}
