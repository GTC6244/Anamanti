# Home Assistant Plan: voice control + an on-screen dashboard

**Targets:** M4 Mac Mini (Anamanti Core) + Echo Show 8 (the dashboard view)
**Feature:** the household's Home Assistant instance (`http://homeassistant.local:8123`)
becomes part of Anamanti, in two parts:

- **Part A — voice control.** Ask Anamanti to *"turn on the kitchen lights"*, *"how many
  lights are on?"*, *"run the movie scene"*, or *"pause the living-room TV"*. The Core
  drives HA and speaks the result back.
- **Part B — the dashboard widget.** If the display's **Home Assistant dashboard** option
  is on, the device can show HA's own dashboard (HA's web UI) full screen, and you can use
  it by touch. It opens by voice (*"show the home dashboard"*) or from a home icon on the
  idle screen, and closes by voice, its ✕, or an idle timeout.

> The `HomeDashboardView` full-screen mode, its idle-screen icon and its settings toggle
> must be catalogued in [`DisplayUI.md`](./DisplayUI.md) when Part B lands (§8, §9,
> and the idle screen §1).

> **Status:** **Scope confirmed, design checked against the live instance (2026-10-15).
> Part A is being built (P1).** Part A was redesigned around a Core-side fast path after
> a latency review (§0); its API path, auth and reachability are confirmed against the
> real HA. Part B has one **hardware unknown**: this WebView setup
> hasn't been run on the Echo Show. Part B therefore starts with a spike on the device
> (§B5, W0) before any production code. Part B adds new Wyoming frames, so **Core and
> Display must be redeployed together** once it lands. Part A alone is Core-only.
>
> **Live validation (2026-10-15), against `http://homeassistant.local:8123`
> (mDNS → 192.168.1.51):** `GET /api/` → `200 {"message":"API running."}`. HA is
> **2026.4.2**, with 196 components; `conversation`, `frontend` and `lovelace` are all
> present. There are **286 entities**: 10 `light`, 30 `switch`, 5 `scene`, 13
> `media_player`, 3 `remote`, 1 `script`, 1 `automation`, plus sensors and buttons. There
> is no `climate`, lock, alarm or garage domain. `POST /api/conversation/process
> {"text":"how many lights are on"}` → `query_answer` → `"5"`. `GET /` and
> `/lovelace/0` both return 200. **No kiosk-mode/HACS add-on is installed**, so HA's
> sidebar and header will show unless we hide them (§B3). The `HOME_ASSISTANT_TOKEN`
> long-lived token is already in `~/.zshenv` on the Core host and authenticates. (The
> `homeassisntant.local` spelling does not resolve; `homeassistant.local` does.)
>
> Read with [`MusicPlan.md`](./MusicPlan.md) and `anamanti-core/src/appsaid/`, the
> closest analogues for Part A. For Part B read [`PlacesPlan.md`](./PlacesPlan.md) and
> [`WeatherPlan.md`](./WeatherPlan.md), which use the same show/dismiss full-screen
> template. The Drive token pull (`anamanti-get-drive-token`) is the model for handing a
> Core-held secret to the device. Also see
> [`system1-fast-decisions.md`](./system1-fast-decisions.md) for the route-only and
> dismiss intents.

There is **no Home Assistant code in the repo today**, so both parts are new code.

---

## 0. Confirmed decisions (2026-10-15, revised for speed)

> **Revision (2026-10-15, after a critical review):** the first draft sent every
> request through two LLM rounds plus HA's own Assist language engine. Measured on the
> live instance, Assist takes **0.52–0.67 s per new phrase** on HA's modest host, and it
> also answered *"what is the temperature"* with **"5:42 PM"**. A direct REST command
> costs **~25 ms**, and the whole controllable house (≈35 working devices with their
> rooms) comes back in **one 46 ms call**. So Part A is now a **Core-side fast path**,
> with the LLM only as the fallback, and Assist is dropped.

