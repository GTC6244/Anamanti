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
    this.orchestratorKey = '',
    this.deviceId = '',
    this.deviceName = '',
    this.wakeWord = 'hey_jarvis',
    this.threshold = 0.5,
    this.activeThreshold = 0.7,
    this.smoothingWindow = 2,
    this.fireOnPeak = false,
    this.playbackBufferSecs = 30,
    this.captureGainDb = 0.0,
    this.useAudioRecord = true,
    this.platformNs = true,
    this.platformAgc = true,
    this.platformAec = false,
    this.endpointCueEnabled = true,
    this.endpointSilenceMs = 600,
    this.endpointRmsThreshold = 0.012,
    this.listeningRingEnabled = true,
    this.ringReactivity = 1.0,
    this.ringAttack = 0.65,
    this.ringRelease = 0.08,
    this.ringDecay = 0.99,
    this.dimDelaySecs = 300,
    this.photoSource = PhotoSourceKind.local,
    this.ambientRefreshToken = '',
    this.ambientDeviceId = '',
    this.ambientLinked = false,
    this.driveRefreshToken = '',
    this.driveFolderIds = const <String>[],
    this.driveLinked = false,
    this.driveClientId = '',
    this.driveClientSecret = '',
  });

  /// Stable selection key (`instance_id` TXT) of the orchestrator this display is
  /// pinned to. Empty = "Auto" (connect to the first available orchestrator).
  /// Device-local: applied by rebuilding the engine's [WakeWordConfig] and used to
  /// pin the settings-control client to the same Mac.
  final String orchestratorKey;

  /// Stable, globally-unique identifier for this physical display, derived from the
  /// device's Wi-Fi MAC (`anamanti-<12 hex>`, e.g. `anamanti-140ac5942aca`). Minted
  /// once on first run (via the Rust `deviceHardwareId()` call, falling back to a
  /// persisted random id) and sent to the Core in the `anamanti-hello` frame so two
  /// displays on one Core are distinguishable. Never changes once set.
  final String deviceId;

  /// Human-friendly label for this display (e.g. "Kitchen"), editable on the Settings
  /// screen and sent to the Core alongside [deviceId]. Empty until the user names it.
  final String deviceName;

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

  /// Software capture gain in **decibels** applied to the resampled mic signal (both
  /// the wake-word detector input and the streamed PCM). `0.0` = unity/no-op. Raise it
  /// if a quiet far-field mic causes wake-word or speech-onset **misses** — the in-app,
  /// root-free analogue of the AEC shim's `persist.vendor.amznaec.gain_db`. Engine
  /// clamps to [0, 36] dB. A/B-tunable.
  final double captureGainDb;

  /// **Android only.** Capture through the Kotlin `AudioRecord` layer
  /// (`VOICE_RECOGNITION` source + platform noise-suppression/AGC) instead of the
  /// default `cpal` path. A/B-tunable on-device to compare far-field pickup.
  final bool useAudioRecord;

  /// **Android only.** Attach the platform `NoiseSuppressor` to the AudioRecord
  /// session. On by default. Note: aggressive noise suppression can *distort*
  /// speech and cause wake-word **misses** in a noisy room — turn this off to A/B
  /// test far-field responsiveness. Only applies on the [useAudioRecord] path.
  final bool platformNs;

  /// **Android only.** Attach the platform `AutomaticGainControl` to the AudioRecord
  /// session. On by default; helps the Echo Show's quiet far-field pickup. Only
  /// applies on the [useAudioRecord] path.
  final bool platformAgc;

  /// **Android only.** Attach the platform `AcousticEchoCanceler` to the AudioRecord
  /// session. Off by default — the host-side WebRTC APM does AEC and this device's
  /// platform AEC was found not to actually cancel (see agents.md). Exposed as an
  /// experimental lever. Only applies on the [useAudioRecord] path.
  final bool platformAec;

  /// Whether the device shows a local "processing" cue the instant the user stops
  /// speaking, instead of waiting for the Mac's VAD + transcript round trip.
  final bool endpointCueEnabled;

  /// Trailing silence (ms) the local end-of-speech cue waits for before flipping to
  /// the "processing" phase.
  final int endpointSilenceMs;

  /// Mic RMS level (0..1) below which audio counts as silence for the local cue.
  final double endpointRmsThreshold;

  /// Whether the large glowing "listening" ring pops up while the device listens to
  /// you (wake word → end-of-speech). Purely presentational; off hides the overlay
  /// entirely. See [ListeningOverlay].
  final bool listeningRingEnabled;

  /// How strongly the listening ring reacts to your voice — a multiplier on the
  /// amplitude-driven swing (thickness/scale/glow). 1.0 = default; higher = more
  /// dramatic. Presentational.
  final double ringReactivity;

  /// Listening-ring attack: how quickly it responds as your voice gets louder
  /// (per-frame ease, 0..1; higher = snappier). Presentational.
  final double ringAttack;

  /// Listening-ring release: how quickly it settles back as you quiet down
  /// (per-frame ease, 0..1; lower = more lingering). Presentational.
  final double ringRelease;

  /// Listening-ring auto-range decay: how fast the ring re-scales to your current
  /// speaking level (per-frame peak decay, closer to 1 = holds the range longer).
  /// Presentational.
  final double ringDecay;

  /// How long (seconds) the idle screen stays fully bright after the room goes
  /// quiet before it dims to the calm "away" clock face. Maps to the camera
  /// proximity detector's release window (`WakeWordConfig.proximityReleaseSecs`):
  /// once no motion has been seen for this long, the engine reports the room empty,
  /// which dims the backlight and shows the large centered clock. Defaults to 5
  /// minutes; the settings screen offers a fixed set of presets from 30 s to 1 hour.
  final int dimDelaySecs;

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

  /// Google Drive "Desktop app" OAuth client id, synced from the orchestrator (which
  /// owns consent). Used with [driveClientSecret] to mint access tokens on-device, so
  /// the APK ships credential-free. Empty = Drive not configured. TODO: secure storage.
  final String driveClientId;

  /// Google Drive OAuth client secret, synced from the orchestrator. TODO: secure storage.
  final String driveClientSecret;

  /// True when both Drive client credentials are present — the device can mint Drive
  /// access tokens. Runtime replacement for the old build-time `kGoogleDriveConfigured`.
  bool get driveConfigured =>
      driveClientId.isNotEmpty && driveClientSecret.isNotEmpty;

  AppSettings copyWith({
    String? orchestratorKey,
    String? deviceId,
    String? deviceName,
    String? wakeWord,
    double? threshold,
    double? activeThreshold,
    int? smoothingWindow,
    bool? fireOnPeak,
    int? playbackBufferSecs,
    double? captureGainDb,
    bool? useAudioRecord,
    bool? platformNs,
    bool? platformAgc,
    bool? platformAec,
    bool? endpointCueEnabled,
    int? endpointSilenceMs,
    double? endpointRmsThreshold,
    bool? listeningRingEnabled,
    double? ringReactivity,
    double? ringAttack,
    double? ringRelease,
    double? ringDecay,
    int? dimDelaySecs,
    PhotoSourceKind? photoSource,
    String? ambientRefreshToken,
    String? ambientDeviceId,
    bool? ambientLinked,
    String? driveRefreshToken,
    List<String>? driveFolderIds,
    bool? driveLinked,
    String? driveClientId,
    String? driveClientSecret,
  }) {
    return AppSettings(
      orchestratorKey: orchestratorKey ?? this.orchestratorKey,
      deviceId: deviceId ?? this.deviceId,
      deviceName: deviceName ?? this.deviceName,
      wakeWord: wakeWord ?? this.wakeWord,
      threshold: threshold ?? this.threshold,
      activeThreshold: activeThreshold ?? this.activeThreshold,
      smoothingWindow: smoothingWindow ?? this.smoothingWindow,
      fireOnPeak: fireOnPeak ?? this.fireOnPeak,
      playbackBufferSecs: playbackBufferSecs ?? this.playbackBufferSecs,
      captureGainDb: captureGainDb ?? this.captureGainDb,
      useAudioRecord: useAudioRecord ?? this.useAudioRecord,
      platformNs: platformNs ?? this.platformNs,
      platformAgc: platformAgc ?? this.platformAgc,
      platformAec: platformAec ?? this.platformAec,
      endpointCueEnabled: endpointCueEnabled ?? this.endpointCueEnabled,
      endpointSilenceMs: endpointSilenceMs ?? this.endpointSilenceMs,
      endpointRmsThreshold: endpointRmsThreshold ?? this.endpointRmsThreshold,
      listeningRingEnabled: listeningRingEnabled ?? this.listeningRingEnabled,
      ringReactivity: ringReactivity ?? this.ringReactivity,
      ringAttack: ringAttack ?? this.ringAttack,
      ringRelease: ringRelease ?? this.ringRelease,
      ringDecay: ringDecay ?? this.ringDecay,
      dimDelaySecs: dimDelaySecs ?? this.dimDelaySecs,
      photoSource: photoSource ?? this.photoSource,
      ambientRefreshToken: ambientRefreshToken ?? this.ambientRefreshToken,
      ambientDeviceId: ambientDeviceId ?? this.ambientDeviceId,
      ambientLinked: ambientLinked ?? this.ambientLinked,
      driveRefreshToken: driveRefreshToken ?? this.driveRefreshToken,
      driveFolderIds: driveFolderIds ?? this.driveFolderIds,
      driveLinked: driveLinked ?? this.driveLinked,
      driveClientId: driveClientId ?? this.driveClientId,
      driveClientSecret: driveClientSecret ?? this.driveClientSecret,
    );
  }

  Map<String, dynamic> toJson() => <String, dynamic>{
    'orchestratorKey': orchestratorKey,
    'deviceId': deviceId,
    'deviceName': deviceName,
    'wakeWord': wakeWord,
    'threshold': threshold,
    'activeThreshold': activeThreshold,
    'smoothingWindow': smoothingWindow,
    'fireOnPeak': fireOnPeak,
    'playbackBufferSecs': playbackBufferSecs,
    'captureGainDb': captureGainDb,
    'useAudioRecord': useAudioRecord,
    'platformNs': platformNs,
    'platformAgc': platformAgc,
    'platformAec': platformAec,
    'endpointCueEnabled': endpointCueEnabled,
    'endpointSilenceMs': endpointSilenceMs,
    'endpointRmsThreshold': endpointRmsThreshold,
    'listeningRingEnabled': listeningRingEnabled,
    'ringReactivity': ringReactivity,
    'ringAttack': ringAttack,
    'ringRelease': ringRelease,
    'ringDecay': ringDecay,
    'dimDelaySecs': dimDelaySecs,
    'photoSource': photoSource.name,
    'ambientRefreshToken': ambientRefreshToken,
    'ambientDeviceId': ambientDeviceId,
    'ambientLinked': ambientLinked,
    'driveRefreshToken': driveRefreshToken,
    'driveFolderIds': driveFolderIds,
    'driveLinked': driveLinked,
    'driveClientId': driveClientId,
    'driveClientSecret': driveClientSecret,
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
      orchestratorKey: json['orchestratorKey'] is String
          ? json['orchestratorKey'] as String
          : defaults.orchestratorKey,
      deviceId: json['deviceId'] is String
          ? json['deviceId'] as String
          : defaults.deviceId,
      deviceName: json['deviceName'] is String
          ? json['deviceName'] as String
          : defaults.deviceName,
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
      captureGainDb: json['captureGainDb'] is num
          ? (json['captureGainDb'] as num).toDouble().clamp(0.0, 36.0)
          : defaults.captureGainDb,
      useAudioRecord: json['useAudioRecord'] is bool
          ? json['useAudioRecord'] as bool
          : defaults.useAudioRecord,
      platformNs: json['platformNs'] is bool
          ? json['platformNs'] as bool
          : defaults.platformNs,
      platformAgc: json['platformAgc'] is bool
          ? json['platformAgc'] as bool
          : defaults.platformAgc,
      platformAec: json['platformAec'] is bool
          ? json['platformAec'] as bool
          : defaults.platformAec,
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
      listeningRingEnabled: json['listeningRingEnabled'] is bool
          ? json['listeningRingEnabled'] as bool
          : defaults.listeningRingEnabled,
      ringReactivity: json['ringReactivity'] is num
          ? (json['ringReactivity'] as num).toDouble().clamp(0.1, 4.0)
          : defaults.ringReactivity,
      ringAttack: json['ringAttack'] is num
          ? (json['ringAttack'] as num).toDouble().clamp(0.05, 1.0)
          : defaults.ringAttack,
      ringRelease: json['ringRelease'] is num
          ? (json['ringRelease'] as num).toDouble().clamp(0.01, 1.0)
          : defaults.ringRelease,
      ringDecay: json['ringDecay'] is num
          ? (json['ringDecay'] as num).toDouble().clamp(0.5, 0.9999)
          : defaults.ringDecay,
      dimDelaySecs: asInt(
        json['dimDelaySecs'],
        defaults.dimDelaySecs,
        min: 30,
        max: 3600,
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
      driveClientId: json['driveClientId'] is String
          ? json['driveClientId'] as String
          : '',
      driveClientSecret: json['driveClientSecret'] is String
          ? json['driveClientSecret'] as String
          : '',
    );
  }

  @override
  bool operator ==(Object other) =>
      other is AppSettings &&
      runtimeType == other.runtimeType &&
      orchestratorKey == other.orchestratorKey &&
      deviceId == other.deviceId &&
      deviceName == other.deviceName &&
      wakeWord == other.wakeWord &&
      threshold == other.threshold &&
      activeThreshold == other.activeThreshold &&
      smoothingWindow == other.smoothingWindow &&
      fireOnPeak == other.fireOnPeak &&
      playbackBufferSecs == other.playbackBufferSecs &&
      captureGainDb == other.captureGainDb &&
      useAudioRecord == other.useAudioRecord &&
      platformNs == other.platformNs &&
      platformAgc == other.platformAgc &&
      platformAec == other.platformAec &&
      endpointCueEnabled == other.endpointCueEnabled &&
      endpointSilenceMs == other.endpointSilenceMs &&
      endpointRmsThreshold == other.endpointRmsThreshold &&
      listeningRingEnabled == other.listeningRingEnabled &&
      ringReactivity == other.ringReactivity &&
      ringAttack == other.ringAttack &&
      ringRelease == other.ringRelease &&
      ringDecay == other.ringDecay &&
      dimDelaySecs == other.dimDelaySecs &&
      photoSource == other.photoSource &&
      ambientRefreshToken == other.ambientRefreshToken &&
      ambientDeviceId == other.ambientDeviceId &&
      ambientLinked == other.ambientLinked &&
      driveRefreshToken == other.driveRefreshToken &&
      listEquals(driveFolderIds, other.driveFolderIds) &&
      driveLinked == other.driveLinked &&
      driveClientId == other.driveClientId &&
      driveClientSecret == other.driveClientSecret;

  @override
  int get hashCode => Object.hash(
    orchestratorKey,
    wakeWord,
    threshold,
    activeThreshold,
    smoothingWindow,
    fireOnPeak,
    playbackBufferSecs,
    Object.hash(
      captureGainDb,
      useAudioRecord,
      platformNs,
      platformAgc,
      platformAec,
      deviceId,
      deviceName,
    ),
    endpointCueEnabled,
    endpointSilenceMs,
    endpointRmsThreshold,
    dimDelaySecs,
    photoSource,
    ambientRefreshToken,
    ambientDeviceId,
    ambientLinked,
    driveRefreshToken,
    Object.hashAll(driveFolderIds),
    driveLinked,
    Object.hash(
      driveClientId,
      driveClientSecret,
      listeningRingEnabled,
      ringReactivity,
      ringAttack,
      ringRelease,
      ringDecay,
    ),
  );
}
