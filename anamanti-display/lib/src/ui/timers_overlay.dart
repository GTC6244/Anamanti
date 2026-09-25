// On-device countdown timers overlay (Plan.MD §3 device actions; Phase 2).
//
// Two presentations of the same live timers, driven by [compact]:
//
//  * Big (compact=false) — the idle presentation. Uses most of the screen so a
//    running timer is readable across the room: one timer fills the screen, two or
//    three sit side by side, four or more tile into a grid. Each timer shows its
//    name, a large mm:ss readout, and a circular progress ring that drains from a
//    full circle to empty as the time runs out.
//  * Compact (compact=true) — a small row of chips shown top-center while a
//    conversation is on screen, so the timers yield the screen to the live turn but
//    stay glanceable.
//
// The device owns the authoritative countdown + alarm; this widget just interpolates
// the remaining time to each timer's deadline (and the ring's fill to its total) with
// its own ~half-second ticker, so it needs no per-second events from Rust. Tapping a
// finished (ringing) timer dismisses it via [onDismiss].

import 'dart:async';
import 'dart:math' as math;

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/assistant_controller.dart';

const _amber = Color(0xFFFFD59E);
const _red = Color(0xFFFFB4A2);

/// Resolve a display name per timer id: the spoken label when present, otherwise a
/// generic "Timer" — numbered ("Timer 2") only when several unnamed timers coexist,
/// so a lone unnamed timer just reads "Timer".
Map<int, String> resolveTimerNames(List<TimerModel> timers) {
  final unnamed = timers.where((t) => t.label.trim().isEmpty).length;
  final names = <int, String>{};
  var n = 0;
  for (final t in timers) {
    final label = t.label.trim();
    names[t.id] =
        label.isNotEmpty ? label : (unnamed > 1 ? 'Timer ${++n}' : 'Timer');
  }
  return names;
}

/// Live countdown timers. [compact] picks the small chip row (over a conversation)
/// vs. the big screen-filling grid (idle).
class TimersOverlay extends StatefulWidget {
  const TimersOverlay({
    super.key,
    required this.timers,
    required this.onDismiss,
    this.compact = false,
    this.clock,
  });

  final List<TimerModel> timers;
  final void Function(int id) onDismiss;

  /// Small chip row (true) vs. big screen-filling layout (false, the default).
  final bool compact;

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
    // Repaint twice a second so the mm:ss readout and the ring stay current.
    // Cancelled in dispose so it never outlives the widget (widget-test invariant).
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
    final names = resolveTimerNames(widget.timers);
    return widget.compact
        ? _CompactTimers(timers: widget.timers, now: now, names: names, onDismiss: widget.onDismiss)
        // SafeArea so the screen-filling grid never hides under the system nav/status
        // bars (the app runs without immersive mode); a no-op where there are no insets.
        : SafeArea(
            child: _FullTimers(
                timers: widget.timers, now: now, names: names, onDismiss: widget.onDismiss),
          );
  }
}

/// The small chip row shown over a live conversation.
class _CompactTimers extends StatelessWidget {
  const _CompactTimers({
    required this.timers,
    required this.now,
    required this.names,
    required this.onDismiss,
  });

  final List<TimerModel> timers;
  final DateTime now;
  final Map<int, String> names;
  final void Function(int id) onDismiss;

  @override
  Widget build(BuildContext context) {
    return SingleChildScrollView(
      scrollDirection: Axis.horizontal,
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          for (final t in timers)
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 4),
              child: _TimerChip(
                timer: t,
                name: names[t.id] ?? 'Timer',
                now: now,
                onDismiss: () => onDismiss(t.id),
              ),
            ),
        ],
      ),
    );
  }
}

/// The big screen-filling layout: single / vertical stack / grid by count.
class _FullTimers extends StatelessWidget {
  const _FullTimers({
    required this.timers,
    required this.now,
    required this.names,
    required this.onDismiss,
  });

  final List<TimerModel> timers;
  final DateTime now;
  final Map<int, String> names;
  final void Function(int id) onDismiss;

  Widget _card(TimerModel t) => _TimerCard(
        timer: t,
        name: names[t.id] ?? 'Timer',
        now: now,
        onDismiss: () => onDismiss(t.id),
      );

  @override
  Widget build(BuildContext context) {
    final n = timers.length;

    // One timer: fill the screen.
    if (n == 1) {
      return Padding(padding: const EdgeInsets.all(24), child: _card(timers.first));
    }

    // Two or three: stack horizontally, each taking an equal vertical column.
    if (n <= 3) {
      return Padding(
        padding: const EdgeInsets.all(16),
        child: Row(
          children: [
            for (final t in timers)
              Expanded(
                child: Padding(
                  padding: const EdgeInsets.symmetric(horizontal: 8),
                  child: _card(t),
                ),
              ),
          ],
        ),
      );
    }

    // Four or more: a grid sized to fill the available space exactly (no scroll).
    final cols = n <= 4 ? 2 : (n <= 9 ? 3 : 4);
    final rows = (n / cols).ceil();
    return Padding(
      padding: const EdgeInsets.all(16),
      child: LayoutBuilder(
        builder: (context, c) {
          const gap = 12.0;
          final cellW = (c.maxWidth - gap * (cols - 1)) / cols;
          final cellH = (c.maxHeight - gap * (rows - 1)) / rows;
          return GridView.count(
            crossAxisCount: cols,
            mainAxisSpacing: gap,
            crossAxisSpacing: gap,
            childAspectRatio: cellW / cellH,
            physics: const NeverScrollableScrollPhysics(),
            children: [for (final t in timers) _card(t)],
          );
        },
      ),
    );
  }
}

