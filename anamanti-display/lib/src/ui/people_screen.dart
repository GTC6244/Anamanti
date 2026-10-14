// People management screen (speaker_id_plan.md Phase C).
//
// Lists the speakers the assistant has identified by voice. Named people show
// their name; still-anonymous clusters show a stable "Speaker N" placeholder. The
// user can name a person, merge two clusters that are really the same person, or
// delete a profile. The voice half ("my name is …") is handled on the Mac during a
// turn; this is the settings-list half.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/settings/orchestrator_client.dart';
import 'package:anamanti_display/src/ui/speaker_enroll_screen.dart';

class PeopleScreen extends StatefulWidget {
  const PeopleScreen({
    super.key,
    required this.client,
    this.controller,
    this.setEnrollContext,
    this.wakeWord = '',
  });

  final OrchestratorClient client;

  /// The live engine controller, needed for the guided "Add a person" enrollment flow
  /// (it surfaces each capture result). When null the Add-person button is hidden —
  /// passive naming/merge/forget still work.
  final AssistantController? controller;

  /// Stamps the enroll display context during a capture (see [EnrollContextSink]);
  /// null disables stamping (tests).
  final EnrollContextSink? setEnrollContext;

  /// The configured wake word, shown in the enrollment capture instructions.
  final String wakeWord;

  @override
  State<PeopleScreen> createState() => _PeopleScreenState();
}

class _PeopleScreenState extends State<PeopleScreen> {
  List<SpeakerView>? _people;

  /// The Mac's household roster (names), for "Link to household member…" and the
  /// per-row household badge. Best-effort: empty when unavailable.
  List<String> _members = const [];
  String? _error;
  bool _busy = false;

  @override
  void initState() {
    super.initState();
    _reload();
  }

  Future<void> _reload() async {
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      final people = await widget.client.listSpeakers();
      List<String> members = const [];
      try {
        members = await widget.client.listHouseholdMembers();
      } catch (_) {}
      if (!mounted) return;
      setState(() {
        _people = people;
        _members = members;
        _busy = false;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() {
        _error = 'Couldn\'t reach the assistant: $e';
        _busy = false;
      });
    }
  }

  /// A user-facing label: the name, or a stable "Speaker N" for an anonymous
  /// cluster (N by first-heard order, which the list preserves).
  String _label(SpeakerView s, int index) => s.name ?? 'Speaker ${index + 1}';

