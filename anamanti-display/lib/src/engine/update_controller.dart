// In-app updater controller (plans/UpdaterPlan.md).
//
// Orchestrates the self-update flow for the Echo Show kiosk: on boot and on a
// periodic timer it asks the Rust engine to fetch `latest.json` from the configured
// Cloudflare R2 base URL, compares the published versionCode against the running
// app's (read via the native updater channel), and — when newer — surfaces an
// "update available" state the UI renders as a banner + a Settings page. The user
// then downloads (Rust streams the APK to cache, verifying its SHA-256) and installs
// (the native `PackageInstaller` session).
//
// It is a [ChangeNotifier] sidecar, independent of the voice-turn lifecycle (mirrors
// `NotificationController`), and fully injectable so unit tests drive it with no
// native channel and no network.

import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:path_provider/path_provider.dart';

import 'package:anamanti_display/src/rust/api/updater.dart';
import 'package:anamanti_display/src/update/updater_channel.dart';

/// Where the flow currently is.
enum UpdateStatus {
  /// Nothing to do yet (not checked, or up to date and dismissed).
  idle,

  /// A check is in flight.
  checking,

  /// Checked; the running build is current.
  upToDate,

  /// A newer build is published (see [UpdateController.manifest]).
  available,

  /// The APK is downloading (see [UpdateController.progress]).
  downloading,

  /// The APK downloaded and verified; ready to hand to the installer.
  readyToInstall,

  /// The `PackageInstaller` session is committing / awaiting the user prompt.
  installing,

  /// Something failed (see [UpdateController.errorMessage]).
  error,
}

/// Fetch the manifest for a base URL (defaults to the Rust FRB call).
typedef CheckForUpdateFn = Future<UpdateManifest> Function(String baseUrl);

/// Stream a verified download (defaults to the Rust FRB call).
typedef DownloadUpdateFn =
    Stream<DownloadProgress> Function({
      required String apkUrl,
      required String expectedSha256,
      required String destPath,
    });

/// Resolve a writable cache directory for the downloaded APK.
typedef CacheDirProvider = Future<String> Function();

class UpdateController extends ChangeNotifier {
  UpdateController({
    required UpdaterChannel channel,
    required String baseUrl,
    bool autoUpdateEnabled = true,
    CheckForUpdateFn? check,
    DownloadUpdateFn? download,
    CacheDirProvider? cacheDir,
    Duration checkInterval = const Duration(hours: 6),
    // These public named params are assigned to private fields; an initializing
    // formal would make them unusable private named parameters (same reason as
    // NotificationController). Assign here and silence the lint.
    // ignore: prefer_initializing_formals
  }) : _channel = channel,
       // ignore: prefer_initializing_formals
       _baseUrl = baseUrl,
       // ignore: prefer_initializing_formals
       _autoUpdateEnabled = autoUpdateEnabled,
       _check = check ?? ((b) => checkForUpdate(baseUrl: b)),
       _download = download ?? downloadUpdate,
       _cacheDir = cacheDir ?? _defaultCacheDir,
       // ignore: prefer_initializing_formals
       _checkInterval = checkInterval;

  static Future<String> _defaultCacheDir() async =>
      (await getTemporaryDirectory()).path;

  final UpdaterChannel _channel;
  final CheckForUpdateFn _check;
  final DownloadUpdateFn _download;
  final CacheDirProvider _cacheDir;
  final Duration _checkInterval;

  String _baseUrl;
  bool _autoUpdateEnabled;

  Timer? _timer;
  StreamSubscription<DownloadProgress>? _downloadSub;
  bool _started = false;

  UpdateStatus _status = UpdateStatus.idle;
  UpdateManifest? _manifest;
  int _currentVersionCode = 0;
  int _downloaded = 0;
  int _total = 0;
  String _errorMessage = '';
  String? _downloadedPath;
  bool _dismissed = false;

  /// True once the user has opted into downloading/installing. Lets the banner show
  /// download/install progress (and errors) without nagging about silent background
  /// *check* failures — important because the default placeholder base URL will
  /// fail every check until it's configured.
  bool _inFlow = false;

  UpdateStatus get status => _status;
  UpdateManifest? get manifest => _manifest;
  int get currentVersionCode => _currentVersionCode;
  String get errorMessage => _errorMessage;
  bool get autoUpdateEnabled => _autoUpdateEnabled;
  String get baseUrl => _baseUrl;

  /// Download progress in [0, 1], or null when the total size is unknown.
  double? get progress => _total > 0 ? (_downloaded / _total).clamp(0.0, 1.0) : null;

  int get downloadedBytes => _downloaded;
  int get totalBytes => _total;

  /// Whether the update banner should show: a fresh "available" prompt, or the
  /// active download/install flow (incl. an error during it). Silent background
  /// *check* failures never raise the banner.
  bool get bannerVisible {
    if (_dismissed) return false;
    switch (_status) {
      case UpdateStatus.available:
        return true;
      case UpdateStatus.downloading:
      case UpdateStatus.readyToInstall:
      case UpdateStatus.installing:
        return true;
      case UpdateStatus.error:
        return _inFlow;
      case UpdateStatus.idle:
      case UpdateStatus.checking:
      case UpdateStatus.upToDate:
        return false;
    }
  }

