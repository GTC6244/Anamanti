import 'dart:convert';

/// Which music widget this display shows while something is playing
/// (`plans/MusicDisplayStylePlan.md`), shared by Spotify and radio. Chosen **per display**
/// by the Core and carried on this display's now-playing pushes as `display_style`: full
/// screen on the display the play request came from and compact elsewhere, unless this
/// display chose otherwise (its expand/minimize button, or voice spoken to it) or has a
/// pin on the config page.
enum MusicDisplayStyle {
  /// The compact transport bar riding the ambient screen (the default).
  compact,

  /// The full-screen now-playing view.
  fullscreen;

  /// This display's style before the Core has stamped one on a now-playing push (or
  /// with an older Core that sends none). The Core then picks per display: full screen
  /// where the play request came from, compact elsewhere.
  static const defaultStyle = MusicDisplayStyle.compact;

  /// Parse the wire label; `null` for anything else (an older Core sends none).
  static MusicDisplayStyle? tryParse(Object? v) => switch (v) {
    'compact' => MusicDisplayStyle.compact,
    'fullscreen' => MusicDisplayStyle.fullscreen,
    _ => null,
  };
}

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
///
/// A snapshot is either Spotify ([source] `spotify`, or absent from an older Core) or
/// live **radio** ([source] `radio`, see `plans/RadioPlan.md`): then [trackTitle] /
/// [artist] are the current song from the stream (or the station name when the
/// stream sends none), [artworkUri] is the station logo, and there is no duration
/// or queue.
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
    this.source = '',
    this.stationId = '',
    this.stationName = '',
    this.displayStyle,
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

  /// `spotify`, `radio`, or empty (an older Core — treated as Spotify).
  final String source;

  /// Radio only: the station's call sign (e.g. `CJDV`) and display name.
  final String stationId;
  final String stationName;

  /// This display's music widget, stamped on this push by the Core, or `null` from an
  /// older Core (then the device keeps its current style).
  final MusicDisplayStyle? displayStyle;

  bool get hasArtwork => artworkUri.trim().isNotEmpty;

  /// Live radio: no seek bar, no queue; pause stops, next/previous change station.
  bool get isRadio => source == 'radio';

  /// This snapshot at a different [volumePercent] — the on-screen controls show a
  /// requested level at once, before the Core's confirming push arrives.
  MusicData withVolume(int percent) => MusicData(
    playing: playing,
    trackTitle: trackTitle,
    artist: artist,
    album: album,
    artworkUri: artworkUri,
    positionSecs: positionSecs,
    durationSecs: durationSecs,
    volumePercent: percent,
    nextUp: nextUp,
    source: source,
    stationId: stationId,
    stationName: stationName,
    displayStyle: displayStyle,
  );

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
        source: _string(decoded['source']),
        stationId: _string(decoded['station_id']),
        stationName: _string(decoded['station_name']),
        displayStyle: MusicDisplayStyle.tryParse(decoded['display_style']),
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
