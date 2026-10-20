// The live camera view's state (plans/CameraPlan.md).
//
// Opened by an `anamanti-camera` "show" frame (a `showCamera` engine event carrying
// only the camera's display name). The controller then fetches the stream URL from
// the Core with `get_camera_stream` and stores it here.
//
// SECURITY: [CameraSession.url] embeds the NVR credentials
// (`rtsp://user:pass@host:554/...`). It lives in memory only: never persist it,
// never log/print it. [toString] redacts it.

import 'package:flutter/foundation.dart';

/// Where the camera view is in its lifecycle.
enum CameraStatus {
  /// Fetching the stream URL from the Core.
  loading,

  /// The stream URL is known; the view plays it.
  ready,

  /// The Core refused or couldn't be reached; the view shows an error.
  error,
}

@immutable
class CameraSession {
  const CameraSession({
    required this.name,
    required this.seq,
    this.url,
    this.error,
  });

  /// The camera's display name (e.g. "Front Door"), shown as the view's label and
  /// reported as the display context.
  final String name;

  /// Bumped on every open, so a stream fetch that finishes after a newer open (or
  /// after a close and reopen) is dropped instead of overwriting the newer session.
  final int seq;

  /// The `rtsp://` stream URL with the NVR credentials embedded, or `null` while
  /// it's being fetched (or after a failure). Memory only: never persist or log.
  final String? url;

  /// Why the stream couldn't be fetched (a speakable reason from the Core), or `null`.
  /// An empty string means "failed, no detail".
  final String? error;

  CameraStatus get status {
    if (error != null) return CameraStatus.error;
    if (url != null) return CameraStatus.ready;
    return CameraStatus.loading;
  }

  CameraSession copyWith({String? name, String? url, String? error}) =>
      CameraSession(
        name: name ?? this.name,
        seq: seq,
        url: url ?? this.url,
        error: error ?? this.error,
      );

  // Never include the URL: it carries credentials.
  @override
  String toString() =>
      'CameraSession(name: $name, seq: $seq, status: ${status.name}, '
      'url: ${url == null ? 'none' : '<redacted>'})';
}
