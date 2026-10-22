import 'package:shared_preferences/shared_preferences.dart';

/// Persists which Core the user picked (its stable TXT `instance_id`), so a
/// household with more than one Core isn't asked every launch.
abstract class SelectionStore {
  Future<String?> load();
  Future<void> save(String key);
  Future<void> clear();
}

class PrefsSelectionStore implements SelectionStore {
  static const _key = 'selected_core_instance_id';

  @override
  Future<String?> load() async =>
      (await SharedPreferences.getInstance()).getString(_key);

  @override
  Future<void> save(String key) async =>
      (await SharedPreferences.getInstance()).setString(_key, key);

  @override
  Future<void> clear() async =>
      (await SharedPreferences.getInstance()).remove(_key);
}
