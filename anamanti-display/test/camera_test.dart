// Live camera view (plans/CameraPlan.md): the controller folding show/dismiss camera
// events into AssistantState (stream fetch, display context, auto-close, mutual
// exclusion), and the CameraView widget's loading / ready / error states with an
// injected fake player (no platform video implementation in `flutter test`).

import 'dart:async';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/engine/camera_session.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:anamanti_display/src/rust/api/settings.dart' show CameraStream;
import 'package:anamanti_display/src/slideshow/photo_source.dart';
import 'package:anamanti_display/src/ui/ambient_screen.dart';
import 'package:anamanti_display/src/ui/camera_view.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:video_player/video_player.dart';

import 'support/wake_word_events.dart';

const _url = 'rtsp://anamanti:s3cret@192.168.1.29:554/h264Preview_05_sub';

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

WakeWordEvent _show(String name) =>
    wakeEvent(WakeWordEventKind.showCamera, recipeAction: name);

CameraStream _ok(String camera, [String url = _url]) =>
    CameraStream(ok: true, camera: camera, url: url, message: '');

CameraStream _refused(String message) =>
    CameraStream(ok: false, camera: '', url: '', message: message);

Future<void> _tick() => Future<void>.delayed(Duration.zero);

/// A controller wired to a synthetic engine stream plus recording sinks.
class _Rig {
  _Rig({
    CameraStreamFetcher? fetch,
    Duration cameraAutoClose = const Duration(seconds: 60),
  }) {
    controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      cameraAutoClose: cameraAutoClose,
      fetchCameraStream: fetch,
      setCameraContext: ({required bool active, required String name}) =>
          contexts.add((active: active, name: name)),
      setPlaceContext:
          ({
            required bool active,
            required String name,
            required String address,
          }) => contexts.add((active: active, name: 'place:$name')),
    )..start();
  }

  final engine = StreamController<WakeWordEvent>.broadcast();
  final contexts = <({bool active, String name})>[];
  late final AssistantController controller;

  AssistantState get state => controller.state;

  Future<void> add(WakeWordEvent e) async {
    engine.add(e);
    await _tick();
    await _tick();
  }

  void dispose() {
    controller.dispose();
    engine.close();
  }
}

/// A stand-in for the platform player: never touches a platform implementation.
class _FakePlayer extends VideoPlayerController {
  _FakePlayer(super.uri, {this.failInit = false})
    : super.networkUrl(viewType: VideoViewType.platformView);

  final bool failInit;
  final Completer<void> _init = Completer<void>();
  double? volume;
  bool played = false;
  bool disposed = false;

  @override
  Future<void> initialize() =>
      failInit ? Future<void>.error(Exception('boom')) : _init.future;

  /// Simulate the first frame: the player reports initialized.
  void becomeReady() {
    value = value.copyWith(isInitialized: true, size: const Size(640, 360));
    _init.complete();
  }

  void fail() {
    value = value.copyWith(errorDescription: 'source error');
  }

  @override
  Future<void> setVolume(double volume) async => this.volume = volume;

  @override
  Future<void> play() async => played = true;

  @override
  Future<void> dispose() async {
    disposed = true;
    await super.dispose();
  }
}