| Question | Decision |
| --- | --- |
| **Control strategy (A)** | **Fast path first, in the Core.** A deterministic matcher runs **before** System-1, memory recall and the LLM. It recognizes imperative home commands, resolves the target against a cached copy of HA's devices and rooms, sends **one direct REST service call** and speaks a fixed confirmation. Anything it can't match with certainty falls through untouched to System-1 and the LLM. |
| **LLM fallback (A)** | A `home_control` rig tool for what the fast path can't handle. It takes a structured `action` and a free-text `target`, resolves the target with the **same resolver**, and makes the same direct REST calls. **HA's Assist / `conversation/process` is not used.** |
| **Device list (A)** | The Core keeps a **live cached catalog** (entity id, domain, name, room, state), fetched in one `POST /api/template` call. It is pre-warmed at boot, served stale-while-refresh after 60 s, and re-fetched once on a miss. Nothing is hardcoded, and HA stays the source of truth. The user is relabelling devices and rooms in HA so names resolve cleanly. |
| **Rooms (A)** | Each display can have a **room**. A bare "turn off the lights" means that room's lights. The device sends `room` on each `audio-start`, through a new device setting shipped with the Part B device work. Until then, the Core-config `home_assistant.default_room` applies. With no room known, a bare "lights" falls through to the LLM. |
| **Safety (A)** | **Fireplace devices respond only to their exact name.** Entities whose name or id matches `home_assistant.exact_only` (default `["fireplace"]`) are never included in room-wide or "all" commands. Unavailable devices are skipped in groups, and a named one gets a spoken "isn't responding". Otherwise there are no domain exclusions. |
| **System-1 (A)** | The route-only `home_control` intent is **deferred**: the fast path runs before System-1 and catches these commands at no model cost, so the intent would add little. The `home_dismiss` intent for Part B stays. |
| **Dashboard placement (B)** | **Full screen, on demand.** It is one more full-screen layer in `AmbientScreen`'s Stack, alongside Recipe, Weather and Place. It is **not** an idle-screen background, so the photo slideshow stays the idle screen. The embedded browser exists **only while the view is open** (§B4). |
| **Interaction (B)** | **Interactive.** Touches go to HA's UI, so lights, scenes and media can be used from the screen. A Flutter ✕ always sits above the web view. |
| **On/off option (B)** | A new **device-local** setting, `AppSettings.homeDashboardEnabled`, **default off**, under Settings → **Display**. When it is off there is no idle-screen icon, voice requests to open it are refused with a spoken reason, and no browser is ever created. |
| **Dashboard content (B)** | **HA's own dashboard (Lovelace) in a native Android System WebView**, through `webview_flutter`. There is no iframe and no re-implementation of the UI. Which dashboard opens is **Core config** (`home_assistant.dashboard_path`, default `lovelace/0`). We recommend a dedicated HA dashboard laid out for the Echo Show's 960×600 dp screen. |
| **Auth on the device (B)** | **The APK stays credential-free.** On each open, the device pulls `{url, token}` from the Core over Wyoming (`anamanti-get-home-dashboard` → `anamanti-home-dashboard`), the same way Drive does. The token is held **in memory only**, is never persisted on the device, and is handed to the HA frontend through its supported **`external_auth`** bridge (§B2). |

If a task seems to require changing one of these, stop and confirm first.

---

# Part A — voice control (Core only)

## A1. Latency budget

| Path | What runs after the transcript | Expected |
| --- | --- | --- |
| **Fast path** (most commands) | string match (µs) → cached catalog (0 ms when warm) → one REST call (~25 ms) → Piper on a short fixed phrase | **~25–50 ms + first-sentence TTS** |
| Status question ("is the porch light on?") | match → fresh catalog fetch (~46 ms, falls back to the cache) → spoken answer | ~50 ms + TTS |
| LLM fallback | memory recall + LLM tool round(s) + REST (~25 ms) + reply | seconds (unchanged System-2 cost), but only for the long tail |

The fast path never delays other turns. It first checks the **sentence shape** with
pure string code and only touches the catalog when the shape looks like a home command.

## A2. The fast path matcher

**Normalisation:** lowercase, strip punctuation, drop leading politeness/fillers (*please,
can you, could you, would you, hey, go ahead and*) and trailing ones (*please, thanks,
now, for me*).

**Sentence shapes** (the whole utterance must match, with no leftover words):

