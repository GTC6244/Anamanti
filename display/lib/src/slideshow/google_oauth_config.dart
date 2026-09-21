// Google OAuth client credentials for the two photo backends.
//
// Injected at build time and intentionally NOT committed:
//   flutter build apk --release --target-platform android-arm \
//     --dart-define-from-file=google_oauth.json
// `google_oauth.json` is gitignored; `google_oauth.example.json` is the template.
//
// There are TWO OAuth clients because the two backends need different flows:
//
//  * **Ambient API** (Google Photos, partner-gated) uses the on-device device-code
//    (QR) flow → an OAuth client of type **"TVs and Limited Input devices"**.
//    Credentials: [kGoogleOAuthClientId] / [kGoogleOAuthClientSecret].
//
//  * **Google Drive** (`drive.readonly`) can't consent on-device (the device-code
//    flow rejects Drive scopes), so consent runs once in a browser on the Mac via a
//    loopback flow → an OAuth client of type **"Desktop app"**. Credentials:
//    [kGoogleDriveClientId] / [kGoogleDriveClientSecret]. The device only refreshes
//    the resulting token, which must use the same (Desktop) client.
//
// A backend whose credentials are absent is simply unavailable (its config getter
// is false); the settings screen degrades honestly rather than attempting it.

// --- Ambient API client ("TVs and Limited Input devices") --------------------
const String kGoogleOAuthClientId = String.fromEnvironment(
  'GOOGLE_OAUTH_CLIENT_ID',
  defaultValue: '',
);
const String kGoogleOAuthClientSecret = String.fromEnvironment(
  'GOOGLE_OAUTH_CLIENT_SECRET',
  defaultValue: '',
);

/// True when the Ambient (TV) client credentials were provided at build time.
bool get kGoogleOAuthConfigured =>
    kGoogleOAuthClientId.isNotEmpty && kGoogleOAuthClientSecret.isNotEmpty;

// --- Drive client ("Desktop app", loopback consent on the Mac) ---------------
const String kGoogleDriveClientId = String.fromEnvironment(
  'GOOGLE_DRIVE_CLIENT_ID',
  defaultValue: '',
);
const String kGoogleDriveClientSecret = String.fromEnvironment(
  'GOOGLE_DRIVE_CLIENT_SECRET',
  defaultValue: '',
);

/// True when the Drive (Desktop) client credentials were provided at build time.
bool get kGoogleDriveConfigured =>
    kGoogleDriveClientId.isNotEmpty && kGoogleDriveClientSecret.isNotEmpty;
