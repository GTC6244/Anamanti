// Client for the orchestrator-managed settings + memory (Plan.MD §3, Phase 6).
//
// The LLM backend, model, and TTS voice live on the Mac orchestrator, as does the
// persistent memory list. The settings screen reads/changes them through this
// interface, which is expressed in plain domain types (not the generated FRB
// types) so the UI and its widget tests never need the native library loaded — the
// production [FrbOrchestratorClient] is the only place that talks to FRB.

import 'package:anamanti_display/src/rust/api/settings.dart' as frb;

/// The orchestrator's current runtime settings.
class OrchestratorSettingsView {
  const OrchestratorSettingsView({
    required this.ok,
    required this.message,
    required this.llmBackend,
    this.llmModel,
    this.anthropicAuth = 'apikey',
    this.ttsVoice,
    this.endSilenceMs = 0,
    this.voiceRmsThreshold = 0,
    this.vadEngine = '',
    this.sileroThreshold = 0,
  });

  final bool ok;
  final String message;
  final String llmBackend;
  final String? llmModel;

  /// Anthropic auth mode: `apikey` or `subscription` (Claude OAuth).
  final String anthropicAuth;
  final String? ttsVoice;

  /// Orchestrator VAD: end-of-speech trailing silence in ms (0 if unknown).
  final int endSilenceMs;

  /// Orchestrator VAD: speech-vs-noise RMS threshold (0 if unknown).
  final double voiceRmsThreshold;

  /// Orchestrator VAD engine: `energy` or `silero` (empty if unknown).
  final String vadEngine;

  /// Orchestrator VAD: Silero speech-probability gate, 0.0–1.0 (0 if unknown).
  final double sileroThreshold;
}

/// One selectable LLM model for the settings model dropdown (last 12 months).
class ModelOption {
  const ModelOption({
    required this.provider,
    required this.id,
    required this.label,
  });

  /// `anthropic` or `openai`.
  final String provider;

  /// The model id sent as `llmModel` (e.g. `claude-opus-5`, `gpt-4o-mini`).
  final String id;

  /// A human-friendly label for the dropdown (falls back to `id`).
  final String label;
}

/// One selectable Piper voice for the settings TTS voice dropdown, as reported by
/// the orchestrator (its installed-voice list).
class VoiceOption {
  const VoiceOption({required this.name, required this.label, this.language});

  /// The voice id sent as `ttsVoice` (e.g. `en_US-amy-medium`).
  final String name;

  /// A human-friendly label for the dropdown (falls back to `name`).
  final String label;

  /// Primary locale (e.g. `en_US`), or `null` if the server didn't report one.
  final String? language;
}

/// The Google Drive photo-slideshow bundle the orchestrator owns, pulled by the
/// device to drive the idle slideshow. The device mints Drive access tokens
/// on-device from these, so the APK ships credential-free.
class DriveTokenView {
  const DriveTokenView({
    required this.linked,
    required this.configured,
    required this.clientId,
    required this.clientSecret,
    required this.refreshToken,
    required this.folderIds,
    required this.scope,
  });

  /// True when a refresh token is present (Drive is fully linked and usable).
  final bool linked;

  /// True when the OAuth client id + secret are present (access tokens can be
  /// minted). Runtime replacement for the old build-time `kGoogleDriveConfigured`.
  final bool configured;

  final String clientId;
  final String clientSecret;
  final String refreshToken;
  final List<String> folderIds;
  final String scope;
}

/// One capability in the Core's read-only "functions" snapshot, shown on the
/// Settings → Functions screen. Describes whether the function runs in the cloud or
/// locally (or is off), which model it uses, and whether a required API key is set —
/// never the key itself.
class FunctionStatusView {
  const FunctionStatusView({
    required this.category,
    required this.name,
    required this.mode,
    required this.provider,
    required this.model,
    required this.keyRequired,
    required this.keySet,
    required this.note,
  });

  /// Grouping header: `Config` | `Tools` | `System1` | `Memory` | `Speech`.
  final String category;

  /// Human-friendly capability name (e.g. `LLM`, `Weather`, `Embeddings`).
  final String name;

  /// Where it runs: `cloud` | `local` | `off`.
  final String mode;

  /// Provider label (e.g. `Anthropic (Claude)`, `Mapbox`); empty when n/a.
  final String provider;

  /// Selected model / voice / dims; empty when n/a.
  final String model;

  /// Whether this function needs an API key / credential to work.
  final bool keyRequired;

