// People management screen (speaker_id_plan.md Phase C).
//
// Lists the speakers the assistant has identified by voice. Named people show
// their name; still-anonymous clusters show a stable "Speaker N" placeholder. The
// user can name a person, merge two clusters that are really the same person, or
// delete a profile. The voice half ("my name is …") is handled on the Mac during a
// turn; this is the settings-list half.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/settings/orchestrator_client.dart';

class PeopleScreen extends StatefulWidget {
  const PeopleScreen({super.key, required this.client});

  final OrchestratorClient client;

  @override
  State<PeopleScreen> createState() => _PeopleScreenState();
}

class _PeopleScreenState extends State<PeopleScreen> {
  List<SpeakerView>? _people;
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
      if (!mounted) return;
      setState(() {
        _people = people;
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
          decoration: const InputDecoration(labelText: 'Name', hintText: 'e.g. Sam'),
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
              child: Text('${_label(e.value, e.key)}  ·  ${e.value.samples} clips'),
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

  void _snack(String message) {
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(message)));
  }

  @override
  Widget build(BuildContext context) {
    final people = _people;
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
        subtitle: 'As people talk to the assistant it learns their voices. Say '
            '"my name is …" to introduce yourself, and they\'ll appear here.',
      );
    }
    return ListView.separated(
      itemCount: people.length,
      separatorBuilder: (_, _) => const Divider(height: 1),
      itemBuilder: (context, i) {
        final s = people[i];
        final label = _label(s, i);
        final clips = s.samples == 1 ? '1 voice clip' : '${s.samples} voice clips';
        return ListTile(
          key: ValueKey('speaker-${s.id}'),
          leading: CircleAvatar(
            child: Icon(s.labeled ? Icons.person : Icons.person_outline),
          ),
          title: Text(label),
          subtitle: Text(s.labeled ? clips : 'Not named yet · $clips'),
          trailing: PopupMenuButton<String>(
            key: ValueKey('speaker-menu-${s.id}'),
            onSelected: (action) {
              switch (action) {
                case 'rename':
                  _rename(s, i);
                case 'merge':
                  _merge(s, i);
                case 'delete':
                  _delete(s, i);
              }
            },
            itemBuilder: (context) => const [
              PopupMenuItem(value: 'rename', child: Text('Name / rename')),
              PopupMenuItem(value: 'merge', child: Text('Merge with…')),
              PopupMenuItem(value: 'delete', child: Text('Forget')),
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
