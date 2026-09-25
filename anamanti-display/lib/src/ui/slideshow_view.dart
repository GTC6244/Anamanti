// Renders the idle photo slideshow with a slow cross-fade (Plan.MD §3, Phase 5).

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/slideshow/photo_source.dart';

class SlideshowView extends StatelessWidget {
  const SlideshowView({super.key, required this.controller});

  final SlideshowController controller;

  @override
  Widget build(BuildContext context) {
    return GestureDetector(
      // Swipe left → next photo, swipe right → previous (manual navigation also
      // resets the auto-advance timer). `opaque` so drags anywhere on the photo are
      // captured, even over transparent gradient areas.
      behavior: HitTestBehavior.opaque,
      onHorizontalDragEnd: (details) {
        final v = details.primaryVelocity ?? 0;
        if (v < 0) {
          controller.next(userInitiated: true);
        } else if (v > 0) {
          controller.previous(userInitiated: true);
        }
      },
      child: AnimatedBuilder(
        animation: controller,
        builder: (context, _) {
          final item = controller.current;
          return AnimatedSwitcher(
            duration: const Duration(milliseconds: 1200),
            switchInCurve: Curves.easeInOut,
            switchOutCurve: Curves.easeInOut,
            child: _Slide(
              // Keying by index makes AnimatedSwitcher cross-fade between slides.
              key: ValueKey<int>(controller.index),
              item: item,
            ),
          );
        },
      ),
    );
  }
}

class _Slide extends StatelessWidget {
  const _Slide({super.key, required this.item});

  final PhotoItem? item;

  @override
  Widget build(BuildContext context) {
    final gradient =
        item?.gradient ?? const [Color(0xFF1A2036), Color(0xFF0B0E1A)];
    final decoration = BoxDecoration(
      gradient: LinearGradient(
        begin: Alignment.topLeft,
        end: Alignment.bottomRight,
        colors: gradient,
      ),
    );

    // A remote image (when present) sits over its gradient, which doubles as the
    // fallback if the image fails to load.
    final url = item?.imageUrl;
    return Container(
      decoration: decoration,
      child: url == null
          ? null
          : Image.network(
              url,
              headers: item?.headers,
              fit: BoxFit.cover,
              width: double.infinity,
              height: double.infinity,
              errorBuilder: (_, _, _) => const SizedBox.expand(),
              frameBuilder: (context, child, frame, wasSyncLoaded) {
                if (wasSyncLoaded) return child;
                return AnimatedOpacity(
                  opacity: frame == null ? 0 : 1,
                  duration: const Duration(milliseconds: 600),
                  child: child,
                );
              },
            ),
    );
  }
}
