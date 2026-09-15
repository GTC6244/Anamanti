// An in-memory [SettingsStore] for widget tests: overrides load/save so the
// settings screen never touches dart:io. That keeps `pumpAndSettle` reliable
// (fake-async can't drive real file IO) while still verifying that Save persists.

import 'package:ambient_display/src/settings/app_settings.dart';
import 'package:ambient_display/src/settings/settings_store.dart';

class InMemorySettingsStore extends SettingsStore {
  InMemorySettingsStore([this.value = const AppSettings()]);

  AppSettings value;

  @override
  Future<AppSettings> load() async => value;

  @override
  Future<void> save(AppSettings settings) async => value = settings;
}
