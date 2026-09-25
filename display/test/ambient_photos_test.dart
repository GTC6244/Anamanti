import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

import 'package:ambient_display/src/slideshow/ambient_photos.dart';

const _ep = AmbientEndpoints(
  deviceCode: 'https://oauth.example/device/code',
  token: 'https://oauth.example/token',
  api: 'https://ambient.example/v1',
);

AmbientApiClient _client(MockClient mock) => AmbientApiClient(
  clientId: 'cid',
  clientSecret: 'sec',
  httpClient: mock,
  endpoints: _ep,
  mediaSize: '=w800-h600',
  sleep: (_) async {},
);

DeviceCodeStart _start() => const DeviceCodeStart(
  deviceCode: 'DEV',
  interval: Duration(seconds: 1),
  prompt: DeviceCodePrompt(
    userCode: 'ABCD-EFGH',
    verificationUrl: 'https://google.com/device',
    verificationUrlComplete: 'https://google.com/device?user_code=ABCD-EFGH',
    expiresIn: Duration(seconds: 60),
  ),
);

void main() {
  test(
    'requestDeviceCode parses the code and requests the ambient scope',
    () async {
      final mock = MockClient((req) async {
        expect(req.url.toString(), _ep.deviceCode);
        expect(req.bodyFields['scope'], kPhotosAmbientScope);
        return http.Response(
          jsonEncode({
            'device_code': 'DEV',
            'user_code': 'ABCD-EFGH',
            'verification_url': 'https://www.google.com/device',
            'interval': 5,
            'expires_in': 1800,
          }),
          200,
        );
      });
      final start = await _client(mock).requestDeviceCode();
      expect(start.deviceCode, 'DEV');
      expect(
        start.prompt.verificationUrlComplete,
        'https://www.google.com/device?user_code=ABCD-EFGH',
      );
    },
  );

  test('pollForTokens waits through pending then returns tokens', () async {
    var n = 0;
    final mock = MockClient((req) async {
      n++;
      if (n < 2) {
        return http.Response(
          jsonEncode({'error': 'authorization_pending'}),
          428,
        );
      }
      return http.Response(
        jsonEncode({'access_token': 'AT', 'refresh_token': 'RT'}),
        200,
      );
    });
    final tokens = await _client(mock).pollForTokens(_start());
    expect(tokens.accessToken, 'AT');
    expect(tokens.refreshToken, 'RT');
  });

  test('refresh keeps the refresh token when the response omits it', () async {
    final mock = MockClient((req) async {
      expect(req.bodyFields['grant_type'], 'refresh_token');
      return http.Response(jsonEncode({'access_token': 'AT2'}), 200);
    });
    final t = await _client(mock).refresh('RT');
    expect(t.accessToken, 'AT2');
    expect(t.refreshToken, 'RT');
  });

  test('createDevice posts a requestId UUID and parses the device', () async {
    final mock = MockClient((req) async {
      expect(req.method, 'POST');
      expect(req.url.path, '/v1/devices');
      final rid = req.url.queryParameters['requestId']!;
      expect(
        rid,
        matches(
          r'^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$',
        ),
      );
      expect(req.headers['Authorization'], 'Bearer AT');
      expect(jsonDecode(req.body)['displayName'], 'Echo Show');
      return http.Response(
        jsonEncode({
          'deviceId': 'dev-123',
          'settingsUri': 'https://photos.google.com/ambient/setup?d=dev-123',
          'mediaSourcesSet': false,
          'pollingConfig': {'pollInterval': '7s'},
        }),
        200,
      );
    });
    final device = await _client(
      mock,
    ).createDevice('AT', displayName: 'Echo Show');
    expect(device.deviceId, 'dev-123');
    expect(device.settingsUri, contains('setup'));
    expect(device.mediaSourcesSet, isFalse);
    expect(device.pollInterval, const Duration(seconds: 7));
  });

  test('getDevice reports mediaSourcesSet', () async {
    final mock = MockClient((req) async {
      expect(req.method, 'GET');
      expect(req.url.path, '/v1/devices/dev-123');
      return http.Response(
        jsonEncode({'deviceId': 'dev-123', 'mediaSourcesSet': true}),
        200,
      );
    });
    final device = await _client(mock).getDevice('AT', 'dev-123');
    expect(device.mediaSourcesSet, isTrue);
  });

  test(
    'listMediaItems maps baseUrls with size suffix + bearer header, paginated',
    () async {
      var calls = 0;
      final mock = MockClient((req) async {
        calls++;
        expect(req.url.path, '/v1/mediaItems');
        expect(req.headers['Authorization'], 'Bearer AT');
        if (req.url.queryParameters['pageToken'] == null) {
          return http.Response(
            jsonEncode({
              'mediaItems': [
                {
                  'id': '1',
                  'mediaFile': {'baseUrl': 'https://lh3/p/AAA'},
                },
              ],
              'nextPageToken': 'P2',
            }),
            200,
          );
        }
        return http.Response(
          jsonEncode({
            'mediaItems': [
              {
                'id': '2',
                'mediaFile': {'baseUrl': 'https://lh3/p/BBB'},
              },
            ],
          }),
          200,
        );
      });
      final items = await _client(mock).listMediaItems('AT');
      expect(calls, 2);
      expect(items.map((i) => i.imageUrl), [
        'https://lh3/p/AAA=w800-h600',
        'https://lh3/p/BBB=w800-h600',
      ]);
      expect(items.first.headers, {'Authorization': 'Bearer AT'});
    },
  );

  test('API errors surface as AmbientApiException', () async {
    final mock = MockClient((req) async => http.Response('nope', 403));
    expect(
      _client(mock).getDevice('AT', 'x'),
      throwsA(isA<AmbientApiException>()),
    );
  });

  test('newUuidV4 has the v4 shape', () {
    expect(
      newUuidV4(),
      matches(
        r'^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$',
      ),
    );
  });
}
