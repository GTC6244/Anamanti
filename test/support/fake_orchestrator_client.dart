// A fake [OrchestratorClient] for widget tests: no native library, no network.
// Records calls and returns canned data so the settings/memory screens can be
// driven deterministically.

import 'package:ambient_display/src/settings/orchestrator_client.dart';

class FakeOrchestratorClient implements OrchestratorClient {
  FakeOrchestratorClient({
    OrchestratorSettingsView? settings,
    List<MemoryView>? memories,
    this.throwOnFetch = false,
  })  : _settings = settings ??
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
    bool setTtsVoice = false,
    String? ttsVoice,
    int? endSilenceMs,
    double? voiceRmsThreshold,
  }) async {
    applyCalls.add({
      'llmBackend': llmBackend,
      'llmModel': llmModel,
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
      ttsVoice: setTtsVoice ? ttsVoice : _settings.ttsVoice,
      endSilenceMs: endSilenceMs ?? _settings.endSilenceMs,
      voiceRmsThreshold: voiceRmsThreshold ?? _settings.voiceRmsThreshold,
    );
    return _settings;
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
