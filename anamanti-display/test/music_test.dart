// Music mode: the AssistantController folding now-playing pushes into AssistantState,
// the music-screen navigation, and the transport/volume control sink. The MusicData
// parser and the widgets have their own coverage in music_widget_test.dart.

import 'dart:async';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:flutter_test/flutter_test.dart';

// A now-playing snapshot with no artwork, so nothing a widget renders fetches over the
// network. Mirrors the Core's `NowPlaying` serialization (snake_case).
const _musicJson = '''
{"playing":true,"track_title":"Paranoid Android","artist":"Radiohead",
 "album":"OK Computer","artwork_uri":"","position_secs":73,"duration_secs":383,
 "volume_percent":65,
 "next_up":[{"track_title":"Let Down","artist":"Radiohead","album":"OK Computer","artwork_uri":""}]}
''';

const _pausedJson = '''
{"playing":false,"track_title":"Let Down","artist":"Radiohead","album":"OK Computer",
 "artwork_uri":"","position_secs":10,"duration_secs":299,"volume_percent":40,"next_up":[]}
''';

WakeWordConfig _cfg() => WakeWordConfig(
  melspecModelPath: '',
  embeddingModelPath: '',
  wakewordModelPath: '',
  modelName: 'test',
  threshold: 0.5,
  activeThreshold: 0.7,
  orchestratorKey: '',
  discoveryTimeoutSecs: BigInt.zero,
  turnTimeoutSecs: BigInt.zero,
  smoothingWindow: 2,
  fireOnPeak: false,
  playbackBufferSecs: 30,
  captureGainDb: 0,
  useAudiorecord: false,
  micSource: 6,
  platformAec: false,
  platformAgc: true,
  platformNs: true,
  cameraProximity: false,
  proximityMotionThreshold: 0,
  proximityReleaseSecs: 0,
);

/// A controller wired with a capturing music-control sink. The engine stream is never
/// started (the music path is driven directly via [applyMusicPush] + the UI methods), so
/// no native library is needed.
AssistantController _controller(List<Map<String, Object?>> sink) => AssistantController(
  config: _cfg(),
  musicControl: ({required String action, int? value}) =>
      sink.add({'action': action, 'value': value}),
);

/// A `musicScreen` voice event carrying the target `screen` token.
WakeWordEvent _musicScreenEvent(String screen) => WakeWordEvent(
  kind: WakeWordEventKind.musicScreen,
  message: '',
  device: '',
  deviceSampleRate: 0,
  channels: 0,
  rms: 0,
  score: 0,
  avgScore: 0,
  threshold: 0,
  gainDb: 0,
  model: '',
  transcript: '',
  reply: '',
  timerId: 0,
  timerLabel: '',
  timerRemainingSecs: 0,
  present: false,
  recipeJson: '',
  weatherJson: '',
  placeJson: '',
  recipeAction: '',
  musicScreen: screen,
);

