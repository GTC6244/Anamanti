// Copyright 2013 The Flutter Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package io.flutter.plugins.videoplayer;

import android.content.Context;
import androidx.annotation.NonNull;
import androidx.annotation.Nullable;
import androidx.annotation.OptIn;
import androidx.media3.common.MediaItem;
import androidx.media3.common.util.UnstableApi;
import androidx.media3.exoplayer.DefaultLoadControl;
import androidx.media3.exoplayer.LoadControl;
import androidx.media3.exoplayer.rtsp.RtspMediaSource;
import androidx.media3.exoplayer.source.MediaSource;

final class RtspVideoAsset extends VideoAsset {
  RtspVideoAsset(@NonNull String assetUrl) {
    super(assetUrl);
  }

  @NonNull
  @Override
  public MediaItem getMediaItem() {
    return new MediaItem.Builder().setUri(assetUrl).build();
  }

  // TODO: Migrate to stable API, see https://github.com/flutter/flutter/issues/147039.
  @OptIn(markerClass = UnstableApi.class)
  @Override
  @NonNull
  public MediaSource.Factory getMediaSourceFactory(@NonNull Context context) {
    // ANAMANTI PATCH (see ANAMANTI_PATCHES.md, patch 1): force RTP-over-TCP (interleaved
    // on the RTSP connection). Stock ExoPlayer tries UDP first and only falls back to TCP
    // after its 8 s DEFAULT_TIMEOUT_MS, which was measured at ~8.2 s to first frame on
    // the Echo Show; forcing TCP cut it to 5.0-6.6 s. Upstream: `new RtspMediaSource.Factory()`.
    return new RtspMediaSource.Factory().setForceUseRtpTcp(true);
  }

  // ANAMANTI PATCH (see ANAMANTI_PATCHES.md, patch 2): a low-latency start buffer for
  // live RTSP. ExoPlayer's default waits for ~2.5 s of media before starting playback;
  // a live camera view should start as soon as a keyframe and ~0.5 s are buffered, and
  // never hold a deep buffer (which only adds latency). Values satisfy DefaultLoadControl's
  // invariants: bufferForPlayback{,AfterRebuffer} <= minBuffer <= maxBuffer.
  static final int RTSP_MIN_BUFFER_MS = 1000;
  static final int RTSP_MAX_BUFFER_MS = 3000;
  static final int RTSP_BUFFER_FOR_PLAYBACK_MS = 500;
  static final int RTSP_BUFFER_FOR_PLAYBACK_AFTER_REBUFFER_MS = 1000;

  @OptIn(markerClass = UnstableApi.class)
  @Override
  @Nullable
  public LoadControl createLoadControl() {
    return new DefaultLoadControl.Builder()
        .setBufferDurationsMs(
            RTSP_MIN_BUFFER_MS,
            RTSP_MAX_BUFFER_MS,
            RTSP_BUFFER_FOR_PLAYBACK_MS,
            RTSP_BUFFER_FOR_PLAYBACK_AFTER_REBUFFER_MS)
        .build();
  }
}
