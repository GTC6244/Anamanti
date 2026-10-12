// Memory management screen (Plan.MD §3, Phase 6: "view/delete stored items").
//
// A tabbed window onto everything the Core remembers:
//   • Facts        — the durable SQLite facts/preferences (explicit + inferred).
//   • Conversations — stored GraphRAG `Turn` nodes (what was said + the reply + the
//                     keywords that turn mentions), searchable and filterable by
//                     person or keyword.
//   • Keywords      — the GraphRAG `Entity` nodes (people/places/things/topics).
//
// Each tab lists its items and lets the user delete them. The voice half of memory
// management ("remember…", "forget that") is handled on the Mac during a turn; this
// is the settings-list half. Deletes on the Conversations/Keywords tabs are
// Helix-index-only (the chat-log source of truth is untouched), so a deleted turn can
// reappear if the embedding index is rebuilt — surfaced as a caption on those tabs.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/settings/orchestrator_client.dart';

class MemoryScreen extends StatefulWidget {
  const MemoryScreen({super.key, required this.client});

  final OrchestratorClient client;

  @override
  State<MemoryScreen> createState() => _MemoryScreenState();
}

class _MemoryScreenState extends State<MemoryScreen>
    with SingleTickerProviderStateMixin {
  late final TabController _tabs = TabController(length: 3, vsync: this);

  /// When the user taps a keyword, we jump to Conversations pre-filtered to turns
  /// that mention it. Empty = no entity filter.
  String _entityFilter = '';

  void _browseEntity(String name) {
    setState(() => _entityFilter = name);
    _tabs.animateTo(1);
  }

  @override
  void dispose() {
    _tabs.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('Memory'),
        bottom: TabBar(
          controller: _tabs,
          tabs: const [
            Tab(text: 'Facts'),
            Tab(text: 'Conversations'),
            Tab(text: 'Keywords'),
          ],
        ),
      ),
      body: TabBarView(
        controller: _tabs,
        children: [
          _FactsTab(client: widget.client),
          _ConversationsTab(
            client: widget.client,
            entityFilter: _entityFilter,
            onClearEntity: () => setState(() => _entityFilter = ''),
          ),
          _KeywordsTab(
            client: widget.client,
            onBrowseEntity: _browseEntity,
          ),
        ],
      ),
    );
  }
}

// ---------------------------------------------------------------------------
// Facts tab — durable SQLite facts/preferences.
// ---------------------------------------------------------------------------

class _FactsTab extends StatefulWidget {
  const _FactsTab({required this.client});

  final OrchestratorClient client;

  @override
  State<_FactsTab> createState() => _FactsTabState();
}

class _FactsTabState extends State<_FactsTab>
    with AutomaticKeepAliveClientMixin {
  List<MemoryView>? _entries;
  String? _error;
  bool _busy = false;

  @override
  bool get wantKeepAlive => true;

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
      _snack(context, 'Delete failed: $e');
      return;
    }
    if (!mounted) return;
    setState(() => _entries?.removeWhere((m) => m.id == entry.id));
  }

  Future<void> _clearAll() async {
    final confirmed = await _confirm(
      context,
      title: 'Forget everything?',
      message: 'This permanently deletes all remembered facts and preferences.',
      confirmLabel: 'Forget all',
    );
    if (confirmed != true) return;
    try {
      await widget.client.clearMemories();
    } catch (e) {
      if (!mounted) return;
      _snack(context, 'Clear failed: $e');
      return;
    }
    if (!mounted) return;
    setState(() => _entries = <MemoryView>[]);
  }

  @override
  Widget build(BuildContext context) {
    super.build(context);
    final entries = _entries;
    return Column(
      children: [
        _toolbar(
          children: [
            const Spacer(),
            IconButton(
              tooltip: 'Refresh',
              onPressed: _busy ? null : _reload,
              icon: const Icon(Icons.refresh),
            ),
            if (entries != null && entries.isNotEmpty)
              IconButton(
                key: const Key('memory-facts-forget-all'),
                tooltip: 'Forget all',
                onPressed: _busy ? null : _clearAll,
                icon: const Icon(Icons.delete_sweep),
              ),
          ],
        ),
        Expanded(child: _body(entries)),
      ],
    );
  }

  Widget _body(List<MemoryView>? entries) {
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
}

// ---------------------------------------------------------------------------
// Conversations tab — GraphRAG Turn nodes.
// ---------------------------------------------------------------------------

