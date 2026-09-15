// Unit tests for the device-local settings model + persistence (Phase 6).
//
// These need no platform channels: [SettingsStore] takes an injected temp-file
// resolver, so the round trip is exercised against a real file with no
// `path_provider`.

import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

import 'package:ambient_display/src/settings/app_settings.dart';
import 'package:ambient_display/src/settings/settings_store.dart';

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
    expect(loaded.wakeWord, 'alexa');
    expect(loaded.photoSource, PhotoSourceKind.local);
  });

  test('save then load round-trips every field', () async {
    const settings = AppSettings(
      wakeWord: 'hey_jarvis',
      threshold: 0.4,
      activeThreshold: 0.8,
      photoSource: PhotoSourceKind.google,
      googleFolderName: 'Family',
      googleLinked: true,
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
    expect(loaded.wakeWord, 'alexa');
    expect(loaded.threshold, 0.5);
    expect(loaded.photoSource, PhotoSourceKind.local);
  });

  test('load recovers from a corrupt file', () async {
    final file = File('${tempDir.path}/settings.json');
    await file.writeAsString('{ this is not valid json');
    final loaded = await store.load();
    expect(loaded, const AppSettings());
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