| Shape | Example | Action |
| --- | --- | --- |
| `turn/switch/shut on/off [the] T` | "turn off the office lights" | on / off |
| `turn/switch/shut [the] T on/off` | "turn the deck lights on" | on / off |
| `T on/off` | "porch lights off" | on / off |
| `activate/run/start/trigger [the] [scene/script] T` | "run media room tv on" | scene / script only |
| `set/dim/brighten [the] T to N [%/percent]` | "dim the office lights to 30 percent" | brightness, lights only |
| `is/are [the] T on/off` | "are the deck lights on" | status, answered from HA |

**Target resolution (T)**, in order:

1. **Room group:** `<room> light(s)` or `light(s) in [the] <room>` → every light-like
   entity in that room. Light-like means the `light` domain, plus `switch` entities whose
   name contains *light* or *lamp*.
2. **All:** `all [the] lights` / `every light` → every light-like entity in the house.
3. **Bare:** `[the] light(s)` → the display's room group (see Rooms). With no room known,
   it falls through.
4. **Name:** an exact (normalised, singular/plural-tolerant) device name, or
   `<room> <name>`. Several devices with the same name are narrowed to the display's
   room. If that still leaves more than one, it falls through rather than guessing.
5. Scenes and scripts match by name for the activate verbs. `turn on <scene name>` also
   works.

Groups always **exclude** `exact_only` (fireplace) devices and unavailable ones. A
brightness command only applies when every target is a `light`.

**Confirmations** (short, fixed): "Office lights off." · "Deck lights on." · "All lights
off." · "Running Media Room TV On." · "Office lights at 30 percent." · "The porch light
is off." / "2 of 3 office lights are on." · "The bathroom light isn't responding right
now." On an HA error or timeout (2 s): "Sorry, I couldn't reach Home Assistant." The
reply leaves the normal follow-up window open, so "and the deck lights" can follow.

## A3. Home Assistant API usage

The base URL is `http://homeassistant.local:8123`: plain HTTP on the LAN, so there is no
TLS. Every call sends `Authorization: Bearer <long-lived token>`, and all of them go
through `crate::http` with a mandatory timeout and secret redaction.

1. **Catalog:** `POST /api/template` with a Jinja template that renders the controllable
   entities (`light`, `switch`, `scene`, `script`, `media_player`, `remote`) as JSON:
   `entity_id`, `name`, `area_name(entity_id)`, `state`.
2. **On/off:** `POST /api/services/homeassistant/turn_on|turn_off` with
   `{"entity_id": [...]}`. One call covers a mixed-domain group.
3. **Scenes/scripts:** `POST /api/services/scene/turn_on` / `script/turn_on`.
4. **Brightness:** `POST /api/services/light/turn_on` with `brightness_pct`.
5. **Handshake:** `GET /api/` (config-page "Test connection").

## A4. Components and ownership

| Piece | Where | Form |
| --- | --- | --- |
| Hub | Core | New `anamanti-core/src/home_assistant/`: `HomeAssistant` (connection, cached `Catalog`, `call`), `HaApi` trait (mockable REST), the pure `matcher` (sentence shapes) and `resolve` (targets), with unit tests against a fixture of the real catalog. One `Arc` is built in `Config::llm_factory()` and shared by the pipeline and the tool. |
| Fast path | Core | `Pipeline::run_home_fast_path` in a new `orchestrator/home.rs`, called right after `parse_command`. On a match: `ctx.timing.path = "home"`, then `speak_fast_reply`. |
| Room | Core | `DeviceContext.room` parsed from `audio-start` `data.room`, with `home_assistant.default_room` as the fallback. |
| Tool | Core | `home_control` in `llm/rig/tools/home.rs` (`action`: turn_on/turn_off/activate/set_brightness/status/list; `target`; `brightness_pct`), registered in `Tools::build` when the hub is configured. Regenerate `advertised_tools.json`. |
| Config | Core | `home_assistant{ base_url, default_room, exact_only, dashboard_path }` in `anamanti.json` / `anamanti.example.json`. The `HOME_ASSISTANT_TOKEN` secret is read from the environment only. |
| Runtime settings + web page | Core | **Done (P2):** `HomeAssistantSettings` (URL, token, default room) seeded from `anamanti.json` + `HOME_ASSISTANT_TOKEN`, editable on Integrations → Home Assistant (masked token, read-only "Test connection"), persisted to the 0600 settings file; a save reconfigures the shared hub in place (no restart). Also listed on Keys & Accounts. |

## A5. Phasing

