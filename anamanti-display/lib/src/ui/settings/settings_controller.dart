// State + apply logic shared by every Settings section (plans/SettingsReorgPlan.md).
//
// Settings applies changes immediately — there is no Save button (§3.5):
//  * Device-local settings are persisted ([SettingsStore]) and handed to the app
//    ([SettingsController.onApplied], which restarts the engine / refreshes the
//    slideshow as needed). Commits are debounced so a burst of changes (three quick
//    toggles, a slider drag) costs one apply, and serialized so applies never overlap.
//  * Capture gain and both wake-word thresholds are also pushed to the running engine
//    on every change ([SettingsController.onLiveTune]) so they take effect while the
//    slider moves, with no restart.
//  * Core-managed settings (LLM, model, voice, VAD) are sent with `applySettings`,
//    debounced + serialized the same way; a rejected or failed apply reverts the
//    controls to the last values the Core confirmed.
// Pending commits are flushed when Settings closes, so nothing is lost by leaving.

import 'dart:async';

import 'package:flutter/foundation.dart';

import 'package:anamanti_display/src/settings/app_settings.dart';
import 'package:anamanti_display/src/settings/orchestrator_client.dart';
import 'package:anamanti_display/src/settings/settings_store.dart';

/// LLM backends Settings can select: value = the Core's backend label, label = the
/// short segmented-button text.
const Map<String, String> kLlmBackends = <String, String>{
  'ollama': 'Local',
  'anthropic': 'Claude',
  'openai': 'OpenAI',
  'mock': 'Mock',
};

/// Backends whose model is chosen from the Core's last-12-months catalog (a
/// dropdown); other backends keep a free-text model field (e.g. an Ollama tag).
const Set<String> kCloudBackends = <String>{'anthropic', 'openai'};

/// Push capture gain (dB) + idle / active wake-word thresholds to the running engine.
typedef LiveTune =
    void Function(double gainDb, double threshold, double activeThreshold);

/// The Core-managed settings Settings edits, as one comparable value.
@immutable
class RemoteSettings {
  const RemoteSettings({
    this.backend = 'ollama',
    this.anthropicAuth = 'apikey',
    this.model = '',
    this.voice = '',
    this.endSilenceMs = 700,
    this.voiceRmsThreshold = 120,
    this.vadEngine = 'silero',
    this.sileroThreshold = 0.5,
  });

  /// Adopt the Core's reported values; unknown/zero fields keep [fallback]'s value.
  factory RemoteSettings.fromView(
    OrchestratorSettingsView v, {
    RemoteSettings fallback = const RemoteSettings(),
  }) {
    return RemoteSettings(
      backend: kLlmBackends.containsKey(v.llmBackend) ? v.llmBackend : 'ollama',
      anthropicAuth: v.anthropicAuth == 'subscription'
          ? 'subscription'
          : 'apikey',
      model: v.llmModel ?? '',
      voice: v.ttsVoice ?? '',
      endSilenceMs: v.endSilenceMs > 0 ? v.endSilenceMs : fallback.endSilenceMs,
      voiceRmsThreshold: v.voiceRmsThreshold > 0
          ? v.voiceRmsThreshold
          : fallback.voiceRmsThreshold,
      vadEngine: v.vadEngine.isNotEmpty ? v.vadEngine : fallback.vadEngine,
      sileroThreshold: v.sileroThreshold > 0
          ? v.sileroThreshold
          : fallback.sileroThreshold,
    );
  }

  final String backend;
  final String anthropicAuth;
  final String model;
  final String voice;
  final int endSilenceMs;
  final double voiceRmsThreshold;
  final String vadEngine;
  final double sileroThreshold;

  RemoteSettings copyWith({
    String? backend,
    String? anthropicAuth,
    String? model,
    String? voice,
    int? endSilenceMs,
    double? voiceRmsThreshold,
    String? vadEngine,
    double? sileroThreshold,
  }) {
    return RemoteSettings(
      backend: backend ?? this.backend,
      anthropicAuth: anthropicAuth ?? this.anthropicAuth,
      model: model ?? this.model,
      voice: voice ?? this.voice,
      endSilenceMs: endSilenceMs ?? this.endSilenceMs,
      voiceRmsThreshold: voiceRmsThreshold ?? this.voiceRmsThreshold,
      vadEngine: vadEngine ?? this.vadEngine,
      sileroThreshold: sileroThreshold ?? this.sileroThreshold,
    );
  }

