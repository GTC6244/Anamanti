// Full-screen place card for the 8-inch Echo Show.
//
// Shown when the orchestrator's `places_lookup` tool pushes a place (see
// [AssistantState.place]). A hero photo sits beside the place's details — name,
// category, address, an open-now chip, the weekly opening hours, rating, phone, and
// website. Landscape-first with large, arm's-length type. Left by voice or the close
// control.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/place_data.dart';

class PlaceView extends StatelessWidget {
  const PlaceView({super.key, required this.place, required this.onClose});

  final PlaceData place;
  final VoidCallback onClose;

  static const _bgTop = Color(0xFF0B1220);
  static const _bgBottom = Color(0xFF13233B);
  static const _accent = Color(0xFF7FB2FF);

  @override
  Widget build(BuildContext context) {
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
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              _header(),
              Expanded(
                child: Padding(
                  padding: const EdgeInsets.fromLTRB(28, 4, 28, 20),
                  child: Row(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      if (place.hasPhoto) ...[
                        _photo(),
                        const SizedBox(width: 24),
                      ],
                      Expanded(child: _details()),
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

  Widget _header() {
    return Padding(
      padding: const EdgeInsets.fromLTRB(28, 16, 12, 0),
      child: Row(
        children: [
          Expanded(
            child: Text(
              place.name.isEmpty ? 'Place' : place.name,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: const TextStyle(
                color: Colors.white,
                fontSize: 30,
                fontWeight: FontWeight.w700,
                letterSpacing: 0.2,
              ),
            ),
          ),
          IconButton(
            key: const Key('place-close'),
            tooltip: 'Close place',
            onPressed: onClose,
            iconSize: 32,
            icon: Icon(Icons.close, color: Colors.white.withValues(alpha: 0.85)),
          ),
        ],
      ),
    );
  }

  /// The hero photo, capped in size and with a graceful fallback to a category icon so
  /// a slow/failed image never blocks the card (respects the ~1 GB memory budget).
  Widget _photo() {
    return ClipRRect(
      borderRadius: BorderRadius.circular(16),
      child: Image.network(
        place.photoUri,
        width: 320,
        height: 320,
        fit: BoxFit.cover,
        gaplessPlayback: true,
        errorBuilder: (_, _, _) => _photoFallback(),
        loadingBuilder: (context, child, progress) =>
            progress == null ? child : _photoFallback(),
      ),
    );
  }

  Widget _photoFallback() {
    return Container(
      width: 320,
      height: 320,
      color: Colors.white.withValues(alpha: 0.06),
      child: Icon(
        Icons.place,
        size: 96,
        color: Colors.white.withValues(alpha: 0.5),
      ),
    );
  }

  Widget _details() {
    final rows = <Widget>[];

    // Category + open-now chip.
    final chips = <Widget>[];
    if (place.category.isNotEmpty) chips.add(_chip(place.category, _accent));
    if (place.openNow == true) {
      chips.add(_chip('Open now', const Color(0xFF57C77A)));
    } else if (place.openNow == false) {
      chips.add(_chip('Closed', const Color(0xFFE0796B)));
    }
    if (chips.isNotEmpty) {
      rows.add(Wrap(spacing: 10, runSpacing: 8, children: chips));
      rows.add(const SizedBox(height: 16));
    }

    // Rating + price.
    final meta = <String>[];
    if (place.rating.isNotEmpty) {
      final count =
          place.ratingCount > 0 ? ' (${place.ratingCount} reviews)' : '';
      meta.add('★ ${place.rating}$count');
    }
    if (place.priceLevel.isNotEmpty) meta.add(place.priceLevel);
    if (meta.isNotEmpty) {
      rows.add(_line(meta.join('   ·   '), size: 20, color: Colors.white));
      rows.add(const SizedBox(height: 12));
    }

    if (place.address.isNotEmpty) {
      rows.add(_iconLine(Icons.location_on, place.address));
    }
    if (place.phone.isNotEmpty) {
      rows.add(_iconLine(Icons.phone, place.phone));
    }
    if (place.website.isNotEmpty) {
      rows.add(_iconLine(Icons.language, place.website));
    }

    if (place.hours.isNotEmpty) {
      rows.add(const SizedBox(height: 14));
      rows.add(_line('Hours', size: 18, color: _accent, weight: FontWeight.w600));
      rows.add(const SizedBox(height: 6));
      for (final h in place.hours) {
        rows.add(_line(h, size: 17, color: Colors.white.withValues(alpha: 0.85)));
      }
    }

    return SingleChildScrollView(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: rows,
      ),
    );
  }

  Widget _chip(String text, Color color) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
      decoration: BoxDecoration(
        color: color.withValues(alpha: 0.18),
        borderRadius: BorderRadius.circular(20),
        border: Border.all(color: color.withValues(alpha: 0.6)),
      ),
      child: Text(
        text,
        style: TextStyle(color: color, fontSize: 16, fontWeight: FontWeight.w600),
      ),
    );
  }

  Widget _iconLine(IconData icon, String text) {
    return Padding(
      padding: const EdgeInsets.only(bottom: 10),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Icon(icon, size: 22, color: _accent),
          const SizedBox(width: 10),
          Expanded(
            child: Text(
              text,
              style: TextStyle(
                color: Colors.white.withValues(alpha: 0.9),
                fontSize: 19,
                height: 1.25,
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _line(String text,
      {required double size, required Color color, FontWeight? weight}) {
    return Padding(
      padding: const EdgeInsets.only(bottom: 2),
      child: Text(
        text,
        style: TextStyle(color: color, fontSize: size, fontWeight: weight),
      ),
    );
  }
}
