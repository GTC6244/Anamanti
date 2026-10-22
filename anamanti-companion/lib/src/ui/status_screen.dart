// Status tab: a phone rendition of the Core admin console's Overview page
// (anamanti-core/src/webconfig/overview.html) — overall health, the live voice
// pipeline (engine, Local/LAN/Cloud tier, health per stage), connected displays,
// recent latency and integrations.

import 'package:flutter/material.dart';

import '../data/companion_api.dart';
import '../data/overview.dart';
import '../util/format.dart';
import 'common.dart';

class StatusScreen extends StatefulWidget {
  const StatusScreen({
    super.key,
    required this.api,
    required this.core,
    required this.active,
    this.onReachability,
    this.pollInterval = const Duration(seconds: 10),
  });

  final CompanionApi api;
  final CoreInfo core;

  /// Whether this tab is the visible one (polling runs only then).
  final bool active;
  final void Function(bool ok)? onReachability;
  final Duration pollInterval;

  @override
  State<StatusScreen> createState() => _StatusScreenState();
}

class _StatusScreenState extends State<StatusScreen> {
  late final Poller _poller = Poller(
    interval: widget.pollInterval,
    onTick: () => _load(probe: false),
  );
  Overview? _overview;
  String? _error;
  bool _loadedOnce = false;

  bool get _hasApi => widget.core.companionPort != null;

  @override
  void initState() {
    super.initState();
    _poller.attach();
    _activate();
  }

  @override
  void didUpdateWidget(StatusScreen old) {
    super.didUpdateWidget(old);
    if (old.core != widget.core) {
      _overview = null;
      _error = null;
      _loadedOnce = false;
    }
    if (old.active != widget.active || old.core != widget.core) _activate();
  }

  void _activate() {
    _poller.active = widget.active && _hasApi;
    if (widget.active && _hasApi && !_loadedOnce) {
      _loadedOnce = true;
      // Paint fast with probes off, then probe the services for real health.
      _load(probe: false).then((_) => _load(probe: true));
    }
  }

  @override
  void dispose() {
    _poller.dispose();
    super.dispose();
  }

  Future<void> _load({required bool probe}) async {
    final core = widget.core;
    try {
      final json = await widget.api.fetchOverview(core, probe: probe);
      final overview = Overview.parse(json);
      if (!mounted || core != widget.core) return;
      setState(() {
        _overview = overview;
        _error = null;
      });
      widget.onReachability?.call(true);
    } catch (e) {
      if (!mounted || core != widget.core) return;
      setState(() => _error = describeError(e));
      widget.onReachability?.call(false);
    }
  }

  @override
  Widget build(BuildContext context) {
    if (!_hasApi) return const NoCompanionApiPanel();
    final o = _overview;
    if (o == null) {
      if (_error != null) {
        return MessagePanel(
          icon: Icons.cloud_off,
          title: "Couldn't load the Core's status",
          message: _error,
          action: FilledButton(
            onPressed: () => _load(probe: true),
            child: const Text('Retry'),
          ),
        );
      }
      return const Center(child: CircularProgressIndicator());
    }
    return RefreshIndicator(
      onRefresh: () => _load(probe: true),
      child: ListView(
        padding: const EdgeInsets.fromLTRB(12, 8, 12, 24),
        children: [
          if (_error != null) _StaleBanner(message: _error!),
          _HealthCard(overview: o),
          const SizedBox(height: 12),
          _SectionTitle('Voice pipeline'),
          Card(
            child: Column(
              children: [
                for (final s in o.stages) _StageTile(stage: s),
                if (o.stages.isEmpty)
                  const ListTile(title: Text('No pipeline stages reported')),
              ],
            ),
          ),
          const SizedBox(height: 12),
          _SectionTitle('Displays'),
          Card(
            child: o.displays.isEmpty
                ? const ListTile(
                    leading: Icon(Icons.tv_off_outlined),
                    title: Text('No displays connected'),
                  )
                : Column(
                    children: [
                      for (final d in o.displays)
                        ListTile(
                          leading: const StatusDot(color: okColor, size: 10),
                          minLeadingWidth: 10,
                          title: Text(d.name.isEmpty ? '(unnamed)' : d.name),
                          subtitle: Text(d.deviceId),
                        ),
                    ],
                  ),
          ),
          const SizedBox(height: 12),
          _SectionTitle('Recent latency'),
          Card(
            child: ListTile(
              leading: const Icon(Icons.speed),
              title: o.latencyCount == 0
                  ? const Text('No turns yet')
                  : Text('p50 ${formatMs(o.p50Ms)} · p95 ${formatMs(o.p95Ms)}'),
              subtitle: Text(
                'End of speech to reply ready, last ${o.latencyCount} turns',
              ),
            ),
          ),
          if (o.integrations.isNotEmpty) ...[
            const SizedBox(height: 12),
            _SectionTitle('Integrations'),
            Card(
              child: Column(
                children: [
                  for (final i in o.integrations)
                    ListTile(
                      dense: true,
                      leading: StatusDot(color: _integrationColor(i.status)),
                      minLeadingWidth: 10,
                      title: Text(i.name),
                      subtitle: Text(i.detail.isEmpty ? i.status : i.detail),
                    ),
                ],
              ),
            ),
          ],
        ],
      ),
    );
  }
}

