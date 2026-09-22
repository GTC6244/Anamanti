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
import 'package:ambient_display/src/settings/app_settings.dart';

/// Tuning defaults for the wake-word turn. `activeThreshold > threshold` is the
/// AEC-interim mitigation: raise the confidence bar while a turn is active so the
/// device's own speaker is less likely to self-trigger during playback.
class WakeWordDefaults {
  static const String modelName = 'hey_jarvis';
  static const double threshold = 0.5;
  static const double activeThreshold = 0.7;
  static const int smoothingWindow = 2;
  static const bool fireOnPeak = false;
  static const int playbackBufferSecs = 30;
  static const int discoveryTimeoutSecs = 3;
  static const int turnTimeoutSecs = 15;

  /// Capture backend + effects (Android only). Default is the `cpal` path; the
  /// AudioRecord path opens the HAL's far-field `VOICE_RECOGNITION` source
  /// (`AudioSource.VOICE_RECOGNITION` == 6) and attaches platform NS/AGC. Platform
  /// AEC is left off — the host-side WebRTC APM does echo cancellation, and this
  /// device's platform AEC was found not to actually cancel.
  static const bool useAudioRecord = true;
  static const int micSource = 6;
  static const bool platformAec = false;
  static const bool platformAgc = true;
  static const bool platformNs = true;

  /// Camera-as-proximity sensor (Android only): a cheap Rust frame-motion detector
  /// on low-res front-camera luma brightens the idle screen when someone approaches
  /// and dims it after a quiet spell (Plan.MD §5). On by default. The `0` tuning
  /// values tell the engine to use its built-in motion threshold / release window
  /// (`camera/presence.rs`).
  static const bool cameraProximity = true;
  static const double proximityMotionThreshold = 0.0;
  static const int proximityReleaseSecs = 0;

  /// Absolute window brightness applied when the proximity sensor reports someone is
  /// present vs. when the room has been quiet. Presentation-side (see
  /// `ScreenBrightnessController`); the engine only reports presence.
  static const double brightnessNear = 1.0;
  static const double brightnessAway = 0.25;

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
  int smoothingWindow = WakeWordDefaults.smoothingWindow,
  bool fireOnPeak = WakeWordDefaults.fireOnPeak,
  int playbackBufferSecs = WakeWordDefaults.playbackBufferSecs,
  bool useAudioRecord = WakeWordDefaults.useAudioRecord,
  int micSource = WakeWordDefaults.micSource,
  bool platformAec = WakeWordDefaults.platformAec,
  bool platformAgc = WakeWordDefaults.platformAgc,
  bool platformNs = WakeWordDefaults.platformNs,
  bool cameraProximity = WakeWordDefaults.cameraProximity,
  double proximityMotionThreshold = WakeWordDefaults.proximityMotionThreshold,
  int proximityReleaseSecs = WakeWordDefaults.proximityReleaseSecs,
  String orchestratorKey = '',
}) async {
  final dir = await wakeWordModelDir();
  return WakeWordConfig(
    melspecModelPath: '${dir.path}/${WakeWordDefaults.melspecFile}',
    embeddingModelPath: '${dir.path}/${WakeWordDefaults.embeddingFile}',
    wakewordModelPath: '${dir.path}/${WakeWordDefaults.wakewordFile(modelName)}',
    modelName: modelName,
    threshold: threshold,
    activeThreshold: activeThreshold,
    orchestratorKey: orchestratorKey,
    discoveryTimeoutSecs: BigInt.from(WakeWordDefaults.discoveryTimeoutSecs),
    turnTimeoutSecs: BigInt.from(WakeWordDefaults.turnTimeoutSecs),
    smoothingWindow: smoothingWindow,
    fireOnPeak: fireOnPeak,
    playbackBufferSecs: playbackBufferSecs,
    useAudiorecord: useAudioRecord,
    micSource: micSource,
    platformAec: platformAec,
    platformAgc: platformAgc,
    platformNs: platformNs,
    cameraProximity: cameraProximity,
    proximityMotionThreshold: proximityMotionThreshold,
    proximityReleaseSecs: proximityReleaseSecs,
  );
}

/// Build a [WakeWordConfig] from the user's persisted [AppSettings] (Phase 6):
/// the settings screen chooses the wake word and detection/playback tuning, and
/// this maps them to the native engine config (resolving the model paths under the
/// app model dir).
Future<WakeWordConfig> buildWakeWordConfigFrom(AppSettings settings) {
  return buildWakeWordConfig(
    modelName: settings.wakeWord,
    threshold: settings.threshold,
    activeThreshold: settings.activeThreshold,
    smoothingWindow: settings.smoothingWindow,
    fireOnPeak: settings.fireOnPeak,
    playbackBufferSecs: settings.playbackBufferSecs,
    useAudioRecord: settings.useAudioRecord,
    orchestratorKey: settings.orchestratorKey,
  );
}
