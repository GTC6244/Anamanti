// Settings screen (Plan.MD §3, Phase 6).
//
// Exposes the four configurable areas from the plan:
//  * Wake word (+ detection thresholds) — device-local, applied by restarting the
//    native engine with a new config.
//  * LLM backend, model, and TTS voice — orchestrator-managed, read/changed over
//    the Wyoming control protocol.
//  * Photo source — local ambient gradients or a linked Google folder (on-device
//    OAuth seam).
//  * Memory management — a link into the view/delete list ([MemoryScreen]).
//
// Device-local settings are persisted with [SettingsStore]; remote settings are
// applied on the Mac. Both happen when the user taps Save; the parent is notified
// via [onApplied] so it can restart the engine and refresh the slideshow.

import 'package:flutter/material.dart';

import 'package:qr_flutter/qr_flutter.dart';

import 'package:ambient_display/src/settings/app_settings.dart';
import 'package:ambient_display/src/settings/orchestrator_client.dart';
import 'package:ambient_display/src/settings/settings_store.dart';
import 'package:ambient_display/src/slideshow/ambient_photos.dart';
import 'package:ambient_display/src/slideshow/drive_photos.dart';
import 'package:ambient_display/src/slideshow/google_oauth_config.dart';
import 'package:ambient_display/src/slideshow/google_token_import.dart';
import 'package:ambient_display/src/ui/memory_screen.dart';
import 'package:ambient_display/src/ui/people_screen.dart';

/// LLM backends the settings screen can select. Labels are user-facing; the value
/// is the orchestrator's backend label.
const Map<String, String> _kBackends = {
  'ollama': 'Local (Ollama)',
  'anthropic': 'Cloud (Claude)',
  'openai': 'Cloud (OpenAI)',
  'mock': 'Offline echo (mock)',
};

/// Backends whose model is chosen from the orchestrator's last-12-months catalog
/// (a dropdown); other backends keep a free-text model field (e.g. an Ollama tag).
const Set<String> _kCloudBackends = {'anthropic', 'openai'};

class SettingsScreen extends StatefulWidget {
  const SettingsScreen({
    super.key,
    required this.initial,
    required this.store,
    required this.client,
    required this.onApplied,
  });

  /// The current device-local settings to edit.
  final AppSettings initial;

  /// Persistence for the device-local settings.
  final SettingsStore store;

  /// Client for the orchestrator-managed settings + memory.
  final OrchestratorClient client;

  /// Called after a successful Save with the new device-local settings, so the app
  /// can restart the engine (wake word/thresholds) and refresh the slideshow.
  final ValueChanged<AppSettings> onApplied;

  @override
  State<SettingsScreen> createState() => _SettingsScreenState();
}

class _SettingsScreenState extends State<SettingsScreen> {
  late AppSettings _settings = widget.initial;

  final TextEditingController _modelController = TextEditingController();
  final TextEditingController _voiceController = TextEditingController();
  final TextEditingController _folderController = TextEditingController();

  String _backend = 'ollama';
  // Anthropic auth mode: 'apikey' or 'subscription' (Claude OAuth).
  String _anthropicAuth = 'apikey';
  bool _remoteLoading = true;
  String? _remoteError;
  bool _saving = false;

  // Ambient-link QR dialog state: whether a QR dialog is showing, and whether the
  // user cancelled (so a late-completing poll doesn't apply a link they aborted).
  bool _qrDialogOpen = false;
  bool _linkCancelled = false;

  /// Selectable models (last 12 months) from the orchestrator, for the dropdown.
  List<ModelOption> _models = const <ModelOption>[];

  /// Installed Piper voices from the orchestrator, for the voice dropdown. Empty
  /// when the Mac is unreachable — the voice field then falls back to free text.
  List<VoiceOption> _voices = const <VoiceOption>[];

  /// Orchestrators discovered on the LAN, for the device-local Orchestrator
  /// dropdown. Populated by pure mDNS (independent of the selected orchestrator's
  /// reachability), so the picker works even when the current selection is offline.
  List<OrchestratorOption> _orchestrators = const <OrchestratorOption>[];

  // Orchestrator-side VAD tuning (loaded from the Mac, applied on Save).
  int _endSilenceMs = 700;
  double _voiceRmsThreshold = 120;