  /// Begin auto-checking: read the current versionCode, wire the install-status
  /// callback, do one check now, and schedule the periodic timer. Idempotent.
  Future<void> start() async {
    if (_started) return;
    _started = true;
    _channel.setInstallStatusHandler(_onInstallStatus);
    _currentVersionCode = await _channel.getVersionCode();
    if (_autoUpdateEnabled) {
      await checkNow();
      _timer = Timer.periodic(_checkInterval, (_) => checkNow());
    }
  }

  /// Apply updated device settings (base URL / auto-update toggle). Re-checks if
  /// auto-update was just turned on.
  Future<void> updateConfig({
    required String baseUrl,
    required bool autoUpdateEnabled,
  }) async {
    final wasEnabled = _autoUpdateEnabled;
    _baseUrl = baseUrl;
    _autoUpdateEnabled = autoUpdateEnabled;
    if (!autoUpdateEnabled) {
      _timer?.cancel();
      _timer = null;
    } else if (!wasEnabled && _started) {
      await checkNow();
      _timer ??= Timer.periodic(_checkInterval, (_) => checkNow());
    }
    notifyListeners();
  }

  /// Fetch `latest.json` and decide whether a newer build exists. Safe to call
  /// manually ("Check now") regardless of the auto-update toggle.
  Future<void> checkNow() async {
    if (_status == UpdateStatus.downloading ||
        _status == UpdateStatus.installing) {
      return;
    }
    _inFlow = false;
    _setStatus(UpdateStatus.checking);
    try {
      if (_currentVersionCode == 0) {
        _currentVersionCode = await _channel.getVersionCode();
      }
      final manifest = await _check(_baseUrl);
      final latest = manifest.versionCode.toInt();
      if (latest > _currentVersionCode) {
        _manifest = manifest;
        _dismissed = false;
        _setStatus(UpdateStatus.available);
      } else {
        _manifest = null;
        _setStatus(UpdateStatus.upToDate);
      }
    } catch (e) {
      _fail('Could not check for updates: $e');
    }
  }

  /// Download + verify the available update, then kick off installation.
  Future<void> download() async {
    final manifest = _manifest;
    if (manifest == null) return;
    _inFlow = true;
    await _downloadSub?.cancel();
    _downloaded = 0;
    _total = 0;
    _downloadedPath = null;
    _setStatus(UpdateStatus.downloading);

    final dir = await _cacheDir();
    final dest = '$dir/anamanti-update-${manifest.versionCode}.apk';
    _downloadSub = _download(
      apkUrl: manifest.apkUrl,
      expectedSha256: manifest.sha256,
      destPath: dest,
    ).listen(
      (p) {
        if (p.error.isNotEmpty) {
          _fail('Download failed: ${p.error}');
          return;
        }
        if (p.done) {
          _downloadedPath = dest;
          _setStatus(UpdateStatus.readyToInstall);
          unawaited(install());
          return;
        }
        _downloaded = p.downloaded.toInt();
        _total = p.total.toInt();
        notifyListeners();
      },
      onError: (Object e, StackTrace _) => _fail('Download failed: $e'),
      cancelOnError: true,
    );
  }

  /// Hand the verified APK to the system installer. If the user hasn't granted
  /// "install unknown apps" yet, open that settings screen and stay in
  /// [UpdateStatus.readyToInstall] so they can retry after granting.
  Future<void> install() async {
    final path = _downloadedPath;
    if (path == null) return;
    if (!await _channel.canInstallPackages()) {
      await _channel.openInstallSettings();
      _setStatus(UpdateStatus.readyToInstall);
      return;
    }
    _setStatus(UpdateStatus.installing);
    try {
      await _channel.installApk(path);
      // Terminal result arrives via _onInstallStatus.
    } catch (e) {
      _fail('Install failed: $e');
    }
  }

  /// Retry after an error: re-install if the APK already downloaded, otherwise
  /// restart the download.
  Future<void> retry() async {
    if (_downloadedPath != null) {
      await install();
    } else {
      await download();
    }
  }

  /// Cancel an in-flight download and reset to idle.
  Future<void> cancel() async {
    await _downloadSub?.cancel();
    _downloadSub = null;
    try {
      await cancelDownload();
    } catch (_) {
      // Best-effort.
    }
    _inFlow = false;
    _setStatus(UpdateStatus.idle);
  }

  /// Dismiss the "available" banner until the next check finds it again.
  void dismissBanner() {
    _dismissed = true;
    notifyListeners();
  }

  void _onInstallStatus(bool success, String message) {
    if (success) {
      // The new APK is installing; the OS will relaunch us. Reset to a clean state.
      _manifest = null;
      _downloadedPath = null;
      _inFlow = false;
      _setStatus(UpdateStatus.upToDate);
    } else {
      _fail(message.isEmpty ? 'Install failed' : message);
    }
  }

  void _setStatus(UpdateStatus status) {
    _status = status;
    if (status != UpdateStatus.error) _errorMessage = '';
    notifyListeners();
  }

  void _fail(String message) {
    _errorMessage = message;
    _status = UpdateStatus.error;
    notifyListeners();
  }

  @override
  void dispose() {
    _timer?.cancel();
    _downloadSub?.cancel();
    _channel.setInstallStatusHandler(null);
    super.dispose();
  }
}