  @override
  bool operator ==(Object other) =>
      other is RemoteSettings &&
      backend == other.backend &&
      anthropicAuth == other.anthropicAuth &&
      model == other.model &&
      voice == other.voice &&
      endSilenceMs == other.endSilenceMs &&
      voiceRmsThreshold == other.voiceRmsThreshold &&
      vadEngine == other.vadEngine &&
      sileroThreshold == other.sileroThreshold;

  @override
  int get hashCode => Object.hash(
    backend,
    anthropicAuth,
    model,
    voice,
    endSilenceMs,
    voiceRmsThreshold,
    vadEngine,
    sileroThreshold,
  );
}

class SettingsController extends ChangeNotifier {
  SettingsController({
    required AppSettings initial,
    required this.store,
    required this.client,
    required this.onApplied,
    this.onLiveTune,
    this.clientForCore,
    this.localDebounce = const Duration(milliseconds: 600),
    this.remoteDebounce = const Duration(milliseconds: 500),
  }) : _settings = initial,
       _committedLocal = initial,
       _clientCore = initial.orchestratorKey;

  final SettingsStore store;

  /// Client for the pinned Core. Replaced (via [clientForCore]) when the user pins a
  /// different Core, so Settings keeps talking to the right Mac while it stays open.
  OrchestratorClient client;

  /// Build a client pinned to a Core key (`''` = Auto). Null keeps [client] as is
  /// (tests).
  final OrchestratorClient Function(String orchestratorKey)? clientForCore;

  /// The Core key [client] is pinned to.
  String _clientCore;

  /// Hand a committed device-local settings snapshot to the app (engine restart,
  /// slideshow refresh, live tuning). Awaited, so applies are serialized.
  final FutureOr<void> Function(AppSettings next) onApplied;

  /// Push live-tunable values to the running engine as they change. Optional (null
  /// in tests / when no engine is running).
  final LiveTune? onLiveTune;

  final Duration localDebounce;
  final Duration remoteDebounce;

  bool _disposed = false;

  // ---------------------------------------------------------------------------
  // Apply status (shown in the AppBar).

  int _inFlight = 0;
  Timer? _localTimer;
  Timer? _remoteTimer;

  /// Last apply failure to surface, cleared by the next successful apply.
  String? lastError;

  /// When the last apply completed successfully (null until one has).
  DateTime? lastAppliedAt;

  /// True while a change is waiting to be applied or being applied.
  bool get busy =>
      _inFlight > 0 ||
      (_localTimer?.isActive ?? false) ||
      (_remoteTimer?.isActive ?? false);

  // ---------------------------------------------------------------------------
  // Device-local settings.

  /// The settings as shown — every applied control change, excluding text-field
  /// drafts (see [setLocalDraft]).
  AppSettings _settings;

  /// The last snapshot successfully persisted + applied.
  AppSettings _committedLocal;

  /// The latest snapshot queued for applying, until its apply settles. Commits
  /// compare against this (falling back to [_committedLocal]) so an edit made while
  /// an apply is in flight — including an undo back to the applied value — is
  /// never mistaken for "already applied".
  AppSettings? _queuedLocal;
  Future<void> _localChain = Future<void>.value();

  /// Uncommitted text-field edits, by field. Kept out of [_settings] so an unrelated
  /// commit never applies a half-typed value; applied on the field's commit or on
  /// [flush].
  final Map<String, AppSettings Function(AppSettings)> _localDrafts =
      <String, AppSettings Function(AppSettings)>{};

  AppSettings get settings => _settings;

  /// Change the device-local settings. With [commit] (the default) the change is
  /// persisted + applied after [localDebounce]; pass `commit: false` while a slider
  /// is being dragged and call [commitLocalSoon] when it is released. Live-tunable
  /// values are pushed to the running engine immediately either way.
  void updateLocal(AppSettings next, {bool commit = true}) {
    if (_disposed) return;
    final prev = _settings;
    _settings = next;
    if (next.captureGainDb != prev.captureGainDb ||
        next.threshold != prev.threshold ||
        next.activeThreshold != prev.activeThreshold) {
      onLiveTune?.call(
        next.captureGainDb,
        next.threshold,
        next.activeThreshold,
      );
    }
    if (commit) commitLocalSoon();
    notifyListeners();
  }