  @override
  void initState() {
    super.initState();
    _folderController.text = _settings.driveFolderIds.join(', ');
    _loadRemote();
  }

  @override
  void dispose() {
    _modelController.dispose();
    _voiceController.dispose();
    _folderController.dispose();
    super.dispose();
  }

  Future<void> _loadRemote() async {
    setState(() {
      _remoteLoading = true;
      _remoteError = null;
    });
    // Discover orchestrators first, in their own best-effort try: this is a pure
    // mDNS browse independent of whether the *selected* orchestrator is reachable,
    // so the Orchestrator dropdown populates even when the pinned Mac is offline.
    List<OrchestratorOption> orchestrators;
    try {
      orchestrators = await widget.client.listOrchestrators();
    } catch (_) {
      orchestrators = const <OrchestratorOption>[];
    }
    if (mounted) setState(() => _orchestrators = orchestrators);
    try {
      final remote = await widget.client.fetchSettings();
      // The model catalog is best-effort: if it fails, the model field falls back
      // to free text so the user is never blocked.
      List<ModelOption> models;
      try {
        models = await widget.client.listModels();
      } catch (_) {
        models = const <ModelOption>[];
      }
      // The voice list is best-effort too: if it fails, the voice field falls back
      // to a free-text Piper voice name.
      List<VoiceOption> voices;
      try {
        voices = await widget.client.listVoices();
      } catch (_) {
        voices = const <VoiceOption>[];
      }
      if (!mounted) return;
      setState(() {
        _models = models;
        _voices = voices;
        _backend = _kBackends.containsKey(remote.llmBackend)
            ? remote.llmBackend
            : 'ollama';
        _anthropicAuth = remote.anthropicAuth == 'subscription'
            ? 'subscription'
            : 'apikey';
        _modelController.text = remote.llmModel ?? '';
        _voiceController.text = remote.ttsVoice ?? '';
        // Adopt the orchestrator's live VAD values (0 = unknown → keep the default).
        if (remote.endSilenceMs > 0) _endSilenceMs = remote.endSilenceMs;
        if (remote.voiceRmsThreshold > 0) {
          _voiceRmsThreshold = remote.voiceRmsThreshold;
        }
        _remoteLoading = false;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() {
        _remoteError = '$e';
        _remoteLoading = false;
      });
    }
  }

  /// Link Google Photos via the **Ambient API** device-code flow. Shows a sign-in QR
  /// (approve on a phone), creates an ambient device, shows a 2nd QR to pick albums
  /// in the Google Photos app, polls until the user has selected sources, then stores
  /// the refresh token + device id. All on-device — no keyboard, no Mac.
  Future<void> _linkAmbient() async {
    _linkCancelled = false;
    final client = AmbientApiClient();
    try {
      // 1. Sign-in QR.
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

      // 2. Create the ambient device, then show the album-picker QR.
      final device = await client.createDevice(tokens.accessToken);
      if (!mounted || _linkCancelled) return;
      _dismissQrDialog();
      _showQrDialog(
        title: 'Step 2 · Choose albums',
        instruction: 'Scan to open Google Photos and pick the albums to show.',
        qrData: device.settingsUri,
        url: device.settingsUri,
      );

      // 3. Poll until the user has selected media sources.
      var current = device;
      while (!current.mediaSourcesSet) {
        await Future<void>.delayed(current.pollInterval);
        if (!mounted || _linkCancelled) return;
        current = await client.getDevice(tokens.accessToken, device.deviceId);
      }
      if (!mounted || _linkCancelled) return;
      _dismissQrDialog();
      setState(() {
        _settings = _settings.copyWith(
          photoSource: PhotoSourceKind.ambient,
          ambientLinked: true,
          ambientRefreshToken:
              tokens.refreshToken ?? _settings.ambientRefreshToken,
          ambientDeviceId: device.deviceId,
        );
      });
      _snack('Google Photos linked. Tap Save to apply.');
    } catch (e) {
      if (!mounted) return;
      _dismissQrDialog();
      if (!_linkCancelled) _snack('Google Photos linking failed: $e');
    } finally {
      client.close();
    }
  }

  /// Import the Google **Drive** refresh token synced from the Mac consent helper
  /// (`tools/google_photo_consent.py` → adb-pushed to the app's external files dir).
  /// Drive's `drive.readonly` scope can't be granted on-device, so consent runs on
  /// the Mac and the token is read here.
  Future<void> _importDriveToken() async {
    final imported = await importDriveTokenFromFile();
    if (!mounted) return;
    if (imported == null) {
      _snack(
        'No token file found. Run the Mac consent helper and adb-push it first.',
      );
      return;
    }
    setState(() {
      if (imported.folderIds.isNotEmpty) {
        _folderController.text = imported.folderIds.join(', ');
      }
      _settings = _settings.copyWith(
        photoSource: PhotoSourceKind.drive,
        driveLinked: true,
        driveRefreshToken: imported.refreshToken,
      );
    });
    final extra = imported.folderIds.isNotEmpty
        ? ' + ${imported.folderIds.length} folder(s)'
        : '';
    _snack(
      'Imported Drive token$extra. Add folder ID(s) if needed, then Save.',
    );
  }

  /// Show a picker of the account's Drive folders (owned + shared) so the user can
  /// choose which to display without knowing folder IDs. Needs an imported Drive
  /// token (mints an access token from it) and the Desktop client creds.
  Future<void> _pickDriveFolders() async {
    if (!kGoogleDriveConfigured) {
      _snack('Drive client not configured in this build.');
      return;
    }
    if (_settings.driveRefreshToken.isEmpty) {
      _snack('Import a Drive token first (Import Drive token).');
      return;
    }
    // Brief loading dialog while we mint a token + list folders.
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
      clientId: kGoogleDriveClientId,
      clientSecret: kGoogleDriveClientSecret,
    );
    try {
      final token = (await client.refresh(
        _settings.driveRefreshToken,
      )).accessToken;
      folders = await listDriveFolders(accessToken: token);
    } catch (e) {
      if (mounted) Navigator.of(context, rootNavigator: true).pop(); // loading
      if (mounted) _snack('Could not list folders: $e');
      return;
    } finally {
      client.close();
    }
    if (!mounted) return;
    Navigator.of(context, rootNavigator: true).pop(); // dismiss loading

    final preselected = _parseFolderIds(_folderController.text).toSet();
    final chosen = await showDialog<Set<String>>(
      context: context,
      builder: (_) =>
          _DriveFolderPicker(folders: folders, initiallySelected: preselected),
    );
    if (chosen == null || !mounted) return;
    setState(() {
      _folderController.text = chosen.join(', ');
      _settings = _settings.copyWith(
        photoSource: PhotoSourceKind.drive,
        driveFolderIds: chosen.toList(),
      );
    });
    _snack('${chosen.length} folder(s) selected. Tap Save to apply.');
  }

