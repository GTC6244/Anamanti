// Settings → Device: which box this is and whether it's healthy. Connection (name +
// which Core it talks to), Updates (in-app updater, selfUpdate flavor only), and
// Maintenance (Android permissions, Advanced mode, the kiosk escape hatch).

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'package:anamanti_display/src/engine/permissions.dart';
import 'package:anamanti_display/src/engine/update_controller.dart';
import 'package:anamanti_display/src/ui/settings/settings_controller.dart';
import 'package:anamanti_display/src/ui/settings/settings_widgets.dart';

/// Device → Connection.
class DeviceConnectionTab extends StatelessWidget {
  const DeviceConnectionTab({
    super.key,
    required this.controller,
    this.updates,
  });

  final SettingsController controller;
  final UpdateController? updates;

  @override
  Widget build(BuildContext context) {
    final c = controller;
    final s = c.settings;
    final current = s.orchestratorKey.trim();
    final keys = c.orchestrators.map((o) => o.key).toSet();
    final String status;
    final IconData statusIcon;
    final Color statusColor;
    if (c.remoteLoading) {
      status = 'Checking…';
      statusIcon = Icons.sync;
      statusColor = Colors.grey;
    } else if (c.remoteError != null) {
      status = 'Can’t reach the assistant';
      statusIcon = Icons.cloud_off;
      statusColor = Colors.orange;
    } else {
      status = 'Connected';
      statusIcon = Icons.check_circle;
      statusColor = Colors.green;
    }
    return SettingsTabBody(
      left: [
        const SettingsHeader('This display'),
        Padding(
          padding: const EdgeInsets.fromLTRB(12, 4, 12, 4),
          child: CommitTextField(
            key: const Key('settings-device-name'),
            text: s.deviceName,
            label: 'Device name',
            hint: 'e.g. Kitchen',
            helper: s.deviceId.isEmpty
                ? 'Shown on the Core’s “Connected devices” list'
                : 'ID: ${s.deviceId}',
            textCapitalization: TextCapitalization.words,
            onChanged: (v) => c.updateLocal(
              c.settings.copyWith(deviceName: v),
              commit: false,
            ),
            onCommit: c.commitLocalSoon,
          ),
        ),
        const SettingsHeader(
          'Anamanti Core',
          caption:
              'Which Mac this display connects to. A pinned Core is strict: '
              'the display stays offline rather than switching Macs.',
        ),
        Padding(
          padding: const EdgeInsets.fromLTRB(12, 0, 12, 4),
          child: DropdownButton<String>(
            key: const Key('settings-orchestrator'),
            isExpanded: true,
            value: current,
            items: [
              const DropdownMenuItem(
                value: '',
                child: Text('Auto (first available)'),
              ),
              for (final o in c.orchestrators)
                DropdownMenuItem(
                  value: o.key,
                  child: Text(
                    o.host.isEmpty ? o.name : '${o.name} · ${o.host}',
                  ),
                ),
              if (current.isNotEmpty && !keys.contains(current))
                DropdownMenuItem(
                  value: current,
                  child: Text('$current (not found)'),
                ),
            ],
            onChanged: (v) =>
                c.updateLocal(s.copyWith(orchestratorKey: v ?? '')),
          ),
        ),
      ],
      right: [
        const SettingsHeader('Status'),
        ListTile(
          key: const Key('settings-connection-status'),
          dense: true,
          leading: Icon(statusIcon, color: statusColor),
          title: Text(status),
          subtitle: Text(
            current.isEmpty ? 'Auto: first Core found' : 'Pinned to $current',
          ),
          trailing: c.remoteError != null
              ? TextButton(onPressed: c.loadRemote, child: const Text('Retry'))
              : null,
        ),
        if (updates != null)
          ListTile(
            dense: true,
            leading: const Icon(Icons.info_outline),
            title: Text('App version code ${updates!.currentVersionCode}'),
          ),
      ],
    );
  }
}

