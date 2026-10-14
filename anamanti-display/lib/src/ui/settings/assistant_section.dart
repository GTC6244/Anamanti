// Settings → Assistant: what the assistant thinks + speaks with (Brain) and the
// Core's read-only capability status (Capabilities). All Core-managed.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/settings/orchestrator_client.dart';
import 'package:anamanti_display/src/ui/settings/settings_controller.dart';
import 'package:anamanti_display/src/ui/settings/settings_widgets.dart';

/// Assistant → Brain: LLM backend, Anthropic auth, model, TTS voice.
class AssistantBrainTab extends StatelessWidget {
  const AssistantBrainTab({
    super.key,
    required this.controller,
    required this.onOpenCapabilities,
  });

  final SettingsController controller;
  final VoidCallback onOpenCapabilities;

  @override
  Widget build(BuildContext context) {
    final c = controller;
    final r = c.remote;
    final editable = c.remoteEditable;
    final advanced = c.settings.advancedMode;
    // Mock is a developer backend: offered in Advanced mode, or kept visible if it's
    // the current selection so the control never shows an empty choice.
    final backends = kLlmBackends.entries.where(
      (e) => e.key != 'mock' || advanced || r.backend == 'mock',
    );

    return SettingsTabBody(
      header: RemoteStatusBanner(
        loading: c.remoteLoading,
        error: c.remoteError,
        onRetry: c.loadRemote,
      ),
      left: [
        const SettingsHeader('Language model'),
        Padding(
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 4),
          child: SizedBox(
            width: double.infinity,
            child: SegmentedButton<String>(
              key: const Key('settings-backend'),
              showSelectedIcon: false,
              segments: [
                for (final e in backends)
                  ButtonSegment(value: e.key, label: Text(e.value)),
              ],
              selected: {r.backend},
              onSelectionChanged: editable
                  ? (sel) {
                      final v = sel.first;
                      if (v == r.backend) return;
                      // Switching backend resets the model to that backend's
                      // default so a stale model id from another provider is
                      // never sent.
                      c.updateRemote(r.copyWith(backend: v, model: ''));
                    }
                  : null,
            ),
          ),
        ),
        if (r.backend == 'anthropic')
          SettingsRow(
            title: 'Anthropic auth',
            trailing: SegmentedButton<String>(
              key: const Key('settings-anthropic-auth'),
              showSelectedIcon: false,
              segments: const [
                ButtonSegment(value: 'apikey', label: Text('API key')),
                ButtonSegment(
                  value: 'subscription',
                  label: Text('Subscription'),
                ),
              ],
              selected: {r.anthropicAuth},
              onSelectionChanged: editable
                  ? (sel) =>
                        c.updateRemote(r.copyWith(anthropicAuth: sel.first))
                  : null,
            ),
          ),
        if (r.backend == 'openai')
          const Padding(
            padding: EdgeInsets.fromLTRB(12, 4, 12, 4),
            child: Text(
              'OpenAI uses an API key (OPENAI_API_KEY); there is no subscription '
              'option.',
              style: TextStyle(fontSize: 12, color: Colors.grey),
            ),
          ),
        _ModelField(controller: c),
      ],
      right: [
        const SettingsHeader('Voice'),
        _VoiceField(controller: c),
        const SettingsHeader('Capabilities'),
        _CapabilitiesSummary(controller: c, onTap: onOpenCapabilities),
      ],
    );
  }
}

/// The model control: a dropdown of the Core's recent models for cloud backends,
/// or a free-text field for local/mock (e.g. an Ollama tag). A model pinned outside
/// the fetched list stays selectable.
class _ModelField extends StatelessWidget {
  const _ModelField({required this.controller});

  final SettingsController controller;