  Future<void> _rename(SpeakerView s, int index) async {
    final controller = TextEditingController(text: s.name ?? '');
    final name = await showDialog<String>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Name ${_label(s, index)}'),
        content: TextField(
          controller: controller,
          autofocus: true,
          decoration: const InputDecoration(
            labelText: 'Name',
            hintText: 'e.g. Sam',
          ),
          onSubmitted: (v) => Navigator.pop(context, v.trim()),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(context, controller.text.trim()),
            child: const Text('Save'),
          ),
        ],
      ),
    );
    if (name == null || name.isEmpty) return;
    try {
      await widget.client.nameSpeaker(s.id, name);
    } catch (e) {
      if (!mounted) return;
      _snack('Rename failed: $e');
      return;
    }
    await _reload();
  }

  /// The roster member a voice is linked to (its name matches, case-insensitive).
  String? _memberOf(SpeakerView s) {
    final n = s.name?.trim().toLowerCase();
    if (n == null || n.isEmpty) return null;
    for (final m in _members) {
      if (m.trim().toLowerCase() == n) return m;
    }
    return null;
  }

  /// Another voiceprint (not [s]) already linked to [member], if any — linking [s]
  /// to that member merges into it.
  SpeakerView? _voiceOf(String member, {required SpeakerView except}) {
    final m = member.trim().toLowerCase();
    for (final p in _people ?? const <SpeakerView>[]) {
      if (p.id != except.id && p.name?.trim().toLowerCase() == m) return p;
    }
    return null;
  }

  /// "Link to household member…": associate this voice with a person in the Mac's
  /// household roster. If that member already has a voiceprint, this voice is merged
  /// into it (after a confirm) so each household member has one voiceprint; otherwise
  /// this voice is named as them. The Core makes the final call
  /// (`OrchestratorClient.linkSpeaker`).
  Future<void> _linkToMember(SpeakerView s, int index) async {
    if (_members.isEmpty) {
      _snack(
        'No household members yet — add people on the Mac (Household page) first.',
      );
      return;
    }
    final label = _label(s, index);
    final current = _memberOf(s);
    final member = await showDialog<String>(
      context: context,
      builder: (context) => SimpleDialog(
        title: Text('Link $label to…'),
        children: [
          for (final m in _members)
            Builder(
              builder: (context) {
                final existing = _voiceOf(m, except: s);
                final isCurrent = current == m && existing == null;
                return SimpleDialogOption(
                  key: ValueKey('link-member-$m'),
                  onPressed: isCurrent ? null : () => Navigator.pop(context, m),
                  child: ListTile(
                    contentPadding: EdgeInsets.zero,
                    leading: Icon(
                      existing != null
                          ? Icons.call_merge
                          : Icons.person_add_alt,
                    ),
                    title: Text(m),
                    subtitle: Text(
                      isCurrent
                          ? 'Already linked'
                          : existing != null
                          ? 'Merge into $m\'s voiceprint'
                          : 'This voice becomes $m',
                    ),
                    enabled: !isCurrent,
                  ),
                );
              },
            ),
        ],
      ),
    );
    if (member == null || !mounted) return;
    final existing = _voiceOf(member, except: s);
    if (existing != null) {
      final ok = await showDialog<bool>(
        context: context,
        builder: (context) => AlertDialog(
          title: Text('Merge $label into $member?'),
          content: Text(
            "$label's voice clips and memories move to $member, and the two "
            'voiceprints become one. This can\'t be undone.',
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(context, false),
              child: const Text('Cancel'),
            ),
            FilledButton(
              key: const Key('link-merge-confirm'),
              onPressed: () => Navigator.pop(context, true),
              child: const Text('Merge'),
            ),
          ],
        ),
      );
      if (ok != true || !mounted) return;
    }
    try {
      await widget.client.linkSpeaker(s.id, member);
    } catch (e) {
      if (!mounted) return;
      _snack('Couldn\'t link: $e');
      return;
    }
    if (!mounted) return;
    _snack(
      existing != null
          ? 'Merged $label into $member.'
          : '$label is now linked to $member.',
    );
    await _reload();
  }

  Future<void> _merge(SpeakerView keep, int keepIndex) async {
    final others = <MapEntry<int, SpeakerView>>[
      for (var i = 0; i < (_people?.length ?? 0); i++)
        if (_people![i].id != keep.id) MapEntry(i, _people![i]),
    ];
    if (others.isEmpty) {
      _snack('No other person to merge with.');
      return;
    }
    final drop = await showDialog<SpeakerView>(
      context: context,
      builder: (context) => SimpleDialog(
        title: Text('Merge into ${_label(keep, keepIndex)}'),
        children: [
          for (final e in others)
            SimpleDialogOption(
              onPressed: () => Navigator.pop(context, e.value),
              child: Text(
                '${_label(e.value, e.key)}  ·  ${e.value.samples} clips',
              ),
            ),
        ],
      ),
    );
    if (drop == null) return;
    try {
      await widget.client.mergeSpeakers(keep: keep.id, drop: drop.id);
    } catch (e) {
      if (!mounted) return;
      _snack('Merge failed: $e');
      return;
    }
    await _reload();
  }

  Future<void> _delete(SpeakerView s, int index) async {
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Forget ${_label(s, index)}?'),
        content: const Text(
          'This deletes their voiceprint. Memories tied to them stay but become '
          'unattributed until they are heard again.',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Forget'),
          ),
        ],
      ),
    );
    if (confirmed != true) return;
    try {
      await widget.client.deleteSpeaker(s.id);
    } catch (e) {
      if (!mounted) return;
      _snack('Delete failed: $e');
      return;
    }
    if (!mounted) return;
    setState(() => _people?.removeWhere((p) => p.id == s.id));
  }

  /// Play back a person's recorded voice clips through the assistant's speaker so a
  /// human can listen and recognize them (then name the cluster). The audio plays on
  /// the Mac-connected device's speaker; this just asks the engine to play it.
  Future<void> _play(SpeakerView s, int index) async {
    try {
      final count = await widget.client.playSpeakerClips(s.id);
      if (!mounted) return;
      if (count == 0) {
        _snack('No voice clips saved yet for ${_label(s, index)}.');
      } else {
        final clips = count == 1 ? '1 clip' : '$count clips';
        _snack('Playing $clips for ${_label(s, index)}…');
      }
    } catch (e) {
      if (!mounted) return;
      _snack('Couldn\'t play clips: $e');
    }
  }

  void _snack(String message) {
    ScaffoldMessenger.of(
      context,
    ).showSnackBar(SnackBar(content: Text(message)));
  }

  /// Open the guided "Add a voice" enrollment flow (for an existing household member
  /// or a new person), then refresh so the new/updated voiceprint appears. Only available when the engine controller is wired.
  Future<void> _addPerson() async {
    final controller = widget.controller;
    if (controller == null) return;
    await Navigator.of(context).push<bool>(
      MaterialPageRoute<bool>(
        builder: (_) => SpeakerEnrollScreen(
          client: widget.client,
          controller: controller,
          setEnrollContext: widget.setEnrollContext,
          wakeWord: widget.wakeWord,
        ),
      ),
    );
    if (!mounted) return;
    await _reload();
  }

  /// "Add voice samples" for a person already on the list: run the guided phrase
  /// capture and fold the samples into *their* voiceprint (by id), then refresh.
  Future<void> _addSamples(SpeakerView s, int index) async {
    final controller = widget.controller;
    if (controller == null) return;
    await Navigator.of(context).push<bool>(
      MaterialPageRoute<bool>(
        builder: (_) => SpeakerEnrollScreen(
          client: widget.client,
          controller: controller,
          setEnrollContext: widget.setEnrollContext,
          wakeWord: widget.wakeWord,
          speakerId: s.id,
          personLabel: _label(s, index),
        ),
      ),
    );
    if (!mounted) return;
    await _reload();
  }

  @override
  Widget build(BuildContext context) {
    final people = _people;
    final canEnroll = widget.controller != null;
    return Scaffold(
      appBar: AppBar(
        title: const Text('People'),
        actions: [
          IconButton(
            tooltip: 'Refresh',
            onPressed: _busy ? null : _reload,
            icon: const Icon(Icons.refresh),
          ),
        ],
      ),
      floatingActionButton: canEnroll
          ? FloatingActionButton.extended(
              key: const Key('people-add'),
              onPressed: _addPerson,
              icon: const Icon(Icons.record_voice_over),
              label: const Text('Add voice'),
            )
          : null,
      body: _buildBody(people),
    );
  }

  Widget _buildBody(List<SpeakerView>? people) {
    if (_busy && people == null) {
      return const Center(child: CircularProgressIndicator());
    }
    if (_error != null) {
      return _Centered(
        icon: Icons.cloud_off,
        title: 'People unavailable',
        subtitle: _error!,
        action: FilledButton(onPressed: _reload, child: const Text('Retry')),
      );
    }
    if (people == null || people.isEmpty) {
      return const _Centered(
        icon: Icons.groups_outlined,
        title: 'No one recognized yet',
        subtitle:
            'As people talk to the assistant it learns their voices. Say '
            '"my name is …" to introduce yourself, and they\'ll appear here.',
      );
    }
    return ListView.separated(
      itemCount: people.length,
      separatorBuilder: (_, _) => const Divider(height: 1),
      itemBuilder: (context, i) {
        final s = people[i];
        final label = _label(s, i);
        final clips = s.samples == 1
            ? '1 voice clip'
            : '${s.samples} voice clips';
        final member = _memberOf(s);
        // Household-linked voices say so; named voices outside the roster (and the
        // roster is known) are flagged so they can be linked.
        final status = member != null
            ? 'Household member · $clips'
            : !s.labeled
            ? 'Not named yet · $clips'
            : _members.isNotEmpty
            ? 'Not linked to household · $clips'
            : clips;
        return ListTile(
          key: ValueKey('speaker-${s.id}'),
          leading: CircleAvatar(
            child: Icon(
              member != null
                  ? Icons.home
                  : s.labeled
                  ? Icons.person
                  : Icons.person_outline,
            ),
          ),
          title: Text(label),
          subtitle: Text(status),
          trailing: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              IconButton(
                key: ValueKey('speaker-play-${s.id}'),
                tooltip: 'Play voice clips',
                icon: const Icon(Icons.play_circle_outline),
                onPressed: () => _play(s, i),
              ),
              PopupMenuButton<String>(
                key: ValueKey('speaker-menu-${s.id}'),
                onSelected: (action) {
                  switch (action) {
                    case 'rename':
                      _rename(s, i);
                    case 'link':
                      _linkToMember(s, i);
                    case 'samples':
                      _addSamples(s, i);
                    case 'merge':
                      _merge(s, i);
                    case 'delete':
                      _delete(s, i);
                  }
                },
                itemBuilder: (context) => [
                  PopupMenuItem(
                    key: ValueKey('speaker-link-${s.id}'),
                    value: 'link',
                    child: const Text('Link to household member…'),
                  ),
                  const PopupMenuItem(
                    value: 'rename',
                    child: Text('Name / rename'),
                  ),
                  // Needs the live engine (captures ride the wake-word turn).
                  if (widget.controller != null)
                    PopupMenuItem(
                      key: ValueKey('speaker-add-samples-${s.id}'),
                      value: 'samples',
                      child: const Text('Add voice samples'),
                    ),
                  const PopupMenuItem(
                    value: 'merge',
                    child: Text('Merge with…'),
                  ),
                  const PopupMenuItem(value: 'delete', child: Text('Forget')),
                ],
              ),
            ],
          ),
          onTap: () => _rename(s, i),
        );
      },
    );
  }
}

class _Centered extends StatelessWidget {
  const _Centered({
    required this.icon,
    required this.title,
    required this.subtitle,
    this.action,
  });

  final IconData icon;
  final String title;
  final String subtitle;
  final Widget? action;

  @override
  Widget build(BuildContext context) {
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(32),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(icon, size: 48, color: Theme.of(context).colorScheme.primary),
            const SizedBox(height: 16),
            Text(title, style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 8),
            Text(subtitle, textAlign: TextAlign.center),
            if (action != null) ...[const SizedBox(height: 16), action!],
          ],
        ),
      ),
    );
  }
}
