// Now playing tab (read-only in v1): what the house speakers are playing —
// Spotify track or radio station — from `GET /music/nowplaying.json`.

import 'dart:async';

import 'package:flutter/material.dart';

import '../data/companion_api.dart';
import '../util/format.dart';
import 'common.dart';

class NowPlayingScreen extends StatefulWidget {
  const NowPlayingScreen({
    super.key,
    required this.api,
    required this.core,
    required this.active,
    this.onReachability,
    this.pollInterval = const Duration(seconds: 5),
    this.clock = monotonicNow,
  });

  final CompanionApi api;
  final CoreInfo core;
  final bool active;
  final void Function(bool ok)? onReachability;
  final Duration pollInterval;
  final MonotonicClock clock;

  @override
  State<NowPlayingScreen> createState() => _NowPlayingScreenState();
}

class _NowPlayingScreenState extends State<NowPlayingScreen> {
  late final Poller _poller = Poller(
    interval: widget.pollInterval,
    onTick: _load,
  );
  NowPlayingStatus? _status;
  Duration _fetchedAt = Duration.zero;
  String? _error;
  bool _loadedOnce = false;
  Timer? _ticker;

  bool get _hasApi => widget.core.companionPort != null;

  @override
  void initState() {
    super.initState();
    _poller.attach();
    _activate();
  }

  @override
  void didUpdateWidget(NowPlayingScreen old) {
    super.didUpdateWidget(old);
    if (old.core != widget.core) {
      _status = null;
      _error = null;
      _loadedOnce = false;
    }
    if (old.active != widget.active || old.core != widget.core) _activate();
  }

  void _activate() {
    _poller.active = widget.active && _hasApi;
    if (widget.active && _hasApi && !_loadedOnce) {
      _loadedOnce = true;
      _load();
    }
    _syncTicker();
  }

  /// A 1 s ticker advances the progress bar between polls while a track plays.
  void _syncTicker() {
    final np = _status?.nowPlaying;
    final want =
        widget.active && np != null && np.playing && np.durationSecs > 0;
    if (want && _ticker == null) {
      _ticker = Timer.periodic(const Duration(seconds: 1), (_) {
        if (mounted) setState(() {});
      });
    } else if (!want) {
      _ticker?.cancel();
      _ticker = null;
    }
  }

  @override
  void dispose() {
    _ticker?.cancel();
    _poller.dispose();
    super.dispose();
  }

  Future<void> _load() async {
    final core = widget.core;
    try {
      final status = await widget.api.fetchNowPlaying(core);
      if (!mounted || core != widget.core) return;
      setState(() {
        _status = status;
        _fetchedAt = widget.clock();
        _error = null;
      });
      _syncTicker();
      widget.onReachability?.call(true);
    } catch (e) {
      if (!mounted || core != widget.core) return;
      setState(() => _error = describeError(e));
      widget.onReachability?.call(false);
    }
  }

  @override
  Widget build(BuildContext context) {
    if (!_hasApi) return const NoCompanionApiPanel();
    final status = _status;
    if (status == null) {
      if (_error != null) {
        return MessagePanel(
          icon: Icons.cloud_off,
          title: "Couldn't load what's playing",
          message: _error,
          action: FilledButton(onPressed: _load, child: const Text('Retry')),
        );
      }
      return const Center(child: CircularProgressIndicator());
    }
    final np = status.nowPlaying;
    final Widget body;
    if (np == null) {
      body = status.linked || status.radio
          ? const MessagePanel(
              icon: Icons.music_off_outlined,
              title: 'Nothing playing',
              message: 'Ask the assistant to play something.',
            )
          : const MessagePanel(
              icon: Icons.link_off,
              title: 'Spotify not linked',
              message:
                  'Link Spotify on the Core (Integrations → Spotify), or ask '
                  'for a radio station.',
            );
    } else {
      body = _Playing(
        track: np,
        elapsedSinceFetch: np.playing
            ? widget.clock() - _fetchedAt
            : Duration.zero,
      );
    }
    return RefreshIndicator(onRefresh: _load, child: body);
  }
}