class _ConversationsTab extends StatefulWidget {
  const _ConversationsTab({
    required this.client,
    required this.entityFilter,
    required this.onClearEntity,
  });

  final OrchestratorClient client;

  /// When non-empty, only show turns mentioning this entity (set when the user
  /// taps a keyword on the Keywords tab).
  final String entityFilter;

  /// Clear the entity filter (the user dismissed the chip).
  final VoidCallback onClearEntity;

  @override
  State<_ConversationsTab> createState() => _ConversationsTabState();
}

class _ConversationsTabState extends State<_ConversationsTab>
    with AutomaticKeepAliveClientMixin {
  final TextEditingController _search = TextEditingController();
  List<TurnView>? _turns;
  List<SpeakerView> _speakers = const [];
  String _speakerId = ''; // empty = Everyone
  String? _error;
  bool _busy = false;

  @override
  bool get wantKeepAlive => true;

  @override
  void initState() {
    super.initState();
    _loadSpeakers();
    _reload();
  }

  @override
  void didUpdateWidget(_ConversationsTab old) {
    super.didUpdateWidget(old);
    // A keyword tap on the other tab changed the entity filter — reload.
    if (old.entityFilter != widget.entityFilter) _reload();
  }

  @override
  void dispose() {
    _search.dispose();
    super.dispose();
  }

  Future<void> _loadSpeakers() async {
    try {
      final people = await widget.client.listSpeakers();
      if (!mounted) return;
      setState(() => _speakers = people);
    } catch (_) {
      // Non-fatal: the person filter just stays as "Everyone".
    }
  }

  Future<void> _reload() async {
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      final turns = await widget.client.listTurns(
        query: _search.text.trim(),
        speakerId: _speakerId,
        entity: widget.entityFilter,
      );
      if (!mounted) return;
      setState(() {
        _turns = turns;
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

  Future<void> _delete(TurnView turn) async {
    final confirmed = await _confirm(
      context,
      title: 'Delete this conversation?',
      message: 'Removes this turn from what the assistant recalls.',
      confirmLabel: 'Delete',
    );
    if (confirmed != true) return;
    try {
      await widget.client.deleteGraphNode(turn.id);
    } catch (e) {
      if (!mounted) return;
      _snack(context, 'Delete failed: $e');
      return;
    }
    if (!mounted) return;
    setState(() => _turns?.removeWhere((t) => t.id == turn.id));
  }

  String _speakerLabel(String id) {
    if (id.isEmpty || id == 'household') return 'Shared';
    for (final s in _speakers) {
      if (s.id == id) return s.name ?? _shortId(id);
    }
    return _shortId(id);
  }

  @override
  Widget build(BuildContext context) {
    super.build(context);
    final turns = _turns;
    return Column(
      children: [
        _toolbar(
          children: [
            Expanded(
              child: TextField(
                key: const Key('memory-turns-search'),
                controller: _search,
                textInputAction: TextInputAction.search,
                onSubmitted: (_) => _reload(),
                decoration: InputDecoration(
                  isDense: true,
                  hintText: 'Search conversations',
                  prefixIcon: const Icon(Icons.search),
                  border: const OutlineInputBorder(),
                  suffixIcon: _search.text.isEmpty
                      ? null
                      : IconButton(
                          icon: const Icon(Icons.clear),
                          onPressed: () {
                            _search.clear();
                            _reload();
                          },
                        ),
                ),
              ),
            ),
            const SizedBox(width: 8),
            _speakerDropdown(),
            IconButton(
              tooltip: 'Refresh',
              onPressed: _busy ? null : _reload,
              icon: const Icon(Icons.refresh),
            ),
          ],
        ),
        if (widget.entityFilter.isNotEmpty)
          Align(
            alignment: Alignment.centerLeft,
            child: Padding(
              padding: const EdgeInsets.fromLTRB(12, 0, 12, 4),
              child: InputChip(
                key: const Key('memory-turns-entity-chip'),
                label: Text('Mentions: ${widget.entityFilter}'),
                onDeleted: widget.onClearEntity,
              ),
            ),
          ),
        Expanded(child: _body(turns)),
      ],
    );
  }

  Widget _speakerDropdown() {
    return DropdownButton<String>(
      key: const Key('memory-turns-speaker'),
      value: _speakerId,
      onChanged: (v) {
        setState(() => _speakerId = v ?? '');
        _reload();
      },
      items: [
        const DropdownMenuItem(value: '', child: Text('Everyone')),
        for (final s in _speakers)
          DropdownMenuItem(value: s.id, child: Text(s.name ?? _shortId(s.id))),
      ],
    );
  }

  Widget _body(List<TurnView>? turns) {
    if (_busy && turns == null) {
      return const Center(child: CircularProgressIndicator());
    }
    if (_error != null) {
      return _Centered(
        icon: Icons.cloud_off,
        title: 'Conversations unavailable',
        subtitle: _error!,
        action: FilledButton(onPressed: _reload, child: const Text('Retry')),
      );
    }
    if (turns == null || turns.isEmpty) {
      return const _Centered(
        icon: Icons.forum_outlined,
        title: 'No conversations',
        subtitle: 'Turns you have with the assistant show up here once they are '
            'indexed for recall.',
      );
    }
    return ListView.separated(
      itemCount: turns.length + 1,
      separatorBuilder: (_, _) => const Divider(height: 1),
      itemBuilder: (context, i) {
        if (i == turns.length) return const _RebuildCaption();
        final t = turns[i];
        return ListTile(
          key: ValueKey('memory-turn-${t.id}'),
          isThreeLine: true,
          title: Text(t.transcript.isEmpty ? '(no transcript)' : t.transcript),
          subtitle: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              if (t.reply.isNotEmpty) Text(t.reply),
              const SizedBox(height: 4),
              Text(
                '${_speakerLabel(t.speakerId)} · ${_relativeTime(t.ts)}',
                style: Theme.of(context).textTheme.labelSmall,
              ),
              if (t.entities.isNotEmpty)
                Padding(
                  padding: const EdgeInsets.only(top: 6),
                  child: Wrap(
                    spacing: 6,
                    runSpacing: -6,
                    children: [
                      for (final e in t.entities)
                        Chip(
                          label: Text(e.name),
                          visualDensity: VisualDensity.compact,
                          materialTapTargetSize:
                              MaterialTapTargetSize.shrinkWrap,
                        ),
                    ],
                  ),
                ),
            ],
          ),
          trailing: IconButton(
            tooltip: 'Delete this',
            icon: const Icon(Icons.close),
            onPressed: () => _delete(t),
          ),
        );
      },
    );
  }
}

// ---------------------------------------------------------------------------
// Keywords tab — GraphRAG Entity nodes.
// ---------------------------------------------------------------------------

class _KeywordsTab extends StatefulWidget {
  const _KeywordsTab({required this.client, required this.onBrowseEntity});

  final OrchestratorClient client;

  /// Tap a keyword → jump to Conversations filtered to turns mentioning it.
  final ValueChanged<String> onBrowseEntity;

  @override
  State<_KeywordsTab> createState() => _KeywordsTabState();
}

class _KeywordsTabState extends State<_KeywordsTab>
    with AutomaticKeepAliveClientMixin {
  final TextEditingController _search = TextEditingController();
  List<EntityView>? _entities;
  String? _error;
  bool _busy = false;

  @override
  bool get wantKeepAlive => true;

  @override
  void initState() {
    super.initState();
    _reload();
  }

  @override
  void dispose() {
    _search.dispose();
    super.dispose();
  }

  Future<void> _reload() async {
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      final entities =
          await widget.client.listEntities(query: _search.text.trim());
      if (!mounted) return;
      setState(() {
        _entities = entities;
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

  Future<void> _delete(EntityView entity) async {
    final confirmed = await _confirm(
      context,
      title: 'Delete "${entity.name}"?',
      message: 'Removes this keyword and its links to conversations. The '
          'conversations themselves stay.',
      confirmLabel: 'Delete',
    );
    if (confirmed != true) return;
    try {
      await widget.client.deleteGraphNode(entity.id);
    } catch (e) {
      if (!mounted) return;
      _snack(context, 'Delete failed: $e');
      return;
    }
    if (!mounted) return;
    setState(() => _entities?.removeWhere((e) => e.id == entity.id));
  }

  @override
  Widget build(BuildContext context) {
    super.build(context);
    final entities = _entities;
    return Column(
      children: [
        _toolbar(
          children: [
            Expanded(
              child: TextField(
                key: const Key('memory-keywords-search'),
                controller: _search,
                textInputAction: TextInputAction.search,
                onSubmitted: (_) => _reload(),
                decoration: InputDecoration(
                  isDense: true,
                  hintText: 'Search keywords',
                  prefixIcon: const Icon(Icons.search),
                  border: const OutlineInputBorder(),
                  suffixIcon: _search.text.isEmpty
                      ? null
                      : IconButton(
                          icon: const Icon(Icons.clear),
                          onPressed: () {
                            _search.clear();
                            _reload();
                          },
                        ),
                ),
              ),
            ),
            IconButton(
              tooltip: 'Refresh',
              onPressed: _busy ? null : _reload,
              icon: const Icon(Icons.refresh),
            ),
          ],
        ),
        Expanded(child: _body(entities)),
      ],
    );
  }

  Widget _body(List<EntityView>? entities) {
    if (_busy && entities == null) {
      return const Center(child: CircularProgressIndicator());
    }
    if (_error != null) {
      return _Centered(
        icon: Icons.cloud_off,
        title: 'Keywords unavailable',
        subtitle: _error!,
        action: FilledButton(onPressed: _reload, child: const Text('Retry')),
      );
    }
    if (entities == null || entities.isEmpty) {
      return const _Centered(
        icon: Icons.label_outline,
        title: 'No keywords',
        subtitle: 'People, places, and topics the assistant picks out of your '
            'conversations show up here.',
      );
    }
    return ListView.separated(
      itemCount: entities.length + 1,
      separatorBuilder: (_, _) => const Divider(height: 1),
      itemBuilder: (context, i) {
        if (i == entities.length) return const _RebuildCaption();
        final e = entities[i];
        return ListTile(
          key: ValueKey('memory-entity-${e.id}'),
          leading: Icon(_entityIcon(e.kind)),
          title: Text(e.name),
          subtitle: e.kind.isEmpty ? null : Text(_titleCase(e.kind)),
          onTap: () => widget.onBrowseEntity(e.name),
          trailing: IconButton(
            tooltip: 'Delete this',
            icon: const Icon(Icons.close),
            onPressed: () => _delete(e),
          ),
        );
      },
    );
  }
}

// ---------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------

/// A thin padded toolbar row atop each tab (search / filters / actions).
Widget _toolbar({required List<Widget> children}) {
  return Padding(
    padding: const EdgeInsets.fromLTRB(12, 8, 4, 4),
    child: Row(children: children),
  );
}

/// A muted caption explaining that Helix-index-only deletes may reappear on a
/// rebuild. Rendered as the last row of the Conversations/Keywords lists.
class _RebuildCaption extends StatelessWidget {
  const _RebuildCaption();

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 12, 16, 20),
      child: Text(
        'Deletes here prune what the assistant recalls. Rebuilding the memory '
        'index (e.g. switching the embedding model) can restore items from the '
        'conversation log.',
        style: Theme.of(context).textTheme.bodySmall?.copyWith(
              color: Theme.of(context).colorScheme.outline,
            ),
        textAlign: TextAlign.center,
      ),
    );
  }
}

Future<bool?> _confirm(
  BuildContext context, {
  required String title,
  required String message,
  required String confirmLabel,
}) {
  return showDialog<bool>(
    context: context,
    builder: (context) => AlertDialog(
      title: Text(title),
      content: Text(message),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context, false),
          child: const Text('Cancel'),
        ),
        FilledButton(
          onPressed: () => Navigator.pop(context, true),
          child: Text(confirmLabel),
        ),
      ],
    ),
  );
}

