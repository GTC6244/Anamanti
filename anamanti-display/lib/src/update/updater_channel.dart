// Platform channel for the NATIVE half of the in-app updater
// (plans/UpdaterPlan.md).
//
// The network work — fetching `latest.json`, downloading the APK, verifying its
// SHA-256 — lives in Rust (behind FRB; see `lib/src/rust/api/updater.dart`). This
// channel covers only the parts that must be Android-native: reading the running
// app's versionCode, the unknown-sources permission gate, and committing the
// `PackageInstaller` session. It mirrors `anamanti_display/brightness`
// (`screen_brightness.dart`) and is injectable so unit tests can fake it.

import 'package:flutter/services.dart';

/// Called when a terminal install result arrives from the native side.
typedef InstallStatusHandler = void Function(bool success, String message);

/// Thin wrapper over the `anamanti_display/updater` MethodChannel.
class UpdaterChannel {
  UpdaterChannel({MethodChannel? channel})
    : _channel = channel ?? const MethodChannel('anamanti_display/updater');

  final MethodChannel _channel;

  /// Whether this build ships the self-updater. False on the `fdroid` flavor — Dart
  /// then hides all updater UI. Defaults to false if the channel is unavailable
  /// (e.g. host tests), so nothing updater-related shows outside a real device.
  Future<bool> isSelfUpdateEnabled() async =>
      (await _invoke<bool>('isSelfUpdateEnabled')) ?? false;

  /// The running app's Android versionCode (compared against `latest.json`).
  Future<int> getVersionCode() async => (await _invoke<int>('getVersionCode')) ?? 0;

  /// Whether the user has granted this app "install unknown apps".
  Future<bool> canInstallPackages() async =>
      (await _invoke<bool>('canInstallPackages')) ?? false;

  /// Open the per-app unknown-sources settings screen.
  Future<void> openInstallSettings() => _invoke<void>('openInstallSettings');

  /// Hand a downloaded+verified APK to the system installer. The terminal result
  /// arrives asynchronously via [setInstallStatusHandler].
  Future<void> installApk(String path) =>
      _invoke<void>('installApk', <String, Object?>{'path': path});

  /// Register (or clear, with null) the native→Dart `installStatus` callback.
  void setInstallStatusHandler(InstallStatusHandler? handler) {
    if (handler == null) {
      _channel.setMethodCallHandler(null);
      return;
    }
    _channel.setMethodCallHandler((MethodCall call) async {
      if (call.method == 'installStatus') {
        final args = (call.arguments as Map).cast<Object?, Object?>();
        handler(args['success'] == true, (args['message'] as String?) ?? '');
      }
      return null;
    });
  }

  Future<T?> _invoke<T>(String method, [Object? args]) async {
    try {
      return await _channel.invokeMethod<T>(method, args);
    } on MissingPluginException {
      // No native side (host tests / non-Android): treat as absent/unsupported.
      return null;
    }
  }
}
