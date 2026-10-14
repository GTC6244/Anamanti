// Audio diagnostics screen (Settings → Audio Diagnostics).
//
// A visual tuning surface for the microphone + wake-word pipeline. It "turns on"
// the mic monitor and renders, in real time from the always-on wake-word engine's
// event stream (folded into [AssistantState] by [AssistantController]):
//   * a live mic input-level (RMS) meter, on a dB scale so quiet far-field speech
//     is visible — the direct signal for the documented mic-gain tuning; its header
//     line also carries the mic-monitor switch (which only pauses/resumes this
//     visualization — the wake-word mic itself is always on);
//   * a live wake-word confidence meter with the firing threshold drawn as a line,
//     so you can say the wake word and watch how close it gets to triggering;
//   * a flash + short history each time the wake word actually fires; and
//   * exact numeric readouts (RMS, dBFS, score, smoothed score, threshold, gain,
//     capture device / sample rate / channels), with a copy-to-clipboard button
//     that exports the current config + live readouts as plain text.
//
// The cards are laid out in two columns with tightened spacing so the whole surface
// fits the Echo Show's wide landscape panel on one screen without scrolling.
//
// The two tuning sliders (capture gain + wake-word sensitivity) apply **live** to
// the running engine via `updateDiagnosticsTuning` — no restart — so the meters
// react as you drag. Changes are mirrored into the parent's [AppSettings] via
// [onChanged] so the settings-screen Save button persists them across restarts.

import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/settings/app_settings.dart';

/// How many recent detections the history card keeps.
const int _kMaxHistory = 4;

/// One recorded wake-word detection, for the history list.
class _DetectionRecord {
  const _DetectionRecord(this.at, this.score);
  final DateTime at;
  final double score;
}

class AudioDiagnosticsView extends StatefulWidget {
  const AudioDiagnosticsView({
    super.key,
    required this.controller,
    required this.settings,
    required this.onChanged,
    required this.onTune,
    required this.onActiveChanged,
  });

  /// The live, already-running wake-word engine controller. Its [AssistantState]
  /// carries the mic level + wake-word diagnostics this screen renders.
  final AssistantController controller;

  /// Current device-local settings (source of the slider values).
  final AppSettings settings;

  /// Report a slider change to the parent, which persists it (Settings applies
  /// changes immediately). Does not itself apply anything to the engine.
  final ValueChanged<AppSettings> onChanged;

  /// Apply a capture-gain (dB) + idle-threshold change to the **running** engine
  /// immediately (no restart), so the meters react as the user drags.
  final void Function(double gainDb, double threshold) onTune;

  /// Announce to the engine that this diagnostics screen is (`true`) or is no longer
  /// (`false`) foreground. Production wires it to the native `setDiagnosticsContext`,
  /// which stamps a transcription-only display context on each voice turn's
  /// `audio-start` so the Core transcribes and shows the words but never sends them to
  /// the LLM. Called with `true` on mount and `false` on dispose.
  final void Function(bool active) onActiveChanged;

  @override
  State<AudioDiagnosticsView> createState() => _AudioDiagnosticsViewState();
}

