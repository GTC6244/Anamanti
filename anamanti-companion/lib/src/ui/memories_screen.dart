// Memories tab: the Core's persistent facts/preferences — list, delete one
// (swipe, confirmed), clear all (confirmed). Same mechanism as the Display's
// Memory screen: Wyoming control frames on the Core's Wyoming port, so this tab
// works even when the Core's companion HTTP API is off.

import 'package:flutter/material.dart';

import '../data/companion_api.dart';
import '../util/format.dart';
import 'common.dart';

class MemoriesScreen extends StatefulWidget {
  const MemoriesScreen({
    super.key,
    required this.api,
    required this.core,
    required this.active,
    this.onReachability,
  });

  final CompanionApi api;
  final CoreInfo core;
  final bool active;
  final void Function(bool ok)? onReachability;

  @override
  State<MemoriesScreen> createState() => _MemoriesScreenState();
}

class _MemoriesScreenState extends State<MemoriesScreen> {
  List<MemoryEntry>? _entries;
  String? _error;
  bool _busy = false;
  bool _loadedOnce = false;

  @override
  void initState() {
    super.initState();
    _maybeLoad();
  }

  @override
  void didUpdateWidget(MemoriesScreen old) {
    super.didUpdateWidget(old);
    if (old.core != widget.core) {
      _entries = null;
      _error = null;
      _loadedOnce = false;
    }
    _maybeLoad();
  }

  /// Memories change rarely and only by voice or here, so no polling: load the
  /// first time the tab is shown, then on pull-to-refresh.
  void _maybeLoad() {
    if (widget.active && !_loadedOnce) {
      _loadedOnce = true;
      _load();
    }
  }

  Future<void> _load() async {
    final core = widget.core;
    _busy = true; // no setState: this can run from initState
    try {
      final entries = await widget.api.listMemories(core);
      if (!mounted || core != widget.core) return;
      setState(() {
        _entries = entries;
        _error = null;
        _busy = false;
      });
      widget.onReachability?.call(true);
    } catch (e) {
      if (!mounted || core != widget.core) return;
      setState(() {
        _error = describeError(e);
        _busy = false;
      });
      widget.onReachability?.call(false);
    }
  }

  Future<bool> _confirmDelete(MemoryEntry entry) async {
    final ok = await _confirm(
      context,
      title: 'Forget this?',
      message: entry.content,
      confirmLabel: 'Forget',
    );
    if (!ok) return false;
    try {
      await widget.api.deleteMemory(widget.core, entry.id);
    } catch (e) {
      if (mounted) _snack('Delete failed: ${describeError(e)}');
      return false;
    }
    return true;
  }

  void _removed(MemoryEntry entry) {
    setState(() => _entries?.removeWhere((m) => m.id == entry.id));
    _snack('Forgotten');
  }

  Future<void> _clearAll() async {
    final ok = await _confirm(
      context,
      title: 'Forget everything?',
      message:
          'This permanently deletes all remembered facts and preferences on '
          'this Core.',
      confirmLabel: 'Forget all',
    );
    if (!ok) return;
    setState(() => _busy = true);
    try {
      final n = await widget.api.clearMemories(widget.core);
      if (!mounted) return;
      setState(() {
        _entries = <MemoryEntry>[];
        _busy = false;
      });
      _snack('Forgot $n ${n == 1 ? 'memory' : 'memories'}');
    } catch (e) {
      if (!mounted) return;
      setState(() => _busy = false);
      _snack('Clear failed: ${describeError(e)}');
    }
  }

  void _snack(String text) {
    ScaffoldMessenger.maybeOf(context)
      ?..hideCurrentSnackBar()
      ..showSnackBar(SnackBar(content: Text(text)));
  }

  @override
  Widget build(BuildContext context) {
    final entries = _entries;
    if (entries == null) {
      if (_error != null) {
        return MessagePanel(
          icon: Icons.cloud_off,
          title: 'Memory unavailable',
          message: _error,
          action: FilledButton(onPressed: _load, child: const Text('Retry')),
        );
      }
      return const Center(child: CircularProgressIndicator());
    }
    return Column(
      children: [
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 4, 8, 0),
          child: Row(
            children: [
              Expanded(
                child: Text(
                  entries.isEmpty
                      ? 'Nothing remembered'
                      : '${entries.length} remembered',
                  style: Theme.of(context).textTheme.titleSmall,
                ),
              ),
              if (entries.isNotEmpty)
                TextButton.icon(
                  key: const Key('memories-clear-all'),
                  onPressed: _busy ? null : _clearAll,
                  icon: const Icon(Icons.delete_sweep_outlined),
                  label: const Text('Forget all'),
                ),
            ],
          ),
        ),
        Expanded(
          child: RefreshIndicator(
            onRefresh: _load,
            child: entries.isEmpty
                ? const MessagePanel(
                    icon: Icons.psychology_outlined,
                    title: 'Nothing remembered yet',
                    message:
                        'Say "remember…" to the assistant, or tell it a fact '
                        'about yourself, and it will show up here.',
                  )
                : ListView.separated(
                    itemCount: entries.length,
                    separatorBuilder: (_, _) => const Divider(height: 1),
                    itemBuilder: (context, i) {
                      final e = entries[i];
                      return Dismissible(
                        key: ValueKey('memory-${e.id}'),
                        direction: DismissDirection.endToStart,
                        confirmDismiss: (_) => _confirmDelete(e),
                        onDismissed: (_) => _removed(e),
                        background: Container(
                          color: Theme.of(context).colorScheme.errorContainer,
                          alignment: Alignment.centerRight,
                          padding: const EdgeInsets.symmetric(horizontal: 24),
                          child: const Icon(Icons.delete_outline),
                        ),
                        child: ListTile(
                          leading: Icon(
                            e.kind == 'preference'
                                ? Icons.favorite_outline
                                : Icons.notes,
                          ),
                          title: Text(e.content),
                          subtitle: Text(
                            [
                              titleCase(e.kind),
                              titleCase(e.source),
                              formatDate(e.createdAt),
                            ].where((s) => s.isNotEmpty).join(' · '),
                          ),
                          trailing: IconButton(
                            tooltip: 'Forget this',
                            icon: const Icon(Icons.close),
                            onPressed: () async {
                              if (await _confirmDelete(e) && mounted) {
                                _removed(e);
                              }
                            },
                          ),
                        ),
                      );
                    },
                  ),
          ),
        ),
      ],
    );
  }
}

Future<bool> _confirm(
  BuildContext context, {
  required String title,
  required String message,
  required String confirmLabel,
}) async {
  final result = await showDialog<bool>(
    context: context,
    builder: (context) => AlertDialog(
      title: Text(title),
      content: Text(message),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(false),
          child: const Text('Cancel'),
        ),
        FilledButton(
          onPressed: () => Navigator.of(context).pop(true),
          child: Text(confirmLabel),
        ),
      ],
    ),
  );
  return result ?? false;
}
