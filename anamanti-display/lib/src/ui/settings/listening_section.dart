// Settings → Listening: how the display hears you. Wake word + sensitivity, the
// microphone capture path (Advanced mode), and end of speech — the Core's VAD (which
// really decides when you're done) side by side with the on-screen "processing" cue
// (which only changes when the indicator appears).

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/settings/app_settings.dart';
import 'package:anamanti_display/src/ui/settings/settings_controller.dart';
import 'package:anamanti_display/src/ui/settings/settings_widgets.dart';

/// Listening → Wake word.
class ListeningWakeWordTab extends StatelessWidget {
  const ListeningWakeWordTab({
    super.key,
    required this.controller,
    required this.onOpenDiagnostics,
  });

  final SettingsController controller;
  final VoidCallback onOpenDiagnostics;

  @override
  Widget build(BuildContext context) {
    final c = controller;
    final s = c.settings;
    // The current wake word may not be in the built-in list (dropped in manually);
    // include it so the dropdown always has a valid selection.
    final words = {...kAvailableWakeWords, s.wakeWord}.toList();
    String fmt2(double v) => v.toStringAsFixed(2);
    return SettingsTabBody(
      left: [
        const SettingsHeader('Wake word'),
        SettingsRow(
          title: 'Wake word',
          icon: Icons.record_voice_over,
          trailing: DropdownButton<String>(
            key: const Key('settings-wakeword'),
            value: s.wakeWord,
            items: [
              for (final w in words)
                DropdownMenuItem(value: w, child: Text(w.replaceAll('_', ' '))),
            ],
            onChanged: (v) {
              if (v != null) c.updateLocal(s.copyWith(wakeWord: v));
            },
          ),
        ),
        SettingsSlider(
          label: 'Sensitivity (idle)',
          sliderKey: const Key('settings-threshold'),
          value: s.threshold,
          min: 0.1,
          max: 0.95,
          divisions: 17,
          format: fmt2,
          onChanged: (v) =>
              c.updateLocal(c.settings.copyWith(threshold: v), commit: false),
          onChangeEnd: (_) => c.commitLocalSoon(),
        ),
        SettingsSlider(
          label: 'While speaking',
          sliderKey: const Key('settings-active-threshold'),
          value: s.activeThreshold,
          min: 0.1,
          max: 0.95,
          divisions: 17,
          format: fmt2,
          onChanged: (v) => c.updateLocal(
            c.settings.copyWith(activeThreshold: v),
            commit: false,
          ),
          onChangeEnd: (_) => c.commitLocalSoon(),
        ),
        const Padding(
          padding: EdgeInsets.fromLTRB(12, 0, 12, 4),
          child: Text(
            'Higher = needs a clearer “wake word” (fewer false triggers). “While '
            'speaking” applies during a reply, so the assistant doesn’t wake itself.',
            style: TextStyle(fontSize: 12, color: Colors.grey),
          ),
        ),
      ],
      right: [
        const SettingsHeader('Microphone level'),
        SettingsSlider(
          label: 'Capture gain',
          sliderKey: const Key('settings-capture-gain'),
          value: s.captureGainDb,
          min: 0,
          max: 36,
          divisions: 36,
          format: (v) => '${v.round()} dB',
          onChanged: (v) => c.updateLocal(
            c.settings.copyWith(captureGainDb: v.roundToDouble()),
            commit: false,
          ),
          onChangeEnd: (_) => c.commitLocalSoon(),
        ),
        if (s.advancedMode) ...[
          const SettingsHeader('Detection tuning'),
          SettingsSlider(
            label: 'Smoothing window',
            sliderKey: const Key('settings-smoothing'),
            value: s.smoothingWindow.toDouble(),
            min: 1,
            max: 6,
            divisions: 5,
            format: (v) => v.round().toString(),
            onChanged: (v) => c.updateLocal(
              c.settings.copyWith(smoothingWindow: v.round()),
              commit: false,
            ),
            onChangeEnd: (_) => c.commitLocalSoon(),
          ),
          SettingsSwitch(
            key: const Key('settings-fire-on-peak'),
            icon: Icons.bolt,
            title: 'Fire on peak',
            info:
                'Trigger on the strongest frame in the smoothing window instead '
                'of the average. Snappier for short or quiet wake words, at a '
                'slightly higher false-trigger rate.',
            value: s.fireOnPeak,
            onChanged: (v) => c.updateLocal(s.copyWith(fireOnPeak: v)),
          ),
        ],
        Padding(
          padding: const EdgeInsets.fromLTRB(12, 8, 12, 0),
          child: Align(
            alignment: Alignment.centerLeft,
            child: OutlinedButton.icon(
              key: const Key('settings-open-diagnostics'),
              onPressed: onOpenDiagnostics,
              icon: const Icon(Icons.equalizer),
              label: const Text('Test it live in Audio Diagnostics'),
            ),
          ),
        ),
      ],
    );
  }
}

