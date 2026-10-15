// Compact music transport bar (analogue of the compact timers chip row).
//
// Rides the ambient screen while music plays, so the screen yields to the
// ambient content but the transport stays glanceable and tappable. A tiny
// artwork thumb, the track title/artist, previous / play-pause / next controls,
// and a compact volume stepper. Tapping the title area opens the now-playing
// card via [onTap]. For live radio the play/pause control is a stop button,
// previous/next change station, and the logo is shown uncropped.
//
// Uses keys DISTINCT from the full now-playing view so both can be mounted at
// once without duplicate-key collisions. The parent handles visibility; this
// widget stays tappable (it is not wrapped in IgnorePointer itself).

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/music_data.dart';
import 'package:anamanti_display/src/ui/album_art.dart';

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
          AlbumArt(
            uri: music.artworkUri,
            size: 40,
            radius: 6,
            iconSize: 22,
            fit: music.isRadio ? BoxFit.contain : BoxFit.cover,
          ),
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
            tooltip: music.isRadio ? 'Previous station' : 'Previous',
            onPressed: onPrevious,
            iconSize: 24,
            icon: const Icon(Icons.skip_previous, color: Colors.white),
          ),
          IconButton(
            key: const Key('music-overlay-play-pause'),
            tooltip: music.isRadio
                ? 'Stop'
                : (music.playing ? 'Pause' : 'Play'),
            onPressed: onPlayPause,
            iconSize: 30,
            icon: Icon(
              music.isRadio
                  ? Icons.stop
                  : music.playing
                  ? Icons.pause
                  : Icons.play_arrow,
              color: _accent,
            ),
          ),
          IconButton(
            key: const Key('music-overlay-next'),
            tooltip: music.isRadio ? 'Next station' : 'Next',
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
            icon: Icon(
              Icons.volume_down,
              color: Colors.white.withValues(alpha: 0.85),
            ),
          ),
          IconButton(
            key: const Key('music-overlay-volume-up'),
            tooltip: 'Volume up',
            onPressed: () =>
                onVolume((music.volumePercent + _step).clamp(0, 100)),
            iconSize: 22,
            icon: Icon(
              Icons.volume_up,
              color: Colors.white.withValues(alpha: 0.85),
            ),
          ),
        ],
      ),
    );
  }

  /// The second line: the artist, or for radio "artist · station" (just the
  /// station when the stream sends no song, since the title is then the station).
  String get _subtitle {
    if (!music.isRadio) return music.artist;
    final station = music.trackTitle == music.stationName
        ? ''
        : music.stationName;
    return [music.artist, station].where((s) => s.isNotEmpty).join(' · ');
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
        if (_subtitle.isNotEmpty)
          Text(
            _subtitle,
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
}
