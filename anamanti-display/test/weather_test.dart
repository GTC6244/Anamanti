// Weather mode: the WeatherData parser, the WMO→icon mapper, the controller folding
// show/current/dismiss weather events into AssistantState, and the WeatherView widget.

import 'dart:async';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/engine/weather_data.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:anamanti_display/src/ui/seven_day_view.dart';
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

// A "week" layout forecast: layout=="week" + a 7-day `daily` array of highs/lows.
const _weekWeatherJson = '''
{"location_label":"Austin, Texas","units":"imperial","layout":"week",
 "current":{"temp":72,"feels_like":70,"weather_code":2,"is_day":true,
            "high":80,"low":60,"description":"partly cloudy"},
 "daily":[
   {"date":"2026-09-29","weekday":"Mon","weather_code":2,"high":80,"low":60,"precip_prob":10},
   {"date":"2026-09-30","weekday":"Tue","weather_code":61,"high":78,"low":58,"precip_prob":70},
   {"date":"2026-10-01","weekday":"Wed","weather_code":3,"high":75,"low":57,"precip_prob":0},
   {"date":"2026-10-02","weekday":"Thu","weather_code":0,"high":82,"low":61,"precip_prob":0},
   {"date":"2026-10-03","weekday":"Fri","weather_code":80,"high":79,"low":59,"precip_prob":40},
   {"date":"2026-10-04","weekday":"Sat","weather_code":2,"high":81,"low":62,"precip_prob":5},
   {"date":"2026-10-05","weekday":"Sun","weather_code":95,"high":77,"low":60,"precip_prob":85}
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
  recipeJson: recipeJson,
  weatherJson: weatherJson,
  placeJson: '',
  recipeAction: '',
  musicScreen: '',
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

    test('defaults to the hourly layout with an empty daily list', () {
      final w = WeatherData.tryParse(_weatherJson)!;
      expect(w.layout, isEmpty);
      expect(w.isWeek, isFalse);
      expect(w.daily, isEmpty);
    });

    test('parses a week payload with a 7-day daily forecast', () {
      final w = WeatherData.tryParse(_weekWeatherJson)!;
      expect(w.layout, 'week');
      expect(w.isWeek, isTrue);
      expect(w.daily, hasLength(7));
      expect(w.daily.first.weekday, 'Mon');
      expect(w.daily.first.high, 80);
      expect(w.daily.first.low, 60);
      expect(w.daily[1].precipProb, 70);
      expect(w.daily.last.weekday, 'Sun');
      expect(w.daily.last.weatherCode, 95);
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

  testWidgets('SevenDayView renders 7 day columns with highs/lows and closes', (
    tester,
  ) async {
    final weather = WeatherData.tryParse(_weekWeatherJson)!;
    var closed = false;

    await tester.pumpWidget(
      MaterialApp(
        home: SevenDayView(weather: weather, onClose: () => closed = true),
      ),
    );

    // Header names the 7-day forecast and the location.
    expect(find.text('7-Day Forecast · Austin, Texas'), findsOneWidget);
    // The first column is labelled "Today"; the rest use their weekday.
    expect(find.text('Today'), findsOneWidget);
    expect(find.text('Tue'), findsOneWidget);
    expect(find.text('Sun'), findsOneWidget);
    // Highs carry the unit suffix; lows carry a bare degree sign.
    expect(find.text('80°F'), findsOneWidget);
    expect(find.text('58°'), findsOneWidget);
    // Precip chance shows only for days with a non-zero probability.
    expect(find.text('70%'), findsOneWidget);
    expect(find.text('85%'), findsOneWidget);

    await tester.tap(find.byKey(const Key('weather-week-close')));
    expect(closed, isTrue);
  });

  testWidgets('SevenDayView shows an empty state when there are no days', (
    tester,
  ) async {
    const weather = WeatherData(
      locationLabel: 'Austin, Texas',
      units: 'imperial',
      current: CurrentConditions(),
      layout: 'week',
    );

    await tester.pumpWidget(
      MaterialApp(
        home: SevenDayView(weather: weather, onClose: () {}),
      ),
    );

    expect(find.text('No forecast available'), findsOneWidget);
  });
}
