// Shared building blocks for the Settings sections (plans/SettingsReorgPlan.md §3.4):
// compact single-line rows sized so every tab fits the Echo Show's 960×600 dp
// landscape screen in two columns without scrolling.

import 'package:flutter/material.dart';

/// A tab's content area: two columns side by side, with an optional full-width
/// [header] (e.g. an offline banner) above. Wrapped in a scroll view purely as a
/// safety net — the tabs are laid out to fit, and a test asserts they don't scroll.
class SettingsTabBody extends StatelessWidget {
  const SettingsTabBody({
    super.key,
    this.header,
    required this.left,
    this.right = const <Widget>[],
  });

  final Widget? header;
  final List<Widget> left;
  final List<Widget> right;

  @override
  Widget build(BuildContext context) {
    return SingleChildScrollView(
      padding: const EdgeInsets.fromLTRB(8, 8, 8, 8),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          ?header,
          Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: left,
                ),
              ),
              const SizedBox(width: 16),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: right,
                ),
              ),
            ],
          ),
        ],
      ),
    );
  }
}

/// A small uppercase group header inside a column.
class SettingsHeader extends StatelessWidget {
  const SettingsHeader(this.title, {super.key, this.caption});

  final String title;

  /// Optional one-line explanation under the header.
  final String? caption;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(12, 8, 12, 4),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            title.toUpperCase(),
            style: theme.textTheme.labelMedium?.copyWith(
              letterSpacing: 1.1,
              color: theme.colorScheme.primary,
            ),
          ),
          if (caption != null)
            Padding(
              padding: const EdgeInsets.only(top: 2),
              child: Text(
                caption!,
                style: theme.textTheme.bodySmall?.copyWith(color: Colors.grey),
              ),
            ),
        ],
      ),
    );
  }
}

/// One labelled slider row: `label | slider | value`, 48 dp tall.
///
/// [onChanged] fires during the drag (update the UI / live-tune without
/// committing); [onChangeEnd] fires on release (commit).
class SettingsSlider extends StatelessWidget {
  const SettingsSlider({
    super.key,
    required this.label,
    required this.value,
    required this.min,
    required this.max,
    required this.divisions,
    required this.format,
    required this.onChanged,
    this.onChangeEnd,
    this.sliderKey,
    this.enabled = true,
  });

  final String label;
  final double value;
  final double min;
  final double max;
  final int divisions;
  final String Function(double) format;
  final ValueChanged<double> onChanged;
  final ValueChanged<double>? onChangeEnd;
  final Key? sliderKey;
  final bool enabled;

