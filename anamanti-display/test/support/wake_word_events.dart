// A single factory for the flat FRB `WakeWordEvent` in tests: every field defaults to
// empty/zero, so a test names only what it cares about — and a new field on the Rust
// struct needs one edit here rather than one per test file.

import 'package:anamanti_display/src/rust/api/engine.dart';

/// A [WakeWordEvent] of [kind] with every other field defaulted.
WakeWordEvent wakeEvent(
  WakeWordEventKind kind, {
  String message = '',
  String transcript = '',
  String reply = '',
  String recipeJson = '',
  String weatherJson = '',
  String placeJson = '',
  String recipeAction = '',
  String enrollJson = '',
}) => WakeWordEvent(
  kind: kind,
  message: message,
  device: '',
  deviceSampleRate: 0,
  channels: 0,
  rms: 0,
  score: 0,
  avgScore: 0,
  threshold: 0,
  gainDb: 0,
  model: '',
  transcript: transcript,
  reply: reply,
  timerId: 0,
  timerLabel: '',
  timerRemainingSecs: 0,
  present: false,
  recipeJson: recipeJson,
  weatherJson: weatherJson,
  placeJson: placeJson,
  recipeAction: recipeAction,
  enrollJson: enrollJson,
);
