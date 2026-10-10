// Android runtime-permission inspection + requesting for the Settings → Permissions
// page. Sensing/actuation lives natively (MainActivity hosts the
// `anamanti_display/permissions` channel); this is the thin, injectable Dart side so
// the UI can show grant state and trigger the Android permission dialog.
//
// Why this exists: the display is a kiosk whose Home/launcher *is* this app, so there
// is no natural onboarding moment to prompt for the "dangerous" CAMERA / RECORD_AUDIO
// permissions. Before this, a missing grant (e.g. CAMERA, which powers presence-based
// auto-brightness) could only be fixed over adb. This surfaces it in Settings.

import 'package:flutter/services.dart';

/// One Android permission with human-facing metadata for the Settings list.
class AppPermission {
  const AppPermission({
    required this.name,
    required this.label,
    required this.description,
    required this.runtime,
  });

  /// The full Android permission string (e.g. `android.permission.CAMERA`).
  final String name;

  /// Short human label shown as the row title.
  final String label;

  /// What the permission powers, shown as the row subtitle.
  final String description;

  /// True for a "dangerous" runtime permission the user can grant on demand (shown
  /// with a Grant button when missing); false for an install-time permission that is
  /// always granted and therefore shown read-only.
  final bool runtime;
}

/// The Android CAMERA permission name — the one whose grant must (re)start the engine
/// so the proximity sensor and auto-brightness begin working immediately.
const String kCameraPermission = 'android.permission.CAMERA';

/// Permissions surfaced on Settings → Permissions, in display order. Any declared
/// permission the device reports that is not listed here is still shown (by raw
/// name), so the panel never silently hides something the app requests.
const List<AppPermission> kKnownPermissions = [
  AppPermission(
    name: kCameraPermission,
    label: 'Camera',
    description:
        'Presence detection — keeps the screen bright while you’re nearby and dims '
        'it to the clock when the room is empty.',
    runtime: true,
  ),
  AppPermission(
    name: 'android.permission.RECORD_AUDIO',
    label: 'Microphone',
    description: 'Wake word and voice commands.',
    runtime: true,
  ),
  AppPermission(
    name: 'android.permission.MODIFY_AUDIO_SETTINGS',
    label: 'Audio settings',
    description: 'Adjusts audio routing for spoken replies.',
    runtime: false,
  ),
  AppPermission(
    name: 'android.permission.INTERNET',
    label: 'Internet',
    description: 'Connects to the Anamanti Core and fetches app updates.',
    runtime: false,
  ),
  AppPermission(
    name: 'android.permission.ACCESS_WIFI_STATE',
    label: 'Wi-Fi state',
    description: 'Finds the Core on your network (mDNS discovery).',
    runtime: false,
  ),
  AppPermission(
    name: 'android.permission.CHANGE_WIFI_MULTICAST_STATE',
    label: 'Wi-Fi multicast',
    description: 'Receives mDNS discovery responses.',
    runtime: false,
  ),
];

/// Queries + requests Android runtime permissions over the native
/// `anamanti_display/permissions` channel. The channel is injectable so host tests
/// can fake the platform with no device; on a platform without the channel every
/// call degrades to "unknown" (an empty map) rather than throwing.
class PermissionsController {
  PermissionsController({MethodChannel? channel})
      : _channel =
            channel ?? const MethodChannel('anamanti_display/permissions');

  final MethodChannel _channel;

  /// Current grant state keyed by full permission name. Empty on a platform without
  /// the channel (host tests / no device), which the UI renders as "unknown".
  Future<Map<String, bool>> status() async {
    try {
      final raw = await _channel.invokeMapMethod<String, bool>('status');
      return raw ?? const <String, bool>{};
    } catch (_) {
      return const <String, bool>{};
    }
  }

  /// Fire the Android runtime dialog for [names]; resolves with the refreshed status
  /// once the user responds. Returns an empty map if the channel is absent.
  Future<Map<String, bool>> request(List<String> names) async {
    try {
      final raw = await _channel.invokeMapMethod<String, bool>(
        'request',
        <String, Object?>{'permissions': names},
      );
      return raw ?? const <String, bool>{};
    } catch (_) {
      return const <String, bool>{};
    }
  }

  /// Open this app's system "App info" page (fallback when a permission was
  /// permanently denied and the runtime dialog no longer appears).
  Future<void> openAppSettings() async {
    try {
      await _channel.invokeMethod<void>('openAppSettings');
    } catch (_) {
      // No channel (host/test) or no settings activity: ignore.
    }
  }
}
