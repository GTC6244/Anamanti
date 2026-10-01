import 'dart:convert';

/// A place report pushed from the orchestrator (`places_lookup` tool) and rendered on
/// the display's full-screen place card: name, address, opening hours, rating, phone,
/// website, and a photo.
///
/// Mirrors the orchestrator's `crate::places::PlaceReport`: it arrives on the engine
/// stream as the `placeJson` string of a `WakeWordEventKind.showPlace` event and is
/// decoded here. All fields tolerate being absent (older/partial payloads).
class PlaceData {
  const PlaceData({
    required this.name,
    this.address = '',
    this.category = '',
    this.openNow,
    this.hours = const [],
    this.rating = '',
    this.ratingCount = 0,
    this.priceLevel = '',
    this.phone = '',
    this.website = '',
    this.mapsUri = '',
    this.photoUri = '',
  });

  final String name;
  final String address;

  /// A short category label, e.g. "Coffee shop" (empty when absent).
  final String category;

  /// Whether the place is open right now: `true`/`false`, or `null` when unknown.
  final bool? openNow;

  /// Human-readable weekly opening hours, one line per weekday.
  final List<String> hours;

  /// Average rating as text, e.g. "4.5" (empty when unrated).
  final String rating;
  final int ratingCount;

  /// Human price level, e.g. "Moderate" (empty when absent).
  final String priceLevel;
  final String phone;
  final String website;
  final String mapsUri;

  /// A keyless photo URL the device can fetch directly (empty when no photo).
  final String photoUri;

  bool get hasPhoto => photoUri.trim().isNotEmpty;

  /// Decode from the engine event's `placeJson`. Returns `null` when the string is empty
  /// or malformed, so a bad payload never crashes the UI.
  static PlaceData? tryParse(String jsonStr) {
    if (jsonStr.trim().isEmpty) return null;
    try {
      final decoded = jsonDecode(jsonStr);
      if (decoded is! Map<String, dynamic>) return null;
      final name = (decoded['name'] as String?) ?? '';
      if (name.trim().isEmpty) return null;
      return PlaceData(
        name: name,
        address: (decoded['address'] as String?) ?? '',
        category: (decoded['category'] as String?) ?? '',
        openNow: decoded['open_now'] as bool?,
        hours: _strings(decoded['hours']),
        rating: (decoded['rating'] as String?) ?? '',
        ratingCount: _int(decoded['rating_count']),
        priceLevel: (decoded['price_level'] as String?) ?? '',
        phone: (decoded['phone'] as String?) ?? '',
        website: (decoded['website'] as String?) ?? '',
        mapsUri: (decoded['maps_uri'] as String?) ?? '',
        photoUri: (decoded['photo_uri'] as String?) ?? '',
      );
    } catch (_) {
      return null;
    }
  }

  static List<String> _strings(Object? value) {
    if (value is! List) return const [];
    return value.whereType<String>().toList(growable: false);
  }
}

/// Tolerantly read an int from a JSON number (handles ints and doubles).
int _int(Object? v) {
  if (v is int) return v;
  if (v is num) return v.round();
  return 0;
}
