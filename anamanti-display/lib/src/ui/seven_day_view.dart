// Full-screen 7-day forecast screen for the 8-inch Echo Show.
//
// A sibling of [WeatherView], shown when the orchestrator's `weather_lookup` tool
// pushes a forecast with `layout == "week"` (see [WeatherData.isWeek]). Instead of
// today's big conditions panel, the screen is divided into seven equal vertical
// columns — one per day — each showing the weekday, a condition icon, the day's high
// and low, and precipitation chance. Landscape-first with large, arm's-length type.
// Left by voice or the close control.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/weather_data.dart';
import 'package:anamanti_display/src/ui/weather_icons.dart';

class SevenDayView extends StatelessWidget {
  const SevenDayView({super.key, required this.weather, required this.onClose});

  final WeatherData weather;
  final VoidCallback onClose;

  static const _bgTop = Color(0xFF0B1220);
  static const _bgBottom = Color(0xFF13233B);

  @override
  Widget build(BuildContext context) {
    // Cap at seven columns so the row always divides the screen evenly.
    final days = weather.daily.take(7).toList(growable: false);
    return Material(
      child: Container(
        decoration: const BoxDecoration(
          gradient: LinearGradient(
            begin: Alignment.topCenter,
            end: Alignment.bottomCenter,
            colors: [_bgTop, _bgBottom],
          ),
        ),
        child: SafeArea(
          child: Column(
            children: [
              _header(),
              Expanded(
                child: days.isEmpty
                    ? _empty()
                    : Padding(
                        padding: const EdgeInsets.fromLTRB(12, 4, 12, 16),
                        child: Row(
                          children: [
                            for (var i = 0; i < days.length; i++)
                              Expanded(
                                child: _DayColumn(
                                  day: days[i],
                                  unitSuffix: weather.unitSuffix,
                                  isToday: i == 0,
                                ),
                              ),
                          ],
                        ),
                      ),
              ),
            ],
          ),
        ),
      ),
    );
  }

  /// The header title: the location, prefixed with "7-Day Forecast".
  String _headerLabel() {
    final loc = weather.locationLabel.isEmpty
        ? 'Weather'
        : weather.locationLabel;
    return '7-Day Forecast · $loc';
  }

  Widget _header() {
    return Padding(
      padding: const EdgeInsets.fromLTRB(28, 16, 12, 4),
      child: Row(
        children: [
          Expanded(
            child: Text(
              _headerLabel(),
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: const TextStyle(
                color: Colors.white,
                fontSize: 26,
                fontWeight: FontWeight.w600,
                letterSpacing: 0.2,
              ),
            ),
          ),
          IconButton(
            key: const Key('weather-week-close'),
            tooltip: 'Close forecast',
            onPressed: onClose,
            iconSize: 32,
            icon: Icon(
              Icons.close,
              color: Colors.white.withValues(alpha: 0.85),
            ),
          ),
        ],
      ),
    );
  }

  Widget _empty() => Center(
    child: Text(
      'No forecast available',
      style: TextStyle(
        color: Colors.white.withValues(alpha: 0.7),
        fontSize: 22,
        fontWeight: FontWeight.w500,
      ),
    ),
  );
}

/// One day's vertical column: weekday header, condition icon, high/low, precip chance.
class _DayColumn extends StatelessWidget {
  const _DayColumn({
    required this.day,
    required this.unitSuffix,
    required this.isToday,
  });

  final WeatherDay day;
  final String unitSuffix;
  final bool isToday;

  @override
  Widget build(BuildContext context) {
    return Container(
      margin: const EdgeInsets.symmetric(horizontal: 4),
      decoration: BoxDecoration(
        color: Colors.white.withValues(alpha: isToday ? 0.10 : 0.05),
        borderRadius: BorderRadius.circular(16),
        border: Border.all(
          color: Colors.white.withValues(alpha: isToday ? 0.22 : 0.08),
        ),
      ),
      padding: const EdgeInsets.symmetric(vertical: 16, horizontal: 4),
      child: Column(
        mainAxisAlignment: MainAxisAlignment.spaceEvenly,
        children: [
          Text(
            isToday ? 'Today' : (day.weekday.isEmpty ? '—' : day.weekday),
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: TextStyle(
              color: Colors.white.withValues(alpha: 0.9),
              fontSize: 20,
              fontWeight: FontWeight.w700,
              letterSpacing: 0.2,
            ),
          ),
          // Icon (day variant — a daily summary has no day/night distinction).
          Icon(
            weatherIcon(day.weatherCode, isDay: true),
            size: 44,
            color: weatherIconColor(day.weatherCode, isDay: true),
          ),
          // Scale-to-fit the high/low: with the global font scale raised, a 3-digit
          // temperature could otherwise wrap in this narrow column. `scaleDown` only
          // shrinks when it would overflow, so the default (1.0×) size is unchanged.
          FittedBox(
            fit: BoxFit.scaleDown,
            child: Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                Text(
                  '${day.high}$unitSuffix',
                  style: const TextStyle(
                    color: Colors.white,
                    fontSize: 30,
                    fontWeight: FontWeight.w700,
                    height: 1.0,
                  ),
                ),
                const SizedBox(height: 4),
                Text(
                  '${day.low}°',
                  style: TextStyle(
                    color: Colors.white.withValues(alpha: 0.6),
                    fontSize: 22,
                    fontWeight: FontWeight.w500,
                    height: 1.0,
                  ),
                ),
              ],
            ),
          ),
          // Precip chance, or a spacer so the columns stay vertically aligned.
          if (day.precipProb > 0)
            Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                const Icon(
                  Icons.water_drop,
                  size: 14,
                  color: Color(0xFF7CC4FA),
                ),
                const SizedBox(width: 3),
                Text(
                  '${day.precipProb}%',
                  style: const TextStyle(
                    color: Color(0xFF7CC4FA),
                    fontSize: 15,
                    fontWeight: FontWeight.w600,
                  ),
                ),
              ],
            )
          else
            const SizedBox(height: 15),
        ],
      ),
    );
  }
}
