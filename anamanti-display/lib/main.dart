// Ambient Smart Display — app entry point (Plan.MD §3, Phases 5–6).
//
// An always-on landscape screen showing an idle photo slideshow that gives way to
// a live transcript + streamed reply during a voice turn, with returned TTS audio
// played by the Rust engine. Phase 6 adds a settings screen (wake word, assistant
// backend + voice, photo source, memory) reachable from a discreet control on the
// ambient screen; changing device-local settings restarts the engine and refreshes
// the slideshow, while assistant/memory settings are applied on the Mac.

import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/engine/model_assets.dart';
import 'package:anamanti_display/src/engine/notification_controller.dart';
import 'package:anamanti_display/src/engine/screen_brightness.dart';
import 'package:anamanti_display/src/engine/wakeword_config.dart';
import 'package:anamanti_display/src/settings/app_settings.dart';
import 'package:anamanti_display/src/settings/orchestrator_client.dart';
import 'package:anamanti_display/src/settings/settings_store.dart';
import 'package:anamanti_display/src/slideshow/ambient_photos.dart';
import 'package:anamanti_display/src/slideshow/drive_photos.dart';
import 'package:anamanti_display/src/slideshow/google_oauth_config.dart';
import 'package:anamanti_display/src/slideshow/photo_source.dart';
import 'package:anamanti_display/src/ui/ambient_screen.dart';
import 'package:anamanti_display/src/ui/settings_screen.dart';
import 'package:anamanti_display/src/ui/slideshow_view.dart';
import 'package:anamanti_display/src/rust/api/engine.dart'
    show NotifyConfig, noteUserActivity;
import 'package:anamanti_display/src/rust/frb_generated.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  // Immersive full-screen kiosk: hide the Android status bar (top) and nav bar
  // (bottom) so the ambient photo fills the whole display. `immersiveSticky`
  // re-hides them automatically after the transient reveal from an edge swipe.
  await SystemChrome.setEnabledSystemUIMode(SystemUiMode.immersiveSticky);
  await RustLib.init();
  runApp(const AmbientDisplayApp());
}

class AmbientDisplayApp extends StatelessWidget {
  const AmbientDisplayApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Ambient Display',
      debugShowCheckedModeBanner: false,
      theme: ThemeData.dark(useMaterial3: true),
      home: const AmbientHome(),
    );
  }
}

/// Owns the app's long-lived state: the persisted [AppSettings], the two
/// controllers (voice turn + slideshow), and the orchestrator client. Starts the
/// native engine once the wake-word config is resolved from settings, and reacts to
/// settings changes by restarting the engine and refreshing the slideshow source.
class AmbientHome extends StatefulWidget {
  const AmbientHome({super.key});

  @override
  State<AmbientHome> createState() => _AmbientHomeState();
}

class _AmbientHomeState extends State<AmbientHome> {
  final SettingsStore _store = SettingsStore();
  // Rebuilt whenever the selected orchestrator changes (boot + settings apply) so
  // every control call is pinned to the chosen Mac.
  OrchestratorClient _client = const FrbOrchestratorClient();
  final SlideshowController _slideshow = SlideshowController();

  /// Actuates the window backlight from the camera proximity sensor's presence
  /// state (Plan.MD §5). Long-lived across engine restarts so it only crosses the
  /// platform channel when the target brightness actually changes.
  final ScreenBrightnessController _brightness = ScreenBrightnessController();

  AppSettings _settings = const AppSettings();
  AssistantController? _assistant;

  /// Proactive-notification channel (Approach A). Rebuilt alongside the assistant so
  /// it re-pins to the selected orchestrator when that changes.
  NotificationController? _notifications;

  /// Live access tokens for each Google backend (minted from the persisted refresh
  /// tokens on boot / after a re-link). Null when unlinked/offline → local gradients.
  String? _ambientAccessToken;
  String? _driveAccessToken;

  /// Periodically re-mints the access token + re-lists the source so an always-on
  /// frame never goes stale (tokens + media URLs expire ~1 h).
  Timer? _photoRefreshTimer;

  @override
  void initState() {
    super.initState();
    // Show the ambient slideshow immediately, independent of settings/connectivity.
    _slideshow.start();
    _boot();
  }

  Future<void> _boot() async {
    _settings = await _store.load();
    // Pin the control client to the persisted orchestrator selection.
    _client = FrbOrchestratorClient(orchestratorKey: _settings.orchestratorKey);
    // Unpack the bundled wake-word models to the filesystem before the native
    // engine tries to load them (no-op on later runs / user-dropped models).
    await ensureWakeWordModels();
    // Start the engine + UI chrome (clock, status, settings gear) FIRST so the
    // screen is usable immediately. The photo/token refresh below is network-bound
    // (mDNS + a Wyoming control round trip to the orchestrator) and must never gate
    // the UI — otherwise a slow/older/unreachable orchestrator would leave the
    // screen stuck on the loading background with no chrome.
    await _startEngine();
    // Best-effort, off the critical path: mint Google tokens (incl. the Drive bundle
    // synced from the orchestrator) and apply the selected photo source. The
    // slideshow already shows local gradients until this resolves.
    unawaited(() async {
      await _refreshGoogleTokens();
      await _applyPhotoSource();
    }());
    // Keep a linked Google source fresh: tokens + media/thumbnail URLs expire ~1 h,
    // so re-mint + re-list every 30 min (well inside that window) so re-downloads
    // never fail on an always-on frame.
    _photoRefreshTimer?.cancel();
    _photoRefreshTimer = Timer.periodic(
      const Duration(minutes: 30),
      (_) => _reloadPhotos(),
    );
  }

