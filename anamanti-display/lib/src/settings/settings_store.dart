// Persistence for the device-local [AppSettings] (Plan.MD §3, Phase 6).
//
// Settings are stored as a small JSON file in the app-support directory (the same
// place the wake-word models live). A JSON file — rather than a plugin like
// shared_preferences — keeps persistence dependency-free and trivially testable:
// tests inject a temp-file resolver and never touch platform channels.

import 'dart:convert';
import 'dart:io';

import 'package:path_provider/path_provider.dart';

import 'package:anamanti_display/src/settings/app_settings.dart';

/// Loads and saves [AppSettings] to a JSON file. The file location is injectable so
/// tests can point at a temp file with no `path_provider` platform channel.
class SettingsStore {
  SettingsStore({Future<File> Function()? fileResolver})
      : _fileResolver = fileResolver ?? _defaultFile;

  final Future<File> Function() _fileResolver;

  static Future<File> _defaultFile() async {
    final dir = await getApplicationSupportDirectory();
    return File('${dir.path}/settings.json');
  }

  /// Load persisted settings, or defaults if none are stored yet or the file is
  /// unreadable/corrupt (a bad file never blocks startup).
  Future<AppSettings> load() async {
    try {
      final file = await _fileResolver();
      if (!await file.exists()) return const AppSettings();
      final raw = await file.readAsString();
      if (raw.trim().isEmpty) return const AppSettings();
      final decoded = jsonDecode(raw);
      if (decoded is! Map<String, dynamic>) return const AppSettings();
      return AppSettings.fromJson(decoded);
    } catch (_) {
      return const AppSettings();
    }
  }

  /// Persist [settings], creating the file (and parent directory) if needed.
  Future<void> save(AppSettings settings) async {
    final file = await _fileResolver();
    await file.parent.create(recursive: true);
    await file.writeAsString(jsonEncode(settings.toJson()), flush: true);
  }
}
