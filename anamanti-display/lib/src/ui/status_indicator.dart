// A subtle connection-status chip (Plan.MD §3, Phase 5 resilience: "a subtle
// disconnected status indicator"). Unobtrusive when healthy; clearly flags when
// the Mac orchestrator is unreachable so the ambient screen never feels broken.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/assistant_controller.dart';

class StatusIndicator extends StatelessWidget {
  const StatusIndicator({super.key, required this.state});

  final AssistantState state;

  @override
  Widget build(BuildContext context) {
    final (color, label, icon) = _describe(state);
    return DecoratedBox(
      decoration: BoxDecoration(
        color: Colors.black.withValues(alpha: 0.35),
        borderRadius: BorderRadius.circular(999),
      ),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 7),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(icon, size: 14, color: color),
            const SizedBox(width: 6),
            Text(
              label,
              style: TextStyle(
                color: Colors.white.withValues(alpha: 0.85),
                fontSize: 12,
                fontWeight: FontWeight.w500,
                letterSpacing: 0.2,
              ),
            ),
          ],
        ),
      ),
    );
  }

  (Color, String, IconData) _describe(AssistantState s) {
    if (s.phase == TurnPhase.error) {
      return (const Color(0xFFFFB4A2), 'Reconnecting', Icons.sync_problem);
    }
    if (!s.online && s.captureReady && s.phase == TurnPhase.idle) {
      return (const Color(0xFFFFD59E), 'Offline', Icons.cloud_off);
    }
    switch (s.phase) {
      case TurnPhase.listening:
        return (const Color(0xFF9BE7FF), 'Listening', Icons.mic);
      case TurnPhase.processing:
        return (const Color(0xFFB8C7FF), 'Processing', Icons.hourglass_top);
      case TurnPhase.connecting:
        return (const Color(0xFF9BE7FF), 'Connecting', Icons.wifi_tethering);
      case TurnPhase.thinking:
        return (const Color(0xFFB8C7FF), 'Thinking', Icons.auto_awesome);
      case TurnPhase.speaking:
        return (const Color(0xFFA8F0C6), 'Speaking', Icons.volume_up);
      case TurnPhase.idle:
      case TurnPhase.error:
        return (const Color(0xFFA8F0C6), 'Ready', Icons.check_circle);
    }
  }
}
