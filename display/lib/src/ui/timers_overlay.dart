// On-device countdown timers overlay (Plan.MD §3 device actions; Phase 2).
//
// Renders a horizontally-scrollable row of countdown chips for an unlimited number
// of concurrent timers. It lives directly in the always-visible Stack (not the
// turn-gated conversation layer) so timers show during both the idle slideshow and a
// live conversation. The device owns the authoritative countdown + alarm; this
// widget just interpolates the remaining time to each timer's deadline with its own
// ~half-second ticker, so it needs no per-second events from Rust.

import 'dart:async';

import 'package:flutter/material.dart';

import 'package:ambient_display/src/engine/assistant_controller.dart';

/// A row of live countdown chips. Tapping a finished (ringing) chip dismisses it via
/// [onDismiss].
class TimersOverlay extends StatefulWidget {
  const TimersOverlay({
    super.key,
    required this.timers,
    required this.onDismiss,
    this.clock,
  });

  final List<TimerModel> timers;
  final void Function(int id) onDismiss;

  /// Injectable clock for deterministic tests; defaults to [DateTime.now].
  final DateTime Function()? clock;

  @override
  State<TimersOverlay> createState() => _TimersOverlayState();
}

class _TimersOverlayState extends State<TimersOverlay> {
  Timer? _ticker;

  @override
  void initState() {
    super.initState();
    // Repaint twice a second so the mm:ss readout stays current. Cancelled in
    // dispose so it never outlives the widget (widget-test invariant).
    _ticker = Timer.periodic(const Duration(milliseconds: 500), (_) {
      if (mounted) setState(() {});
    });
  }

  @override
  void dispose() {
    _ticker?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    if (widget.timers.isEmpty) return const SizedBox.shrink();
    final now = (widget.clock ?? DateTime.now)();
    return SingleChildScrollView(
      scrollDirection: Axis.horizontal,
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          for (final t in widget.timers)
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 4),
              child: _TimerChip(
                timer: t,
                now: now,
                onDismiss: () => widget.onDismiss(t.id),
              ),
            ),
        ],
      ),
    );
  }
}

class _TimerChip extends StatelessWidget {
  const _TimerChip({required this.timer, required this.now, required this.onDismiss});

  final TimerModel timer;
  final DateTime now;
  final VoidCallback onDismiss;

  static const _amber = Color(0xFFFFD59E);
  static const _red = Color(0xFFFFB4A2);

  @override
  Widget build(BuildContext context) {
    final remaining = timer.deadline.difference(now);
    final ringing = timer.finished || remaining <= Duration.zero;
    final accent = ringing ? _red : _amber;
    final label = timer.label.trim();

    final text = ringing
        ? (label.isEmpty ? "Time's up" : "$label — time's up")
        : (label.isEmpty ? formatRemaining(remaining) : '$label  ${formatRemaining(remaining)}');

    final chip = Container(
      padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 8),
      decoration: BoxDecoration(
        color: Colors.black.withValues(alpha: ringing ? 0.55 : 0.35),
        borderRadius: BorderRadius.circular(999),
        border: Border.all(color: accent.withValues(alpha: ringing ? 0.9 : 0.4)),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Icon(ringing ? Icons.alarm_on : Icons.timer_outlined, size: 18, color: accent),
          const SizedBox(width: 8),
          Text(
            text,
            style: TextStyle(
              color: accent,
              fontSize: 18,
              fontFeatures: const [FontFeature.tabularFigures()],
              fontWeight: ringing ? FontWeight.w700 : FontWeight.w500,
            ),
          ),
          if (ringing) ...[
            const SizedBox(width: 6),
            Icon(Icons.close, size: 16, color: accent.withValues(alpha: 0.8)),
          ],
        ],
      ),
    );

    // Only ringing chips are dismissable (the alarm already sounded); a running
    // timer isn't tappable so a stray touch can't clear it.
    if (!ringing) return chip;
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTap: onDismiss,
      child: chip,
    );
  }
}

/// Format a countdown as `m:ss` (or `h:mm:ss` past an hour); negatives clamp to
/// zero. Seconds and minutes are zero-padded; the lead unit is not.
String formatRemaining(Duration d) {
  if (d.isNegative) d = Duration.zero;
  final h = d.inHours;
  final m = d.inMinutes % 60;
  final s = d.inSeconds % 60;
  final ss = s.toString().padLeft(2, '0');
  if (h > 0) {
    final mm = m.toString().padLeft(2, '0');
    return '$h:$mm:$ss';
  }
  return '$m:$ss';
}
