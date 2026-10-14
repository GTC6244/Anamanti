// Guided voice-enrollment screen (SpeakerEnrollmentPlan.md Phase 2).
//
// "Add a voice": pick an existing household member (or enter a new person's name),
// then read a few short phrases so the assistant builds a strong voiceprint, and
// finally confirm it recognizes the voice. Choosing an existing member ties the
// voiceprint to that person in the household roster (and adds to their voiceprint if
// they already have one); a new name is also added to the household by the Core.
//
// "Add voice samples" (People → a person's ⋮ menu): the same capture flow, opened with
// a [SpeakerEnrollScreen.speakerId] — it skips the name step, guides the user through a
// fresh set of phrases, and folds the samples into that exact person's voiceprint. Each
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

/// Short, phonetically varied prompts to read during enrollment. A first enrollment
/// starts at the top; "Add voice samples" starts after the first
/// [kMinEnrollSamples] so a re-train hears different sentences.
const List<String> kEnrollPhrases = [
  'The quick brown fox jumps over the lazy dog.',
  'How are you doing this fine morning?',
  'I would like to listen to some music, please.',
  'Set a timer for ten minutes and remind me later.',
  'What is the weather going to be like tomorrow?',
  'Please add milk, eggs, and bread to the shopping list.',
  'Turn the volume down a little and play something relaxing.',
  'Remind me to call my brother on Saturday afternoon.',
];

enum _Stage { name, capturing, committing, verifying, done }

class SpeakerEnrollScreen extends StatefulWidget {
  const SpeakerEnrollScreen({
    super.key,
    required this.client,
    required this.controller,
    this.setEnrollContext,
    this.wakeWord = '',
    this.speakerId,
    this.personLabel = '',
  });

  /// Control-plane client for commit / cancel.
  final OrchestratorClient client;

  /// The live engine controller; its [AssistantState] surfaces each capture result.
  final AssistantController controller;

  /// Stamps the enroll display context on the next turn (null disables it, tests).
  final EnrollContextSink? setEnrollContext;

  /// The configured wake word, shown in the capture instructions ("Say '…'").
  final String wakeWord;

  /// "Add voice samples" mode: the existing speaker profile to add samples to. When
  /// set, the name step is skipped and the commit folds into this profile by id.
  final String? speakerId;

  /// How to refer to the [speakerId] person on screen (their name, or "Speaker N").
  final String personLabel;

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

  /// Household roster names from the Core (empty while loading / when unreachable /
  /// when the roster is empty — the new-person field always works).
  List<String> _members = const [];

  /// Lower-cased names that already have a voiceprint (a named speaker profile).
  Set<String> _voiced = const {};

  /// The existing member picked in the name step, or null for "someone new".
  String? _selectedMember;

  @override
  void initState() {
    super.initState();
    _lastSeq = widget.controller.state.enrollSeq;
    widget.controller.addListener(_onControllerTick);
    if (_addingSamples) {
      // Straight to the phrase guide for a person who's already on the People list.
      _name = widget.personLabel;
      _stage = _Stage.capturing;
      _pushContext('sample');
    } else {
      _loadMembers();
    }
  }

  /// Whether this is "Add voice samples" for an existing person (vs. a new enrollment).
  bool get _addingSamples => widget.speakerId != null;

  /// Where in [kEnrollPhrases] this session's guide starts.
  int get _phraseOffset => _addingSamples ? kMinEnrollSamples : 0;

  /// The phrase for guide slot [i] of this session.
  String _phraseAt(int i) =>
      kEnrollPhrases[(_phraseOffset + i) % kEnrollPhrases.length];

  /// Fetch the household roster (and which names already have a voiceprint) for the
  /// "existing person" picker. Best-effort: a failure leaves only the new-name field.
  Future<void> _loadMembers() async {
    List<String> members = const [];
    Set<String> voiced = const {};
    try {
      members = await widget.client.listHouseholdMembers();
    } catch (_) {}
    try {
      final speakers = await widget.client.listSpeakers();
      voiced = {
        for (final s in speakers)
          if (s.name != null && s.name!.trim().isNotEmpty)
            s.name!.trim().toLowerCase(),
      };
    } catch (_) {}
    if (!mounted) return;
    setState(() {
      _members = members;
      _voiced = voiced;
    });
  }

  /// The roster member matching [name] (case-insensitive), or null.
  String? _memberMatching(String name) {
    final needle = name.trim().toLowerCase();
    if (needle.isEmpty) return null;
    for (final m in _members) {
      if (m.trim().toLowerCase() == needle) return m;
    }
    return null;
  }

