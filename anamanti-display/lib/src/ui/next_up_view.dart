// Full-screen "up next" queue for the 8-inch Echo Show.
//
// Lists the upcoming tracks pushed from the Core ([MusicData.nextUp]): a small
// artwork thumbnail, the track title, and the artist per row. Reachable from the
// now-playing card's "up next" button; left by the close control (or back).

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/music_data.dart';
import 'package:anamanti_display/src/ui/album_art.dart';

class NextUpView extends StatelessWidget {
  const NextUpView({
    super.key,
    required this.music,
    required this.onClose,
    this.onBack,
  });

  final MusicData music;
  final VoidCallback onClose;

  /// Returns to the now-playing card. A leading back control shows only when
  /// this is non-null.
  final VoidCallback? onBack;

  static const _bgTop = Color(0xFF101014);
  static const _bgBottom = Color(0xFF1C1A24);

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
              Expanded(child: music.nextUp.isEmpty ? _empty() : _list()),
            ],
          ),
        ),
      ),
    );
  }

  Widget _header() {
    return Padding(
      padding: const EdgeInsets.fromLTRB(12, 16, 12, 0),
      child: Row(
        children: [
          if (onBack != null)
            IconButton(
              key: const Key('next-up-back'),
              tooltip: 'Back',
              onPressed: onBack,
              iconSize: 32,
              icon: Icon(
                Icons.arrow_back,
                color: Colors.white.withValues(alpha: 0.85),
              ),
            )
          else
            const SizedBox(width: 16),
          const Expanded(
            child: Text(
              'Up Next',
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: TextStyle(
                color: Colors.white,
                fontSize: 30,
                fontWeight: FontWeight.w700,
                letterSpacing: 0.2,
              ),
            ),
          ),
          IconButton(
            key: const Key('next-up-close'),
            tooltip: 'Close',
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

  Widget _empty() {
    return Center(
      child: Text(
        'Nothing queued',
        style: TextStyle(
          color: Colors.white.withValues(alpha: 0.6),
          fontSize: 22,
        ),
      ),
    );
  }

  Widget _list() {
    return ListView.separated(
      padding: const EdgeInsets.fromLTRB(28, 12, 28, 20),
      itemCount: music.nextUp.length,
      separatorBuilder: (_, _) => const SizedBox(height: 12),
      itemBuilder: (context, i) => _row(music.nextUp[i]),
    );
  }

  Widget _row(QueueTrack track) {
    return Row(
      crossAxisAlignment: CrossAxisAlignment.center,
      children: [
        AlbumArt(uri: track.artworkUri, size: 56, radius: 8, iconSize: 28),
        const SizedBox(width: 16),
        Expanded(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(
                track.trackTitle,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: const TextStyle(
                  color: Colors.white,
                  fontSize: 20,
                  fontWeight: FontWeight.w600,
                ),
              ),
              if (track.artist.isNotEmpty) ...[
                const SizedBox(height: 2),
                Text(
                  track.artist,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(
                    color: Colors.white.withValues(alpha: 0.6),
                    fontSize: 16,
                  ),
                ),
              ],
            ],
          ),
        ),
      ],
    );
  }
}
