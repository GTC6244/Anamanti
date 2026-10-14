// Settings → Display: what the screen looks like. Screen (dim delay, font size, the
// listening ring) and Photos (the idle slideshow source + Google linking).

import 'package:flutter/material.dart';

import 'package:qr_flutter/qr_flutter.dart';

import 'package:anamanti_display/src/settings/app_settings.dart';
import 'package:anamanti_display/src/settings/orchestrator_client.dart';
import 'package:anamanti_display/src/slideshow/ambient_photos.dart';
import 'package:anamanti_display/src/slideshow/drive_photos.dart';
import 'package:anamanti_display/src/ui/settings/settings_controller.dart';
import 'package:anamanti_display/src/ui/settings/settings_widgets.dart';

/// The fixed "dim the screen after" choices (seconds): 30s … 1h, ascending.
const List<int> kDimDelayPresets = <int>[
  30,
  60,
  120,
  300,
  600,
  900,
  1800,
  3600,
];

/// The preset closest to [seconds], so a persisted value that predates the preset
/// list (or an out-of-range one) still shows a valid selection.
int nearestDimPreset(int seconds) {
  var best = kDimDelayPresets.first;
  for (final p in kDimDelayPresets) {
    if ((p - seconds).abs() < (best - seconds).abs()) best = p;
  }
  return best;
}

/// Human-readable seconds → "45s" / "2m" / "1h".
String formatDimDelay(int s) {
  if (s < 60) return '${s}s';
  if (s % 3600 == 0) return '${s ~/ 3600}h';
  final m = s ~/ 60;
  final rem = s % 60;
  return rem == 0 ? '${m}m' : '${m}m ${rem}s';
}

/// Display → Screen.
class DisplayScreenTab extends StatelessWidget {
  const DisplayScreenTab({super.key, required this.controller});

  final SettingsController controller;

