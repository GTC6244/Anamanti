// Live capture-status panel for guided voice enrollment (SpeakerEnrollmentPlan.md).
//
// Each enrollment phrase rides an ordinary wake-word turn, so without feedback the
// user can't tell whether the wake word was heard, whether the mic is recording, or
// whether the sample is being checked. This panel maps the engine's existing turn
// signals (`AssistantState.phase`, `micLevel`, `captureReady`, `online`) plus the
// enroll screen's last per-sample result into one clear cue:
//
//   waiting → (wake word) → listening [live mic meter] → checking → accepted/rejected
//
// Presentation only — no new engine events. Uses implicit animations only (no
// repeating controllers) so it is cheap on the device and lets widget tests settle.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/assistant_controller.dart';

/// What the enrollment capture is doing right now, from the user's point of view.
enum EnrollCue {
  /// The mic hasn't started yet (engine still coming up).
  micStarting,

  /// The engine hit an error and is restarting capture.
  restarting,

  /// The Mac (Anamanti Core) can't be reached — captures can't be checked.
  offline,

  /// Idle: waiting for the user to say the wake word.
  waiting,

  /// Wake word heard; the mic is recording the phrase.
  listening,

  /// End of speech; the Core is checking the sample.
  checking,

  /// The last sample was accepted (shown briefly).
  accepted,

  /// The last sample was rejected (shown briefly; the reason is shown separately).
  rejected,
}

/// Derive the cue from the live engine [state] and the enroll screen's most recent
/// per-sample result ([lastResultAccepted], non-null only while its brief flash is
/// showing). A live turn always wins over a stale result flash.
EnrollCue enrollCueFor(AssistantState state, {bool? lastResultAccepted}) {
  switch (state.phase) {
    case TurnPhase.error:
      return EnrollCue.restarting;
    case TurnPhase.listening:
    case TurnPhase.connecting:
      // Connecting still buffers the mic, so the user should keep reading.
      return EnrollCue.listening;
    case TurnPhase.processing:
    case TurnPhase.thinking:
    case TurnPhase.speaking:
      return lastResultAccepted == null
          ? EnrollCue.checking
          : (lastResultAccepted ? EnrollCue.accepted : EnrollCue.rejected);
    case TurnPhase.idle:
      if (lastResultAccepted != null) {
        return lastResultAccepted ? EnrollCue.accepted : EnrollCue.rejected;
      }
      if (!state.captureReady) return EnrollCue.micStarting;
      if (!state.online) return EnrollCue.offline;
      return EnrollCue.waiting;
  }
}

/// The capture-status card shown on the enroll screen's capture + verify steps.
class EnrollCaptureStatus extends StatelessWidget {
  const EnrollCaptureStatus({
    super.key,
    required this.cue,
    required this.micLevel,
    this.wakeWord = '',
    this.verifying = false,
    this.resultDetail = '',
  });

  final EnrollCue cue;

  /// Latest mic RMS (~0..1), drives the live meter while [EnrollCue.listening].
  final double micLevel;

  /// The configured wake word for the copy ("Say 'hey jarvis'"); empty = generic.
  final String wakeWord;

  /// Verify step: "speak naturally" rather than "read the phrase".
  final bool verifying;

  /// The Core's message for the flashed sample result (e.g. the rejection reason
  /// "too short — please speak a little longer"); shown on the accepted/rejected cue.
  final String resultDetail;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final wake = wakeWord.isEmpty ? 'the wake word' : '"$wakeWord"';
    final speakNow = verifying
        ? 'Listening — speak naturally now'
        : 'Listening — read the phrase now';

    final (
      IconData icon,
      Color color,
      String title,
      String detail,
    ) = switch (cue) {
      EnrollCue.micStarting => (
        Icons.mic_off,
        theme.disabledColor,
        'Starting microphone…',
        'One moment.',
      ),
      EnrollCue.restarting => (
        Icons.sync_problem,
        scheme.error,
        'Microphone restarting…',
        'Hang on — capture will resume by itself.',
      ),
      EnrollCue.offline => (
        Icons.cloud_off,
        scheme.error,
        "Can't reach the assistant",
        'Samples are checked on the Mac — make sure it is running.',
      ),
      EnrollCue.waiting => (
        Icons.mic_none,
        scheme.primary,
        'Waiting for $wake',
        verifying
            ? 'Say $wake, then talk for a few seconds.'
            : 'Say $wake, then read the highlighted phrase.',
      ),
      EnrollCue.listening => (
        Icons.mic,
        Colors.red,
        speakNow,
        'Heard $wake — recording.',
      ),
      EnrollCue.checking => (
        Icons.graphic_eq,
        scheme.primary,
        verifying ? 'Checking your voice…' : 'Checking that sample…',
        'Got it — one moment.',
      ),
      EnrollCue.accepted => (
        Icons.check_circle,
        Colors.green,
        verifying ? 'Done' : 'Sample captured',
        resultDetail.isNotEmpty
            ? resultDetail
            : (verifying ? '' : 'Nice — on to the next phrase.'),
      ),
      EnrollCue.rejected => (
        Icons.error_outline,
        Colors.orange,
        'Sample not used',
        resultDetail.isNotEmpty
            ? '$resultDetail. Say $wake and try that phrase again.'
            : 'Say $wake and try that phrase again.',
      ),
    };