class _Playing extends StatelessWidget {
  const _Playing({required this.track, required this.elapsedSinceFetch});

  final NowPlayingTrack track;
  final Duration elapsedSinceFetch;

  bool get _isRadio => track.source == 'radio';

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final duration = track.durationSecs;
    final position = duration > 0
        ? (track.positionSecs + elapsedSinceFetch.inSeconds).clamp(0, duration)
        : 0;
    final subtitle = [
      track.artist,
      if (!_isRadio) track.album,
    ].where((s) => s.isNotEmpty).join(' · ');
    return ListView(
      padding: const EdgeInsets.all(24),
      children: [
        Center(
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 320),
            child: AspectRatio(
              aspectRatio: 1,
              child: ClipRRect(
                borderRadius: BorderRadius.circular(16),
                child: _Artwork(url: track.artworkUri, radio: _isRadio),
              ),
            ),
          ),
        ),
        const SizedBox(height: 24),
        if (_isRadio && track.stationName.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(bottom: 4),
            child: Row(
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                Icon(Icons.radio, size: 16, color: theme.colorScheme.primary),
                const SizedBox(width: 6),
                Text(
                  track.stationName,
                  style: theme.textTheme.labelLarge?.copyWith(
                    color: theme.colorScheme.primary,
                  ),
                ),
              ],
            ),
          ),
        Text(
          track.trackTitle.isEmpty ? 'Unknown track' : track.trackTitle,
          style: theme.textTheme.headlineSmall,
          textAlign: TextAlign.center,
        ),
        if (subtitle.isNotEmpty) ...[
          const SizedBox(height: 4),
          Text(
            subtitle,
            style: theme.textTheme.bodyLarge?.copyWith(
              color: theme.colorScheme.onSurfaceVariant,
            ),
            textAlign: TextAlign.center,
          ),
        ],
        const SizedBox(height: 24),
        if (duration > 0) ...[
          LinearProgressIndicator(
            key: const Key('now-playing-progress'),
            value: position / duration,
          ),
          const SizedBox(height: 6),
          Row(
            mainAxisAlignment: MainAxisAlignment.spaceBetween,
            children: [
              Text(formatClock(position), style: theme.textTheme.bodySmall),
              Text(formatClock(duration), style: theme.textTheme.bodySmall),
            ],
          ),
        ],
        const SizedBox(height: 16),
        Row(
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            Icon(
              track.playing ? Icons.play_arrow : Icons.pause,
              color: theme.colorScheme.primary,
            ),
            const SizedBox(width: 4),
            Text(
              track.playing ? 'Playing' : 'Paused',
              style: theme.textTheme.labelLarge,
            ),
            if (track.volumePercent > 0) ...[
              const SizedBox(width: 16),
              const Icon(Icons.volume_up_outlined, size: 18),
              const SizedBox(width: 4),
              Text(
                '${track.volumePercent}%',
                style: theme.textTheme.labelLarge,
              ),
            ],
          ],
        ),
      ],
    );
  }
}

class _Artwork extends StatelessWidget {
  const _Artwork({required this.url, required this.radio});
  final String url;
  final bool radio;

  @override
  Widget build(BuildContext context) {
    final placeholder = ColoredBox(
      color: Theme.of(context).colorScheme.surfaceContainerHighest,
      child: Icon(
        radio ? Icons.radio : Icons.album,
        size: 96,
        color: Theme.of(context).colorScheme.outline,
      ),
    );
    if (url.isEmpty) return placeholder;
    // Artwork is a public image URL (Spotify CDN / station logo) loaded by
    // Flutter's image cache — presentation, not Core networking.
    return Image.network(
      url,
      fit: BoxFit.cover,
      errorBuilder: (_, _, _) => placeholder,
    );
  }
}
