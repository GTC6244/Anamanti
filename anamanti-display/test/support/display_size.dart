// Size the test viewport like the Echo Show 8: 1280×800 px at 213 dpi, which
// Flutter lays out as 960×600 logical pixels (plans/SettingsReorgPlan.md §0).

import 'dart:ui';

import 'package:flutter_test/flutter_test.dart';

/// Logical size of the Echo Show 8 screen.
const Size kEchoShowLogicalSize = Size(960, 600);

/// Make [tester]'s view match the Echo Show 8 for the rest of the test.
void useEchoShowDisplay(WidgetTester tester) {
  tester.view.physicalSize = const Size(1280, 800);
  tester.view.devicePixelRatio = 1280 / kEchoShowLogicalSize.width;
  addTearDown(tester.view.reset);
}
