// Device-local settings persisted on the Echo Show (Plan.MD §3, Phase 6).
//
// These are the settings the *device* owns and applies locally: the wake word and
// its confidence thresholds (handed to the native engine as a
// [WakeWordConfig]) and the idle photo source (local vs a linked Google folder).
//
// The remotely-managed settings — LLM backend, model, and TTS voice — live on the
// Mac orchestrator and are read/changed over the control protocol
// (`OrchestratorClient`), not stored here.

import 'package:flutter/foundation.dart';

/// Which idle-screen photo source to use.
enum PhotoSourceKind { local, google }

/// The set of wake words the app offers in settings. These map to bundled/dropped
/// openWakeWord `.onnx` classifier files (`<name>.onnx`); if the file is absent the
/// engine degrades to capture-only, so the list is safe to show regardless.
const List<String> kAvailableWakeWords = <String>[
  'alexa',
  'hey_jarvis',
  'hey_mycroft',
  'ok_nabu',
];

@immutable
class AppSettings {
  const AppSettings({
    this.wakeWord = 'alexa',
    this.threshold = 0.5,
    this.activeThreshold = 0.7,
    this.photoSource = PhotoSourceKind.local,
    this.googleFolderName = '',
    this.googleLinked = false,
  });

  /// Selected wake-word model name (`<name>.onnx`).
  final String wakeWord;

  /// Idle-listening detection threshold in [0, 1].
  final double threshold;

  /// Higher threshold applied while a turn is active (the AEC-interim mitigation).
  /// Always coerced to at least [threshold] by the engine.
  final double activeThreshold;

  /// Idle photo source.
  final PhotoSourceKind photoSource;

  /// The chosen Google Photos album / Drive folder name (for display + loading).
  final String googleFolderName;

  /// Whether an on-device Google account has been linked (a token is present).
  final bool googleLinked;

  AppSettings copyWith({
    String? wakeWord,
    double? threshold,
    double? activeThreshold,
    PhotoSourceKind? photoSource,
    String? googleFolderName,
    bool? googleLinked,
  }) {
    return AppSettings(
      wakeWord: wakeWord ?? this.wakeWord,
      threshold: threshold ?? this.threshold,
      activeThreshold: activeThreshold ?? this.activeThreshold,
      photoSource: photoSource ?? this.photoSource,
      googleFolderName: googleFolderName ?? this.googleFolderName,
      googleLinked: googleLinked ?? this.googleLinked,
    );
  }

  Map<String, dynamic> toJson() => <String, dynamic>{
        'wakeWord': wakeWord,
        'threshold': threshold,
        'activeThreshold': activeThreshold,
        'photoSource': photoSource.name,
        'googleFolderName': googleFolderName,
        'googleLinked': googleLinked,
      };

  /// Parse from persisted JSON, tolerating missing/invalid keys by falling back to
  /// defaults so a partial or older settings file never crashes startup.
  factory AppSettings.fromJson(Map<String, dynamic> json) {
    const defaults = AppSettings();
    double asDouble(Object? v, double fallback) =>
        v is num ? v.toDouble().clamp(0.0, 1.0) : fallback;
    return AppSettings(
      wakeWord: json['wakeWord'] is String && (json['wakeWord'] as String).isNotEmpty
          ? json['wakeWord'] as String
          : defaults.wakeWord,
      threshold: asDouble(json['threshold'], defaults.threshold),
      activeThreshold: asDouble(json['activeThreshold'], defaults.activeThreshold),
      photoSource: PhotoSourceKind.values.firstWhere(
        (k) => k.name == json['photoSource'],
        orElse: () => defaults.photoSource,
      ),
      googleFolderName:
          json['googleFolderName'] is String ? json['googleFolderName'] as String : '',
      googleLinked: json['googleLinked'] == true,
    );
  }

  @override
  bool operator ==(Object other) =>
      other is AppSettings &&
      runtimeType == other.runtimeType &&
      wakeWord == other.wakeWord &&
      threshold == other.threshold &&
      activeThreshold == other.activeThreshold &&
      photoSource == other.photoSource &&
      googleFolderName == other.googleFolderName &&
      googleLinked == other.googleLinked;

  @override
  int get hashCode => Object.hash(
        wakeWord,
        threshold,
        activeThreshold,
        photoSource,
        googleFolderName,
        googleLinked,
      );
}