/// Listening → Microphone (Advanced mode only): the Android capture path.
class ListeningMicrophoneTab extends StatelessWidget {
  const ListeningMicrophoneTab({super.key, required this.controller});

  final SettingsController controller;

  @override
  Widget build(BuildContext context) {
    final c = controller;
    final s = c.settings;
    final ar = s.useAudioRecord;
    return SettingsTabBody(
      left: [
        const SettingsHeader('Capture'),
        SettingsSwitch(
          key: const Key('settings-use-audiorecord'),
          icon: Icons.settings_voice,
          title: 'AudioRecord capture (far-field)',
          info:
              'Android: capture through AudioRecord with the VOICE_RECOGNITION '
              'source and the platform noise suppression / gain control below, '
              'instead of the default cpal path. Restarts the listener.',
          value: ar,
          onChanged: (v) => c.updateLocal(s.copyWith(useAudioRecord: v)),
        ),
        const SettingsHeader('Playback'),
        SettingsSlider(
          label: 'Playback buffer',
          sliderKey: const Key('settings-playback-buffer'),
          value: s.playbackBufferSecs.toDouble(),
          min: 2,
          max: 60,
          divisions: 58,
          format: (v) => '${v.round()}s',
          onChanged: (v) => c.updateLocal(
            c.settings.copyWith(playbackBufferSecs: v.round()),
            commit: false,
          ),
          onChangeEnd: (_) => c.commitLocalSoon(),
        ),
        const Padding(
          padding: EdgeInsets.fromLTRB(12, 0, 12, 4),
          child: Text(
            'Must be longer than the longest spoken reply, or long answers are cut '
            'off.',
            style: TextStyle(fontSize: 12, color: Colors.grey),
          ),
        ),
      ],
      right: [
        SettingsHeader(
          'Platform audio effects',
          caption: ar ? null : 'Only used with AudioRecord capture.',
        ),
        SettingsSwitch(
          key: const Key('settings-platform-ns'),
          icon: Icons.noise_control_off,
          title: 'Noise suppression',
          info:
              'The platform NoiseSuppressor. On by default, but if the wake word '
              'is missed in a noisy room try turning it off: aggressive noise '
              'suppression can distort speech and hurt detection.',
          enabled: ar,
          value: s.platformNs,
          onChanged: (v) => c.updateLocal(s.copyWith(platformNs: v)),
        ),
        SettingsSwitch(
          key: const Key('settings-platform-agc'),
          icon: Icons.graphic_eq,
          title: 'Automatic gain control',
          info:
              'The platform AutomaticGainControl. Boosts quiet far-field speech.',
          enabled: ar,
          value: s.platformAgc,
          onChanged: (v) => c.updateLocal(s.copyWith(platformAgc: v)),
        ),
        SettingsSwitch(
          key: const Key('settings-platform-aec'),
          icon: Icons.hearing,
          title: 'Echo cancellation',
          info:
              'The platform AcousticEchoCanceler. Off by default: echo is '
              'cancelled by the device’s audio-HAL shim (libamznaec_shim.so), and '
              'this platform effect was found not to cancel on the Echo Show.',
          enabled: ar,
          value: s.platformAec,
          onChanged: (v) => c.updateLocal(s.copyWith(platformAec: v)),
        ),
      ],
    );
  }
}

/// Listening → End of speech: the Core's VAD (left) and the local on-screen cue
/// (right), kept side by side and labelled so they aren't confused.
class ListeningEndOfSpeechTab extends StatelessWidget {
  const ListeningEndOfSpeechTab({super.key, required this.controller});

  final SettingsController controller;