  @override
  Widget build(BuildContext context) {
    final c = controller;
    final s = c.settings;
    final dim = nearestDimPreset(s.dimDelaySecs);
    final ring = s.listeningRingEnabled;
    return SettingsTabBody(
      left: [
        const SettingsHeader(
          'Dim screen after',
          caption:
              'How long the screen stays bright after the room goes quiet. '
              'Approaching it brightens it again instantly.',
        ),
        Padding(
          padding: const EdgeInsets.fromLTRB(12, 4, 12, 4),
          child: Wrap(
            key: const Key('settings-dim-delay'),
            spacing: 6,
            runSpacing: 6,
            children: [
              for (final p in kDimDelayPresets)
                ChoiceChip(
                  key: Key('settings-dim-delay-$p'),
                  label: Text(formatDimDelay(p)),
                  selected: p == dim,
                  showCheckmark: false,
                  onSelected: (_) => c.updateLocal(s.copyWith(dimDelaySecs: p)),
                ),
            ],
          ),
        ),
        const SettingsHeader(
          'Text size',
          caption:
              'All on-screen text. Also adjustable by voice (“increase font”).',
        ),
        SettingsSlider(
          label: 'Font size',
          sliderKey: const Key('settings-font-scale'),
          value: s.fontScale,
          min: kFontScaleMin,
          max: kFontScaleMax,
          // Snap to 5% stops (finer than the 10% voice step).
          divisions: ((kFontScaleMax - kFontScaleMin) / 0.05).round(),
          format: (v) => '${(v * 100).round()}%',
          onChanged: (v) =>
              c.updateLocal(c.settings.copyWith(fontScale: v), commit: false),
          onChangeEnd: (_) => c.commitLocalSoon(),
        ),
        Padding(
          padding: const EdgeInsets.fromLTRB(12, 0, 12, 4),
          child: Text(
            'Preview: 10:42 · Partly cloudy',
            style: TextStyle(fontSize: 16 * s.fontScale),
          ),
        ),
      ],
      right: [
        const SettingsHeader(
          'Listening ring',
          caption: 'The glowing ring shown while the assistant listens.',
        ),
        SettingsSwitch(
          key: const Key('settings-listening-ring'),
          icon: Icons.blur_circular,
          title: 'Show listening ring',
          value: ring,
          onChanged: (v) => c.updateLocal(s.copyWith(listeningRingEnabled: v)),
        ),
        SettingsSlider(
          label: 'Reactivity',
          sliderKey: const Key('settings-ring-reactivity'),
          enabled: ring,
          value: s.ringReactivity,
          min: 0.25,
          max: 2.5,
          divisions: 45,
          format: (v) => '${v.toStringAsFixed(2)}×',
          onChanged: (v) => c.updateLocal(
            c.settings.copyWith(ringReactivity: v),
            commit: false,
          ),
          onChangeEnd: (_) => c.commitLocalSoon(),
        ),
        if (s.advancedMode) ...[
          SettingsSlider(
            label: 'Attack',
            sliderKey: const Key('settings-ring-attack'),
            enabled: ring,
            value: s.ringAttack,
            min: 0.1,
            max: 1.0,
            divisions: 18,
            format: (v) => v.toStringAsFixed(2),
            onChanged: (v) => c.updateLocal(
              c.settings.copyWith(ringAttack: v),
              commit: false,
            ),
            onChangeEnd: (_) => c.commitLocalSoon(),
          ),
          SettingsSlider(
            label: 'Release',
            sliderKey: const Key('settings-ring-release'),
            enabled: ring,
            value: s.ringRelease,
            min: 0.02,
            max: 0.5,
            divisions: 48,
            format: (v) => v.toStringAsFixed(2),
            onChanged: (v) => c.updateLocal(
              c.settings.copyWith(ringRelease: v),
              commit: false,
            ),
            onChangeEnd: (_) => c.commitLocalSoon(),
          ),
          SettingsSlider(
            label: 'Auto-range',
            sliderKey: const Key('settings-ring-decay'),
            enabled: ring,
            value: s.ringDecay,
            min: 0.90,
            max: 0.999,
            divisions: 99,
            format: (v) => v.toStringAsFixed(3),
            onChanged: (v) =>
                c.updateLocal(c.settings.copyWith(ringDecay: v), commit: false),
            onChangeEnd: (_) => c.commitLocalSoon(),
          ),
        ],
      ],
    );
  }
}

/// Display → Photos: the idle slideshow source and Google linking.
class DisplayPhotosTab extends StatefulWidget {
  const DisplayPhotosTab({super.key, required this.controller});

  final SettingsController controller;

  @override
  State<DisplayPhotosTab> createState() => _DisplayPhotosTabState();
}

class _DisplayPhotosTabState extends State<DisplayPhotosTab> {
  SettingsController get _c => widget.controller;
  OrchestratorClient get _client => _c.client;

  // Ambient-link QR dialog state: whether a QR dialog is showing, and whether the
  // user cancelled (so a late-completing poll doesn't apply a link they aborted).
  bool _qrDialogOpen = false;
  bool _linkCancelled = false;

  void _snack(String message) {
    ScaffoldMessenger.of(
      context,
    ).showSnackBar(SnackBar(content: Text(message)));
  }

