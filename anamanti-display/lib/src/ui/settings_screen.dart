// Settings screen (Plan.MD §3, Phase 6).
//
// A paged settings surface: the root shows a menu of categories, and tapping one
// opens that category's page (back returns to the menu). The categories are:
//  * Assistant — orchestrator pin + LLM backend, model, and TTS voice
//    (orchestrator-managed, read/changed over the Wyoming control protocol).
//  * Device Config (Wake word & Display) — wake word, detection thresholds +
//    capture tuning, and the idle-dim delay (device-local; restarts the engine).
//  * Speech Processing — playback buffer and the local end-of-speech "processing"
//    cue (device-local).
//  * Speech Detection — the Anamanti Core's end-of-speech VAD: engine (energy /
//    Silero), silence window, voice-level threshold, Silero probability threshold
//    (orchestrator-managed).
//  * Background — idle-screen photo source (local gradients or a linked Google
//    folder, on-device OAuth seam).
//
// Device-local settings are persisted with [SettingsStore]; remote settings are
// applied on the Mac. Both happen when the user taps Save (available from every
// page); the parent is notified via [onApplied] so it can restart the engine and
// refresh the slideshow.

import 'package:flutter/material.dart';

import 'package:qr_flutter/qr_flutter.dart';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/engine/update_controller.dart';
import 'package:anamanti_display/src/settings/app_settings.dart';
import 'package:anamanti_display/src/settings/orchestrator_client.dart';
import 'package:anamanti_display/src/settings/settings_store.dart';
import 'package:anamanti_display/src/slideshow/ambient_photos.dart';
import 'package:anamanti_display/src/slideshow/drive_photos.dart';
import 'package:anamanti_display/src/ui/audio_diagnostics_view.dart';
import 'package:anamanti_display/src/rust/api/engine.dart'
    show updateDiagnosticsTuning;

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

/// The fixed set of "dim the screen after" durations (seconds) offered by the
/// Display picker: 30s, 1m, 2m, 5m, 10m, 15m, 30m, 1h. The slider snaps between
/// these presets rather than sweeping a continuous range. Kept in ascending order.
const List<int> _kDimDelayPresets = <int>[30, 60, 120, 300, 600, 900, 1800, 3600];

/// The top-level settings categories, shown as a menu; selecting one opens its
/// page. Order matches the menu order.
enum _SettingsCategory {
  assistant('Assistant'),
  deviceConfig('Device Config'),
  audioDiagnostics('Audio Diagnostics'),
  speechProcessing('Speech Processing'),
  speechDetection('Speech Detection'),
  background('Background'),
  updates('Updates');

  const _SettingsCategory(this.title);

  /// The page's AppBar title.
  final String title;
}

