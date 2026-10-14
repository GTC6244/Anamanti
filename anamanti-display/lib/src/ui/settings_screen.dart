// Settings screen (Plan.MD §3, Phase 6; layout per plans/SettingsReorgPlan.md).
//
// A side rail of six sections, each with at most a few tabs laid out in two columns
// that fit the Echo Show's 960×600 dp landscape screen without scrolling:
//  * Assistant  — Brain (LLM backend/auth/model, TTS voice) · Capabilities (the Core's
//                 read-only cloud/local/off status per function)
//  * Listening  — Wake word (word, sensitivity, capture gain) · Microphone (Advanced:
//                 AudioRecord + platform NS/AGC/AEC, playback buffer) · End of speech
//                 (the Core's VAD beside the on-screen processing cue)
//  * Audio Diagnostics — live mic + wake-word meters with live tuning
//  * Display    — Screen (dim delay, font size, listening ring) · Photos (slideshow
//                 source + Google linking)
//  * Household  — buttons opening People, Link a phone, and Memory
//  * Device     — Connection (device name, Anamanti Core pin) · Updates (selfUpdate
//                 flavor) · Maintenance (permissions, Advanced mode, Android escape)
//
// Changes apply immediately — there is no Save button. [SettingsController] persists
// device-local settings and hands them to the app via [SettingsScreen.onApplied]
// (engine restart / slideshow refresh / live tuning), and sends Core-managed
// settings to the Mac, debounced + serialized; see settings/settings_controller.dart.
// Advanced mode ([AppSettings.advancedMode]) hides the expert controls.

import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/engine/update_controller.dart';
import 'package:anamanti_display/src/rust/api/engine.dart'
    show setDiagnosticsContext, setEnrollContext;
import 'package:anamanti_display/src/settings/app_settings.dart';
import 'package:anamanti_display/src/settings/orchestrator_client.dart';
import 'package:anamanti_display/src/settings/settings_store.dart';
import 'package:anamanti_display/src/ui/audio_diagnostics_view.dart';
import 'package:anamanti_display/src/ui/memory_screen.dart';
import 'package:anamanti_display/src/ui/pair_phone_screen.dart';
import 'package:anamanti_display/src/ui/people_screen.dart';
import 'package:anamanti_display/src/ui/settings/assistant_section.dart';
import 'package:anamanti_display/src/ui/settings/device_section.dart';
import 'package:anamanti_display/src/ui/settings/display_section.dart';
import 'package:anamanti_display/src/ui/settings/listening_section.dart';
import 'package:anamanti_display/src/ui/settings/settings_controller.dart';

export 'package:anamanti_display/src/ui/settings/settings_controller.dart'
    show LiveTune;

/// The rail sections, in rail order.
enum SettingsSection {
  assistant('Assistant', 'Assistant', Icons.smart_toy_outlined),
  listening('Listening', 'Listening', Icons.mic_none),
  diagnostics('Audio Diagnostics', 'Audio\nDiagnostics', Icons.equalizer),
  display('Display', 'Display', Icons.tv_outlined),
  household('Household', 'Household', Icons.people_outline),
  device('Device', 'Device', Icons.settings_outlined);

  const SettingsSection(this.title, this.railLabel, this.icon);

  /// AppBar title.
  final String title;

  /// Rail label (may wrap).
  final String railLabel;
  final IconData icon;
}