  @override
  Widget build(BuildContext context) {
    final s = _c.settings;
    final source = s.photoSource;
    return SettingsTabBody(
      left: [
        const SettingsHeader('Idle photos'),
        Padding(
          padding: const EdgeInsets.fromLTRB(12, 4, 12, 8),
          child: SizedBox(
            width: double.infinity,
            child: SegmentedButton<PhotoSourceKind>(
              key: const Key('settings-photo-source'),
              showSelectedIcon: false,
              segments: const [
                ButtonSegment(
                  value: PhotoSourceKind.local,
                  label: Text('Gradients', key: Key('settings-photos-local')),
                ),
                ButtonSegment(
                  value: PhotoSourceKind.ambient,
                  label: Text(
                    'Google Photos',
                    key: Key('settings-photos-ambient'),
                  ),
                ),
                ButtonSegment(
                  value: PhotoSourceKind.drive,
                  label: Text('Drive', key: Key('settings-photos-drive')),
                ),
              ],
              selected: {source},
              onSelectionChanged: (sel) =>
                  _c.updateLocal(s.copyWith(photoSource: sel.first)),
            ),
          ),
        ),
        Padding(
          padding: const EdgeInsets.symmetric(horizontal: 12),
          child: Text(switch (source) {
            PhotoSourceKind.local =>
              'Built-in ambient gradients. Works offline; nothing to link.',
            PhotoSourceKind.ambient =>
              s.ambientLinked
                  ? 'Google Photos is linked.'
                  : 'Not linked. Needs Google Photos partner-program access.',
            PhotoSourceKind.drive =>
              s.driveLinked
                  ? 'Drive is linked · ${s.driveFolderIds.length} folder(s).'
                  : 'Drive is not linked yet.',
          }),
        ),
      ],
      right: [
        if (source == PhotoSourceKind.ambient) ..._ambientControls(s),
        if (source == PhotoSourceKind.drive) ..._driveControls(s),
      ],
    );
  }

  List<Widget> _ambientControls(AppSettings s) => [
    const SettingsHeader(
      'Google Photos',
      caption: 'Scan a QR to sign in on your phone, then pick the albums.',
    ),
    Padding(
      padding: const EdgeInsets.fromLTRB(12, 4, 12, 4),
      child: Align(
        alignment: Alignment.centerLeft,
        child: FilledButton.tonalIcon(
          key: const Key('settings-ambient-link'),
          onPressed: _linkAmbient,
          icon: const Icon(Icons.qr_code_2),
          label: Text(
            s.ambientLinked ? 'Re-link Google Photos' : 'Link Google Photos',
          ),
        ),
      ),
    ),
  ];

  List<Widget> _driveControls(AppSettings s) => [
    const SettingsHeader(
      'Google Drive',
      caption:
          '1. Link Drive on the Core’s config page (Photos tab). '
          '2. Sync here. 3. Choose folders. Sync also runs on boot.',
    ),
    Padding(
      padding: const EdgeInsets.fromLTRB(12, 4, 12, 4),
      child: Wrap(
        spacing: 8,
        runSpacing: 8,
        children: [
          FilledButton.tonalIcon(
            key: const Key('settings-drive-sync'),
            onPressed: _syncDriveFromCore,
            icon: const Icon(Icons.sync),
            label: Text(s.driveLinked ? 'Re-sync from Core' : 'Sync from Core'),
          ),
          FilledButton.icon(
            key: const Key('settings-drive-pick'),
            onPressed: s.driveConfigured && s.driveRefreshToken.isNotEmpty
                ? _pickDriveFolders
                : null,
            icon: const Icon(Icons.folder_open),
            label: const Text('Choose folders'),
          ),
        ],
      ),
    ),
    if (s.advancedMode)
      Padding(
        padding: const EdgeInsets.fromLTRB(12, 8, 12, 4),
        child: CommitTextField(
          key: const Key('settings-drive-folders'),
          text: s.driveFolderIds.join(', '),
          label: 'Drive folder ID(s) or link(s)',
          helper: 'Filled by “Choose folders”, or paste manually.',
          onChanged: (v) => _c.updateLocal(
            _c.settings.copyWith(driveFolderIds: parseDriveFolderIds(v)),
            commit: false,
          ),
          onCommit: _c.commitLocalSoon,
        ),
      ),
  ];

