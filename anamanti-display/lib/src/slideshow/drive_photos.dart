// Google Drive image listing for the photo slideshow (interim source while the
// Google Photos Ambient API is partner-gated).
//
// Lists every image in the configured Drive folder IDs and turns each into a
// [PhotoItem] whose URL is the file's `?alt=media` download, fetched with the
// account's bearer token. Works for folders the linked account owns *and* folders
// shared into it ("Shared with me"): `corpora=user` covers both. Needs the
// `drive.readonly` scope, which can only be granted via off-device (Mac) consent —
// the device just refreshes the resulting token.
//
// Matches the [DrivePhotoLister] typedef; networking is injectable for offline
// unit tests.

import 'dart:convert';

import 'package:http/http.dart' as http;

import 'package:anamanti_display/src/slideshow/ambient_photos.dart'
    show AmbientApiException;
import 'package:anamanti_display/src/slideshow/photo_source.dart';

const String _kDriveFilesEndpoint = 'https://www.googleapis.com/drive/v3/files';

/// A Drive folder the user can choose from in the settings picker.
class DriveFolder {
  const DriveFolder({
    required this.id,
    required this.name,
    this.shared = false,
  });
  final String id;
  final String name;

  /// True if the folder is shared with the user (not owned by them).
  final bool shared;
}

/// List the account's Drive folders (owned + "Shared with me") so the user can pick
/// which to show, without hunting for folder IDs. Needs `drive.readonly`. Throws
/// [AmbientApiException] on an HTTP error.
Future<List<DriveFolder>> listDriveFolders({
  required String accessToken,
  http.Client? httpClient,
  String filesEndpoint = _kDriveFilesEndpoint,
  int maxFolders = 500,
}) async {
  final client = httpClient ?? http.Client();
  final authHeader = <String, String>{'Authorization': 'Bearer $accessToken'};
  final folders = <DriveFolder>[];
  try {
    String? pageToken;
    do {
      final params = <String, String>{
        'q':
            "mimeType = 'application/vnd.google-apps.folder' and trashed = false",
        'fields': 'nextPageToken,files(id,name,shared,ownedByMe)',
        'pageSize': '100',
        'orderBy': 'name',
        'corpora': 'user',
        'supportsAllDrives': 'true',
        'includeItemsFromAllDrives': 'true',
      };
      if (pageToken != null) params['pageToken'] = pageToken;
      final uri = Uri.parse(filesEndpoint).replace(queryParameters: params);
      final http.Response resp;
      try {
        resp = await client.get(uri, headers: authHeader);
      } catch (e) {
        throw AmbientApiException('Could not reach Google Drive: $e');
      }
      if (resp.statusCode != 200) {
        throw AmbientApiException(
          'Drive folder list failed (${resp.statusCode}).',
        );
      }
      final json = jsonDecode(resp.body);
      final files = (json is Map && json['files'] is List)
          ? json['files'] as List
          : const [];
      for (final f in files) {
        if (f is! Map) continue;
        final id = f['id'] as String?;
        if (id == null) continue;
        folders.add(
          DriveFolder(
            id: id,
            name: (f['name'] as String?) ?? id,
            shared: f['ownedByMe'] == false || f['shared'] == true,
          ),
        );
        if (folders.length >= maxFolders) return folders;
      }
      pageToken = (json is Map ? json['nextPageToken'] : null) as String?;
    } while (pageToken != null);
  } finally {
    if (httpClient == null) client.close();
  }
  return folders;
}

/// List images across [folderIds] as slideshow [PhotoItem]s. Throws
/// [AmbientApiException] on an HTTP error (e.g. an expired/invalid token) so the
/// slideshow controller falls back to local gradients.
Future<List<PhotoItem>> listDrivePhotos({
  required String accessToken,
  required List<String> folderIds,
  http.Client? httpClient,
  String filesEndpoint = _kDriveFilesEndpoint,
  int maxPerFolder = 100,
  int maxDimension = 1600,
}) async {
  final client = httpClient ?? http.Client();
  final authHeader = <String, String>{'Authorization': 'Bearer $accessToken'};
  final items = <PhotoItem>[];
  try {
    for (final rawId in folderIds) {
      final folderId = rawId.trim();
      if (folderId.isEmpty) continue;
      String? pageToken;
      var fetched = 0;
      do {
        final params = <String, String>{
          'q':
              "'$folderId' in parents and mimeType contains 'image/' and trashed = false",
          'fields': 'nextPageToken,files(id,name,mimeType,thumbnailLink)',
          'pageSize': '100',
          'orderBy': 'createdTime desc',
          'corpora': 'user',
          'supportsAllDrives': 'true',
          'includeItemsFromAllDrives': 'true',
        };
        if (pageToken != null) params['pageToken'] = pageToken;
        final uri = Uri.parse(filesEndpoint).replace(queryParameters: params);
        final http.Response resp;
        try {
          resp = await client.get(uri, headers: authHeader);
        } catch (e) {
          throw AmbientApiException('Could not reach Google Drive: $e');
        }
        if (resp.statusCode != 200) {
          throw AmbientApiException(
            'Drive listing failed (${resp.statusCode}) for folder $folderId.',
          );
        }
        final json = jsonDecode(resp.body);
        final files = (json is Map && json['files'] is List)
            ? json['files'] as List
            : const [];
        for (final f in files) {
          if (f is! Map) continue;
          final id = f['id'] as String?;
          if (id == null) continue;
          // Prefer Drive's downscaled thumbnail (sized to the screen) over the
          // full-resolution original — a 24 MP `alt=media` download is too heavy
          // for the 1 GB device (slow + can OOM the decoder). Fall back to the
          // original only when no thumbnail is available.
          final thumb = f['thumbnailLink'] as String?;
          items.add(
            PhotoItem.network(
              thumb != null
                  ? _resizedThumb(thumb, maxDimension)
                  : '$filesEndpoint/$id?alt=media',
              headers: authHeader,
              caption: f['name'] as String?,
            ),
          );
          fetched++;
          if (fetched >= maxPerFolder) break;
        }
        pageToken = (json is Map ? json['nextPageToken'] : null) as String?;
      } while (pageToken != null && fetched < maxPerFolder);
    }
  } finally {
    if (httpClient == null) client.close();
  }
  return items;
}

/// Drive `thumbnailLink`s end with a size param (`=s220`); swap it for a
/// screen-appropriate bound so the slideshow downloads ~200 KB images, not 24 MP
/// originals.
String _resizedThumb(String thumbnailLink, int maxDimension) {
  final eq = thumbnailLink.lastIndexOf('=');
  final base = eq >= 0 ? thumbnailLink.substring(0, eq) : thumbnailLink;
  return '$base=s$maxDimension';
}
