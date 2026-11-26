import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

import 'package:anamanti_display/src/slideshow/ambient_photos.dart'
    show AmbientApiException;
import 'package:anamanti_display/src/slideshow/drive_photos.dart';

const _endpoint = 'https://drive.example/v3/files';

void main() {
  test(
    'uses a resized thumbnailLink, falls back to alt=media, with auth header',
    () async {
      final mock = MockClient((req) async {
        final q = req.url.queryParameters['q']!;
        expect(req.headers['Authorization'], 'Bearer TOKEN');
        expect(q, contains("mimeType contains 'image/'"));
        expect(req.url.queryParameters['fields'], contains('thumbnailLink'));
        // Both folders arrive in one batched `'a' in parents or 'b' in parents`.
        return http.Response(
          jsonEncode({
            'files': [
              if (q.contains("'folderA'"))
                {
                  'id': 'a1',
                  'name': 'one.jpg',
                  'thumbnailLink': 'https://lh3.googleusercontent.com/x=s220',
                },
              if (q.contains("'folderB'"))
                {'id': 'b1', 'name': 'two.png'}, // no thumbnailLink → fallback
            ],
          }),
          200,
        );
      });

      final items = await listDrivePhotos(
        accessToken: 'TOKEN',
        folderIds: const ['folderA', 'folderB'],
        httpClient: mock,
        filesEndpoint: _endpoint,
        maxDepth: 0, // image listing only; subfolders are covered below
        maxDimension: 1600,
      );

      expect(items, hasLength(2));
      expect(items.first.imageUrl, 'https://lh3.googleusercontent.com/x=s1600');
      expect(items.first.headers, {'Authorization': 'Bearer TOKEN'});
      expect(items.first.caption, 'one.jpg');
      expect(items.last.imageUrl, '$_endpoint/b1?alt=media'); // fallback
    },
  );

  test('follows pagination via nextPageToken', () async {
    var calls = 0;
    final mock = MockClient((req) async {
      calls++;
      final page = req.url.queryParameters['pageToken'];
      if (page == null) {
        return http.Response(
          jsonEncode({
            'nextPageToken': 'P2',
            'files': [
              {'id': 'p1', 'name': '1.jpg'},
            ],
          }),
          200,
        );
      }
      expect(page, 'P2');
      return http.Response(
        jsonEncode({
          'files': [
            {'id': 'p2', 'name': '2.jpg'},
          ],
        }),
        200,
      );
    });

    final items = await listDrivePhotos(
      accessToken: 'TOKEN',
      folderIds: const ['f'],
      httpClient: mock,
      filesEndpoint: _endpoint,
      maxDepth: 0,
    );
    expect(calls, 2);
    expect(items.map((i) => i.imageUrl), [
      '$_endpoint/p1?alt=media',
      '$_endpoint/p2?alt=media',
    ]);
  });

  test('skips blank folder ids', () async {
    var calls = 0;
    final mock = MockClient((req) async {
      calls++;
      return http.Response(jsonEncode({'files': []}), 200);
    });
    final items = await listDrivePhotos(
      accessToken: 'TOKEN',
      folderIds: const ['', '   '],
      httpClient: mock,
      filesEndpoint: _endpoint,
    );
    expect(calls, 0);
    expect(items, isEmpty);
  });

  test('throws AmbientApiException on a non-200', () async {
    final mock = MockClient((req) async => http.Response('unauthorized', 401));
    expect(
      listDrivePhotos(
        accessToken: 'TOKEN',
        folderIds: const ['f'],
        httpClient: mock,
        filesEndpoint: _endpoint,
      ),
      throwsA(isA<AmbientApiException>()),
    );
  });

  group('subfolders', () {
    // A fake Drive tree: folder id → child folder ids; every folder holds one
    // image named after it.
    MockClient driveTree(
      Map<String, List<String>> children, {
      List<String>? queries,
    }) {
      return MockClient((req) async {
        final q = req.url.queryParameters['q']!;
        queries?.add(q);
        final parents = RegExp(
          r"'([^']+)' in parents",
        ).allMatches(q).map((m) => m.group(1)!);
        final isFolderQuery = q.contains('vnd.google-apps.folder');
        return http.Response(
          jsonEncode({
            'files': [
              for (final p in parents)
                if (isFolderQuery)
                  for (final c in children[p] ?? const <String>[]) {'id': c}
                else
                  {'id': 'img-$p', 'name': '$p.jpg'},
            ],
          }),
          200,
        );
      });
    }

    test('finds images up to three levels below the chosen folder', () async {
      final items = await listDrivePhotos(
        accessToken: 'TOKEN',
        folderIds: const ['root'],
        httpClient: driveTree({
          'root': ['l1'],
          'l1': ['l2'],
          'l2': ['l3'],
          'l3': ['l4'],
        }),
        filesEndpoint: _endpoint,
      );
      expect(items.map((i) => i.caption), [
        'root.jpg',
        'l1.jpg',
        'l2.jpg',
        'l3.jpg',
      ]);
    });

    test('a folder reachable twice is searched once', () async {
      final items = await listDrivePhotos(
        accessToken: 'TOKEN',
        folderIds: const ['a', 'b'],
        httpClient: driveTree({
          'a': ['shared'],
          'b': ['shared'],
        }),
        filesEndpoint: _endpoint,
      );
      expect(items.map((i) => i.caption), ['a.jpg', 'b.jpg', 'shared.jpg']);
    });

    test(
      'thousands of folders are capped and batched into few requests',
      () async {
        final queries = <String>[];
        final items = await listDrivePhotos(
          accessToken: 'TOKEN',
          folderIds: const ['root'],
          httpClient: driveTree({
            'root': [for (var i = 0; i < 5000; i++) 'sub$i'],
          }, queries: queries),
          filesEndpoint: _endpoint,
          maxFolders: 300,
          maxPhotos: 1000,
        );
        // root + 299 subfolders; one folder query for root, then 300 / 40 = 8
        // batched image queries (no further folder levels once the cap is hit).
        expect(items, hasLength(300));
        expect(queries, hasLength(1 + 8));
      },
    );

    test('stops at maxPhotos', () async {
      final items = await listDrivePhotos(
        accessToken: 'TOKEN',
        folderIds: const ['root'],
        httpClient: driveTree({
          'root': [for (var i = 0; i < 50; i++) 'sub$i'],
        }),
        filesEndpoint: _endpoint,
        maxPhotos: 10,
      );
      expect(items, hasLength(10));
    });
  });

  test(
    'listDriveFolders returns folders (owned + shared) for the picker',
    () async {
      final mock = MockClient((req) async {
        expect(
          req.url.queryParameters['q'],
          contains("mimeType = 'application/vnd.google-apps.folder'"),
        );
        return http.Response(
          jsonEncode({
            'files': [
              {
                'id': 'own1',
                'name': 'Family',
                'ownedByMe': true,
                'shared': false,
              },
              {
                'id': 'shr1',
                'name': 'Trip',
                'ownedByMe': false,
                'shared': true,
              },
            ],
          }),
          200,
        );
      });
      final folders = await listDriveFolders(
        accessToken: 'TOKEN',
        httpClient: mock,
        filesEndpoint: _endpoint,
      );
      expect(folders.map((f) => f.id), ['own1', 'shr1']);
      expect(folders.first.name, 'Family');
      expect(folders.first.shared, isFalse);
      expect(folders.last.shared, isTrue);
    },
  );
}
