// Compact music transport bar (analogue of the compact timers chip row).
//
// Rides the ambient screen while music plays, so the screen yields to the
// ambient content but the transport stays glanceable and tappable. A tiny
// artwork thumb, the track title/artist, previous / play-pause / next controls,
// and a compact volume stepper. Tapping the title area opens the now-playing
// card via [onTap].
//
// Uses keys DISTINCT from the full now-playing view so both can be mounted at
// once without duplicate-key collisions. The parent handles visibility; this
// widget stays tappable (it is not wrapped in IgnorePointer itself).

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/music_data.dart';

class MusicControlOverlay extends StatelessWidget {
  const MusicControlOverlay({
    super.key,
    required this.music,
    required this.onPlayPause,
    required this.onNext,
    required this.onPrevious,
    required this.onVolume,
    this.onTap,
  });

  final MusicData music;
  final VoidCallback onPlayPause;
  final VoidCallback onNext;
  final VoidCallback onPrevious;

  /// New volume as a percent, 0..100.
  final ValueChanged<int> onVolume;

  /// Opens the now-playing card. The title area is tappable only when non-null.
  final VoidCallback? onTap;

  static const _accent = Color(0xFF8E7CFF);

  /// Volume step for the compact down/up buttons.
  static const _step = 5;

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
      decoration: BoxDecoration(
        color: Colors.black.withValues(alpha: 0.55),
        borderRadius: BorderRadius.circular(20),
        border: Border.all(color: Colors.white.withValues(alpha: 0.12)),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          _thumb(),
          const SizedBox(width: 10),
          Flexible(
            child: GestureDetector(
              key: const Key('music-overlay-open'),
              behavior: HitTestBehavior.opaque,
              onTap: onTap,
              child: _titleBlock(),
            ),
          ),
          const SizedBox(width: 8),
          IconButton(
            key: const Key('music-overlay-previous'),
            tooltip: 'Previous',
            onPressed: onPrevious,
            iconSize: 24,
            icon: const Icon(Icons.skip_previous, color: Colors.white),
          ),
          IconButton(
            key: const Key('music-overlay-play-pause'),
            tooltip: music.playing ? 'Pause' : 'Play',
            onPressed: onPlayPause,
            iconSize: 30,
            icon: Icon(
              music.playing ? Icons.pause : Icons.play_arrow,
              color: _accent,
            ),
          ),
          IconButton(
            key: const Key('music-overlay-next'),
            tooltip: 'Next',
            onPressed: onNext,
            iconSize: 24,
            icon: const Icon(Icons.skip_next, color: Colors.white),
          ),
          IconButton(
            key: const Key('music-overlay-volume'),
            tooltip: 'Volume down',
            onPressed: () =>
                onVolume((music.volumePercent - _step).clamp(0, 100)),
            iconSize: 22,
            icon: Icon(Icons.volume_down, color: Colors.white.withValues(alpha: 0.85)),
          ),
          IconButton(
            key: const Key('music-overlay-volume-up'),
            tooltip: 'Volume up',
            onPressed: () =>
                onVolume((music.volumePercent + _step).clamp(0, 100)),
            iconSize: 22,
            icon: Icon(Icons.volume_up, color: Colors.white.withValues(alpha: 0.85)),
          ),
        ],
      ),
    );
  }

  Widget _titleBlock() {
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          music.trackTitle.isEmpty ? 'Music' : music.trackTitle,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: const TextStyle(
            color: Colors.white,
            fontSize: 16,
            fontWeight: FontWeight.w600,
          ),
        ),
        if (music.artist.isNotEmpty)
          Text(
            music.artist,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: TextStyle(
              color: Colors.white.withValues(alpha: 0.6),
              fontSize: 13,
            ),
          ),
      ],
    );
  }

  Widget _thumb() {
    if (!music.hasArtwork) return _thumbFallback();
    return ClipRRect(
      borderRadius: BorderRadius.circular(6),
      child: Image.network(
        music.artworkUri,
        width: 40,
        height: 40,
        fit: BoxFit.cover,
        gaplessPlayback: true,
        errorBuilder: (_, _, _) => _thumbFallback(),
        loadingBuilder: (context, child, progress) =>
            progress == null ? child : _thumbFallback(),
      ),
    );
  }

  Widget _thumbFallback() {
    return Container(
      width: 40,
      height: 40,
      decoration: BoxDecoration(
        color: Colors.white.withValues(alpha: 0.06),
        borderRadius: BorderRadius.circular(6),
      ),
      child: Icon(
        Icons.music_note,
        size: 22,
        color: Colors.white.withValues(alpha: 0.5),
      ),
    );
  }
}