  /// Whether that key/credential is currently set on the Core.
  final bool keySet;

  /// Optional nuance (e.g. a keyless fallback); empty when none.
  final String note;

  /// True when a required key is missing — the UI shows the red warning.
  bool get keyMissing => keyRequired && !keySet;
}

/// One discovered orchestrator, for the settings "Orchestrator" dropdown.
class OrchestratorOption {
  const OrchestratorOption({
    required this.key,
    required this.name,
    required this.host,
    required this.port,
  });

  /// Stable selection key (TXT `instance_id`) persisted device-locally to pin
  /// this orchestrator across restarts / IP changes.
  final String key;

  /// Human-friendly label shown in the dropdown.
  final String name;

  /// Resolved LAN address (for display / diagnostics).
  final String host;

  /// Resolved Wyoming port.
  final int port;
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

/// One entity ("keyword") a turn mentions, decorating a [TurnView].
class EntityRefView {
  const EntityRefView({required this.name, required this.kind});

  /// Canonical surface form (e.g. `jazz`, `Portland`, `Sam`).
  final String name;

  /// Coarse type: `person` | `place` | `thing` | `topic`.
  final String kind;
}

/// One stored conversation turn from the GraphRAG graph (Memory → Conversations).
class TurnView {
  const TurnView({
    required this.id,
    required this.extId,
    required this.session,
    required this.ts,
    required this.transcript,
    required this.reply,
    required this.speakerId,
    required this.entities,
  });

  /// Graph node id, used to delete this turn.
  final int id;

  /// The chat-log record id this turn was ingested from.
  final String extId;

  /// The conversation session id.
  final String session;

  /// Unix seconds when the turn happened.
  final int ts;

  /// What the user said (STT transcript).
  final String transcript;

  /// What the assistant replied.
  final String reply;

  /// Speaker id (`spk-…`, or `household` for the shared/unattributed user).
  final String speakerId;

  /// Entities ("keywords") this turn mentions.
  final List<EntityRefView> entities;
}

/// One stored entity ("keyword") from the GraphRAG graph (Memory → Keywords).
class EntityView {
  const EntityView({required this.id, required this.name, required this.kind});

  /// Graph node id, used to delete this entity.
  final int id;

  /// Canonical surface form.
  final String name;

  /// Coarse type: `person` | `place` | `thing` | `topic`.
  final String kind;
}

/// One identified speaker, for the settings "People" list.
class SpeakerView {
  const SpeakerView({
    required this.id,
    required this.labeled,
    required this.samples,
    required this.createdAt,
    this.name,
  });

  /// Stable id (`spk-…`).
  final String id;

  /// User-given name, or `null` while the cluster is still anonymous.
  final String? name;

  /// Whether a person has named this cluster (vs. auto-created).
  final bool labeled;

  /// How many utterances back this voiceprint.
  final int samples;

  /// Unix seconds when the cluster was first heard.
  final int createdAt;
}

/// One stored voice clip for a speaker, for the People screen's clip playback.
class SpeakerClipView {
  const SpeakerClipView({
    required this.name,
    required this.createdMs,
    required this.samples,
    required this.sampleRate,
  });

  /// Opaque clip id (the key to fetch/play it).
  final String name;

  /// Capture time in Unix milliseconds (newest first).
  final int createdMs;

  /// Number of `i16` samples in the clip.
  final int samples;

  /// Sample rate of the clip's PCM in Hz.
  final int sampleRate;
}

/// Reads and changes orchestrator-side settings and persistent memory. All calls
/// hit the network (mDNS discovery + a short Wyoming control connection) and may
/// throw if the Mac is unreachable; callers surface that as an offline state.
abstract class OrchestratorClient {
  /// Discover every orchestrator on the LAN for the "Orchestrator" dropdown.
  /// Unfiltered by the current selection (pure mDNS), so the picker always shows
  /// all choices. May be empty if none are reachable.
  Future<List<OrchestratorOption>> listOrchestrators();

