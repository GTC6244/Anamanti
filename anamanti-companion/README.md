# Anamanti Companion

The **phone app** for Anamanti: an Android + iOS Flutter app wrapped around a Rust
core, built the same way as [Anamanti Display](../anamanti-display/) (Flutter owns
presentation, Rust owns networking/discovery, joined by `flutter_rust_bridge` v2).

It is a **scaled-back version of the display app**. It does not listen, wake, or
speak. It connects to an Anamanti Core on the home LAN to:

- **do some administrative tasks** on the Core,
- **view the pipeline** (the live STT → LLM → TTS stages and their status),
- **see some of what's on screen**, such as running **timers**.

> **Status: v1 built (2026-10-21), not yet tried on a phone against a live Core.**
> Four tabs (Status, Now playing, Timers, Memories) over mDNS discovery, the Core's
> read-only companion HTTP API, and the Wyoming memory frames. Needs a Core with the
> companion listener (`companion_port` in its mDNS TXT) for the first three tabs.
> Open items: see "Known gaps" below.

## Identity

| Thing | Value |
|---|---|
| Dart package | `anamanti_companion` |
| Rust crate (FRB lib) | `rust_lib_anamanti_companion` (`rust/`) |
| Android `applicationId` | `com.anamanti.anamanti_companion` |
| iOS bundle id | `com.anamanti.anamantiCompanion` |
| Platforms | Android, iOS (no desktop/web) |

## How it connects

1. **Find the Core (mDNS only).** Rust browses `_wyoming._tcp.local.` and keeps
   only TXT `role=core`. It reads `instance_id` (the stable key), `name`, and
   `companion_port`, and picks an address the same way the Display does (no
   loopback, unspecified, or link-local addresses; IPv4 first). One Core is picked
   automatically. With more than one, the user picks, and the choice is saved as
   its `instance_id` (shared_preferences). If the saved Core doesn't answer, the
   picker opens with a note. There is no manual-IP fallback.
2. **Status / Now playing / Timers: HTTP GET** to
   `http://<addr>:<companion_port>`: `/overview.json` (`?probe=0` for the 10 s
   refresh, probes on first load and pull-to-refresh), `/music/nowplaying.json`
   (every 5 s), and `/timers.json` (every 5 s). The client is plain HTTP with no
   auth, never sends `Origin` (the Core answers 403 if it does), uses a 3 s
   connect / 8 s total timeout, and follows no redirects. If the TXT record has no
   `companion_port`, these tabs say "This Core doesn't have the companion API
   turned on". The app polls only while a tab is visible and the app is in the
   foreground. This is a deliberate exception to "no polling", because the API is
   request/response HTTP. Timers count down on the phone between polls from
   `remaining_secs`, timed by a monotonic clock. Phone and Mac wall clocks are
   never compared.
3. **Memories: Wyoming control frames** on the Core's Wyoming port (the mDNS
   service port), one short connection per request, the same as the Display's
   Memory screen: `anamanti-list-memories` → `anamanti-memories`,
   `anamanti-delete-memory` / `anamanti-clear-memories` → `anamanti-memory-result`.
   This tab works without the companion API.