class SettingsScreen extends StatefulWidget {
  const SettingsScreen({
    super.key,
    required this.initial,
    required this.store,
    required this.client,
    required this.onApplied,
    this.assistant,
    this.updates,
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

  /// The live wake-word engine controller, for the Audio Diagnostics page's real-time
  /// meters. Null in contexts without a running engine (e.g. some tests); the page
  /// then shows an "engine unavailable" note.
  final AssistantController? assistant;

  /// The in-app updater controller (plans/UpdaterPlan.md). Null on the
  /// `fdroid` flavor / in tests, which hides the Updates category entirely.
  final UpdateController? updates;

  @override
  State<SettingsScreen> createState() => _SettingsScreenState();
}

class _SettingsScreenState extends State<SettingsScreen> {
  late AppSettings _settings = widget.initial;

  final TextEditingController _modelController = TextEditingController();
  final TextEditingController _voiceController = TextEditingController();
  final TextEditingController _folderController = TextEditingController();
  final TextEditingController _updateUrlController = TextEditingController();
  late final TextEditingController _deviceNameController = TextEditingController(
    text: widget.initial.deviceName,
  );

  String _backend = 'ollama';
  // Anthropic auth mode: 'apikey' or 'subscription' (Claude OAuth).
  String _anthropicAuth = 'apikey';
  bool _remoteLoading = true;
  String? _remoteError;
  bool _saving = false;

  /// The open category page, or null while the top-level menu is shown.
  _SettingsCategory? _category;

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
  // VAD engine (energy | silero) and the Silero speech-probability gate.
  String _vadEngine = 'energy';
  double _sileroThreshold = 0.5;

  @override
  void initState() {
    super.initState();
    _folderController.text = _settings.driveFolderIds.join(', ');
    _updateUrlController.text = _settings.updateBaseUrl;
    _loadRemote();
  }

  @override
  void dispose() {
    _modelController.dispose();
    _voiceController.dispose();
    _folderController.dispose();
    _updateUrlController.dispose();
    _deviceNameController.dispose();
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
        if (remote.vadEngine.isNotEmpty) _vadEngine = remote.vadEngine;
        if (remote.sileroThreshold > 0) _sileroThreshold = remote.sileroThreshold;
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

  /// Sync the Google **Drive** bundle (OAuth client creds + refresh token + folder
  /// ids) from the orchestrator, which owns consent. Link Drive once on the
  /// orchestrator's config page (Photos tab, `http://<mac>:8730/drive`); the device
  /// then pulls everything over Wyoming — no adb, no build-time credentials. This
  /// also runs automatically on boot / the periodic photo refresh.
  Future<void> _syncDriveFromOrchestrator() async {
    _snack('Syncing Drive from the orchestrator…');
    DriveTokenView token;
    try {
      token = await widget.client.fetchDriveToken();
    } catch (e) {
      if (!mounted) return;
      _snack('Could not reach the orchestrator: $e');
      return;
    }
    if (!mounted) return;
    if (!token.configured) {
      _snack(
        'The orchestrator has no Drive credentials yet. Link Drive on its config '
        'page (Photos tab), then Sync.',
      );
      return;
    }
    setState(() {
      if (token.folderIds.isNotEmpty) {
        _folderController.text = token.folderIds.join(', ');
      }
      _settings = _settings.copyWith(
        photoSource: PhotoSourceKind.drive,
        driveClientId: token.clientId,
        driveClientSecret: token.clientSecret,
        driveRefreshToken: token.refreshToken,
        driveFolderIds: token.folderIds,
        driveLinked: token.linked,
      );
    });
    _snack(
      token.linked
          ? 'Drive synced. Tap Save to apply.'
          : 'Drive credentials synced, but not linked yet — link Drive on the '
              'orchestrator config page, then Sync again.',
    );
  }

  /// Show a picker of the account's Drive folders (owned + shared) so the user can
  /// choose which to display without knowing folder IDs. Needs an imported Drive
  /// token (mints an access token from it) and the Desktop client creds.
  Future<void> _pickDriveFolders() async {
    if (!_settings.driveConfigured) {
      _snack('Drive not configured yet. Sync from the orchestrator first.');
      return;
    }
    if (_settings.driveRefreshToken.isEmpty) {
      _snack('Drive not linked yet. Link it on the orchestrator, then Sync.');
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
      clientId: _settings.driveClientId,
      clientSecret: _settings.driveClientSecret,
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
    final updateUrl = _updateUrlController.text.trim();
    final local = _settings.copyWith(
      driveFolderIds: _parseFolderIds(_folderController.text),
      updateBaseUrl: updateUrl.isEmpty ? _settings.updateBaseUrl : updateUrl,
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
          vadEngine: _vadEngine,
          sileroThreshold: _sileroThreshold,
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
    final category = _category;
    final atMenu = category == null;
    return PopScope(
      // At the menu, let the route pop (leave settings). On a category page,
      // intercept the pop and return to the menu instead.
      canPop: atMenu,
      onPopInvokedWithResult: (didPop, _) {
        if (!didPop && !atMenu) setState(() => _category = null);
      },
      child: Scaffold(
        appBar: AppBar(
          leading: atMenu
              ? null
              : IconButton(
                  key: const Key('settings-back-to-menu'),
                  icon: const Icon(Icons.arrow_back),
                  tooltip: 'Settings',
                  onPressed: () => setState(() => _category = null),
                ),
          title: Text(atMenu ? 'Settings' : category.title),
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
        body: atMenu ? _menu() : _categoryPage(category),
      ),
    );
  }

  /// The top-level category menu.
  Widget _menu() {
    return ListView(
      padding: const EdgeInsets.symmetric(vertical: 8),
      children: [
        _menuTile(
          _SettingsCategory.assistant,
          Icons.smart_toy_outlined,
          'LLM backend, model, and voice',
        ),
        _menuTile(
          _SettingsCategory.deviceConfig,
          Icons.tune,
          'Wake word & display',
        ),
        _menuTile(
          _SettingsCategory.audioDiagnostics,
          Icons.equalizer,
          'Live mic meter & wake-word tuning',
        ),
        _menuTile(
          _SettingsCategory.speechProcessing,
          Icons.graphic_eq,
          'Playback buffer & processing cue',
        ),
        _menuTile(
          _SettingsCategory.speechDetection,
          Icons.record_voice_over,
          'End-of-speech VAD & thresholds',
        ),
        _menuTile(
          _SettingsCategory.background,
          Icons.photo_library_outlined,
          'Idle photo slideshow',
        ),
        // Only on the selfUpdate flavor (the controller is null on fdroid).
        if (widget.updates != null)
          _menuTile(
            _SettingsCategory.updates,
            Icons.system_update_alt,
            'In-app app updates',
          ),
      ],
    );
  }

  Widget _menuTile(_SettingsCategory category, IconData icon, String subtitle) {
    return ListTile(
      key: Key('settings-menu-${category.name}'),
      leading: Icon(icon),
      title: Text(category.title),
      subtitle: Text(subtitle),
      trailing: const Icon(Icons.chevron_right),
      onTap: () => setState(() => _category = category),
    );
  }

  /// The body for a single category page.
  Widget _categoryPage(_SettingsCategory category) {
    // Audio Diagnostics is a full custom page (live meters), not a tile list.
    if (category == _SettingsCategory.audioDiagnostics) {
      return _audioDiagnosticsPage();
    }
    // Updates is a full custom page (live download/install state).
    if (category == _SettingsCategory.updates) {
      return _updatesPage();
    }
    final List<Widget> children;
    switch (category) {
      case _SettingsCategory.assistant:
        children = [
          // Device-local identity: this display's name (sent to the Core) + its
          // stable, MAC-derived id. Shown first so it's usable even when the
          // selected orchestrator is offline.
          _deviceNameTile(),
          // Device-local: which orchestrator this display talks to. Shown above
          // (and outside) the orchestrator-fetched tiles so it stays usable even
          // when the selected orchestrator is offline.
          _orchestratorTile(),
          ..._assistantTiles(),
        ];
      case _SettingsCategory.deviceConfig:
        children = [
          _section('Wake word'),
          _wakeWordTile(),
          _thresholdTile(),
          ..._detectionTuningTiles(),
          const Divider(),
          _section('Display'),
          ..._displayTiles(),
        ];
      case _SettingsCategory.audioDiagnostics:
        // Handled by the early return above; keep the switch exhaustive.
        children = const [];
      case _SettingsCategory.speechProcessing:
        children = _speechTiles();
      case _SettingsCategory.speechDetection:
        children = _vadTiles();
      case _SettingsCategory.background:
        children = _photoTiles();
      case _SettingsCategory.updates:
        // Handled by the early return above; keep the switch exhaustive.
        children = const [];
    }
    return ListView(
      padding: const EdgeInsets.symmetric(vertical: 8),
      children: [...children, const SizedBox(height: 24)],
    );
  }

  /// The Audio Diagnostics page: live mic + wake-word meters with live-applied
  /// tuning. Needs the running engine controller; without one (e.g. the Mac-less
  /// test harness) it shows a short unavailable note.
  Widget _audioDiagnosticsPage() {
    final assistant = widget.assistant;
    if (assistant == null) {
      return const Center(
        child: Padding(
          padding: EdgeInsets.all(24),
          child: Text(
            'Microphone engine is not running yet — open this page once the '
            'display has started listening.',
            textAlign: TextAlign.center,
          ),
        ),
      );
    }
    return AudioDiagnosticsView(
      key: const Key('audio-diagnostics-view'),
      controller: assistant,
      settings: _settings,
      onChanged: (next) => setState(() => _settings = next),
      onTune: (gain, threshold) =>
          updateDiagnosticsTuning(gainDb: gain, threshold: threshold),
    );
  }

  /// The Updates page (plans/UpdaterPlan.md): the auto-update toggle +
  /// base URL (saved with the device-local settings on Save), the current version,
  /// and a live "Check now / Update / Install" area driven by the updater controller.
  Widget _updatesPage() {
    final updates = widget.updates;
    if (updates == null) {
      return const Center(
        child: Padding(
          padding: EdgeInsets.all(24),
          child: Text(
            'The in-app updater is not available in this build.',
            textAlign: TextAlign.center,
          ),
        ),
      );
    }
    return ListView(
      padding: const EdgeInsets.symmetric(vertical: 8),
      children: [
        SwitchListTile(
          key: const Key('settings-auto-update'),
          secondary: const Icon(Icons.autorenew),
          title: const Text('Automatic update checks'),
          subtitle: const Text('Check for new versions on launch and periodically'),
          value: _settings.autoUpdateEnabled,
          onChanged: (v) =>
              setState(() => _settings = _settings.copyWith(autoUpdateEnabled: v)),
        ),
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 8, 16, 8),
          child: TextField(
            key: const Key('settings-update-url'),
            controller: _updateUrlController,
            keyboardType: TextInputType.url,
            autocorrect: false,
            decoration: const InputDecoration(
              labelText: 'Update URL',
              helperText: 'Base URL hosting latest.json + the APK (no trailing slash). '
                  'Save to apply before checking.',
              border: OutlineInputBorder(),
            ),
          ),
        ),
        const Divider(),
        AnimatedBuilder(
          animation: updates,
          builder: (context, _) => _updateStatusTile(updates),
        ),
        const SizedBox(height: 24),
      ],
    );
  }

  Widget _updateStatusTile(UpdateController updates) {
    final manifest = updates.manifest;
    final busy = updates.status == UpdateStatus.checking ||
        updates.status == UpdateStatus.downloading ||
        updates.status == UpdateStatus.installing;

    final String statusLine = switch (updates.status) {
      UpdateStatus.idle => 'Not checked yet.',
      UpdateStatus.checking => 'Checking…',
      UpdateStatus.upToDate => 'You’re up to date.',
      UpdateStatus.available => manifest == null
          ? 'An update is available.'
          : 'Version ${manifest.versionName.isNotEmpty ? manifest.versionName : manifest.versionCode} is available.',
      UpdateStatus.downloading => updates.progress == null
          ? 'Downloading…'
          : 'Downloading… ${(updates.progress! * 100).round()}%',
      UpdateStatus.readyToInstall => 'Downloaded — ready to install.',
      UpdateStatus.installing => 'Installing…',
      UpdateStatus.error => updates.errorMessage,
    };

    final (String? actionLabel, VoidCallback? action) = switch (updates.status) {
      UpdateStatus.available => ('Download & install', updates.download),
      UpdateStatus.readyToInstall => ('Install', updates.install),
      UpdateStatus.error => ('Retry', updates.retry),
      _ => (null, null),
    };

    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        ListTile(
          leading: const Icon(Icons.info_outline),
          title: Text('Installed version code: ${updates.currentVersionCode}'),
          subtitle: Text(statusLine),
        ),
        if (updates.status == UpdateStatus.available &&
            manifest != null &&
            manifest.notes.isNotEmpty)
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
            child: Text(manifest.notes),
          ),
        if (updates.status == UpdateStatus.downloading)
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
            child: LinearProgressIndicator(value: updates.progress),
          ),
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
          child: Row(
            children: [
              OutlinedButton.icon(
                key: const Key('settings-check-update'),
                onPressed: busy ? null : updates.checkNow,
                icon: const Icon(Icons.refresh),
                label: const Text('Check now'),
              ),
              const SizedBox(width: 12),
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

  /// Device-local editor for this display's friendly name, sent to the Core in the
  /// `anamanti-hello` frame so two displays on one Core are distinguishable (and shown
  /// in the Core config page's "Connected devices" list). The subtitle shows the stable
  /// MAC-derived [AppSettings.deviceId]. Persisted via [SettingsStore]; a change
  /// restarts the engine so the channels re-announce (see `main.dart`).
  Widget _deviceNameTile() {
    return ListTile(
      leading: const Icon(Icons.devices_other),
      title: const Text('Device name'),
      subtitle: Text(
        _settings.deviceId.isEmpty
            ? 'A name for this display, shown on the Core'
            : 'ID: ${_settings.deviceId}',
      ),
      trailing: SizedBox(
        width: 160,
        child: TextField(
          key: const Key('settings-device-name'),
          controller: _deviceNameController,
          textAlign: TextAlign.end,
          textCapitalization: TextCapitalization.words,
          decoration: const InputDecoration(hintText: 'e.g. Kitchen'),
          onChanged: (v) =>
              setState(() => _settings = _settings.copyWith(deviceName: v)),
        ),
      ),
    );
  }

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
      _rangeSlider(
        label: 'Capture gain (dB)',
        value: _settings.captureGainDb,
        min: 0,
        max: 36,
        divisions: 36,
        format: (v) => '${v.round()} dB',
        sliderKey: const Key('settings-capture-gain'),
        onChanged: (v) => setState(
          () => _settings = _settings.copyWith(captureGainDb: v.roundToDouble()),
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
      if (_settings.useAudioRecord) ...[
        SwitchListTile(
          key: const Key('settings-platform-ns'),
          secondary: const Icon(Icons.noise_control_off),
          title: const Text('Noise suppression'),
          subtitle: const Text(
            'Platform NoiseSuppressor. On by default — but if the wake word is '
            'missed in a noisy room, try turning this OFF (aggressive NS can '
            'distort speech and hurt detection)',
          ),
          value: _settings.platformNs,
          onChanged: (v) =>
              setState(() => _settings = _settings.copyWith(platformNs: v)),
        ),
        SwitchListTile(
          key: const Key('settings-platform-agc'),
          secondary: const Icon(Icons.graphic_eq),
          title: const Text('Automatic gain control'),
          subtitle: const Text(
            'Platform AutomaticGainControl — boosts quiet far-field speech',
          ),
          value: _settings.platformAgc,
          onChanged: (v) =>
              setState(() => _settings = _settings.copyWith(platformAgc: v)),
        ),
        SwitchListTile(
          key: const Key('settings-platform-aec'),
          secondary: const Icon(Icons.hearing),
          title: const Text('Echo cancellation'),
          subtitle: const Text(
            'Platform AcousticEchoCanceler. Off by default — the Mac does AEC and '
            'this device\'s platform AEC was found not to actually cancel',
          ),
          value: _settings.platformAec,
          onChanged: (v) =>
              setState(() => _settings = _settings.copyWith(platformAec: v)),
        ),
      ],
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
      // Listening ring: the glowing blue overlay shown while listening, which reacts
      // to your voice. A master on/off plus its reactivity/attack/release/auto-range
      // dials. Purely presentational — applied instantly on Save, no engine restart.
      SwitchListTile(
        key: const Key('settings-listening-ring'),
        secondary: const Icon(Icons.blur_circular),
        title: const Text('Listening ring'),
        subtitle: const Text(
          'Show a glowing ring while listening that reacts to your voice',
        ),
        value: _settings.listeningRingEnabled,
        onChanged: (v) => setState(
          () => _settings = _settings.copyWith(listeningRingEnabled: v),
        ),
      ),
      if (_settings.listeningRingEnabled) ...[
        _rangeSlider(
          label: 'Ring reactivity',
          value: _settings.ringReactivity,
          min: 0.25,
          max: 2.5,
          divisions: 45,
          format: (v) => '${v.toStringAsFixed(2)}×',
          sliderKey: const Key('settings-ring-reactivity'),
          onChanged: (v) =>
              setState(() => _settings = _settings.copyWith(ringReactivity: v)),
        ),
        _rangeSlider(
          label: 'Ring attack',
          value: _settings.ringAttack,
          min: 0.1,
          max: 1.0,
          divisions: 18,
          format: (v) => v.toStringAsFixed(2),
          sliderKey: const Key('settings-ring-attack'),
          onChanged: (v) =>
              setState(() => _settings = _settings.copyWith(ringAttack: v)),
        ),
        _rangeSlider(
          label: 'Ring release',
          value: _settings.ringRelease,
          min: 0.02,
          max: 0.5,
          divisions: 48,
          format: (v) => v.toStringAsFixed(2),
          sliderKey: const Key('settings-ring-release'),
          onChanged: (v) =>
              setState(() => _settings = _settings.copyWith(ringRelease: v)),
        ),
        _rangeSlider(
          label: 'Ring auto-range',
          value: _settings.ringDecay,
          min: 0.90,
          max: 0.999,
          divisions: 99,
          format: (v) => v.toStringAsFixed(3),
          sliderKey: const Key('settings-ring-decay'),
          onChanged: (v) =>
              setState(() => _settings = _settings.copyWith(ringDecay: v)),
        ),
      ],
    ];
  }

  /// Placeholder tiles shown for the orchestrator-managed sections (Assistant,
  /// Speech Detection) while the Mac is being contacted or is unreachable. Returns
  /// null once the remote settings have loaded, so the caller renders its controls.
  List<Widget>? _remoteGuardTiles() {
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
            'These settings need the Mac to be reachable.',
          ),
          trailing: TextButton(
            onPressed: _loadRemote,
            child: const Text('Retry'),
          ),
        ),
      ];
    }
    return null;
  }

  List<Widget> _assistantTiles() {
    final guard = _remoteGuardTiles();
    if (guard != null) return guard;
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
    ];
  }

