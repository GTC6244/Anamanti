// Guided voice-enrollment screen (SpeakerEnrollmentPlan.md Phase 2).
//
// "Add a person": enter a name, then read a few short phrases so the assistant
// builds a strong voiceprint, and finally confirm it recognizes the voice. Each
// phrase capture reuses the ordinary wake-word turn — the screen stamps an
// `enroll` display context (via [EnrollContextSink]) so the Core routes the spoken
// audio to the voiceprint accumulator instead of the assistant, and relays the
// per-sample result back on the engine event stream (folded into
// [AssistantState.enrollResultJson] / [AssistantState.enrollSeq] by
// [AssistantController]). There is no second audio path.
//
// v1 is wake-word-gated per sample: the user says the wake word, then reads a
// phrase. Commit + cancel are control round-trips through the [OrchestratorClient].

import 'dart:convert';

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/settings/orchestrator_client.dart';

/// Pushes the enrollment display context down to the native engine so the next
/// voice turn's `audio-start` carries it (`screen.kind = "enroll"`). Production
/// wires the native `setEnrollContext`; null (tests) disables stamping.
typedef EnrollContextSink =
    void Function({
      required bool active,
      required String session,
      required String name,
      required int sampleIndex,
      required String phase,
    });

/// Minimum accepted samples before enrollment can be committed. Mirrors the Core's
/// `MIN_ENROLL_SAMPLES` so the UI enables "Save" at the same point the Core accepts.
const int kMinEnrollSamples = 3;

/// A few short, phonetically varied prompts to read during enrollment.
const List<String> kEnrollPhrases = [
  'The quick brown fox jumps over the lazy dog.',
  'How are you doing this fine morning?',
  'I would like to listen to some music, please.',
  'Set a timer for ten minutes and remind me later.',
  'What is the weather going to be like tomorrow?',
];

enum _Stage { name, capturing, committing, verifying, done }

class SpeakerEnrollScreen extends StatefulWidget {
  const SpeakerEnrollScreen({
    super.key,
    required this.client,
    required this.controller,
    this.setEnrollContext,
    this.wakeWord = '',
  });

  /// Control-plane client for commit / cancel.
  final OrchestratorClient client;

  /// The live engine controller; its [AssistantState] surfaces each capture result.
  final AssistantController controller;

  /// Stamps the enroll display context on the next turn (null disables it, tests).
  final EnrollContextSink? setEnrollContext;

  /// The configured wake word, shown in the capture instructions ("Say '…'").
  final String wakeWord;

  @override
  State<SpeakerEnrollScreen> createState() => _SpeakerEnrollScreenState();
}

class _SpeakerEnrollScreenState extends State<SpeakerEnrollScreen> {
  final TextEditingController _nameCtl = TextEditingController();
  late final String _session =
      'enroll-${DateTime.now().millisecondsSinceEpoch}';

  _Stage _stage = _Stage.name;
  String _name = '';
  int _accepted = 0;
  String _message = '';
  bool _busy = false;
  bool _committed = false;
  double _verifyConfidence = 0;
  String _verifyName = '';
  bool _verifyRecognized = false;

  int _lastSeq = 0;

  @override
  void initState() {
    super.initState();
    _lastSeq = widget.controller.state.enrollSeq;
    widget.controller.addListener(_onControllerTick);
  }

  @override
  void dispose() {
    widget.controller.removeListener(_onControllerTick);
    // Leaving the screen: clear the enroll context so normal turns resume, and discard
    // an uncommitted session on the Core so its buffered samples don't linger.
    widget.setEnrollContext?.call(
      active: false,
      session: _session,
      name: _name,
      sampleIndex: 0,
      phase: 'sample',
    );
    if (!_committed) {
      // Fire-and-forget; the screen is going away.
      widget.client.enrollCancel(_session).catchError((_) {});
    }
    _nameCtl.dispose();
    super.dispose();
  }

  /// Push the current enroll context so the next capture carries the right session,
  /// name, progress index, and phase. A no-op when no sink is wired (tests).
  void _pushContext(String phase) {
    widget.setEnrollContext?.call(
      active: true,
      session: _session,
      name: _name,
      sampleIndex: _accepted,
      phase: phase,
    );
  }

  void _onControllerTick() {
    final s = widget.controller.state;
    if (s.enrollSeq == _lastSeq) return;
    _lastSeq = s.enrollSeq;
    if (!mounted) return;
    _handleResult(s.enrollResultJson);
  }