/// Device → Updates (selfUpdate flavor only): the auto-check toggle, the live
/// check / download / install area, and (Advanced) the update base URL.
class DeviceUpdatesTab extends StatelessWidget {
  const DeviceUpdatesTab({
    super.key,
    required this.controller,
    required this.updates,
  });

  final SettingsController controller;
  final UpdateController updates;

  @override
  Widget build(BuildContext context) {
    final c = controller;
    final s = c.settings;
    return SettingsTabBody(
      left: [
        const SettingsHeader('Automatic updates'),
        SettingsSwitch(
          key: const Key('settings-auto-update'),
          icon: Icons.autorenew,
          title: 'Check for updates automatically',
          info:
              'Checks for a new version on launch and every few hours, and shows '
              'a banner when one is ready.',
          value: s.autoUpdateEnabled,
          onChanged: (v) => c.updateLocal(s.copyWith(autoUpdateEnabled: v)),
        ),
        if (s.advancedMode)
          Padding(
            padding: const EdgeInsets.fromLTRB(12, 8, 12, 4),
            child: CommitTextField(
              key: const Key('settings-update-url'),
              text: s.updateBaseUrl,
              label: 'Update URL',
              helper:
                  'Base URL hosting latest.json + the APK (no trailing slash)',
              keyboardType: TextInputType.url,
              border: true,
              onChanged: (v) {
                final url = v.trim();
                // Keep the previous URL rather than none.
                if (url.isEmpty) return;
                c.updateLocal(
                  c.settings.copyWith(updateBaseUrl: url),
                  commit: false,
                );
              },
              onCommit: c.commitLocalSoon,
            ),
          ),
      ],
      right: [
        const SettingsHeader('This app'),
        AnimatedBuilder(
          animation: updates,
          builder: (context, _) => _UpdateStatus(updates: updates),
        ),
      ],
    );
  }
}

class _UpdateStatus extends StatelessWidget {
  const _UpdateStatus({required this.updates});

  final UpdateController updates;

  @override
  Widget build(BuildContext context) {
    final manifest = updates.manifest;
    final busy =
        updates.status == UpdateStatus.checking ||
        updates.status == UpdateStatus.downloading ||
        updates.status == UpdateStatus.installing;
    final String statusLine = switch (updates.status) {
      UpdateStatus.idle => 'Not checked yet.',
      UpdateStatus.checking => 'Checking…',
      UpdateStatus.upToDate => 'You’re up to date.',
      UpdateStatus.available =>
        manifest == null
            ? 'An update is available.'
            : 'Version ${manifest.versionName.isNotEmpty ? manifest.versionName : manifest.versionCode} is available.',
      UpdateStatus.downloading =>
        updates.progress == null
            ? 'Downloading…'
            : 'Downloading… ${(updates.progress! * 100).round()}%',
      UpdateStatus.readyToInstall => 'Downloaded — ready to install.',
      UpdateStatus.installing => 'Installing…',
      UpdateStatus.error => updates.errorMessage,
    };
    final (
      String? actionLabel,
      VoidCallback? action,
    ) = switch (updates.status) {
      UpdateStatus.available => ('Download & install', updates.download),
      UpdateStatus.readyToInstall => ('Install', updates.install),
      UpdateStatus.error => ('Retry', updates.retry),
      _ => (null, null),
    };
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        ListTile(
          dense: true,
          leading: const Icon(Icons.info_outline),
          title: Text('Installed version code: ${updates.currentVersionCode}'),
          subtitle: Text(statusLine),
        ),
        if (updates.status == UpdateStatus.available &&
            manifest != null &&
            manifest.notes.isNotEmpty)
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
            child: Text(
              manifest.notes,
              maxLines: 4,
              overflow: TextOverflow.ellipsis,
            ),
          ),
        if (updates.status == UpdateStatus.downloading)
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
            child: LinearProgressIndicator(value: updates.progress),
          ),
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
          child: Wrap(
            spacing: 12,
            runSpacing: 8,
            children: [
              OutlinedButton.icon(
                key: const Key('settings-check-update'),
                onPressed: busy ? null : updates.checkNow,
                icon: const Icon(Icons.refresh),
                label: const Text('Check now'),
              ),
              if (actionLabel != null)
                FilledButton(
                  key: const Key('settings-update-action'),
                  onPressed: busy ? null : action,
                  child: Text(actionLabel),
                ),
            ],
          ),
        ),
      ],
    );
  }
}