void _snack(BuildContext context, String message) {
  ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(message)));
}

String _titleCase(String s) =>
    s.isEmpty ? s : '${s[0].toUpperCase()}${s.substring(1)}';

String _shortId(String id) => id.length <= 10 ? id : '${id.substring(0, 10)}…';

IconData _entityIcon(String kind) {
  switch (kind) {
    case 'person':
      return Icons.person_outline;
    case 'place':
      return Icons.place_outlined;
    case 'thing':
      return Icons.category_outlined;
    case 'topic':
      return Icons.tag;
    default:
      return Icons.label_outline;
  }
}

/// A short "x minutes/hours/days ago" from a unix-seconds timestamp.
String _relativeTime(int tsSeconds) {
  if (tsSeconds <= 0) return 'unknown time';
  final then = DateTime.fromMillisecondsSinceEpoch(tsSeconds * 1000);
  final d = DateTime.now().difference(then);
  if (d.inSeconds < 60) return 'just now';
  if (d.inMinutes < 60) return '${d.inMinutes}m ago';
  if (d.inHours < 24) return '${d.inHours}h ago';
  if (d.inDays < 7) return '${d.inDays}d ago';
  return '${then.year}-${then.month.toString().padLeft(2, '0')}-'
      '${then.day.toString().padLeft(2, '0')}';
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