  void _handleResult(String rawJson) {
    Map<String, dynamic> data;
    try {
      final decoded = jsonDecode(rawJson);
      if (decoded is! Map<String, dynamic>) return;
      data = decoded;
    } catch (_) {
      return;
    }
    final phase = (data['phase'] as String?) ?? 'sample';
    final ok = (data['ok'] as bool?) ?? false;
    final message = (data['message'] as String?) ?? '';

    if (phase == 'verify') {
      final confidence = (data['confidence'] as num?)?.toDouble() ?? 0.0;
      final name = (data['name'] as String?) ?? _name;
      setState(() {
        _verifyConfidence = confidence;
        _verifyName = name;
        _verifyRecognized = ok && confidence > 0;
        _message = message;
        _stage = _Stage.done;
      });
      return;
    }

    // A training-sample result.
    final accepted = (data['accepted'] as bool?) ?? false;
    final samples = (data['samples'] as num?)?.toInt() ?? _accepted;
    setState(() {
      _accepted = samples;
      _message = message.isEmpty
          ? (accepted ? 'Captured a sample.' : 'Sample rejected — try again.')
          : message;
      if (_stage == _Stage.capturing) _pushContext('sample');
    });
  }

  Future<void> _start() async {
    final name = _nameCtl.text.trim();
    if (name.isEmpty) return;
    setState(() {
      _name = name;
      _stage = _Stage.capturing;
      _message = '';
    });
    _pushContext('sample');
  }

  Future<void> _finish() async {
    setState(() {
      _busy = true;
      _stage = _Stage.committing;
    });
    try {
      final msg = await widget.client.enrollFinish(_session, _name);
      _committed = true;
      if (!mounted) return;
      // Only advance if we're still committing — a verify result could already have
      // landed (and moved us to `done`); don't clobber it.
      if (_stage != _Stage.committing) return;
      setState(() {
        _busy = false;
        _message = msg;
        _stage = _Stage.verifying;
      });
      _pushContext('verify');
    } catch (e) {
      if (!mounted || _stage != _Stage.committing) return;
      setState(() {
        _busy = false;
        _stage = _Stage.capturing;
        _message = 'Couldn\'t save yet: $e';
      });
      _pushContext('sample');
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Add a person')),
      body: SafeArea(
        child: Padding(
          padding: const EdgeInsets.all(20),
          child: switch (_stage) {
            _Stage.name => _nameStep(),
            _Stage.capturing => _captureStep(),
            _Stage.committing => _busyStep('Saving voiceprint…'),
            _Stage.verifying => _verifyStep(),
            _Stage.done => _doneStep(),
          },
        ),
      ),
    );
  }

