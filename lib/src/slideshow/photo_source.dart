// The idle ambient photo slideshow (Plan.MD §3, Phase 5; decision table: idle
// screen = photo slideshow from a chosen Google Photos/Drive folder).
//
// Design goals:
//  * The slideshow runs **independently of the assistant connection** — it keeps
//    cycling even when the Mac is unreachable (Plan.MD §3, Phase 5 resilience).
//  * The photo *source* is pluggable. [LocalPhotoSource] is an always-available
//    offline fallback (curated ambient gradients) so a fresh device shows a
//    pleasant idle screen before any account is linked; [GooglePhotoSource] is the
//    on-device OAuth source wired in the settings screen (Phase 6).
//  * Loading is resilient: if the configured source fails (no auth / offline) the
//    controller silently falls back to the local source.

import 'dart:async';

import 'package:flutter/material.dart';

/// One slide: either a remote image (with a graceful gradient fallback) or a
/// built-in ambient gradient. Kept intentionally small so sources can be mocked.
@immutable
class PhotoItem {
  const PhotoItem.network(this.imageUrl, {this.caption})
      : gradient = _defaultGradient;

  const PhotoItem.gradient(this.gradient, {this.caption}) : imageUrl = null;

  /// Remote image URL, or null for a pure gradient slide.
  final String? imageUrl;

  /// Fallback / background gradient colors.
  final List<Color> gradient;

  /// Optional caption (e.g. album name, date).
  final String? caption;

  static const List<Color> _defaultGradient = [
    Color(0xFF1A2036),
    Color(0xFF0B0E1A),
  ];
}

/// A source of slideshow photos.
abstract class PhotoSource {
  /// Human-readable label shown in settings ("Local", "Google Photos: Family").
  String get label;

  /// Load the current set of photos. May throw if the source is unavailable
  /// (offline, not authorized); callers should fall back to [LocalPhotoSource].
  Future<List<PhotoItem>> loadPhotos();
}

/// Always-available offline source: a curated set of calm ambient gradients so the
/// idle screen looks intentional with no account linked and no network.
class LocalPhotoSource implements PhotoSource {
  const LocalPhotoSource();

  @override
  String get label => 'Local ambient';

  @override
  Future<List<PhotoItem>> loadPhotos() async {
    return const [
      PhotoItem.gradient([Color(0xFF243B55), Color(0xFF141E30)], caption: 'Dusk'),
      PhotoItem.gradient([Color(0xFF3A1C71), Color(0xFF161226)], caption: 'Aurora'),
      PhotoItem.gradient([Color(0xFF0F2027), Color(0xFF203A43)], caption: 'Deep'),
      PhotoItem.gradient([Color(0xFF42275A), Color(0xFF1B1035)], caption: 'Twilight'),
      PhotoItem.gradient([Color(0xFF16222A), Color(0xFF3A6073)], caption: 'Slate'),
    ];
  }
}

/// On-device Google Photos / Drive source (Plan.MD decision: on-device OAuth — the
/// Echo Show runs the consent flow and calls Google directly).
///
/// The OAuth flow itself is wired in the Phase-6 settings screen (it needs a
/// registered Google client ID and the `google_sign_in` / `googleapis` packages);
/// this class is the seam the slideshow talks to. Until an access token + folder
/// are configured, [loadPhotos] throws so the controller falls back to
/// [LocalPhotoSource]. (Per Plan.MD §4, the claude.ai Drive connector in this
/// environment is unauthorized and cannot prototype the flow — real OAuth is wired
/// into the app.)
class GooglePhotoSource implements PhotoSource {
  const GooglePhotoSource({
    required this.folderName,
    this.accessToken,
    this.photoUrls = const [],
  });

  /// The chosen album / Drive folder name (shown in settings).
  final String folderName;

  /// OAuth access token obtained by the on-device consent flow, or null if the
  /// account is not yet linked.
  final String? accessToken;

  /// Pre-resolved image URLs for the folder (populated by the folder picker).
  final List<String> photoUrls;

  /// The scopes the on-device consent flow requests.
  static const List<String> oauthScopes = [
    'https://www.googleapis.com/auth/photoslibrary.readonly',
    'https://www.googleapis.com/auth/drive.readonly',
  ];

  @override
  String get label => 'Google Photos: $folderName';

  @override
  Future<List<PhotoItem>> loadPhotos() async {
    if (accessToken == null) {
      throw StateError('Google account not linked — run the settings consent flow');
    }
    return [
      for (final url in photoUrls) PhotoItem.network(url, caption: folderName),
    ];
  }
}

/// Cycles through a [PhotoSource]'s photos on a timer, independent of the
/// assistant. Falls back to [LocalPhotoSource] if the configured source fails.
class SlideshowController extends ChangeNotifier {
  SlideshowController({
    PhotoSource? source,
    this.interval = const Duration(seconds: 10),
  }) : _source = source ?? const LocalPhotoSource();

  PhotoSource _source;
  final Duration interval;

  List<PhotoItem> _photos = const [];
  int _index = 0;
  Timer? _timer;
  bool _disposed = false;

  List<PhotoItem> get photos => _photos;

  /// The slide currently on screen, or null before the first load.
  PhotoItem? get current => _photos.isEmpty ? null : _photos[_index % _photos.length];

  int get index => _index;

  /// Load photos and begin cycling. Falls back to local ambient gradients if the
  /// configured source throws (offline / not authorized).
  Future<void> start() async {
    await _reload();
    _timer?.cancel();
    _timer = Timer.periodic(interval, (_) => next());
  }

  /// Swap the source (e.g. after linking a Google account in settings) and reload.
  Future<void> setSource(PhotoSource source) async {
    _source = source;
    _index = 0;
    await _reload();
  }

  Future<void> _reload() async {
    List<PhotoItem> loaded;
    try {
      loaded = await _source.loadPhotos();
      if (loaded.isEmpty) {
        loaded = await const LocalPhotoSource().loadPhotos();
      }
    } catch (_) {
      loaded = await const LocalPhotoSource().loadPhotos();
    }
    if (_disposed) return;
    _photos = loaded;
    if (_index >= _photos.length) _index = 0;
    notifyListeners();
  }

  /// Advance to the next slide (wraps around).
  void next() {
    if (_photos.isEmpty || _disposed) return;
    _index = (_index + 1) % _photos.length;
    notifyListeners();
  }

  @override
  void dispose() {
    _disposed = true;
    _timer?.cancel();
    super.dispose();
  }
}