/// One tab within a section: a stable id (for keys `settings-tab-<id>`) + label.
class _TabSpec {
  const _TabSpec(this.id, this.label);
  final String id;
  final String label;
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
    this.onRequestEngineRestart,
    this.onLiveTune,
    this.clientForCore,
    this.onDiagnosticsActive = _nativeDiagnosticsContext,
    this.initialSection = SettingsSection.assistant,
    this.localDebounce = const Duration(milliseconds: 600),
    this.remoteDebounce = const Duration(milliseconds: 500),
  });

  /// The current device-local settings to edit.
  final AppSettings initial;

  /// Persistence for the device-local settings.
  final SettingsStore store;

  /// Client for the Core-managed settings + memory, pinned to the Core selected
  /// when Settings opened.
  final OrchestratorClient client;

  /// Build a client pinned to another Core (`''` = Auto). When the user re-pins the
  /// Core in Device → Connection, Settings switches to it once the change applies,
  /// so everything after that talks to the newly selected Mac. Null in tests.
  final OrchestratorClient Function(String orchestratorKey)? clientForCore;

  /// Called (and awaited) with each committed device-local settings snapshot so the
  /// app can restart the engine / refresh the slideshow / live-tune as needed.
  final FutureOr<void> Function(AppSettings next) onApplied;

  /// The live wake-word engine controller, for Audio Diagnostics' meters and voice
  /// enrollment. A listenable because applying a setting can restart the engine,
  /// which replaces the controller while Settings stays open. Null (or a null
  /// value) where no engine is running — Diagnostics then shows a short note.
  final ValueListenable<AssistantController?>? assistant;

  /// The in-app updater controller (plans/UpdaterPlan.md). Null on the `fdroid`
  /// flavor / in tests, which hides Device → Updates.
  final UpdateController? updates;

  /// Force an engine restart — after a newly granted Camera permission, so the
  /// proximity sensor + auto-brightness start without a relaunch.
  final VoidCallback? onRequestEngineRestart;

  /// Push capture gain + wake-word thresholds to the running engine as they change
  /// (no restart). Null in tests.
  final LiveTune? onLiveTune;

  /// Called with `true` when Audio Diagnostics opens and `false` when it closes.
  /// Defaults to the native `setDiagnosticsContext`, which marks voice turns as
  /// transcription-only (shown, never sent to the LLM) while the page is up.
  final void Function(bool active) onDiagnosticsActive;

  static void _nativeDiagnosticsContext(bool active) =>
      setDiagnosticsContext(active: active);

  /// Which section Settings opens on.
  final SettingsSection initialSection;

  /// Debounce before a device-local / Core-managed change is applied.
  final Duration localDebounce;
  final Duration remoteDebounce;

  @override
  State<SettingsScreen> createState() => _SettingsScreenState();
}

class _SettingsScreenState extends State<SettingsScreen> {
  late final SettingsController _c = SettingsController(
    initial: widget.initial,
    store: widget.store,
    client: widget.client,
    onApplied: widget.onApplied,
    onLiveTune: widget.onLiveTune,
    clientForCore: widget.clientForCore,
    localDebounce: widget.localDebounce,
    remoteDebounce: widget.remoteDebounce,
  );

  late SettingsSection _section = widget.initialSection;

  /// The selected tab id per section (sections remember their tab).
  final Map<SettingsSection, String> _tabs = <SettingsSection, String>{};

  /// Stand-in for [SettingsScreen.assistant] when none is wired (tests).
  final ValueNotifier<AssistantController?> _noEngine =
      ValueNotifier<AssistantController?>(null);

  @override
  void initState() {
    super.initState();
    _c.loadRemote();
    // The Brain tab's capability summary (and the Capabilities tab) need this.
    _c.loadFunctions();
  }

  @override
  void dispose() {
    // Leaving Settings: apply anything still pending (a debounce, a slider mid-drag,
    // a text field mid-edit) so nothing is lost by navigating away.
    _c.flush();
    _c.dispose();
    _noEngine.dispose();
    super.dispose();
  }

  List<_TabSpec> _tabsFor(SettingsSection section) {
    final advanced = _c.settings.advancedMode;
    return switch (section) {
      SettingsSection.assistant => const [
        _TabSpec('brain', 'Brain'),
        _TabSpec('capabilities', 'Capabilities'),
      ],
      SettingsSection.listening => [
        const _TabSpec('wakeWord', 'Wake word'),
        if (advanced) const _TabSpec('microphone', 'Microphone'),
        const _TabSpec('endOfSpeech', 'End of speech'),
      ],
      SettingsSection.display => const [
        _TabSpec('screen', 'Screen'),
        _TabSpec('photos', 'Photos'),
      ],
      SettingsSection.device => [
        const _TabSpec('connection', 'Connection'),
        if (widget.updates != null) const _TabSpec('updates', 'Updates'),
        const _TabSpec('maintenance', 'Maintenance'),
      ],
      SettingsSection.diagnostics || SettingsSection.household => const [],
    };
  }

