// Weather mode: the WeatherData parser, the WMO→icon mapper, the controller folding
// show/current/dismiss weather events into AssistantState, and the WeatherView widget.

import 'dart:async';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/engine/weather_data.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:anamanti_display/src/ui/weather_icons.dart';
import 'package:anamanti_display/src/ui/weather_view.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

const _weatherJson = '''
{"location_label":"Austin, Texas","units":"imperial",
 "current":{"temp":72,"feels_like":70,"weather_code":2,"is_day":true,
            "high":80,"low":60,"description":"partly cloudy"},
 "hourly":[
   {"time":"12 PM","weather_code":2,"temp":72,"precip_prob":10,"is_day":true},
   {"time":"1 PM","weather_code":61,"temp":73,"precip_prob":80,"is_day":true}
 ]}
''';

// A future-day forecast: a day label + the day's summary in `current`.
const _futureWeatherJson = '''
{"location_label":"Austin, Texas","units":"imperial","when_label":"Sat, Sep 26",
 "current":{"temp":75,"feels_like":75,"weather_code":61,"is_day":true,
            "high":75,"low":58,"description":"rain"},
 "hourly":[
   {"time":"8 AM","weather_code":2,"temp":62,"precip_prob":15,"is_day":true},
   {"time":"9 AM","weather_code":3,"temp":64,"precip_prob":15,"is_day":true}
 ]}
''';

const _recipeJson =
    '{"title":"Spaghetti","ingredients":["pasta"],"steps":["boil","serve"]}';

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
  String weatherJson = '',
  String recipeJson = '',
}) => WakeWordEvent(
  kind: kind,
  message: '',
  device: '',
  deviceSampleRate: 0,
  channels: 0,
  rms: 0,
  score: 0,
  model: '',
  transcript: '',
  reply: '',
  timerId: 0,
  timerLabel: '',
  timerRemainingSecs: 0,
  present: false,
  recipeJson: recipeJson,
  weatherJson: weatherJson,
  placeJson: '',
  recipeAction: '',
);