    final live = cue == EnrollCue.listening;
    return AnimatedContainer(
      key: const Key('enroll-capture-status'),
      duration: const Duration(milliseconds: 200),
      padding: const EdgeInsets.all(14),
      decoration: BoxDecoration(
        color: color.withValues(alpha: live ? 0.14 : 0.08),
        borderRadius: BorderRadius.circular(12),
        border: Border.all(
          color: color.withValues(alpha: live ? 0.9 : 0.4),
          width: live ? 2 : 1,
        ),
      ),
      child: Row(
        children: [
          SizedBox(
            width: 44,
            height: 44,
            child: cue == EnrollCue.checking
                ? Padding(
                    padding: const EdgeInsets.all(8),
                    child: CircularProgressIndicator(
                      strokeWidth: 3,
                      color: color,
                    ),
                  )
                : Icon(icon, size: 36, color: color),
          ),
          const SizedBox(width: 12),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisSize: MainAxisSize.min,
              children: [
                Row(
                  children: [
                    if (live) ...[const _RecDot(), const SizedBox(width: 6)],
                    Flexible(
                      child: Text(
                        title,
                        key: const Key('enroll-cue-title'),
                        style: theme.textTheme.titleMedium?.copyWith(
                          fontWeight: FontWeight.w600,
                        ),
                      ),
                    ),
                  ],
                ),
                if (detail.isNotEmpty) ...[
                  const SizedBox(height: 2),
                  Text(detail, style: theme.textTheme.bodySmall),
                ],
                if (live) ...[
                  const SizedBox(height: 8),
                  MicLevelMeter(level: micLevel, color: color),
                ],
              ],
            ),
          ),
        ],
      ),
    );
  }
}

/// A small solid "recording" dot.
class _RecDot extends StatelessWidget {
  const _RecDot();

  @override
  Widget build(BuildContext context) => Container(
    width: 10,
    height: 10,
    decoration: const BoxDecoration(color: Colors.red, shape: BoxShape.circle),
  );
}

/// A compact, auto-ranged mic level meter: a row of bars that light up with the
/// voice. Auto-ranges against a slowly decaying peak (like the ambient listening
/// ring) because on-device speech RMS is small and varies with gain/distance.
class MicLevelMeter extends StatefulWidget {
  const MicLevelMeter({
    super.key,
    required this.level,
    required this.color,
    this.bars = 12,
  });

  final double level;
  final Color color;
  final int bars;

  @override
  State<MicLevelMeter> createState() => _MicLevelMeterState();
}

class _MicLevelMeterState extends State<MicLevelMeter> {
  static const double _floor = 0.002;
  static const double _minPeak = 0.02;
  double _peak = _minPeak;

  @override
  Widget build(BuildContext context) {
    final level = widget.level;
    final decayed = _peak * 0.97;
    _peak = level > decayed ? level : decayed;
    if (_peak < _minPeak) _peak = _minPeak;
    final norm = ((level - _floor) / (_peak - _floor)).clamp(0.0, 1.0);
    final lit = (norm * widget.bars).round();
    return Row(
      key: const Key('enroll-mic-meter'),
      children: [
        for (var i = 0; i < widget.bars; i++)
          Expanded(
            child: AnimatedContainer(
              duration: const Duration(milliseconds: 90),
              margin: const EdgeInsets.symmetric(horizontal: 1.5),
              height: 8 + 10.0 * (i / widget.bars),
              decoration: BoxDecoration(
                color: i < lit
                    ? widget.color
                    : widget.color.withValues(alpha: 0.15),
                borderRadius: BorderRadius.circular(2),
              ),
            ),
          ),
      ],
    );
  }
}