4. **Re-browse on failure.** Two failed requests in a row start a quiet mDNS
   re-browse for the same `instance_id`, at most once every 10 s. If the Core
   answers at a new address, the app switches to it. The app-bar dot shows
   green (connected), orange (reconnecting), or red (can't reach the Core).

## Layout

```
lib/
  main.dart                     app entry: RustLib.init, controller, MaterialApp
  src/data/companion_api.dart   CompanionApi seam (+ RustCompanionApi → FRB)
  src/data/selection_store.dart saved Core pick (shared_preferences)
  src/data/overview.dart        lenient parser for /overview.json
  src/state/connection_controller.dart  discovery, picker, re-browse, health
  src/ui/home_shell.dart        search / not-found / picker / 4-tab NavigationBar
  src/ui/{status,now_playing,timers,memories}_screen.dart
  src/ui/common.dart            Poller, MonotonicClock, panels, chips
  src/util/format.dart
  src/rust/                     FRB-GENERATED (do not edit)
rust/src/
  api/companion.rs              FRB API + the data types that cross the bridge
  discovery.rs                  mDNS browse, role filter, choose_address, companion_port
  http.rs                       ureq GET client (no TLS, timeouts, never Origin)
  decode.rs                     nowplaying / timers / memories JSON → FRB structs
  memory.rs                     Wyoming memory round trip (anamanti-protocol frames)
test/                           widget + controller tests; fakes in test/support/
integration_test/               on-device boot + a real short mDNS browse
rust_builder/                   cargokit glue that builds rust/ into the app
android/ ios/                   platform runners
```

FRB API (`rust/src/api/companion.rs`): `engine_version`,
`discover_cores(timeout_ms, preferred_key)`, `fetch_overview(core, probe)` (raw
JSON), `fetch_now_playing(core)`, `fetch_timers(core)`, `list_memories(core)`,
`delete_memory(core, id)`, `clear_memories(core)`. Dart keeps the selected
`CoreInfo` (as found by mDNS) and passes it back on every call, so the Rust
side holds no state.

Platform config: Android has `INTERNET`, `ACCESS_WIFI_STATE`,
`CHANGE_WIFI_MULTICAST_STATE`, `usesCleartextTraffic="true"`, and a
`WifiManager.MulticastLock` held by `MainActivity`, the same as the Display. iOS
has `NSLocalNetworkUsageDescription`, `NSBonjourServices` (`_wyoming._tcp`), and
`NSAppTransportSecurity` → `NSAllowsLocalNetworking`. iPhone is portrait only.

## Known gaps

- **iOS multicast entitlement.** `mdns-sd` opens its own multicast UDP sockets.
  On iOS 14+ that likely needs Apple's `com.apple.developer.networking.multicast`
  entitlement, which you have to request from Apple. Without it the browse may
  find nothing on a real iPhone, even with `NSBonjourServices`. The other option
  is to browse from Rust through the system `dns_sd` API (`DNSServiceBrowse`),
  which needs only `NSBonjourServices`. Neither is done yet, and this hasn't been
  tried on a device.
- Not run on a phone yet (no APK or IPA built).

## Rules this app follows (from the repo's `agents.md`)

- **Wire protocol comes from `../anamanti-protocol`** (a path dependency), never copied.
  A wire change is made there once, and Core + Display + Companion are redeployed together.
- **Discovery is mDNS** (`_wyoming._tcp`, TXT `role=core`, optional `instance_id`
  pin). No hardcoded IPs and no IP fallback.
- **Rust owns sockets/discovery; Flutter only consumes FRB streams/calls.** No
  ad-hoc platform channels to get around FRB.
- **FRB bindings are generated.** Change Rust signatures, then regenerate. Never
  hand-edit `lib/src/rust/` or `rust/src/frb_generated.rs`.

## Build

Do not set `CARGO_TARGET_DIR` — build reuse is handled by the shared build cache
(see `agents.md`). Still redirect the APK scratch dir and the Flutter `./build`:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export TMPDIR=/Volumes/External/DeveloperSupport/ambient-build/tmp
cd anamanti-companion
ln -sfn /Volumes/External/DeveloperSupport/ambient-companion-build/build build   # git-ignored

flutter_rust_bridge_codegen generate       # after any rust/src/api change
(cd rust && cargo test && cargo clippy --all-targets)
flutter analyze && flutter test
flutter run                                # attached Android phone or iOS simulator/device
```

## v1 scope (owner decisions, 2026-10-21)

| Feature | Decision | What exists in the Core today |
|---|---|---|
| **Memories** | List them, delete one, and clear all, the same as the Display's Memory screen. | Wyoming frames `anamanti-list-memories` / `-memories` / `-delete-memory` / `-clear-memories` / `-memory-result` (the Display uses these). Web: `/sqlite.json` + `POST /memory/delete`. |
| **Music now playing** | Show it (read-only in v1). | Web: `GET /music/nowplaying.json` (`{ok, linked, radio, now_playing}`). |
| **System status** | Show the admin console's **Overview** page (live pipeline with Local/LAN/Cloud per stage, displays, latency). | Web: `GET /overview.json`. It returns status and tier labels, and only "is a key set" bits for secrets, never the keys. |
| **Pipeline status** | Get it **through the web APIs** (not new Wyoming frames). | As above. But the web server is loopback-only (see the decision below). |
| **Timers** | Show **every timer on every display**. | **Nothing.** Timers belong to each display (`anamanti-timer` is Core → device, fire-and-forget). The Core only sees a display's timers in the per-turn timer context. A new device → Core report is needed. |
| **Network** | **Home network only** (for now). Discovery through mDNS, no remote access. | Matches the locked mDNS / no-IP-fallback decision. |

## Build plan

1. **Core: a LAN read-only "companion API".** *(Done 2026-10-22.)* A **second HTTP
   listener** (`companion_addr` in `anamanti.json`, default `0.0.0.0:8740`, `"off"`
   disables). It serves a **fixed
   allowlist of GET routes** and nothing else: `/overview.json`,
   `/music/nowplaying.json`, and a new `/timers.json`. These reuse the existing
   handler functions. The admin console stays loopback-only, unchanged. Advertise
   the port in the mDNS TXT record (e.g. `companion_port`) so the phone can find it
   without an IP.
2. **Timers across displays (protocol + Display + Core).** *(Done 2026-10-22.)*
   - Protocol: a new device → Core frame, `anamanti-timers`, carrying the full list
     `{timers:[{id, label, duration_secs, remaining_secs, ringing}]}`
     (`ReportedTimer` in `anamanti-protocol`).
   - Display: send it over its existing persistent `notify` channel whenever its
     timer list changes, and again on reconnect.
   - Core: keep the latest list per `device_id` (dropped when the channel closes),
     served at `/timers.json`.
   - This changes the Display, so update `DisplayUI.md`/`architecture.md` and
     redeploy Core and Display together.
3. **Companion Rust engine.** *(Done 2026-10-21.)*
   - mDNS browse (`role=core`, `instance_id` pin, the Display's `choose_address`).
   - A small HTTP client for the three JSON endpoints.
   - The Display's Wyoming memory round trip.
   - Exposed through FRB.
4. **Companion UI.** *(Done 2026-10-21.)* Four tabs: **Status** (Overview
   pipeline), **Now playing**, **Timers** (grouped by display, counting down
   locally from `remaining_secs` on a monotonic clock), **Memories** (list, swipe
   or tap to delete with a confirmation, clear all with a confirmation).
5. **Platform config.** *(Done 2026-10-21.)* See "How it connects" above. On
   Android, mDNS also needs `CHANGE_WIFI_MULTICAST_STATE` and a MulticastLock,
   not just `INTERNET`. iOS may also need the multicast entitlement (see "Known
   gaps").

## Authentication (decided 2026-10-22)

**No auth, read-only** (owner decision):
- The companion listener can't change anything (GET only, fixed route list, any
  request with an `Origin` header refused).
- Memory deletes go over the existing Wyoming frames, which already let any device
  on the LAN delete memories without a password.

If the companion later gets write actions over HTTP (music control, settings), add
pairing first, for example a code shown on the admin console and entered on the phone.