void main() {
  group('AssistantController music pushes', () {
    test('a now-playing push populates music state (overlay only, screen hidden)', () {
      final controller = _controller([]);
      addTearDown(controller.dispose);

      expect(controller.state.musicActive, isFalse);
      controller.applyMusicPush(_musicJson);

      final s = controller.state;
      expect(s.musicActive, isTrue);
      expect(s.music!.trackTitle, 'Paranoid Android');
      expect(s.music!.artist, 'Radiohead');
      expect(s.music!.volumePercent, 65);
      expect(s.music!.nextUp, hasLength(1));
      // A push alone never opens a full screen — only the compact overlay rides.
      expect(s.musicScreen, MusicScreen.hidden);
      expect(s.musicScreenActive, isFalse);
    });

    test('an empty push dismisses music and hides any full screen', () {
      final controller = _controller([]);
      addTearDown(controller.dispose);

      controller.applyMusicPush(_musicJson);
      controller.openNowPlaying();
      expect(controller.state.musicScreenActive, isTrue);

      controller.applyMusicPush('');
      expect(controller.state.musicActive, isFalse);
      expect(controller.state.music, isNull);
      expect(controller.state.musicScreen, MusicScreen.hidden);
    });

    test('a malformed non-empty push is ignored, keeping the last snapshot', () {
      final controller = _controller([]);
      addTearDown(controller.dispose);

      controller.applyMusicPush(_musicJson);
      controller.applyMusicPush('not json at all');
      expect(controller.state.music!.trackTitle, 'Paranoid Android');
    });
  });

  group('AssistantController music screen navigation', () {
    test('open → queue → back → close transitions', () {
      final controller = _controller([]);
      addTearDown(controller.dispose);
      controller.applyMusicPush(_musicJson);

      controller.openNowPlaying();
      expect(controller.state.musicScreen, MusicScreen.nowPlaying);
      expect(controller.state.screenAwake, isTrue);

      controller.showMusicQueue();
      expect(controller.state.musicScreen, MusicScreen.nextUp);

      controller.backToNowPlaying();
      expect(controller.state.musicScreen, MusicScreen.nowPlaying);

      // Closing returns to the overlay but keeps the track playing.
      controller.closeMusicScreen();
      expect(controller.state.musicScreen, MusicScreen.hidden);
      expect(controller.state.musicActive, isTrue);
    });

    test('screen methods are no-ops when nothing is playing', () {
      final controller = _controller([]);
      addTearDown(controller.dispose);
      controller.openNowPlaying();
      expect(controller.state.musicScreen, MusicScreen.hidden);
      controller.showMusicQueue();
      expect(controller.state.musicScreen, MusicScreen.hidden);
    });

    test('a later push keeps the open screen (updates in place)', () {
      final controller = _controller([]);
      addTearDown(controller.dispose);
      controller.applyMusicPush(_musicJson);
      controller.openNowPlaying();
      controller.applyMusicPush(_pausedJson);
      expect(controller.state.musicScreen, MusicScreen.nowPlaying);
      expect(controller.state.music!.playing, isFalse);
    });
  });

  group('AssistantController music transport', () {
    test('play/pause sends the explicit opposite of the current state', () {
      final calls = <Map<String, Object?>>[];
      final controller = _controller(calls);
      addTearDown(controller.dispose);

      controller.applyMusicPush(_musicJson); // playing: true
      controller.playPauseMusic();
      expect(calls.last, {'action': 'pause', 'value': null});

      controller.applyMusicPush(_pausedJson); // playing: false
      controller.playPauseMusic();
      expect(calls.last, {'action': 'resume', 'value': null});
    });

    test('next / previous / volume send the right action and value', () {
      final calls = <Map<String, Object?>>[];
      final controller = _controller(calls);
      addTearDown(controller.dispose);
      controller.applyMusicPush(_musicJson);

      controller.nextTrack();
      expect(calls.last, {'action': 'next', 'value': null});

      controller.previousTrack();
      expect(calls.last, {'action': 'previous', 'value': null});

      controller.setMusicVolume(40);
      expect(calls.last, {'action': 'volume', 'value': 40});

      // Volume is clamped into 0..100.
      controller.setMusicVolume(250);
      expect(calls.last, {'action': 'volume', 'value': 100});
    });

    test('transport is a no-op when no control sink is wired', () {
      final controller = AssistantController(config: _cfg());
      addTearDown(controller.dispose);
      controller.applyMusicPush(_musicJson);
      // Should not throw despite no sink.
      controller.playPauseMusic();
      controller.setMusicVolume(50);
    });
  });

  group('AssistantController music voice control', () {
    test('a musicScreen voice event drives the screen via the engine stream', () async {
      final engine = StreamController<WakeWordEvent>.broadcast();
      final controller = AssistantController(
        config: _cfg(),
        startEngine: (_) => engine.stream,
      )..start();
      addTearDown(controller.dispose);
      controller.applyMusicPush(_musicJson);

      engine.add(_musicScreenEvent('now_playing'));
      await Future<void>.delayed(Duration.zero);
      expect(controller.state.musicScreen, MusicScreen.nowPlaying);

      engine.add(_musicScreenEvent('up_next'));
      await Future<void>.delayed(Duration.zero);
      expect(controller.state.musicScreen, MusicScreen.nextUp);

      engine.add(_musicScreenEvent('hidden'));
      await Future<void>.delayed(Duration.zero);
      expect(controller.state.musicScreen, MusicScreen.hidden);
    });

    test('voice show is a no-op when nothing is playing', () async {
      final engine = StreamController<WakeWordEvent>.broadcast();
      final controller = AssistantController(
        config: _cfg(),
        startEngine: (_) => engine.stream,
      )..start();
      addTearDown(controller.dispose);

      engine.add(_musicScreenEvent('now_playing'));
      await Future<void>.delayed(Duration.zero);
      expect(controller.state.musicScreen, MusicScreen.hidden);
    });

    test('opening a full screen pushes music display context; closing clears it', () {
      final ctx = <Map<String, Object?>>[];
      final controller = AssistantController(
        config: _cfg(),
        setMusicContext: ({
          required bool active,
          required bool playing,
          required String title,
          required String artist,
          required String screen,
        }) => ctx.add({
          'active': active,
          'playing': playing,
          'title': title,
          'screen': screen,
        }),
      );
      addTearDown(controller.dispose);
      controller.applyMusicPush(_musicJson); // playing, screen hidden → no context yet
      expect(ctx, isEmpty);

      controller.openNowPlaying();
      expect(ctx.last, {
        'active': true,
        'playing': true,
        'title': 'Paranoid Android',
        'screen': 'now_playing',
      });

      controller.showMusicQueue();
      expect(ctx.last['screen'], 'up_next');

      controller.closeMusicScreen();
      expect(ctx.last['active'], isFalse);
    });

    test('a Core screen (place) hides the music full screen', () async {
      final engine = StreamController<WakeWordEvent>.broadcast();
      final controller = AssistantController(
        config: _cfg(),
        startEngine: (_) => engine.stream,
      )..start();
      addTearDown(controller.dispose);
      controller.applyMusicPush(_musicJson);
      controller.openNowPlaying();
      expect(controller.state.musicScreen, MusicScreen.nowPlaying);

      engine.add(
        WakeWordEvent(
          kind: WakeWordEventKind.showPlace,
          message: '',
          device: '',
          deviceSampleRate: 0,
          channels: 0,
          rms: 0,
          score: 0,
          avgScore: 0,
          threshold: 0,
          gainDb: 0,
          model: '',
          transcript: '',
          reply: '',
          timerId: 0,
          timerLabel: '',
          timerRemainingSecs: 0,
          present: false,
          recipeJson: '',
          weatherJson: '',
          placeJson: '{"name":"Blue Bottle","address":"1 Main St"}',
          recipeAction: '',
          musicScreen: '',
        ),
      );
      await Future<void>.delayed(Duration.zero);
      expect(controller.state.placeActive, isTrue);
      // Music keeps playing, but its full screen yields to the Core-pushed place card.
      expect(controller.state.musicActive, isTrue);
      expect(controller.state.musicScreen, MusicScreen.hidden);
    });
  });
}