  /// The selected tab of [section], falling back to its first tab when the stored
  /// one is gone (e.g. Microphone after Advanced mode is turned off).
  String? _tabOf(SettingsSection section) {
    final tabs = _tabsFor(section);
    if (tabs.isEmpty) return null;
    final id = _tabs[section];
    return tabs.any((t) => t.id == id) ? id : tabs.first.id;
  }

  void _go(SettingsSection section, [String? tab]) {
    setState(() {
      _section = section;
      if (tab != null) _tabs[section] = tab;
    });
    if (section == SettingsSection.assistant && _c.functions == null) {
      _c.loadFunctions();
    }
  }

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: _c,
      builder: (context, _) {
        final tabs = _tabsFor(_section);
        final tab = _tabOf(_section);
        return Scaffold(
          appBar: AppBar(
            title: Text(_section.title),
            actions: [
              _ApplyStatus(controller: _c),
              if (_c.settings.advancedMode)
                Padding(
                  padding: const EdgeInsets.only(left: 8),
                  child: ActionChip(
                    key: const Key('settings-advanced-chip'),
                    avatar: const Icon(Icons.tune, size: 16),
                    label: const Text('Advanced'),
                    onPressed: () => _go(SettingsSection.device, 'maintenance'),
                  ),
                ),
              const SizedBox(width: 12),
            ],
          ),
          body: Row(
            children: [
              _Rail(selected: _section, onSelect: _go),
              const VerticalDivider(width: 1),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    if (tabs.length > 1)
                      _TabStrip(
                        tabs: tabs,
                        selected: tab!,
                        onSelect: (id) => _go(_section, id),
                      ),
                    Expanded(
                      child: KeyedSubtree(
                        key: Key('settings-page-${_section.name}-${tab ?? ''}'),
                        child: _page(_section, tab),
                      ),
                    ),
                  ],
                ),
              ),
            ],
          ),
        );
      },
    );
  }

  Widget _page(SettingsSection section, String? tab) {
    switch (section) {
      case SettingsSection.assistant:
        return tab == 'capabilities'
            ? AssistantCapabilitiesTab(controller: _c)
            : AssistantBrainTab(
                controller: _c,
                onOpenCapabilities: () =>
                    _go(SettingsSection.assistant, 'capabilities'),
              );
      case SettingsSection.listening:
        return switch (tab) {
          'microphone' => ListeningMicrophoneTab(controller: _c),
          'endOfSpeech' => ListeningEndOfSpeechTab(controller: _c),
          _ => ListeningWakeWordTab(
            controller: _c,
            onOpenDiagnostics: () => _go(SettingsSection.diagnostics),
          ),
        };
      case SettingsSection.diagnostics:
        return _diagnosticsPage();
      case SettingsSection.display:
        return tab == 'photos'
            ? DisplayPhotosTab(controller: _c)
            : DisplayScreenTab(controller: _c);
      case SettingsSection.household:
        return _householdPage();
      case SettingsSection.device:
        final updates = widget.updates;
        return switch (tab) {
          'updates' when updates != null => DeviceUpdatesTab(
            controller: _c,
            updates: updates,
          ),
          'maintenance' => DeviceMaintenanceTab(
            controller: _c,
            onRequestEngineRestart: widget.onRequestEngineRestart,
          ),
          _ => DeviceConnectionTab(controller: _c, updates: updates),
        };
    }
  }

  /// Audio Diagnostics: live mic + wake-word meters with live-applied tuning.
  /// Follows the engine controller across restarts.
  Widget _diagnosticsPage() {
    return ValueListenableBuilder<AssistantController?>(
      valueListenable: widget.assistant ?? _noEngine,
      builder: (context, assistant, _) {
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
        // Not keyed by the controller: across an engine restart the same view
        // swaps controllers (didUpdateWidget), so the transcription-only context
        // stays set for as long as the page is up.
        return AudioDiagnosticsView(
          key: const Key('audio-diagnostics-view'),
          controller: assistant,
          settings: _c.settings,
          // Applies + persists like any other change; the controller also pushes
          // gain/threshold to the running engine live, so onTune has nothing to do.
          onChanged: (next) => _c.updateLocal(next),
          onTune: (_, _) {},
          // Entering/leaving toggles the Core's transcription-only context, so voice
          // turns here show the recognized words but are never sent to the LLM.
          onActiveChanged: widget.onDiagnosticsActive,
        );
      },
    );
  }

  /// Household: big buttons opening the existing full screens.
  Widget _householdPage() {
    return Padding(
      padding: const EdgeInsets.all(16),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Expanded(
            child: _HouseholdCard(
              key: const Key('settings-menu-people'),
              icon: Icons.record_voice_over_outlined,
              title: 'People',
              subtitle: 'Recognize & train voices',
              onTap: _openPeople,
            ),
          ),
          const SizedBox(width: 16),
          Expanded(
            child: _HouseholdCard(
              key: const Key('settings-menu-pair-phone'),
              icon: Icons.phonelink_ring,
              title: 'Link a phone',
              subtitle: 'Pair a family member’s phone for messages',
              onTap: _openPairPhone,
            ),
          ),
          const SizedBox(width: 16),
          Expanded(
            child: _HouseholdCard(
              key: const Key('settings-menu-memory'),
              icon: Icons.psychology_outlined,
              title: 'Memory',
              subtitle: 'Browse & delete what the assistant remembers',
              onTap: _openMemory,
            ),
          ),
        ],
      ),
    );
  }

  /// People: recognized voices + the guided "Add a person" enrollment flow. The
  /// live engine controller powers enrollment capture; without one the Add-person
  /// button hides and passive management still works.
  void _openPeople() {
    Navigator.of(context).push(
      MaterialPageRoute<void>(
        // Rebuilt with the live engine controller, so an engine restart while People
        // is open doesn't leave enrollment bound to a disposed one.
        builder: (_) => ValueListenableBuilder<AssistantController?>(
          valueListenable: widget.assistant ?? _noEngine,
          builder: (context, assistant, _) => PeopleScreen(
            client: _c.client,
            controller: assistant,
            setEnrollContext:
                ({
                  required bool active,
                  required String session,
                  required String name,
                  required int sampleIndex,
                  required String phase,
                }) => setEnrollContext(
                  active: active,
                  session: session,
                  name: name,
                  sampleIndex: sampleIndex,
                  phase: phase,
                ),
            wakeWord: _c.settings.wakeWord.replaceAll('_', ' '),
          ),
        ),
      ),
    );
  }

  /// Memory: Facts / Conversations / Keywords, each with browse + delete.
  void _openMemory() {
    Navigator.of(context).push(
      MaterialPageRoute<void>(builder: (_) => MemoryScreen(client: _c.client)),
    );
  }

  /// Link a phone: a family member's name + the 4-character AppSaid pairing code.
  void _openPairPhone() {
    Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (_) => PairPhoneScreen(client: _c.client),
      ),
    );
  }
}

