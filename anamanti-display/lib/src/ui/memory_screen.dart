// Memory management screen (Plan.MD §3, Phase 6: "view/delete stored items").
//
// Lists the orchestrator's persistent memory entries (facts + preferences,
// explicit + inferred) and lets the user delete individual entries or clear them
// all. The voice half of memory management ("remember…", "forget that") is handled
// on the Mac during a turn; this is the settings-list half.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/settings/orchestrator_client.dart';

class MemoryScreen extends StatefulWidget {
  const MemoryScreen({super.key, required this.client});

  final OrchestratorClient client;

  @override
  State<MemoryScreen> createState() => _MemoryScreenState();
}

class _MemoryScreenState extends State<MemoryScreen> {
  List<MemoryView>? _entries;
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
      final entries = await widget.client.listMemories();
      if (!mounted) return;
      setState(() {
        _entries = entries;
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

  Future<void> _delete(MemoryView entry) async {
    try {
      await widget.client.deleteMemory(entry.id);
    } catch (e) {
      if (!mounted) return;
      _snack('Delete failed: $e');
      return;
    }
    if (!mounted) return;
    setState(() => _entries?.removeWhere((m) => m.id == entry.id));
  }

  Future<void> _clearAll() async {
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Forget everything?'),
        content: const Text('This permanently deletes all remembered facts and preferences.'),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Forget all'),
          ),
        ],
      ),
    );
    if (confirmed != true) return;
    try {
      await widget.client.clearMemories();
    } catch (e) {
      if (!mounted) return;
      _snack('Clear failed: $e');
      return;
    }
    if (!mounted) return;
    setState(() => _entries = <MemoryView>[]);
  }

  void _snack(String message) {
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(message)));
  }

  @override
  Widget build(BuildContext context) {
    final entries = _entries;
    return Scaffold(
      appBar: AppBar(
        title: const Text('Memory'),
        actions: [
          IconButton(
            tooltip: 'Refresh',
            onPressed: _busy ? null : _reload,
            icon: const Icon(Icons.refresh),
          ),
          if (entries != null && entries.isNotEmpty)
            IconButton(
              tooltip: 'Forget all',
              onPressed: _busy ? null : _clearAll,
              icon: const Icon(Icons.delete_sweep),
            ),
        ],
      ),
      body: _buildBody(entries),
    );
  }

  Widget _buildBody(List<MemoryView>? entries) {
    if (_busy && entries == null) {
      return const Center(child: CircularProgressIndicator());
    }
    if (_error != null) {
      return _Centered(
        icon: Icons.cloud_off,
        title: 'Memory unavailable',
        subtitle: _error!,
        action: FilledButton(onPressed: _reload, child: const Text('Retry')),
      );
    }
    if (entries == null || entries.isEmpty) {
      return const _Centered(
        icon: Icons.psychology_outlined,
        title: 'Nothing remembered yet',
        subtitle: 'Say "remember…" during a conversation, or tell the assistant a '
            'fact about yourself and it will be saved here.',
      );
    }
    return ListView.separated(
      itemCount: entries.length,
      separatorBuilder: (_, _) => const Divider(height: 1),
      itemBuilder: (context, i) {
        final e = entries[i];
        final isPreference = e.kind == 'preference';
        return ListTile(
          key: ValueKey('memory-${e.id}'),
          leading: Icon(isPreference ? Icons.favorite_outline : Icons.notes),
          title: Text(e.content),
          subtitle: Text('${_titleCase(e.kind)} · ${_titleCase(e.source)}'),
          trailing: IconButton(
            tooltip: 'Forget this',
            icon: const Icon(Icons.close),
            onPressed: () => _delete(e),
          ),
        );
      },
    );
  }

  String _titleCase(String s) =>
      s.isEmpty ? s : '${s[0].toUpperCase()}${s.substring(1)}';
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
