// Top-center banner for the in-app updater (plans/UpdaterPlan.md).
//
// Shown over the idle slideshow when a newer build is available, and while the
// user-initiated download/install runs. Styling mirrors [NotificationBanner]. Its
// visibility is decided by the caller (AmbientScreen) via
// `UpdateController.bannerVisible`; this widget only renders the current state and
// wires the Update / Install / Dismiss actions.

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/engine/update_controller.dart';

class UpdateBanner extends StatelessWidget {
  const UpdateBanner({
    super.key,
    required this.controller,
    required this.onAction,
    required this.onDismiss,
  });

  final UpdateController controller;

  /// Primary action (download when available, retry install when ready, retry
  /// after an error). The banner decides the label; the caller performs it.
  final VoidCallback onAction;
  final VoidCallback onDismiss;

  static const Color _accent = Color(0xFF4FC3F7);

  @override
  Widget build(BuildContext context) {
    final manifest = controller.manifest;
    final version = manifest == null
        ? ''
        : (manifest.versionName.isNotEmpty
              ? manifest.versionName
              : 'build ${manifest.versionCode}');

    final (String title, String body, String? actionLabel, bool showProgress) =
        switch (controller.status) {
          UpdateStatus.available => (
            'Update available',
            version.isEmpty ? 'A new version is ready.' : 'Version $version is ready.',
            'Update',
            false,
          ),
          UpdateStatus.downloading => (
            'Downloading update…',
            _progressText(),
            null,
            true,
          ),
          UpdateStatus.readyToInstall => (
            'Ready to install',
            'Tap Install to finish updating.',
            'Install',
            false,
          ),
          UpdateStatus.installing => ('Installing…', 'Finishing up.', null, false),
          UpdateStatus.error => (
            'Update failed',
            controller.errorMessage,
            'Retry',
            false,
          ),
          _ => ('Update', '', null, false),
        };

    return SafeArea(
      child: Align(
        alignment: Alignment.topCenter,
        child: Padding(
          padding: const EdgeInsets.only(top: 16),
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 560),
            child: Material(
              color: Colors.transparent,
              child: Container(
                padding: const EdgeInsets.fromLTRB(18, 14, 14, 14),
                decoration: BoxDecoration(
                  color: Colors.black.withValues(alpha: 0.82),
                  borderRadius: BorderRadius.circular(16),
                  border: const Border(left: BorderSide(color: _accent, width: 4)),
                  boxShadow: [
                    BoxShadow(
                      color: Colors.black.withValues(alpha: 0.4),
                      blurRadius: 18,
                      offset: const Offset(0, 6),
                    ),
                  ],
                ),
                child: Row(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    const Icon(
                      Icons.system_update_alt,
                      color: _accent,
                      size: 22,
                    ),
                    const SizedBox(width: 12),
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        mainAxisSize: MainAxisSize.min,
                        children: [
                          Text(
                            title,
                            style: const TextStyle(
                              color: Colors.white,
                              fontSize: 16,
                              fontWeight: FontWeight.w600,
                            ),
                          ),
                          if (body.isNotEmpty) ...[
                            const SizedBox(height: 4),
                            Text(
                              body,
                              style: TextStyle(
                                color: Colors.white.withValues(alpha: 0.85),
                                fontSize: 14,
                                height: 1.3,
                              ),
                            ),
                          ],
                          if (showProgress) ...[
                            const SizedBox(height: 10),
                            ClipRRect(
                              borderRadius: BorderRadius.circular(4),
                              child: LinearProgressIndicator(
                                value: controller.progress,
                                minHeight: 5,
                                backgroundColor: Colors.white.withValues(
                                  alpha: 0.15,
                                ),
                                valueColor: const AlwaysStoppedAnimation<Color>(
                                  _accent,
                                ),
                              ),
                            ),
                          ],
                        ],
                      ),
                    ),
                    const SizedBox(width: 8),
                    if (actionLabel != null)
                      TextButton(
                        onPressed: onAction,
                        child: Text(
                          actionLabel,
                          style: const TextStyle(
                            color: _accent,
                            fontWeight: FontWeight.w600,
                          ),
                        ),
                      ),
                    IconButton(
                      onPressed: onDismiss,
                      icon: Icon(
                        Icons.close,
                        color: Colors.white.withValues(alpha: 0.5),
                        size: 20,
                      ),
                    ),
                  ],
                ),
              ),
            ),
          ),
        ),
      ),
    );
  }

  String _progressText() {
    final p = controller.progress;
    if (p == null) {
      final mb = (controller.downloadedBytes / (1024 * 1024)).toStringAsFixed(1);
      return '$mb MB';
    }
    return '${(p * 100).round()}%';
  }
}