1. **P1:** the hub (catalog, REST, matcher, resolver), the fast path, the room, the
   `home_control` tool, and config from the file and environment. Then test against the
   live HA.
2. **P2:** runtime settings and the Integrations config page (masked token, test
   button), plus a Keys-tab entry.
3. **P3 (optional):** room aliases, if HA labels alone aren't enough, and a websocket
   state subscription if status answers ever need to be instant.

---

# Part B — the Home Assistant dashboard widget

## B1. How it works end to end

```
open by voice                          open by touch
"show the home dashboard"              home icon on AmbientScreen (only if enabled)
  │ Core `home_dashboard` tool               │
  │ (action=show) → DeviceAction::           │
  │ HomeDashboard(Show) → `anamanti-home`     │
  │ {action:"show"} frame                     │
  ▼                                           ▼
device: setting off? ──yes──► ignore + (voice path) Core already told the user "it's off"
  │ no
  ▼
FRB getHomeDashboard() ── Wyoming control socket ──► Core
  ◄── `anamanti-home-dashboard` {url: base+dashboard_path, token}  (token in memory only)
  ▼
AssistantState.homeDashboard = HomeDashboardSession{url, token}
  ▼
HomeDashboardView: create WebViewController → install the external_auth shim →
load "<url>?external_auth=1" → HA asks the shim for a token → shim answers with it
  ▼
touch-interactive HA UI, with a Flutter ✕ on top; set_display_context({kind:"home"})
  ▼
close: ✕ │ "close the dashboard" (tool action=close / home_dismiss / stop ladder)
       │ idle timeout (5 min with no touch) │ any other full-screen view replacing it
  → dispose the WebViewController and drop the token
```

## B2. Authenticating the HA frontend in the web view

HA's web UI normally logs in through its OAuth login page. We don't want the user typing
a password on the Echo Show, and we don't want the APK to carry credentials. HA's
frontend has a supported hook for apps that embed it, which is the hook the official
Companion app uses. Loading the page with **`?external_auth=1`** makes the frontend ask
the host for its token instead of showing a login page. On Android it calls
`window.externalApp.getExternalAuth({callback})`, and the host replies by calling
`window[callback](true, {access_token, expires_in})`.

`webview_flutter`'s `JavaScriptChannel` only exposes `postMessage`, so we inject a small
JavaScript shim before HA's scripts run. The shim defines `window.externalApp` with these
methods:

- `getExternalAuth`: forwards to the Dart channel. Dart replies with
  `runJavaScript("window[cb](true,{access_token:…,expires_in:…})")`, using the in-memory
  token.
- `revokeExternalAuth`: replies with success and does nothing else, so a "log out" tap in
  HA can't strand the view.
- `externalBus`: answers HA's `config/get` handshake with a minimal "no extra features"
  config, so the frontend doesn't wait on it. Every other message is ignored.

**Fallback if the bridge misbehaves on this WebView:** before the first navigation,
write the HA frontend's `hassTokens` entry to `localStorage`, filling in the long-lived
token as the access token with a far-off expiry. People commonly use this for
kiosk-style HA displays. It is less clean, because the token then sits in WebView
storage, so we clear storage when the view closes. The W0 spike decides which path to
use.

**Token hygiene.** The token is fetched on each open, kept only in Dart memory, and
dropped on close. Cookies and storage are cleared on dispose. Any log line that would
contain it is redacted. **Recommendation, not a blocker:** give the display a separate
**non-admin HA user** with its own long-lived token, set on the Core as an optional
`HOME_ASSISTANT_DISPLAY_TOKEN` that falls back to `HOME_ASSISTANT_TOKEN`. That way the
device never holds an admin credential. It is cheap to add in W1.

## B3. Making it look like a widget, not a browser

- **Sidebar:** before loading, seed HA's own per-browser setting
  `localStorage.dockedSidebar = "always_hidden"`. This is a supported HA preference, not
  a CSS hack.
- **Header and toolbar:** HA core has no kiosk mode, and no add-on is installed. There
  are two options:
  1. Install the HACS **kiosk-mode** add-on in HA and add `?kiosk` to the URL. This is
     robust and survives HA upgrades, but it is HA-side setup you would do.
  2. Inject CSS after the page loads to hide the toolbar. This needs no HA changes, but
     it reaches into HA's shadow DOM, so an HA upgrade may break it.

  **Recommended: option 1 if you're willing to install it, otherwise option 2.** Either
  way the ✕ stays visible, because Flutter draws it above the web view.