  /// Link Google Photos via the **Ambient API** device-code flow: a sign-in QR, then
  /// an album-picker QR, polling until the user has selected sources. All on-device.
  Future<void> _linkAmbient() async {
    _linkCancelled = false;
    final client = AmbientApiClient();
    try {
      final start = await client.requestDeviceCode();
      if (!mounted || _linkCancelled) return;
      _showQrDialog(
        title: 'Step 1 · Sign in',
        instruction: 'Scan with your phone and sign in to Google.',
        qrData: start.prompt.verificationUrlComplete,
        url: start.prompt.verificationUrl,
        code: start.prompt.userCode,
      );
      final tokens = await client.pollForTokens(start);
      if (!mounted || _linkCancelled) return;

      final device = await client.createDevice(tokens.accessToken);
      if (!mounted || _linkCancelled) return;
      _dismissQrDialog();
      _showQrDialog(
        title: 'Step 2 · Choose albums',
        instruction: 'Scan to open Google Photos and pick the albums to show.',
        qrData: device.settingsUri,
        url: device.settingsUri,
      );

      var current = device;
      while (!current.mediaSourcesSet) {
        await Future<void>.delayed(current.pollInterval);
        if (!mounted || _linkCancelled) return;
        current = await client.getDevice(tokens.accessToken, device.deviceId);
      }
      if (!mounted || _linkCancelled) return;
      _dismissQrDialog();
      final s = _c.settings;
      _c.updateLocal(
        s.copyWith(
          photoSource: PhotoSourceKind.ambient,
          ambientLinked: true,
          ambientRefreshToken: tokens.refreshToken ?? s.ambientRefreshToken,
          ambientDeviceId: device.deviceId,
        ),
      );
      _snack('Google Photos linked.');
    } catch (e) {
      if (!mounted) return;
      _dismissQrDialog();
      if (!_linkCancelled) _snack('Google Photos linking failed: $e');
    } finally {
      client.close();
    }
  }

  /// Pull the Google **Drive** bundle (client creds + refresh token + folder ids)
  /// from the Core, which owns consent (config page → Photos tab).
  Future<void> _syncDriveFromCore() async {
    _snack('Syncing Drive from the Core…');
    DriveTokenView token;
    try {
      token = await _client.fetchDriveToken();
    } catch (e) {
      if (!mounted) return;
      _snack('Could not reach the Core: $e');
      return;
    }
    if (!mounted) return;
    if (!token.configured) {
      _snack(
        'The Core has no Drive credentials yet. Link Drive on its config page '
        '(Photos tab), then Sync.',
      );
      return;
    }
    final s = _c.settings;
    _c.updateLocal(
      s.copyWith(
        photoSource: PhotoSourceKind.drive,
        driveClientId: token.clientId,
        driveClientSecret: token.clientSecret,
        driveRefreshToken: token.refreshToken,
        driveFolderIds: token.folderIds.isNotEmpty
            ? token.folderIds
            : s.driveFolderIds,
        driveLinked: token.linked,
      ),
    );
    _snack(
      token.linked
          ? 'Drive synced.'
          : 'Drive credentials synced, but not linked yet — link Drive on the '
                'Core config page, then Sync again.',
    );
  }

  /// Pick which Drive folders to show (owned + shared), without knowing folder IDs.
  Future<void> _pickDriveFolders() async {
    final s = _c.settings;
    showDialog<void>(
      context: context,
      barrierDismissible: false,
      builder: (_) => const AlertDialog(
        content: Row(
          children: [
            SizedBox(
              width: 20,
              height: 20,
              child: CircularProgressIndicator(strokeWidth: 2),
            ),
            SizedBox(width: 16),
            Text('Loading your Drive folders…'),
          ],
        ),
      ),
    );
    List<DriveFolder> folders;
    final client = AmbientApiClient(
      clientId: s.driveClientId,
      clientSecret: s.driveClientSecret,
    );
    try {
      final token = (await client.refresh(s.driveRefreshToken)).accessToken;
      folders = await listDriveFolders(accessToken: token);
    } catch (e) {
      if (mounted) Navigator.of(context, rootNavigator: true).pop();
      if (mounted) _snack('Could not list folders: $e');
      return;
    } finally {
      client.close();
    }
    if (!mounted) return;
    Navigator.of(context, rootNavigator: true).pop(); // dismiss loading

    final chosen = await showDialog<Set<String>>(
      context: context,
      builder: (_) => _DriveFolderPicker(
        folders: folders,
        initiallySelected: _c.settings.driveFolderIds.toSet(),
      ),
    );
    if (chosen == null || !mounted) return;
    _c.updateLocal(
      _c.settings.copyWith(
        photoSource: PhotoSourceKind.drive,
        driveFolderIds: chosen.toList(),
      ),
    );
    _snack('${chosen.length} folder(s) selected.');
  }

