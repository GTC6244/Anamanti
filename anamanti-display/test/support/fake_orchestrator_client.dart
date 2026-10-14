// A fake [OrchestratorClient] for widget tests: no native library, no network.
// Records calls and returns canned data so the settings/memory screens can be
// driven deterministically.

import 'dart:async';

import 'package:anamanti_display/src/settings/orchestrator_client.dart';

class FakeOrchestratorClient implements OrchestratorClient {
  FakeOrchestratorClient({
    OrchestratorSettingsView? settings,
    List<MemoryView>? memories,
    List<SpeakerView>? speakers,
    List<ModelOption>? models,
    List<VoiceOption>? voices,
    List<OrchestratorOption>? orchestrators,
    DriveTokenView? driveToken,
    List<FunctionStatusView>? functions,
    List<TurnView>? turns,
    List<EntityView>? entities,
    Map<String, List<SpeakerClipView>>? clipsBySpeaker,
    List<String>? householdMembers,
    this.throwOnFetch = false,
  }) : _householdMembers = householdMembers ?? <String>[],
       _turns = turns ?? <TurnView>[],
       _clipsBySpeaker = clipsBySpeaker ?? <String, List<SpeakerClipView>>{},
       _entities = entities ?? <EntityView>[],
       _functions = functions ?? <FunctionStatusView>[],
       _driveToken =
           driveToken ??
           const DriveTokenView(
             linked: false,
             configured: false,
             clientId: '',
             clientSecret: '',
             refreshToken: '',
             folderIds: <String>[],
             scope: '',
           ),
       _orchestrators =
           orchestrators ??
           const <OrchestratorOption>[
             OrchestratorOption(
               key: 'mac-mini',
               name: 'Mac Mini',
               host: '192.168.1.10',
               port: 10700,
             ),
           ],
       _models =
           models ??
           const <ModelOption>[
             ModelOption(
               provider: 'anthropic',
               id: 'claude-opus-5',
               label: 'Claude Opus 5',
             ),
             ModelOption(
               provider: 'openai',
               id: 'gpt-4o-mini',
               label: 'gpt-4o-mini',
             ),
           ],
       _voices =
           voices ??
           const <VoiceOption>[
             VoiceOption(
               name: 'en_US-amy-medium',
               label: 'amy (medium)',
               language: 'en_US',
             ),
             VoiceOption(
               name: 'en_US-lessac-medium',
               label: 'lessac (medium)',
               language: 'en_US',
             ),
           ],
       _settings =
           settings ??
           const OrchestratorSettingsView(
             ok: true,
             message: 'ok',
             llmBackend: 'ollama',
             llmModel: 'llama3.2',
             ttsVoice: null,
           ),
       _memories = memories ?? <MemoryView>[],
       _speakers = List.of(speakers ?? const <SpeakerView>[]);

  OrchestratorSettingsView _settings;
  final List<MemoryView> _memories;
  final List<SpeakerView> _speakers;
  final List<String> _householdMembers;
  final List<ModelOption> _models;
  final List<VoiceOption> _voices;
  final List<OrchestratorOption> _orchestrators;
  final DriveTokenView _driveToken;
  final List<FunctionStatusView> _functions;
  final List<TurnView> _turns;
  final List<EntityView> _entities;
  final bool throwOnFetch;

  // Call records for assertions.
  int fetchCount = 0;
  final List<Map<String, dynamic>> applyCalls = <Map<String, dynamic>>[];
  final List<int> deleted = <int>[];
  int clearCount = 0;
  final List<Map<String, String>> namedCalls = <Map<String, String>>[];
  final List<Map<String, String>> mergeCalls = <Map<String, String>>[];
  final List<String> deletedSpeakers = <String>[];
  final List<Map<String, String>> enrollFinishCalls = <Map<String, String>>[];
  final List<String> enrollCancelCalls = <String>[];
  final List<Map<String, String>> pairPhoneCalls = <Map<String, String>>[];

  // Per-speaker stored voice clips, and a record of play requests.
  final Map<String, List<SpeakerClipView>> _clipsBySpeaker;
  final List<String> playedClips = <String>[];

  // GraphRAG browse/delete call records.
  final List<Map<String, dynamic>> listTurnsCalls = <Map<String, dynamic>>[];
  final List<Map<String, dynamic>> listEntitiesCalls = <Map<String, dynamic>>[];
  final List<int> deletedGraphNodes = <int>[];

  /// When non-null, [enrollFinish] throws this instead of succeeding (simulates the
  /// orchestrator rejecting the commit, e.g. too few samples).
  String? enrollFinishError;

  /// When set, [applySettings] records the call and then waits for this to complete
  /// (simulates an apply still in flight on the Mac).
  Completer<void>? applyGate;

  /// When true, [applySettings] throws (simulates the Mac dropping off mid-edit).
  bool throwOnApply = false;

  /// When non-null, [applySettings] returns `ok: false` with this message
  /// (simulates the Core rejecting a change).
  String? rejectApply;

  /// When set, [pairPhone] throws with this message (simulates a bad/expired code
  /// or an unreachable Mac).
  String? pairPhoneError;