- **Dark theme and sizing:** use a dedicated "Echo Show" dashboard with a dark theme,
  built for 960×600 dp, and set it as the Core's `dashboard_path`.
- **Other links:** navigation is limited to the HA origin. Any link that leaves HA is
  blocked, and `window.open` is ignored.

## B4. Memory budget on a ~1 GB device (the main risk)

A Chromium WebView running HA's frontend typically uses about **150–250 MB**, alongside
wake word, audio and Flutter. The rules:

- Create the `WebViewController` **on open** and **dispose it on close**. Never keep it
  around while hidden, and never pre-warm it.
- Close automatically after **5 minutes without a touch**, as Place does. A voice turn
  doesn't reset the timer, but a touch does.
- Opening any other full-screen view (weather, recipe, place, music) **replaces** this one
  and disposes it, following the existing "each replaces any other full-screen mode" rule.
- If Android reports memory pressure (`onTrimMemory` / `didHaveMemoryPressure`) while the
  view is open, close it and show a banner. The wake word must never starve.
- The W0 spike measures RSS with the view open, on a **release** APK on the real device.
  If headroom is unacceptable, the feature stays off by default and we document the
  limit.

## B5. Components and ownership

| Piece | Where | Form |
| --- | --- | --- |
| Config | Core | `home_assistant.dashboard_path` (default `lovelace/0`), plus the optional `HOME_ASSISTANT_DISPLAY_TOKEN` secret. |
| Token frame | both crates | `anamanti-get-home-dashboard` (request) and `anamanti-home-dashboard` (reply, `{url, token}` or `{error}`). Defined once in `anamanti-protocol`, modeled on `GET_DRIVE_TOKEN`/`DRIVE_TOKEN`, with round-trip tests. Core handler in the control-socket dispatch. The URL is built from `base_url` + `dashboard_path`. |
| Show/close frame | both crates | `anamanti-home` with `action` = `show` or `dismiss` (the `anamanti-place` template). Core `DeviceAction::HomeDashboard(HomeNav::{Show, Close})` in `llm/mod.rs`. Device: `TurnUpdate::Home` → `WakeWordEvent::{show,dismiss}_home`. |
| Voice tool | Core | `home_dashboard` (`action`: show/close) in `llm/rig/tools/display.rs`, advertised when HA is configured. If the turn's device context says the option is off, the tool returns "The home dashboard is turned off on this display — you can enable it in Settings → Display." and emits nothing. |
| Device capability | both | The device adds `home_dashboard: <enabled>` to the device context it already stamps on `audio-start`, so the Core knows per turn whether the option is on. Displays that don't send the flag are treated as off. |
| Display context | both | While open: `set_display_context({kind:"home"})` → Core `DisplayContext::Home` → a prompt line ("the Home Assistant dashboard is on screen") so that "close it" resolves correctly. |
| System-1 | Core | A `HomeDismiss` intent (`screen_dismiss_reply` precondition: `widget == home`), plus a `stop_dismiss` ladder rung covering it, alongside the other open-screen dismissals. |
| FRB | device Rust | `get_home_dashboard()` in `api/settings.rs` (mirrors `get_drive_token`) and `wyoming/control.rs`. Regenerate the bindings; never hand-edit them. |
| Setting | Flutter | `AppSettings.homeDashboardEnabled` (default `false`), persisted through `SettingsStore`, with a switch in Settings → **Display** (key `settings-home-dashboard`) and ⓘ help explaining it needs Home Assistant configured on the Core. It must still fit the one-screen layout test. |
| View | Flutter | New `lib/src/ui/home_dashboard_view.dart` with `HomeDashboardView`: the web view plus the external_auth shim, origin lock, the ✕ (key `home-dashboard-close`), a loading/error state ("Couldn't reach Home Assistant"), the idle timer and memory-pressure close. `AssistantState.homeDashboard` and an `AnimatedOpacity` layer in `AmbientScreen`. |
| Idle icon | Flutter | A home icon button on `AmbientScreen` (key `ambient-home-dashboard`), shown only when the option is on and a Core is connected. Tapping it calls `getHomeDashboard()` and opens the view. |
| Dependency | Flutter | `webview_flutter` (the official plugin, which uses the system WebView: no Play Services, so it is fine for the `fdroid` flavor). **Pin a version whose Android implementation builds on the locked AGP 8.7.3 / Gradle 8.14.3 / Flutter 3.44.1.** Check this in W0, because a plugin that needs AGP 9 is a blocker. |

