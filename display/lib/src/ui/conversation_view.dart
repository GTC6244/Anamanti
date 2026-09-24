// The live conversation panel shown over the slideshow during a turn (Plan.MD §3,
// Phase 5: "Dart screen state consumes FRB StreamSink events (transcript + reply
// tokens)… smooth token-by-token rendering").

import 'package:flutter/material.dart';

import 'package:ambient_display/src/engine/assistant_controller.dart';

class ConversationView extends StatelessWidget {
  const ConversationView({super.key, required this.state});

  final AssistantState state;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        // Tuned for the 8-inch landscape display: cap the reading column so long
        // replies stay legible rather than spanning the full width.
        final maxWidth = constraints.maxWidth.clamp(0.0, 720.0);
        return Center(
          child: ConstrainedBox(
            constraints: BoxConstraints(maxWidth: maxWidth),
            child: Column(
              mainAxisAlignment: MainAxisAlignment.center,
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisSize: MainAxisSize.min,
              children: [
                // The phase text ("Listening…", "Thinking…", "Speaking…", etc.) is
                // deliberately NOT shown here — the same information lives in the
                // top-right status indicator. This panel keeps only the spoken
                // transcript and the assistant's reply.
                if (state.transcript.isNotEmpty)
                  _Bubble(
                    text: state.transcript,
                    alignEnd: true,
                    color: const Color(0xFF2A3350),
                    textColor: Colors.white,
                  ),
                if (state.reply.isNotEmpty) ...[
                  const SizedBox(height: 12),
                  _Bubble(
                    text: state.reply,
                    alignEnd: false,
                    color: const Color(0xFFEAF2FF),
                    textColor: const Color(0xFF10162A),
                    // A caret while the reply is still streaming in.
                    showCaret: state.phase == TurnPhase.thinking,
                  ),
                ],
              ],
            ),
          ),
        );
      },
    );
  }
}

class _Bubble extends StatelessWidget {
  const _Bubble({
    required this.text,
    required this.alignEnd,
    required this.color,
    required this.textColor,
    this.showCaret = false,
  });

  final String text;
  final bool alignEnd;
  final Color color;
  final Color textColor;
  final bool showCaret;

  @override
  Widget build(BuildContext context) {
    return Align(
      alignment: alignEnd ? Alignment.centerRight : Alignment.centerLeft,
      child: AnimatedSize(
        duration: const Duration(milliseconds: 120),
        curve: Curves.easeOut,
        alignment: Alignment.topCenter,
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 18, vertical: 14),
          decoration: BoxDecoration(
            color: color,
            borderRadius: BorderRadius.circular(18),
          ),
          child: Text(
            showCaret ? '$text▌' : text,
            style: TextStyle(
              color: textColor,
              fontSize: 22,
              height: 1.35,
              fontWeight: FontWeight.w400,
            ),
          ),
        ),
      ),
    );
  }
}