void main() {
  group('WeatherData.tryParse', () {
    test('parses a full report payload', () {
      final w = WeatherData.tryParse(_weatherJson)!;
      expect(w.locationLabel, 'Austin, Texas');
      expect(w.units, 'imperial');
      expect(w.unitSuffix, '°F');
      expect(w.current.temp, 72);
      expect(w.current.weatherCode, 2);
      expect(w.current.isDay, isTrue);
      expect(w.current.high, 80);
      expect(w.current.description, 'partly cloudy');
      expect(w.isFutureDay, isFalse);
      expect(w.whenLabel, isEmpty);
      expect(w.hourly, hasLength(2));
      expect(w.hourly[0].time, '12 PM');
      expect(w.hourly[1].temp, 73);
      expect(w.hourly[1].precipProb, 80);
    });

    test('parses a future-day payload with a day label and summary', () {
      final w = WeatherData.tryParse(_futureWeatherJson)!;
      expect(w.isFutureDay, isTrue);
      expect(w.whenLabel, 'Sat, Sep 26');
      expect(w.current.high, 75);
      expect(w.current.description, 'rain');
      expect(w.hourly.first.time, '8 AM');
    });

    test('rejects empty and malformed payloads', () {
      expect(WeatherData.tryParse(''), isNull);
      expect(WeatherData.tryParse('not json'), isNull);
      expect(WeatherData.tryParse('[1,2,3]'), isNull);
    });

    test('tolerates missing current/hourly', () {
      final w = WeatherData.tryParse(
        '{"location_label":"X","units":"metric"}',
      )!;
      expect(w.unitSuffix, '°C');
      expect(w.current.temp, 0);
      expect(w.hourly, isEmpty);
    });
  });

  group('weatherIcon mapping', () {
    test('day vs night for a clear sky', () {
      expect(weatherIcon(0, isDay: true), Icons.wb_sunny_rounded);
      expect(weatherIcon(0, isDay: false), Icons.nightlight_round);
    });
    test('rain, snow, and thunderstorm groups', () {
      expect(weatherIcon(63, isDay: true), Icons.water_drop_rounded);
      expect(weatherIcon(75, isDay: true), Icons.ac_unit_rounded);
      expect(weatherIcon(95, isDay: true), Icons.flash_on_rounded);
      expect(weatherIcon(9999, isDay: true), Icons.cloud_rounded);
    });
  });

  test('controller opens, refreshes ambient, and closes weather', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
    )..start();
    addTearDown(controller.dispose);

    expect(controller.state.weatherActive, isFalse);
    expect(controller.state.weatherCurrent, isNull);

    // show opens the full screen AND seeds the ambient indicator.
    engine.add(_ev(WakeWordEventKind.showWeather, weatherJson: _weatherJson));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.weatherActive, isTrue);
    expect(controller.state.weather!.current.temp, 72);
    expect(controller.state.weatherCurrent!.current.temp, 72);

    // dismiss closes the full screen but keeps the ambient indicator.
    engine.add(_ev(WakeWordEventKind.dismissWeather));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.weatherActive, isFalse);
    expect(controller.state.weatherCurrent, isNotNull);

    // a current push updates the ambient indicator only (never opens the screen).
    controller.applyWeatherPush(_weatherJson);
    expect(controller.state.weatherActive, isFalse);
    expect(
      controller.state.weatherCurrent!.current.description,
      'partly cloudy',
    );

    // re-open then dismiss via the UI close method.
    engine.add(_ev(WakeWordEventKind.showWeather, weatherJson: _weatherJson));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.weatherActive, isTrue);
    controller.dismissWeather();
    expect(controller.state.weatherActive, isFalse);
  });

  test(
    'full-screen weather auto-closes after the timeout (chip stays)',
    () async {
      final engine = StreamController<WakeWordEvent>.broadcast();
      final controller = AssistantController(
        config: _cfg(),
        startEngine: (_) => engine.stream,
        weatherAutoClose: const Duration(milliseconds: 40),
      )..start();
      addTearDown(controller.dispose);

      engine.add(_ev(WakeWordEventKind.showWeather, weatherJson: _weatherJson));
      await Future<void>.delayed(Duration.zero);
      expect(controller.state.weatherActive, isTrue);

      // After the timeout the full screen closes on its own, but the ambient
      // indicator (the clock chip) is left in place.
      await Future<void>.delayed(const Duration(milliseconds: 80));
      expect(controller.state.weatherActive, isFalse);
      expect(controller.state.weatherCurrent, isNotNull);
    },
  );

  test('a new forecast resets the auto-close timer', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
      weatherAutoClose: const Duration(milliseconds: 60),
    )..start();
    addTearDown(controller.dispose);

    engine.add(_ev(WakeWordEventKind.showWeather, weatherJson: _weatherJson));
    await Future<void>.delayed(const Duration(milliseconds: 40));
    // Re-show before the first timer would fire; it should restart the 60ms clock.
    engine.add(_ev(WakeWordEventKind.showWeather, weatherJson: _weatherJson));
    await Future<void>.delayed(const Duration(milliseconds: 40));
    // 80ms elapsed since the first show, but only 40ms since the reset → still up.
    expect(controller.state.weatherActive, isTrue);
    await Future<void>.delayed(const Duration(milliseconds: 40));
    expect(controller.state.weatherActive, isFalse);
  });

  test(
    'pushes weather display-context on show and clears it on dismiss',
    () async {
      final engine = StreamController<WakeWordEvent>.broadcast();
      final pushes = <Map<String, Object?>>[];
      final controller = AssistantController(
        config: _cfg(),
        startEngine: (_) => engine.stream,
        setWeatherContext:
            ({
              required bool active,
              required String location,
              required String units,
              required int temp,
              required String description,
            }) => pushes.add({
              'active': active,
              'location': location,
              'units': units,
              'temp': temp,
              'description': description,
            }),
      )..start();
      addTearDown(controller.dispose);

      engine.add(_ev(WakeWordEventKind.showWeather, weatherJson: _weatherJson));
      await Future<void>.delayed(Duration.zero);
      expect(pushes.last['active'], isTrue);
      expect(pushes.last['location'], 'Austin, Texas');
      expect(pushes.last['units'], 'imperial');
      expect(pushes.last['temp'], 72);
      expect(pushes.last['description'], 'partly cloudy');

      controller.dismissWeather();
      expect(pushes.last['active'], isFalse);
    },
  );

  test('a full-screen widget unloads the previously-loaded one', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
    )..start();
    addTearDown(controller.dispose);

    // Weather up first.
    engine.add(_ev(WakeWordEventKind.showWeather, weatherJson: _weatherJson));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.weatherActive, isTrue);

    // Asking for a recipe must unload the weather screen (not stack behind it),
    // while the ambient clock chip (weatherCurrent) is preserved.
    engine.add(_ev(WakeWordEventKind.showRecipe, recipeJson: _recipeJson));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.recipeActive, isTrue);
    expect(controller.state.weatherActive, isFalse);
    expect(controller.state.weatherCurrent, isNotNull);

    // And the reverse: showing weather unloads the recipe.
    engine.add(_ev(WakeWordEventKind.showWeather, weatherJson: _weatherJson));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.weatherActive, isTrue);
    expect(controller.state.recipeActive, isFalse);
  });

  test('controller ignores an unparseable weather payload', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
    )..start();
    addTearDown(controller.dispose);

    engine.add(_ev(WakeWordEventKind.showWeather, weatherJson: 'garbage'));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.weatherActive, isFalse);
  });

  testWidgets('WeatherView renders today panel + hourly row and closes', (
    tester,
  ) async {
    final weather = WeatherData.tryParse(_weatherJson)!;
    var closed = false;

    await tester.pumpWidget(
      MaterialApp(
        home: WeatherView(weather: weather, onClose: () => closed = true),
      ),
    );

    expect(find.text('Austin, Texas'), findsOneWidget);
    expect(find.text('72°F'), findsOneWidget);
    expect(find.text('Partly cloudy'), findsOneWidget);
    // A right-now forecast shows the live "Feels" figure.
    expect(find.textContaining('Feels'), findsOneWidget);
    // The hourly row shows each hour's clock label.
    expect(find.text('12 PM'), findsOneWidget);
    expect(find.text('1 PM'), findsOneWidget);

    await tester.tap(find.byKey(const Key('weather-close')));
    expect(closed, isTrue);
  });

  testWidgets(
    'WeatherView shows the day label and hides Feels for a future day',
    (tester) async {
      final weather = WeatherData.tryParse(_futureWeatherJson)!;

      await tester.pumpWidget(
        MaterialApp(
          home: WeatherView(weather: weather, onClose: () {}),
        ),
      );

      // The header appends the day; the panel drops the live-only "Feels" figure.
      expect(find.text('Austin, Texas · Sat, Sep 26'), findsOneWidget);
      expect(find.textContaining('Feels'), findsNothing);
      expect(find.text('8 AM'), findsOneWidget);
    },
  );
}