  /// Record an in-progress text edit for [field] without applying it.
  void setLocalDraft(String field, AppSettings Function(AppSettings) apply) {
    _localDrafts[field] = apply;
  }

  /// Finish a text edit: apply it (debounced like any other change).
  void commitLocalText(String field, AppSettings Function(AppSettings) apply) {
    _localDrafts.remove(field);
    updateLocal(apply(_settings));
  }

  /// Schedule a debounced commit of the current device-local settings.
  void commitLocalSoon() {
    if (_disposed) return;
    _localTimer?.cancel();
    _localTimer = Timer(localDebounce, _commitLocal);
    notifyListeners();
  }

  void _commitLocal() {
    _localTimer?.cancel();
    final next = _settings;
    if (next == (_queuedLocal ?? _committedLocal)) {
      notifyListeners();
      return;
    }
    _queuedLocal = next;
    _inFlight++;
    notifyListeners();
    _localChain = _localChain.then((_) async {
      try {
        await store.save(next);
        await onApplied(next);
        _committedLocal = next;
        lastError = null;
        lastAppliedAt = DateTime.now();
        _repinIfNeeded(next.orchestratorKey);
      } catch (e) {
        // Not marked applied, so the next commit (or flush) retries it.
        lastError = 'Couldn’t apply: $e';
      } finally {
        if (identical(_queuedLocal, next)) _queuedLocal = null;
        _inFlight--;
        notifyListeners();
      }
    });
  }

  /// After a new Core pin has been applied, point Settings at that Core and reload
  /// its settings + capabilities.
  void _repinIfNeeded(String core) {
    final make = clientForCore;
    if (make == null || core == _clientCore || _disposed) return;
    _clientCore = core;
    client = make(core);
    functions = null;
    loadRemote();
    loadFunctions();
  }

  // ---------------------------------------------------------------------------
  // Core-managed (remote) settings.

  bool remoteLoading = true;
  String? remoteError;
  RemoteSettings _remote = const RemoteSettings();
  RemoteSettings _confirmedRemote = const RemoteSettings();
  Future<void> _remoteChain = Future<void>.value();

  /// Selectable models (last 12 months) from the Core, for the model dropdown.
  List<ModelOption> models = const <ModelOption>[];

  /// Installed Piper voices from the Core. Empty → the voice falls back to free text.
  List<VoiceOption> voices = const <VoiceOption>[];

  /// Cores discovered on the LAN (pure mDNS — independent of the pinned Core's
  /// reachability, so the picker works while it is offline).
  List<OrchestratorOption> orchestrators = const <OrchestratorOption>[];

  RemoteSettings get remote => _remote;

  /// Whether Core-managed controls can be edited (loaded and reachable).
  bool get remoteEditable => !remoteLoading && remoteError == null;

  Future<void> loadRemote() async {
    remoteLoading = true;
    remoteError = null;
    notifyListeners();
    try {
      orchestrators = await client.listOrchestrators();
    } catch (_) {
      orchestrators = const <OrchestratorOption>[];
    }
    if (_disposed) return;
    notifyListeners();
    try {
      final view = await client.fetchSettings();
      // Model + voice catalogs are best-effort: on failure the fields fall back to
      // free text so the user is never blocked.
      try {
        models = await client.listModels();
      } catch (_) {
        models = const <ModelOption>[];
      }
      try {
        voices = await client.listVoices();
      } catch (_) {
        voices = const <VoiceOption>[];
      }
      _remote = RemoteSettings.fromView(view, fallback: _remote);
      _confirmedRemote = _remote;
      remoteLoading = false;
    } catch (e) {
      remoteError = '$e';
      remoteLoading = false;
    }
    if (!_disposed) notifyListeners();
  }

  /// Change Core-managed settings. Applied after [remoteDebounce] unless
  /// `commit: false` (slider drags; call [commitRemoteSoon] on release). Ignored
  /// while the Core is loading or offline (the controls are disabled then).
  void updateRemote(RemoteSettings next, {bool commit = true}) {
    if (_disposed || !remoteEditable) return;
    _remote = next;
    if (commit) commitRemoteSoon();
    notifyListeners();
  }

