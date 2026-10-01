// Full-screen now-playing card for the 8-inch Echo Show.
//
// Shown while the Core is playing music: a large album artwork, the track title,
// artist, and album, a progress bar with mm:ss position/duration labels, a
// transport row (previous / play-pause / next), a volume slider, and an optional
// "up next" button. Landscape-first with large, arm's-length type. Left by voice
// or the close control.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/music_data.dart';

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
                      _artwork(),
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

  Widget _header() {
    return Padding(
      padding: const EdgeInsets.fromLTRB(28, 16, 12, 0),
      child: Row(
        children: [
          Expanded(
            child: Text(
              music.trackTitle.isEmpty ? 'Music' : music.trackTitle,
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
            tooltip: 'Close',
            onPressed: onClose,
            iconSize: 32,
            icon: Icon(Icons.close, color: Colors.white.withValues(alpha: 0.85)),
          ),
        ],
      ),
    );
  }

  /// The large album artwork, capped in size and with a graceful fallback to a
  /// music-note icon so a slow/failed image never blocks the card. When there's
  /// no artwork URL at all we show the fallback directly (no network fetch).
  Widget _artwork() {
    if (!music.hasArtwork) return _artworkFallback();
    return ClipRRect(
      borderRadius: BorderRadius.circular(16),
      child: Image.network(
        music.artworkUri,
        width: 280,
        height: 280,
        fit: BoxFit.cover,
        gaplessPlayback: true,
        errorBuilder: (_, _, _) => _artworkFallback(),
        loadingBuilder: (context, child, progress) =>
            progress == null ? child : _artworkFallback(),
      ),
    );
  }

  Widget _artworkFallback() {
    return Container(
      width: 280,
      height: 280,
      decoration: BoxDecoration(
        color: Colors.white.withValues(alpha: 0.06),
        borderRadius: BorderRadius.circular(16),
      ),
      child: Icon(
        Icons.music_note,
        size: 96,
        color: Colors.white.withValues(alpha: 0.5),
      ),
    );
  }

  Widget _details() {
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
    final value =
        music.durationSecs > 0 ? (music.positionSecs / music.durationSecs).clamp(0.0, 1.0) : 0.0;
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
    return Row(
      mainAxisAlignment: MainAxisAlignment.center,
      children: [
        IconButton(
          key: const Key('music-previous'),
          tooltip: 'Previous',
          onPressed: onPrevious,
          iconSize: 44,
          icon: const Icon(Icons.skip_previous, color: Colors.white),
        ),
        const SizedBox(width: 12),
        IconButton(
          key: const Key('music-play-pause'),
          tooltip: music.playing ? 'Pause' : 'Play',
          onPressed: onPlayPause,
          iconSize: 72,
          icon: Icon(
            music.playing ? Icons.pause_circle_filled : Icons.play_circle_filled,
            color: _accent,
          ),
        ),
        const SizedBox(width: 12),
        IconButton(
          key: const Key('music-next'),
          tooltip: 'Next',
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
