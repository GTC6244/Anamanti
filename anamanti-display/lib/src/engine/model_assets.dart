// Unpacks the bundled openWakeWord ONNX models onto the device filesystem
// (Plan.MD §3, Phase 2/6).
//
// The native Rust engine loads the wake-word model chain from the filesystem
// (`<app-support>/models/*.onnx`), not from Flutter's asset bundle. This copies
// the models bundled in the APK (`assets/models/`, declared in `pubspec.yaml`)
// into that directory on first run, so a fresh install detects the wake word with
// no manual `adb push`. Models the user drops in manually (e.g. an extra wake
// word) are preserved — a file that already exists is left untouched.

import 'dart:io';

import 'package:flutter/services.dart' show rootBundle, AssetManifest;

import 'package:anamanti_display/src/engine/wakeword_config.dart';

/// Copy every bundled `assets/models/*.onnx` into the app-support model dir,
/// skipping any that already exist. Best-effort: a copy failure for one file is
/// logged-by-return and never blocks startup (the engine degrades to capture-only
/// if a required model is missing).
Future<void> ensureWakeWordModels() async {
  final dir = await wakeWordModelDir();

  final manifest = await AssetManifest.loadFromAssetBundle(rootBundle);
  final assets = manifest
      .listAssets()
      .where((a) => a.startsWith('assets/models/') && a.endsWith('.onnx'));

  for (final asset in assets) {
    final name = asset.split('/').last;
    final dest = File('${dir.path}/$name');
    if (await dest.exists()) continue;
    try {
      final data = await rootBundle.load(asset);
      await dest.writeAsBytes(
        data.buffer.asUint8List(data.offsetInBytes, data.lengthInBytes),
        flush: true,
      );
    } catch (_) {
      // Leave it missing; the engine will report capture-only for this model.
    }
  }
}
