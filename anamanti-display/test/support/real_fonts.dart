// Load the real Material fonts (Roboto + Material Icons) from the Flutter SDK for
// tests that measure layout. flutter_test otherwise renders every glyph as a
// full-em square, which makes text much wider than on the device — fine for most
// tests, but it would make the "every Settings tab fits one screen" checks
// meaningless (they'd fail on text that fits on the Echo Show, or pass on text
// that doesn't).

import 'dart:io';

import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

bool _loaded = false;

/// Load Roboto (regular + medium) and Material Icons once per test process.
/// Returns false when the SDK fonts can't be found (the caller may then skip).
Future<bool> loadRealFonts(WidgetTester tester) async {
  if (_loaded) return true;
  final root = Platform.environment['FLUTTER_ROOT'];
  if (root == null) return false;
  final dir = '$root/bin/cache/artifacts/material_fonts';
  final fonts = <String, List<String>>{
    'Roboto': ['Roboto-Regular.ttf', 'Roboto-Medium.ttf', 'Roboto-Bold.ttf'],
    'MaterialIcons': ['MaterialIcons-Regular.otf'],
  };
  for (final files in fonts.values) {
    for (final f in files) {
      if (!File('$dir/$f').existsSync()) return false;
    }
  }
  await tester.runAsync(() async {
    for (final entry in fonts.entries) {
      final loader = FontLoader(entry.key);
      for (final f in entry.value) {
        final bytes = File('$dir/$f').readAsBytesSync();
        loader.addFont(
          Future.value(ByteData.sublistView(Uint8List.fromList(bytes))),
        );
      }
      await loader.load();
    }
  });
  _loaded = true;
  return true;
}