  Future<OrchestratorSettingsView> fetchSettings();

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
  });

  /// The selectable LLM models for the model dropdown (Anthropic + OpenAI, each
  /// scoped to the last 12 months). May be empty if the Mac is unreachable.
  Future<List<ModelOption>> listModels();

  /// The installed Piper voices for the TTS voice dropdown. May be empty if the
  /// Mac is unreachable or Piper reports no voices.
  Future<List<VoiceOption>> listVoices();

  /// Fetch the Google Drive photo bundle (client creds + refresh token + folder
  /// ids) for the idle slideshow. Throws if the Mac is unreachable; callers fall
  /// back to the last-synced values persisted in app settings.
  Future<DriveTokenView> fetchDriveToken();

  /// Fetch the Core's read-only "functions" snapshot (cloud/local, model, and
  /// whether each required API key is set) for the Settings → Functions screen.
  /// Throws if the Mac is unreachable; callers show an offline guard.
  Future<List<FunctionStatusView>> fetchFunctions();

  Future<List<MemoryView>> listMemories();

  Future<bool> deleteMemory(int id);

  Future<int> clearMemories();

  /// Browse stored conversation turns from the GraphRAG graph (Memory →
  /// Conversations), newest first. All filters optional (empty = no filter):
  /// [query] substring over the turn text, [speakerId], and [entity] (turns
  /// mentioning that keyword). Throws if the Mac is unreachable or the backend has
  /// no graph (SQLite recall).
  Future<List<TurnView>> listTurns({
    String query = '',
    String speakerId = '',
    String entity = '',
    int limit = 100,
  });

  /// Browse stored entities / "keywords" from the GraphRAG graph (Memory →
  /// Keywords). [query] is an optional substring filter on the name.
  Future<List<EntityView>> listEntities({String query = '', int limit = 200});

  /// Drop one graph node (a turn or entity) by its id. Helix-index-only — the
  /// durable SQLite facts and the chat log are untouched. Returns whether the Core
  /// accepted the delete.
  Future<bool> deleteGraphNode(int id);

  /// List the identified speakers (the "People" view).
  Future<List<SpeakerView>> listSpeakers();

  /// The household roster's member names (Core → Household), so voice enrollment
  /// can attach a voiceprint to an existing home member. Empty when there are none.
  Future<List<String>> listHouseholdMembers();

  /// Name (or rename) a speaker; returns whether it was applied.
  Future<bool> nameSpeaker(String id, String name);

  /// Merge the `drop` speaker into `keep` (same person, two clusters).
  Future<bool> mergeSpeakers({required String keep, required String drop});

  /// Link voice [id] to the household member [member] on the Mac: merged into that
  /// member's existing voiceprint (voice, memories, clips) if they have one, otherwise
  /// named as them. Returns the Core's confirmation; throws with the reason (e.g. not
  /// a household member).
  Future<String> linkSpeaker(String id, String member);

  /// Delete a speaker profile; returns whether one was removed.
  Future<bool> deleteSpeaker(String id);

  /// List a speaker's stored voice clips (newest first), for playback. Empty when
  /// the person has no saved clips yet.
  Future<List<SpeakerClipView>> listSpeakerClips(String id);

  /// Play a person's stored voice clips through the assistant's speaker so a human
  /// can listen and identify them. Returns the number of clips queued (0 when there
  /// are none); throws if audio output isn't available or the Mac is unreachable.
  Future<int> playSpeakerClips(String id);

  /// Commit the current guided voice-enrollment session into a named profile
  /// (SpeakerEnrollmentPlan.md Phase 2). A [name] matching a household member
  /// attaches the voiceprint to that member (folding into their existing voiceprint,
  /// if any); a new name also adds them to the household. Returns the orchestrator's
  /// confirmation message; throws with the reason on failure (e.g. too few samples).
  ///
  /// With [speakerId] (People → "Add voice samples") the samples are folded into that
  /// existing person's voiceprint instead; [name] is then only informational.
  Future<String> enrollFinish(String session, String name, {String? speakerId});

  /// Discard an in-flight guided voice-enrollment session (the user backed out).
  Future<void> enrollCancel(String session);

  /// Link a family member's phone to AppSaid by redeeming the 4-character pairing
  /// [code] shown in their AppSaid app and storing it on the Core as a
  /// `send_phone_message` recipient under [name]. Returns the Core's confirmation
  /// message; throws with the reason on failure (bad/expired code, or the Mac is
  /// unreachable).
  Future<String> pairPhone(String name, String code);
}

/// Production client backed by the generated FRB control functions.
class FrbOrchestratorClient implements OrchestratorClient {
  const FrbOrchestratorClient({
    this.discoveryTimeoutSecs = 4,
    this.orchestratorKey = '',
  });

  /// Seconds to browse mDNS for the orchestrator before falling back to the cache.
  final int discoveryTimeoutSecs;