  @override
  Widget build(BuildContext context) {
    final style = enabled ? null : const TextStyle(color: Colors.grey);
    return SizedBox(
      height: 48,
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 12),
        child: Row(
          children: [
            SizedBox(width: 150, child: Text(label, style: style)),
            Expanded(
              child: Slider(
                key: sliderKey,
                min: min,
                max: max,
                divisions: divisions,
                value: value.clamp(min, max),
                label: format(value),
                onChanged: enabled ? onChanged : null,
                onChangeEnd: enabled ? onChangeEnd : null,
              ),
            ),
            SizedBox(
              width: 60,
              child: Text(
                format(value),
                textAlign: TextAlign.end,
                style: style,
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// A compact single-line switch row. Long explanations live behind an ⓘ button
/// ([info]) that opens a small dialog, instead of a multi-line subtitle.
class SettingsSwitch extends StatelessWidget {
  const SettingsSwitch({
    super.key,
    required this.title,
    required this.value,
    required this.onChanged,
    this.icon,
    this.info,
    this.enabled = true,
  });

  final String title;
  final bool value;
  final ValueChanged<bool> onChanged;
  final IconData? icon;
  final String? info;
  final bool enabled;

  @override
  Widget build(BuildContext context) {
    return SwitchListTile(
      dense: true,
      contentPadding: const EdgeInsets.symmetric(horizontal: 12),
      secondary: icon == null ? null : Icon(icon),
      title: Row(
        children: [
          Flexible(child: Text(title, overflow: TextOverflow.ellipsis)),
          if (info != null) InfoButton(title: title, text: info!),
        ],
      ),
      value: value,
      onChanged: enabled ? onChanged : null,
    );
  }
}

/// The ⓘ button: shows [text] in a small dialog titled [title].
class InfoButton extends StatelessWidget {
  const InfoButton({super.key, required this.title, required this.text});

  final String title;
  final String text;

  @override
  Widget build(BuildContext context) {
    return IconButton(
      icon: const Icon(Icons.info_outline, size: 18),
      tooltip: 'About $title',
      visualDensity: VisualDensity.compact,
      onPressed: () => showDialog<void>(
        context: context,
        builder: (ctx) => AlertDialog(
          title: Text(title),
          content: Text(text),
          actions: [
            TextButton(
              onPressed: () => Navigator.of(ctx).pop(),
              child: const Text('OK'),
            ),
          ],
        ),
      ),
    );
  }
}

/// A labelled row with an arbitrary trailing control (dropdown, segmented button…).
class SettingsRow extends StatelessWidget {
  const SettingsRow({
    super.key,
    required this.title,
    required this.trailing,
    this.icon,
    this.subtitle,
  });

  final String title;
  final Widget trailing;
  final IconData? icon;
  final String? subtitle;

  @override
  Widget build(BuildContext context) {
    return ListTile(
      dense: true,
      contentPadding: const EdgeInsets.symmetric(horizontal: 12),
      leading: icon == null ? null : Icon(icon),
      title: Text(title),
      subtitle: subtitle == null ? null : Text(subtitle!),
      trailing: trailing,
    );
  }
}

/// A text field that applies its value only when the user is done: on the keyboard's
/// submit action or when it loses focus, never on every keystroke. Each keystroke
/// is reported through [onChanged] as an uncommitted draft (so Settings can still
/// apply it if it closes mid-edit — see `SettingsController.flush`); [onCommit]
/// marks the edit finished. External changes to [text] are adopted while the field
/// isn't being edited.
class CommitTextField extends StatefulWidget {
  const CommitTextField({
    super.key,
    required this.text,
    required this.onChanged,
    required this.onCommit,
    this.label,
    this.hint,
    this.helper,
    this.keyboardType,
    this.textAlign = TextAlign.start,
    this.textCapitalization = TextCapitalization.none,
    this.enabled = true,
    this.border = false,
  });

  final String text;
  final ValueChanged<String> onChanged;
  final VoidCallback onCommit;
  final String? label;
  final String? hint;
  final String? helper;
  final TextInputType? keyboardType;
  final TextAlign textAlign;
  final TextCapitalization textCapitalization;
  final bool enabled;
  final bool border;

  @override
  State<CommitTextField> createState() => _CommitTextFieldState();
}

class _CommitTextFieldState extends State<CommitTextField> {
  late final TextEditingController _controller = TextEditingController(
    text: widget.text,
  );
  final FocusNode _focus = FocusNode();
  bool _dirty = false;

  @override
  void initState() {
    super.initState();
    _focus.addListener(() {
      if (!_focus.hasFocus) _commit();
    });
  }

  @override
  void didUpdateWidget(CommitTextField old) {
    super.didUpdateWidget(old);
    if (widget.text != _controller.text && !_focus.hasFocus && !_dirty) {
      _controller.text = widget.text;
    }
  }

  void _commit() {
    if (!_dirty) return;
    _dirty = false;
    widget.onCommit();
  }

  @override
  void dispose() {
    _focus.dispose();
    _controller.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return TextField(
      controller: _controller,
      focusNode: _focus,
      enabled: widget.enabled,
      keyboardType: widget.keyboardType,
      textAlign: widget.textAlign,
      textCapitalization: widget.textCapitalization,
      autocorrect: false,
      onChanged: (v) {
        _dirty = true;
        widget.onChanged(v);
      },
      onSubmitted: (_) => _commit(),
      decoration: InputDecoration(
        labelText: widget.label,
        hintText: widget.hint,
        helperText: widget.helper,
        isDense: true,
        border: widget.border ? const OutlineInputBorder() : null,
      ),
    );
  }
}

/// "Contacting the assistant…" / "Assistant offline · Retry" banner for tabs whose
/// controls need the Core. Renders nothing once loaded.
class RemoteStatusBanner extends StatelessWidget {
  const RemoteStatusBanner({
    super.key,
    required this.loading,
    required this.error,
    required this.onRetry,
  });

  final bool loading;
  final String? error;
  final VoidCallback onRetry;

  @override
  Widget build(BuildContext context) {
    if (loading) {
      return const ListTile(
        dense: true,
        leading: SizedBox(
          width: 20,
          height: 20,
          child: CircularProgressIndicator(strokeWidth: 2),
        ),
        title: Text('Contacting the assistant…'),
      );
    }
    if (error != null) {
      return ListTile(
        dense: true,
        leading: const Icon(Icons.cloud_off),
        title: const Text('Assistant offline'),
        subtitle: const Text('These settings need the Mac to be reachable.'),
        trailing: TextButton(onPressed: onRetry, child: const Text('Retry')),
      );
    }
    return const SizedBox.shrink();
  }
}

/// A compact status chip: a colored icon + label.
class StatusChip extends StatelessWidget {
  const StatusChip({
    super.key,
    required this.label,
    required this.color,
    required this.icon,
  });

  final String label;
  final Color color;
  final IconData icon;

  @override
  Widget build(BuildContext context) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Icon(icon, color: color, size: 16),
        const SizedBox(width: 4),
        Text(label, style: TextStyle(color: color, fontSize: 13)),
      ],
    );
  }
}