void main() {
  group('AssistantController camera', () {
    test(
      'show opens the view, sets the context, and fetches the stream',
      () async {
        final asked = <String>[];
        final rig = _Rig(
          fetch: (name) async {
            asked.add(name);
            return _ok('Front Door');
          },
        );
        addTearDown(rig.dispose);

        expect(rig.state.cameraActive, isFalse);
        await rig.add(_show('Front Door'));

        expect(rig.state.cameraActive, isTrue);
        expect(rig.state.camera!.name, 'Front Door');
        expect(asked, ['Front Door']);
        expect(rig.state.camera!.status, CameraStatus.ready);
        expect(rig.state.camera!.url, _url);
        expect(rig.contexts.last, (active: true, name: 'Front Door'));
        // A camera view keeps the screen awake.
        expect(rig.state.screenAwake, isTrue);
      },
    );

    test('voice dismiss closes the view and clears the context', () async {
      final rig = _Rig(fetch: (_) async => _ok('Front Door'));
      addTearDown(rig.dispose);

      await rig.add(_show('Front Door'));
      await rig.add(wakeEvent(WakeWordEventKind.dismissCamera));

      expect(rig.state.cameraActive, isFalse);
      expect(rig.contexts.last, (active: false, name: ''));
    });

    test('touch dismiss closes the view and clears the context', () async {
      final rig = _Rig(fetch: (_) async => _ok('Front Door'));
      addTearDown(rig.dispose);

      await rig.add(_show('Front Door'));
      rig.controller.dismissCamera();

      expect(rig.state.cameraActive, isFalse);
      expect(rig.contexts.last, (active: false, name: ''));
    });

    test('the view auto-closes after the timeout', () async {
      final rig = _Rig(
        fetch: (_) async => _ok('Front Door'),
        cameraAutoClose: const Duration(milliseconds: 40),
      );
      addTearDown(rig.dispose);

      await rig.add(_show('Front Door'));
      expect(rig.state.cameraActive, isTrue);

      await Future<void>.delayed(const Duration(milliseconds: 80));
      expect(rig.state.cameraActive, isFalse);
      expect(rig.contexts.last, (active: false, name: ''));
    });

    test('a new show restarts the auto-close timer', () async {
      final rig = _Rig(
        fetch: (_) async => _ok('Front Door'),
        cameraAutoClose: const Duration(milliseconds: 60),
      );
      addTearDown(rig.dispose);

      await rig.add(_show('Front Door'));
      await Future<void>.delayed(const Duration(milliseconds: 40));
      await rig.add(_show('Front Door'));
      await Future<void>.delayed(const Duration(milliseconds: 40));
      expect(rig.state.cameraActive, isTrue);
      await Future<void>.delayed(const Duration(milliseconds: 40));
      expect(rig.state.cameraActive, isFalse);
    });

    test('a refused fetch shows the error with the Core reason', () async {
      final rig = _Rig(
        fetch: (_) async => _refused("The camera isn't set up yet."),
      );
      addTearDown(rig.dispose);

      await rig.add(_show('Front Door'));

      expect(rig.state.cameraActive, isTrue);
      expect(rig.state.camera!.status, CameraStatus.error);
      expect(rig.state.camera!.error, "The camera isn't set up yet.");
      expect(rig.state.camera!.url, isNull);
    });

    test('a fetch that throws shows the error state', () async {
      final rig = _Rig(fetch: (_) async => throw Exception('no core'));
      addTearDown(rig.dispose);

      await rig.add(_show('Front Door'));

      expect(rig.state.camera!.status, CameraStatus.error);
      expect(rig.state.camera!.url, isNull);
    });

    test(
      'a newer show replaces the camera and drops the stale fetch',
      () async {
        final pending = <String, Completer<CameraStream>>{};
        final rig = _Rig(
          fetch: (name) => (pending[name] = Completer<CameraStream>()).future,
        );
        addTearDown(rig.dispose);

        await rig.add(_show('Front Door'));
        await rig.add(_show('Driveway'));
        expect(rig.state.camera!.name, 'Driveway');
        expect(rig.contexts.last, (active: true, name: 'Driveway'));

        // The first fetch finishing late must not overwrite the newer session.
        pending['Front Door']!.complete(_ok('Front Door', 'rtsp://old'));
        await _tick();
        expect(rig.state.camera!.name, 'Driveway');
        expect(rig.state.camera!.url, isNull);

        pending['Driveway']!.complete(_ok('Driveway'));
        await _tick();
        expect(rig.state.camera!.status, CameraStatus.ready);
        expect(rig.state.camera!.url, _url);
      },
    );

    test('a fetch finishing after close is ignored', () async {
      final pending = Completer<CameraStream>();
      final rig = _Rig(fetch: (_) => pending.future);
      addTearDown(rig.dispose);

      await rig.add(_show('Front Door'));
      rig.controller.dismissCamera();
      pending.complete(_ok('Front Door'));
      await _tick();
      expect(rig.state.cameraActive, isFalse);
    });

    test('camera and place are mutually exclusive', () async {
      final rig = _Rig(fetch: (_) async => _ok('Front Door'));
      addTearDown(rig.dispose);

      await rig.add(
        wakeEvent(
          WakeWordEventKind.showPlace,
          placeJson: '{"name":"Blue Bottle","address":"1 Main St"}',
        ),
      );
      expect(rig.state.placeActive, isTrue);

      await rig.add(_show('Front Door'));
      expect(rig.state.cameraActive, isTrue);
      expect(rig.state.placeActive, isFalse);
      // The single display-context slot ends on the camera (written last).
      expect(rig.contexts.last, (active: true, name: 'Front Door'));

      await rig.add(
        wakeEvent(
          WakeWordEventKind.showPlace,
          placeJson: '{"name":"Blue Bottle","address":"1 Main St"}',
        ),
      );
      expect(rig.state.placeActive, isTrue);
      expect(rig.state.cameraActive, isFalse);
      expect(rig.contexts.last, (active: true, name: 'place:Blue Bottle'));
    });

    test('the session never prints its URL', () {
      const s = CameraSession(name: 'Front Door', seq: 1, url: _url);
      expect(s.toString(), isNot(contains('s3cret')));
      expect(s.toString(), isNot(contains('rtsp://')));
    });
  });

  group('CameraView', () {
    testWidgets('loading: spinner + name until the URL arrives; ✕ closes', (
      tester,
    ) async {
      var closed = false;
      final made = <Uri>[];
      await tester.pumpWidget(
        MaterialApp(
          home: CameraView(
            session: const CameraSession(name: 'Front Door', seq: 1),
            onClose: () => closed = true,
            playerFactory: (uri) {
              made.add(uri);
              return _FakePlayer(uri);
            },
          ),
        ),
      );

      expect(find.text('Front Door'), findsOneWidget);
      expect(find.byKey(const Key('camera-loading')), findsOneWidget);
      expect(find.byKey(const Key('camera-error')), findsNothing);
      expect(made, isEmpty); // no URL yet → no player

      await tester.tap(find.byKey(const Key('camera-close')));
      expect(closed, isTrue);
    });

    testWidgets('ready: builds a muted player, plays, hides the spinner', (
      tester,
    ) async {
      late _FakePlayer player;
      Widget view(CameraSession s) => MaterialApp(
        home: CameraView(
          session: s,
          onClose: () {},
          playerFactory: (uri) => player = _FakePlayer(uri),
        ),
      );
      await tester.pumpWidget(
        view(const CameraSession(name: 'Front Door', seq: 1, url: _url)),
      );

      expect(player.dataSource, _url);
      expect(player.volume, 0);
      expect(find.byKey(const Key('camera-loading')), findsOneWidget);

      player.becomeReady();
      await tester.pump();
      expect(player.played, isTrue);
      expect(find.byKey(const Key('camera-loading')), findsNothing);
      expect(find.byKey(const Key('camera-error')), findsNothing);

      // Closing the view disposes the player.
      await tester.pumpWidget(const SizedBox.shrink());
      expect(player.disposed, isTrue);
    });

    testWidgets('fetch failure: error state with the Core reason', (
      tester,
    ) async {
      await tester.pumpWidget(
        MaterialApp(
          home: CameraView(
            session: const CameraSession(
              name: 'Front Door',
              seq: 1,
              error: "The camera isn't set up yet.",
            ),
            onClose: () {},
            playerFactory: _FakePlayer.new,
          ),
        ),
      );

      expect(find.text("Couldn't reach the camera"), findsOneWidget);
      expect(find.text("The camera isn't set up yet."), findsOneWidget);
      expect(find.byKey(const Key('camera-loading')), findsNothing);
      expect(find.byKey(const Key('camera-close')), findsOneWidget);
    });

    testWidgets('player error: error state', (tester) async {
      late _FakePlayer player;
      await tester.pumpWidget(
        MaterialApp(
          home: CameraView(
            session: const CameraSession(name: 'Front Door', seq: 1, url: _url),
            onClose: () {},
            playerFactory: (uri) => player = _FakePlayer(uri),
          ),
        ),
      );
      player.fail();
      await tester.pump();
      expect(find.text("Couldn't reach the camera"), findsOneWidget);
    });

    testWidgets('player that fails to start: error state', (tester) async {
      await tester.pumpWidget(
        MaterialApp(
          home: CameraView(
            session: const CameraSession(name: 'Front Door', seq: 1, url: _url),
            onClose: () {},
            playerFactory: (uri) => _FakePlayer(uri, failInit: true),
          ),
        ),
      );
      await tester.pump();
      expect(find.text("Couldn't reach the camera"), findsOneWidget);
    });
  });

  testWidgets('AmbientScreen shows the camera layer and its ✕ closes it', (
    tester,
  ) async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final slideshow = SlideshowController();
    final players = <_FakePlayer>[];
    final assistant = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      fetchCameraStream: (_) async => _ok('Front Door'),
    )..start();
    await slideshow.start();
    await tester.pumpWidget(
      MaterialApp(
        home: AmbientScreen(
          assistant: assistant,
          slideshow: slideshow,
          cameraPlayerFactory: (uri) {
            final p = _FakePlayer(uri);
            players.add(p);
            return p;
          },
        ),
      ),
    );

    engine.add(_show('Front Door'));
    await tester.pump();
    await tester.pump();
    await tester.pump();
    expect(find.byType(CameraView), findsOneWidget);
    expect(find.text('Front Door'), findsOneWidget);
    expect(players, hasLength(1));

    await tester.tap(find.byKey(const Key('camera-close')));
    await tester.pump();
    expect(assistant.state.cameraActive, isFalse);
    await tester.pump(const Duration(milliseconds: 300));
    expect(find.byType(CameraView), findsNothing);
    expect(players.single.disposed, isTrue);

    await tester.pumpWidget(const SizedBox.shrink());
    assistant.dispose();
    slideshow.dispose();
    await engine.close();
  });
}