/// A single big timer: name heading, a draining progress ring, and the mm:ss (or
/// "Time's up") readout centered inside the ring. Sizes itself to its slot.
class _TimerCard extends StatelessWidget {
  const _TimerCard({
    required this.timer,
    required this.name,
    required this.now,
    required this.onDismiss,
  });

  final TimerModel timer;
  final String name;
  final DateTime now;
  final VoidCallback onDismiss;

  @override
  Widget build(BuildContext context) {
    final remaining = timer.deadline.difference(now);
    final ringing = timer.finished || remaining <= Duration.zero;
    final accent = ringing ? _red : _amber;

    // Ring fill drains from 1 (full) at start to 0 at zero.
    final total = timer.total.inMilliseconds;
    final fraction = ringing || total <= 0
        ? 0.0
        : (remaining.inMilliseconds / total).clamp(0.0, 1.0);

    final card = LayoutBuilder(
      builder: (context, c) {
        final side = math.min(c.maxWidth, c.maxHeight);
        final nameFont = (side * 0.10).clamp(14.0, 48.0);

        return Column(
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            Text(
              name,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              textAlign: TextAlign.center,
              style: TextStyle(
                color: accent,
                fontSize: nameFont,
                fontWeight: FontWeight.w600,
                letterSpacing: 0.5,
              ),
            ),
            SizedBox(height: side * 0.04),
            // The ring takes whatever space is left below the name and stays a
            // circle, so name + ring always fit their slot (no overflow in a grid).
            Expanded(
              child: Center(
                child: AspectRatio(
                  aspectRatio: 1,
                  child: LayoutBuilder(
                    builder: (context, rc) {
                      final diameter = math.min(rc.maxWidth, rc.maxHeight);
                      final stroke = (diameter * 0.06).clamp(4.0, 26.0);
                      final timeFont = diameter * (ringing ? 0.16 : 0.24);
                      final readout = ringing ? "Time's up" : formatRemaining(remaining);
                      return CustomPaint(
                        painter: _RingPainter(fraction: fraction, accent: accent, stroke: stroke),
                        child: Center(
                          child: Padding(
                            padding: EdgeInsets.all(stroke * 1.6),
                            child: FittedBox(
                              fit: BoxFit.scaleDown,
                              child: Column(
                                mainAxisSize: MainAxisSize.min,
                                children: [
                                  Text(
                                    readout,
                                    style: TextStyle(
                                      color: accent,
                                      fontSize: timeFont,
                                      fontWeight: FontWeight.w700,
                                      fontFeatures: const [FontFeature.tabularFigures()],
                                      height: 1.0,
                                    ),
                                  ),
                                  if (ringing)
                                    Padding(
                                      padding: EdgeInsets.only(top: diameter * 0.03),
                                      child: Text(
                                        'Tap to dismiss',
                                        style: TextStyle(
                                          color: accent.withValues(alpha: 0.75),
                                          fontSize: diameter * 0.06,
                                          fontWeight: FontWeight.w500,
                                        ),
                                      ),
                                    ),
                                ],
                              ),
                            ),
                          ),
                        ),
                      );
                    },
                  ),
                ),
              ),
            ),
          ],
        );
      },
    );

    // Only ringing timers are tappable (their alarm already sounded); a running
    // timer isn't dismissable so a stray touch can't clear it.
    if (!ringing) return card;
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTap: onDismiss,
      child: card,
    );
  }
}

/// Paints the timer ring: a faint full-circle track plus an [accent] arc filling
/// [fraction] of the circle, sweeping clockwise from 12 o'clock. Draining the
/// fraction toward 0 shrinks the arc back to nothing as time runs out.
class _RingPainter extends CustomPainter {
  const _RingPainter({required this.fraction, required this.accent, required this.stroke});

  final double fraction;
  final Color accent;
  final double stroke;

  @override
  void paint(Canvas canvas, Size size) {
    final rect = Offset.zero & size;
    final center = rect.center;
    final radius = (math.min(size.width, size.height) - stroke) / 2;

    final track = Paint()
      ..style = PaintingStyle.stroke
      ..strokeWidth = stroke
      ..color = accent.withValues(alpha: 0.18);
    canvas.drawCircle(center, radius, track);

    if (fraction <= 0) return;
    final arc = Paint()
      ..style = PaintingStyle.stroke
      ..strokeWidth = stroke
      ..strokeCap = StrokeCap.round
      ..color = accent;
    canvas.drawArc(
      Rect.fromCircle(center: center, radius: radius),
      -math.pi / 2, // 12 o'clock
      2 * math.pi * fraction, // clockwise
      false,
      arc,
    );
  }

  @override
  bool shouldRepaint(_RingPainter old) =>
      old.fraction != fraction || old.accent != accent || old.stroke != stroke;
}

/// A compact countdown chip (used over a live conversation).
class _TimerChip extends StatelessWidget {
  const _TimerChip({
    required this.timer,
    required this.name,
    required this.now,
    required this.onDismiss,
  });

  final TimerModel timer;
  final String name;
  final DateTime now;
  final VoidCallback onDismiss;

  @override
  Widget build(BuildContext context) {
    final remaining = timer.deadline.difference(now);
    final ringing = timer.finished || remaining <= Duration.zero;
    final accent = ringing ? _red : _amber;

    final text = ringing
        ? '$name — time\'s up'
        : '$name  ${formatRemaining(remaining)}';

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