## B6. Phasing

- **W0: hardware spike (do this first, throwaway branch).** In a release APK on the
  **real Echo Show** (not the emulator), check that LineageOS has a working, recent
  System WebView (`adb shell dumpsys webviewupdate`), and that a `webview_flutter`
  version builds on the locked toolchain. Load the HA dashboard with the external_auth
  shim and confirm it authenticates without a login page. If it doesn't, try the
  `hassTokens` fallback. Confirm the sidebar is hidden, then measure RSS and wake-word
  responsiveness with the view open. This step decides go or no-go, and which auth path
  to use.
- **W1: Core.** `dashboard_path` and the optional display token, the `anamanti-home` and
  `anamanti-*-home-dashboard` frames in `anamanti-protocol`, the control-socket handler,
  `DeviceAction::HomeDashboard`, the `home_dashboard` tool with the capability check,
  `DisplayContext::Home`, and System-1 `HomeDismiss` plus its ladder rung.
- **W2: Device.** The FRB `get_home_dashboard`, decoding the `anamanti-home` event, the
  setting and its toggle, `HomeDashboardView`, the idle icon, and the display context.
  Then a release APK, published through the normal OTA path (`agents.md`).
- **W3: Polish.** Kiosk header handling (§B3), the error and offline states, and the
  memory-pressure close.

Core and Display ship **together** for W1 and W2, because both add new frames.

## B7. Risks and what to check

- **WebView on the Echo Show (LineageOS, 32-bit, ~1 GB).** This is unverified, and the
  connected `adb` target during planning was an emulator. W0 is the gate.
- **Toolchain lock.** `webview_flutter_android` must build on AGP 8.7.3 / Gradle 8.x,
  because cargokit can't move to AGP 9. Pin it, and don't bump Gradle to make it fit.
- **HA frontend changes.** The external_auth protocol and the shadow-DOM structure are
  HA internals. External_auth is stable because the Companion apps depend on it, and the
  CSS option is the fragile one. Pin the tested HA version in this doc, and re-test
  after HA upgrades.
- **Token on the device.** Mitigated by memory-only handling, clearing storage, and the
  recommended non-admin display user (§B2).
- **mDNS.** `homeassistant.local` has to resolve on the device as well as on the Mac.
  Android's `.local` resolution is patchy, so the Core could hand the device the
  resolved IP URL (`http://192.168.1.51:8123`) instead of the `.local` name. W0 checks
  this.

## B8. Non-goals (v1)

- HA dashboard as the idle screen or background. The slideshow stays.
- Showing the dashboard on more than one device in sync, or dashboard-only views.
- Building or editing HA dashboards from Anamanti. You design the dashboard in HA.
- HA notifications or camera streams pushed to the display.

---

## 9. What we still need from you

- **Done:** the token, the address, and the scope decisions.
- **HA side, Part A:** relabel devices and rooms in HA so names resolve cleanly (in
  progress, by you). Assist exposure no longer matters, because Part A doesn't use Assist.
- **HA side, Part B (recommended, not blocking):**
  1. Create a dedicated **"Echo Show" dashboard** in HA, dark theme, laid out for 960×600.
  2. Optionally create a **non-admin HA user** for the display and give us its long-lived
     token.
  3. Optionally install HACS **kiosk-mode** to hide HA's header (§B3).
- **Hardware:** the **real Echo Show** on `adb` for the W0 spike. During planning only an
  emulator was attached.

## 10. Docs updated on landing

- `agents.md`: the `HOME_ASSISTANT_TOKEN` and `HOME_ASSISTANT_DISPLAY_TOKEN` secrets, the
  feature-plan list, and the `home_assistant` config block.
- `architecture.md`: §4 (the frame catalog, plus the home device mode) and §8 (tools).
- `Plan.MD`: decision-table rows.
- `DisplayUI.md`: §1 (the idle icon), §8 (`HomeDashboardView`), §9 (the Display toggle).
- `TODO.md`.
