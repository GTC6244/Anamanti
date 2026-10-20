// Full-screen now-playing card for the 8-inch Echo Show.
//
// Shown while the Core is playing music: a large album artwork, the track title,
// artist, and album, a progress bar with mm:ss position/duration labels, a
// transport row (previous / play-pause / next), a volume slider, and an optional
// "up next" button. Landscape-first with large, arm's-length type. Left by voice
// or the minimize control (back to the compact bar; when this display's style is full
// screen, minimizing switches this display — only — to compact).
//
// For live radio (`MusicData.isRadio`) the header is the station name with a LIVE
// badge, the logo is shown uncropped, the song from the stream is the headline, the
// seek bar and queue are hidden, and the transport is previous station / stop / next
// station.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/music_data.dart';
import 'package:anamanti_display/src/ui/album_art.dart';

/// Format a clock as `m:ss` (seconds zero-padded, minutes not). Negatives clamp
/// to zero. Used for both the position and duration labels.
String formatClock(int secs) {
  if (secs < 0) secs = 0;
  final m = secs ~/ 60;
  final s = secs % 60;
  return '$m:${s.toString().padLeft(2, '0')}';
}

class NowPlayingView extends StatelessWidget {
  const NowPlayingView({
    super.key,
    required this.music,
    required this.onClose,
    required this.onPlayPause,
    required this.onNext,
    required this.onPrevious,
    required this.onVolume,
    this.onShowQueue,
  });

  final MusicData music;
  final VoidCallback onClose;
  final VoidCallback onPlayPause;
  final VoidCallback onNext;
  final VoidCallback onPrevious;

  /// New volume as a percent, 0..100.
  final ValueChanged<int> onVolume;

  /// Opens the Next Up list. The button shows only when this is non-null AND
  /// [MusicData.nextUp] is non-empty.
  final VoidCallback? onShowQueue;

  static const _bgTop = Color(0xFF101014);
  static const _bgBottom = Color(0xFF1C1A24);
  static const _accent = Color(0xFF8E7CFF);

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
                  padding: const EdgeInsets.fromLTRB(28, 4, 28, 16),
                  child: Row(
                    crossAxisAlignment: CrossAxisAlignment.center,
                    children: [
                      AlbumArt(
                        uri: music.artworkUri,
                        size: 280,
                        radius: 16,
                        iconSize: 96,
                        fit: music.isRadio ? BoxFit.contain : BoxFit.cover,
                      ),
                      const SizedBox(width: 28),
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

  /// The header line: the track for Spotify, the station for radio.
  String get _headline {
    if (music.isRadio) {
      return music.stationName.isEmpty ? 'Radio' : music.stationName;
    }
    return music.trackTitle.isEmpty ? 'Music' : music.trackTitle;
  }

  Widget _header() {
    return Padding(
      padding: const EdgeInsets.fromLTRB(28, 16, 12, 0),
      child: Row(
        children: [
          if (music.isRadio) ...[
            Container(
              key: const Key('music-live-badge'),
              padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 3),
              decoration: BoxDecoration(
                color: const Color(0xFFE5484D),
                borderRadius: BorderRadius.circular(6),
              ),
              child: const Text(
                'LIVE',
                style: TextStyle(
                  color: Colors.white,
                  fontSize: 14,
                  fontWeight: FontWeight.w800,
                  letterSpacing: 1.2,
                ),
              ),
            ),
            const SizedBox(width: 12),
          ],
          Expanded(
            child: Text(
              _headline,
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
            key: const Key('music-close'),
            tooltip: 'Small player',
            onPressed: onClose,
            iconSize: 32,
            icon: Icon(
              Icons.close_fullscreen,
              color: Colors.white.withValues(alpha: 0.85),
            ),
          ),
        ],
      ),
    );
  }

  /// Radio: the current song (when the stream sends one) as the big line, the
  /// artist under it. With no song, the station name is already the header.
  List<Widget> _radioSong() {
    final hasSong =
        music.trackTitle.isNotEmpty && music.trackTitle != music.stationName;
    if (!hasSong) {
      return [
        Text(
          'Live radio',
          style: TextStyle(
            color: Colors.white.withValues(alpha: 0.6),
            fontSize: 20,
          ),
        ),
      ];
    }
    return [
      Text(
        music.trackTitle,
        key: const Key('music-radio-song'),
        maxLines: 2,
        overflow: TextOverflow.ellipsis,
        style: const TextStyle(
          color: Colors.white,
          fontSize: 26,
          fontWeight: FontWeight.w700,
        ),
      ),
      if (music.artist.isNotEmpty) ...[
        const SizedBox(height: 4),
        Text(
          music.artist,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: TextStyle(
            color: Colors.white.withValues(alpha: 0.75),
            fontSize: 20,
            fontWeight: FontWeight.w500,
          ),
        ),
      ],
    ];
  }

  Widget _details() {
    if (music.isRadio) {
      return Column(
        mainAxisAlignment: MainAxisAlignment.center,
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          ..._radioSong(),
          const SizedBox(height: 24),
          _transport(),
          const SizedBox(height: 10),
          _volume(),
        ],
      );
    }
    return Column(
      mainAxisAlignment: MainAxisAlignment.center,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (music.artist.isNotEmpty)
          Text(
            music.artist,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: const TextStyle(
              color: Colors.white,
              fontSize: 22,
              fontWeight: FontWeight.w600,
            ),
          ),
        if (music.album.isNotEmpty) ...[
          const SizedBox(height: 4),
          Text(
            music.album,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: TextStyle(
              color: Colors.white.withValues(alpha: 0.6),
              fontSize: 16,
            ),
          ),
        ],
        const SizedBox(height: 18),
        _progress(),
        const SizedBox(height: 14),
        _transport(),
        const SizedBox(height: 10),
        _volume(),
        if (onShowQueue != null && music.nextUp.isNotEmpty) ...[
          const SizedBox(height: 8),
          Align(
            alignment: Alignment.centerLeft,
            child: TextButton.icon(
              key: const Key('music-show-queue'),
              onPressed: onShowQueue,
              icon: const Icon(Icons.queue_music, color: _accent),
              label: Text(
                'Up next (${music.nextUp.length})',
                style: const TextStyle(color: _accent, fontSize: 16),
              ),
            ),
          ),
        ],
      ],
    );
  }

