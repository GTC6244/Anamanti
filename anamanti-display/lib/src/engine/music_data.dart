import 'dart:convert';

/// A single queued track in the music "next up" list.
///
/// Mirrors the Core's queue item: a title, artist, album, and a keyless artwork
/// URL the device can fetch directly. All fields tolerate being absent (older or
/// partial payloads) via [fromJson].
class QueueTrack {
  const QueueTrack({
    required this.trackTitle,
    this.artist = '',
    this.album = '',
    this.artworkUri = '',
  });

  final String trackTitle;
  final String artist;
  final String album;

  /// A keyless artwork URL the device can fetch directly (empty when none).
  final String artworkUri;

  bool get hasArtwork => artworkUri.trim().isNotEmpty;

  /// Tolerantly decode one queue item. Returns `null` when `track_title` is
  /// empty, so a malformed entry is skipped rather than rendered blank.
  static QueueTrack? fromJson(Map<String, dynamic> json) {
    final title = _string(json['track_title']);
    if (title.trim().isEmpty) return null;
    return QueueTrack(
      trackTitle: title,
      artist: _string(json['artist']),
      album: _string(json['album']),
      artworkUri: _string(json['artwork_uri']),
    );
  }
}

/// The music state pushed from the Core and rendered on the display's music
/// surfaces (now-playing, next-up, and the compact control overlay): what's
/// playing, its artwork and progress, the volume, and the upcoming queue.
///
/// All fields tolerate being absent (older/partial payloads). Decode with
/// [tryParse], which never throws and returns `null` on a bad payload so the UI
/// never crashes.
class MusicData {
  const MusicData({
    this.playing = false,
    required this.trackTitle,
    this.artist = '',
    this.album = '',
    this.artworkUri = '',
    this.positionSecs = 0,
    this.durationSecs = 0,
    this.volumePercent = 0,
    this.nextUp = const [],
  });

  final bool playing;
  final String trackTitle;
  final String artist;
  final String album;

  /// A keyless artwork URL the device can fetch directly (empty when none).
  final String artworkUri;

  final int positionSecs;
  final int durationSecs;

  /// Current volume as a percent, 0..100.
  final int volumePercent;

  /// The upcoming queue (empty when nothing is queued).
  final List<QueueTrack> nextUp;

  bool get hasArtwork => artworkUri.trim().isNotEmpty;

  /// Decode from the Core's music JSON. Returns `null` when the string is empty,
  /// malformed, not a JSON object, or carries an empty `track_title`, so a bad
  /// payload never crashes the UI. Malformed `next_up` items are skipped.
  static MusicData? tryParse(String jsonStr) {
    if (jsonStr.trim().isEmpty) return null;
    try {
      final decoded = jsonDecode(jsonStr);
      if (decoded is! Map<String, dynamic>) return null;
      final title = _string(decoded['track_title']);
      if (title.trim().isEmpty) return null;
      return MusicData(
        playing: _bool(decoded['playing']),
        trackTitle: title,
        artist: _string(decoded['artist']),
        album: _string(decoded['album']),
        artworkUri: _string(decoded['artwork_uri']),
        positionSecs: _int(decoded['position_secs']),
        durationSecs: _int(decoded['duration_secs']),
        volumePercent: _int(decoded['volume_percent']),
        nextUp: _queue(decoded['next_up']),
      );
    } catch (_) {
      return null;
    }
  }

  static List<QueueTrack> _queue(Object? value) {
    if (value is! List) return const [];
    final out = <QueueTrack>[];
    for (final item in value) {
      if (item is Map<String, dynamic>) {
        final track = QueueTrack.fromJson(item);
        if (track != null) out.add(track);
      }
    }
    return List.unmodifiable(out);
  }
}

/// Tolerantly read a string from a JSON value (empty string when absent/null).
String _string(Object? v) => v is String ? v : '';

/// Tolerantly read an int from a JSON number (handles ints and doubles).
int _int(Object? v) {
  if (v is int) return v;
  if (v is num) return v.round();
  return 0;
}

/// Tolerantly read a bool from a JSON value (false when absent/non-bool).
bool _bool(Object? v) => v is bool ? v : false;