  @override
  Widget build(BuildContext context) {
    final c = controller;
    final r = c.remote;
    if (!kCloudBackends.contains(r.backend)) {
      return Padding(
        padding: const EdgeInsets.fromLTRB(12, 4, 12, 4),
        child: CommitTextField(
          key: const Key('settings-model'),
          text: r.model,
          enabled: c.remoteEditable,
          label: 'Model',
          hint: 'e.g. llama3.2 or qwen2.5',
          onChanged: (v) => c.updateRemote(r.copyWith(model: v), commit: false),
          onCommit: c.commitRemoteSoon,
        ),
      );
    }
    final current = r.model.trim();
    final providerModels = c.models.where((m) => m.provider == r.backend);
    final ids = providerModels.map((m) => m.id).toSet();
    return Padding(
      padding: const EdgeInsets.fromLTRB(12, 4, 12, 4),
      child: InputDecorator(
        decoration: const InputDecoration(
          labelText: 'Model',
          helperText: 'Released in the last 12 months',
          isDense: true,
        ),
        child: DropdownButton<String>(
          key: const Key('settings-model'),
          isExpanded: true,
          isDense: true,
          underline: const SizedBox.shrink(),
          value: current,
          items: [
            const DropdownMenuItem(value: '', child: Text('Backend default')),
            for (final m in providerModels)
              DropdownMenuItem(value: m.id, child: Text(m.label)),
            if (current.isNotEmpty && !ids.contains(current))
              DropdownMenuItem(
                value: current,
                child: Text('$current (pinned)'),
              ),
          ],
          onChanged: c.remoteEditable
              ? (v) => c.updateRemote(r.copyWith(model: v ?? ''))
              : null,
        ),
      ),
    );
  }
}

/// The TTS voice: a dropdown of the Core's installed Piper voices, or free text when
/// the list is unavailable. A voice not in the list stays selectable.
class _VoiceField extends StatelessWidget {
  const _VoiceField({required this.controller});

  final SettingsController controller;

  @override
  Widget build(BuildContext context) {
    final c = controller;
    final r = c.remote;
    if (c.voices.isEmpty) {
      return Padding(
        padding: const EdgeInsets.fromLTRB(12, 4, 12, 4),
        child: CommitTextField(
          key: const Key('settings-voice'),
          text: r.voice,
          enabled: c.remoteEditable,
          label: 'TTS voice',
          hint: 'Piper voice, e.g. en_US-amy-medium (blank = default)',
          onChanged: (v) => c.updateRemote(r.copyWith(voice: v), commit: false),
          onCommit: c.commitRemoteSoon,
        ),
      );
    }
    final current = r.voice.trim();
    final names = c.voices.map((v) => v.name).toSet();
    String labelFor(VoiceOption v) =>
        v.language == null ? v.label : '${v.label} · ${v.language}';
    return Padding(
      padding: const EdgeInsets.fromLTRB(12, 4, 12, 4),
      child: InputDecorator(
        decoration: const InputDecoration(
          labelText: 'TTS voice',
          helperText: 'Installed Piper voices',
          isDense: true,
        ),
        child: DropdownButton<String>(
          key: const Key('settings-voice'),
          isExpanded: true,
          isDense: true,
          underline: const SizedBox.shrink(),
          value: current,
          items: [
            const DropdownMenuItem(value: '', child: Text('Server default')),
            for (final v in c.voices)
              DropdownMenuItem(value: v.name, child: Text(labelFor(v))),
            if (current.isNotEmpty && !names.contains(current))
              DropdownMenuItem(
                value: current,
                child: Text('$current (not installed)'),
              ),
          ],
          onChanged: c.remoteEditable
              ? (v) => c.updateRemote(r.copyWith(voice: v ?? ''))
              : null,
        ),
      ),
    );
  }
}

/// A one-line health summary of the Capabilities tab ("12 on · 1 key missing"),
/// tapping through to it.
class _CapabilitiesSummary extends StatelessWidget {
  const _CapabilitiesSummary({required this.controller, required this.onTap});

  final SettingsController controller;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final fns = controller.functions;
    final String text;
    var warn = false;
    if (controller.functionsLoading) {
      text = 'Checking…';
    } else if (controller.functionsError != null || fns == null) {
      text = 'Status unavailable';
    } else {
      final on = fns.where((f) => f.mode != 'off' && !f.keyMissing).length;
      final missing = fns.where((f) => f.keyMissing).length;
      warn = missing > 0;
      text = missing == 0 ? '$on on' : '$on on · $missing key missing';
    }
    return ListTile(
      key: const Key('settings-capabilities-summary'),
      dense: true,
      contentPadding: const EdgeInsets.symmetric(horizontal: 12),
      leading: Icon(
        warn ? Icons.key_off : Icons.checklist,
        color: warn ? Colors.red : null,
      ),
      title: Text(text),
      subtitle: const Text('What the assistant can use'),
      trailing: const Icon(Icons.chevron_right),
      onTap: onTap,
    );
  }
}

