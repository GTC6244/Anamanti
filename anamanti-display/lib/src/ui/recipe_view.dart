// Recipe-mode screen: a focused, glanceable cooking view for the 8-inch Echo Show.
//
// Shown when the orchestrator's `recipe_lookup` tool pushes a parsed recipe (see
// [AssistantState.recipe]). Three bottom tabs — Overview / Ingredients / Steps —
// switch a full-screen [IndexedStack]. Landscape-first with large, arm's-length type.
// The user leaves recipe mode by voice ("done cooking") or the close control, which
// calls [onClose].

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/recipe_data.dart';

class RecipeView extends StatefulWidget {
  const RecipeView({
    super.key,
    required this.recipe,
    required this.onClose,
    this.tab = 0,
    this.onTabSelected,
    this.scrollSeq = 0,
    this.scrollDir = '',
    this.onScrollPositionChanged,
  });

  final RecipeData recipe;
  final VoidCallback onClose;

  /// The active tab: 0 = Overview, 1 = Ingredients, 2 = Steps. Driven externally (by
  /// voice via the controller); a touch of the bottom bar reports through
  /// [onTabSelected] and comes back as a new [tab].
  final int tab;

  /// Called when the user taps a bottom-bar tab, so the controller can update its
  /// state (and push the new tab to the orchestrator as screen context).
  final ValueChanged<int>? onTabSelected;

  /// Bumped on each voice scroll command (paired with [scrollDir]); the view scrolls
  /// the active pane whenever it changes. 0 = no command yet.
  final int scrollSeq;

  /// Direction of the latest voice scroll command: `'up'` / `'down'` / `'top'` /
  /// `'bottom'`.
  final String scrollDir;

  /// Reports the active pane's scroll position (at top / at bottom) so the controller
  /// can tell the orchestrator whether a scroll command would do anything.
  final void Function(bool atTop, bool atBottom)? onScrollPositionChanged;

  @override
  State<RecipeView> createState() => _RecipeViewState();
}

class _RecipeViewState extends State<RecipeView> {
  int _tab = 0;
  late final List<ScrollController> _controllers;

  static const _bg = Color(0xFF14120E);
  static const _accent = Color(0xFFE8A33D);

  @override
  void initState() {
    super.initState();
    _tab = widget.tab;
    _controllers = List.generate(3, (_) => ScrollController());
    for (var i = 0; i < 3; i++) {
      final idx = i;
      _controllers[idx].addListener(() {
        if (idx == _tab) _reportPosition();
      });
    }
    // Report the initial pane position once it has been laid out (e.g. a short
    // Overview that fits fully → at top *and* bottom).
    WidgetsBinding.instance.addPostFrameCallback((_) => _reportPosition());
  }

  @override
  void dispose() {
    for (final c in _controllers) {
      c.dispose();
    }
    super.dispose();
  }

  @override
  void didUpdateWidget(RecipeView old) {
    super.didUpdateWidget(old);
    // A newly pushed recipe (different dish) resets to the Overview tab.
    final newDish =
        old.recipe.title != widget.recipe.title ||
        old.recipe.sourceUrl != widget.recipe.sourceUrl;
    if (newDish) {
      _tab = 0;
      WidgetsBinding.instance.addPostFrameCallback((_) => _reportPosition());
    } else if (widget.tab != old.tab && widget.tab != _tab) {
      // A voice/controller-driven tab switch.
      _tab = widget.tab;
      WidgetsBinding.instance.addPostFrameCallback((_) => _reportPosition());
    }
    // A new voice scroll command (the counter changed).
    if (widget.scrollSeq != old.scrollSeq && widget.scrollSeq > 0) {
      _applyScroll(widget.scrollDir);
    }
  }

  /// Scroll the active pane in response to a voice command. A page is ~85% of the
  /// viewport so a little context carries over; `top`/`bottom` jump to the ends.
  void _applyScroll(String dir) {
    final c = _controllers[_tab];
    if (!c.hasClients) return;
    final pos = c.position;
    final page = pos.viewportDimension * 0.85;
    final double target;
    switch (dir) {
      case 'up':
        target = pos.pixels - page;
      case 'down':
        target = pos.pixels + page;
      case 'top':
        target = pos.minScrollExtent;
      case 'bottom':
        target = pos.maxScrollExtent;
      default:
        return;
    }
    c.animateTo(
      target.clamp(pos.minScrollExtent, pos.maxScrollExtent),
      duration: const Duration(milliseconds: 300),
      curve: Curves.easeOutCubic,
    );
  }