  /// Refresh the linked Google source in place (re-mint token + re-list). Keeps the
  /// current slideshow if the refresh fails (transient offline) rather than dropping
  /// to gradients.
  Future<void> _reloadPhotos() async {
    if (_settings.photoSource == PhotoSourceKind.local) return;
    await _refreshGoogleTokens();
    final ok = _settings.photoSource == PhotoSourceKind.ambient
        ? (_ambientAccessToken?.isNotEmpty ?? false)
        : (_driveAccessToken?.isNotEmpty ?? false);
    if (ok) await _applyPhotoSource();
  }

  /// Mint fresh access tokens from the persisted refresh tokens so the slideshow
  /// resumes a linked source without re-linking. Each backend uses its own OAuth
  /// client (Ambient = TV client; Drive = Desktop client). Leaves a token null when
  /// unconfigured/unlinked/offline (→ local gradients).
  Future<void> _refreshGoogleTokens() async {
    _ambientAccessToken = null;
    _driveAccessToken = null;
    if (kGoogleOAuthConfigured &&
        _settings.ambientLinked &&
        _settings.ambientRefreshToken.isNotEmpty) {
      final client = AmbientApiClient();
      try {
        _ambientAccessToken = (await client.refresh(
          _settings.ambientRefreshToken,
        )).accessToken;
      } catch (_) {
        _ambientAccessToken = null;
      } finally {
        client.close();
      }
    }
    // Drive is orchestrator-owned: pull the latest client creds + refresh token +
    // folder ids from the Mac (which runs consent), then mint an access token
    // on-device. The APK ships no Drive credentials — they arrive over Wyoming.
    if (_settings.photoSource == PhotoSourceKind.drive) {
      await _syncDriveFromOrchestrator();
    }
    if (_settings.driveConfigured &&
        _settings.driveLinked &&
        _settings.driveRefreshToken.isNotEmpty) {
      // Drive's refresh must use the Desktop client that issued the token (synced
      // from the orchestrator).
      final client = AmbientApiClient(
        clientId: _settings.driveClientId,
        clientSecret: _settings.driveClientSecret,
      );
      try {
        _driveAccessToken = (await client.refresh(
          _settings.driveRefreshToken,
        )).accessToken;
      } catch (_) {
        _driveAccessToken = null;
      } finally {
        client.close();
      }
    }
  }

  /// Pull the Google Drive bundle (client creds + refresh token + folder ids) from
  /// the orchestrator, which owns consent, and persist it so the slideshow keeps
  /// working offline afterward. Silently keeps the last-synced values if the Mac is
  /// unreachable, or if the orchestrator has no Drive credentials configured.
  Future<void> _syncDriveFromOrchestrator() async {
    try {
      final t = await _client.fetchDriveToken();
      if (!t.configured) return; // orchestrator not set up for Drive; keep our values
      final next = _settings.copyWith(
        driveClientId: t.clientId,
        driveClientSecret: t.clientSecret,
        driveRefreshToken: t.refreshToken,
        driveFolderIds: t.folderIds,
        driveLinked: t.linked,
      );
      if (next != _settings) {
        _settings = next;
        await _store.save(_settings);
      }
    } catch (_) {
      // Offline / unreachable: keep the persisted bundle.
    }
  }

  /// List the user's picked Google Photos via the Ambient API for the slideshow.
  Future<List<PhotoItem>> _listAmbientMedia({
    required String accessToken,
  }) async {
    final client = AmbientApiClient();
    try {
      return await client.listMediaItems(accessToken);
    } finally {
      client.close();
    }
  }

  Future<void> _applyPhotoSource() async {
    // Build the selected Google source from its live token; with no token
    // (unlinked/offline) this resolves to the local ambient source.
    final source = photoSourceFromSettings(
      _settings,
      ambientLister: _listAmbientMedia,
      driveLister: listDrivePhotos,
      ambientAccessToken: _ambientAccessToken,
      driveAccessToken: _driveAccessToken,
    );
    await _slideshow.setSource(source);
  }

