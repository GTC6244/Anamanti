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
//
// Google Photos linking uses the **Ambient API** (device-code + QR; the Echo Show
// has no Play Services). The flow + media listing + token refresh live in
// `ambient_photos.dart`; this file only defines the [PhotoSource] abstraction and
// the [AmbientMediaLister] seam, so it carries no network dependency and stays
// trivially testable.

import 'dart:async';

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/settings/app_settings.dart';

/// One slide: either a remote image (with a graceful gradient fallback) or a
/// built-in ambient gradient. Kept intentionally small so sources can be mocked.
@immutable
class PhotoItem {
  const PhotoItem.network(this.imageUrl, {this.caption, this.headers})
    : gradient = _defaultGradient;

  const PhotoItem.gradient(this.gradient, {this.caption})
    : imageUrl = null,
      headers = null;

  /// Remote image URL, or null for a pure gradient slide.
  final String? imageUrl;

  /// Optional HTTP headers used to fetch [imageUrl] (e.g. the Drive
  /// `Authorization: Bearer <token>` header for `?alt=media` downloads).
  final Map<String, String>? headers;

  /// Fallback / background gradient colors.
  final List<Color> gradient;

  /// Optional caption (e.g. album name, date).
  final String? caption;

  static const List<Color> _defaultGradient = [
    Color(0xFF1A2036),
    Color(0xFF0B0E1A),
  ];
}

/// Loads the user's picked Google Photos media items using an OAuth access token.
/// Implemented by `AmbientApiClient.listMediaItems` in `ambient_photos.dart`;
/// injected so [AmbientPhotoSource] and [photoSourceFromSettings] carry no network
/// dependency.
typedef AmbientMediaLister =
    Future<List<PhotoItem>> Function({required String accessToken});

/// Loads the images in a set of Drive folder IDs using an OAuth access token.
/// Implemented by `listDrivePhotos` in `drive_photos.dart`; injected so
/// [DrivePhotoSource] and [photoSourceFromSettings] carry no network dependency.
typedef DrivePhotoLister =
    Future<List<PhotoItem>> Function({
      required String accessToken,
      required List<String> folderIds,
    });

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
      PhotoItem.gradient([
        Color(0xFF243B55),
        Color(0xFF141E30),
      ], caption: 'Dusk'),
      PhotoItem.gradient([
        Color(0xFF3A1C71),
        Color(0xFF161226),
      ], caption: 'Aurora'),
      PhotoItem.gradient([
        Color(0xFF0F2027),
        Color(0xFF203A43),
      ], caption: 'Deep'),
      PhotoItem.gradient([
        Color(0xFF42275A),
        Color(0xFF1B1035),
      ], caption: 'Twilight'),
      PhotoItem.gradient([
        Color(0xFF16222A),
        Color(0xFF3A6073),
      ], caption: 'Slate'),
    ];
  }
}

/// On-device Google Photos source via the **Ambient API** (the purpose-built API
/// for ambient devices). Reads the media items the user selected in the Google
/// Photos app for this device, via the injected [lister]. Throws (→ caller falls
/// back to [LocalPhotoSource]) when there's no token.
class AmbientPhotoSource implements PhotoSource {
  const AmbientPhotoSource({required this.accessToken, required this.lister});

  /// OAuth access token (minted on boot from the persisted refresh token).
  final String accessToken;

  /// Injected media loader (see `AmbientApiClient.listMediaItems`).
  final AmbientMediaLister lister;

  @override
  String get label => 'Google Photos';

  @override
  Future<List<PhotoItem>> loadPhotos() async {
    if (accessToken.isEmpty) {
      throw StateError('Google Photos not linked — link it in settings');
    }
    return lister(accessToken: accessToken);
  }
}

/// On-device Google **Drive** source (interim, while the Ambient API is
/// partner-gated). Lists every image in the configured [folderIds] via the injected
/// [lister]. Throws (→ caller falls back to [LocalPhotoSource]) when there's no token
/// or no folder configured.
class DrivePhotoSource implements PhotoSource {
  const DrivePhotoSource({
    required this.accessToken,
    required this.folderIds,
    required this.lister,
  });

  /// OAuth access token (minted on boot from the persisted Drive refresh token).
  final String accessToken;

  /// Drive folder IDs to pull images from.
  final List<String> folderIds;

  /// Injected Drive image loader (see `listDrivePhotos`).
  final DrivePhotoLister lister;

  @override
  String get label => 'Google Drive';

  @override
  Future<List<PhotoItem>> loadPhotos() async {
    if (accessToken.isEmpty) {
      throw StateError('Google Drive not linked — link it in settings');
    }
    if (folderIds.isEmpty) {
      throw StateError('No Drive folder configured');
    }
    return lister(accessToken: accessToken, folderIds: folderIds);
  }
}

/// Build the [PhotoSource] the slideshow should use for the given [settings], based
/// on the selected [PhotoSourceKind]. Falls back to [LocalPhotoSource] whenever the
/// chosen Google source isn't linked or has no live access token, so the idle screen
/// is always populated. Access tokens are minted from the persisted refresh tokens
/// on boot; the listers are the concrete Ambient / Drive loaders.
PhotoSource photoSourceFromSettings(
  AppSettings settings, {
  required AmbientMediaLister ambientLister,
  required DrivePhotoLister driveLister,
  String? ambientAccessToken,
  String? driveAccessToken,
}) {
  switch (settings.photoSource) {
    case PhotoSourceKind.ambient:
      if (settings.ambientLinked &&
          ambientAccessToken != null &&
          ambientAccessToken.isNotEmpty) {
        return AmbientPhotoSource(
          accessToken: ambientAccessToken,
          lister: ambientLister,
        );
      }
    case PhotoSourceKind.drive:
      if (settings.driveLinked &&
          driveAccessToken != null &&
          driveAccessToken.isNotEmpty &&
          settings.driveFolderIds.isNotEmpty) {
        return DrivePhotoSource(
          accessToken: driveAccessToken,
          folderIds: settings.driveFolderIds,
          lister: driveLister,
        );
      }
    case PhotoSourceKind.local:
      break;
  }
  return const LocalPhotoSource();
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
  PhotoItem? get current =>
      _photos.isEmpty ? null : _photos[_index % _photos.length];

  int get index => _index;

  /// Load photos and begin cycling. Falls back to local ambient gradients if the
  /// configured source throws (offline / not authorized).
  Future<void> start() async {
    await _reload();
    _restartTimer();
  }

  void _restartTimer() {
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

  /// Advance to the next slide (wraps around). [userInitiated] (e.g. a swipe) resets
  /// the auto-advance timer so it waits a full interval after manual navigation.
  void next({bool userInitiated = false}) {
    if (_photos.isEmpty || _disposed) return;
    _index = (_index + 1) % _photos.length;
    if (userInitiated) _restartTimer();
    notifyListeners();
  }

  /// Go back to the previous slide (wraps around). See [next] re [userInitiated].
  void previous({bool userInitiated = false}) {
    if (_photos.isEmpty || _disposed) return;
    _index = (_index - 1 + _photos.length) % _photos.length;
    if (userInitiated) _restartTimer();
    notifyListeners();
  }

  @override
  void dispose() {
    _disposed = true;
    _timer?.cancel();
    super.dispose();
  }
}
