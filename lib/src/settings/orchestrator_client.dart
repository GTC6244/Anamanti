// Client for the orchestrator-managed settings + memory (Plan.MD §3, Phase 6).
//
// The LLM backend, model, and TTS voice live on the Mac orchestrator, as does the
// persistent memory list. The settings screen reads/changes them through this
// interface, which is expressed in plain domain types (not the generated FRB
// types) so the UI and its widget tests never need the native library loaded — the
// production [FrbOrchestratorClient] is the only place that talks to FRB.

import 'package:ambient_display/src/rust/api/settings.dart' as frb;

/// The orchestrator's current runtime settings.
class OrchestratorSettingsView {
  const OrchestratorSettingsView({
    required this.ok,
    required this.message,
    required this.llmBackend,
    this.llmModel,
    this.ttsVoice,
  });

  final bool ok;
  final String message;
  final String llmBackend;
  final String? llmModel;
  final String? ttsVoice;
}

/// One persistent memory entry.
class MemoryView {
  const MemoryView({
    required this.id,
    required this.kind,
    required this.content,
    required this.source,
    required this.createdAt,
  });

  final int id;

  /// `fact` or `preference`.
  final String kind;
  final String content;

  /// `explicit` (user asked) or `inferred` (auto-extracted).
  final String source;

  /// Unix seconds when the entry was stored.
  final int createdAt;
}

/// Reads and changes orchestrator-side settings and persistent memory. All calls
/// hit the network (mDNS discovery + a short Wyoming control connection) and may
/// throw if the Mac is unreachable; callers surface that as an offline state.
abstract class OrchestratorClient {
  Future<OrchestratorSettingsView> fetchSettings();

  Future<OrchestratorSettingsView> applySettings({
    String? llmBackend,
    String? llmModel,
    bool setTtsVoice = false,
    String? ttsVoice,
  });

  Future<List<MemoryView>> listMemories();

  Future<bool> deleteMemory(int id);

  Future<int> clearMemories();
}

/// Production client backed by the generated FRB control functions.
class FrbOrchestratorClient implements OrchestratorClient {
  const FrbOrchestratorClient({this.discoveryTimeoutSecs = 4});

  /// Seconds to browse mDNS for the orchestrator before falling back to the cache.
  final int discoveryTimeoutSecs;

  BigInt get _timeout => BigInt.from(discoveryTimeoutSecs);

  @override
  Future<OrchestratorSettingsView> fetchSettings() async {
    return _view(await frb.fetchOrchestratorSettings(discoveryTimeoutSecs: _timeout));
  }

  @override
  Future<OrchestratorSettingsView> applySettings({
    String? llmBackend,
    String? llmModel,
    bool setTtsVoice = false,
    String? ttsVoice,
  }) async {
    final result = await frb.updateOrchestratorSettings(
      update: frb.SettingsUpdate(
        llmBackend: llmBackend,
        llmModel: llmModel,
        setTtsVoice: setTtsVoice,
        ttsVoice: ttsVoice,
      ),
      discoveryTimeoutSecs: _timeout,
    );
    return _view(result);
  }

  @override
  Future<List<MemoryView>> listMemories() async {
    final entries = await frb.listMemories(discoveryTimeoutSecs: _timeout);
    return entries
        .map((e) => MemoryView(
              id: e.id.toInt(),
              kind: e.kind,
              content: e.content,
              source: e.source,
              createdAt: e.createdAt.toInt(),
            ))
        .toList();
  }

  @override
  Future<bool> deleteMemory(int id) =>
      frb.deleteMemory(id: id, discoveryTimeoutSecs: _timeout);

  @override
  Future<int> clearMemories() => frb.clearMemories(discoveryTimeoutSecs: _timeout);

  OrchestratorSettingsView _view(frb.OrchestratorSettings s) => OrchestratorSettingsView(
        ok: s.ok,
        message: s.message,
        llmBackend: s.llmBackend,
        llmModel: s.llmModel,
        ttsVoice: s.ttsVoice,
      );
}
