// Place mode: the PlaceData parser, the controller folding show/dismiss place events
// into AssistantState, and the PlaceView widget.

import 'dart:async';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/engine/place_data.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:anamanti_display/src/ui/place_view.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

// A place payload with no photo, so the widget test never issues a network fetch.
const _placeJson = '''
{"name":"Blue Bottle Coffee","address":"1 Main St, Austin, TX 78701",
 "category":"Coffee shop","open_now":true,
 "hours":["Monday: 7:00 AM – 6:00 PM","Tuesday: 7:00 AM – 6:00 PM"],
 "rating":"4.6","rating_count":1234,"price_level":"Moderate",
 "phone":"(512) 555-0100","website":"https://bluebottlecoffee.com/",
 "maps_uri":"https://maps.google.com/?cid=1","photo_uri":""}
''';

const _weatherJson =
    '{"location_label":"Austin","units":"imperial","current":{"temp":72}}';

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

WakeWordEvent _ev(
  WakeWordEventKind kind, {
  String placeJson = '',
  String weatherJson = '',
}) => WakeWordEvent(
  kind: kind,
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
  weatherJson: weatherJson,
  placeJson: placeJson,
  recipeAction: '',
);

void main() {
  group('PlaceData.tryParse', () {
    test('parses a full place payload', () {
      final p = PlaceData.tryParse(_placeJson)!;
      expect(p.name, 'Blue Bottle Coffee');
      expect(p.address, '1 Main St, Austin, TX 78701');
      expect(p.category, 'Coffee shop');
      expect(p.openNow, isTrue);
      expect(p.hours, hasLength(2));
      expect(p.rating, '4.6');
      expect(p.ratingCount, 1234);
      expect(p.priceLevel, 'Moderate');
      expect(p.phone, '(512) 555-0100');
      expect(p.website, 'https://bluebottlecoffee.com/');
      expect(p.hasPhoto, isFalse);
    });

    test('rejects empty, malformed, and nameless payloads', () {
      expect(PlaceData.tryParse(''), isNull);
      expect(PlaceData.tryParse('not json'), isNull);
      expect(PlaceData.tryParse('[1,2,3]'), isNull);
      expect(PlaceData.tryParse('{"address":"x"}'), isNull); // no name
    });

    test('tolerates missing optional fields', () {
      final p = PlaceData.tryParse('{"name":"X"}')!;
      expect(p.name, 'X');
      expect(p.openNow, isNull);
      expect(p.hours, isEmpty);
      expect(p.rating, '');
      expect(p.ratingCount, 0);
      expect(p.hasPhoto, isFalse);
    });
  });

  test('controller opens and closes the place card', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
    )..start();
    addTearDown(controller.dispose);

    expect(controller.state.placeActive, isFalse);

    engine.add(_ev(WakeWordEventKind.showPlace, placeJson: _placeJson));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.placeActive, isTrue);
    expect(controller.state.place!.name, 'Blue Bottle Coffee');

    engine.add(_ev(WakeWordEventKind.dismissPlace));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.placeActive, isFalse);

    // Re-open, then dismiss via the UI close method.
    engine.add(_ev(WakeWordEventKind.showPlace, placeJson: _placeJson));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.placeActive, isTrue);
    controller.dismissPlace();
    expect(controller.state.placeActive, isFalse);
  });

  test('a full-screen widget unloads the previously-loaded one', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
    )..start();
    addTearDown(controller.dispose);

    // Weather up first, then a place must unload it (and vice versa).
    engine.add(_ev(WakeWordEventKind.showWeather, weatherJson: _weatherJson));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.weatherActive, isTrue);

    engine.add(_ev(WakeWordEventKind.showPlace, placeJson: _placeJson));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.placeActive, isTrue);
    expect(controller.state.weatherActive, isFalse);

    engine.add(_ev(WakeWordEventKind.showWeather, weatherJson: _weatherJson));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.weatherActive, isTrue);
    expect(controller.state.placeActive, isFalse);
  });

  test('place card auto-closes after the timeout', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      placeAutoClose: const Duration(milliseconds: 40),
    )..start();
    addTearDown(controller.dispose);

    engine.add(_ev(WakeWordEventKind.showPlace, placeJson: _placeJson));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.placeActive, isTrue);

    // After the timeout the card closes on its own.
    await Future<void>.delayed(const Duration(milliseconds: 80));
    expect(controller.state.placeActive, isFalse);
  });

  test('a new place resets the auto-close timer', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      placeAutoClose: const Duration(milliseconds: 60),
    )..start();
    addTearDown(controller.dispose);

    engine.add(_ev(WakeWordEventKind.showPlace, placeJson: _placeJson));
    await Future<void>.delayed(const Duration(milliseconds: 40));
    engine.add(_ev(WakeWordEventKind.showPlace, placeJson: _placeJson));
    await Future<void>.delayed(const Duration(milliseconds: 40));
    // 80ms since the first show, only 40ms since the reset → still up.
    expect(controller.state.placeActive, isTrue);
    await Future<void>.delayed(const Duration(milliseconds: 40));
    expect(controller.state.placeActive, isFalse);
  });

  test('pushes place display-context on show and clears it on dismiss', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final pushes = <Map<String, Object?>>[];
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      setPlaceContext: ({
        required bool active,
        required String name,
        required String address,
      }) => pushes.add({'active': active, 'name': name, 'address': address}),
    )..start();
    addTearDown(controller.dispose);

    engine.add(_ev(WakeWordEventKind.showPlace, placeJson: _placeJson));
    await Future<void>.delayed(Duration.zero);
    expect(pushes.last['active'], isTrue);
    expect(pushes.last['name'], 'Blue Bottle Coffee');

    controller.dismissPlace();
    expect(pushes.last['active'], isFalse);
  });

  test('controller ignores an unparseable place payload', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
    )..start();
    addTearDown(controller.dispose);

    engine.add(_ev(WakeWordEventKind.showPlace, placeJson: 'garbage'));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.placeActive, isFalse);
  });

  testWidgets('PlaceView renders details and closes', (tester) async {
    final place = PlaceData.tryParse(_placeJson)!;
    var closed = false;

    await tester.pumpWidget(
      MaterialApp(home: PlaceView(place: place, onClose: () => closed = true)),
    );

    expect(find.text('Blue Bottle Coffee'), findsOneWidget);
    expect(find.text('1 Main St, Austin, TX 78701'), findsOneWidget);
    expect(find.text('Open now'), findsOneWidget);
    expect(find.text('(512) 555-0100'), findsOneWidget);
    expect(find.text('Monday: 7:00 AM – 6:00 PM'), findsOneWidget);

    await tester.tap(find.byKey(const Key('place-close')));
    expect(closed, isTrue);
  });
}
