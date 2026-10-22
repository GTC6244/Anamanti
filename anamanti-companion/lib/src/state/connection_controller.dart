import 'package:flutter/foundation.dart';

import '../data/companion_api.dart';
import '../data/selection_store.dart';

/// Where the app is in finding a Core.
enum ConnectionPhase {
  /// Browsing mDNS (first launch, or the user asked to look again).
  searching,

  /// A Core is selected; the tabs are live.
  connected,

  /// More than one Core answered and none is saved — the user picks.
  choosing,

  /// No Core answered.
  notFound,
}

/// Health of the selected Core, for the app-bar indicator.
enum ConnectionHealth { ok, reconnecting, offline }

/// Finds the Core over mDNS (via Rust), remembers the user's pick, and re-browses
/// when requests start failing (the Core may have moved to a new address).
class ConnectionController extends ChangeNotifier {
  ConnectionController({
    required this.api,
    required this.store,
    this.browseWindow = const Duration(seconds: 3),
    this.rebrowseCooldown = const Duration(seconds: 10),
    this.failuresBeforeRebrowse = 2,
    DateTime Function()? now,
  }) : _now = now ?? DateTime.now;

  final CompanionApi api;
  final SelectionStore store;
  final Duration browseWindow;
  final Duration rebrowseCooldown;
  final int failuresBeforeRebrowse;
  final DateTime Function() _now;

  ConnectionPhase _phase = ConnectionPhase.searching;
  ConnectionHealth _health = ConnectionHealth.ok;
  List<CoreInfo> _cores = const [];
  CoreInfo? _core;
  String? _savedKey;
  String? _error;
  int _failures = 0;
  DateTime? _lastRebrowse;
  bool _browsing = false;
  bool _disposed = false;

  ConnectionPhase get phase => _phase;
  ConnectionHealth get health => _health;
  List<CoreInfo> get cores => _cores;
  CoreInfo? get core => _core;
  String? get error => _error;

  /// The saved Core's key, when it didn't answer the last browse.
  String? get missingSavedKey =>
      _savedKey != null && !_cores.any((c) => c.key == _savedKey)
      ? _savedKey
      : null;

  /// First launch: load the saved pick, then browse.
  Future<void> start() async {
    try {
      _savedKey = await store.load();
    } catch (_) {
      _savedKey = null;
    }
    await discover();
  }

  /// Browse and settle on a Core. With a saved pick, the browse returns as soon
  /// as it answers; `full` browses the whole window (for the "switch Core" list).
  Future<void> discover({bool full = false}) async {
    if (_browsing) return;
    _browsing = true;
    _phase = ConnectionPhase.searching;
    _error = null;
    _notify();
    try {
      final found = await api.discoverCores(
        window: browseWindow,
        preferredKey: full ? null : _savedKey,
      );
      _cores = found;
      final saved = _savedKey == null
          ? null
          : found.where((c) => c.key == _savedKey).firstOrNull;
      if (saved != null) {
        _connect(saved);
      } else if (found.length == 1 && _savedKey == null) {
        _connect(found.single); // one Core, nothing saved: auto-select
      } else if (found.isEmpty) {
        _phase = ConnectionPhase.notFound;
      } else {
        _phase = ConnectionPhase.choosing;
      }
    } catch (e) {
      _error = describeError(e);
      _phase = ConnectionPhase.notFound;
    } finally {
      _browsing = false;
      _notify();
    }
  }

  /// The user picked a Core: remember it and connect.
  Future<void> select(CoreInfo core) async {
    _savedKey = core.key;
    _connect(core);
    _notify();
    try {
      await store.save(core.key);
    } catch (_) {
      // Not fatal: the pick just won't survive a restart.
    }
  }

  /// Show the picker with a fresh full browse.
  Future<void> switchCore() async {
    await discover(full: true);
    if (_phase == ConnectionPhase.connected && _cores.length > 1) {
      _phase = ConnectionPhase.choosing;
      _notify();
    }
  }

  /// A tab's request succeeded.
  void reportSuccess() {
    if (_failures == 0 && _health == ConnectionHealth.ok) return;
    _failures = 0;
    _health = ConnectionHealth.ok;
    _notify();
  }

  /// A tab's request failed. After a few in a row, re-browse quietly (the tabs
  /// stay up) in case the Core's address changed.
  void reportFailure() {
    _failures++;
    if (_failures < failuresBeforeRebrowse || _browsing || _core == null) {
      return;
    }
    final last = _lastRebrowse;
    if (last != null && _now().difference(last) < rebrowseCooldown) return;
    _lastRebrowse = _now();
    _rebrowse();
  }

  Future<void> _rebrowse() async {
    final current = _core!;
    _browsing = true;
    _health = ConnectionHealth.reconnecting;
    _notify();
    try {
      final found = await api.discoverCores(
        window: browseWindow,
        preferredKey: current.key,
      );
      final again = found.where((c) => c.key == current.key).firstOrNull;
      if (again != null) {
        _core = again; // possibly a new address/port
        _health = ConnectionHealth.ok;
        _failures = 0;
      } else {
        _health = ConnectionHealth.offline;
      }
    } catch (_) {
      _health = ConnectionHealth.offline;
    } finally {
      _browsing = false;
      _notify();
    }
  }

  void _connect(CoreInfo core) {
    _core = core;
    _phase = ConnectionPhase.connected;
    _health = ConnectionHealth.ok;
    _failures = 0;
  }

  void _notify() {
    if (!_disposed) notifyListeners();
  }

  @override
  void dispose() {
    _disposed = true;
    super.dispose();
  }
}
