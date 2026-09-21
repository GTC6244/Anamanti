// Import a Google Drive refresh token synced from the Mac consent helper.
//
// Drive's `drive.readonly` scope can't be granted on-device (the device-code/QR
// flow rejects Drive scopes), so consent runs once on the Mac
// (`tools/google_photo_consent.py`) and the resulting refresh token (+ folder IDs)
// is adb-pushed to the app's external files dir. The settings screen reads it here
// and stores it in [AppSettings]; the device then refreshes + reads Drive directly.

import 'dart:convert';
import 'dart:io';

import 'package:path_provider/path_provider.dart';

/// File name the Mac helper writes and adb-pushes to the app's external files dir
/// (`/sdcard/Android/data/<pkg>/files/`), readable without extra permissions.
const String kDriveTokenFileName = 'google_drive_token.json';

/// A refresh token (and optional folder IDs) imported from the synced file.
class ImportedDriveToken {
  const ImportedDriveToken({
    required this.refreshToken,
    this.folderIds = const [],
  });
  final String refreshToken;
  final List<String> folderIds;
}

/// Locate + parse the synced token file, or null if it's absent/invalid. Checks the
/// external files dir first (where adb push lands) then app support as a fallback.
Future<ImportedDriveToken?> importDriveTokenFromFile() async {
  for (final dir in await _candidateDirs()) {
    final file = File('${dir.path}/$kDriveTokenFileName');
    try {
      if (!await file.exists()) continue;
      final decoded = jsonDecode(await file.readAsString());
      if (decoded is! Map) continue;
      final rt = decoded['refresh_token'];
      if (rt is! String || rt.isEmpty) continue;
      final ids = decoded['folder_ids'] is List
          ? (decoded['folder_ids'] as List)
                .whereType<String>()
                .map((s) => s.trim())
                .where((s) => s.isNotEmpty)
                .toList()
          : const <String>[];
      return ImportedDriveToken(refreshToken: rt, folderIds: ids);
    } catch (_) {
      continue;
    }
  }
  return null;
}

Future<List<Directory>> _candidateDirs() async {
  final dirs = <Directory>[];
  try {
    final ext = await getExternalStorageDirectory(); // Android only
    if (ext != null) dirs.add(ext);
  } catch (_) {
    // Not Android / unavailable — skip.
  }
  try {
    dirs.add(await getApplicationSupportDirectory());
  } catch (_) {
    // Unavailable in some test contexts — skip.
  }
  return dirs;
}
