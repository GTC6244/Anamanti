#!/usr/bin/env bash
# Build + publish an Anamanti Display release to Cloudflare R2 for the in-app
# updater (plans/UpdaterPlan.md).
#
# Produces a signed `selfUpdate`-flavor release APK, computes its SHA-256, writes a
# latest.json manifest, and uploads both to your R2 bucket with `wrangler`. The
# device's in-app updater fetches `<UPDATE_BASE_URL>/latest.json`, compares its
# versionCode to the running build, and downloads + verifies the APK.
#
# This does NOT replace the GitHub Releases + Obtainium path (release.yml) — both
# run for now; retire Obtainium later once the in-app path is hardware-proven.
#
# Prerequisites (one-time):
#   * A release keystore wired via android/key.properties or ANDROID_* env vars
#     (see android/app/build.gradle.kts). The SAME key must sign every release —
#     a PackageInstaller self-update requires a matching signature.
#   * `wrangler` logged in (`npx wrangler login`) with access to the bucket.
#   * An R2 bucket with a custom domain (UPDATE_BASE_URL) and a ~60 s edge cache
#     rule on latest.json (or purge it each release).
#
# Usage:
#   R2_BUCKET=anamanti-dl \
#   UPDATE_BASE_URL=https://dl.example.com \
#   NOTES="Bug fixes" \
#   anamanti-display/scripts/release-r2.sh
#
# Bump `version:` in pubspec.yaml first (e.g. 1.2.0+12). The `+N` build number is
# the Android versionCode and MUST increase every release.

set -euo pipefail

# --- locate the Flutter project root (this script lives in <root>/scripts) ---
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

# --- required configuration ---
: "${R2_BUCKET:?Set R2_BUCKET to your Cloudflare R2 bucket name}"
: "${UPDATE_BASE_URL:?Set UPDATE_BASE_URL to your public R2 domain, e.g. https://dl.example.com}"
NOTES="${NOTES:-Bug fixes and improvements}"
WRANGLER="${WRANGLER:-npx wrangler}"
UPDATE_BASE_URL="${UPDATE_BASE_URL%/}" # strip any trailing slash

# Derive the R2 key prefix from any path component of UPDATE_BASE_URL. The bucket
# is shared across products (e.g. immediacy-releases), so Anamanti lives under a
# per-product subpath like .../anamanti → objects are uploaded as `anamanti/…`.
# A bare domain (no path) yields an empty prefix (bucket root).
R2_PREFIX="$(printf '%s' "$UPDATE_BASE_URL" | sed -E 's#^https?://[^/]+##; s#^/##')"
[ -n "$R2_PREFIX" ] && R2_PREFIX="${R2_PREFIX}/"

# --- read version + versionCode from pubspec.yaml (version: X.Y.Z+N) ---
VERSION_LINE="$(grep -m1 '^version:' pubspec.yaml | sed 's/version:[[:space:]]*//')"
VERSION_NAME="${VERSION_LINE%%+*}"
VERSION_CODE="${VERSION_LINE##*+}"
if [ -z "$VERSION_NAME" ] || [ -z "$VERSION_CODE" ] || [ "$VERSION_NAME" = "$VERSION_CODE" ]; then
  echo "error: pubspec.yaml version must be 'X.Y.Z+N' (got '$VERSION_LINE')" >&2
  exit 1
fi
APK_NAME="app-${VERSION_NAME}.apk"
echo "Releasing versionName=$VERSION_NAME versionCode=$VERSION_CODE"

# --- build the signed selfUpdate release APK ---
# android-arm: the Echo Show 8 (crown) is 32-bit armeabi-v7a (see agents.md).
echo "Building signed selfUpdate release APK…"
flutter build apk --release --flavor selfUpdate --target-platform android-arm
APK_SRC="build/app/outputs/flutter-apk/app-selfUpdate-release.apk"
[ -f "$APK_SRC" ] || { echo "error: APK not found at $APK_SRC" >&2; exit 1; }

# --- compute SHA-256 (portable across macOS/Linux) ---
if command -v sha256sum >/dev/null 2>&1; then
  SHA256="$(sha256sum "$APK_SRC" | awk '{print $1}')"
else
  SHA256="$(shasum -a 256 "$APK_SRC" | awk '{print $1}')"
fi
echo "sha256=$SHA256"

# --- write latest.json ---
OUT_DIR="build/r2"
mkdir -p "$OUT_DIR"
cp "$APK_SRC" "$OUT_DIR/$APK_NAME"
cat > "$OUT_DIR/latest.json" <<JSON
{
  "versionCode": ${VERSION_CODE},
  "versionName": "${VERSION_NAME}",
  "apkUrl": "${UPDATE_BASE_URL}/${APK_NAME}",
  "sha256": "${SHA256}",
  "notes": "${NOTES}"
}
JSON
echo "Wrote $OUT_DIR/latest.json:"
cat "$OUT_DIR/latest.json"

# --- upload: APK first, then latest.json (so the manifest never points at a
#     not-yet-uploaded APK) ---
# NOTE: --remote is REQUIRED. Without it, wrangler v3+ writes to a LOCAL simulator
# store ("Resource location: local") and nothing is published to the real bucket —
# the device then keeps 404ing. Always target the remote R2 instance.
echo "Uploading APK → r2://$R2_BUCKET/${R2_PREFIX}$APK_NAME"
$WRANGLER r2 object put "$R2_BUCKET/${R2_PREFIX}$APK_NAME" --file "$OUT_DIR/$APK_NAME" \
  --content-type application/vnd.android.package-archive --remote
echo "Uploading manifest → r2://$R2_BUCKET/${R2_PREFIX}latest.json"
$WRANGLER r2 object put "$R2_BUCKET/${R2_PREFIX}latest.json" --file "$OUT_DIR/latest.json" \
  --content-type application/json --remote

echo
echo "Done. Devices will pick up versionCode $VERSION_CODE on their next check"
echo "(within the latest.json edge-cache TTL). If you didn't set a short cache"
echo "rule, purge $UPDATE_BASE_URL/latest.json in the Cloudflare dashboard."
