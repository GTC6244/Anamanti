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
# NOTES is free text shown in the device update banner; quotes, backslashes and
# non-ASCII are fine — latest.json is serialized with python3's json module (never
# string-pasted), and re-parsed before anything is uploaded.
#
# Bump `version:` in pubspec.yaml first (e.g. 1.2.0+12). The `+N` build number is
# the Android versionCode and MUST increase every release.
#
# Stale-UI guard: the build starts from a clean Gradle output dir (build/app), and
# before publishing the script compares the new APK's compiled Dart UI
# (lib/armeabi-v7a/libapp.so) with the currently published release's. Identical ⇒
# it refuses to publish — that is how 1.0.18 shipped 1.0.17's UI (a stale Gradle
# mergeJniLibFolders output was repackaged). For a deliberate native/Rust-only
# release with no Dart change, set ALLOW_SAME_DART=1.

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
# latest.json is written with python3's json module so NOTES is always escaped
# correctly. Check for it now, before the multi-minute build, not after.
command -v python3 >/dev/null 2>&1 || { echo "error: python3 is required to write latest.json" >&2; exit 1; }
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
# Start from a clean Gradle output dir: an incremental build has been seen to keep a
# stale merged libapp.so (Gradle's mergeJniLibFolders reported up-to-date while
# Flutter's fresh AOT output had changed), silently repackaging an old Dart UI. The
# Flutter (.dart_tool) and Cargo caches are kept, so this only re-runs packaging.
rm -rf build/app
flutter build apk --release --flavor selfUpdate --target-platform android-arm
APK_SRC="build/app/outputs/flutter-apk/app-selfUpdate-release.apk"
[ -f "$APK_SRC" ] || { echo "error: APK not found at $APK_SRC" >&2; exit 1; }

# --- SHA-256 helper (portable across macOS/Linux): file arg, or stdin when omitted ---
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$@" | awk '{print $1}'
  else
    shasum -a 256 "$@" | awk '{print $1}'
  fi
}
SHA256="$(sha256 "$APK_SRC")"
echo "sha256=$SHA256"

# --- stale-UI guard: refuse to republish the previous release's compiled Dart UI ---
APP_SO="lib/armeabi-v7a/libapp.so"
NEW_DART="$(unzip -p "$APK_SRC" "$APP_SO" | sha256)"
PREV_URL="$(curl -fsS "$UPDATE_BASE_URL/latest.json" 2>/dev/null \
  | sed -n 's/.*"apkUrl"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' || true)"
if [ -z "$PREV_URL" ]; then
  echo "note: no published latest.json/apkUrl found — skipping the stale-UI check"
else
  PREV_APK="$(mktemp)"
  if curl -fsS -o "$PREV_APK" "$PREV_URL"; then
    PREV_DART="$(unzip -p "$PREV_APK" "$APP_SO" | sha256)"
    echo "libapp.so: new=${NEW_DART:0:12} published=${PREV_DART:0:12} ($PREV_URL)"
    if [ "$NEW_DART" = "$PREV_DART" ] && [ "${ALLOW_SAME_DART:-0}" != "1" ]; then
      rm -f "$PREV_APK"
      echo "error: the new APK's compiled Dart UI ($APP_SO) is byte-identical to the" >&2
      echo "  published release's. Either the build reused a stale libapp.so (try a" >&2
      echo "  'flutter clean' and rebuild), or this release has no Dart change — then" >&2
      echo "  re-run with ALLOW_SAME_DART=1. Nothing was uploaded." >&2
      exit 1
    fi
  else
    echo "warning: couldn't download the published APK ($PREV_URL) — skipping the stale-UI check" >&2
  fi
  rm -f "$PREV_APK"
fi

# --- write latest.json ---
OUT_DIR="build/r2"
mkdir -p "$OUT_DIR"
cp "$APK_SRC" "$OUT_DIR/$APK_NAME"
# Serialize with a real JSON encoder, never by pasting values into a template: a
# double quote in NOTES (e.g. NOTES='Say "stop"') once produced an invalid manifest
# that devices could not parse, so they silently saw no update. Values are passed
# through the environment (not interpolated into code), then the file is re-parsed
# so a bad manifest can never be uploaded.
VERSION_CODE="$VERSION_CODE" VERSION_NAME="$VERSION_NAME" \
APK_URL="${UPDATE_BASE_URL}/${APK_NAME}" SHA256="$SHA256" NOTES="$NOTES" \
python3 -I - "$OUT_DIR/latest.json" <<'PY'
import json, os, sys
manifest = {
    "versionCode": int(os.environ["VERSION_CODE"]),
    "versionName": os.environ["VERSION_NAME"],
    "apkUrl": os.environ["APK_URL"],
    "sha256": os.environ["SHA256"],
    "notes": os.environ["NOTES"],
}
with open(sys.argv[1], "w", encoding="utf-8") as f:
    json.dump(manifest, f, indent=2, ensure_ascii=False)
    f.write("\n")
with open(sys.argv[1], encoding="utf-8") as f:
    assert json.load(f) == manifest, "latest.json did not round-trip"
PY
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