  @override
  Future<List<OrchestratorOption>> listOrchestrators() async {
    // Discovery is a separate mDNS path from the selected-orchestrator round
    // trip, so it succeeds even when `throwOnFetch` simulates the selected Mac
    // being unreachable.
    return List.of(_orchestrators);
  }

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
    String? vadEngine,
    double? sileroThreshold,
  }) async {
    applyCalls.add({
      'llmBackend': llmBackend,
      'llmModel': llmModel,
      'anthropicAuth': anthropicAuth,
      'setTtsVoice': setTtsVoice,
      'ttsVoice': ttsVoice,
      'endSilenceMs': endSilenceMs,
      'voiceRmsThreshold': voiceRmsThreshold,
      'vadEngine': vadEngine,
      'sileroThreshold': sileroThreshold,
    });
    final gate = applyGate;
    if (gate != null) await gate.future;
    if (throwOnApply) throw Exception('offline');
    final reject = rejectApply;
    if (reject != null) {
      return OrchestratorSettingsView(
        ok: false,
        message: reject,
        llmBackend: _settings.llmBackend,
      );
    }
    _settings = OrchestratorSettingsView(
      ok: true,
      message: 'applied',
      llmBackend: llmBackend ?? _settings.llmBackend,
      llmModel: llmModel ?? _settings.llmModel,
      anthropicAuth: anthropicAuth ?? _settings.anthropicAuth,
      ttsVoice: setTtsVoice ? ttsVoice : _settings.ttsVoice,
      endSilenceMs: endSilenceMs ?? _settings.endSilenceMs,
      voiceRmsThreshold: voiceRmsThreshold ?? _settings.voiceRmsThreshold,
      vadEngine: vadEngine ?? _settings.vadEngine,
      sileroThreshold: sileroThreshold ?? _settings.sileroThreshold,
    );
    return _settings;
  }

  @override
  Future<List<ModelOption>> listModels() async {
    if (throwOnFetch) throw Exception('offline');
    return List.of(_models);
  }

  @override
  Future<List<VoiceOption>> listVoices() async {
    if (throwOnFetch) throw Exception('offline');
    return List.of(_voices);
  }

  @override
  Future<DriveTokenView> fetchDriveToken() async {
    if (throwOnFetch) throw Exception('offline');
    return _driveToken;
  }

  @override
  Future<List<FunctionStatusView>> fetchFunctions() async {
    if (throwOnFetch) throw Exception('offline');
    return List.of(_functions);
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
  Future<List<TurnView>> listTurns({
    String query = '',
    String speakerId = '',
    String entity = '',
    int limit = 100,
  }) async {
    listTurnsCalls.add({
      'query': query,
      'speakerId': speakerId,
      'entity': entity,
      'limit': limit,
    });
    if (throwOnFetch) throw Exception('offline');
    Iterable<TurnView> out = _turns;
    if (query.isNotEmpty) {
      final q = query.toLowerCase();
      out = out.where(
        (t) =>
            t.transcript.toLowerCase().contains(q) ||
            t.reply.toLowerCase().contains(q),
      );
    }
    if (speakerId.isNotEmpty) {
      out = out.where((t) => t.speakerId == speakerId);
    }
    if (entity.isNotEmpty) {
      out = out.where((t) => t.entities.any((e) => e.name == entity));
    }
    return out.toList();
  }

  @override
  Future<List<EntityView>> listEntities({
    String query = '',
    int limit = 200,
  }) async {
    listEntitiesCalls.add({'query': query, 'limit': limit});
    if (throwOnFetch) throw Exception('offline');
    if (query.isEmpty) return List.of(_entities);
    final q = query.toLowerCase();
    return _entities.where((e) => e.name.toLowerCase().contains(q)).toList();
  }

  @override
  Future<bool> deleteGraphNode(int id) async {
    deletedGraphNodes.add(id);
    _turns.removeWhere((t) => t.id == id);
    _entities.removeWhere((e) => e.id == id);
    return true;
  }

  @override
  Future<List<SpeakerView>> listSpeakers() async => List.of(_speakers);

  @override
  Future<List<String>> listHouseholdMembers() async {
    if (throwOnFetch) throw Exception('offline');
    return List.of(_householdMembers);
  }

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
  Future<bool> mergeSpeakers({
    required String keep,
    required String drop,
  }) async {
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

  @override
  Future<List<SpeakerClipView>> listSpeakerClips(String id) async {
    if (throwOnFetch) throw Exception('offline');
    return List.of(_clipsBySpeaker[id] ?? const <SpeakerClipView>[]);
  }

  @override
  Future<int> playSpeakerClips(String id) async {
    playedClips.add(id);
    if (throwOnFetch) throw Exception('offline');
    return (_clipsBySpeaker[id] ?? const <SpeakerClipView>[]).length;
  }

  @override
  Future<String> enrollFinish(
    String session,
    String name, {
    String? speakerId,
  }) async {
    enrollFinishCalls.add({
      'session': session,
      'name': name,
      'speakerId': ?speakerId,
    });
    final err = enrollFinishError;
    if (err != null) throw Exception(err);
    if (speakerId != null) {
      final i = _speakers.indexWhere((s) => s.id == speakerId);
      if (i < 0) throw Exception("that person's voiceprint no longer exists");
      final s = _speakers[i];
      _speakers[i] = SpeakerView(
        id: s.id,
        name: s.name,
        labeled: s.labeled,
        samples: s.samples + 3,
        createdAt: s.createdAt,
      );
      return 'added 3 samples to ${s.name ?? 'this person'}\'s voiceprint';
    }
    _speakers.add(
      SpeakerView(
        id: 'spk-$session',
        name: name,
        labeled: true,
        samples: 3,
        createdAt: 0,
      ),
    );
    return 'enrolled $name from 3 samples';
  }

  @override
  Future<void> enrollCancel(String session) async {
    enrollCancelCalls.add(session);
  }

  @override
  Future<String> pairPhone(String name, String code) async {
    pairPhoneCalls.add({'name': name, 'code': code});
    final err = pairPhoneError;
    if (err != null) throw Exception(err);
    return "Linked $name's phone.";
  }
}
