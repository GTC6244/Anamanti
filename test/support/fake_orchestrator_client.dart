// A fake [OrchestratorClient] for widget tests: no native library, no network.
// Records calls and returns canned data so the settings/memory screens can be
// driven deterministically.

import 'package:ambient_display/src/settings/orchestrator_client.dart';

class FakeOrchestratorClient implements OrchestratorClient {
  FakeOrchestratorClient({
    OrchestratorSettingsView? settings,
    List<MemoryView>? memories,
    List<SpeakerView>? speakers,
    this.throwOnFetch = false,
  })  : _settings = settings ??
            const OrchestratorSettingsView(
              ok: true,
              message: 'ok',
              llmBackend: 'ollama',
              llmModel: 'llama3.2',
              ttsVoice: null,
            ),
        _memories = memories ?? <MemoryView>[],
        _speakers = speakers ?? <SpeakerView>[];

  OrchestratorSettingsView _settings;
  final List<MemoryView> _memories;
  final List<SpeakerView> _speakers;
  final bool throwOnFetch;

  // Call records for assertions.
  int fetchCount = 0;
  final List<Map<String, dynamic>> applyCalls = <Map<String, dynamic>>[];
  final List<int> deleted = <int>[];
  int clearCount = 0;
  final List<Map<String, String>> namedCalls = <Map<String, String>>[];
  final List<Map<String, String>> mergeCalls = <Map<String, String>>[];
  final List<String> deletedSpeakers = <String>[];

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
  }) async {
    applyCalls.add({
      'llmBackend': llmBackend,
      'llmModel': llmModel,
      'setTtsVoice': setTtsVoice,
      'ttsVoice': ttsVoice,
    });
    _settings = OrchestratorSettingsView(
      ok: true,
      message: 'applied',
      llmBackend: llmBackend ?? _settings.llmBackend,
      llmModel: llmModel ?? _settings.llmModel,
      ttsVoice: setTtsVoice ? ttsVoice : _settings.ttsVoice,
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

  @override
  Future<List<SpeakerView>> listSpeakers() async => List.of(_speakers);

  @override
  Future<bool> nameSpeaker(String id, String name) async {
    namedCalls.add({'id': id, 'name': name});
    final i = _speakers.indexWhere((s) => s.id == id);
    if (i < 0) return false;
    final s = _speakers[i];
    _speakers[i] = SpeakerView(
      id: s.id,
      name: name,
      labeled: true,
      samples: s.samples,
      createdAt: s.createdAt,
    );
    return true;
  }

  @override
  Future<bool> mergeSpeakers({required String keep, required String drop}) async {
    mergeCalls.add({'keep': keep, 'drop': drop});
    _speakers.removeWhere((s) => s.id == drop);
    return true;
  }

  @override
  Future<bool> deleteSpeaker(String id) async {
    deletedSpeakers.add(id);
    _speakers.removeWhere((s) => s.id == id);
    return true;
  }
}
