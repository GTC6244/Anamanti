// Music now-playing channel (the twin of the weather channel).
//
// The Rust engine holds a persistent "music" channel open to the pinned orchestrator
// and streams now-playing pushes (see `start_music_channel` in `rust/src/api/engine.rs`).
// This controller subscribes to that stream and forwards each push's JSON to [onPush] —
// which the app shell wires to `AssistantController.applyMusicPush`, so the compact music
// control overlay and the full music screens reflect what's playing. An empty payload is
// a "dismiss" (nothing playing). Independent of the voice-turn lifecycle, like the
// weather and notify channels.

import 'dart:async';

import 'package:flutter/foundation.dart';

import 'package:anamanti_display/src/rust/api/engine.dart';

/// Injectable stream factory so widget tests can drive music pushes without the native
/// channel. Defaults to [startMusicChannel].
typedef MusicStreamFactory = Stream<MusicPush> Function(MusicConfig);

/// Owns the music channel subscription and forwards pushes to [onPush].
class MusicChannelController {
  MusicChannelController({
    required MusicConfig config,
    required this.onPush,
    MusicStreamFactory? startChannel,
    // A `this._config` initializing formal would be an unusable private named
    // parameter, so assign it here.
  })  : _config = config, // ignore: prefer_initializing_formals
        _startChannel = startChannel ?? _defaultChannel;

  static Stream<MusicPush> _defaultChannel(MusicConfig config) =>
      startMusicChannel(config: config);

  final MusicConfig _config;
  final MusicStreamFactory _startChannel;

  /// Called with each pushed now-playing JSON string (empty = dismiss / nothing playing).
  final void Function(String nowPlayingJson) onPush;

  StreamSubscription<MusicPush>? _sub;

  /// Subscribe to the music channel. Safe to call once; a second call is a no-op.
  void start() {
    if (_sub != null) return;
    _sub = _startChannel(_config).listen(
      (push) => onPush(push.nowPlayingJson),
      onError: (Object e, StackTrace _) => debugPrint('music channel error: $e'),
      onDone: () => debugPrint('music channel closed'),
      cancelOnError: false,
    );
  }

  void dispose() {
    _sub?.cancel();
  }
}