  /// Speech Detection: the Anamanti Core's end-of-speech VAD tuning, applied on the
  /// Mac (orchestrator-managed). Shortening the silence window cuts the wait before
  /// the reply; lowering the level helps a quiet far-field mic register as speech
  /// instead of hitting the slow timeout. Also selects the VAD engine (energy vs
  /// Silero) and, for Silero, its speech-probability threshold.
  List<Widget> _vadTiles() {
    final guard = _remoteGuardTiles();
    if (guard != null) return guard;
    return [
      // VAD engine selection (applied on the Mac; the energy⇄silero swap takes effect
      // without a restart). Silero is a neural detector — more robust to noise, but it
      // needs a Core built with the `vad-silero` feature + a model, else it falls back
      // to energy. Its probability threshold is shown only when Silero is selected.
      ListTile(
        leading: const Icon(Icons.graphic_eq),
        title: const Text('VAD engine'),
        subtitle: const Text('Energy (RMS) or Silero (neural)'),
        trailing: DropdownButton<String>(
          key: const Key('settings-vad-engine'),
          value: _vadEngine == 'silero' ? 'silero' : 'energy',
          items: const [
            DropdownMenuItem(value: 'energy', child: Text('Energy')),
            DropdownMenuItem(value: 'silero', child: Text('Silero')),
          ],
          onChanged: (v) {
            if (v == null || v == _vadEngine) return;
            setState(() => _vadEngine = v);
          },
        ),
      ),
      if (_vadEngine == 'silero')
        _rangeSlider(
          label: 'Silero speech threshold',
          value: _sileroThreshold,
          min: 0.0,
          max: 1.0,
          divisions: 20,
          format: (v) => v.toStringAsFixed(2),
          sliderKey: const Key('settings-vad-silero-threshold'),
          onChanged: (v) => setState(() => _sileroThreshold = v),
        ),
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

  /// Idle-screen presentation: how long the display stays fully bright after the
  /// room goes quiet before dimming to the calm away-mode clock face. Maps to the
  /// camera proximity release window (device-local; restarts the engine on Save).
  List<Widget> _displayTiles() {
    // The slider snaps across the fixed presets: map the stored seconds to the
    // nearest preset index so an out-of-band persisted value still lands on a
    // valid stop, and store back the exact preset the user lands on.
    final index = _nearestDimPresetIndex(_settings.dimDelaySecs);
    return [
      const Padding(
        padding: EdgeInsets.fromLTRB(16, 0, 16, 8),
        child: Text(
          'How long the screen stays bright after the room goes quiet before dimming '
          'to the clock. Approaching the display brightens it again instantly.',
          style: TextStyle(fontSize: 12, color: Colors.grey),
        ),
      ),
      _rangeSlider(
        label: 'Dim screen after',
        value: index.toDouble(),
        min: 0,
        max: (_kDimDelayPresets.length - 1).toDouble(),
        divisions: _kDimDelayPresets.length - 1,
        format: (v) => _fmtDuration(
          _kDimDelayPresets[v.round().clamp(0, _kDimDelayPresets.length - 1)]
              .toDouble(),
        ),
        sliderKey: const Key('settings-dim-delay'),
        onChanged: (v) => setState(
          () => _settings = _settings.copyWith(
            dimDelaySecs: _kDimDelayPresets[v
                .round()
                .clamp(0, _kDimDelayPresets.length - 1)],
          ),
        ),
      ),
      const Divider(),
      const Padding(
        padding: EdgeInsets.fromLTRB(16, 0, 16, 8),
        child: Text(
          'Size of all on-screen text. A manual override of the voice commands '
          '"increase font" / "decrease font"; applies on Save.',
          style: TextStyle(fontSize: 12, color: Colors.grey),
        ),
      ),
      _rangeSlider(
        label: 'Font size',
        value: _settings.fontScale,
        min: kFontScaleMin,
        max: kFontScaleMax,
        // Snap to 5% stops across the [0.85, 1.6] range (finer than the 0.1 voice step).
        divisions: ((kFontScaleMax - kFontScaleMin) / 0.05).round(),
        format: (v) => '${(v * 100).round()}%',
        sliderKey: const Key('settings-font-scale'),
        onChanged: (v) =>
            setState(() => _settings = _settings.copyWith(fontScale: v)),
      ),
    ];
  }

  /// The index of the dim-delay preset closest to [seconds], so a persisted value
  /// that predates the preset list (or an out-of-range one) still shows a valid stop.
  int _nearestDimPresetIndex(int seconds) {
    var best = 0;
    var bestDelta = (_kDimDelayPresets[0] - seconds).abs();
    for (var i = 1; i < _kDimDelayPresets.length; i++) {
      final delta = (_kDimDelayPresets[i] - seconds).abs();
      if (delta < bestDelta) {
        bestDelta = delta;
        best = i;
      }
    }
    return best;
  }

  /// Human-readable seconds → "45s" / "2m" / "1h" for the dim-delay picker.
  String _fmtDuration(double seconds) {
    final s = seconds.round();
    if (s < 60) return '${s}s';
    if (s % 3600 == 0) return '${s ~/ 3600}h';
    final m = s ~/ 60;
    final rem = s % 60;
    return rem == 0 ? '${m}m' : '${m}m ${rem}s';
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
            '1. Link Drive once on the orchestrator config page (Photos tab).  '
            '2. Sync from the orchestrator here.  3. Choose folders (or paste IDs). '
            'Syncing also happens automatically on boot.',
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
                key: const Key('settings-drive-sync'),
                onPressed: _syncDriveFromOrchestrator,
                icon: const Icon(Icons.sync),
                label: Text(
                  _settings.driveLinked
                      ? 'Re-sync Drive from Mac'
                      : 'Sync Drive from Mac',
                ),
              ),
              FilledButton.icon(
                key: const Key('settings-drive-pick'),
                onPressed:
                    _settings.driveConfigured && _settings.driveRefreshToken.isNotEmpty
                        ? _pickDriveFolders
                        : null,
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