  /// Report the active pane's top/bottom position to the controller.
  void _reportPosition() {
    final cb = widget.onScrollPositionChanged;
    if (cb == null || !mounted) return;
    final c = _controllers[_tab];
    if (!c.hasClients) {
      cb(true, false);
      return;
    }
    final pos = c.position;
    final atTop = pos.pixels <= pos.minScrollExtent + 1.0;
    final atBottom = pos.pixels >= pos.maxScrollExtent - 1.0;
    cb(atTop, atBottom);
  }

  void _selectTab(int i) {
    if (i != _tab) {
      setState(() => _tab = i);
      WidgetsBinding.instance.addPostFrameCallback((_) => _reportPosition());
    }
    widget.onTabSelected?.call(i);
  }

  @override
  Widget build(BuildContext context) {
    final r = widget.recipe;
    return Material(
      color: _bg,
      child: SafeArea(
        child: Column(
          children: [
            _header(r),
            Expanded(
              child: IndexedStack(
                index: _tab,
                sizing: StackFit.expand,
                children: [
                  _OverviewTab(
                    recipe: r,
                    accent: _accent,
                    controller: _controllers[0],
                  ),
                  _ListTab(
                    key: const Key('recipe-ingredients'),
                    items: r.ingredients,
                    accent: _accent,
                    numbered: false,
                    emptyLabel: 'No ingredients listed.',
                    controller: _controllers[1],
                  ),
                  _ListTab(
                    key: const Key('recipe-steps'),
                    items: r.steps,
                    accent: _accent,
                    numbered: true,
                    emptyLabel: 'No steps listed.',
                    controller: _controllers[2],
                  ),
                ],
              ),
            ),
            _tabBar(),
          ],
        ),
      ),
    );
  }

  Widget _header(RecipeData r) {
    final facts = <String>[
      if (r.servings.isNotEmpty) r.servings,
      if (r.totalTime.isNotEmpty) r.totalTime,
    ].join('  ·  ');
    return Padding(
      padding: const EdgeInsets.fromLTRB(28, 18, 16, 10),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  r.title,
                  maxLines: 2,
                  overflow: TextOverflow.ellipsis,
                  style: const TextStyle(
                    color: Colors.white,
                    fontSize: 34,
                    fontWeight: FontWeight.w600,
                    letterSpacing: 0.2,
                  ),
                ),
                if (facts.isNotEmpty)
                  Padding(
                    padding: const EdgeInsets.only(top: 4),
                    child: Text(
                      facts,
                      style: TextStyle(
                        color: _accent.withValues(alpha: 0.95),
                        fontSize: 16,
                        fontWeight: FontWeight.w500,
                      ),
                    ),
                  ),
              ],
            ),
          ),
          IconButton(
            key: const Key('recipe-close'),
            tooltip: 'Close recipe',
            onPressed: widget.onClose,
            iconSize: 32,
            icon: Icon(
              Icons.close,
              color: Colors.white.withValues(alpha: 0.85),
            ),
          ),
        ],
      ),
    );
  }

  Widget _tabBar() {
    const labels = ['Overview', 'Ingredients', 'Steps'];
    const icons = [
      Icons.article_outlined,
      Icons.egg_alt_outlined,
      Icons.format_list_numbered,
    ];
    return Container(
      decoration: BoxDecoration(
        color: Colors.black.withValues(alpha: 0.35),
        border: Border(
          top: BorderSide(color: Colors.white.withValues(alpha: 0.08)),
        ),
      ),
      child: Row(
        children: List.generate(3, (i) {
          final selected = i == _tab;
          return Expanded(
            child: InkWell(
              key: Key('recipe-tab-$i'),
              onTap: () => _selectTab(i),
              child: Padding(
                padding: const EdgeInsets.symmetric(vertical: 12),
                child: Column(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Icon(
                      icons[i],
                      size: 26,
                      color: selected
                          ? _accent
                          : Colors.white.withValues(alpha: 0.55),
                    ),
                    const SizedBox(height: 4),
                    Text(
                      labels[i],
                      style: TextStyle(
                        color: selected
                            ? _accent
                            : Colors.white.withValues(alpha: 0.55),
                        fontSize: 14,
                        fontWeight: selected
                            ? FontWeight.w600
                            : FontWeight.w400,
                      ),
                    ),
                  ],
                ),
              ),
            ),
          );
        }),
      ),
    );
  }
}

