// Unit tests for the device-local settings model + persistence (Phase 6).
//
// These need no platform channels: [SettingsStore] takes an injected temp-file
// resolver, so the round trip is exercised against a real file with no
// `path_provider`.

import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

import 'package:anamanti_display/src/settings/app_settings.dart';
import 'package:anamanti_display/src/settings/settings_store.dart';

void main() {
  late Directory tempDir;
  late SettingsStore store;

  setUp(() async {
    tempDir = await Directory.systemTemp.createTemp('ambient_settings_test');
    store = SettingsStore(
      fileResolver: () async => File('${tempDir.path}/settings.json'),
    );
  });

  tearDown(() async {
    if (await tempDir.exists()) await tempDir.delete(recursive: true);
  });

  test('load returns defaults when no file exists', () async {
    final loaded = await store.load();
    expect(loaded, const AppSettings());
    expect(loaded.wakeWord, 'hey_jarvis');
    expect(loaded.photoSource, PhotoSourceKind.local);
  });

  test('save then load round-trips every field', () async {
    const settings = AppSettings(
      orchestratorKey: 'test-mac',
      wakeWord: 'hey_jarvis',
      threshold: 0.4,
      activeThreshold: 0.8,
      dimDelaySecs: 45,
      photoSource: PhotoSourceKind.drive,
      ambientRefreshToken: 'refresh-abc',
      ambientDeviceId: 'dev-77',
      ambientLinked: true,
      driveRefreshToken: 'drive-refresh',
      driveFolderIds: ['fld-1', 'fld-2'],
      driveLinked: true,
    );
    await store.save(settings);
    final loaded = await store.load();
    expect(loaded, settings);
  });

  test('fromJson tolerates missing and invalid fields', () {
    final loaded = AppSettings.fromJson({
      'wakeWord': '',
      'threshold': 'not-a-number',
      'photoSource': 'bogus',
    });
    // Falls back to defaults rather than throwing.
    expect(loaded.wakeWord, 'hey_jarvis');
    expect(loaded.threshold, 0.5);
    expect(loaded.photoSource, PhotoSourceKind.local);
  });

  test('load recovers from a corrupt file', () async {
    final file = File('${tempDir.path}/settings.json');
    await file.writeAsString('{ this is not valid json');
    final loaded = await store.load();
    expect(loaded, const AppSettings());
  });

  test('orchestratorKey round-trips and legacy JSON defaults to Auto', () {
    // A persisted selection survives a JSON round trip.
    final withKey = AppSettings.fromJson(
      const AppSettings(orchestratorKey: 'mac-mini').toJson(),
    );
    expect(withKey.orchestratorKey, 'mac-mini');

    // A pre-feature settings file (no orchestratorKey) defaults to '' = Auto.
    final legacy = AppSettings.fromJson({'wakeWord': 'hey_jarvis'});
    expect(legacy.orchestratorKey, '');
  });

  test('dimDelaySecs defaults to 300 (5 min) and clamps out-of-range values', () {
    expect(const AppSettings().dimDelaySecs, 300);
    // Below the floor / above the ceiling clamp; a non-numeric value falls back.
    expect(AppSettings.fromJson({'dimDelaySecs': 1}).dimDelaySecs, 30);
    expect(AppSettings.fromJson({'dimDelaySecs': 99999}).dimDelaySecs, 3600);
    expect(AppSettings.fromJson({'dimDelaySecs': 'nope'}).dimDelaySecs, 300);
    // A legacy file with no dim delay keeps the default.
    expect(AppSettings.fromJson({'wakeWord': 'hey_jarvis'}).dimDelaySecs, 300);
  });

  test('copyWith changes only the given fields', () {
    const base = AppSettings();
    final next = base.copyWith(wakeWord: 'ok_nabu', threshold: 0.6);
    expect(next.wakeWord, 'ok_nabu');
    expect(next.threshold, 0.6);
    expect(next.activeThreshold, base.activeThreshold);
    expect(next.photoSource, base.photoSource);
  });
}
