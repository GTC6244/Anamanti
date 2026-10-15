// Album artwork for the music surfaces (control overlay, now-playing, up-next): a
// rounded network image that falls back to a music-note tile while loading, on error,
// or when there's no artwork URL at all (no network fetch then) — so a slow/failed
// image never blocks the card.

import 'package:flutter/material.dart';

/// A square album-art tile of [size] logical pixels.
class AlbumArt extends StatelessWidget {
  const AlbumArt({
    super.key,
    required this.uri,
    required this.size,
    required this.radius,
    required this.iconSize,
    this.fit = BoxFit.cover,
  });

  /// The artwork URL; empty → the fallback tile.
  final String uri;

  /// Width and height of the tile.
  final double size;

  /// Corner radius.
  final double radius;

  /// Size of the fallback music-note icon.
  final double iconSize;

  /// How the image fills the square: `cover` for album art, `contain` for station
  /// logos (often non-square, and cropping a logo looks broken).
  final BoxFit fit;

  @override
  Widget build(BuildContext context) {
    if (uri.trim().isEmpty) return _fallback();
    return ClipRRect(
      borderRadius: BorderRadius.circular(radius),
      child: Image.network(
        uri,
        width: size,
        height: size,
        fit: fit,
        gaplessPlayback: true,
        errorBuilder: (_, _, _) => _fallback(),
        loadingBuilder: (context, child, progress) =>
            progress == null ? child : _fallback(),
      ),
    );
  }

  Widget _fallback() {
    return Container(
      width: size,
      height: size,
      decoration: BoxDecoration(
        color: Colors.white.withValues(alpha: 0.06),
        borderRadius: BorderRadius.circular(radius),
      ),
      child: Icon(
        Icons.music_note,
        size: iconSize,
        color: Colors.white.withValues(alpha: 0.5),
      ),
    );
  }
}
