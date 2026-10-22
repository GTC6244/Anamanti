import 'package:flutter/material.dart';

import '../data/companion_api.dart';
import '../state/connection_controller.dart';
import 'common.dart';
import 'memories_screen.dart';
import 'now_playing_screen.dart';
import 'status_screen.dart';
import 'timers_screen.dart';

/// The app's root screen: finds the Core (or lets the user pick one), then shows
/// the four tabs.
class HomeShell extends StatefulWidget {
  const HomeShell({super.key, required this.controller, required this.api});

  final ConnectionController controller;
  final CompanionApi api;

  @override
  State<HomeShell> createState() => _HomeShellState();
}

class _HomeShellState extends State<HomeShell> {
  int _tab = 0;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: widget.controller,
      builder: (context, _) {
        final c = widget.controller;
        switch (c.phase) {
          case ConnectionPhase.searching:
            return const _Bare(
              child: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  CircularProgressIndicator(),
                  SizedBox(height: 16),
                  Text('Looking for your Anamanti Core…'),
                ],
              ),
            );
          case ConnectionPhase.notFound:
            return _Bare(
              child: MessagePanel(
                icon: Icons.wifi_find,
                title: 'No Anamanti Core found',
                message: [
                  'Make sure this phone is on your home Wi-Fi and the Core is '
                      'running.',
                  if (c.error != null) c.error!,
                ].join('\n\n'),
                action: FilledButton.icon(
                  onPressed: () => c.discover(full: true),
                  icon: const Icon(Icons.refresh),
                  label: const Text('Search again'),
                ),
              ),
            );
          case ConnectionPhase.choosing:
            return _CorePicker(controller: c);
          case ConnectionPhase.connected:
            return _tabs(context, c, c.core!);
        }
      },
    );
  }

  Widget _tabs(BuildContext context, ConnectionController c, CoreInfo core) {
    void reach(bool ok) => ok ? c.reportSuccess() : c.reportFailure();
    return Scaffold(
      appBar: AppBar(
        title: Text(core.name),
        actions: [
          _HealthIndicator(health: c.health),
          IconButton(
            tooltip: 'Switch Core',
            icon: const Icon(Icons.swap_horiz),
            onPressed: c.switchCore,
          ),
        ],
      ),
      body: IndexedStack(
        index: _tab,
        children: [
          StatusScreen(
            api: widget.api,
            core: core,
            active: _tab == 0,
            onReachability: reach,
          ),
          NowPlayingScreen(
            api: widget.api,
            core: core,
            active: _tab == 1,
            onReachability: reach,
          ),
          TimersScreen(
            api: widget.api,
            core: core,
            active: _tab == 2,
            onReachability: reach,
          ),
          MemoriesScreen(
            api: widget.api,
            core: core,
            active: _tab == 3,
            onReachability: reach,
          ),
        ],
      ),
      bottomNavigationBar: NavigationBar(
        selectedIndex: _tab,
        onDestinationSelected: (i) => setState(() => _tab = i),
        destinations: const [
          NavigationDestination(
            icon: Icon(Icons.monitor_heart_outlined),
            selectedIcon: Icon(Icons.monitor_heart),
            label: 'Status',
          ),
          NavigationDestination(
            icon: Icon(Icons.music_note_outlined),
            selectedIcon: Icon(Icons.music_note),
            label: 'Now playing',
          ),
          NavigationDestination(
            icon: Icon(Icons.timer_outlined),
            selectedIcon: Icon(Icons.timer),
            label: 'Timers',
          ),
          NavigationDestination(
            icon: Icon(Icons.psychology_outlined),
            selectedIcon: Icon(Icons.psychology),
            label: 'Memories',
          ),
        ],
      ),
    );
  }
}

class _Bare extends StatelessWidget {
  const _Bare({required this.child});
  final Widget child;

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(title: const Text('Anamanti Companion')),
    body: Center(child: child),
  );
}

class _HealthIndicator extends StatelessWidget {
  const _HealthIndicator({required this.health});
  final ConnectionHealth health;

  @override
  Widget build(BuildContext context) {
    final (color, label) = switch (health) {
      ConnectionHealth.ok => (okColor, 'Connected'),
      ConnectionHealth.reconnecting => (warnColor, 'Reconnecting…'),
      ConnectionHealth.offline => (errorColor, "Can't reach the Core"),
    };
    return Tooltip(
      message: label,
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 8),
        child: Semantics(
          label: label,
          child: StatusDot(
            key: Key('health-${health.name}'),
            color: color,
            size: 12,
          ),
        ),
      ),
    );
  }
}

class _CorePicker extends StatelessWidget {
  const _CorePicker({required this.controller});
  final ConnectionController controller;

  @override
  Widget build(BuildContext context) {
    final missing = controller.missingSavedKey;
    return Scaffold(
      appBar: AppBar(
        title: const Text('Choose your Core'),
        actions: [
          IconButton(
            tooltip: 'Search again',
            icon: const Icon(Icons.refresh),
            onPressed: () => controller.discover(full: true),
          ),
        ],
      ),
      body: ListView(
        children: [
          if (missing != null)
            ListTile(
              leading: const Icon(Icons.info_outline),
              title: Text('Your saved Core "$missing" didn\'t answer.'),
              subtitle: const Text(
                'Pick one of the Cores found on this network.',
              ),
            ),
          for (final core in controller.cores)
            ListTile(
              leading: const Icon(Icons.dns_outlined),
              title: Text(core.name),
              subtitle: Text(
                [
                  core.host.isEmpty ? core.address : core.host,
                  if (core.companionPort == null) 'companion API off',
                ].join(' · '),
              ),
              trailing: controller.core?.key == core.key
                  ? const Icon(Icons.check)
                  : const Icon(Icons.chevron_right),
              onTap: () => controller.select(core),
            ),
        ],
      ),
    );
  }
}
