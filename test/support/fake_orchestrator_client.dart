// A fake [OrchestratorClient] for widget tests: no native library, no network.
// Records calls and returns canned data so the settings/memory screens can be
// driven deterministically.

import 'package:ambient_display/src/settings/orchestrator_client.dart';

class FakeOrchestratorClient implements OrchestratorClient {
  FakeOrchestratorClient({
    OrchestratorSettingsView? settings,
    List<MemoryView>? memories,
    List<ModelOption>? models,
    this.throwOnFetch = false,
  })  : _models = models ??
            const <ModelOption>[
              ModelOption(provider: 'anthropic', id: 'claude-opus-5', label: 'Claude Opus 5'),
              ModelOption(provider: 'openai', id: 'gpt-4o-mini', label: 'gpt-4o-mini'),
            ],
        _settings = settings ??
            const OrchestratorSettingsView(
              ok: true,
              message: 'ok',
              llmBackend: 'ollama',
              llmModel: 'llama3.2',
              ttsVoice: null,
            ),
        _memories = memories ?? <MemoryView>[];

  OrchestratorSettingsView _settings;
  final List<MemoryView> _memories;
  final List<ModelOption> _models;
  final bool throwOnFetch;

  // Call records for assertions.
  int fetchCount = 0;
  final List<Map<String, dynamic>> applyCalls = <Map<String, dynamic>>[];
  final List<int> deleted = <int>[];
  int clearCount = 0;

  @override
  Future<OrchestratorSettingsView> fetchSettings() async {
    fetchCount++;
    if (throwOnFetch) throw Exception('offline');
    return _settings;
  }

  @override
  Future<OrchestratorSettingsView> applySettings({
    String? llmBackend,
    String? llmModel,
    String? anthropicAuth,
    bool setTtsVoice = false,
    String? ttsVoice,
    int? endSilenceMs,
    double? voiceRmsThreshold,
  }) async {
    applyCalls.add({
      'llmBackend': llmBackend,
      'llmModel': llmModel,
      'anthropicAuth': anthropicAuth,
      'setTtsVoice': setTtsVoice,
      'ttsVoice': ttsVoice,
      'endSilenceMs': endSilenceMs,
      'voiceRmsThreshold': voiceRmsThreshold,
    });
    _settings = OrchestratorSettingsView(
      ok: true,
      message: 'applied',
      llmBackend: llmBackend ?? _settings.llmBackend,
      llmModel: llmModel ?? _settings.llmModel,
      anthropicAuth: anthropicAuth ?? _settings.anthropicAuth,
      ttsVoice: setTtsVoice ? ttsVoice : _settings.ttsVoice,
      endSilenceMs: endSilenceMs ?? _settings.endSilenceMs,
      voiceRmsThreshold: voiceRmsThreshold ?? _settings.voiceRmsThreshold,
    );
    return _settings;
  }

  @override
  Future<List<ModelOption>> listModels() async {
    if (throwOnFetch) throw Exception('offline');
    return List.of(_models);
  }

  @override
  Future<List<MemoryView>> listMemories() async => List.of(_memories);

  @override
  Future<bool> deleteMemory(int id) async {
    deleted.add(id);
    _memories.removeWhere((m) => m.id == id);
    return true;
  }

  @override
  Future<int> clearMemories() async {
    clearCount++;
    final n = _memories.length;
    _memories.clear();
    return n;
  }
}
