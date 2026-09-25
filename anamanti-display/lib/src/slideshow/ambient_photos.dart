// Google Photos **Ambient API** client — the purpose-built API for connecting an
// ambient device to a user's Google Photos and showing their chosen albums.
// Docs: https://developers.google.com/photos/ambient
//
// Auth is "OAuth 2.0 for TVs and Limited Input devices" (device-code + QR), which is
// exactly right for the keyboardless Echo Show (no Play Services). Flow:
//   1. requestDeviceCode(scope=photosambient.tv) → show sign-in QR, user approves on phone
//   2. pollForTokens → access + refresh token
//   3. createDevice → deviceId + settingsUri (show as a 2nd QR; user picks albums
//      in the Google Photos app)
//   4. poll getDevice until mediaSourcesSet == true
//   5. listMediaItems → each AmbientMediaItem.mediaFile.baseUrl → slideshow
//
// baseUrl needs a size param (`=w<W>-h<H>`) and an `Authorization: Bearer` header,
// and (like all Photos APIs) expires ~60 min, so the device refreshes + re-lists.
//
// Networking is injectable (http.Client) so the whole flow is unit-testable offline.

import 'dart:async';
import 'dart:convert';
import 'dart:math';

import 'package:http/http.dart' as http;

import 'package:anamanti_display/src/slideshow/google_oauth_config.dart';
import 'package:anamanti_display/src/slideshow/photo_source.dart';

/// The Ambient API OAuth scope. This is the scope the API methods require
/// (devices.create / mediaItems.list) and the one the device-code endpoint
/// accepts — NOT `photosambient.tv`, which the device flow rejects as invalid.
const String kPhotosAmbientScope =
    'https://www.googleapis.com/auth/photosambient.mediaitems';

/// Thrown when the flow can't complete (denied, expired, HTTP/transport error).
/// The message is safe to surface to the user.
class AmbientApiException implements Exception {
  const AmbientApiException(this.message);
  final String message;
  @override
  String toString() => message;
}

/// Endpoints (overridable in tests).
class AmbientEndpoints {
  const AmbientEndpoints({
    this.deviceCode = 'https://oauth2.googleapis.com/device/code',
    this.token = 'https://oauth2.googleapis.com/token',
    this.api = 'https://photosambient.googleapis.com/v1',
  });
  final String deviceCode;
  final String token;
  final String api;
}

/// OAuth tokens.
class GoogleTokens {
  const GoogleTokens({
    required this.accessToken,
    this.refreshToken,
    this.expiresIn,
  });
  final String accessToken;
  final String? refreshToken;
  final Duration? expiresIn;
}

/// A device-code challenge to show as a QR (+ code/URL fallback) while polling.
class DeviceCodePrompt {
  const DeviceCodePrompt({
    required this.userCode,
    required this.verificationUrl,
    required this.verificationUrlComplete,
    this.expiresIn,
  });
  final String userCode;
  final String verificationUrl;
  final String verificationUrlComplete;
  final Duration? expiresIn;
}

/// Result of the device-code request (step 1).
class DeviceCodeStart {
  const DeviceCodeStart({
    required this.deviceCode,
    required this.interval,
    required this.prompt,
  });
  final String deviceCode;
  final Duration interval;
  final DeviceCodePrompt prompt;
}

/// An ambient device in the user's Google Photos account.
class AmbientDevice {
  const AmbientDevice({
    required this.deviceId,
    this.settingsUri = '',
    this.mediaSourcesSet = false,
    this.pollInterval = const Duration(seconds: 5),
  });
  final String deviceId;

  /// URI to show as a QR — opens the Google Photos app for the user to pick albums.
  final String settingsUri;

  /// True once the user has selected media sources in the Google Photos app.
  final bool mediaSourcesSet;

  /// Suggested polling cadence for [AmbientApiClient.getDevice].
  final Duration pollInterval;
}

const String _kDeviceGrant = 'urn:ietf:params:oauth:grant-type:device_code';

/// Client over the device-code OAuth endpoints + the Ambient API. Construct with the
/// build-time "TVs and Limited Input devices" credentials by default, or inject for
/// tests.
class AmbientApiClient {
  AmbientApiClient({
    String? clientId,
    String? clientSecret,
    http.Client? httpClient,
    this.endpoints = const AmbientEndpoints(),
    this.mediaSize = '=w1920-h1200',
    Future<void> Function(Duration)? sleep,
  }) : clientId = clientId ?? kGoogleOAuthClientId,
       clientSecret = clientSecret ?? kGoogleOAuthClientSecret,
       _http = httpClient ?? http.Client(),
       _sleep = sleep ?? _defaultSleep;

  final String clientId;
  final String clientSecret;
  final AmbientEndpoints endpoints;

  /// baseUrl size suffix for downloaded images (Echo Show 8 is 1280×800; a bit
  /// larger keeps it crisp). Applied to every media `baseUrl`.
  final String mediaSize;

  final http.Client _http;
  final Future<void> Function(Duration) _sleep;

  static Future<void> _defaultSleep(Duration d) => Future<void>.delayed(d);

