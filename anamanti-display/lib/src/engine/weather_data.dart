import 'dart:convert';

/// A weather report pushed from the orchestrator (`weather_lookup` tool, or the
/// periodic ambient push) and rendered on the display: the full-screen weather view
/// (a big conditions panel + a 10-hour hourly row) and the small icon + temperature
/// beside the idle clock.
///
/// Mirrors the orchestrator's `crate::weather::WeatherReport`: it arrives on the
/// engine stream as the `weatherJson` string of a `WakeWordEventKind.showWeather` /
/// `.weatherCurrent` event (or the weather channel's `WeatherPush`) and is decoded
/// here. All fields tolerate being absent (older/partial payloads).
class WeatherData {
  const WeatherData({
    required this.locationLabel,
    required this.units,
    required this.current,
    this.whenLabel = '',
    this.hourly = const [],
    this.daily = const [],
    this.layout = '',
  });

  final String locationLabel;

  /// `"imperial"` (°F) or `"metric"` (°C).
  final String units;

  /// A short label for the forecast day when it is **not** the current day (e.g.
  /// "Sat, Oct 3"); empty for a right-now/today forecast. When set, `current` is that
  /// day's summary and the view hides the live-only "Feels" figure.
  final String whenLabel;

  /// Current conditions, or — when [whenLabel] is set — the requested day's summary.
  final CurrentConditions current;

  /// The hourly forecast row (up to 10 entries).
  final List<WeatherHour> hourly;

  /// The daily forecast (up to 7 days starting today), rendered by the separate 7-day
  /// forecast widget when [isWeek] is true.
  final List<WeatherDay> daily;

  /// Presentation hint from the orchestrator: `"week"` selects the separate 7-day
  /// forecast widget; empty or `"hourly"` selects the default hourly view.
  final String layout;

  /// Whether this report is for a future day rather than right now.
  bool get isFutureDay => whenLabel.isNotEmpty;

  /// Whether the display should render the separate 7-day forecast widget.
  bool get isWeek => layout == 'week';

  /// The degree symbol suffix for this report's units ("°F" / "°C").
  String get unitSuffix => units == 'imperial' ? '°F' : '°C';

  /// Decode from the engine event's `weatherJson`. Returns `null` when the string is
  /// empty or malformed, so a bad payload never crashes the UI.
  static WeatherData? tryParse(String jsonStr) {
    if (jsonStr.trim().isEmpty) return null;
    try {
      final decoded = jsonDecode(jsonStr);
      if (decoded is! Map<String, dynamic>) return null;
      final currentMap = decoded['current'];
      return WeatherData(
        locationLabel: (decoded['location_label'] as String?) ?? '',
        units: (decoded['units'] as String?) ?? 'metric',
        whenLabel: (decoded['when_label'] as String?) ?? '',
        current: CurrentConditions._fromMap(
          currentMap is Map<String, dynamic> ? currentMap : const {},
        ),
        hourly: _hours(decoded['hourly']),
        daily: _days(decoded['daily']),
        layout: (decoded['layout'] as String?) ?? '',
      );
    } catch (_) {
      return null;
    }
  }

  static List<WeatherHour> _hours(Object? value) {
    if (value is! List) return const [];
    return value
        .whereType<Map<String, dynamic>>()
        .map(WeatherHour._fromMap)
        .toList(growable: false);
  }

  static List<WeatherDay> _days(Object? value) {
    if (value is! List) return const [];
    return value
        .whereType<Map<String, dynamic>>()
        .map(WeatherDay._fromMap)
        .toList(growable: false);
  }
}

/// Conditions right now, plus today's high/low.
class CurrentConditions {
  const CurrentConditions({
    this.temp = 0,
    this.feelsLike = 0,
    this.weatherCode = 0,
    this.isDay = true,
    this.high = 0,
    this.low = 0,
    this.description = '',
  });

  final int temp;
  final int feelsLike;
  final int weatherCode;
  final bool isDay;
  final int high;
  final int low;
  final String description;

  static CurrentConditions _fromMap(Map<String, dynamic> m) =>
      CurrentConditions(
        temp: _int(m['temp']),
        feelsLike: _int(m['feels_like']),
        weatherCode: _int(m['weather_code']),
        isDay: m['is_day'] as bool? ?? true,
        high: _int(m['high']),
        low: _int(m['low']),
        description: (m['description'] as String?) ?? '',
      );
}

/// One hour of the forecast (a cell in the hourly row).
class WeatherHour {
  const WeatherHour({
    this.time = '',
    this.weatherCode = 0,
    this.temp = 0,
    this.precipProb = 0,
    this.isDay = true,
  });

  /// Short clock label for the hour, e.g. "3 PM".
  final String time;
  final int weatherCode;
  final int temp;
  final int precipProb;
  final bool isDay;

  static WeatherHour _fromMap(Map<String, dynamic> m) => WeatherHour(
    time: (m['time'] as String?) ?? '',
    weatherCode: _int(m['weather_code']),
    temp: _int(m['temp']),
    precipProb: _int(m['precip_prob']),
    isDay: m['is_day'] as bool? ?? true,
  );
}

/// One day of the forecast (a column in the 7-day forecast widget).
class WeatherDay {
  const WeatherDay({
    this.date = '',
    this.weekday = '',
    this.weatherCode = 0,
    this.high = 0,
    this.low = 0,
    this.precipProb = 0,
  });

  /// ISO date, `YYYY-MM-DD`.
  final String date;

  /// Short weekday label for the column header, e.g. "Thu".
  final String weekday;
  final int weatherCode;
  final int high;
  final int low;
  final int precipProb;

  static WeatherDay _fromMap(Map<String, dynamic> m) => WeatherDay(
    date: (m['date'] as String?) ?? '',
    weekday: (m['weekday'] as String?) ?? '',
    weatherCode: _int(m['weather_code']),
    high: _int(m['high']),
    low: _int(m['low']),
    precipProb: _int(m['precip_prob']),
  );
}

/// Tolerantly read an int from a JSON number (handles ints and doubles).
int _int(Object? v) {
  if (v is int) return v;
  if (v is double) return v.round();
  if (v is num) return v.round();
  return 0;
}
