# Releasing EasyHome to the Echo Show fleet

How new builds of **EasyHome** (`com.ambientdisplay.ambient_display`) get published to
GitHub Releases and auto-installed on the Echo Show 8 (gen1, LineageOS 18.1 / Android 11)
devices around the house via **Obtainium**.

## How it works

1. You bump the version and push a `v*` git tag.
2. GitHub Actions (`.github/workflows/release.yml`) builds a **signed universal APK**
   (Flutter + Rust/cargokit) and attaches it to a GitHub Release.
3. **Obtainium** on each Echo Show polls this repo's releases over WiFi and installs the
   new APK silently (via root or Shizuku) — no taps, no cables.

```
  bump pubspec  ──►  git tag vX.Y.Z  ──►  GitHub Actions builds+signs+releases APK
                                                        │
                                                        ▼
                        each Echo Show's Obtainium polls releases ──► auto-installs
```

## Files involved

| File | Purpose |
|------|---------|
| `.github/workflows/release.yml` | Builds signed APK on `v*` tag, publishes the Release |
| `display/android/app/build.gradle.kts`  | Release signing config (env vars in CI, `key.properties` locally, debug fallback) |
| `.gitignore`                    | Blocks keystores / `key.properties` from being committed |

---

## One-time setup

### Step 1 — Create the permanent release keystore

```bash
keytool -genkey -v -keystore easyhome-release.jks \
  -keyalg RSA -keysize 2048 -validity 10000 -alias easyhome
```

> ⚠️ **Back up `easyhome-release.jks` and its passwords forever.** If you lose this file,
> you can **never** update the app again — Android rejects updates signed with a different
> key, so you'd have to uninstall/reinstall on every device. This is the one irreplaceable
> artifact in the whole setup. Store it somewhere safe and offline.

### Step 2 — Load the keystore into GitHub as secrets

```bash
base64 -i easyhome-release.jks > keystore.b64
gh secret set ANDROID_KEYSTORE_BASE64   -R GTC6244/EasyHome < keystore.b64
gh secret set ANDROID_KEYSTORE_PASSWORD -R GTC6244/EasyHome   # paste store password
gh secret set ANDROID_KEY_ALIAS         -R GTC6244/EasyHome   # -> easyhome
gh secret set ANDROID_KEY_PASSWORD      -R GTC6244/EasyHome   # paste key password
rm keystore.b64
```

### Step 3 (optional) — Local signed release builds

To build signed release APKs on your Mac (not just in CI), create `display/android/key.properties`
(git-ignored):

```properties
storeFile=/absolute/path/to/easyhome-release.jks
storePassword=<store password>
keyAlias=easyhome
keyPassword=<key password>
```

Without this file, `flutter build apk --release` on your Mac falls back to the debug key.

### Step 4 — Configure Obtainium on each Echo Show

- **Add App** → URL: `https://github.com/GTC6244/EasyHome`
  (repo is **public**, so **no token needed**).
- Set **install method → Root** (or **Shizuku** if not rooted).
- Enable **background update checks** + **auto-install**.

After this, each device self-updates over WiFi whenever you publish a release.

### Step 5 — ⚠️ One-time migration off the debug key

Devices currently running a **debug-signed** build will **refuse** the first
**release-signed** APK (signature mismatch). On each existing device, **once**:

```bash
adb uninstall com.ambientdisplay.ambient_display
# then install the new release-signed APK via Obtainium or:
adb install EasyHome-<version>.apk
```

All future updates are then seamless. Do this while the fleet is small.

---

## Shipping a release (every time)

1. **Bump the version** in `display/pubspec.yaml`. The `+N` build number is the Android
   `versionCode` and **must increase every release**, or Android/Obtainium won't see an
   update:
   ```yaml
   version: 1.0.1+2   # was 1.0.0+1
   ```
2. **Commit, tag, push:**
   ```bash
   git commit -am "release 1.0.1"
   git tag v1.0.1
   git push origin main --tags
   ```
3. GitHub Actions builds, signs, and publishes the Release automatically. Within its poll
   interval, every Echo Show's Obtainium picks it up and installs it.

> Keep the tag and pubspec version in sync (`v1.0.1` ↔ `version: 1.0.1+N`). The workflow
> emits a warning if they diverge. Obtainium compares the APK's **embedded** version, so
> the pubspec bump is what actually matters.

---

## Troubleshooting

- **Build fails on NDK version** — the workflow pins `NDK_VERSION: 27.0.12077973` at the
  top of `.github/workflows/release.yml`. If Flutter asks for a different NDK, set that env
  to the version it names.
- **Rust/cargokit build failure** — the cross-compiled Rust (`rust_lib_ambient_display`) is
  the most likely first-run snag. Check the Actions log for the failing target/linker step.
- **Device won't update** — confirm the `versionCode` (`+N`) actually increased, and that
  the device was migrated off the debug key (Step 5).
- **Obtainium shows no releases** — verify the Release has an `.apk` asset attached and the
  repo URL is exactly `https://github.com/GTC6244/EasyHome`.

## Key facts

- **App ID:** `com.ambientdisplay.ambient_display`
- **Target device:** Echo Show 8 gen1 ("crown"), LineageOS 18.1 (Android 11), arm64-v8a
- **APK type:** universal (one asset covers arm64) — simplest for Obtainium
- **Repo visibility:** public (no PAT required in Obtainium)
