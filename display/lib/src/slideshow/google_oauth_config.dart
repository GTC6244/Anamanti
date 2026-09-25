// Google OAuth client credentials for the Ambient (Google Photos) photo backend.
//
// Injected at build time and intentionally NOT committed:
//   flutter build apk --release --target-platform android-arm \
//     --dart-define-from-file=google_oauth.json
// `google_oauth.json` is gitignored; `google_oauth.example.json` is the template.
//
//  * **Ambient API** (Google Photos, partner-gated) uses the on-device device-code
//    (QR) flow → an OAuth client of type **"TVs and Limited Input devices"**.
//    Credentials: [kGoogleOAuthClientId] / [kGoogleOAuthClientSecret]. Absent
//    credentials mean the backend is simply unavailable (the getter is false); the
//    settings screen degrades honestly rather than attempting it.
//
//  * **Google Drive** (`drive.readonly`) is NO LONGER configured here. The
//    orchestrator owns the Drive "Desktop app" OAuth client, runs the one-time
//    consent, and the device pulls the client id/secret + refresh token over Wyoming
//    (`ambient-get-drive-token`) into [AppSettings]. Drive readiness is therefore a
//    runtime check (`AppSettings.driveConfigured`), not a build-time constant.

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
