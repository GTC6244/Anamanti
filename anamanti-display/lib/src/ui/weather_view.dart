// Full-screen weather screen for the 8-inch Echo Show.
//
// Shown when the orchestrator's `weather_lookup` tool pushes a forecast (see
// [AssistantState.weather]). A big conditions panel — large condition icon,
// temperature, high/low, description, location — fills the screen, with a 10-hour
// hourly forecast row along the bottom (per-hour icon, clock label, temperature,
// precip chance). For a future-day request the panel shows that day's summary and its
// label sits beside the location. Landscape-first with large, arm's-length type. Left
// by voice or the close control.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/weather_data.dart';
import 'package:anamanti_display/src/ui/weather_icons.dart';

class WeatherView extends StatelessWidget {
  const WeatherView({super.key, required this.weather, required this.onClose});

  final WeatherData weather;
  final VoidCallback onClose;

  static const _bgTop = Color(0xFF0B1220);
  static const _bgBottom = Color(0xFF13233B);

  @override
  Widget build(BuildContext context) {
    final c = weather.current;
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
              Expanded(child: _today(c)),
              _hourlyRow(),
            ],
          ),
        ),
      ),
    );
  }

  /// The header title: the location, with the forecast day appended for a future-day
  /// request (e.g. "Austin, Texas · Sat, Oct 3").
  String _headerLabel() {
    final loc = weather.locationLabel.isEmpty
        ? 'Weather'
        : weather.locationLabel;
    return weather.isFutureDay ? '$loc · ${weather.whenLabel}' : loc;
  }

  Widget _header() {
    return Padding(
      padding: const EdgeInsets.fromLTRB(28, 16, 12, 0),
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
            key: const Key('weather-close'),
            tooltip: 'Close weather',
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

  /// The big "today" panel: a large condition icon beside the current temperature,
  /// with description and high/low beneath.
  Widget _today(CurrentConditions c) {
    final suffix = weather.unitSuffix;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 32),
      child: Row(
        mainAxisAlignment: MainAxisAlignment.center,
        children: [
          Icon(
            weatherIcon(c.weatherCode, isDay: c.isDay),
            key: const Key('weather-today-icon'),
            size: 150,
            color: weatherIconColor(c.weatherCode, isDay: c.isDay),
          ),
          const SizedBox(width: 36),
          Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(
                '${c.temp}$suffix',
                style: const TextStyle(
                  color: Colors.white,
                  fontSize: 108,
                  fontWeight: FontWeight.w300,
                  height: 1.0,
                ),
              ),
              if (c.description.isNotEmpty)
                Padding(
                  padding: const EdgeInsets.only(top: 4),
                  child: Text(
                    _titleCase(c.description),
                    style: TextStyle(
                      color: Colors.white.withValues(alpha: 0.9),
                      fontSize: 28,
                      fontWeight: FontWeight.w500,
                    ),
                  ),
                ),
              Padding(
                padding: const EdgeInsets.only(top: 10),
                child: Text(
                  // "Feels like" is a live-now figure; omit it for a future day.
                  weather.isFutureDay
                      ? 'H ${c.high}°   L ${c.low}°'
                      : 'H ${c.high}°   L ${c.low}°   Feels ${c.feelsLike}°',
                  style: TextStyle(
                    color: Colors.white.withValues(alpha: 0.65),
                    fontSize: 20,
                    fontWeight: FontWeight.w500,
                  ),
                ),
              ),
            ],
          ),
        ],
      ),
    );
  }

  /// The 10-hour hourly forecast row along the bottom.
  Widget _hourlyRow() {
    final hours = weather.hourly;
    if (hours.isEmpty) return const SizedBox.shrink();
    return Container(
      decoration: BoxDecoration(
        color: Colors.black.withValues(alpha: 0.28),
        border: Border(
          top: BorderSide(color: Colors.white.withValues(alpha: 0.08)),
        ),
      ),
      padding: const EdgeInsets.symmetric(vertical: 12, horizontal: 8),
      child: Row(
        children: hours
            .map((h) => Expanded(child: _HourCell(hour: h)))
            .toList(growable: false),
      ),
    );
  }

  static String _titleCase(String s) =>
      s.isEmpty ? s : s[0].toUpperCase() + s.substring(1);
}

class _HourCell extends StatelessWidget {
  const _HourCell({required this.hour});

  final WeatherHour hour;

  @override
  Widget build(BuildContext context) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        Text(
          hour.time.isEmpty ? '—' : hour.time,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: TextStyle(
            color: Colors.white.withValues(alpha: 0.85),
            fontSize: 15,
            fontWeight: FontWeight.w600,
          ),
        ),
        const SizedBox(height: 6),
        Icon(
          weatherIcon(hour.weatherCode, isDay: hour.isDay),
          size: 34,
          color: weatherIconColor(hour.weatherCode, isDay: hour.isDay),
        ),
        const SizedBox(height: 6),
        Text(
          '${hour.temp}°',
          style: const TextStyle(
            color: Colors.white,
            fontSize: 18,
            fontWeight: FontWeight.w600,
          ),
        ),
        if (hour.precipProb > 0)
          Padding(
            padding: const EdgeInsets.only(top: 2),
            child: Text(
              '${hour.precipProb}%',
              style: const TextStyle(
                color: Color(0xFF7CC4FA),
                fontSize: 13,
                fontWeight: FontWeight.w500,
              ),
            ),
          ),
      ],
    );
  }
}