  Future<void> _startEngine() async {
    final config = await buildWakeWordConfigFrom(_settings);
    if (!mounted) return;
    _assistant?.dispose();
    final assistant = AssistantController(
      config: config,
      // A voice turn counts as activity: reset the screen-dim countdown (and
      // brighten a dimmed screen) so a hands-free conversation keeps the display awake.
      onUserActivity: noteUserActivity,
      // Local end-of-speech cue tuning (device-local, A/B-adjustable in settings):
      // flip to a "processing" indicator the instant the user stops talking.
      endpointCueEnabled: _settings.endpointCueEnabled,
      endpointSilence: Duration(milliseconds: _settings.endpointSilenceMs),
      endpointRmsThreshold: _settings.endpointRmsThreshold,
      // While the status is offline, poll the orchestrator every few seconds so
      // the UI recovers on its own (e.g. after the Mac restarts) instead of
      // waiting for the next wake word. A control-protocol fetch is a full
      // discover+connect handshake, so success means we're genuinely reachable.
      probeOrchestrator: () async {
        try {
          // Pin the probe to the selected orchestrator; otherwise the Offline
          // chip could clear against a *different* Mac while the pinned one is
          // still down (strict selection).
          await FrbOrchestratorClient(
            discoveryTimeoutSecs: 2,
            orchestratorKey: _settings.orchestratorKey,
          ).fetchSettings();
          return true;
        } catch (_) {
          return false;
        }
      },
    )..start();
    // Actuate the screen backlight whenever the proximity sensor's presence flips.
    // The old controller (if any) was just disposed, dropping its listeners.
    assistant.addListener(() => _brightness.apply(assistant.state.userPresent));

    // Proactive-notification channel: a persistent, device-dialed connection to the
    // pinned orchestrator that receives pushed visual notifications (Approach A).
    // Independent of the voice engine, but re-pinned here when the orchestrator
    // selection changes (which already triggers an engine restart).
    _notifications?.dispose();
    final notifications = NotificationController(
      config: NotifyConfig(
        orchestratorKey: _settings.orchestratorKey,
        discoveryTimeoutSecs: BigInt.zero,
        deviceId: 'anamanti-display',
      ),
    )..start();

    setState(() {
      _assistant = assistant;
      _notifications = notifications;
    });
  }

  /// Apply settings changed on the [SettingsScreen] (already persisted there):
  /// refresh the slideshow if the photo source changed and restart the engine if a
  /// wake-word/threshold knob changed. Assistant + memory settings are applied on
  /// the Mac by the settings screen itself.
  Future<void> _onSettingsApplied(AppSettings next) async {
    final engineChanged =
        next.orchestratorKey != _settings.orchestratorKey ||
        next.wakeWord != _settings.wakeWord ||
        next.threshold != _settings.threshold ||
        next.activeThreshold != _settings.activeThreshold ||
        next.smoothingWindow != _settings.smoothingWindow ||
        next.fireOnPeak != _settings.fireOnPeak ||
        next.playbackBufferSecs != _settings.playbackBufferSecs ||
        next.endpointCueEnabled != _settings.endpointCueEnabled ||
        next.endpointSilenceMs != _settings.endpointSilenceMs ||
        next.endpointRmsThreshold != _settings.endpointRmsThreshold ||
        next.dimDelaySecs != _settings.dimDelaySecs;
    final photoChanged =
        next.photoSource != _settings.photoSource ||
        next.ambientRefreshToken != _settings.ambientRefreshToken ||
        next.ambientDeviceId != _settings.ambientDeviceId ||
        next.ambientLinked != _settings.ambientLinked ||
        next.driveRefreshToken != _settings.driveRefreshToken ||
        !listEquals(next.driveFolderIds, _settings.driveFolderIds) ||
        next.driveLinked != _settings.driveLinked;

    _settings = next;
    // Re-pin the control client to the (possibly new) orchestrator selection.
    _client = FrbOrchestratorClient(orchestratorKey: _settings.orchestratorKey);
    if (photoChanged) {
      // A new/changed link means new refresh tokens: re-mint before rebuilding.
      await _refreshGoogleTokens();
      await _applyPhotoSource();
    }
    if (engineChanged) await _startEngine();
  }

  void _openSettings() {
    Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (_) => SettingsScreen(
          initial: _settings,
          store: _store,
          client: _client,
          onApplied: _onSettingsApplied,
        ),
      ),
    );
  }

  @override
  void dispose() {
    _photoRefreshTimer?.cancel();
    _assistant?.dispose();
    _notifications?.dispose();
    _slideshow.dispose();
    _brightness.reset();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final assistant = _assistant;
    final Widget content;
    if (assistant == null) {
      // Before the engine config resolves, still show the ambient slideshow so the
      // screen is never blank on startup.
      content = Scaffold(
        backgroundColor: Colors.black,
        body: SlideshowView(controller: _slideshow),
      );
    } else {
      content = AmbientScreen(
        assistant: assistant,
        slideshow: _slideshow,
        notifications: _notifications,
        onOpenSettings: _openSettings,
      );
    }
    // A screen touch counts as user activity: reset the dim countdown (and brighten
    // a dimmed screen). Translucent so it observes every touch without stealing it
    // from the widgets below (settings control, timer chips, notification banner).
    return Listener(
      behavior: HitTestBehavior.translucent,
      onPointerDown: (_) => noteUserActivity(),
      child: content,
    );
  }
}