  /// Stable selection key of the pinned orchestrator; empty = Auto (first
  /// available). Threaded into every control call so the settings path hits the
  /// same Mac the voice-turn path does (strict: errors if it's unreachable).
  final String orchestratorKey;

  BigInt get _timeout => BigInt.from(discoveryTimeoutSecs);

  @override
  Future<List<OrchestratorOption>> listOrchestrators() async {
    final list = await frb.listOrchestrators(discoveryTimeoutSecs: _timeout);
    return list
        .map(
          (o) => OrchestratorOption(
            key: o.key,
            name: o.name,
            host: o.host,
            port: o.port,
          ),
        )
        .toList();
  }

  @override
  Future<OrchestratorSettingsView> fetchSettings() async {
    return _view(
      await frb.fetchOrchestratorSettings(
        orchestratorKey: orchestratorKey,
        discoveryTimeoutSecs: _timeout,
      ),
    );
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
    final result = await frb.updateOrchestratorSettings(
      orchestratorKey: orchestratorKey,
      update: frb.SettingsUpdate(
        llmBackend: llmBackend,
        llmModel: llmModel,
        anthropicAuth: anthropicAuth,
        setTtsVoice: setTtsVoice,
        ttsVoice: ttsVoice,
        endSilenceMs: endSilenceMs,
        voiceRmsThreshold: voiceRmsThreshold,
        vadEngine: vadEngine,
        sileroThreshold: sileroThreshold,
      ),
      discoveryTimeoutSecs: _timeout,
    );
    return _view(result);
  }

  @override
  Future<List<ModelOption>> listModels() async {
    final models = await frb.listModels(
      orchestratorKey: orchestratorKey,
      discoveryTimeoutSecs: _timeout,
    );
    return models
        .map((m) => ModelOption(provider: m.provider, id: m.id, label: m.label))
        .toList();
  }

  @override
  Future<List<VoiceOption>> listVoices() async {
    final voices = await frb.listVoices(
      orchestratorKey: orchestratorKey,
      discoveryTimeoutSecs: _timeout,
    );
    return voices
        .map(
          (v) =>
              VoiceOption(name: v.name, label: v.label, language: v.language),
        )
        .toList();
  }

  @override
  Future<DriveTokenView> fetchDriveToken() async {
    final t = await frb.getDriveToken(
      orchestratorKey: orchestratorKey,
      discoveryTimeoutSecs: _timeout,
    );
    return DriveTokenView(
      linked: t.linked,
      configured: t.configured,
      clientId: t.clientId,
      clientSecret: t.clientSecret,
      refreshToken: t.refreshToken,
      folderIds: t.folderIds,
      scope: t.scope,
    );
  }

  @override
  Future<List<FunctionStatusView>> fetchFunctions() async {
    final fns = await frb.getFunctions(
      orchestratorKey: orchestratorKey,
      discoveryTimeoutSecs: _timeout,
    );
    return fns
        .map(
          (f) => FunctionStatusView(
            category: f.category,
            name: f.name,
            mode: f.mode,
            provider: f.provider,
            model: f.model,
            keyRequired: f.keyRequired,
            keySet: f.keySet,
            note: f.note,
          ),
        )
        .toList();
  }

  @override
  Future<List<MemoryView>> listMemories() async {
    final entries = await frb.listMemories(
      orchestratorKey: orchestratorKey,
      discoveryTimeoutSecs: _timeout,
    );
    return entries
        .map(
          (e) => MemoryView(
            id: e.id.toInt(),
            kind: e.kind,
            content: e.content,
            source: e.source,
            createdAt: e.createdAt.toInt(),
          ),
        )
        .toList();
  }

  @override
  Future<bool> deleteMemory(int id) => frb.deleteMemory(
    orchestratorKey: orchestratorKey,
    id: id,
    discoveryTimeoutSecs: _timeout,
  );

  @override
  Future<int> clearMemories() => frb.clearMemories(
    orchestratorKey: orchestratorKey,
    discoveryTimeoutSecs: _timeout,
  );

  @override
  Future<List<TurnView>> listTurns({
    String query = '',
    String speakerId = '',
    String entity = '',
    int limit = 100,
  }) async {
    final turns = await frb.listTurns(
      orchestratorKey: orchestratorKey,
      query: query,
      speakerId: speakerId,
      entity: entity,
      limit: limit,
      discoveryTimeoutSecs: _timeout,
    );
    return turns
        .map(
          (t) => TurnView(
            id: t.id.toInt(),
            extId: t.extId,
            session: t.session,
            ts: t.ts.toInt(),
            transcript: t.transcript,
            reply: t.reply,
            speakerId: t.speakerId,
            entities: t.entities
                .map((e) => EntityRefView(name: e.name, kind: e.kind))
                .toList(),
          ),
        )
        .toList();
  }

