// The "listening" cue overlay (Plan.MD §5 UI).
//
// A large, glowing blue ring (hollow — the slideshow shows through the centre)
// shown centered on screen while the device is actively listening to the user:
// from the wake word (or a follow-up listen reopening the mic) until end-of-speech.
// It is a live voice visualizer — its thickness, scale, and glow react to the
// microphone amplitude ([level], the input RMS the Rust engine already streams via
// `WakeWordEventKind.level`), so the ring visibly pumps as you speak. Under a
// baseline it keeps a slow "breathing" pulse so it feels alive before you talk.
//
// The reaction is **auto-ranged** against a slowly-decaying peak, so it stays
// visibly dynamic regardless of the absolute mic RMS (which varies a lot with
// capture gain, the device AEC, and how far away you are) — a fixed gain looked
// nearly flat because on-device speech RMS is small.
//
// Presentation only: shown by [AmbientScreen] while `AssistantState.listening`,
// fed `AssistantState.micLevel`. The enclosing screen fades it in/out and gates
// pointer events; this widget owns the pulse animation + amplitude smoothing.

import 'package:flutter/material.dart';

class ListeningOverlay extends StatefulWidget {
  const ListeningOverlay({
    super.key,
    this.level = 0.0,
    this.reactivity = 1.0,
    this.attack = 0.65,
    this.release = 0.08,
    this.decay = 0.99,
  });

  /// Latest microphone input RMS (~0.0–1.0). Drives the ring's reactive size/glow.
  final double level;

  /// Multiplier on the amplitude-driven swing (thickness/scale/glow). Higher = more
  /// dramatic. User-tunable via Speech Processing settings (`ringReactivity`).
  final double reactivity;

  /// Per-frame ease-up coefficient (0..1) as the level rises — higher is snappier
  /// (`ringAttack`).
  final double attack;

  /// Per-frame ease-down coefficient (0..1) as the level falls — lower lingers
  /// longer (`ringRelease`).
  final double release;

  /// Per-frame decay of the auto-range reference peak (closer to 1 holds the range
  /// longer) (`ringDecay`).
  final double decay;

  @override
  State<ListeningOverlay> createState() => _ListeningOverlayState();
}

class _ListeningOverlayState extends State<ListeningOverlay>
    with SingleTickerProviderStateMixin {
  // A slow breathe under the amplitude response so the ring feels alive even in
  // silence. One controller, cheap enough for the memory-tight device.
  late final AnimationController _pulse = AnimationController(
    vsync: this,
    duration: const Duration(milliseconds: 1400),
  )..repeat(reverse: true);

  // Smoothed amplitude (0..1) actually rendered, and a decaying reference peak the
  // raw level is auto-ranged against. Reset each time the ring is (re)mounted for a
  // fresh turn, since the widget only exists while shown.
  double _amp = 0.0;
  double _peak = _minPeak;

  // Room noise below this raw RMS is ignored (the speech threshold is ~0.012), and
  // the auto-range span never collapses below _minPeak so a first loud syllable
  // doesn't peg the ring permanently.
  static const double _floor = 0.008;
  static const double _minPeak = 0.03;

  @override
  void dispose() {
    _pulse.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    const glowBlue = Color(0xFF3B9CFF);
    return Center(
      child: AnimatedBuilder(
        animation: _pulse,
        builder: (context, _) {
          final level = widget.level;

          // Auto-range: track a peak that decays each frame (decay closer to 1 holds
          // the range longer), so the response spans the recent vocal range instead
          // of the tiny absolute RMS. Held at a sane minimum so quiet rooms don't
          // over-amplify.
          final decayed = _peak * widget.decay;
          _peak = level > decayed ? level : decayed;
          if (_peak < _minPeak) _peak = _minPeak;

          final norm = ((level - _floor) / (_peak - _floor)).clamp(0.0, 1.0);

          // Snappy attack, lingering release so each syllable visibly pumps the ring
          // rather than blurring into a steady glow.
          final k = norm > _amp ? widget.attack : widget.release;
          _amp += (norm - _amp) * k;
          // The reactivity dial scales how far the amplitude pushes the visuals.
          final amp = (_amp * widget.reactivity).clamp(0.0, 1.0);

          final breathe = Curves.easeInOut.transform(_pulse.value);
          // Voice dominates; only a whisper of baseline breathing underneath.
          final scale = 0.88 + 0.04 * breathe + 0.34 * amp; // ~0.88 → ~1.26
          final glow = (0.28 + 0.12 * breathe + 0.70 * amp).clamp(0.0, 1.0);
          final ringWidth = 9.0 + 29.0 * amp; // 9 → 38 px as you speak

          return Transform.scale(
            scale: scale,
            child: Container(
              width: 260,
              height: 260,
              decoration: BoxDecoration(
                shape: BoxShape.circle,
                // Hollow: no fill, so the slideshow/scrim shows through the centre.
                color: Colors.transparent,
                // The glowing ring itself — a bright blue-white stroke.
                border: Border.all(
                  color: const Color(0xFFBFE1FF),
                  width: ringWidth,
                ),
                boxShadow: [
                  // Wide soft halo bleeding outward, swelling hard with the voice.
                  BoxShadow(
                    color: glowBlue.withValues(alpha: glow),
                    blurRadius: 30 + 100 * amp + 10 * breathe,
                    spreadRadius: 2 + 30 * amp,
                  ),
                  // A tighter, brighter bloom hugging the stroke so it reads as
                  // genuinely luminous rather than just an outlined circle.
                  BoxShadow(
                    color: const Color(0xFFBFE1FF).withValues(alpha: glow),
                    blurRadius: 10 + 22 * amp,
                    spreadRadius: 1 + 5 * amp,
                  ),
                ],
              ),
            ),
          );
        },
      ),
    );
  }
}
