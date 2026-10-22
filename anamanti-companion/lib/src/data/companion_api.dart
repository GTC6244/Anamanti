// The seam between the UI and the Rust engine. Screens and the connection
// controller depend on [CompanionApi], never on the generated FRB functions
// directly, so widget tests can swap in a fake (test/support/fake_api.dart)
// without loading the native library.
//
// All networking (mDNS, HTTP, Wyoming) happens in Rust; this file only forwards.
// The data classes are the FRB-generated plain Dart classes, which are
// constructible without Rust.

import 'package:flutter_rust_bridge/flutter_rust_bridge.dart'
    show AnyhowException;

import '../rust/api/companion.dart' as rust;

export '../rust/api/companion.dart'
    show
        CoreInfo,
        CoreTimer,
        DisplayTimers,
        MemoryEntry,
        NowPlayingStatus,
        NowPlayingTrack,
        TimersSnapshot;

abstract class CompanionApi {
  /// Browse mDNS for Cores. Ends early once [preferredKey] answers.
  Future<List<rust.CoreInfo>> discoverCores({
    required Duration window,
    String? preferredKey,
  });

  /// `GET /overview.json` as raw JSON. `probe: false` → `?probe=0`.
  Future<String> fetchOverview(rust.CoreInfo core, {required bool probe});

  Future<rust.NowPlayingStatus> fetchNowPlaying(rust.CoreInfo core);

  Future<rust.TimersSnapshot> fetchTimers(rust.CoreInfo core);

  Future<List<rust.MemoryEntry>> listMemories(rust.CoreInfo core);

  /// `true` if the entry was removed.
  Future<bool> deleteMemory(rust.CoreInfo core, int id);

  /// Number of entries removed.
  Future<int> clearMemories(rust.CoreInfo core);
}

/// The real implementation: thin forwards to the FRB bindings.
class RustCompanionApi implements CompanionApi {
  const RustCompanionApi();

  @override
  Future<List<rust.CoreInfo>> discoverCores({
    required Duration window,
    String? preferredKey,
  }) => rust.discoverCores(
    timeoutMs: window.inMilliseconds,
    preferredKey: preferredKey,
  );

  @override
  Future<String> fetchOverview(rust.CoreInfo core, {required bool probe}) =>
      rust.fetchOverview(core: core, probe: probe);

  @override
  Future<rust.NowPlayingStatus> fetchNowPlaying(rust.CoreInfo core) =>
      rust.fetchNowPlaying(core: core);

  @override
  Future<rust.TimersSnapshot> fetchTimers(rust.CoreInfo core) =>
      rust.fetchTimers(core: core);

  @override
  Future<List<rust.MemoryEntry>> listMemories(rust.CoreInfo core) =>
      rust.listMemories(core: core);

  @override
  Future<bool> deleteMemory(rust.CoreInfo core, int id) =>
      rust.deleteMemory(core: core, id: id);

  @override
  Future<int> clearMemories(rust.CoreInfo core) =>
      rust.clearMemories(core: core);
}

/// A user-facing message for an error thrown across the bridge.
String describeError(Object error) {
  if (error is AnyhowException) return error.message;
  return error.toString();
}