  Widget _progress() {
    final value = music.durationSecs > 0
        ? (music.positionSecs / music.durationSecs).clamp(0.0, 1.0)
        : 0.0;
    final label = TextStyle(
      color: Colors.white.withValues(alpha: 0.7),
      fontSize: 14,
      fontFeatures: const [FontFeature.tabularFigures()],
    );
    return Row(
      children: [
        Text(formatClock(music.positionSecs), style: label),
        const SizedBox(width: 10),
        Expanded(
          child: ClipRRect(
            borderRadius: BorderRadius.circular(3),
            child: LinearProgressIndicator(
              value: value,
              minHeight: 6,
              backgroundColor: Colors.white.withValues(alpha: 0.14),
              valueColor: const AlwaysStoppedAnimation<Color>(_accent),
            ),
          ),
        ),
        const SizedBox(width: 10),
        Text(formatClock(music.durationSecs), style: label),
      ],
    );
  }

  Widget _transport() {
    final radio = music.isRadio;
    return Row(
      mainAxisAlignment: MainAxisAlignment.center,
      children: [
        IconButton(
          key: const Key('music-previous'),
          tooltip: radio ? 'Previous station' : 'Previous',
          onPressed: onPrevious,
          iconSize: 44,
          icon: const Icon(Icons.skip_previous, color: Colors.white),
        ),
        const SizedBox(width: 12),
        IconButton(
          key: const Key('music-play-pause'),
          tooltip: radio ? 'Stop' : (music.playing ? 'Pause' : 'Play'),
          onPressed: onPlayPause,
          iconSize: 72,
          icon: Icon(
            radio
                ? Icons.stop_circle
                : music.playing
                ? Icons.pause_circle_filled
                : Icons.play_circle_filled,
            color: _accent,
          ),
        ),
        const SizedBox(width: 12),
        IconButton(
          key: const Key('music-next'),
          tooltip: radio ? 'Next station' : 'Next',
          onPressed: onNext,
          iconSize: 44,
          icon: const Icon(Icons.skip_next, color: Colors.white),
        ),
      ],
    );
  }

  Widget _volume() {
    return Row(
      children: [
        Icon(Icons.volume_down, color: Colors.white.withValues(alpha: 0.7)),
        Expanded(
          child: Slider(
            key: const Key('music-volume'),
            min: 0,
            max: 100,
            value: music.volumePercent.toDouble().clamp(0, 100),
            activeColor: _accent,
            onChanged: (v) => onVolume(v.round()),
          ),
        ),
        Icon(Icons.volume_up, color: Colors.white.withValues(alpha: 0.7)),
      ],
    );
  }
}
