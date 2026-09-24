// Visual banner for a proactive notification pushed by the orchestrator
// (Approach A, visual-only phase). Rendered as a top-center card over the idle
// slideshow; tap to dismiss. Styling keys off the notification's priority.

import 'package:flutter/material.dart';

import 'package:ambient_display/src/rust/api/engine.dart';

class NotificationBanner extends StatelessWidget {
  const NotificationBanner({
    super.key,
    required this.notification,
    required this.onDismiss,
  });

  final NotifyEvent notification;
  final VoidCallback onDismiss;

  Color get _accent {
    switch (notification.priority) {
      case 'alert':
        return const Color(0xFFFF5252);
      case 'reminder':
        return const Color(0xFFFFB300);
      case 'info':
      default:
        return const Color(0xFF4FC3F7);
    }
  }

  @override
  Widget build(BuildContext context) {
    final accent = _accent;
    return SafeArea(
      child: Align(
        alignment: Alignment.topCenter,
        child: Padding(
          padding: const EdgeInsets.only(top: 16),
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 560),
            child: Material(
              color: Colors.transparent,
              child: InkWell(
                borderRadius: BorderRadius.circular(16),
                onTap: onDismiss,
                child: Container(
                  padding: const EdgeInsets.fromLTRB(18, 14, 14, 14),
                  decoration: BoxDecoration(
                    color: Colors.black.withValues(alpha: 0.82),
                    borderRadius: BorderRadius.circular(16),
                    border: Border(left: BorderSide(color: accent, width: 4)),
                    boxShadow: [
                      BoxShadow(
                        color: Colors.black.withValues(alpha: 0.4),
                        blurRadius: 18,
                        offset: const Offset(0, 6),
                      ),
                    ],
                  ),
                  child: Row(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Icon(_icon, color: accent, size: 22),
                      const SizedBox(width: 12),
                      Expanded(
                        child: Column(
                          crossAxisAlignment: CrossAxisAlignment.start,
                          mainAxisSize: MainAxisSize.min,
                          children: [
                            if (notification.title.isNotEmpty)
                              Text(
                                notification.title,
                                style: const TextStyle(
                                  color: Colors.white,
                                  fontSize: 16,
                                  fontWeight: FontWeight.w600,
                                ),
                              ),
                            if (notification.title.isNotEmpty &&
                                notification.body.isNotEmpty)
                              const SizedBox(height: 4),
                            if (notification.body.isNotEmpty)
                              Text(
                                notification.body,
                                style: TextStyle(
                                  color: Colors.white.withValues(alpha: 0.85),
                                  fontSize: 14,
                                  height: 1.3,
                                ),
                              ),
                          ],
                        ),
                      ),
                      const SizedBox(width: 8),
                      Icon(
                        Icons.close,
                        color: Colors.white.withValues(alpha: 0.5),
                        size: 20,
                      ),
                    ],
                  ),
                ),
              ),
            ),
          ),
        ),
      ),
    );
  }

  IconData get _icon {
    switch (notification.priority) {
      case 'alert':
        return Icons.warning_amber_rounded;
      case 'reminder':
        return Icons.schedule;
      case 'info':
      default:
        return Icons.notifications_none;
    }
  }
}
