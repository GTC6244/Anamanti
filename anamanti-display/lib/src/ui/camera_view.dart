// Full-screen live camera view for the 8-inch Echo Show (plans/CameraPlan.md).
//
// Shown while [AssistantState.camera] is set: the Core asked for a camera (voice
// fast path / `show_camera` tool) and the controller fetched its RTSP stream URL.
// Plays the NVR's H.264 sub-stream through ExoPlayer (the vendored, patched
// `video_player_android`: RTP over TCP + a low-latency start buffer), video only.
//
// Hard requirements, measured on the Echo Show in the W0 spike:
//  * `VideoViewType.platformView`. The MTK hardware decoder can't allocate output
//    buffers on Flutter's texture surface (the default `textureView`), so nothing
//    plays there.
//  * Don't treat `VideoPlayerValue.position` as a liveness signal: it stays ~0 for a
//    live RTSP stream. Readiness comes from `isInitialized`; failure from `hasError`.
//
// SECURITY: the stream URL embeds the NVR credentials. It is never logged here (not
// even in error messages, which may echo the URI), and it lives only in memory.

import 'package:flutter/material.dart';
import 'package:video_player/video_player.dart';

import 'package:anamanti_display/src/engine/camera_session.dart';

/// Builds the player for a stream URL. Injectable so widget tests can supply a fake
/// controller without a platform video implementation.
typedef CameraPlayerFactory = VideoPlayerController Function(Uri uri);

/// The production player: a platform-view (SurfaceView) ExoPlayer that doesn't take
/// audio focus (the view is muted; the assistant owns the speaker).
VideoPlayerController defaultCameraPlayer(Uri uri) =>
    VideoPlayerController.networkUrl(
      uri,
      // Mandatory on the Echo Show: textureView fails with the MTK decoder.
      viewType: VideoViewType.platformView,
      videoPlayerOptions: VideoPlayerOptions(mixWithOthers: true),
    );

class CameraView extends StatefulWidget {
  const CameraView({
    super.key,
    required this.session,
    required this.onClose,
    this.playerFactory = defaultCameraPlayer,
  });

  final CameraSession session;
  final VoidCallback onClose;
  final CameraPlayerFactory playerFactory;

  @override
  State<CameraView> createState() => _CameraViewState();
}

class _CameraViewState extends State<CameraView> {
  VideoPlayerController? _player;

  /// The URL [_player] was built for, so a rebuild with the same session doesn't
  /// restart the stream. Memory only.
  String? _playerUrl;

  /// The player failed to start (initialize threw, or the URL didn't parse).
  bool _playerFailed = false;

  /// Latched once the player first reports ready, so a later rebuffer doesn't bring
  /// the full loading spinner back.
  bool _started = false;

  @override
  void initState() {
    super.initState();
    _syncPlayer();
  }

  @override
  void didUpdateWidget(CameraView oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (widget.session.url != _playerUrl) _syncPlayer();
  }

  @override
  void dispose() {
    _disposePlayer();
    super.dispose();
  }

  void _disposePlayer() {
    final p = _player;
    _player = null;
    _playerUrl = null;
    if (p != null) {
      p.removeListener(_onPlayerValue);
      p.dispose();
    }
  }

  /// (Re)build the player for the session's current URL, or tear it down when there
  /// is none (still fetching, or the fetch failed).
  void _syncPlayer() {
    _disposePlayer();
    _playerFailed = false;
    _started = false;
    final url = widget.session.url;
    if (url == null || widget.session.status != CameraStatus.ready) return;
    _playerUrl = url;
    final uri = Uri.tryParse(url);
    if (uri == null) {
      _playerFailed = true;
      return;
    }
    final p = widget.playerFactory(uri);
    _player = p;
    p.addListener(_onPlayerValue);
    // Video only: the NVR sub-stream carries an AAC track, which stays silent.
    p.setVolume(0);
    p.initialize().then(
      (_) {
        if (mounted && identical(_player, p)) p.play();
      },
      onError: (Object e) {
        // Log only the error type: platform errors can echo the URI (credentials).
        debugPrint('camera player failed to start (${e.runtimeType})');
        if (mounted && identical(_player, p)) {
          setState(() => _playerFailed = true);
        }
      },
    );
  }

  void _onPlayerValue() {
    final p = _player;
    if (!mounted || p == null) return;
    final v = p.value;
    if (!_started && v.isInitialized && !v.hasError) _started = true;
    setState(() {});
  }

  bool get _failed =>
      widget.session.status == CameraStatus.error ||
      _playerFailed ||
      (_player?.value.hasError ?? false);

  @override
  Widget build(BuildContext context) {
    final p = _player;
    final failed = _failed;
    final loading = !failed && !_started;
    return Material(
      color: Colors.black,
      child: Stack(
        fit: StackFit.expand,
        children: [
          if (p != null && !failed)
            Center(
              child: AspectRatio(
                aspectRatio: p.value.isInitialized
                    ? p.value.aspectRatio
                    : 16 / 9,
                child: VideoPlayer(p),
              ),
            ),
          if (loading)
            const Center(
              key: Key('camera-loading'),
              child: SizedBox(
                width: 56,
                height: 56,
                child: CircularProgressIndicator(
                  strokeWidth: 4,
                  color: Colors.white70,
                ),
              ),
            ),
          if (failed) _error(),
          Positioned(left: 0, top: 0, right: 0, child: _header()),
        ],
      ),
    );
  }

  Widget _header() {
    final name = widget.session.name.isEmpty ? 'Camera' : widget.session.name;
    return Container(
      decoration: BoxDecoration(
        gradient: LinearGradient(
          begin: Alignment.topCenter,
          end: Alignment.bottomCenter,
          colors: [Colors.black.withValues(alpha: 0.6), Colors.transparent],
        ),
      ),
      padding: const EdgeInsets.fromLTRB(28, 16, 12, 24),
      child: SafeArea(
        bottom: false,
        child: Row(
          children: [
            Icon(
              Icons.videocam,
              color: Colors.white.withValues(alpha: 0.85),
              size: 28,
            ),
            const SizedBox(width: 12),
            Expanded(
              child: Text(
                name,
                key: const Key('camera-name'),
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: const TextStyle(
                  color: Colors.white,
                  fontSize: 28,
                  fontWeight: FontWeight.w700,
                  letterSpacing: 0.2,
                ),
              ),
            ),
            IconButton(
              key: const Key('camera-close'),
              tooltip: 'Close camera',
              onPressed: widget.onClose,
              iconSize: 36,
              icon: Icon(
                Icons.close,
                color: Colors.white.withValues(alpha: 0.9),
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _error() {
    final detail = widget.session.status == CameraStatus.error
        ? (widget.session.error ?? '')
        : '';
    return Center(
      key: const Key('camera-error'),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 48),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(
              Icons.videocam_off,
              size: 64,
              color: Colors.white.withValues(alpha: 0.7),
            ),
            const SizedBox(height: 16),
            const Text(
              "Couldn't reach the camera",
              textAlign: TextAlign.center,
              style: TextStyle(
                color: Colors.white,
                fontSize: 26,
                fontWeight: FontWeight.w600,
              ),
            ),
            if (detail.isNotEmpty) ...[
              const SizedBox(height: 8),
              Text(
                detail,
                textAlign: TextAlign.center,
                style: TextStyle(
                  color: Colors.white.withValues(alpha: 0.7),
                  fontSize: 18,
                ),
              ),
            ],
          ],
        ),
      ),
    );
  }
}
