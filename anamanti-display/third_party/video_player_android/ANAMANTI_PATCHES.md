# Anamanti patches to `video_player_android`

This is a vendored copy of [`video_player_android` 2.12.2](https://pub.dev/packages/video_player_android/versions/2.12.2)
(media3 ExoPlayer 1.9.2), wired into the app with a `dependency_overrides` path entry in
`anamanti-display/pubspec.yaml`. It exists only for the live front-door camera view
(`lib/src/ui/camera_view.dart`, `plans/CameraPlan.md`): `video_player` does not expose
the two RTSP knobs below, and both were measured on the Echo Show during the W0 spike.

Upstream files are unchanged except where marked `// ANAMANTI PATCH:` (grep for it).
The upstream `example/` app was not copied (2.5 MB, unused). `LICENSE` (BSD-3) is kept.

## Patch 1 — force RTP over TCP for RTSP

- **File:** `android/src/main/java/io/flutter/plugins/videoplayer/RtspVideoAsset.java`,
  `getMediaSourceFactory`.
- **Change:** `new RtspMediaSource.Factory()` → `new RtspMediaSource.Factory().setForceUseRtpTcp(true)`.
- **Why:** stock ExoPlayer tries RTP over UDP first and only falls back to TCP after its
  8 s `DEFAULT_TIMEOUT_MS`. On the Echo Show against the Reolink NVR that gave ~8.2 s to
  first frame. With TCP forced it was 5.0–6.6 s (3 runs).

## Patch 2 — low-latency start buffer for RTSP only

- **Files:**
  - `VideoAsset.java`: a new `@Nullable LoadControl createLoadControl()` hook that returns
    `null` by default (every stock asset type: local file, HTTP/HLS/DASH/SS).
  - `RtspVideoAsset.java`: overrides it with a `DefaultLoadControl` using
    `setBufferDurationsMs(min=1000, max=3000, bufferForPlayback=500,
    bufferForPlaybackAfterRebuffer=1000)`.
  - `platformview/PlatformViewVideoPlayer.java` and `texture/TextureVideoPlayer.java`:
    after the stock back-buffer `LoadControl` block in `create(...)`, apply
    `asset.createLoadControl()` when it isn't `null`.
- **Why:** ExoPlayer's default waits for ~2.5 s of buffered media before it starts
  playback, and keeps a deep buffer that only adds latency to a live camera. Non-RTSP
  assets get `null`, so their behaviour is exactly upstream's.

## Re-applying on an upgrade

1. Copy the new version from `~/.pub-cache/hosted/pub.dev/video_player_android-<ver>/`
   over this folder (skip `example/`), keeping this file.
2. Re-apply both patches by hand (they are small; grep the old copy for
   `ANAMANTI PATCH` and port each hunk). Check that `RtspVideoAsset` and the two
   `*VideoPlayer.create(...)` builders still look the same.
3. Make sure the new version still builds on the locked AGP 8.7.3 / Gradle 8.14.3 /
   Flutter 3.44.1 toolchain (see `agents.md`), and bump `video_player` in `pubspec.yaml`
   to a version that pairs with it.
4. Test on the Echo Show: the camera view must still use `VideoViewType.platformView`
   (the MTK hardware decoder fails on the default texture view), and time to first
   frame should be about 5 s or less.

If upstream ever exposes these options, drop the override and this folder.