/// The left navigation rail: one large touch target per section.
class _Rail extends StatelessWidget {
  const _Rail({required this.selected, required this.onSelect});

  final SettingsSection selected;
  final void Function(SettingsSection section) onSelect;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return SizedBox(
      width: 96,
      child: Column(
        children: [
          const SizedBox(height: 4),
          for (final s in SettingsSection.values)
            InkWell(
              key: Key('settings-rail-${s.name}'),
              onTap: () => onSelect(s),
              child: Container(
                height: 76,
                width: 96,
                padding: const EdgeInsets.symmetric(horizontal: 4),
                color: s == selected
                    ? scheme.secondaryContainer.withValues(alpha: 0.6)
                    : null,
                child: Column(
                  mainAxisAlignment: MainAxisAlignment.center,
                  children: [
                    Icon(s.icon, color: s == selected ? scheme.primary : null),
                    const SizedBox(height: 4),
                    Text(
                      s.railLabel,
                      textAlign: TextAlign.center,
                      maxLines: 2,
                      style: TextStyle(
                        fontSize: 11,
                        height: 1.1,
                        color: s == selected ? scheme.primary : null,
                        fontWeight: s == selected
                            ? FontWeight.w600
                            : FontWeight.normal,
                      ),
                    ),
                  ],
                ),
              ),
            ),
        ],
      ),
    );
  }
}

