// Shared fakes for widget tests: a scriptable CompanionApi (no Rust needed — the
// FRB data classes are plain Dart), an in-memory selection store, a manual
// monotonic clock, and sample data.

import 'dart:convert';

import 'package:anamanti_companion/src/data/companion_api.dart';
import 'package:anamanti_companion/src/data/selection_store.dart';

CoreInfo sampleCore({
  String key = 'paul-family',
  String name = 'Kitchen Mac',
  int? companionPort = 8740,
  String address = '192.168.1.50',
}) => CoreInfo(
  key: key,
  name: name,
  host: '$key.local.',
  address: address,
  wyomingPort: 10700,
  companionPort: companionPort,
);

String sampleOverviewJson({List<Map<String, Object?>>? stages}) => jsonEncode({
  'ok': true,
  'service_name': 'Anamanti Core',
  'instance_id': 'paul-family',
  'version': '0.9.0',
  'uptime_secs': 7260,
  'stages':
      stages ??
      [
        {
          'key': 'vad',
          'label': 'End-of-speech',
          'engine': 'silero',
          'tier': 'local',
          'status': 'ok',
        },
        {
          'key': 'stt',
          'label': 'Speech-to-text',
          'engine': 'Wyoming Whisper',
          'tier': 'lan',
          'status': 'error',
          'detail': '192.168.1.20:10300',
        },
        {
          'key': 'llm',
          'label': 'Language model',
          'engine': 'Claude claude-opus-5',
          'tier': 'cloud',
          'status': 'ok',
        },
        {
          'key': 'search',
          'label': 'Web search',
          'engine': 'Off',
          'tier': 'off',
          'status': 'off',
        },
      ],
  'devices': [
    {'device_id': 'anamanti-140ac5942aca', 'name': 'Kitchen Show'},
  ],
  'integrations': [
    {
      'key': 'weather',
      'name': 'Weather',
      'status': 'active',
      'detail': 'Visual Crossing',
    },
  ],
  'turns': [],
  'latency': {'count': 12, 'p50_ms': 1250, 'p95_ms': 2900},
});

NowPlayingTrack sampleTrack({
  bool playing = true,
  String source = 'spotify',
  int position = 60,
  int duration = 300,
  String stationName = '',
}) => NowPlayingTrack(
  playing: playing,
  trackTitle: source == 'radio' ? 'CBC Music' : 'So What',
  artist: source == 'radio' ? '' : 'Miles Davis',
  album: source == 'radio' ? stationName : 'Kind of Blue',
  artworkUri: '',
  positionSecs: position,
  durationSecs: duration,
  volumePercent: 40,
  source: source,
  stationId: source == 'radio' ? 'CBCM' : '',
  stationName: stationName,
);

CoreTimer sampleTimer({
  int id = 1,
  String label = 'pasta',
  int duration = 600,
  int remaining = 125,
  bool ringing = false,
}) => CoreTimer(
  id: id,
  label: label,
  durationSecs: duration,
  remainingSecs: remaining,
  ringing: ringing,
  endsAtMs: 0,
);

MemoryEntry sampleMemory(int id, String content, {String kind = 'fact'}) =>
    MemoryEntry(
      id: id,
      kind: kind,
      content: content,
      source: 'explicit',
      createdAt: 1760000000,
    );

/// A scriptable [CompanionApi]. Set the `next*` fields (a value, or an
/// `Exception` to throw) and inspect the call logs.
class FakeCompanionApi implements CompanionApi {
  Object discoverResult = <CoreInfo>[];
  Object overviewResult = sampleOverviewJson();
  Object nowPlayingResult = const NowPlayingStatus(
    linked: true,
    radio: false,
    nowPlaying: null,
  );
  Object timersResult = const TimersSnapshot(nowMs: 0, displays: []);
  Object memoriesResult = <MemoryEntry>[];
  Object deleteResult = true;
  Object clearResult = 0;

  final List<String?> discoverCalls = [];
  final List<bool> overviewProbes = [];
  int nowPlayingCalls = 0;
  int timersCalls = 0;
  int listCalls = 0;
  final List<int> deletedIds = [];
  int clearCalls = 0;

  T _answer<T>(Object result) {
    if (result is Exception) throw result;
    if (result is Error) throw result;
    return result as T;
  }

  @override
  Future<List<CoreInfo>> discoverCores({
    required Duration window,
    String? preferredKey,
  }) async {
    discoverCalls.add(preferredKey);
    return _answer(discoverResult);
  }

  @override
  Future<String> fetchOverview(CoreInfo core, {required bool probe}) async {
    overviewProbes.add(probe);
    return _answer(overviewResult);
  }

  @override
  Future<NowPlayingStatus> fetchNowPlaying(CoreInfo core) async {
    nowPlayingCalls++;
    return _answer(nowPlayingResult);
  }

  @override
  Future<TimersSnapshot> fetchTimers(CoreInfo core) async {
    timersCalls++;
    return _answer(timersResult);
  }

  @override
  Future<List<MemoryEntry>> listMemories(CoreInfo core) async {
    listCalls++;
    return List.of(_answer<List<MemoryEntry>>(memoriesResult));
  }

  @override
  Future<bool> deleteMemory(CoreInfo core, int id) async {
    deletedIds.add(id);
    return _answer(deleteResult);
  }

  @override
  Future<int> clearMemories(CoreInfo core) async {
    clearCalls++;
    return _answer(clearResult);
  }
}

class MemorySelectionStore implements SelectionStore {
  MemorySelectionStore([this.value]);
  String? value;

  @override
  Future<String?> load() async => value;

  @override
  Future<void> save(String key) async => value = key;

  @override
  Future<void> clear() async => value = null;
}

/// A manual monotonic clock for countdown/progress tests.
class ManualClock {
  Duration now = Duration.zero;
  Duration call() => now;
  void advance(Duration d) => now += d;
}