/// Device → Maintenance: Android permissions (with Grant for the runtime ones), the
/// Advanced mode switch, and the kiosk escape hatch. The app is the device's
/// Home/launcher (Android relaunches it after a crash), so the escape-hatch tiles
/// are the way back to the stock launcher / system settings. (adb fallback:
/// `adb shell cmd package set-home-activity <stock-launcher>/.Activity`.)
class DeviceMaintenanceTab extends StatefulWidget {
  const DeviceMaintenanceTab({
    super.key,
    required this.controller,
    this.onRequestEngineRestart,
  });

  final SettingsController controller;

  /// Restart the engine after a newly granted Camera permission so presence
  /// detection + auto-brightness start without a relaunch.
  final VoidCallback? onRequestEngineRestart;

  @override
  State<DeviceMaintenanceTab> createState() => _DeviceMaintenanceTabState();
}

class _DeviceMaintenanceTabState extends State<DeviceMaintenanceTab>
    with WidgetsBindingObserver {
  /// Native channel for the kiosk escape hatch, handled by MainActivity; a no-op on
  /// host/test platforms.
  static const MethodChannel _maintenanceChannel = MethodChannel(
    'anamanti_display/maintenance',
  );

  final PermissionsController _permissions = PermissionsController();

  /// Grant state keyed by full permission name; null until first loaded.
  Map<String, bool>? _status;

  /// True while a status read or grant request is in flight.
  bool _busy = false;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    _load();
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    // Returning from the system "App info" page can change grants — re-read them.
    if (state == AppLifecycleState.resumed) _load();
  }

  Future<void> _load() async {
    setState(() => _busy = true);
    final status = await _permissions.status();
    if (!mounted) return;
    setState(() {
      _status = status;
      _busy = false;
    });
  }

  Future<void> _request(AppPermission p) async {
    final wasGranted = _status?[p.name] == true;
    setState(() => _busy = true);
    final status = await _permissions.request([p.name]);
    if (!mounted) return;
    final nowGranted = status[p.name] == true;
    setState(() {
      // An empty map means the channel was absent (host/test) — keep prior state.
      if (status.isNotEmpty) _status = status;
      _busy = false;
    });
    if (!wasGranted && nowGranted && p.name == kCameraPermission) {
      widget.onRequestEngineRestart?.call();
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(
          const SnackBar(
            content: Text('Camera enabled — starting presence detection…'),
          ),
        );
      }
    }
  }

  Future<void> _invokeMaintenance(String method) async {
    try {
      await _maintenanceChannel.invokeMethod<void>(method);
    } catch (_) {
      // No channel (host/test) or the settings activity is unavailable: ignore.
    }
  }

  @override
  Widget build(BuildContext context) {
    final c = widget.controller;
    final s = c.settings;
    final status = _status;
    final knownNames = kKnownPermissions.map((p) => p.name).toSet();
    // Any declared permission the device reported that we have no metadata for —
    // shown by raw name so nothing the app requests is silently hidden.
    final extras =
        (status ?? const <String, bool>{}).keys
            .where((n) => !knownNames.contains(n))
            .toList()
          ..sort();
    return SettingsTabBody(
      left: [
        Row(
          children: [
            const Expanded(child: SettingsHeader('Permissions')),
            IconButton(
              key: const Key('settings-permissions-refresh'),
              tooltip: 'Refresh',
              icon: const Icon(Icons.refresh),
              onPressed: _busy ? null : _load,
            ),
          ],
        ),
        if (status == null)
          const Padding(
            padding: EdgeInsets.all(16),
            child: Text('Checking permissions…'),
          )
        else ...[
          for (final p in kKnownPermissions) _tile(p, status[p.name]),
          for (final name in extras)
            _tile(
              AppPermission(
                name: name,
                label: name,
                description: '',
                runtime: false,
              ),
              status[name],
            ),
        ],
      ],
      right: [
        const SettingsHeader('Settings view'),
        SettingsSwitch(
          key: const Key('settings-advanced-mode'),
          icon: Icons.tune,
          title: 'Advanced mode',
          info:
              'Shows expert tuning controls: wake-word smoothing, the Microphone '
              'tab, the VAD engine and thresholds, listening-ring timing, raw Drive '
              'folder IDs and the update URL. Turn off for a simpler view; it never '
              'changes how the device behaves.',
          value: s.advancedMode,
          onChanged: (v) => c.updateLocal(s.copyWith(advancedMode: v)),
        ),
        const SettingsHeader('Android'),
        ListTile(
          key: const Key('settings-permissions-app-settings'),
          dense: true,
          leading: const Icon(Icons.app_settings_alt_outlined),
          title: const Text('Open app settings'),
          subtitle: const Text('Toggle a permanently denied permission'),
          trailing: const Icon(Icons.open_in_new),
          onTap: _permissions.openAppSettings,
        ),
        ListTile(
          key: const Key('settings-switch-home'),
          dense: true,
          leading: const Icon(Icons.home_outlined),
          title: const Text('Switch Home app'),
          subtitle: const Text('Return to the stock launcher'),
          trailing: const Icon(Icons.open_in_new),
          onTap: () => _invokeMaintenance('openHomeSettings'),
        ),
        ListTile(
          key: const Key('settings-open-android-settings'),
          dense: true,
          leading: const Icon(Icons.settings_applications_outlined),
          title: const Text('Open Android settings'),
          subtitle: const Text('The full system Settings app'),
          trailing: const Icon(Icons.open_in_new),
          onTap: () => _invokeMaintenance('openSystemSettings'),
        ),
      ],
    );
  }

  /// One permission row: status icon + label + what it powers, with a trailing chip
  /// or (for a missing runtime permission) a Grant button. [granted] is null when
  /// the platform channel is absent (host/test).
  Widget _tile(AppPermission p, bool? granted) {
    final Widget trailing;
    if (granted == null) {
      trailing = const Text('—', style: TextStyle(color: Colors.grey));
    } else if (granted) {
      trailing = const StatusChip(
        label: 'Granted',
        color: Colors.green,
        icon: Icons.check_circle,
      );
    } else if (p.runtime) {
      trailing = FilledButton.tonal(
        key: Key('settings-permission-grant-${p.name}'),
        onPressed: _busy ? null : () => _request(p),
        child: const Text('Grant'),
      );
    } else {
      trailing = const StatusChip(
        label: 'Not granted',
        color: Colors.red,
        icon: Icons.cancel,
      );
    }
    return ListTile(
      key: Key('settings-permission-${p.name}'),
      dense: true,
      visualDensity: VisualDensity.compact,
      contentPadding: const EdgeInsets.symmetric(horizontal: 12),
      leading: Icon(
        granted == null
            ? Icons.help_outline
            : granted
            ? Icons.check_circle
            : Icons.error_outline,
        color: granted == null
            ? Colors.grey
            : granted
            ? Colors.green
            : Colors.orange,
      ),
      title: Text(p.label),
      subtitle: p.description.isEmpty
          ? null
          : Text(p.description, maxLines: 1, overflow: TextOverflow.ellipsis),
      trailing: trailing,
    );
  }
}