  Widget _nameStep() {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Text(
          "Who are we adding?",
          style: Theme.of(context).textTheme.headlineSmall,
        ),
        const SizedBox(height: 8),
        Text(
          "Enter a name, then read a few short phrases so the assistant learns this "
          "voice. It'll confirm it can recognize them at the end.",
          style: Theme.of(context).textTheme.bodyMedium,
        ),
        const SizedBox(height: 20),
        TextField(
          key: const Key('enroll-name'),
          controller: _nameCtl,
          autofocus: true,
          textCapitalization: TextCapitalization.words,
          decoration: const InputDecoration(
            labelText: 'Name',
            hintText: 'e.g. Dana',
            border: OutlineInputBorder(),
          ),
          onChanged: (_) => setState(() {}),
          onSubmitted: (_) => _start(),
        ),
        const SizedBox(height: 20),
        FilledButton.icon(
          key: const Key('enroll-start'),
          onPressed: _nameCtl.text.trim().isEmpty ? null : _start,
          icon: const Icon(Icons.mic),
          label: const Text('Start'),
        ),
      ],
    );
  }

  Widget _captureStep() {
    final remaining = (kMinEnrollSamples - _accepted).clamp(
      0,
      kMinEnrollSamples,
    );
    final ready = _accepted >= kMinEnrollSamples;
    final phrase = kEnrollPhrases[_accepted % kEnrollPhrases.length];
    final wake = widget.wakeWord.isEmpty
        ? 'the wake word'
        : '"${widget.wakeWord}"';
    return ListView(
      children: [
        Text(
          'Enrolling $_name',
          style: Theme.of(context).textTheme.headlineSmall,
        ),
        const SizedBox(height: 4),
        Text(
          ready
              ? 'Enough samples captured — add more for accuracy, or save.'
              : 'Say $remaining more phrase${remaining == 1 ? '' : 's'}.',
          style: Theme.of(context).textTheme.bodyMedium,
        ),
        const SizedBox(height: 16),
        _ProgressDots(accepted: _accepted, min: kMinEnrollSamples),
        const SizedBox(height: 20),
        Card(
          child: Padding(
            padding: const EdgeInsets.all(16),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  children: [
                    const Icon(Icons.record_voice_over),
                    const SizedBox(width: 8),
                    Text(
                      'Say $wake, then read:',
                      style: Theme.of(context).textTheme.titleSmall,
                    ),
                  ],
                ),
                const SizedBox(height: 12),
                Text(
                  phrase,
                  key: const Key('enroll-phrase'),
                  style: Theme.of(context).textTheme.headlineSmall,
                ),
              ],
            ),
          ),
        ),
        const SizedBox(height: 12),
        if (_message.isNotEmpty)
          Text(
            _message,
            key: const Key('enroll-message'),
            style: Theme.of(context).textTheme.bodyMedium?.copyWith(
              color: Theme.of(context).colorScheme.primary,
            ),
          ),
        const SizedBox(height: 20),
        FilledButton.icon(
          key: const Key('enroll-finish'),
          onPressed: ready && !_busy ? _finish : null,
          icon: const Icon(Icons.check),
          label: Text(
            ready
                ? 'Save & check voice'
                : 'Save (need ${kMinEnrollSamples - _accepted} more)',
          ),
        ),
        const SizedBox(height: 8),
        TextButton(
          onPressed: () => Navigator.of(context).maybePop(),
          child: const Text('Cancel'),
        ),
      ],
    );
  }

  Widget _verifyStep() {
    final wake = widget.wakeWord.isEmpty
        ? 'the wake word'
        : '"${widget.wakeWord}"';
    return ListView(
      children: [
        Text(
          'Saved! Now let\'s check it',
          style: Theme.of(context).textTheme.headlineSmall,
        ),
        const SizedBox(height: 8),
        Text(
          'Say $wake and speak naturally for a moment so I can confirm I recognize '
          '$_name.',
          style: Theme.of(context).textTheme.bodyMedium,
        ),
        const SizedBox(height: 20),
        const Center(child: Icon(Icons.hearing, size: 64)),
        const SizedBox(height: 20),
        if (_message.isNotEmpty)
          Text(_message, style: Theme.of(context).textTheme.bodyMedium),
        const SizedBox(height: 20),
        TextButton(
          key: const Key('enroll-skip-verify'),
          onPressed: () => Navigator.of(context).maybePop(),
          child: const Text('Skip & finish'),
        ),
      ],
    );
  }

  Widget _busyStep(String label) {
    return Center(
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          const CircularProgressIndicator(),
          const SizedBox(height: 16),
          Text(label),
        ],
      ),
    );
  }

  Widget _doneStep() {
    final theme = Theme.of(context);
    final recognized = _verifyRecognized;
    return Center(
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Icon(
            recognized ? Icons.verified_user : Icons.person_add_alt_1,
            size: 72,
            color: recognized ? Colors.green : theme.colorScheme.primary,
          ),
          const SizedBox(height: 16),
          Text(
            recognized
                ? 'Recognized ${_verifyName.isEmpty ? _name : _verifyName}!'
                : '$_name is enrolled',
            key: const Key('enroll-done-title'),
            style: theme.textTheme.headlineSmall,
            textAlign: TextAlign.center,
          ),
          const SizedBox(height: 8),
          Text(
            recognized
                ? 'Confidence ${_verifyConfidence.toStringAsFixed(2)}. '
                      'The assistant will now tailor answers to them.'
                : 'You can test or add more samples any time from the People list.',
            textAlign: TextAlign.center,
            style: theme.textTheme.bodyMedium,
          ),
          const SizedBox(height: 24),
          FilledButton(
            key: const Key('enroll-done'),
            onPressed: () => Navigator.of(context).pop(true),
            child: const Text('Done'),
          ),
        ],
      ),
    );
  }
}

/// A row of dots showing captured-sample progress toward the minimum.
class _ProgressDots extends StatelessWidget {
  const _ProgressDots({required this.accepted, required this.min});

  final int accepted;
  final int min;

  @override
  Widget build(BuildContext context) {
    final count = accepted > min ? accepted : min;
    final primary = Theme.of(context).colorScheme.primary;
    return Row(
      mainAxisAlignment: MainAxisAlignment.center,
      children: [
        for (var i = 0; i < count; i++)
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 4),
            child: Icon(
              i < accepted ? Icons.circle : Icons.circle_outlined,
              size: 18,
              color: i < accepted ? primary : Theme.of(context).disabledColor,
            ),
          ),
      ],
    );
  }
}
