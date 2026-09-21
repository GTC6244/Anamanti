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
///  * [local]   — built-in ambient gradients (offline fallback).
///  * [ambient] — Google Photos via the Ambient API (device-code/QR; partner-gated).
///  * [drive]   — a Google Drive folder (`drive.readonly`; consent on the Mac).
enum PhotoSourceKind { local, ambient, drive }

/// The set of wake words the app offers in settings. These map to openWakeWord
/// `.onnx` classifier files (`<name>.onnx`) in the model dir. Only `hey_jarvis`
/// ships bundled in the app (see `assets/models/`); the others work once the
/// matching classifier is dropped into the model dir — otherwise the engine
/// degrades to capture-only, so the list is safe to show regardless.
const List<String> kAvailableWakeWords = <String>[
  'hey_jarvis',
  'alexa',
  'hey_mycroft',
  'ok_nabu',
];

@immutable
class AppSettings {
  const AppSettings({
    this.wakeWord = 'hey_jarvis',
    this.threshold = 0.5,
    this.activeThreshold = 0.7,
    this.smoothingWindow = 2,
    this.fireOnPeak = false,
    this.playbackBufferSecs = 30,
    this.useAudioRecord = true,
    this.endpointCueEnabled = true,
    this.endpointSilenceMs = 600,
    this.endpointRmsThreshold = 0.012,
    this.photoSource = PhotoSourceKind.local,
    this.ambientRefreshToken = '',
    this.ambientDeviceId = '',
    this.ambientLinked = false,
    this.driveRefreshToken = '',
    this.driveFolderIds = const <String>[],
    this.driveLinked = false,
  });

  /// Selected wake-word model name (`<name>.onnx`).
  final String wakeWord;

  /// Idle-listening detection threshold in [0, 1].
  final double threshold;

  /// Higher threshold applied while a turn is active (the AEC-interim mitigation).
  /// Always coerced to at least [threshold] by the engine.
  final double activeThreshold;

  /// Wake-word detection smoothing window (blocks averaged/peaked before a fire).
  /// Lower = snappier response to brief/quiet wake words. A/B-tunable.
  final int smoothingWindow;

  /// When true, the detection gate fires on the *peak* score in the smoothing
  /// window rather than its average — much more responsive to short/faint "hey
  /// jarvis" utterances at a slightly higher false-trigger rate.
  final bool fireOnPeak;

  /// Speaker playback buffer depth in seconds. Must exceed the longest expected
  /// spoken reply so long TTS answers are not truncated. A/B-tunable.
  final int playbackBufferSecs;

  /// **Android only.** Capture through the Kotlin `AudioRecord` layer
  /// (`VOICE_RECOGNITION` source + platform noise-suppression/AGC) instead of the
  /// default `cpal` path. A/B-tunable on-device to compare far-field pickup.
  final bool useAudioRecord;

  /// Whether the device shows a local "processing" cue the instant the user stops
  /// speaking, instead of waiting for the Mac's VAD + transcript round trip.
  final bool endpointCueEnabled;

  /// Trailing silence (ms) the local end-of-speech cue waits for before flipping to
  /// the "processing" phase.
  final int endpointSilenceMs;

  /// Mic RMS level (0..1) below which audio counts as silence for the local cue.
  final double endpointRmsThreshold;

  /// Idle photo source.
  final PhotoSourceKind photoSource;

  /// Ambient API (Google Photos) OAuth refresh token from the on-device device-code
  /// flow, persisted so the slideshow re-mints an access token on boot without
  /// re-scanning the QR. TODO: move to platform secure storage.
  final String ambientRefreshToken;

  /// The Ambient API device id created during linking; used to list the user's
  /// picked media.
  final String ambientDeviceId;

  /// Whether Google Photos (Ambient API) has been linked.
  final bool ambientLinked;

  /// Google Drive OAuth refresh token (issued by the Desktop client via Mac
  /// consent), persisted so the slideshow re-mints an access token on boot.
  /// TODO: move to platform secure storage.
  final String driveRefreshToken;

  /// Drive folder IDs the slideshow pulls images from (owned or "Shared with me").
  final List<String> driveFolderIds;

  /// Whether Google Drive has been linked (a refresh token is held).
  final bool driveLinked;

  AppSettings copyWith({
    String? wakeWord,
    double? threshold,
    double? activeThreshold,
    int? smoothingWindow,
    bool? fireOnPeak,
    int? playbackBufferSecs,
    bool? useAudioRecord,
    bool? endpointCueEnabled,
    int? endpointSilenceMs,
    double? endpointRmsThreshold,
    PhotoSourceKind? photoSource,
    String? ambientRefreshToken,
    String? ambientDeviceId,
    bool? ambientLinked,
    String? driveRefreshToken,
    List<String>? driveFolderIds,
    bool? driveLinked,
  }) {
    return AppSettings(
      wakeWord: wakeWord ?? this.wakeWord,
      threshold: threshold ?? this.threshold,
      activeThreshold: activeThreshold ?? this.activeThreshold,
      smoothingWindow: smoothingWindow ?? this.smoothingWindow,
      fireOnPeak: fireOnPeak ?? this.fireOnPeak,
      playbackBufferSecs: playbackBufferSecs ?? this.playbackBufferSecs,
      useAudioRecord: useAudioRecord ?? this.useAudioRecord,
      endpointCueEnabled: endpointCueEnabled ?? this.endpointCueEnabled,
      endpointSilenceMs: endpointSilenceMs ?? this.endpointSilenceMs,
      endpointRmsThreshold: endpointRmsThreshold ?? this.endpointRmsThreshold,
      photoSource: photoSource ?? this.photoSource,
      ambientRefreshToken: ambientRefreshToken ?? this.ambientRefreshToken,
      ambientDeviceId: ambientDeviceId ?? this.ambientDeviceId,
      ambientLinked: ambientLinked ?? this.ambientLinked,
      driveRefreshToken: driveRefreshToken ?? this.driveRefreshToken,
      driveFolderIds: driveFolderIds ?? this.driveFolderIds,
      driveLinked: driveLinked ?? this.driveLinked,
    );
  }