  /// The name the enrollment will be saved under: the picked member, else the typed one.
  String get _chosenName => _selectedMember ?? _nameCtl.text.trim();

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
    // A typed name that matches a roster member is that member (canonical spelling).
    final typed = _nameCtl.text.trim();
    final name = _selectedMember ?? _memberMatching(typed) ?? typed;
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
      final msg = await widget.client.enrollFinish(
        _session,
        _name,
        speakerId: widget.speakerId,
      );
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
      appBar: AppBar(
        title: Text(_addingSamples ? 'Add voice samples' : 'Add a voice'),
      ),
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
    final theme = Theme.of(context);
    final chosen = _chosenName;
    final member = _selectedMember ?? _memberMatching(chosen);
    final hasVoice =
        member != null && _voiced.contains(member.trim().toLowerCase());
    final String? hint = member == null
        ? (chosen.isEmpty
              ? null
              : '$chosen will be added to the household as a new person.')
        : hasVoice
        ? '$member already has a voiceprint — these samples will be added to it.'
        : 'This voice will be linked to $member in the household.';
    return ListView(
      children: [
        Text("Whose voice is this?", style: theme.textTheme.headlineSmall),
        const SizedBox(height: 8),
        Text(
          _members.isEmpty
              ? "Enter a name, then read a few short phrases so the assistant learns "
                    "this voice. It'll confirm it can recognize them at the end."
              : "Pick someone already in the household, or add someone new. Then "
                    "read a few short phrases so the assistant learns their voice.",
          style: theme.textTheme.bodyMedium,
        ),
        if (_members.isNotEmpty) ...[
          const SizedBox(height: 20),
          Text('Household members', style: theme.textTheme.titleSmall),
          const SizedBox(height: 8),
          Wrap(
            spacing: 8,
            runSpacing: 8,
            children: [
              for (final m in _members)
                ChoiceChip(
                  key: Key('enroll-member-$m'),
                  avatar: Icon(
                    _voiced.contains(m.trim().toLowerCase())
                        ? Icons.record_voice_over
                        : Icons.person_outline,
                    size: 18,
                  ),
                  label: Text(m),
                  selected: _selectedMember == m,
                  onSelected: (on) => setState(() {
                    _selectedMember = on ? m : null;
                    if (on) _nameCtl.clear();
                  }),
                ),
            ],
          ),
          const SizedBox(height: 20),
          Text('Or someone new', style: theme.textTheme.titleSmall),
        ],
        const SizedBox(height: 12),
        TextField(
          key: const Key('enroll-name'),
          controller: _nameCtl,
          autofocus: _members.isEmpty,
          textCapitalization: TextCapitalization.words,
          decoration: const InputDecoration(
            labelText: 'Name',
            hintText: 'e.g. Dana',
            border: OutlineInputBorder(),
          ),
          onChanged: (v) => setState(() {
            if (v.trim().isNotEmpty) _selectedMember = null;
          }),
          onSubmitted: (_) => _start(),
        ),
        if (hint != null) ...[
          const SizedBox(height: 12),
          Text(
            hint,
            key: const Key('enroll-name-hint'),
            style: theme.textTheme.bodyMedium?.copyWith(
              color: theme.colorScheme.primary,
            ),
          ),
        ],
        const SizedBox(height: 20),
        FilledButton.icon(
          key: const Key('enroll-start'),
          onPressed: chosen.isEmpty ? null : _start,
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
    final phrase = _phraseAt(_accepted);
    // The guide lists the minimum set of phrases (growing by one per extra sample),
    // ticking off the ones already captured.
    final guideCount = _accepted + 1 > kMinEnrollSamples
        ? _accepted + 1
        : kMinEnrollSamples;
    final wake = widget.wakeWord.isEmpty
        ? 'the wake word'
        : '"${widget.wakeWord}"';
    final main = <Widget>[
      Text(
        _addingSamples ? 'More samples for $_name' : 'Enrolling $_name',
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
                  Flexible(
                    child: Text(
                      'Say $wake, then read:',
                      style: Theme.of(context).textTheme.titleSmall,
                    ),
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
    ];
    final guide = _PhraseGuide(
      phrases: [for (var i = 0; i < guideCount; i++) _phraseAt(i)],
      done: _accepted,
    );
    // Landscape-first: the phrase guide sits beside the capture controls so the
    // Save button stays above the fold on the 8" screen; narrow screens stack it.
    return LayoutBuilder(
      builder: (context, constraints) {
        if (constraints.maxWidth < 700) {
          return ListView(
            children: [
              ...main.take(main.length - 5),
              guide,
              ...main.skip(main.length - 5),
            ],
          );
        }
        return Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Expanded(flex: 3, child: ListView(children: main)),
            const SizedBox(width: 24),
            Expanded(flex: 2, child: ListView(children: [guide])),
          ],
        );
      },
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
                : _addingSamples
                ? 'Added samples for $_name'
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

/// The session's phrase checklist: captured phrases ticked off, the current one
/// highlighted, the rest still to come — so the user sees the whole set up front.
class _PhraseGuide extends StatelessWidget {
  const _PhraseGuide({required this.phrases, required this.done});

  final List<String> phrases;
  final int done;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text('Phrase guide', style: theme.textTheme.titleSmall),
        const SizedBox(height: 4),
        for (var i = 0; i < phrases.length; i++)
          Padding(
            key: Key('enroll-guide-$i'),
            padding: const EdgeInsets.symmetric(vertical: 4),
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Icon(
                  i < done
                      ? Icons.check_circle
                      : i == done
                      ? Icons.arrow_right_alt
                      : Icons.radio_button_unchecked,
                  size: 20,
                  color: i < done
                      ? Colors.green
                      : i == done
                      ? theme.colorScheme.primary
                      : theme.disabledColor,
                ),
                const SizedBox(width: 8),
                Expanded(
                  child: Text(
                    phrases[i],
                    style: theme.textTheme.bodyMedium?.copyWith(
                      fontWeight: i == done ? FontWeight.w600 : null,
                      color: i < done ? theme.disabledColor : null,
                      decoration: i < done ? TextDecoration.lineThrough : null,
                    ),
                  ),
                ),
              ],
            ),
          ),
      ],
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
