// The always-on ambient display screen (Plan.MD §3, Phase 5).
//
// Layered, landscape-first layout tuned for the 8-inch Echo Show:
//  * Background: the idle photo slideshow (always running).
//  * A scrim + live conversation panel fades in while a turn is active.
//  * A subtle clock (idle) and connection-status chip sit in the corners.
//
// The screen is driven by two [ChangeNotifier]s — [AssistantController] for the
// voice turn and [SlideshowController] for the idle imagery — so the photo cycle
// is fully decoupled from connectivity and keeps running when the Mac is offline.

import 'package:flutter/material.dart';

import 'package:ambient_display/src/engine/assistant_controller.dart';
import 'package:ambient_display/src/slideshow/photo_source.dart';
import 'package:ambient_display/src/ui/conversation_view.dart';
import 'package:ambient_display/src/ui/slideshow_view.dart';
import 'package:ambient_display/src/ui/status_indicator.dart';
import 'package:ambient_display/src/ui/timers_overlay.dart';

class AmbientScreen extends StatelessWidget {
  const AmbientScreen({
    super.key,
    required this.assistant,
    required this.slideshow,
    this.onOpenSettings,
  });

  final AssistantController assistant;
  final SlideshowController slideshow;

  /// Opens the settings screen (Phase 6). When null, no settings control is shown
  /// (e.g. in widget tests that only exercise the reactive turn UI).
  final VoidCallback? onOpenSettings;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: Colors.black,
      body: AnimatedBuilder(
        animation: assistant,
        builder: (context, _) {
          final state = assistant.state;
          final active = state.turnActive;
          return Stack(
            fit: StackFit.expand,
            children: [
              // Idle imagery, always cycling underneath.
              SlideshowView(controller: slideshow),

              // Dim scrim that deepens while a turn is active so text stays legible.
              AnimatedContainer(
                duration: const Duration(milliseconds: 400),
                color: Colors.black.withValues(alpha: active ? 0.55 : 0.15),
              ),

              // The live conversation panel fades in for the duration of a turn.
              AnimatedOpacity(
                opacity: active ? 1 : 0,
                duration: const Duration(milliseconds: 300),
                child: IgnorePointer(
                  ignoring: !active,
                  child: Padding(
                    padding: const EdgeInsets.all(28),
                    child: ConversationView(state: state),
                  ),
                ),
              ),

              // Idle clock, bottom-left.
              Positioned(
                left: 28,
                bottom: 24,
                child: AnimatedOpacity(
                  opacity: active ? 0 : 1,
                  duration: const Duration(milliseconds: 300),
                  child: const _AmbientClock(),
                ),
              ),

              // Connection-status chip, top-right.
              Positioned(
                right: 20,
                top: 18,
                child: StatusIndicator(state: state),
              ),

              // On-device timers, top-center — visible during both idle slideshow
              // and a live turn (not gated by `active`). Padded clear of the
              // settings button (top-left) and status chip (top-right).
              Positioned(
                top: 16,
                left: 64,
                right: 120,
                child: Align(
                  alignment: Alignment.topCenter,
                  child: TimersOverlay(
                    timers: state.timers,
                    onDismiss: assistant.dismissTimer,
                  ),
                ),
              ),

              // Discreet settings control, top-left. Fades out during a turn so it
              // never competes with the live conversation.
              if (onOpenSettings != null)
                Positioned(
                  left: 12,
                  top: 10,
                  child: AnimatedOpacity(
                    opacity: active ? 0 : 0.7,
                    duration: const Duration(milliseconds: 300),
                    child: IconButton(
                      key: const Key('open-settings'),
                      tooltip: 'Settings',
                      onPressed: active ? null : onOpenSettings,
                      icon: Icon(
                        Icons.settings,
                        color: Colors.white.withValues(alpha: 0.9),
                      ),
                    ),
                  ),
                ),
            ],
          );
        },
      ),
    );
  }
}

/// A minimal ticking clock for the idle screen.
class _AmbientClock extends StatefulWidget {
  const _AmbientClock();

  @override
  State<_AmbientClock> createState() => _AmbientClockState();
}

class _AmbientClockState extends State<_AmbientClock> {
  late final Stream<DateTime> _ticks =
      Stream<DateTime>.periodic(const Duration(seconds: 1), (_) => DateTime.now());

  String _fmt(DateTime t) {
    final h = t.hour % 12 == 0 ? 12 : t.hour % 12;
    final m = t.minute.toString().padLeft(2, '0');
    final ampm = t.hour < 12 ? 'AM' : 'PM';
    return '$h:$m $ampm';
  }

  @override
  Widget build(BuildContext context) {
    return StreamBuilder<DateTime>(
      stream: _ticks,
      builder: (context, snap) {
        final now = snap.data ?? DateTime.now();
        return Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              _fmt(now),
              style: TextStyle(
                color: Colors.white.withValues(alpha: 0.92),
                fontSize: 44,
                fontWeight: FontWeight.w300,
                letterSpacing: 1.0,
              ),
            ),
            Text(
              'Say the wake word to begin',
              style: TextStyle(
                color: Colors.white.withValues(alpha: 0.6),
                fontSize: 14,
              ),
            ),
          ],
        );
      },
    );
  }
}