  Map<String, dynamic> toJson() => <String, dynamic>{
    'wakeWord': wakeWord,
    'threshold': threshold,
    'activeThreshold': activeThreshold,
    'smoothingWindow': smoothingWindow,
    'fireOnPeak': fireOnPeak,
    'playbackBufferSecs': playbackBufferSecs,
    'useAudioRecord': useAudioRecord,
    'endpointCueEnabled': endpointCueEnabled,
    'endpointSilenceMs': endpointSilenceMs,
    'endpointRmsThreshold': endpointRmsThreshold,
    'photoSource': photoSource.name,
    'ambientRefreshToken': ambientRefreshToken,
    'ambientDeviceId': ambientDeviceId,
    'ambientLinked': ambientLinked,
    'driveRefreshToken': driveRefreshToken,
    'driveFolderIds': driveFolderIds,
    'driveLinked': driveLinked,
  };

  /// Parse from persisted JSON, tolerating missing/invalid keys by falling back to
  /// defaults so a partial or older settings file never crashes startup.
  factory AppSettings.fromJson(Map<String, dynamic> json) {
    const defaults = AppSettings();
    double asDouble(Object? v, double fallback) =>
        v is num ? v.toDouble().clamp(0.0, 1.0) : fallback;
    int asInt(Object? v, int fallback, {int min = 1, int max = 1 << 30}) =>
        v is num ? v.toInt().clamp(min, max) : fallback;
    return AppSettings(
      wakeWord:
          json['wakeWord'] is String && (json['wakeWord'] as String).isNotEmpty
          ? json['wakeWord'] as String
          : defaults.wakeWord,
      threshold: asDouble(json['threshold'], defaults.threshold),
      activeThreshold: asDouble(
        json['activeThreshold'],
        defaults.activeThreshold,
      ),
      smoothingWindow: asInt(
        json['smoothingWindow'],
        defaults.smoothingWindow,
        min: 1,
        max: 10,
      ),
      fireOnPeak: json['fireOnPeak'] == true,
      playbackBufferSecs: asInt(
        json['playbackBufferSecs'],
        defaults.playbackBufferSecs,
        min: 2,
        max: 120,
      ),
      useAudioRecord: json['useAudioRecord'] is bool
          ? json['useAudioRecord'] as bool
          : defaults.useAudioRecord,
      endpointCueEnabled: json['endpointCueEnabled'] is bool
          ? json['endpointCueEnabled'] as bool
          : defaults.endpointCueEnabled,
      endpointSilenceMs: asInt(
        json['endpointSilenceMs'],
        defaults.endpointSilenceMs,
        min: 100,
        max: 3000,
      ),
      endpointRmsThreshold: asDouble(
        json['endpointRmsThreshold'],
        defaults.endpointRmsThreshold,
      ),
      photoSource: PhotoSourceKind.values.firstWhere(
        (k) => k.name == json['photoSource'],
        orElse: () => defaults.photoSource,
      ),
      ambientRefreshToken: json['ambientRefreshToken'] is String
          ? json['ambientRefreshToken'] as String
          : '',
      ambientDeviceId: json['ambientDeviceId'] is String
          ? json['ambientDeviceId'] as String
          : '',
      ambientLinked: json['ambientLinked'] == true,
      driveRefreshToken: json['driveRefreshToken'] is String
          ? json['driveRefreshToken'] as String
          : '',
      driveFolderIds: json['driveFolderIds'] is List
          ? (json['driveFolderIds'] as List)
                .whereType<String>()
                .map((s) => s.trim())
                .where((s) => s.isNotEmpty)
                .toList()
          : const <String>[],
      driveLinked: json['driveLinked'] == true,
    );
  }

  @override
  bool operator ==(Object other) =>
      other is AppSettings &&
      runtimeType == other.runtimeType &&
      wakeWord == other.wakeWord &&
      threshold == other.threshold &&
      activeThreshold == other.activeThreshold &&
      smoothingWindow == other.smoothingWindow &&
      fireOnPeak == other.fireOnPeak &&
      playbackBufferSecs == other.playbackBufferSecs &&
      useAudioRecord == other.useAudioRecord &&
      endpointCueEnabled == other.endpointCueEnabled &&
      endpointSilenceMs == other.endpointSilenceMs &&
      endpointRmsThreshold == other.endpointRmsThreshold &&
      photoSource == other.photoSource &&
      ambientRefreshToken == other.ambientRefreshToken &&
      ambientDeviceId == other.ambientDeviceId &&
      ambientLinked == other.ambientLinked &&
      driveRefreshToken == other.driveRefreshToken &&
      listEquals(driveFolderIds, other.driveFolderIds) &&
      driveLinked == other.driveLinked;

  @override
  int get hashCode => Object.hash(
    wakeWord,
    threshold,
    activeThreshold,
    smoothingWindow,
    fireOnPeak,
    playbackBufferSecs,
    useAudioRecord,
    endpointCueEnabled,
    endpointSilenceMs,
    endpointRmsThreshold,
    photoSource,
    ambientRefreshToken,
    ambientDeviceId,
    ambientLinked,
    driveRefreshToken,
    Object.hashAll(driveFolderIds),
    driveLinked,
  );
}