  void _showQrDialog({
    required String title,
    required String instruction,
    required String qrData,
    required String url,
    String? code,
  }) {
    _qrDialogOpen = true;
    showDialog<void>(
      context: context,
      barrierDismissible: false,
      builder: (ctx) => AlertDialog(
        key: const Key('ambient-qr-dialog'),
        title: Text(title),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text(instruction, textAlign: TextAlign.center),
            const SizedBox(height: 16),
            Container(
              color: Colors.white,
              padding: const EdgeInsets.all(12),
              child: QrImageView(
                data: qrData,
                size: 200,
                backgroundColor: Colors.white,
              ),
            ),
            const SizedBox(height: 16),
            SelectableText(url, textAlign: TextAlign.center),
            if (code != null) ...[
              const SizedBox(height: 8),
              SelectableText(
                code,
                style: Theme.of(
                  ctx,
                ).textTheme.headlineSmall?.copyWith(letterSpacing: 2),
              ),
            ],
          ],
        ),
        actions: [
          TextButton(
            onPressed: () {
              _linkCancelled = true;
              Navigator.of(ctx).pop();
            },
            child: const Text('Cancel'),
          ),
        ],
      ),
    ).then((_) => _qrDialogOpen = false);
  }

  void _dismissQrDialog() {
    if (_qrDialogOpen && mounted) {
      Navigator.of(context, rootNavigator: true).pop();
      _qrDialogOpen = false;
    }
  }
}

/// Parse a folder field into Drive folder IDs. Accepts bare IDs or folder share links
/// (`.../folders/<id>` or `?id=<id>`), comma / space / newline separated.
List<String> parseDriveFolderIds(String raw) {
  String extract(String token) {
    if (token.isEmpty) return '';
    final byPath = RegExp(r'/folders/([A-Za-z0-9_-]+)').firstMatch(token);
    if (byPath != null) return byPath.group(1)!;
    final byQuery = RegExp(r'[?&]id=([A-Za-z0-9_-]+)').firstMatch(token);
    if (byQuery != null) return byQuery.group(1)!;
    return token;
  }

  return raw
      .split(RegExp(r'[\s,]+'))
      .map((t) => extract(t.trim()))
      .where((t) => t.isNotEmpty)
      .toList();
}

/// A checkbox list of Drive folders. Returns the selected folder IDs (null if
/// cancelled).
class _DriveFolderPicker extends StatefulWidget {
  const _DriveFolderPicker({
    required this.folders,
    required this.initiallySelected,
  });

  final List<DriveFolder> folders;
  final Set<String> initiallySelected;

  @override
  State<_DriveFolderPicker> createState() => _DriveFolderPickerState();
}

class _DriveFolderPickerState extends State<_DriveFolderPicker> {
  late final Set<String> _selected = {...widget.initiallySelected};

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('Choose Drive folders'),
      content: SizedBox(
        width: double.maxFinite,
        child: widget.folders.isEmpty
            ? const Text('No folders found in this account.')
            : ListView(
                shrinkWrap: true,
                children: [
                  for (final f in widget.folders)
                    CheckboxListTile(
                      key: Key('folder-${f.id}'),
                      value: _selected.contains(f.id),
                      title: Text(f.name),
                      subtitle: f.shared ? const Text('Shared with me') : null,
                      onChanged: (v) => setState(() {
                        if (v == true) {
                          _selected.add(f.id);
                        } else {
                          _selected.remove(f.id);
                        }
                      }),
                    ),
                ],
              ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        FilledButton(
          onPressed: () => Navigator.of(context).pop(_selected),
          child: Text('Use ${_selected.length}'),
        ),
      ],
    );
  }
}