  @override
  Future<List<EntityView>> listEntities({
    String query = '',
    int limit = 200,
  }) async {
    final entities = await frb.listEntities(
      orchestratorKey: orchestratorKey,
      query: query,
      limit: limit,
      discoveryTimeoutSecs: _timeout,
    );
    return entities
        .map((e) => EntityView(id: e.id.toInt(), name: e.name, kind: e.kind))
        .toList();
  }

  @override
  Future<bool> deleteGraphNode(int id) => frb.deleteGraphNode(
    orchestratorKey: orchestratorKey,
    id: id,
    discoveryTimeoutSecs: _timeout,
  );

  @override
  Future<List<SpeakerView>> listSpeakers() async {
    final people = await frb.listSpeakers(
      orchestratorKey: orchestratorKey,
      discoveryTimeoutSecs: _timeout,
    );
    return people
        .map(
          (s) => SpeakerView(
            id: s.id,
            name: s.name,
            labeled: s.labeled,
            samples: s.samples.toInt(),
            createdAt: s.createdAt.toInt(),
          ),
        )
        .toList();
  }

  @override
  Future<List<String>> listHouseholdMembers() => frb.listHouseholdMembers(
    orchestratorKey: orchestratorKey,
    discoveryTimeoutSecs: _timeout,
  );

  @override
  Future<bool> nameSpeaker(String id, String name) => frb.nameSpeaker(
    orchestratorKey: orchestratorKey,
    id: id,
    name: name,
    discoveryTimeoutSecs: _timeout,
  );

  @override
  Future<bool> mergeSpeakers({required String keep, required String drop}) =>
      frb.mergeSpeakers(
        orchestratorKey: orchestratorKey,
        keep: keep,
        drop: drop,
        discoveryTimeoutSecs: _timeout,
      );

  @override
  Future<String> linkSpeaker(String id, String member) => frb.linkSpeaker(
    orchestratorKey: orchestratorKey,
    id: id,
    member: member,
    discoveryTimeoutSecs: _timeout,
  );

  @override
  Future<bool> deleteSpeaker(String id) => frb.deleteSpeaker(
    orchestratorKey: orchestratorKey,
    id: id,
    discoveryTimeoutSecs: _timeout,
  );

  @override
  Future<List<SpeakerClipView>> listSpeakerClips(String id) async {
    final clips = await frb.listSpeakerClips(
      orchestratorKey: orchestratorKey,
      id: id,
      discoveryTimeoutSecs: _timeout,
    );
    return clips
        .map(
          (c) => SpeakerClipView(
            name: c.name,
            createdMs: c.createdMs.toInt(),
            samples: c.samples.toInt(),
            sampleRate: c.sampleRate,
          ),
        )
        .toList();
  }

  @override
  Future<int> playSpeakerClips(String id) => frb.playSpeakerClips(
    orchestratorKey: orchestratorKey,
    id: id,
    discoveryTimeoutSecs: _timeout,
  );

  @override
  Future<String> enrollFinish(
    String session,
    String name, {
    String? speakerId,
  }) => frb.enrollFinish(
    orchestratorKey: orchestratorKey,
    session: session,
    name: name,
    speakerId: speakerId,
    discoveryTimeoutSecs: _timeout,
  );

  @override
  Future<void> enrollCancel(String session) => frb.enrollCancel(
    orchestratorKey: orchestratorKey,
    session: session,
    discoveryTimeoutSecs: _timeout,
  );

  @override
  Future<String> pairPhone(String name, String code) => frb.pairPhone(
    orchestratorKey: orchestratorKey,
    name: name,
    code: code,
    discoveryTimeoutSecs: _timeout,
  );

  OrchestratorSettingsView _view(frb.OrchestratorSettings s) =>
      OrchestratorSettingsView(
        ok: s.ok,
        message: s.message,
        llmBackend: s.llmBackend,
        llmModel: s.llmModel,
        anthropicAuth: s.anthropicAuth,
        ttsVoice: s.ttsVoice,
        endSilenceMs: s.endSilenceMs,
        voiceRmsThreshold: s.voiceRmsThreshold,
        vadEngine: s.vadEngine,
        sileroThreshold: s.sileroThreshold,
      );
}