  @override
  Widget build(BuildContext context) {
    final c = controller;
    final s = c.settings;
    final r = c.remote;
    final editable = c.remoteEditable;
    final advanced = s.advancedMode;
    return SettingsTabBody(
      header: RemoteStatusBanner(
        loading: c.remoteLoading,
        error: c.remoteError,
        onRetry: c.loadRemote,
      ),
      left: [
        const SettingsHeader(
          'Assistant (Core)',
          caption: 'Decides when you’ve finished talking.',
        ),
        SettingsSlider(
          label: 'End-of-speech wait',
          sliderKey: const Key('settings-vad-silence'),
          enabled: editable,
          value: r.endSilenceMs.toDouble(),
          min: 300,
          max: 1500,
          divisions: 24,
          format: (v) => '${v.round()} ms',
          onChanged: (v) => c.updateRemote(
            c.remote.copyWith(endSilenceMs: v.round()),
            commit: false,
          ),
          onChangeEnd: (_) => c.commitRemoteSoon(),
        ),
        if (advanced) ...[
          SettingsRow(
            title: 'VAD engine',
            trailing: SegmentedButton<String>(
              key: const Key('settings-vad-engine'),
              showSelectedIcon: false,
              segments: const [
                ButtonSegment(value: 'silero', label: Text('Silero')),
                ButtonSegment(value: 'energy', label: Text('Energy')),
              ],
              selected: {r.vadEngine == 'energy' ? 'energy' : 'silero'},
              onSelectionChanged: editable
                  ? (sel) => c.updateRemote(r.copyWith(vadEngine: sel.first))
                  : null,
            ),
          ),
          if (r.vadEngine != 'energy')
            SettingsSlider(
              label: 'Silero threshold',
              sliderKey: const Key('settings-vad-silero-threshold'),
              enabled: editable,
              value: r.sileroThreshold,
              min: 0.0,
              max: 1.0,
              divisions: 20,
              format: (v) => v.toStringAsFixed(2),
              onChanged: (v) => c.updateRemote(
                c.remote.copyWith(sileroThreshold: v),
                commit: false,
              ),
              onChangeEnd: (_) => c.commitRemoteSoon(),
            ),
          SettingsSlider(
            label: 'Voice level',
            sliderKey: const Key('settings-vad-level'),
            enabled: editable,
            value: r.voiceRmsThreshold,
            min: 20,
            max: 400,
            divisions: 38,
            format: (v) => '${v.round()}',
            onChanged: (v) => c.updateRemote(
              c.remote.copyWith(voiceRmsThreshold: v),
              commit: false,
            ),
            onChangeEnd: (_) => c.commitRemoteSoon(),
          ),
        ],
      ],
      right: [
        const SettingsHeader(
          'On-screen cue',
          caption: 'Only changes when the “thinking” indicator appears.',
        ),
        SettingsSwitch(
          key: const Key('settings-endpoint-cue'),
          icon: Icons.hourglass_top,
          title: 'Instant processing cue',
          info:
              'Show the processing indicator the moment you stop speaking, '
              'measured on this device, instead of waiting for the assistant. It '
              'does not change when the assistant actually stops listening — that '
              'is the End-of-speech wait on the left.',
          value: s.endpointCueEnabled,
          onChanged: (v) => c.updateLocal(s.copyWith(endpointCueEnabled: v)),
        ),
        if (advanced) ...[
          SettingsSlider(
            label: 'Cue silence',
            sliderKey: const Key('settings-endpoint-silence'),
            enabled: s.endpointCueEnabled,
            value: s.endpointSilenceMs.toDouble(),
            min: 200,
            max: 1500,
            divisions: 26,
            format: (v) => '${v.round()} ms',
            onChanged: (v) => c.updateLocal(
              c.settings.copyWith(endpointSilenceMs: v.round()),
              commit: false,
            ),
            onChangeEnd: (_) => c.commitLocalSoon(),
          ),
          SettingsSlider(
            label: 'Cue silence level',
            sliderKey: const Key('settings-endpoint-level'),
            enabled: s.endpointCueEnabled,
            value: s.endpointRmsThreshold,
            min: 0.002,
            max: 0.05,
            divisions: 48,
            format: (v) => v.toStringAsFixed(3),
            onChanged: (v) => c.updateLocal(
              c.settings.copyWith(endpointRmsThreshold: v),
              commit: false,
            ),
            onChangeEnd: (_) => c.commitLocalSoon(),
          ),
        ],
      ],
    );
  }
}
