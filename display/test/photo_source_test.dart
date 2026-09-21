import 'package:flutter_test/flutter_test.dart';

import 'package:ambient_display/src/settings/app_settings.dart';
import 'package:ambient_display/src/slideshow/photo_source.dart';

Future<List<PhotoItem>> _ambientLister({required String accessToken}) async {
  return [PhotoItem.network('https://photos/$accessToken')];
}

Future<List<PhotoItem>> _driveLister({
  required String accessToken,
  required List<String> folderIds,
}) async {
  return [for (final id in folderIds) PhotoItem.network('https://drive/$id')];
}

Future<List<PhotoItem>> _throwingAmbient({required String accessToken}) async {
  throw StateError('offline');
}

Future<List<PhotoItem>> _threeLister({required String accessToken}) async {
  return const [
    PhotoItem.network('a'),
    PhotoItem.network('b'),
    PhotoItem.network('c'),
  ];
}

PhotoSource _build(AppSettings s, {String? ambientToken, String? driveToken}) =>
    photoSourceFromSettings(
      s,
      ambientLister: _ambientLister,
      driveLister: _driveLister,
      ambientAccessToken: ambientToken,
      driveAccessToken: driveToken,
    );

void main() {
  group('photoSourceFromSettings — ambient', () {
    const linked = AppSettings(
      photoSource: PhotoSourceKind.ambient,
      ambientLinked: true,
      ambientRefreshToken: 'RT',
      ambientDeviceId: 'dev-1',
    );

    test('AmbientPhotoSource when linked + token', () {
      expect(_build(linked, ambientToken: 'AT'), isA<AmbientPhotoSource>());
    });
    test('local when no token', () {
      expect(_build(linked, ambientToken: null), isA<LocalPhotoSource>());
    });
    test('local when not linked', () {
      expect(
        _build(
          const AppSettings(photoSource: PhotoSourceKind.ambient),
          ambientToken: 'AT',
        ),
        isA<LocalPhotoSource>(),
      );
    });
  });

  group('photoSourceFromSettings — drive', () {
    const linked = AppSettings(
      photoSource: PhotoSourceKind.drive,
      driveLinked: true,
      driveRefreshToken: 'RT',
      driveFolderIds: ['f1', 'f2'],
    );

    test('DrivePhotoSource when linked + token + folders', () {
      expect(_build(linked, driveToken: 'AT'), isA<DrivePhotoSource>());
    });
    test('local when no folders', () {
      expect(
        _build(
          const AppSettings(
            photoSource: PhotoSourceKind.drive,
            driveLinked: true,
            driveRefreshToken: 'RT',
          ),
          driveToken: 'AT',
        ),
        isA<LocalPhotoSource>(),
      );
    });
    test('local when no token', () {
      expect(_build(linked, driveToken: null), isA<LocalPhotoSource>());
    });
  });

  test('local when photoSource is local', () {
    expect(
      _build(const AppSettings(), ambientToken: 'AT', driveToken: 'AT'),
      isA<LocalPhotoSource>(),
    );
  });

  group('sources load via injected listers', () {
    test('AmbientPhotoSource', () async {
      const src = AmbientPhotoSource(accessToken: 'AT', lister: _ambientLister);
      expect((await src.loadPhotos()).first.imageUrl, 'https://photos/AT');
    });
    test('DrivePhotoSource', () async {
      const src = DrivePhotoSource(
        accessToken: 'AT',
        folderIds: ['f1'],
        lister: _driveLister,
      );
      expect((await src.loadPhotos()).first.imageUrl, 'https://drive/f1');
    });
    test('DrivePhotoSource throws with no folders', () {
      const src = DrivePhotoSource(
        accessToken: 'AT',
        folderIds: [],
        lister: _driveLister,
      );
      expect(src.loadPhotos(), throwsStateError);
    });
  });

  test('SlideshowController next/previous wrap around the list', () async {
    final controller = SlideshowController(
      source: const AmbientPhotoSource(accessToken: 'AT', lister: _threeLister),
    );
    await controller.start();
    expect(controller.index, 0);
    controller.previous(); // wraps to last
    expect(controller.index, 2);
    controller.next(); // wraps back to 0
    expect(controller.index, 0);
    controller.next();
    expect(controller.index, 1);
    controller.dispose();
  });

  test(
    'SlideshowController falls back to local gradients when source throws',
    () async {
      const src = AmbientPhotoSource(
        accessToken: 'AT',
        lister: _throwingAmbient,
      );
      final controller = SlideshowController(source: src);
      await controller.start();
      expect(controller.photos, isNotEmpty);
      expect(controller.current?.imageUrl, isNull);
      controller.dispose();
    },
  );

  group('AppSettings persistence', () {
    test('round-trips ambient + drive fields', () {
      const s = AppSettings(
        photoSource: PhotoSourceKind.drive,
        ambientRefreshToken: 'a-rt',
        ambientDeviceId: 'dev-42',
        ambientLinked: true,
        driveRefreshToken: 'd-rt',
        driveFolderIds: ['abc', 'def'],
        driveLinked: true,
      );
      expect(AppSettings.fromJson(s.toJson()), s);
    });

    test('tolerates missing fields', () {
      final r = AppSettings.fromJson({'photoSource': 'drive'});
      expect(r.photoSource, PhotoSourceKind.drive);
      expect(r.driveFolderIds, isEmpty);
      expect(r.ambientLinked, isFalse);
      expect(r.driveLinked, isFalse);
    });
  });
}