Color _integrationColor(String status) => switch (status) {
  'active' => okColor,
  'attention' => warnColor,
  _ => offColor,
};

Color stageStatusColor(String status) => switch (status) {
  'ok' => okColor,
  'error' => errorColor,
  'fallback' || 'needs_key' => warnColor,
  _ => offColor,
};

String stageStatusText(String status) => switch (status) {
  'ok' => 'healthy',
  'error' => 'unreachable',
  'fallback' => 'fallback',
  'needs_key' => 'needs key',
  'off' => 'off',
  'unknown' => 'not probed',
  _ => status,
};

IconData _stageIcon(String key) => switch (key) {
  'vad' => Icons.graphic_eq,
  'stt' => Icons.mic_none,
  'system1' => Icons.bolt,
  'llm' => Icons.psychology_outlined,
  'search' => Icons.travel_explore,
  'embed' => Icons.storage,
  'extract' => Icons.hub_outlined,
  'tts' => Icons.volume_up_outlined,
  _ => Icons.layers_outlined,
};

class _SectionTitle extends StatelessWidget {
  const _SectionTitle(this.text);
  final String text;

  @override
  Widget build(BuildContext context) => Padding(
    padding: const EdgeInsets.fromLTRB(4, 4, 4, 6),
    child: Text(text, style: Theme.of(context).textTheme.titleSmall),
  );
}

class _StaleBanner extends StatelessWidget {
  const _StaleBanner({required this.message});
  final String message;

  @override
  Widget build(BuildContext context) => Card(
    color: Theme.of(context).colorScheme.errorContainer,
    child: ListTile(
      leading: const Icon(Icons.sync_problem),
      title: const Text('Showing the last status we got'),
      subtitle: Text(message),
    ),
  );
}

class _HealthCard extends StatelessWidget {
  const _HealthCard({required this.overview});
  final Overview overview;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final bad = overview.unreachable.length;
    final warn = overview.needsAttention.length;
    final (color, text) = bad > 0
        ? (errorColor, '$bad stage${bad > 1 ? 's' : ''} unreachable')
        : warn > 0
        ? (warnColor, '$warn need${warn > 1 ? '' : 's'} attention')
        : (okColor, 'All stages healthy');
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                StatusDot(color: color, size: 12),
                const SizedBox(width: 8),
                Expanded(child: Text(text, style: theme.textTheme.titleMedium)),
              ],
            ),
            const SizedBox(height: 8),
            Wrap(
              spacing: 12,
              runSpacing: 4,
              children: [
                if (overview.serviceName.isNotEmpty)
                  Text(overview.serviceName, style: theme.textTheme.bodySmall),
                if (overview.version.isNotEmpty)
                  Text(
                    'v${overview.version}',
                    style: theme.textTheme.bodySmall,
                  ),
                if (overview.uptimeSecs != null)
                  Text(
                    'up ${formatDuration(overview.uptimeSecs!)}',
                    style: theme.textTheme.bodySmall,
                  ),
              ],
            ),
          ],
        ),
      ),
    );
  }
}

class _StageTile extends StatelessWidget {
  const _StageTile({required this.stage});
  final PipelineStage stage;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final tier = tierColors(stage.tier, theme.colorScheme);
    return ListTile(
      key: ValueKey('stage-${stage.key}'),
      leading: Icon(_stageIcon(stage.key)),
      title: Text(stage.label),
      subtitle: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(stage.engine),
          if (stage.detail.isNotEmpty)
            Text(
              stage.detail,
              style: theme.textTheme.bodySmall,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
            ),
          const SizedBox(height: 4),
          Wrap(
            spacing: 6,
            runSpacing: 4,
            children: [
              Pill(
                text: tierLabel(stage.tier, keyless: stage.keyless),
                background: tier.bg,
                foreground: tier.fg,
              ),
              if (stage.status != 'off')
                Pill(
                  text: stageStatusText(stage.status),
                  background: theme.colorScheme.surfaceContainerHighest,
                  foreground: theme.colorScheme.onSurface,
                  leading: StatusDot(color: stageStatusColor(stage.status)),
                ),
            ],
          ),
        ],
      ),
      isThreeLine: true,
    );
  }
}
