// Timers tab: every timer on every display, from `GET /timers.json`, grouped by
// display. Between polls each timer counts down locally from `remaining_secs`,
// measured against the phone's monotonic clock from the moment the response
// arrived — the phone's and the Mac's wall clocks are never compared
// (`ends_at_ms` / `now_ms` are informational only).

import 'dart:async';

import 'package:flutter/material.dart';

import '../data/companion_api.dart';
import '../util/format.dart';
import 'common.dart';

class TimersScreen extends StatefulWidget {
  const TimersScreen({
    super.key,
    required this.api,
    required this.core,
    required this.active,
    this.onReachability,
    this.pollInterval = const Duration(seconds: 5),
    this.clock = monotonicNow,
  });

  final CompanionApi api;
  final CoreInfo core;
  final bool active;
  final void Function(bool ok)? onReachability;
  final Duration pollInterval;
  final MonotonicClock clock;

  @override
  State<TimersScreen> createState() => _TimersScreenState();
}

class _TimersScreenState extends State<TimersScreen> {
  late final Poller _poller = Poller(
    interval: widget.pollInterval,
    onTick: _load,
  );
  TimersSnapshot? _snapshot;
  Duration _fetchedAt = Duration.zero;
  String? _error;
  bool _loadedOnce = false;
  Timer? _ticker;

  bool get _hasApi => widget.core.companionPort != null;

  @override
  void initState() {
    super.initState();
    _poller.attach();
    _activate();
  }

  @override
  void didUpdateWidget(TimersScreen old) {
    super.didUpdateWidget(old);
    if (old.core != widget.core) {
      _snapshot = null;
      _error = null;
      _loadedOnce = false;
    }
    if (old.active != widget.active || old.core != widget.core) _activate();
  }

  void _activate() {
    _poller.active = widget.active && _hasApi;
    if (widget.active && _hasApi && !_loadedOnce) {
      _loadedOnce = true;
      _load();
    }
    _syncTicker();
  }

  void _syncTicker() {
    final any = _snapshot?.displays.any((d) => d.timers.isNotEmpty) ?? false;
    final want = widget.active && any;
    if (want && _ticker == null) {
      _ticker = Timer.periodic(const Duration(seconds: 1), (_) {
        if (mounted) setState(() {});
      });
    } else if (!want) {
      _ticker?.cancel();
      _ticker = null;
    }
  }

  @override
  void dispose() {
    _ticker?.cancel();
    _poller.dispose();
    super.dispose();
  }

  Future<void> _load() async {
    final core = widget.core;
    try {
      final snapshot = await widget.api.fetchTimers(core);
      if (!mounted || core != widget.core) return;
      setState(() {
        _snapshot = snapshot;
        _fetchedAt = widget.clock();
        _error = null;
      });
      _syncTicker();
      widget.onReachability?.call(true);
    } catch (e) {
      if (!mounted || core != widget.core) return;
      setState(() => _error = _friendly(describeError(e)));
      widget.onReachability?.call(false);
    }
  }

  static String _friendly(String message) => message.contains('HTTP 404')
      ? "This Core doesn't report timers yet. Update the Core to see them."
      : message;

  @override
  Widget build(BuildContext context) {
    if (!_hasApi) return const NoCompanionApiPanel();
    final snapshot = _snapshot;
    if (snapshot == null) {
      if (_error != null) {
        return MessagePanel(
          icon: Icons.cloud_off,
          title: "Couldn't load timers",
          message: _error,
          action: FilledButton(onPressed: _load, child: const Text('Retry')),
        );
      }
      return const Center(child: CircularProgressIndicator());
    }
    final groups = snapshot.displays.where((d) => d.timers.isNotEmpty).toList()
      ..sort((a, b) => _displayName(a).compareTo(_displayName(b)));
    final elapsed = (widget.clock() - _fetchedAt).inSeconds;
    return RefreshIndicator(
      onRefresh: _load,
      child: groups.isEmpty
          ? const MessagePanel(
              icon: Icons.timer_off_outlined,
              title: 'No timers running',
              message: 'Timers set on any display show up here.',
            )
          : ListView(
              padding: const EdgeInsets.fromLTRB(12, 8, 12, 24),
              children: [
                for (final d in groups) ...[
                  Padding(
                    padding: const EdgeInsets.fromLTRB(4, 8, 4, 6),
                    child: Row(
                      children: [
                        const Icon(Icons.tv, size: 18),
                        const SizedBox(width: 8),
                        Text(
                          _displayName(d),
                          style: Theme.of(context).textTheme.titleSmall,
                        ),
                      ],
                    ),
                  ),
                  for (final t in d.timers)
                    _TimerCard(
                      key: ValueKey('timer-${d.deviceId}-${t.id}'),
                      timer: t,
                      remaining: t.ringing
                          ? 0
                          : (t.remainingSecs - elapsed).clamp(0, 1 << 31),
                    ),
                ],
              ],
            ),
    );
  }
}

String _displayName(DisplayTimers d) => d.name.isEmpty ? d.deviceId : d.name;

class _TimerCard extends StatelessWidget {
  const _TimerCard({super.key, required this.timer, required this.remaining});

  final CoreTimer timer;
  final int remaining;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final text = Theme.of(context).textTheme;
    final ringing = timer.ringing;
    final done = !ringing && remaining == 0;
    final label = timer.label.isEmpty
        ? '${formatDuration(timer.durationSecs)} timer'
        : timer.label;
    final fraction = timer.durationSecs > 0
        ? (remaining / timer.durationSecs).clamp(0.0, 1.0)
        : 0.0;
    return Card(
      color: ringing ? scheme.errorContainer : null,
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Icon(
                  ringing ? Icons.alarm : Icons.timer_outlined,
                  color: ringing ? scheme.onErrorContainer : scheme.primary,
                ),
                const SizedBox(width: 8),
                Expanded(
                  child: Text(
                    label,
                    style: text.titleMedium?.copyWith(
                      color: ringing ? scheme.onErrorContainer : null,
                    ),
                  ),
                ),
                if (ringing)
                  Text(
                    'Ringing',
                    style: text.labelLarge?.copyWith(
                      color: scheme.onErrorContainer,
                      fontWeight: FontWeight.w700,
                    ),
                  )
                else
                  Text(
                    done ? "Time's up" : formatClock(remaining),
                    style: text.headlineSmall?.copyWith(
                      fontFeatures: const [FontFeature.tabularFigures()],
                    ),
                  ),
              ],
            ),
            const SizedBox(height: 10),
            LinearProgressIndicator(value: ringing ? 0 : fraction),
          ],
        ),
      ),
    );
  }
}