  Map<String, String> _bearer(String token) => {
    'Authorization': 'Bearer $token',
  };

  // --- OAuth: device-code flow -------------------------------------------------

  /// Step 1: request a device + user code.
  Future<DeviceCodeStart> requestDeviceCode() async {
    final http.Response resp;
    try {
      resp = await _http.post(
        Uri.parse(endpoints.deviceCode),
        headers: const {'Content-Type': 'application/x-www-form-urlencoded'},
        body: {'client_id': clientId, 'scope': kPhotosAmbientScope},
      );
    } catch (e) {
      throw AmbientApiException('Could not reach Google to start linking: $e');
    }
    if (resp.statusCode != 200) {
      throw AmbientApiException(
        'Google refused the linking request (${resp.statusCode}). '
        'Check the OAuth client and that the Ambient API + scope are enabled.',
      );
    }
    final json = _decode(resp.body);
    final userCode = json['user_code'] as String?;
    final deviceCode = json['device_code'] as String?;
    final verificationUrl =
        (json['verification_url'] ?? json['verification_uri']) as String?;
    if (userCode == null || deviceCode == null || verificationUrl == null) {
      throw const AmbientApiException(
        'Incomplete device-code response from Google.',
      );
    }
    final completeFromServer =
        (json['verification_url_complete'] ?? json['verification_uri_complete'])
            as String?;
    final complete =
        completeFromServer ??
        '$verificationUrl?user_code=${Uri.encodeQueryComponent(userCode)}';
    return DeviceCodeStart(
      deviceCode: deviceCode,
      interval: Duration(seconds: (json['interval'] as num?)?.toInt() ?? 5),
      prompt: DeviceCodePrompt(
        userCode: userCode,
        verificationUrl: verificationUrl,
        verificationUrlComplete: complete,
        expiresIn: json['expires_in'] is num
            ? Duration(seconds: (json['expires_in'] as num).toInt())
            : null,
      ),
    );
  }

  /// Step 2: poll until the user approves (or the code is denied/expires).
  Future<GoogleTokens> pollForTokens(DeviceCodeStart start) async {
    var interval = start.interval;
    final deadline = DateTime.now().add(
      start.prompt.expiresIn ?? const Duration(minutes: 5),
    );
    while (true) {
      if (DateTime.now().isAfter(deadline)) {
        throw const AmbientApiException(
          'Linking timed out before it was approved.',
        );
      }
      await _sleep(interval);
      final http.Response resp;
      try {
        resp = await _http.post(
          Uri.parse(endpoints.token),
          headers: const {'Content-Type': 'application/x-www-form-urlencoded'},
          body: {
            'client_id': clientId,
            'client_secret': clientSecret,
            'device_code': start.deviceCode,
            'grant_type': _kDeviceGrant,
          },
        );
      } catch (_) {
        continue; // transient; keep polling until the deadline
      }
      final json = _decode(resp.body);
      if (resp.statusCode == 200) {
        final accessToken = json['access_token'] as String?;
        if (accessToken == null) {
          throw const AmbientApiException('Google returned no access token.');
        }
        return GoogleTokens(
          accessToken: accessToken,
          refreshToken: json['refresh_token'] as String?,
          expiresIn: json['expires_in'] is num
              ? Duration(seconds: (json['expires_in'] as num).toInt())
              : null,
        );
      }
      switch (json['error'] as String?) {
        case 'authorization_pending':
          break;
        case 'slow_down':
          interval += const Duration(seconds: 5);
          break;
        case 'access_denied':
          throw const AmbientApiException('Linking was denied on the phone.');
        case 'expired_token':
          throw const AmbientApiException(
            'The code expired before it was approved.',
          );
        default:
          throw AmbientApiException(
            'Linking failed: ${json['error'] ?? resp.body}',
          );
      }
    }
  }

  /// Exchange a stored refresh token for a fresh access token (boot / re-list).
  Future<GoogleTokens> refresh(String refreshToken) async {
    final http.Response resp;
    try {
      resp = await _http.post(
        Uri.parse(endpoints.token),
        headers: const {'Content-Type': 'application/x-www-form-urlencoded'},
        body: {
          'client_id': clientId,
          'client_secret': clientSecret,
          'refresh_token': refreshToken,
          'grant_type': 'refresh_token',
        },
      );
    } catch (e) {
      throw AmbientApiException('Could not refresh the Google token: $e');
    }
    if (resp.statusCode != 200) {
      throw AmbientApiException(
        'Refreshing the Google token failed (${resp.statusCode}).',
      );
    }
    final json = _decode(resp.body);
    final accessToken = json['access_token'] as String?;
    if (accessToken == null) {
      throw const AmbientApiException(
        'Token refresh returned no access token.',
      );
    }
    return GoogleTokens(
      accessToken: accessToken,
      refreshToken: json['refresh_token'] as String? ?? refreshToken,
      expiresIn: json['expires_in'] is num
          ? Duration(seconds: (json['expires_in'] as num).toInt())
          : null,
    );
  }

  // --- Ambient API: devices + media -------------------------------------------