/// The tab strip across the top of a section with more than one tab.
class _TabStrip extends StatelessWidget {
  const _TabStrip({
    required this.tabs,
    required this.selected,
    required this.onSelect,
  });

  final List<_TabSpec> tabs;
  final String selected;
  final ValueChanged<String> onSelect;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Container(
      height: 48,
      decoration: BoxDecoration(
        border: Border(
          bottom: BorderSide(color: Theme.of(context).dividerColor),
        ),
      ),
      child: Row(
        children: [
          for (final t in tabs)
            InkWell(
              key: Key('settings-tab-${t.id}'),
              onTap: () => onSelect(t.id),
              child: Container(
                padding: const EdgeInsets.symmetric(horizontal: 20),
                alignment: Alignment.center,
                decoration: BoxDecoration(
                  border: Border(
                    bottom: BorderSide(
                      color: t.id == selected
                          ? scheme.primary
                          : Colors.transparent,
                      width: 3,
                    ),
                  ),
                ),
                child: Text(
                  t.label,
                  style: TextStyle(
                    color: t.id == selected ? scheme.primary : null,
                    fontWeight: t.id == selected
                        ? FontWeight.w600
                        : FontWeight.normal,
                  ),
                ),
              ),
            ),
        ],
      ),
    );
  }
}

/// AppBar status: "Saving…" while a change is pending/applying, the last error, or
/// a brief "Saved" tick.
class _ApplyStatus extends StatelessWidget {
  const _ApplyStatus({required this.controller});

  final SettingsController controller;

  @override
  Widget build(BuildContext context) {
    final c = controller;
    final Widget child;
    if (c.busy) {
      child = const Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          SizedBox(
            width: 14,
            height: 14,
            child: CircularProgressIndicator(strokeWidth: 2),
          ),
          SizedBox(width: 8),
          Text('Saving…'),
        ],
      );
    } else if (c.lastError != null) {
      child = ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 380),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            const Icon(Icons.error_outline, color: Colors.orange, size: 18),
            const SizedBox(width: 6),
            Flexible(
              child: Text(
                c.lastError!,
                overflow: TextOverflow.ellipsis,
                style: const TextStyle(color: Colors.orange),
              ),
            ),
          ],
        ),
      );
    } else if (c.lastAppliedAt != null) {
      child = const Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Icon(Icons.check, size: 18, color: Colors.green),
          SizedBox(width: 6),
          Text('Saved'),
        ],
      );
    } else {
      child = const SizedBox.shrink();
    }
    return Center(
      child: KeyedSubtree(
        key: const Key('settings-apply-status'),
        child: child,
      ),
    );
  }
}

/// A large Household button.
class _HouseholdCard extends StatelessWidget {
  const _HouseholdCard({
    super.key,
    required this.icon,
    required this.title,
    required this.subtitle,
    required this.onTap,
  });

  final IconData icon;
  final String title;
  final String subtitle;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    return Card(
      clipBehavior: Clip.antiAlias,
      child: InkWell(
        onTap: onTap,
        child: SizedBox(
          height: 220,
          child: Padding(
            padding: const EdgeInsets.all(16),
            child: Column(
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                Icon(icon, size: 48),
                const SizedBox(height: 12),
                Text(title, style: Theme.of(context).textTheme.titleMedium),
                const SizedBox(height: 4),
                Text(
                  subtitle,
                  textAlign: TextAlign.center,
                  style: const TextStyle(color: Colors.grey),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
