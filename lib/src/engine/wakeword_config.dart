// Builds the [WakeWordConfig] handed to the native engine (Plan.MD §3, Phase 5).
//
// The engine needs the three openWakeWord ONNX model paths plus tuning. In Phase
// 5 these come from the app's model directory (populated by bundled assets or the
// Phase-6 settings screen). If the files are absent the engine degrades to
// capture-only mode — the UI still runs, showing mic levels — so a first run
// without models is graceful rather than fatal.

import 'dart:io';

import 'package:path_provider/path_provider.dart';

import 'package:ambient_display/src/rust/api/engine.dart';

/// Tuning defaults for the wake-word turn. `activeThreshold > threshold` is the
/// AEC-interim mitigation: raise the confidence bar while a turn is active so the
/// device's own speaker is less likely to self-trigger during playback.
class WakeWordDefaults {
  static const String modelName = 'alexa';
  static const double threshold = 0.5;
  static const double activeThreshold = 0.7;
  static const int discoveryTimeoutSecs = 3;
  static const int turnTimeoutSecs = 15;

  static const String melspecFile = 'melspectrogram.onnx';
  static const String embeddingFile = 'embedding_model.onnx';
  static String wakewordFile(String name) => '$name.onnx';
}

/// Resolve the wake-word model directory: `<app-support>/models`. Created on first
/// use so the settings screen (Phase 6) or an asset unpacker can drop models in.
Future<Directory> wakeWordModelDir() async {
  final support = await getApplicationSupportDirectory();
  final dir = Directory('${support.path}/models');
  if (!await dir.exists()) {
    await dir.create(recursive: true);
  }
  return dir;
}

/// Build a [WakeWordConfig] pointing at the resolved model directory. The paths
/// are always well-formed strings; whether the files exist is the engine's
/// concern (missing files → capture-only mode with a status event).
Future<WakeWordConfig> buildWakeWordConfig({
  String modelName = WakeWordDefaults.modelName,
  double threshold = WakeWordDefaults.threshold,
  double activeThreshold = WakeWordDefaults.activeThreshold,
}) async {
  final dir = await wakeWordModelDir();
  return WakeWordConfig(
    melspecModelPath: '${dir.path}/${WakeWordDefaults.melspecFile}',
    embeddingModelPath: '${dir.path}/${WakeWordDefaults.embeddingFile}',
    wakewordModelPath: '${dir.path}/${WakeWordDefaults.wakewordFile(modelName)}',
    modelName: modelName,
    threshold: threshold,
    activeThreshold: activeThreshold,
    discoveryTimeoutSecs: BigInt.from(WakeWordDefaults.discoveryTimeoutSecs),
    turnTimeoutSecs: BigInt.from(WakeWordDefaults.turnTimeoutSecs),
  );
}