class _AudioDiagnosticsViewState extends State<AudioDiagnosticsView>
    with SingleTickerProviderStateMixin {
  /// Whether the live meters are updating. The wake-word mic itself is always on
  /// (it must be, to hear the wake word); this only pauses/resumes the visualization.
  bool _monitoring = true;

  late final AnimationController _flash;
  final List<_DetectionRecord> _history = <_DetectionRecord>[];
  int _lastSeq = 0;

  @override
  void initState() {
    super.initState();
    _flash = AnimationController(
      vsync: this,
      duration: const Duration(milliseconds: 700),
    );
    _lastSeq = widget.controller.state.detectionSeq;
    widget.controller.addListener(_onTick);
    // Tell the Core this is the diagnostics screen: subsequent voice turns transcribe
    // and display the words but are NOT sent to the LLM (diagnostic surface only).
    widget.onActiveChanged(true);
  }

  @override
  void didUpdateWidget(AudioDiagnosticsView old) {
    super.didUpdateWidget(old);
    // An engine restart (e.g. a setting applied from Settings) replaces the
    // controller while this screen stays up: follow the new one. The
    // transcription-only context is deliberately NOT re-announced — it's a single
    // global flag, and toggling it here could leave it cleared while the page is up.
    if (!identical(old.controller, widget.controller)) {
      old.controller.removeListener(_onTick);
      _lastSeq = widget.controller.state.detectionSeq;
      widget.controller.addListener(_onTick);
    }
  }

  @override
  void dispose() {
    // Leaving the diagnostics screen: clear the transcription-only context so normal
    // (LLM-backed) turns resume elsewhere.
    widget.onActiveChanged(false);
    widget.controller.removeListener(_onTick);
    _flash.dispose();
    super.dispose();
  }

  /// Fires on every controller change; only acts when a *new* detection lands.
  void _onTick() {
    final s = widget.controller.state;
    if (s.detectionSeq == _lastSeq) return;
    _lastSeq = s.detectionSeq;
    if (!_monitoring || !mounted) return;
    _flash.forward(from: 0);
    setState(() {
      _history.insert(
        0,
        _DetectionRecord(DateTime.now(), s.lastDetectionScore),
      );
      // Keep the last few only, so the card never grows the page past one screen.
      if (_history.length > _kMaxHistory) _history.removeLast();
    });
  }

  /// Map an RMS amplitude (~0..1, but speech is often ~0.003..0.2) onto a 0..1 bar
  /// fraction using a dBFS scale, so quiet far-field audio is still visible.
  double _rmsFraction(double rms) {
    if (rms <= 0) return 0;
    final db = 20 * (math.log(rms) / math.ln10); // dBFS (negative)
    return ((db + 60) / 60).clamp(0.0, 1.0);
  }

  String _dbfs(double rms) {
    if (rms <= 0) return '−∞ dB';
    final db = 20 * (math.log(rms) / math.ln10);
    return '${db.toStringAsFixed(1)} dB';
  }

  @override
  Widget build(BuildContext context) {
    // The Echo Show is a wide landscape panel, so lay the cards out in two
    // columns to keep everything on one screen without scrolling. The outer
    // scroll view is a safety net for shorter test surfaces only.
    return SingleChildScrollView(
      padding: const EdgeInsets.fromLTRB(16, 12, 16, 16),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                _metersCard(),
                const SizedBox(height: 12),
                _tuningCard(),
                const SizedBox(height: 12),
                _historyCard(),
              ],
            ),
          ),
          const SizedBox(width: 12),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                _readoutsCard(),
                const SizedBox(height: 12),
                _transcriptCard(),
              ],
            ),
          ),
        ],
      ),
    );
  }

  // --- Clipboard export ----------------------------------------------------

  /// A plain-text snapshot of the current audio/wake-word configuration plus the
  /// live readouts — copied to the clipboard for pasting into a bug report or
  /// sharing while tuning.
  String _configText() {
    final s = widget.controller.state;
    final threshold = s.wakeThreshold > 0
        ? s.wakeThreshold
        : widget.settings.threshold;
    final device = s.captureDevice.isEmpty ? '—' : s.captureDevice;
    final rate = s.captureSampleRate > 0 ? '${s.captureSampleRate} Hz' : '—';
    final chans = s.captureChannels > 0 ? '${s.captureChannels}' : '—';
    return (StringBuffer()
          ..writeln('Anamanti audio diagnostics')
          ..writeln('Wake word: ${widget.settings.wakeWord}')
          ..writeln('Capture device: $device')
          ..writeln('Sample rate: $rate')
          ..writeln('Channels: $chans')
          ..writeln('Capture gain: ${s.captureGainDb.toStringAsFixed(0)} dB')
          ..writeln('Idle threshold: ${threshold.toStringAsFixed(2)}')
          ..writeln(
            'Active threshold: '
            '${widget.settings.activeThreshold.toStringAsFixed(2)}',
          )
          ..writeln('Smoothing window: ${widget.settings.smoothingWindow}')
          ..writeln('Fire on peak: ${widget.settings.fireOnPeak}')
          ..writeln('AudioRecord input: ${widget.settings.useAudioRecord}')
          ..writeln(
            'Platform NS/AGC/AEC: ${widget.settings.platformNs}/'
            '${widget.settings.platformAgc}/${widget.settings.platformAec}',
          )
          ..writeln('— live —')
          ..writeln(
            'RMS: ${s.micLevel.toStringAsFixed(4)} (${_dbfs(s.micLevel)})',
          )
          ..writeln('Score: ${s.wakeScore.toStringAsFixed(3)}')
          ..writeln('Smoothed: ${s.wakeAvgScore.toStringAsFixed(3)}'))
        .toString();
  }

  Future<void> _copyConfig() async {
    await Clipboard.setData(ClipboardData(text: _configText()));
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(
      const SnackBar(
        content: Text('Diagnostics config copied to clipboard'),
        duration: Duration(seconds: 2),
      ),
    );
  }

  // --- Meters --------------------------------------------------------------

  Widget _metersCard() {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: AnimatedBuilder(
          animation: Listenable.merge([widget.controller, _flash]),
          builder: (context, _) {
            final s = widget.controller.state;
            final live = _monitoring;
            final rms = live ? s.micLevel : 0.0;
            final score = live ? s.wakeScore : 0.0;
            final avg = live ? s.wakeAvgScore : 0.0;
            // Prefer the engine-reported live threshold; fall back to the setting
            // before the first level event arrives.
            final threshold = s.wakeThreshold > 0
                ? s.wakeThreshold
                : widget.settings.threshold;
            final fired = score >= threshold && threshold > 0;

            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                // The mic-monitor switch lives on the mic-input line: it only
                // pauses/resumes this visualization — the wake-word mic itself
                // is always on (it must be, to hear the wake word).
                Row(
                  children: [
                    Expanded(
                      child: _meterHeader(
                        'Mic input level',
                        live ? _dbfs(rms) : 'paused',
                      ),
                    ),
                    const SizedBox(width: 8),
                    Switch.adaptive(
                      key: const Key('diag-monitor-toggle'),
                      value: _monitoring,
                      materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
                      onChanged: (v) => setState(() => _monitoring = v),
                    ),
                  ],
                ),
                const SizedBox(height: 6),
                _MeterBar(
                  fraction: _rmsFraction(rms),
                  color: Colors.tealAccent.shade400,
                ),
                const SizedBox(height: 14),
                Row(
                  children: [
                    Expanded(
                      child: _meterHeader(
                        'Wake-word score',
                        live ? score.toStringAsFixed(3) : 'paused',
                      ),
                    ),
                    AnimatedOpacity(
                      opacity: _flash.value,
                      duration: Duration.zero,
                      child: Container(
                        padding: const EdgeInsets.symmetric(
                          horizontal: 10,
                          vertical: 4,
                        ),
                        decoration: BoxDecoration(
                          color: Colors.greenAccent.shade700,
                          borderRadius: BorderRadius.circular(12),
                        ),
                        child: const Text(
                          'DETECTED',
                          style: TextStyle(
                            fontWeight: FontWeight.bold,
                            fontSize: 12,
                          ),
                        ),
                      ),
                    ),
                  ],
                ),
                const SizedBox(height: 6),
                _MeterBar(
                  fraction: score.clamp(0.0, 1.0),
                  color: fired
                      ? Colors.greenAccent.shade400
                      : (score >= threshold * 0.6
                            ? Colors.amber
                            : Colors.lightBlueAccent),
                  markerFraction: threshold.clamp(0.0, 1.0),
                  secondaryFraction: avg.clamp(0.0, 1.0),
                ),
                const SizedBox(height: 6),
                Row(
                  mainAxisAlignment: MainAxisAlignment.spaceBetween,
                  children: [
                    Text(
                      'smoothed ${avg.toStringAsFixed(3)}',
                      style: Theme.of(context).textTheme.bodySmall,
                    ),
                    Text(
                      'threshold ${threshold.toStringAsFixed(2)}',
                      style: Theme.of(context).textTheme.bodySmall,
                    ),
                  ],
                ),
              ],
            );
          },
        ),
      ),
    );
  }

  Widget _meterHeader(String label, String value) {
    return Row(
      mainAxisAlignment: MainAxisAlignment.spaceBetween,
      children: [
        Flexible(
          child: Text(
            label,
            overflow: TextOverflow.ellipsis,
            style: Theme.of(context).textTheme.titleSmall,
          ),
        ),
        Text(
          value,
          style: Theme.of(context).textTheme.titleSmall?.copyWith(
            fontFeatures: const [FontFeature.tabularFigures()],
            color: Theme.of(context).colorScheme.primary,
          ),
        ),
      ],
    );
  }

  // --- Transcript (diagnostic, never sent to the LLM) ----------------------

  /// Shows what the server transcribed from the last thing you said while this screen
  /// is up. The device stamps a transcription-only display context on each turn's
  /// `audio-start`, so the Core transcribes and relays the words here but does NOT send
  /// them to the LLM — this screen is diagnostic only (no spoken reply). Say the wake
  /// word, then talk, and the recognized words appear below.
  Widget _transcriptCard() {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: AnimatedBuilder(
          animation: widget.controller,
          builder: (context, _) {
            final theme = Theme.of(context);
            final transcript = widget.controller.state.transcript.trim();
            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  children: [
                    const Icon(Icons.hearing, size: 18),
                    const SizedBox(width: 8),
                    Text('Heard words', style: theme.textTheme.titleSmall),
                  ],
                ),
                const SizedBox(height: 4),
                Text(
                  'Diagnostic only — these words are shown but never sent to the '
                  'assistant. Say the wake word, then speak.',
                  style: theme.textTheme.bodySmall?.copyWith(
                    color: theme.hintColor,
                  ),
                ),
                const SizedBox(height: 12),
                Text(
                  key: const Key('diag-transcript'),
                  transcript.isEmpty ? '—' : transcript,
                  maxLines: 2,
                  overflow: TextOverflow.ellipsis,
                  style: theme.textTheme.headlineSmall?.copyWith(
                    color: transcript.isEmpty ? theme.disabledColor : null,
                  ),
                ),
              ],
            );
          },
        ),
      ),
    );
  }

  // --- Numeric readouts ----------------------------------------------------

  Widget _readoutsCard() {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: AnimatedBuilder(
          animation: widget.controller,
          builder: (context, _) {
            final s = widget.controller.state;
            final device = s.captureDevice.isEmpty ? '—' : s.captureDevice;
            final rate = s.captureSampleRate > 0
                ? '${s.captureSampleRate} Hz'
                : '—';
            final chans = s.captureChannels > 0 ? '${s.captureChannels}' : '—';
            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  mainAxisAlignment: MainAxisAlignment.spaceBetween,
                  children: [
                    Text(
                      'Readouts',
                      style: Theme.of(context).textTheme.titleSmall,
                    ),
                    IconButton(
                      key: const Key('diag-copy-config'),
                      icon: const Icon(Icons.copy_all, size: 18),
                      tooltip: 'Copy config to clipboard',
                      visualDensity: VisualDensity.compact,
                      onPressed: _copyConfig,
                    ),
                  ],
                ),
                const SizedBox(height: 8),
                LayoutBuilder(
                  // Three readouts per row at whatever width the column has, so the
                  // card stays short (the page must fit one screen).
                  builder: (context, box) {
                    final w = (box.maxWidth - 20) / 3;
                    final threshold = s.wakeThreshold > 0
                        ? s.wakeThreshold
                        : widget.settings.threshold;
                    return Wrap(
                      spacing: 10,
                      runSpacing: 10,
                      children: [
                        _stat(w, 'RMS', s.micLevel.toStringAsFixed(4)),
                        _stat(w, 'Level', _dbfs(s.micLevel)),
                        _stat(w, 'Score', s.wakeScore.toStringAsFixed(3)),
                        _stat(w, 'Smoothed', s.wakeAvgScore.toStringAsFixed(3)),
                        _stat(w, 'Threshold', threshold.toStringAsFixed(2)),
                        _stat(
                          w,
                          'Gain',
                          '${s.captureGainDb.toStringAsFixed(0)} dB',
                        ),
                        _stat(w, 'Device', device),
                        _stat(w, 'Rate', rate),
                        _stat(w, 'Channels', chans),
                      ],
                    );
                  },
                ),
              ],
            );
          },
        ),
      ),
    );
  }

  Widget _stat(double width, String label, String value) {
    return SizedBox(
      width: width,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            label.toUpperCase(),
            style: Theme.of(context).textTheme.labelSmall?.copyWith(
              letterSpacing: 0.8,
              color: Theme.of(context).colorScheme.outline,
            ),
          ),
          const SizedBox(height: 2),
          Text(
            value,
            overflow: TextOverflow.ellipsis,
            style: Theme.of(context).textTheme.titleMedium?.copyWith(
              fontFeatures: const [FontFeature.tabularFigures()],
            ),
          ),
        ],
      ),
    );
  }

  // --- Live tuning ---------------------------------------------------------

  Widget _tuningCard() {
    final gain = widget.settings.captureGainDb.clamp(0.0, 36.0);
    final threshold = widget.settings.threshold.clamp(0.1, 0.95);
    return Card(
      child: Padding(
        padding: const EdgeInsets.fromLTRB(12, 12, 12, 4),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Live tuning', style: Theme.of(context).textTheme.titleSmall),
            const SizedBox(height: 4),
            Text(
              'Applies to the mic instantly; saved automatically.',
              style: Theme.of(context).textTheme.bodySmall,
            ),
            const SizedBox(height: 8),
            _tuneSlider(
              label: 'Capture gain',
              value: gain,
              min: 0,
              max: 36,
              divisions: 36,
              format: (v) => '${v.round()} dB',
              sliderKey: const Key('diag-gain'),
              onChanged: (v) {
                final next = widget.settings.copyWith(
                  captureGainDb: v.roundToDouble(),
                );
                widget.onChanged(next);
                widget.onTune(next.captureGainDb, next.threshold);
              },
            ),
            _tuneSlider(
              label: 'Sensitivity (idle)',
              value: threshold,
              min: 0.1,
              max: 0.95,
              divisions: 17,
              format: (v) => v.toStringAsFixed(2),
              sliderKey: const Key('diag-threshold'),
              onChanged: (v) {
                final next = widget.settings.copyWith(threshold: v);
                widget.onChanged(next);
                widget.onTune(next.captureGainDb, next.threshold);
              },
            ),
          ],
        ),
      ),
    );
  }

  Widget _tuneSlider({
    required String label,
    required double value,
    required double min,
    required double max,
    required int divisions,
    required String Function(double) format,
    required ValueChanged<double> onChanged,
    Key? sliderKey,
  }) {
    return Row(
      children: [
        SizedBox(width: 130, child: Text(label)),
        Expanded(
          child: Slider(
            key: sliderKey,
            min: min,
            max: max,
            divisions: divisions,
            value: value.clamp(min, max),
            label: format(value),
            onChanged: onChanged,
          ),
        ),
        SizedBox(width: 52, child: Text(format(value))),
      ],
    );
  }

  // --- Detection history ---------------------------------------------------

  Widget _historyCard() {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Expanded(
                  child: Text(
                    'Recent detections',
                    style: Theme.of(context).textTheme.titleSmall,
                  ),
                ),
                if (_history.isNotEmpty)
                  InkWell(
                    key: const Key('diag-history-clear'),
                    onTap: () => setState(_history.clear),
                    child: const Padding(
                      padding: EdgeInsets.symmetric(horizontal: 6, vertical: 2),
                      child: Text('Clear', style: TextStyle(fontSize: 13)),
                    ),
                  ),
              ],
            ),
            const SizedBox(height: 4),
            if (_history.isEmpty)
              Text(
                'Say the wake word — fires appear here with the score they hit.',
                style: Theme.of(context).textTheme.bodySmall,
              )
            else
              // Compact entries, two per row, so the card's height stays small and
              // fixed and the page fits one screen.
              LayoutBuilder(
                builder: (context, box) => Wrap(
                  spacing: 12,
                  runSpacing: 4,
                  children: [
                    for (final d in _history)
                      SizedBox(
                        width: (box.maxWidth - 12) / 2,
                        child: Row(
                          children: [
                            const Icon(
                              Icons.graphic_eq,
                              size: 14,
                              color: Colors.greenAccent,
                            ),
                            const SizedBox(width: 6),
                            Text(_fmtTime(d.at)),
                            const Spacer(),
                            Text(
                              d.score.toStringAsFixed(3),
                              style: const TextStyle(
                                fontFeatures: [FontFeature.tabularFigures()],
                              ),
                            ),
                          ],
                        ),
                      ),
                  ],
                ),
              ),
          ],
        ),
      ),
    );
  }

  String _fmtTime(DateTime t) {
    String two(int n) => n.toString().padLeft(2, '0');
    return '${two(t.hour)}:${two(t.minute)}:${two(t.second)}';
  }
}