  /// Create an ambient device in the user's Google Photos account. [requestId] must
  /// be a v4 UUID (defaults to a fresh one); reuse it to avoid duplicates on retry.
  Future<AmbientDevice> createDevice(
    String accessToken, {
    String displayName = 'Ambient Display',
    String? requestId,
  }) async {
    final id = requestId ?? newUuidV4();
    final resp = await _apiCall(
      'POST',
      '/devices?requestId=$id',
      accessToken,
      body: {'displayName': displayName},
    );
    return _parseDevice(_decode(resp.body));
  }

  /// Fetch a device to check [AmbientDevice.mediaSourcesSet].
  Future<AmbientDevice> getDevice(String accessToken, String deviceId) async {
    final resp = await _apiCall('GET', '/devices/$deviceId', accessToken);
    return _parseDevice(_decode(resp.body));
  }

  /// List the curated media items across all sources the user picked (omit a source
  /// id for the ambient experience). Returns slideshow-ready [PhotoItem]s with the
  /// size suffix + bearer header applied to each `baseUrl`.
  Future<List<PhotoItem>> listMediaItems(
    String accessToken, {
    int pageSize = 100,
    int maxItems = 200,
  }) async {
    final header = _bearer(accessToken);
    final items = <PhotoItem>[];
    String? pageToken;
    do {
      final params = <String, String>{'pageSize': '$pageSize'};
      if (pageToken != null) params['pageToken'] = pageToken;
      final resp = await _apiCall(
        'GET',
        '/mediaItems',
        accessToken,
        query: params,
      );
      final json = _decode(resp.body);
      final list = json['mediaItems'];
      if (list is List) {
        for (final raw in list) {
          if (raw is! Map) continue;
          final mediaFile = raw['mediaFile'];
          final baseUrl = mediaFile is Map
              ? mediaFile['baseUrl'] as String?
              : null;
          if (baseUrl == null) continue;
          items.add(PhotoItem.network('$baseUrl$mediaSize', headers: header));
          if (items.length >= maxItems) return items;
        }
      }
      pageToken = json['nextPageToken'] as String?;
    } while (pageToken != null && pageToken.isNotEmpty);
    return items;
  }

  Future<http.Response> _apiCall(
    String method,
    String path,
    String accessToken, {
    Map<String, String>? query,
    Map<String, dynamic>? body,
  }) async {
    var uri = Uri.parse('${endpoints.api}$path');
    if (query != null && query.isNotEmpty) {
      uri = uri.replace(queryParameters: {...uri.queryParameters, ...query});
    }
    final headers = {
      ..._bearer(accessToken),
      if (body != null) 'Content-Type': 'application/json',
    };
    final http.Response resp;
    try {
      resp = method == 'POST'
          ? await _http.post(
              uri,
              headers: headers,
              body: jsonEncode(body ?? {}),
            )
          : await _http.get(uri, headers: headers);
    } catch (e) {
      throw AmbientApiException('Could not reach the Ambient API: $e');
    }
    if (resp.statusCode < 200 || resp.statusCode >= 300) {
      throw AmbientApiException(
        'Ambient API $method $path failed (${resp.statusCode}).',
      );
    }
    return resp;
  }

  AmbientDevice _parseDevice(Map<String, dynamic> json) {
    // The resource id may come back as `deviceId` or the resource `name`.
    final id = (json['deviceId'] ?? json['name']) as String?;
    if (id == null) {
      throw const AmbientApiException('Ambient device response had no id.');
    }
    final polling = json['pollingConfig'];
    final pollSecs = polling is Map ? _seconds(polling['pollInterval']) : null;
    return AmbientDevice(
      deviceId: id,
      settingsUri: (json['settingsUri'] as String?) ?? '',
      mediaSourcesSet: json['mediaSourcesSet'] == true,
      pollInterval: Duration(seconds: pollSecs ?? 5),
    );
  }

  /// Parse a protobuf-style duration string like `"7s"` into whole seconds.
  int? _seconds(Object? v) {
    if (v is num) return v.toInt();
    if (v is String) {
      final m = RegExp(r'^(\d+)s?$').firstMatch(v.trim());
      if (m != null) return int.tryParse(m.group(1)!);
    }
    return null;
  }

  Map<String, dynamic> _decode(String body) {
    try {
      final decoded = jsonDecode(body);
      return decoded is Map<String, dynamic> ? decoded : <String, dynamic>{};
    } catch (_) {
      return <String, dynamic>{};
    }
  }

  void close() => _http.close();
}

/// Generate a random v4 UUID (for the Ambient API `requestId` / device dedupe).
String newUuidV4() {
  final rng = Random.secure();
  final b = List<int>.generate(16, (_) => rng.nextInt(256));
  b[6] = (b[6] & 0x0f) | 0x40; // version 4
  b[8] = (b[8] & 0x3f) | 0x80; // variant
  String hex(int i) => b[i].toRadixString(16).padLeft(2, '0');
  final s = List.generate(16, hex).join();
  return '${s.substring(0, 8)}-${s.substring(8, 12)}-${s.substring(12, 16)}-'
      '${s.substring(16, 20)}-${s.substring(20)}';
}
