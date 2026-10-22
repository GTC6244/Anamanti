import 'dart:async';

import 'package:flutter/material.dart';

/// A monotonic "time since app start". Countdowns and progress bars measure
/// elapsed time with this (never the phone's wall clock against the Mac's), and
/// tests inject a manual one.
typedef MonotonicClock = Duration Function();

final Stopwatch _appClock = Stopwatch()..start();

Duration monotonicNow() => _appClock.elapsed;

/// Periodic refresh for a tab, running only while the tab is the visible one and
/// the app is in the foreground.
///
/// Deviation from the repo's "stream-driven, no polling" convention: the Core's
/// companion API is plain request/response HTTP (GET-only, by design), so light
/// polling while a tab is on screen is the agreed way to keep it fresh.
class Poller {
  Poller({required this.interval, required this.onTick});

  final Duration interval;
  final VoidCallback onTick;

  Timer? _timer;
  AppLifecycleListener? _lifecycle;
  bool _wanted = false;
  bool _foreground = true;

  /// Start listening to app lifecycle (call from `initState`).
  void attach() {
    final state = WidgetsBinding.instance.lifecycleState;
    _foreground = state == null || state == AppLifecycleState.resumed;
    _lifecycle = AppLifecycleListener(
      onStateChange: (s) {
        final was = _foreground;
        _foreground = s == AppLifecycleState.resumed;
        if (_foreground && !was && _wanted) onTick(); // catch up on resume
        _sync();
      },
    );
  }

  /// Whether the owning tab is visible.
  set active(bool value) {
    _wanted = value;
    _sync();
  }

  void _sync() {
    final run = _wanted && _foreground;
    if (run && _timer == null) {
      _timer = Timer.periodic(interval, (_) => onTick());
    } else if (!run) {
      _timer?.cancel();
      _timer = null;
    }
  }

  void dispose() {
    _timer?.cancel();
    _timer = null;
    _lifecycle?.dispose();
  }
}

/// A centred icon + title + message (+ optional action) for empty/error states.
/// Scrollable so it works inside a `RefreshIndicator`.
class MessagePanel extends StatelessWidget {
  const MessagePanel({
    super.key,
    required this.icon,
    required this.title,
    this.message,
    this.action,
  });

  final IconData icon;
  final String title;
  final String? message;
  final Widget? action;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return LayoutBuilder(
      builder: (context, constraints) => SingleChildScrollView(
        physics: const AlwaysScrollableScrollPhysics(),
        child: ConstrainedBox(
          constraints: BoxConstraints(minHeight: constraints.maxHeight),
          child: Center(
            child: Padding(
              padding: const EdgeInsets.all(32),
              child: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  Icon(icon, size: 56, color: theme.colorScheme.outline),
                  const SizedBox(height: 16),
                  Text(
                    title,
                    style: theme.textTheme.titleMedium,
                    textAlign: TextAlign.center,
                  ),
                  if (message != null) ...[
                    const SizedBox(height: 8),
                    Text(
                      message!,
                      style: theme.textTheme.bodyMedium?.copyWith(
                        color: theme.colorScheme.onSurfaceVariant,
                      ),
                      textAlign: TextAlign.center,
                    ),
                  ],
                  if (action != null) ...[const SizedBox(height: 16), action!],
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}

/// Shown on the HTTP-backed tabs when the Core's mDNS record has no
/// `companion_port` (listener disabled, or a Core from before the companion API).
class NoCompanionApiPanel extends StatelessWidget {
  const NoCompanionApiPanel({super.key});

  @override
  Widget build(BuildContext context) => const MessagePanel(
    icon: Icons.api_outlined,
    title: "This Core doesn't have the companion API turned on",
    message:
        'Enable the companion listener in the Core\'s anamanti.json (or update '
        'the Core). Memories still work.',
  );
}

/// The colour pair for a Local / LAN / Cloud / Off tier chip.
({Color bg, Color fg}) tierColors(String tier, ColorScheme scheme) {
  switch (tier) {
    case 'local':
      return (bg: const Color(0xFFDDF3E4), fg: const Color(0xFF1B5E20));
    case 'lan':
      return (bg: const Color(0xFFDCEBFB), fg: const Color(0xFF0D47A1));
    case 'cloud':
      return (bg: const Color(0xFFF3E5F5), fg: const Color(0xFF6A1B9A));
    default:
      return (bg: scheme.surfaceContainerHighest, fg: scheme.onSurfaceVariant);
  }
}

String tierLabel(String tier, {bool keyless = false}) {
  switch (tier) {
    case 'local':
      return 'Local';
    case 'lan':
      return 'LAN';
    case 'cloud':
      return keyless ? 'Cloud · keyless' : 'Cloud';
    default:
      return 'Off';
  }
}

/// A small rounded label.
class Pill extends StatelessWidget {
  const Pill({
    super.key,
    required this.text,
    required this.background,
    required this.foreground,
    this.leading,
  });

  final String text;
  final Color background;
  final Color foreground;
  final Widget? leading;

  @override
  Widget build(BuildContext context) => Container(
    padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
    decoration: BoxDecoration(
      color: background,
      borderRadius: BorderRadius.circular(999),
    ),
    child: Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        if (leading != null) ...[leading!, const SizedBox(width: 4)],
        Text(
          text,
          style: Theme.of(context).textTheme.labelSmall?.copyWith(
            color: foreground,
            fontWeight: FontWeight.w600,
          ),
        ),
      ],
    ),
  );
}

/// A coloured status dot.
class StatusDot extends StatelessWidget {
  const StatusDot({super.key, required this.color, this.size = 8});
  final Color color;
  final double size;

  @override
  Widget build(BuildContext context) => Container(
    width: size,
    height: size,
    decoration: BoxDecoration(color: color, shape: BoxShape.circle),
  );
}

const okColor = Color(0xFF2E7D32);
const warnColor = Color(0xFFEF6C00);
const errorColor = Color(0xFFC62828);
const offColor = Color(0xFF9E9E9E);