/// A horizontal level meter: a filled [fraction] of the track in [color], with an
/// optional [markerFraction] line (the threshold) and an optional [secondaryFraction]
/// tick (the smoothed score).
class _MeterBar extends StatelessWidget {
  const _MeterBar({
    required this.fraction,
    required this.color,
    this.markerFraction,
    this.secondaryFraction,
  });

  final double fraction;
  final Color color;
  final double? markerFraction;
  final double? secondaryFraction;

  static const double height = 24;

  @override
  Widget build(BuildContext context) {
    final radius = BorderRadius.circular(6);
    return LayoutBuilder(
      builder: (context, constraints) {
        final w = constraints.maxWidth;
        final marker = markerFraction;
        final secondary = secondaryFraction;
        return SizedBox(
          height: height,
          child: Stack(
            children: [
              // Track.
              Container(
                decoration: BoxDecoration(
                  color: Colors.white.withValues(alpha: 0.08),
                  borderRadius: radius,
                ),
              ),
              // Fill.
              FractionallySizedBox(
                widthFactor: fraction.clamp(0.0, 1.0),
                child: Container(
                  decoration: BoxDecoration(color: color, borderRadius: radius),
                ),
              ),
              // Smoothed-score tick (thin, lighter).
              if (secondary != null)
                Positioned(
                  left: (secondary.clamp(0.0, 1.0) * w) - 1,
                  top: 2,
                  bottom: 2,
                  child: Container(
                    width: 2,
                    color: Colors.white.withValues(alpha: 0.5),
                  ),
                ),
              // Threshold marker (bright).
              if (marker != null)
                Positioned(
                  left: (marker.clamp(0.0, 1.0) * w) - 1.5,
                  child: Container(
                    width: 3,
                    height: height,
                    color: Colors.white,
                  ),
                ),
            ],
          ),
        );
      },
    );
  }
}
