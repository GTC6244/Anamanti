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

import 'package:ambient_display/src/settings/app_settings.dart';
import 'package:ambient_display/src/settings/orchestrator_client.dart';
import 'package:ambient_display/src/settings/settings_store.dart';
import 'package:ambient_display/src/slideshow/photo_source.dart';
import 'package:ambient_display/src/ui/memory_screen.dart';
import 'package:ambient_display/src/ui/people_screen.dart';

/// LLM backends the settings screen can select. Labels are user-facing; the value
/// is the orchestrator's backend label.
const Map<String, String> _kBackends = {
  'ollama': 'Local (Ollama)',
  'anthropic': 'Cloud (Claude)',
  'mock': 'Offline echo (mock)',
};

class SettingsScreen extends StatefulWidget {
  const SettingsScreen({
    super.key,
    required this.initial,
    required this.store,
    required this.client,
    required this.onApplied,
    this.authenticator = const StubGoogleAuthenticator(),
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

  /// The on-device Google consent seam (Phase 6).
  final GoogleAuthenticator authenticator;

  @override
  State<SettingsScreen> createState() => _SettingsScreenState();
}

class _SettingsScreenState extends State<SettingsScreen> {
  late AppSettings _settings = widget.initial;

  final TextEditingController _modelController = TextEditingController();
  final TextEditingController _voiceController = TextEditingController();
  final TextEditingController _folderController = TextEditingController();

  String _backend = 'ollama';
  bool _remoteLoading = true;
  String? _remoteError;
  bool _saving = false;

  // Orchestrator-side VAD tuning (loaded from the Mac, applied on Save).
  int _endSilenceMs = 700;
  double _voiceRmsThreshold = 120;

  @override
  void initState() {
    super.initState();
    _folderController.text = _settings.googleFolderName;
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
    try {
      final remote = await widget.client.fetchSettings();
      if (!mounted) return;
      setState(() {
        _backend = _kBackends.containsKey(remote.llmBackend) ? remote.llmBackend : 'ollama';
        _modelController.text = remote.llmModel ?? '';
        _voiceController.text = remote.ttsVoice ?? '';
        // Adopt the orchestrator's live VAD values (0 = unknown → keep the default).
        if (remote.endSilenceMs > 0) _endSilenceMs = remote.endSilenceMs;
        if (remote.voiceRmsThreshold > 0) _voiceRmsThreshold = remote.voiceRmsThreshold;
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

  Future<void> _linkGoogle() async {
    try {
      final result = await widget.authenticator.link();
      if (!mounted) return;
      setState(() {
        _folderController.text = result.folderName;
        _settings = _settings.copyWith(
          photoSource: PhotoSourceKind.google,
          googleFolderName: result.folderName,
          googleLinked: true,
        );
      });
      _snack('Linked Google folder "${result.folderName}"');
    } catch (e) {
      if (!mounted) return;
      _snack('Google linking unavailable: $e');
    }
  }

  Future<void> _save() async {
    setState(() => _saving = true);

    // 1. Persist + apply the device-local settings (wake word, thresholds, photo).
    final local = _settings.copyWith(googleFolderName: _folderController.text.trim());
    await widget.store.save(local);
    widget.onApplied(local);

    // 2. Apply the orchestrator-managed settings, if the Mac was reachable.
    String? remoteNote;
    if (_remoteError == null) {
      try {
        final result = await widget.client.applySettings(
          llmBackend: _backend,
          llmModel: _modelController.text.trim().isEmpty ? null : _modelController.text.trim(),
          setTtsVoice: true,
          ttsVoice: _voiceController.text.trim().isEmpty ? null : _voiceController.text.trim(),
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
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(message)));
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
            subtitle: const Text('View and delete what the assistant remembers'),
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
          if (v != null) setState(() => _settings = _settings.copyWith(wakeWord: v));
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
          onChanged: (v) => setState(() => _settings = _settings.copyWith(threshold: v)),
        ),
        _slider(
          label: 'Sensitivity while speaking',
          value: _settings.activeThreshold,
          onChanged: (v) =>
              setState(() => _settings = _settings.copyWith(activeThreshold: v)),
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
        onChanged: (v) =>
            setState(() => _settings = _settings.copyWith(smoothingWindow: v.round())),
      ),
      SwitchListTile(
        key: const Key('settings-fire-on-peak'),
        secondary: const Icon(Icons.bolt),
        title: const Text('Fire on peak'),
        subtitle: const Text(
            'Trigger on the strongest frame, not the average — snappier for quiet wake words'),
        value: _settings.fireOnPeak,
        onChanged: (v) => setState(() => _settings = _settings.copyWith(fireOnPeak: v)),
      ),
      SwitchListTile(
        key: const Key('settings-use-audiorecord'),
        secondary: const Icon(Icons.settings_voice),
        title: const Text('AudioRecord capture (far-field)'),
        subtitle: const Text(
            'Android: capture via VOICE_RECOGNITION + platform noise-suppression/AGC instead of cpal'),
        value: _settings.useAudioRecord,
        onChanged: (v) => setState(() => _settings = _settings.copyWith(useAudioRecord: v)),
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
        onChanged: (v) =>
            setState(() => _settings = _settings.copyWith(playbackBufferSecs: v.round())),
      ),
      SwitchListTile(
        key: const Key('settings-endpoint-cue'),
        secondary: const Icon(Icons.hourglass_top),
        title: const Text('Instant "processing" cue'),
        subtitle: const Text(
            'Show a processing indicator the moment you stop speaking, before the reply'),
        value: _settings.endpointCueEnabled,
        onChanged: (v) =>
            setState(() => _settings = _settings.copyWith(endpointCueEnabled: v)),
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
          onChanged: (v) =>
              setState(() => _settings = _settings.copyWith(endpointSilenceMs: v.round())),
        ),
        _rangeSlider(
          label: 'Silence level',
          value: _settings.endpointRmsThreshold,
          min: 0.002,
          max: 0.05,
          divisions: 48,
          format: (v) => v.toStringAsFixed(3),
          sliderKey: const Key('settings-endpoint-level'),
          onChanged: (v) =>
              setState(() => _settings = _settings.copyWith(endpointRmsThreshold: v)),
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
          subtitle: const Text('LLM and voice settings need the Mac to be reachable.'),
          trailing: TextButton(onPressed: _loadRemote, child: const Text('Retry')),
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
            if (v != null) setState(() => _backend = v);
          },
        ),
      ),
      Padding(
        padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
        child: TextField(
          key: const Key('settings-model'),
          controller: _modelController,
          decoration: const InputDecoration(
            labelText: 'Model',
            hintText: 'e.g. llama3.2 or claude-opus-5',
          ),
        ),
      ),
      Padding(
        padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
        child: TextField(
          key: const Key('settings-voice'),
          controller: _voiceController,
          decoration: const InputDecoration(
            labelText: 'TTS voice',
            hintText: 'Piper voice, e.g. en_US-amy-medium (blank = default)',
          ),
        ),
      ),
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

  List<Widget> _photoTiles() {
    return [
      RadioGroup<PhotoSourceKind>(
        groupValue: _settings.photoSource,
        onChanged: (v) => setState(
          () => _settings = _settings.copyWith(photoSource: v ?? PhotoSourceKind.local),
        ),
        child: Column(
          children: [
            const RadioListTile<PhotoSourceKind>(
              key: Key('settings-photos-local'),
              value: PhotoSourceKind.local,
              title: Text('Local ambient gradients'),
            ),
            RadioListTile<PhotoSourceKind>(
              key: const Key('settings-photos-google'),
              value: PhotoSourceKind.google,
              title: const Text('Google Photos / Drive folder'),
              subtitle: Text(_settings.googleLinked
                  ? 'Linked: ${_settings.googleFolderName}'
                  : 'Not linked'),
            ),
          ],
        ),
      ),
      if (_settings.photoSource == PhotoSourceKind.google) ...[
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
          child: TextField(
            controller: _folderController,
            decoration: const InputDecoration(
              labelText: 'Folder / album name',
            ),
          ),
        ),
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
          child: Align(
            alignment: Alignment.centerLeft,
            child: FilledButton.tonalIcon(
              key: const Key('settings-google-link'),
              onPressed: _linkGoogle,
              icon: const Icon(Icons.link),
              label: Text(_settings.googleLinked ? 'Re-link Google account' : 'Link Google account'),
            ),
          ),
        ),
      ],
    ];
  }
}
