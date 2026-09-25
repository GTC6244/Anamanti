import 'package:flutter/material.dart';

/// Maps a WMO weather code (from Open-Meteo, via the orchestrator) to a bundled
/// Material icon and a tint. Material Icons ship with Flutter, so this is an offline,
/// scalable "bundled icon set" — no raster assets to load, which suits the device's
/// ~1 GB memory budget. The same mapper drives both the big today panel and the small
/// icon beside the clock (size/color are chosen by the caller).
///
/// WMO code groups: 0–3 clear→overcast, 45/48 fog, 51–57 drizzle, 61–67 rain,
/// 71–77 snow, 80–82 rain showers, 85/86 snow showers, 95–99 thunderstorm.
IconData weatherIcon(int code, {required bool isDay}) {
  switch (code) {
    case 0:
    case 1:
      return isDay ? Icons.wb_sunny_rounded : Icons.nightlight_round;
    case 2:
      return isDay ? Icons.wb_cloudy_rounded : Icons.nights_stay_rounded;
    case 3:
      return Icons.cloud_rounded;
    case 45:
    case 48:
      return Icons.blur_on_rounded; // fog
    case 51:
    case 53:
    case 55:
    case 56:
    case 57:
      return Icons.grain_rounded; // drizzle
    case 61:
    case 63:
    case 65:
    case 66:
    case 67:
    case 80:
    case 81:
    case 82:
      return Icons.water_drop_rounded; // rain / showers
    case 71:
    case 73:
    case 75:
    case 77:
    case 85:
    case 86:
      return Icons.ac_unit_rounded; // snow
    case 95:
    case 96:
    case 99:
      return Icons.flash_on_rounded; // thunderstorm
    default:
      return Icons.cloud_rounded;
  }
}

/// A tint for the weather icon, keyed to the condition group. Warm for sun, cool blues
/// for rain/snow, amber for storms — readable on the dark ambient background.
Color weatherIconColor(int code, {required bool isDay}) {
  switch (code) {
    case 0:
    case 1:
      return isDay ? const Color(0xFFFFD166) : const Color(0xFFCBD5E1);
    case 2:
    case 3:
      return const Color(0xFFE2E8F0);
    case 45:
    case 48:
      return const Color(0xFFCBD5E1);
    case 51:
    case 53:
    case 55:
    case 56:
    case 57:
    case 61:
    case 63:
    case 65:
    case 66:
    case 67:
    case 80:
    case 81:
    case 82:
      return const Color(0xFF7CC4FA); // rain blue
    case 71:
    case 73:
    case 75:
    case 77:
    case 85:
    case 86:
      return const Color(0xFFBFE9FF); // snow pale blue
    case 95:
    case 96:
    case 99:
      return const Color(0xFFFFC24B); // storm amber
    default:
      return const Color(0xFFE2E8F0);
  }
}