class _OverviewTab extends StatelessWidget {
  const _OverviewTab({
    required this.recipe,
    required this.accent,
    this.controller,
  });

  final RecipeData recipe;
  final Color accent;
  final ScrollController? controller;

  @override
  Widget build(BuildContext context) {
    final r = recipe;
    return ListView(
      controller: controller,
      padding: const EdgeInsets.fromLTRB(28, 8, 28, 24),
      children: [
        if (r.imageUrl.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(bottom: 18),
            child: ClipRRect(
              borderRadius: BorderRadius.circular(14),
              child: AspectRatio(
                aspectRatio: 16 / 9,
                child: Image.network(
                  r.imageUrl,
                  fit: BoxFit.cover,
                  // A broken/slow image never blanks the screen.
                  errorBuilder: (_, _, _) =>
                      Container(color: Colors.white.withValues(alpha: 0.05)),
                  loadingBuilder: (context, child, progress) => progress == null
                      ? child
                      : Container(color: Colors.white.withValues(alpha: 0.05)),
                ),
              ),
            ),
          ),
        if (r.summary.isNotEmpty)
          Text(
            r.summary,
            style: TextStyle(
              color: Colors.white.withValues(alpha: 0.9),
              fontSize: 20,
              height: 1.4,
            ),
          ),
        const SizedBox(height: 18),
        Wrap(
          spacing: 24,
          runSpacing: 10,
          children: [
            if (r.servings.isNotEmpty)
              _fact(Icons.people_alt_outlined, r.servings),
            if (r.totalTime.isNotEmpty) _fact(Icons.schedule, r.totalTime),
            _fact(
              Icons.egg_alt_outlined,
              '${r.ingredients.length} ingredients',
            ),
            _fact(Icons.format_list_numbered, '${r.steps.length} steps'),
          ],
        ),
        if (r.sourceUrl.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(top: 22),
            child: Text(
              'Source: ${r.sourceUrl}',
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: TextStyle(
                color: Colors.white.withValues(alpha: 0.4),
                fontSize: 13,
              ),
            ),
          ),
      ],
    );
  }

  Widget _fact(IconData icon, String label) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Icon(icon, size: 20, color: accent),
        const SizedBox(width: 8),
        Text(
          label,
          style: const TextStyle(
            color: Colors.white,
            fontSize: 17,
            fontWeight: FontWeight.w500,
          ),
        ),
      ],
    );
  }
}

/// The Ingredients / Steps tab: a large, scrollable list. `numbered` renders step
/// numbers; otherwise a bullet accent for ingredients.
class _ListTab extends StatelessWidget {
  const _ListTab({
    super.key,
    required this.items,
    required this.accent,
    required this.numbered,
    required this.emptyLabel,
    this.controller,
  });

  final List<String> items;
  final Color accent;
  final bool numbered;
  final String emptyLabel;
  final ScrollController? controller;

  @override
  Widget build(BuildContext context) {
    if (items.isEmpty) {
      return Center(
        child: Text(
          emptyLabel,
          style: TextStyle(
            color: Colors.white.withValues(alpha: 0.5),
            fontSize: 18,
          ),
        ),
      );
    }
    return ListView.separated(
      controller: controller,
      padding: const EdgeInsets.fromLTRB(28, 12, 28, 24),
      itemCount: items.length,
      separatorBuilder: (_, _) => const SizedBox(height: 14),
      itemBuilder: (context, i) {
        return Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            SizedBox(
              width: 34,
              child: numbered
                  ? Text(
                      '${i + 1}',
                      style: TextStyle(
                        color: accent,
                        fontSize: 22,
                        fontWeight: FontWeight.w700,
                      ),
                    )
                  : Padding(
                      padding: const EdgeInsets.only(top: 8),
                      child: Icon(Icons.circle, size: 9, color: accent),
                    ),
            ),
            Expanded(
              child: Text(
                items[i],
                style: const TextStyle(
                  color: Colors.white,
                  fontSize: 21,
                  height: 1.35,
                ),
              ),
            ),
          ],
        );
      },
    );
  }
}
