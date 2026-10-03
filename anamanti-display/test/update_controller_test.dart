// Tests for the in-app updater controller (plans/UpdaterPlan.md).
//
// Drives UpdateController with a fake native channel and injected check/download
// functions — no FRB, no real network, no device.

import 'dart:async';

import 'package:anamanti_display/src/engine/update_controller.dart';
import 'package:anamanti_display/src/rust/api/updater.dart';
import 'package:anamanti_display/src/update/updater_channel.dart';
import 'package:flutter_test/flutter_test.dart';

/// A fully in-memory stand-in for the native updater channel.
class FakeUpdaterChannel extends UpdaterChannel {
  FakeUpdaterChannel({
    this.versionCode = 1,
    this.selfUpdateEnabled = true,
    this.canInstall = true,
  });

  int versionCode;
  bool selfUpdateEnabled;
  bool canInstall;

  int installCalls = 0;
  int openSettingsCalls = 0;
  String? installedPath;
  InstallStatusHandler? handler;

  @override
  Future<bool> isSelfUpdateEnabled() async => selfUpdateEnabled;

  @override
  Future<int> getVersionCode() async => versionCode;

  @override
  Future<bool> canInstallPackages() async => canInstall;

  @override
  Future<void> openInstallSettings() async => openSettingsCalls++;

  @override
  Future<void> installApk(String path) async {
    installCalls++;
    installedPath = path;
  }

  @override
  void setInstallStatusHandler(InstallStatusHandler? h) => handler = h;
}

UpdateManifest _manifest({int versionCode = 12, String notes = 'notes'}) =>
    UpdateManifest(
      versionCode: versionCode,
      versionName: '1.2.0',
      apkUrl: 'https://dl.example.com/app.apk',
      sha256: 'deadbeef',
      notes: notes,
    );

DownloadProgress _progress({
  int downloaded = 0,
  int total = 0,
  bool done = false,
  String error = '',
}) => DownloadProgress(
  downloaded: downloaded,
  total: total,
  done: done,
  error: error,
);

void main() {
  test('a newer versionCode becomes available (and raises the banner)', () async {
    final channel = FakeUpdaterChannel(versionCode: 5);
    final c = UpdateController(
      channel: channel,
      baseUrl: 'https://x',
      check: (_) async => _manifest(versionCode: 12),
      download: ({required apkUrl, required expectedSha256, required destPath}) =>
          const Stream.empty(),
      cacheDir: () async => '/tmp',
    );
    addTearDown(c.dispose);

    await c.start();

    expect(c.status, UpdateStatus.available);
    expect(c.manifest?.versionCode, 12);
    expect(c.bannerVisible, isTrue);
  });

  test('an equal/older versionCode is up to date (no banner)', () async {
    final channel = FakeUpdaterChannel(versionCode: 12);
    final c = UpdateController(
      channel: channel,
      baseUrl: 'https://x',
      check: (_) async => _manifest(versionCode: 12),
      download: ({required apkUrl, required expectedSha256, required destPath}) =>
          const Stream.empty(),
      cacheDir: () async => '/tmp',
    );
    addTearDown(c.dispose);

    await c.start();

    expect(c.status, UpdateStatus.upToDate);
    expect(c.bannerVisible, isFalse);
  });

  test('a background check failure does NOT raise the banner', () async {
    final channel = FakeUpdaterChannel(versionCode: 5);
    final c = UpdateController(
      channel: channel,
      baseUrl: 'https://x',
      check: (_) async => throw Exception('unreachable'),
      download: ({required apkUrl, required expectedSha256, required destPath}) =>
          const Stream.empty(),
      cacheDir: () async => '/tmp',
    );
    addTearDown(c.dispose);

    await c.start();

    expect(c.status, UpdateStatus.error);
    expect(c.bannerVisible, isFalse, reason: 'silent background check failure');
  });

  test('download → verify → install, then success resets to up to date', () async {
    final stream = StreamController<DownloadProgress>();
    addTearDown(stream.close);
    final channel = FakeUpdaterChannel(versionCode: 5, canInstall: true);
    final c = UpdateController(
      channel: channel,
      baseUrl: 'https://x',
      check: (_) async => _manifest(versionCode: 12),
      download: ({required apkUrl, required expectedSha256, required destPath}) =>
          stream.stream,
      cacheDir: () async => '/tmp',
      fileExists: (_) async => true,
    );
    addTearDown(c.dispose);

    await c.start();
    expect(c.status, UpdateStatus.available);

    await c.download();
    stream.add(_progress(downloaded: 50, total: 100));
    await Future<void>.delayed(Duration.zero);
    expect(c.status, UpdateStatus.downloading);
    expect(c.progress, closeTo(0.5, 1e-9));

    stream.add(_progress(done: true));
    await Future<void>.delayed(Duration.zero);
    // done → install() runs; canInstall is true so it hands off to the installer.
    expect(channel.installCalls, 1);
    expect(channel.installedPath, '/tmp/anamanti-update-12.apk');

    // The native side reports success → clean reset.
    channel.handler?.call(true, '');
    expect(c.status, UpdateStatus.upToDate);
  });

  test('a hash/download error surfaces and keeps the banner (in-flow)', () async {
    final stream = StreamController<DownloadProgress>();
    addTearDown(stream.close);
    final channel = FakeUpdaterChannel(versionCode: 5);
    final c = UpdateController(
      channel: channel,
      baseUrl: 'https://x',
      check: (_) async => _manifest(versionCode: 12),
      download: ({required apkUrl, required expectedSha256, required destPath}) =>
          stream.stream,
      cacheDir: () async => '/tmp',
    );
    addTearDown(c.dispose);

    await c.start();
    await c.download();
    stream.add(_progress(error: 'sha256 mismatch'));
    await Future<void>.delayed(Duration.zero);

    expect(c.status, UpdateStatus.error);
    expect(c.errorMessage, contains('sha256 mismatch'));
    expect(c.bannerVisible, isTrue, reason: 'error during an active flow');
  });

  test('missing install permission opens settings instead of installing', () async {
    final stream = StreamController<DownloadProgress>();
    addTearDown(stream.close);
    final channel = FakeUpdaterChannel(versionCode: 5, canInstall: false);
    final c = UpdateController(
      channel: channel,
      baseUrl: 'https://x',
      check: (_) async => _manifest(versionCode: 12),
      download: ({required apkUrl, required expectedSha256, required destPath}) =>
          stream.stream,
      cacheDir: () async => '/tmp',
      fileExists: (_) async => true,
    );
    addTearDown(c.dispose);

    await c.start();
    await c.download();
    stream.add(_progress(done: true));
    await Future<void>.delayed(Duration.zero);

    expect(channel.openSettingsCalls, 1);
    expect(channel.installCalls, 0);
    expect(c.status, UpdateStatus.readyToInstall);
  });
}
