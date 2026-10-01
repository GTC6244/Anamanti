# UpdaterPlan — in-app APK auto-updater (Cloudflare R2)

Self-rolled, in-app over-the-air updates for **Anamanti Display** on the Echo Show.
Replaces reliance on the third-party **Obtainium** app (which polls GitHub
Releases) with an updater built into the APK that pulls a signed APK + a
`latest.json` manifest from a **Cloudflare R2** bucket on a custom domain.

See `agents.md` (build + flavor commands), `architecture.md §4` (the design), and
`DisplayUI.md §6 / §9` (the update banner + Updates settings page).

## Why

The kiosk is always-on and headless; updates must be hands-off. The prior path
(CI → GitHub Releases → Obtainium auto-install) works but depends on a third-party
app and a GitHub account. This makes updates first-party and controllable, and
prepares an `fdroid` flavor that ships without the updater.

## Decisions

- **Networking in Rust**, behind the FRB boundary (the "Rust owns networking"
  locked decision). The install step is unavoidably native Kotlin.
- **Keep the GitHub Releases + Obtainium path too** (`.github/workflows/release.yml`)
  for now; retire Obtainium once the in-app path is hardware-proven.
- **Base URL is a device-local setting** (`AppSettings.updateBaseUrl`, default
  placeholder `https://dl.example.com`), editable in Settings → Updates.
- **Client = `ureq` (blocking) + rustls/`ring` + `webpki-roots` + `sha2`**, NOT
  reqwest: reqwest's default aws-lc-rs / native-tls don't reliably cross-compile for
  32-bit armv7 Android; ring does, and ureq fits the codebase's dedicated-`std::thread`
  model with no async-runtime duplication.

## Architecture (who owns what)

| Layer | Owns | Key files |
| --- | --- | --- |
| **Rust engine** | Fetch `latest.json`, stream-download the APK, incremental SHA-256 verify, progress stream | `rust/src/update/mod.rs` (impl), `rust/src/api/updater.rs` (FRB: `check_for_update`, `download_update`, `cancel_download`) |
| **Kotlin channel** | `PackageInstaller` session, unknown-sources redirect, running `versionCode`, flavor flag | `MainActivity.kt` (`anamanti_display/updater`), `InstallReceiver.kt` |
| **Flutter** | Periodic check, version compare, banner + Settings page, orchestration | `lib/src/engine/update_controller.dart`, `lib/src/update/updater_channel.dart`, `lib/src/ui/update_banner.dart`, Settings → Updates in `settings_screen.dart`; wired in `main.dart` |

### Flow
1. On boot + every 6 h (a Dart `Timer`, only on the `selfUpdate` flavor and when
   `AppSettings.autoUpdateEnabled`), `UpdateController` calls Rust
   `check_for_update(baseUrl)`.
2. If `manifest.versionCode > getVersionCode()` (native channel) → status
   `available` → the update banner shows.
3. User taps **Update** → Rust `download_update` streams the APK to the cache dir,
   hashing as it goes; a mismatch/cancel deletes the partial and surfaces an error.
4. On success → check `canRequestPackageInstalls()`; if not granted, open
   `ACTION_MANAGE_UNKNOWN_APP_SOURCES`; else `installApk` commits a
   `PackageInstaller` session. `InstallReceiver` launches the system prompt
   (`STATUS_PENDING_USER_ACTION`), deletes the cached APK on completion, and relays
   the result to Dart.

### Flavors (prepare for F-Droid)
`android/app/build.gradle.kts` defines a `distribution` dimension:
`selfUpdate` (`ENABLE_SELF_UPDATE=true`, ships the updater; `REQUEST_INSTALL_PACKAGES`
+ the receiver live in `src/selfUpdate/AndroidManifest.xml`) and `fdroid`
(`ENABLE_SELF_UPDATE=false`, no permission/receiver, updater UI hidden). Same
`applicationId` + release signing key across flavors. **Every apk build/run must
name a flavor**, e.g. `flutter build apk --release --flavor selfUpdate`.

## Releasing

1. Bump `version:` in `pubspec.yaml` (e.g. `1.2.0+12`). The `+N` build number is the
   Android `versionCode` and **must increase every release**.
2. Sign with the **one** release keystore (`android/key.properties` or `ANDROID_*`
   env). The key + `applicationId` must never change — a `PackageInstaller`
   self-update requires a matching signature.
3. `R2_BUCKET=… UPDATE_BASE_URL=https://dl.example.com NOTES="…" \
   anamanti-display/scripts/release-r2.sh` — builds the signed `selfUpdate` APK,
   computes its SHA-256, writes `latest.json` (template:
   `anamanti-display/scripts/latest.example.json`), and uploads both via `wrangler`.
4. Ensure `latest.json` has a short edge-cache TTL (~60 s) or purge it each release.

### One-time ops (human, not in this repo)
- Generate + **back up** the release keystore (`keytool`); never commit it.
- Create the R2 bucket, attach the custom domain, add the `latest.json` cache rule.
- `npx wrangler login` with access to the bucket.
- On the LineageOS kiosk, grant "install unknown apps" once (adb / Settings).

## Verification
- Rust: `cargo clippy --all-targets -- -D warnings` + `cargo test` (covers manifest
  parse + a live localhost download/verify/cleanup test).
- Flutter: `dart analyze` + `flutter test` (incl. `test/update_controller_test.dart`).
- Build-risk gate (proves `ring` cross-compiles on armv7):
  `flutter build apk --release --flavor selfUpdate --target-platform android-arm`.
- On-device: install the `selfUpdate` APK; put a higher-`versionCode` signed APK +
  `latest.json` on a test base URL; confirm banner → download → SHA-256 → install →
  relaunch. Confirm the `fdroid` flavor shows no updater UI and has no
  `REQUEST_INSTALL_PACKAGES`.

## Status / follow-ups
- [x] Rust fetch/download/verify + FRB; Kotlin install channel + receiver; flavors;
  Flutter controller + banner + Settings page; release script + manifest template;
  docs. Suites green (Rust tests, `dart analyze`, `flutter test`).
- [ ] **Verify on hardware** (the full flow above) — not yet device-tested.
- [ ] Device-local secrets / integrity hardening is unchanged by this work; the
  manifest + APK ride plain HTTPS from a host you control, pinned by SHA-256.
- [ ] Later: retire Obtainium + the GitHub Releases workflow once the in-app path is
  proven; actually build and publish the `fdroid` flavor.