  /// Parse the folder field into Drive folder IDs. Accepts bare IDs or full folder
  /// share links (`.../folders/<id>`), comma/space/newline separated.
  List<String> _parseFolderIds(String raw) {
    return raw
        .split(RegExp(r'[\s,]+'))
        .map((t) => _extractFolderId(t.trim()))
        .where((t) => t.isNotEmpty)
        .toList();
  }

  String _extractFolderId(String token) {
    if (token.isEmpty) return '';
    final byPath = RegExp(r'/folders/([A-Za-z0-9_-]+)').firstMatch(token);
    if (byPath != null) return byPath.group(1)!;
    final byQuery = RegExp(r'[?&]id=([A-Za-z0-9_-]+)').firstMatch(token);
    if (byQuery != null) return byQuery.group(1)!;
    return token;
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

  Future<void> _save() async {
    setState(() => _saving = true);

    // 1. Persist + apply the device-local settings (wake word, thresholds, photo).
    final local = _settings.copyWith(
      driveFolderIds: _parseFolderIds(_folderController.text),
    );
    await widget.store.save(local);
    widget.onApplied(local);

    // 2. Apply the orchestrator-managed settings, if the Mac was reachable.
    String? remoteNote;
    if (_remoteError == null) {
      try {
        final result = await widget.client.applySettings(
          llmBackend: _backend,
          llmModel: _modelController.text.trim().isEmpty
              ? null
              : _modelController.text.trim(),
          anthropicAuth: _backend == 'anthropic' ? _anthropicAuth : null,
          setTtsVoice: true,
          ttsVoice: _voiceController.text.trim().isEmpty
              ? null
              : _voiceController.text.trim(),
          endSilenceMs: _endSilenceMs,
          voiceRmsThreshold: _voiceRmsThreshold,
        );
        if (!result.ok) remoteNote = 'Assistant: ${result.message}';
      } catch (e) {
        remoteNote = 'Assistant offline — device settings saved. ($e)';
      }
    }

    if (!mounted) return;
    setState(() {
      _settings = local;
      _saving = false;
    });
    _snack(remoteNote ?? 'Settings saved');
    Navigator.of(context).maybePop();
  }

  void _snack(String message) {
    ScaffoldMessenger.of(
      context,
    ).showSnackBar(SnackBar(content: Text(message)));
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('Settings'),
        actions: [
          _saving
              ? const Padding(
                  padding: EdgeInsets.all(16),
                  child: SizedBox(
                    width: 20,
                    height: 20,
                    child: CircularProgressIndicator(strokeWidth: 2),
                  ),
                )
              : TextButton(
                  key: const Key('settings-save'),
                  onPressed: _save,
                  child: const Text('Save'),
                ),
        ],
      ),
      body: ListView(
        padding: const EdgeInsets.symmetric(vertical: 8),
        children: [
          _section('Wake word'),
          _wakeWordTile(),
          _thresholdTile(),
          const Divider(),
          _section('Assistant'),
          // Device-local: which orchestrator this display talks to. Shown above
          // (and outside) the orchestrator-fetched tiles so it stays usable even
          // when the selected orchestrator is offline.
          _orchestratorTile(),
          ..._assistantTiles(),
          const Divider(),
          _section('Idle photos'),
          ..._photoTiles(),
          const Divider(),
          _section('Speech & detection'),
          ..._detectionTuningTiles(),
          ..._speechTiles(),
          const Divider(),
          _section('Memory'),
          ListTile(
            key: const Key('settings-memory'),
            leading: const Icon(Icons.psychology_outlined),
            title: const Text('Manage remembered facts'),
            subtitle: const Text(
              'View and delete what the assistant remembers',
            ),
            trailing: const Icon(Icons.chevron_right),
            onTap: () => Navigator.of(context).push(
              MaterialPageRoute<void>(
                builder: (_) => MemoryScreen(client: widget.client),
              ),
            ),
          ),
          const Divider(),
          _section('People'),
          ListTile(
            key: const Key('settings-people'),
            leading: const Icon(Icons.groups_outlined),
            title: const Text('Manage people'),
            subtitle: const Text('Name the voices the assistant recognizes'),
            trailing: const Icon(Icons.chevron_right),
            onTap: () => Navigator.of(context).push(
              MaterialPageRoute<void>(
                builder: (_) => PeopleScreen(client: widget.client),
              ),
            ),
          ),
          const SizedBox(height: 24),
        ],
      ),
    );
  }

  Widget _section(String title) => Padding(
    padding: const EdgeInsets.fromLTRB(16, 16, 16, 8),
    child: Text(
      title.toUpperCase(),
      style: Theme.of(context).textTheme.labelMedium?.copyWith(
        letterSpacing: 1.1,
        color: Theme.of(context).colorScheme.primary,
      ),
    ),
  );

  /// Device-local picker for which orchestrator this display connects to. "Auto"
  /// (empty key) uses the first available orchestrator; selecting a specific one
  /// pins the device to it (strict: it stays offline rather than switching if that
  /// orchestrator is unreachable). Persisted via [SettingsStore]; a change restarts
  /// the engine (see `main.dart`). A persisted selection not currently discovered
  /// stays selected with a "(not found)" label so it isn't silently dropped.
  Widget _orchestratorTile() {
    final current = _settings.orchestratorKey.trim();
    final keys = _orchestrators.map((o) => o.key).toSet();
    final items = <DropdownMenuItem<String>>[
      const DropdownMenuItem(value: '', child: Text('Auto (first available)')),
      for (final o in _orchestrators)
        DropdownMenuItem(
          value: o.key,
          child: Text(o.host.isEmpty ? o.name : '${o.name} · ${o.host}'),
        ),
      if (current.isNotEmpty && !keys.contains(current))
        DropdownMenuItem(value: current, child: Text('$current (not found)')),
    ];
    return ListTile(
      leading: const Icon(Icons.dns_outlined),
      title: const Text('Orchestrator'),
      subtitle: const Text('Which Mac this display connects to'),
      trailing: DropdownButton<String>(
        key: const Key('settings-orchestrator'),
        value: current.isEmpty ? '' : current,
        items: items,
        onChanged: (v) => setState(
          () => _settings = _settings.copyWith(orchestratorKey: v ?? ''),
        ),
      ),
    );
  }

  Widget _wakeWordTile() {
    // The current wake word may not be in the built-in list (dropped in manually);
    // include it so the dropdown always has a valid selection.
    final items = {...kAvailableWakeWords, _settings.wakeWord}.toList();
    return ListTile(
      leading: const Icon(Icons.record_voice_over),
      title: const Text('Wake word'),
      trailing: DropdownButton<String>(
        key: const Key('settings-wakeword'),
        value: _settings.wakeWord,
        items: [
          for (final w in items)
            DropdownMenuItem(value: w, child: Text(w.replaceAll('_', ' '))),
        ],
        onChanged: (v) {
          if (v != null) {
            setState(() => _settings = _settings.copyWith(wakeWord: v));
          }
        },
      ),
    );
  }

  Widget _thresholdTile() {
    return Column(
      children: [
        _slider(
          label: 'Sensitivity (idle)',
          value: _settings.threshold,
          onChanged: (v) =>
              setState(() => _settings = _settings.copyWith(threshold: v)),
        ),
        _slider(
          label: 'Sensitivity while speaking',
          value: _settings.activeThreshold,
          onChanged: (v) => setState(
            () => _settings = _settings.copyWith(activeThreshold: v),
          ),
        ),
      ],
    );
  }

  Widget _slider({
    required String label,
    required double value,
    required ValueChanged<double> onChanged,
  }) {
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 16),
      child: Row(
        children: [
          SizedBox(width: 190, child: Text(label)),
          Expanded(
            child: Slider(
              min: 0.1,
              max: 0.95,
              divisions: 17,
              value: value.clamp(0.1, 0.95),
              label: value.toStringAsFixed(2),
              onChanged: onChanged,
            ),
          ),
          SizedBox(width: 44, child: Text(value.toStringAsFixed(2))),
        ],
      ),
    );
  }

  /// A general-purpose labelled slider with a configurable range + value format,
  /// for the detection/playback tuning knobs (which aren't all 0..1).
  Widget _rangeSlider({
    required String label,
    required double value,
    required double min,
    required double max,
    required int divisions,
    required String Function(double) format,
    required ValueChanged<double> onChanged,
    Key? sliderKey,
  }) {
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 16),
      child: Row(
        children: [
          SizedBox(width: 190, child: Text(label)),
          Expanded(
            child: Slider(
              key: sliderKey,
              min: min,
              max: max,
              divisions: divisions,
              value: value.clamp(min, max),
              label: format(value),
              onChanged: onChanged,
            ),
          ),
          SizedBox(width: 56, child: Text(format(value))),
        ],
      ),
    );
  }

  /// Wake-word detection tuning: smoothing window + fire-on-peak (WakeWordDetection
  /// responsiveness levers, adjustable on-device).
  List<Widget> _detectionTuningTiles() {
    return [
      _rangeSlider(
        label: 'Smoothing window',
        value: _settings.smoothingWindow.toDouble(),
        min: 1,
        max: 6,
        divisions: 5,
        format: (v) => v.round().toString(),
        sliderKey: const Key('settings-smoothing'),
        onChanged: (v) => setState(
          () => _settings = _settings.copyWith(smoothingWindow: v.round()),
        ),
      ),
      SwitchListTile(
        key: const Key('settings-fire-on-peak'),
        secondary: const Icon(Icons.bolt),
        title: const Text('Fire on peak'),
        subtitle: const Text(
          'Trigger on the strongest frame, not the average — snappier for quiet wake words',
        ),
        value: _settings.fireOnPeak,
        onChanged: (v) =>
            setState(() => _settings = _settings.copyWith(fireOnPeak: v)),
      ),
      SwitchListTile(
        key: const Key('settings-use-audiorecord'),
        secondary: const Icon(Icons.settings_voice),
        title: const Text('AudioRecord capture (far-field)'),
        subtitle: const Text(
          'Android: capture via VOICE_RECOGNITION + platform noise-suppression/AGC instead of cpal',
        ),
        value: _settings.useAudioRecord,
        onChanged: (v) =>
            setState(() => _settings = _settings.copyWith(useAudioRecord: v)),
      ),
    ];
  }

  /// Speech + playback tuning: playback buffer depth and the local end-of-speech
  /// cue (silence window + level threshold).
  List<Widget> _speechTiles() {
    return [
      _rangeSlider(
        label: 'Playback buffer (s)',
        value: _settings.playbackBufferSecs.toDouble(),
        min: 2,
        max: 60,
        divisions: 58,
        format: (v) => '${v.round()}s',
        sliderKey: const Key('settings-playback-buffer'),
        onChanged: (v) => setState(
          () => _settings = _settings.copyWith(playbackBufferSecs: v.round()),
        ),
      ),
      SwitchListTile(
        key: const Key('settings-endpoint-cue'),
        secondary: const Icon(Icons.hourglass_top),
        title: const Text('Instant "processing" cue'),
        subtitle: const Text(
          'Show a processing indicator the moment you stop speaking, before the reply',
        ),
        value: _settings.endpointCueEnabled,
        onChanged: (v) => setState(
          () => _settings = _settings.copyWith(endpointCueEnabled: v),
        ),
      ),
      if (_settings.endpointCueEnabled) ...[
        _rangeSlider(
          label: 'End-of-speech silence (ms)',
          value: _settings.endpointSilenceMs.toDouble(),
          min: 200,
          max: 1500,
          divisions: 26,
          format: (v) => '${v.round()}',
          sliderKey: const Key('settings-endpoint-silence'),
          onChanged: (v) => setState(
            () => _settings = _settings.copyWith(endpointSilenceMs: v.round()),
          ),
        ),
        _rangeSlider(
          label: 'Silence level',
          value: _settings.endpointRmsThreshold,
          min: 0.002,
          max: 0.05,
          divisions: 48,
          format: (v) => v.toStringAsFixed(3),
          sliderKey: const Key('settings-endpoint-level'),
          onChanged: (v) => setState(
            () => _settings = _settings.copyWith(endpointRmsThreshold: v),
          ),
        ),
      ],
    ];
  }

  List<Widget> _assistantTiles() {
    if (_remoteLoading) {
      return const [
        ListTile(
          leading: SizedBox(
            width: 24,
            height: 24,
            child: CircularProgressIndicator(strokeWidth: 2),
          ),
          title: Text('Contacting the assistant…'),
        ),
      ];
    }
    if (_remoteError != null) {
      return [
        ListTile(
          leading: const Icon(Icons.cloud_off),
          title: const Text('Assistant offline'),
          subtitle: const Text(
            'LLM and voice settings need the Mac to be reachable.',
          ),
          trailing: TextButton(
            onPressed: _loadRemote,
            child: const Text('Retry'),
          ),
        ),
      ];
    }
    return [
      ListTile(
        leading: const Icon(Icons.smart_toy_outlined),
        title: const Text('LLM backend'),
        trailing: DropdownButton<String>(
          key: const Key('settings-backend'),
          value: _backend,
          items: [
            for (final entry in _kBackends.entries)
              DropdownMenuItem(value: entry.key, child: Text(entry.value)),
          ],
          onChanged: (v) {
            if (v == null || v == _backend) return;
            setState(() {
              _backend = v;
              // Switching backend resets the model to that backend's default so a
              // stale model id from another provider is never sent.
              _modelController.text = '';
            });
          },
        ),
      ),
      _authField(),
      _modelField(),
      _voiceField(),
      // Orchestrator-side end-of-speech VAD tuning (applied on the Mac). Shortening
      // the silence window cuts the wait before the reply; lowering the level helps
      // a quiet far-field mic register as speech instead of hitting the slow timeout.
      _rangeSlider(
        label: 'End-of-speech wait (ms)',
        value: _endSilenceMs.toDouble(),
        min: 300,
        max: 1500,
        divisions: 24,
        format: (v) => '${v.round()}',
        sliderKey: const Key('settings-vad-silence'),
        onChanged: (v) => setState(() => _endSilenceMs = v.round()),
      ),
      _rangeSlider(
        label: 'Voice level threshold',
        value: _voiceRmsThreshold,
        min: 20,
        max: 400,
        divisions: 38,
        format: (v) => '${v.round()}',
        sliderKey: const Key('settings-vad-level'),
        onChanged: (v) => setState(() => _voiceRmsThreshold = v),
      ),
    ];
  }

  /// The Anthropic auth selector (API key vs Claude subscription/OAuth), shown only
  /// for the Anthropic backend. OpenAI is API-key-only (no subscription→API path).
  Widget _authField() {
    if (_backend == 'anthropic') {
      return ListTile(
        leading: const Icon(Icons.key_outlined),
        title: const Text('Anthropic auth'),
        subtitle: Text(
          _anthropicAuth == 'subscription'
              ? 'Claude subscription (OAuth via claude setup-token)'
              : 'API key (ANTHROPIC_API_KEY)',
        ),
        trailing: DropdownButton<String>(
          key: const Key('settings-anthropic-auth'),
          value: _anthropicAuth == 'subscription' ? 'subscription' : 'apikey',
          items: const [
            DropdownMenuItem(value: 'apikey', child: Text('API key')),
            DropdownMenuItem(
              value: 'subscription',
              child: Text('Subscription'),
            ),
          ],
          onChanged: (v) {
            if (v != null) setState(() => _anthropicAuth = v);
          },
        ),
      );
    }
    if (_backend == 'openai') {
      return const Padding(
        padding: EdgeInsets.fromLTRB(16, 4, 16, 8),
        child: Text(
          'OpenAI uses an API key (OPENAI_API_KEY). Subscription auth is not available for OpenAI.',
          style: TextStyle(fontSize: 12, color: Colors.grey),
        ),
      );
    }
    return const SizedBox.shrink();
  }

  /// The model control: a dropdown of the orchestrator's last-12-months models for
  /// cloud backends (Anthropic/OpenAI), or a free-text field for local/mock (e.g. an
  /// Ollama tag). A model pinned outside the fetched list stays selectable.
  Widget _modelField() {
    if (!_kCloudBackends.contains(_backend)) {
      return Padding(
        padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
        child: TextField(
          key: const Key('settings-model'),
          controller: _modelController,
          decoration: const InputDecoration(
            labelText: 'Model',
            hintText: 'e.g. llama3.2 or qwen2.5',
          ),
        ),
      );
    }

    final current = _modelController.text.trim();
    final providerModels = _models
        .where((m) => m.provider == _backend)
        .toList();
    final ids = providerModels.map((m) => m.id).toSet();
    final items = <DropdownMenuItem<String>>[
      const DropdownMenuItem(value: '', child: Text('Backend default')),
      for (final m in providerModels)
        DropdownMenuItem(value: m.id, child: Text(m.label)),
      // Keep a pinned model that isn't in the fetched list selectable + visible.
      if (current.isNotEmpty && !ids.contains(current))
        DropdownMenuItem(value: current, child: Text('$current (pinned)')),
    ];

    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 8, 16, 8),
      child: InputDecorator(
        decoration: const InputDecoration(
          labelText: 'Model',
          helperText: 'Released in the last 12 months',
        ),
        child: DropdownButton<String>(
          key: const Key('settings-model'),
          isExpanded: true,
          underline: const SizedBox.shrink(),
          value: current.isEmpty ? '' : current,
          items: items,
          onChanged: (v) => setState(() => _modelController.text = v ?? ''),
        ),
      ),
    );
  }

  /// The TTS voice control: a dropdown of the orchestrator's installed Piper
  /// voices (with a "Server default" entry), or a free-text field when the voice
  /// list is unavailable (Mac offline / no voices reported). A voice that isn't in
  /// the fetched list stays selectable + visible so a hand-set value is preserved.
  Widget _voiceField() {
    if (_voices.isEmpty) {
      return Padding(
        padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
        child: TextField(
          key: const Key('settings-voice'),
          controller: _voiceController,
          decoration: const InputDecoration(
            labelText: 'TTS voice',
            hintText: 'Piper voice, e.g. en_US-amy-medium (blank = default)',
          ),
        ),
      );
    }

    final current = _voiceController.text.trim();
    final names = _voices.map((v) => v.name).toSet();
    String labelFor(VoiceOption v) =>
        v.language == null ? v.label : '${v.label} · ${v.language}';
    final items = <DropdownMenuItem<String>>[
      const DropdownMenuItem(value: '', child: Text('Server default')),
      for (final v in _voices)
        DropdownMenuItem(value: v.name, child: Text(labelFor(v))),
      // Keep a hand-set voice that isn't in the installed list selectable.
      if (current.isNotEmpty && !names.contains(current))
        DropdownMenuItem(
          value: current,
          child: Text('$current (not installed)'),
        ),
    ];

    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 8, 16, 8),
      child: InputDecorator(
        decoration: const InputDecoration(
          labelText: 'TTS voice',
          helperText: 'Installed Piper voices',
        ),
        child: DropdownButton<String>(
          key: const Key('settings-voice'),
          isExpanded: true,
          underline: const SizedBox.shrink(),
          value: current.isEmpty ? '' : current,
          items: items,
          onChanged: (v) => setState(() => _voiceController.text = v ?? ''),
        ),
      ),
    );
  }

  List<Widget> _photoTiles() {
    return [
      RadioGroup<PhotoSourceKind>(
        groupValue: _settings.photoSource,
        onChanged: (v) => setState(
          () => _settings = _settings.copyWith(
            photoSource: v ?? PhotoSourceKind.local,
          ),
        ),
        child: Column(
          children: [
            const RadioListTile<PhotoSourceKind>(
              key: Key('settings-photos-local'),
              value: PhotoSourceKind.local,
              title: Text('Local ambient gradients'),
            ),
            RadioListTile<PhotoSourceKind>(
              key: const Key('settings-photos-ambient'),
              value: PhotoSourceKind.ambient,
              title: const Text('Google Photos (Ambient API)'),
              subtitle: Text(
                _settings.ambientLinked
                    ? 'Linked'
                    : 'Not linked · needs partner-program access',
              ),
            ),
            RadioListTile<PhotoSourceKind>(
              key: const Key('settings-photos-drive'),
              value: PhotoSourceKind.drive,
              title: const Text('Google Drive folder'),
              subtitle: Text(
                _settings.driveLinked
                    ? 'Linked · ${_settings.driveFolderIds.length} folder(s)'
                    : 'Not linked',
              ),
            ),
          ],
        ),
      ),
      if (_settings.photoSource == PhotoSourceKind.ambient) ...[
        const Padding(
          padding: EdgeInsets.fromLTRB(16, 0, 16, 8),
          child: Text(
            'Link with a QR: sign in on your phone, then pick the albums to show. '
            'Uses the Google Photos Ambient API (requires partner-program access).',
            style: TextStyle(fontSize: 12, color: Colors.grey),
          ),
        ),
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
          child: Align(
            alignment: Alignment.centerLeft,
            child: FilledButton.tonalIcon(
              key: const Key('settings-ambient-link'),
              onPressed: _linkAmbient,
              icon: const Icon(Icons.qr_code_2),
              label: Text(
                _settings.ambientLinked
                    ? 'Re-link Google Photos'
                    : 'Link Google Photos',
              ),
            ),
          ),
        ),
      ],
      if (_settings.photoSource == PhotoSourceKind.drive) ...[
        const Padding(
          padding: EdgeInsets.fromLTRB(16, 0, 16, 8),
          child: Text(
            '1. Consent once on the Mac (tools/google_photo_consent.py) → Import the '
            'synced token.  2. Choose folders (or paste IDs).',
            style: TextStyle(fontSize: 12, color: Colors.grey),
          ),
        ),
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
          child: Wrap(
            spacing: 8,
            runSpacing: 8,
            crossAxisAlignment: WrapCrossAlignment.center,
            children: [
              FilledButton.tonalIcon(
                key: const Key('settings-drive-import'),
                onPressed: _importDriveToken,
                icon: const Icon(Icons.download_for_offline_outlined),
                label: Text(
                  _settings.driveLinked
                      ? 'Re-import Drive token'
                      : 'Import Drive token',
                ),
              ),
              FilledButton.icon(
                key: const Key('settings-drive-pick'),
                onPressed: _settings.driveLinked ? _pickDriveFolders : null,
                icon: const Icon(Icons.folder_open),
                label: const Text('Choose folders'),
              ),
            ],
          ),
        ),
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
          child: TextField(
            key: const Key('settings-drive-folders'),
            controller: _folderController,
            decoration: const InputDecoration(
              labelText: 'Drive folder ID(s) or link(s)',
              helperText: 'Filled by "Choose folders", or paste manually.',
            ),
          ),
        ),
      ],
    ];
  }
}

/// A checkbox list of Drive folders for the user to pick which to show. Returns the
/// set of selected folder IDs (or null if cancelled).
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
