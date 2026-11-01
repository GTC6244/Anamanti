// The Dart side of the always-on wake-word trace (`rust/src/engine/wake_trace.rs`):
// where the files live, and the screen state the engine can't see for itself.

import 'dart:io';

import 'package:path_provider/path_provider.dart';

import 'package:anamanti_display/src/engine/assistant_controller.dart';

/// Where the trace is kept. App-external storage on Android, because a release build's
/// private files can't be read over adb; `adb pull` works from here with no root.
Future<String> wakeTraceDir() async {
  final external = Platform.isAndroid
      ? await getExternalStorageDirectory()
      : null;
  final base = external ?? await getApplicationSupportDirectory();
  return '${base.path}/wake_trace';
}

/// One trace line for the display's current state. Written only when it changes, so a
/// failed wake can be read against what the screen was doing at the time.
String wakeTraceStateLine(
  AssistantState state, {
  required bool dark,
  required double brightness,
}) =>
    'state phase=${state.phase.name} awake=${state.screenAwake} '
    'present=${state.userPresent} dark=$dark '
    'brightness=${brightness.toStringAsFixed(2)} online=${state.online}';