/// Assistant → Capabilities: a read-only grid of what the Core is set up to use —
/// cloud / local / off per capability, the provider + model, and a red warning when
/// a required API key is missing. Secret values are never sent, only whether each
/// key is set.
class AssistantCapabilitiesTab extends StatelessWidget {
  const AssistantCapabilitiesTab({super.key, required this.controller});

  final SettingsController controller;

  /// Fixed display order of the Core's categories.
  static const List<String> _order = <String>[
    'Config',
    'Speech',
    'Tools',
    'System1',
    'Memory',
  ];

  @override
  Widget build(BuildContext context) {
    final c = controller;
    final Widget body;
    if (c.functionsLoading && c.functions == null) {
      body = const ListTile(
        leading: SizedBox(
          width: 20,
          height: 20,
          child: CircularProgressIndicator(strokeWidth: 2),
        ),
        title: Text('Contacting the assistant…'),
      );
    } else if (c.functionsError != null) {
      body = ListTile(
        leading: const Icon(Icons.cloud_off),
        title: const Text('Assistant offline'),
        subtitle: const Text('This needs the Mac to be reachable.'),
        trailing: TextButton(
          onPressed: c.loadFunctions,
          child: const Text('Retry'),
        ),
      );
    } else {
      final fns = List<FunctionStatusView>.of(c.functions ?? const [])
        ..sort((a, b) {
          int rank(String cat) {
            final i = _order.indexOf(cat);
            return i < 0 ? _order.length : i;
          }

          return rank(a.category).compareTo(rank(b.category));
        });
      body = fns.isEmpty
          ? const Padding(
              padding: EdgeInsets.all(24),
              child: Center(child: Text('No functions reported.')),
            )
          : LayoutBuilder(
              builder: (context, box) {
                const gap = 8.0;
                final w = (box.maxWidth - 2 * gap) / 3;
                return Wrap(
                  spacing: gap,
                  runSpacing: gap,
                  children: [
                    for (final f in fns)
                      SizedBox(
                        width: w,
                        child: _FunctionCard(f: f),
                      ),
                  ],
                );
              },
            );
    }

    return SingleChildScrollView(
      padding: const EdgeInsets.all(12),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            children: [
              const Expanded(
                child: Text(
                  '“Cloud” runs on an internet service; “Local” runs on the Mac. A '
                  'red key means a required API key is missing on the Mac.',
                  style: TextStyle(fontSize: 12, color: Colors.grey),
                ),
              ),
              TextButton.icon(
                key: const Key('settings-functions-refresh'),
                onPressed: c.functionsLoading ? null : c.loadFunctions,
                icon: const Icon(Icons.refresh),
                label: const Text('Refresh'),
              ),
            ],
          ),
          const SizedBox(height: 8),
          body,
        ],
      ),
    );
  }
}

/// One capability card: name, provider · model, and a Cloud / Local / Off chip — or
/// the red "API key missing" warning.
class _FunctionCard extends StatelessWidget {
  const _FunctionCard({required this.f});

  final FunctionStatusView f;

  @override
  Widget build(BuildContext context) {
    final detail = [
      if (f.provider.isNotEmpty) f.provider,
      if (f.model.isNotEmpty) f.model,
    ].join(' · ');
    final missing = f.keyMissing;
    final Widget chip = missing
        ? const StatusChip(
            label: 'API key missing',
            color: Colors.red,
            icon: Icons.key_off,
          )
        : switch (f.mode) {
            'cloud' => const StatusChip(
              label: 'Cloud',
              color: Colors.blue,
              icon: Icons.cloud_outlined,
            ),
            'local' => const StatusChip(
              label: 'Local',
              color: Colors.green,
              icon: Icons.computer,
            ),
            _ => const StatusChip(
              label: 'Off',
              color: Colors.grey,
              icon: Icons.remove_circle_outline,
            ),
          };
    return Card(
      key: Key('settings-function-${f.category}-${f.name}'),
      margin: EdgeInsets.zero,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(10, 8, 10, 8),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Expanded(
                  child: Text(
                    f.name,
                    style: const TextStyle(fontWeight: FontWeight.w600),
                    overflow: TextOverflow.ellipsis,
                  ),
                ),
                chip,
              ],
            ),
            const SizedBox(height: 2),
            Text(
              [
                detail,
                if (f.note.isNotEmpty) f.note,
              ].where((s) => s.isNotEmpty).join(' — '),
              maxLines: 2,
              overflow: TextOverflow.ellipsis,
              style: const TextStyle(fontSize: 12, color: Colors.grey),
            ),
          ],
        ),
      ),
    );
  }
}
