// Phone-pairing screen ("Link a phone").
//
// Matches a 4-character AppSaid pairing code — shown on a family member's phone in
// the AppSaid app — to a person, so the assistant can push messages / map links to
// that phone via the `send_phone_message` tool. The device never talks to AppSaid
// directly: it sends the name + code to the Core over a control round-trip
// (`anamanti-pair-phone` → `anamanti-pair-result`); the Core redeems the code
// (`POST /v1/pair/claim`), learns the phone's `user_key`, and stores it as a
// recipient under the name. The claimed key never comes back to the device.
//
// Enter a name and the code, submit, and the screen shows the Core's confirmation or
// the reason it was rejected (bad/expired code, or the Mac unreachable).

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'package:anamanti_display/src/settings/orchestrator_client.dart';

/// AppSaid pairing codes are 4 characters.
const int kPairCodeLength = 4;

class PairPhoneScreen extends StatefulWidget {
  const PairPhoneScreen({super.key, required this.client});

  /// Control-plane client that round-trips the pairing request to the Core.
  final OrchestratorClient client;

  @override
  State<PairPhoneScreen> createState() => _PairPhoneScreenState();
}

class _PairPhoneScreenState extends State<PairPhoneScreen> {
  final TextEditingController _nameCtl = TextEditingController();
  final TextEditingController _codeCtl = TextEditingController();

  bool _busy = false;
  String _error = '';
  String? _doneMessage;

  @override
  void dispose() {
    _nameCtl.dispose();
    _codeCtl.dispose();
    super.dispose();
  }

  /// The code with spaces/dashes stripped and upper-cased — the Core normalizes the
  /// same way, so "ab 2k" / "ab-2k" link the same as "AB2K".
  String get _normalizedCode =>
      _codeCtl.text.replaceAll(RegExp(r'[\s-]'), '').toUpperCase();

  bool get _canSubmit =>
      !_busy &&
      _nameCtl.text.trim().isNotEmpty &&
      _normalizedCode.length == kPairCodeLength;

  Future<void> _link() async {
    if (!_canSubmit) return;
    final name = _nameCtl.text.trim();
    final code = _normalizedCode;
    setState(() {
      _busy = true;
      _error = '';
    });
    try {
      final message = await widget.client.pairPhone(name, code);
      if (!mounted) return;
      setState(() {
        _busy = false;
        _doneMessage = message.isEmpty ? "Linked $name's phone." : message;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() {
        _busy = false;
        _error = _reason(e);
      });
    }
  }

  /// Strip the `Exception: ` prefix Dart prepends so the user sees just the reason.
  String _reason(Object e) {
    final s = e.toString();
    return s.startsWith('Exception: ') ? s.substring('Exception: '.length) : s;
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Link a phone')),
      body: SafeArea(
        child: Padding(
          padding: const EdgeInsets.all(20),
          child: _doneMessage != null ? _doneStep() : _formStep(),
        ),
      ),
    );
  }

  Widget _formStep() {
    final theme = Theme.of(context);
    return ListView(
      children: [
        Text('Link a phone', style: theme.textTheme.headlineSmall),
        const SizedBox(height: 8),
        Text(
          "On the person's phone, open the AppSaid app and tap "
          '"Link another device" to show a short 4-character code. Enter their '
          'name and that code here — they\'ll be added to your household so the '
          'assistant can text them.',
          style: theme.textTheme.bodyMedium,
        ),
        const SizedBox(height: 20),
        TextField(
          key: const Key('pair-name'),
          controller: _nameCtl,
          autofocus: true,
          textCapitalization: TextCapitalization.words,
          textInputAction: TextInputAction.next,
          enabled: !_busy,
          decoration: const InputDecoration(
            labelText: 'Name',
            hintText: 'e.g. Mom',
            border: OutlineInputBorder(),
          ),
          onChanged: (_) => setState(() {}),
        ),
        const SizedBox(height: 16),
        TextField(
          key: const Key('pair-code'),
          controller: _codeCtl,
          textCapitalization: TextCapitalization.characters,
          autocorrect: false,
          enableSuggestions: false,
          maxLength: kPairCodeLength,
          enabled: !_busy,
          inputFormatters: [
            // Codes are unambiguous alphanumerics (no 0/O/1/I); upper-case as typed.
            FilteringTextInputFormatter.allow(RegExp(r'[A-Za-z0-9]')),
            _UpperCaseFormatter(),
          ],
          decoration: const InputDecoration(
            labelText: 'Pairing code',
            hintText: 'e.g. AB2K',
            border: OutlineInputBorder(),
          ),
          onChanged: (_) => setState(() {}),
          onSubmitted: (_) => _link(),
        ),
        if (_error.isNotEmpty) ...[
          const SizedBox(height: 8),
          Text(
            _error,
            key: const Key('pair-error'),
            style: theme.textTheme.bodyMedium
                ?.copyWith(color: theme.colorScheme.error),
          ),
        ],
        const SizedBox(height: 20),
        FilledButton.icon(
          key: const Key('pair-submit'),
          onPressed: _canSubmit ? _link : null,
          icon: _busy
              ? const SizedBox(
                  width: 18,
                  height: 18,
                  child: CircularProgressIndicator(strokeWidth: 2),
                )
              : const Icon(Icons.phonelink_ring),
          label: Text(_busy ? 'Linking…' : 'Link phone'),
        ),
      ],
    );
  }

  Widget _doneStep() {
    final theme = Theme.of(context);
    return Center(
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Icon(Icons.phonelink_ring, size: 72, color: Colors.green),
          const SizedBox(height: 16),
          Text(
            _doneMessage ?? 'Linked.',
            key: const Key('pair-done-title'),
            style: theme.textTheme.headlineSmall,
            textAlign: TextAlign.center,
          ),
          const SizedBox(height: 8),
          Text(
            'You can now send messages and map links to this phone by voice.',
            textAlign: TextAlign.center,
            style: theme.textTheme.bodyMedium,
          ),
          const SizedBox(height: 24),
          FilledButton(
            key: const Key('pair-done'),
            onPressed: () => Navigator.of(context).pop(true),
            child: const Text('Done'),
          ),
        ],
      ),
    );
  }
}

/// Keeps the pairing-code field upper-cased as the user types, preserving the
/// cursor position.
class _UpperCaseFormatter extends TextInputFormatter {
  @override
  TextEditingValue formatEditUpdate(
    TextEditingValue oldValue,
    TextEditingValue newValue,
  ) {
    return newValue.copyWith(text: newValue.text.toUpperCase());
  }
}