  /// Uncommitted Core-managed text edits (model, free-text voice), by field.
  final Map<String, RemoteSettings Function(RemoteSettings)> _remoteDrafts =
      <String, RemoteSettings Function(RemoteSettings)>{};

  /// Record an in-progress text edit for [field] without applying it.
  void setRemoteDraft(
    String field,
    RemoteSettings Function(RemoteSettings) apply,
  ) {
    _remoteDrafts[field] = apply;
  }

  /// Finish a text edit: apply it (debounced like any other change).
  void commitRemoteText(
    String field,
    RemoteSettings Function(RemoteSettings) apply,
  ) {
    _remoteDrafts.remove(field);
    updateRemote(apply(_remote));
  }

  void commitRemoteSoon() {
    if (_disposed) return;
    _remoteTimer?.cancel();
    _remoteTimer = Timer(remoteDebounce, _commitRemote);
    notifyListeners();
  }

  /// The latest value queued for `applySettings`, until it settles. Commits compare
  /// against this (falling back to [_confirmedRemote]) so an edit made while an
  /// apply is in flight — including an undo back to the confirmed value — is sent,
  /// and an apply already in flight isn't resent.
  RemoteSettings? _queuedRemote;

  void _commitRemote() {
    _remoteTimer?.cancel();
    final sent = _remote;
    if (sent == (_queuedRemote ?? _confirmedRemote) || !remoteEditable) {
      notifyListeners();
      return;
    }
    _queuedRemote = sent;
    _inFlight++;
    notifyListeners();
    _remoteChain = _remoteChain.then((_) async {
      try {
        final result = await client.applySettings(
          llmBackend: sent.backend,
          llmModel: sent.model.trim().isEmpty ? null : sent.model.trim(),
          anthropicAuth: sent.backend == 'anthropic'
              ? sent.anthropicAuth
              : null,
          setTtsVoice: true,
          ttsVoice: sent.voice.trim().isEmpty ? null : sent.voice.trim(),
          endSilenceMs: sent.endSilenceMs,
          voiceRmsThreshold: sent.voiceRmsThreshold,
          vadEngine: sent.vadEngine,
          sileroThreshold: sent.sileroThreshold,
        );
        if (result.ok) {
          _confirmedRemote = sent;
          lastError = null;
          lastAppliedAt = DateTime.now();
        } else {
          _revertRemote(sent);
          lastError = 'Assistant: ${result.message}';
        }
      } catch (e) {
        _revertRemote(sent);
        lastError = 'Couldn’t reach the assistant — change undone.';
      } finally {
        if (identical(_queuedRemote, sent)) _queuedRemote = null;
        _inFlight--;
        notifyListeners();
      }
    });
  }

  /// Undo a failed apply, unless the user has already moved on to newer edits
  /// (those get their own apply attempt).
  void _revertRemote(RemoteSettings failed) {
    if (_remote == failed) _remote = _confirmedRemote;
  }

  // ---------------------------------------------------------------------------
  // Capabilities (the Core's read-only "functions" status snapshot).

  List<FunctionStatusView>? functions;
  bool functionsLoading = false;
  String? functionsError;

  Future<void> loadFunctions() async {
    if (functionsLoading) return;
    functionsLoading = true;
    functionsError = null;
    notifyListeners();
    try {
      functions = await client.fetchFunctions();
    } catch (e) {
      functionsError = '$e';
    }
    functionsLoading = false;
    if (!_disposed) notifyListeners();
  }

  // ---------------------------------------------------------------------------
  // Lifecycle.

  /// Apply anything not yet applied right now — a change waiting on its debounce,
  /// a slider mid-drag, or a text field mid-edit (Settings is closing).
  /// Fire-and-forget: the applies complete after the screen is gone.
  void flush() {
    for (final apply in _localDrafts.values) {
      _settings = apply(_settings);
    }
    _localDrafts.clear();
    if (remoteEditable) {
      for (final apply in _remoteDrafts.values) {
        _remote = apply(_remote);
      }
    }
    _remoteDrafts.clear();
    _commitLocal();
    _commitRemote();
  }

  @override
  void notifyListeners() {
    if (!_disposed) super.notifyListeners();
  }

  @override
  void dispose() {
    _disposed = true;
    _localTimer?.cancel();
    _remoteTimer?.cancel();
    super.dispose();
  }
}
