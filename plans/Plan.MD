# Project Blueprint: Anamanti

**Targets:** Echo Show 8 (LineageOS) + M4 Mac Mini

> **Status:** Planning / pre-implementation. Scope confirmed as a **full voice
> assistant** (STT → LLM → TTS) with local wake-word detection on-device.

---

## 0. Confirmed Decisions (2026-09-15)

These answers override any earlier ambiguity in this document:

| Question | Decision |
| --- | --- |
| **Scope** | **Full voice assistant** — wake word → stream audio → STT → LLM → TTS → spoken reply + on-screen text |
| **Service discovery** | **mDNS / Zeroconf** — the Echo Show auto-discovers the Mac Mini's Wyoming service on the LAN |
| **Wake word** | **openWakeWord `.onnx`** model executed on-device via `tract-onnx` (no custom training for v1) |
| **LLM runtime** | **Pluggable** — abstracted behind an interface; local (Ollama / llama.cpp) or cloud (Claude / OpenAI) are swappable |
| **TTS** | **Piper** via the Wyoming TTS protocol (local, Wyoming-native) |
| **Playback** | **Rust (cpal / oboe)** plays the returned TTS audio frames — symmetric with capture |
| **Barge-in** | **Full-duplex** — wake word stays active during playback so the user can interrupt at any time |
| **Echo cancellation** | **Deferred for v1** — ship full-duplex without robust AEC; accept some self-triggering and revisit (see §4) |
| **End-of-speech (VAD)** | **Anamanti Core-side, pluggable VAD engine** — neither STT engine does streaming VAD, so the Mac detects end-of-speech and finalizes the transcriber; the device streams until told to stop (device runs no VAD). **Neural Silero is the committed default (flipped 2026-09-30; `vad-silero` feature on by default, requires the v4 model on disk); energy/RMS is the opt-in fallback** — see the *"End-of-speech VAD — pluggable engine + Silero"* row and [`VadSileroPlan.md`](./VadSileroPlan.md) |
| **Conversation memory** | **Persistent**, stored **on the Mac Mini** in **SQLite** (structured facts + preferences across sessions) |
| **Memory policy** | **Explicit + inferred** — remember on request *and* auto-extract facts/prefs from conversation |
| **Memory UX** | Manage via **settings list (view/delete)** + **voice** ("remember…", "forget that") |
| **Resilience** | **Auto-reconnect** with backoff via mDNS + a subtle **disconnected** status; slideshow keeps running |
| **Idle screen** | **Photo slideshow** — two selectable backends: **Google Photos** (Ambient API; ideal, but **partner-program gated**) and **Google Drive** folder (interim, works now); plus local gradients |
| **Photo auth** | **Ambient API:** on-device device-code + QR (`photosambient.mediaitems`) — blocked by Partner Program (`createDevice` 403); TV client baked into the APK. **Drive:** `drive.readonly` consent runs **in the Anamanti Core** (Desktop client, config-page Photos tab) → device pulls the token over Wyoming (`anamanti-get-drive-token`). APK is credential-free for Drive; Mac holds the Drive client (`ANAMANTI_GOOGLE_DRIVE_CLIENT_ID/SECRET`) |
| **AEC** | **Ship full-duplex**; raise wake-word confidence **threshold during playback** to suppress self-triggers; real AEC only if still bad |
| **Settings screen** | User-configurable: **LLM backend**, **TTS voice**, **wake word**, **photo source** |
| **Camera proximity (2026-09-21)** | Front camera doubles as a **proximity sensor**: a cheap **frame-motion** detector in **Rust** (no ML, no face recognition) brightens the idle screen on approach and dims it after a quiet spell. Sensing lives in Rust (`camera/presence.rs` + Kotlin `CameraBridge` capture shim, twin of `MicBridge`); **brightness actuation stays in Flutter/Android** (`ScreenBrightnessController` → `MainActivity` `MethodChannel`). Degrades silently if the camera is unavailable (permission denied / **privacy shutter** closed / no camera) — the screen just stays bright. `CAMERA` is granted out-of-band (`pm grant`), like `RECORD_AUDIO` |
| **Tool calling / actions** | **rig-core is the default engine** (tool-calling loop); info tools run on the Mac, device actions (timers/alarms) emit `anamanti-*` frames and execute **on-device**. Weather is grounded by a configured **home location** (`ANAMANTI_HOME_LOCATION`). Calendar/reminders designed but deferred. **Spotify** playback is a further tool of this kind — see [`MusicPlan.md`](./MusicPlan.md) (control plane on the Anamanti Core; audio via librespot + Snapcast, not the TTS pipeline). |
| **Canonical household info (2026-09-22)** | Each Anamanti Core holds a persisted **`Household`** record (`settings::Household`) — the home **location + units** and a **roster of people** (name, emails, phones, relationship) — as canonical context for tools + the prompt. Location/units **seed from `ANAMANTI_HOME_LOCATION`/`ANAMANTI_WEATHER_UNITS` at first boot**, then are editable (with the people) from the config page **Household tab** (`/household`, `webconfig.rs`); a persisted value wins over env at the next boot. Stored 0600 (PII) in `anamanti_settings.json` alongside Drive. The prompt reads location + roster from the **per-turn settings snapshot** (`orchestrator::location_line` + `household_line`), so edits apply with no restart. The **directions tool's default origin** reads a shared live handle (`directions::LiveHomeLocation` on `LlmFactory`) that `apply_household` updates, so a location edit re-homes routing with no backend rebuild. An **identified speaker** whose name matches a roster member is reconciled to it (`Household::member_matching`): the identity line then uses the canonical name + relationship. Not yet: the roster isn't surfaced to the device (config-page only). |
| **Shopping list / Cadora integration (2026-09-22)** | The assistant can **add to the household's shared shopping list** via the `shopping_list_add` rig tool. The list lives in **Cadora** — the shared backend behind the family apps (NextHaul et al.), which already ships a headless **voice API** ("one brain, thin front-ends"). The Anamanti Core is a **fourth voice front-end**: it never touches Supabase/RLS directly (Cadora owns member attribution, list creation, dedupe, and the spoken confirmation); it only calls `POST /voice/command` with the canonical `{action:"add_shopping_item",item,quantity}` body over a `vl_…` **voice-link token** (LLM parses intent, so Cadora's NLU is skipped). **Linking**: mint a **6-digit code** in **NextHaul → Settings → Voice & Integrations** (`POST /voice/links/pair`); enter it once on the config page (Household tab), which redeems it (`POST /voice/links/redeem`) for a durable token stored 0600 in `anamanti_settings.json` — the Anamanti Core is the "speaker" that redeems. (A pasted `vl_…` token is also accepted.) Client seam `anamanti-core/src/cadora/` (`GroceryController` trait, mirroring `SpotifyController`); config seed `cadora{base_url,link_token}` (base_url default `https://cadora-server.fly.dev`). Tool advertised only when linked; relinks/withdraws live like `spotify_control`. **v1 exposes add-only** (Cadora's other voice action, `agenda_today`, is deliberately not exposed — it would collide with this assistant's own `calendar_lookup`). |
| **mDNS advertiser → OS mDNSResponder (2026-09-30)** | The Core advertises `_wyoming._tcp` through Apple's **system `mDNSResponder`** via `DNSServiceRegister` (`astro-dnssd`) on macOS, instead of running a second in-process pure-Rust responder (`mdns-sd`). **Why:** the pure-Rust responder produced a recurring, ~daily "mDNS breaks, only a Core restart fixes it" failure — it pinned a single routable IPv4 and had to re-register on `IpAdd`/`IpDel` events within `mdns-sd`'s ~30 s interface poll, and it could not reliably rejoin multicast groups after the Mac slept, so an overnight sleep/wake (the Core runs detached, no `caffeinate`/launchd yet — see §4) left a zombie record answering nothing. `mDNSResponder` owns address refresh, multicast rejoin, sleep/wake recovery, and collision auto-rename itself. **Protocol unchanged** (same `_wyoming._tcp` + TXT `role=core`/`name`/`instance_id`), so the device browser is untouched — this swaps the implementation, not the locked mDNS/Zeroconf decision or the no-hardcoded-IP rule. **Companion fix:** since `mDNSResponder` advertises *all* host addresses (no single-IPv4 pin), the device now selects a reachable address (`choose_address`: skip loopback/unspecified/link-local, prefer IPv4 over global IPv6) rather than an arbitrary set member. Off macOS (dev/CI) the crate keeps the `mdns-sd` pinned-IPv4 advertiser. `anamanti-core/src/discovery.rs` + `anamanti-display/rust/src/wyoming/discovery.rs`. **Residual:** an `mDNSResponder` restart drops the registration until the Core restarts (rare); launchd + no-sleep for the Core is still open (§4). |
| **Proactive notifications (2026-09-23)** | The Anamanti Core can push notifications to the display **unprompted** (no voice turn) via **Approach A: a persistent, device-dialed Wyoming channel** — the device opens a second long-lived socket to the pinned Anamanti Core (`anamanti-hello` `role=notify`), reconnects with backoff, and receives `anamanti-notify` frames the Mac pushes down it. Chosen over a reverse connection / device listener (keeps the device the dialer; reuses the mDNS + `instance_id` pin and the existing accept loop; no new trust direction) and over a doorbell/long-poll (idle-socket savings are free on a mains-powered display). **Shipped visual-only first**: Rust owns the socket + reconnect (`rust/src/wyoming/notify.rs`, `start_notify_channel`), Flutter shows a dismissible banner (`NotificationController` + `NotificationBanner`); no spoken output, no voice-state-machine interaction. Anamanti Core side: `NotificationService` registry + config-page **Notify** tab test button (`POST /notifications/test`). Deferred: spoken notifications + barge-in, acks/store-and-forward/TTL, per-device targeting, quiet hours, paired/TLS channel. **Device identity (2026-09-30):** each display now announces a **stable, globally-unique `device_id` derived from its Wi-Fi MAC** (`anamanti-<12 hex>`, minted once by `api::engine::device_hardware_id`, persisted; random-id fallback) plus a **human-friendly `name`** set on the Settings screen — both carried in the `anamanti-hello` frame (`data.device_id` + `data.name`). The Core keys each connection by id+name and renders a **"Connected devices" list** on the config-page Notify tab (`/notifications/status.json` → `devices[]`). This unblocks future **per-device targeting** (push fan-out is still to all for now). |
| **STT engine — in-process option (2026-09-25)** | STT sits behind a `Transcriber` seam (`anamanti-core/src/stt/`) with **two interchangeable engines**, chosen by the `stt.engine` config key: **`wyoming`** (default) dials the external `wyoming-faster-whisper` Python server (`stt_addr`); **`whisper-rs`** runs **whisper.cpp in-process** (`stt-whisper-local` feature; `stt-whisper-metal`/`-coreml` for accel), loading a ggml `base`/`small` model and decoding in a `spawn_blocking` task. Same Whisper-class transcript; the Core's energy VAD still decides end-of-speech either way. Motivation is **deploy simplification** (drop the Python STT process/port/venv), *not* speed — the model math is C++ in both. Stages 1–4 landed + verified (incl. a live decode + Metal on an M1 Max dev box); **the committed default stays `wyoming`** — flipping it is **gated on M4 Mac Mini validation** (would otherwise break a default build's boot). Full plan + cutover checklist: [`python-to-rust-whisper.md`](./python-to-rust-whisper.md). |
| **Weather tool + display (2026-09-25)** | Asking about the weather brings up a **full-screen forecast** on the device — today's conditions with **big (Material-icon) imagery** + a **7-day row** along the bottom — and the idle/home screen always shows a **small weather icon + current temperature beside the clock**. Three pieces along the locked boundary (recipe mode as the template): (1) a `weather_lookup` / `close_weather` **rig tool** (`llm/rig.rs`) over a `WeatherProvider` trait (`anamanti-core/src/weather/`) with two backends (injected, offline fixture tests): **Visual Crossing** (default, `weather.provider="visualcrossing"`; Timeline API in one keyed request, `VISUALCROSSING_API_KEY` secret, VC-icon→WMO mapping) and the **keyless Open-Meteo** current+7-day forecast + geocoding API (the fallback when the key is absent); units from the household `weather_units`, place defaults to `home_location`. The provider label + Visual Crossing key are **runtime-settable on the config-page Tools tab** (persisted; `apply_weather_tool` rebuilds `weather_lookup` + retargets the ambient push live, mirroring the Mapbox token flow). (2) a new **`anamanti-weather`** frame (byte-identical in both `protocol.rs`; `action` = `show`/`current`/`dismiss`) carrying the structured `WeatherReport`. The voice-triggered **full screen** rides the per-turn socket via `DeviceAction::{ShowWeather,DismissWeather}` → `drain_device_actions` → `TurnUpdate::Weather` → FRB `WakeWordEvent::{show,dismiss}_weather`. The **always-on ambient indicator** is refreshed by a Core **`WeatherService`** that fetches every `weather.refresh_interval_secs` (default 30 min) and broadcasts `current` frames down a **persistent `role=weather` channel** (twin of the notify channel: `rust/src/wyoming/weather.rs` + `start_weather_channel` → `WeatherPush` FRB stream). (3) Flutter: `WeatherView` (today panel + 7-day row) as an `AmbientScreen` layer keyed off `AssistantState.weatherActive` that **auto-dismisses after 60 s** (`AssistantController._weatherAutoClose`; resets on a new forecast, cancelled by an early voice/touch dismiss, leaves the ambient chip up), and the `_AmbientClock` chip (icon + **temperature with units**, e.g. `18°C`) driven by `AssistantState.weatherCurrent`; WMO→icon map in `weather_icons.dart` (bundled Material icons, no raster assets). Config: `weather{enabled,provider,refresh_interval_secs}` + the `VISUALCROSSING_API_KEY` secret. Full design: [`WeatherPlan.md`](./WeatherPlan.md). **Pending on-device QA.** |
| **Places tool + display (2026-09-27)** | Asking about a place — a business, shop, restaurant, cafe, landmark — brings up a **full-screen place card** on the device (name, a hero photo, address, opening hours with an **open-now** chip, rating + review count, phone, website, category, price level). Mirrors the weather triad. (1) a `places_lookup` / `close_places` **rig tool** (`llm/rig.rs`) over a `PlacesProvider` trait (`anamanti-core/src/places/`) backed by the **Google Places API (New)** — a **Text Search** (`places:searchText`) then **Place Details**, biased toward `home_location`; the `GOOGLE_PLACES_API_KEY` secret is sent as the `X-Goog-Api-Key` **header** with a field mask; **no keyless fallback** (mirrors `directions::from_token`: no key ⇒ tool not advertised). The key is **runtime-settable on the config-page Tools tab** (`apply_places_tool` rebuilds the tool live). **Disambiguation**: a single match shows the card; multiple comparable candidates return a spoken candidate list (no card) so the model asks which one and re-calls with the chosen `place_id`. The hero photo is resolved to a **keyless `photoUri`** (`skipHttpRedirect=true`), so the device fetches it directly (like the recipe image) and the APK stays credential-free. (2) a new **`anamanti-place`** frame (byte-identical in both `protocol.rs`; `action` = `show`/`dismiss`) carrying the structured `PlaceReport`, over the per-turn socket via `DeviceAction::{ShowPlace,DismissPlace}` → `drain_device_actions` → `TurnUpdate::Place` → FRB `WakeWordEvent::{show,dismiss}_place`. No ambient push. (3) Flutter: `PlaceView` (photo + details) as an `AmbientScreen` layer keyed off `AssistantState.placeActive`; `place_data.dart` parses the report; `set_place_context` stamps `{kind:"place"}` on `audio-start` so a follow-up ("is it open Sunday?" / "close it") routes in context. Plus a **route-only System-1 `place` intent** (classified, then deferred to System-2 which owns the free-form slot; `system1-fast-decisions.md` §15.2). Config: `places{enabled,provider}` + the `GOOGLE_PLACES_API_KEY` secret. Full design: [`PlacesPlan.md`](./PlacesPlan.md). **Pending on-device QA.** |
| **Doc intent** | Onboarding + AI codegen — docs guide AI agents and humans to build from scratch |
| **Music routing / Snapcast (2026-09-22)** | House-wide music originates on the **Mac Anamanti Core host**, not the Echo Show (offloads the ~1 GB device; **no on-device system-audio capture, no second device audio path**), and reaches synchronized speakers via **Snapcast** (~1 s buffered latency accepted). Two Mac-hosted **external** sources — **librespot** (Spotify Connect, the shared "Ambient" device) and a **web-URL/stream player** (mpv/ffmpeg) — each write raw PCM to a **snapfifo**; **snapserver runs on the Mac** and reads the fifos. **PCM never enters `anamanti_core`** — the Anamanti Core only issues control (Web API, mpv IPC, and snapserver **JSON-RPC** for ducking on THINKING/SPEAKING + stream-select). Snapclients: the **Linux main speaker**, other rooms, **and the Echo Show(s)** — the device client is the **prebuilt `snapclient` binary bundled in the existing app but run as a *separate OS process*** (Kotlin foreground service; **not** in the Rust engine, **not** a separate APK), so the app's single locked cpal path (capture + TTS) is untouched. Per-device toggle `ANAMANTI_MUSIC_INCLUDE_DEVICE` (default **off**): the device playing continuous music with **no AEC** worsens wake-word self-triggering, so enable only where needed and hold the raised wake-word threshold while it's in the active group. Full design: [`snapcast_routing_plan.md`](./snapcast_routing_plan.md) (the "Snapcast routing" transport layer) + [`MusicPlan.md`](./MusicPlan.md) (Spotify source + `spotify_control` control plane). |
| **Multi-device / multi-Anamanti Core (2026-09-22)** | (1) **Many displays → one Anamanti Core**: already correct — the accept loop handles each connection independently and replies on that socket; memory + settings are one shared **household** pool (speaker ID scopes per person, not per device). (2) **Many Anamanti Cores → device picks one**: Anamanti Cores advertise TXT `role`/`name`/`instance_id`; the device filters by `role=core` (excludes raw STT/TTS), enumerates all, and pins a chosen `instance_id` from a settings dropdown. `instance_id` resolves env → git **branch code** → sanitized service name, so a copy run from a test worktree self-names by its branch. "Local production" is a copied release binary at `/Volumes/External/DeveloperSupport/Anamanti Core/` that sets `ANAMANTI_INSTANCE_ID` in `~/.zshenv`; test copies run from their worktree on distinct ports. **Strict**: a pinned display stays offline if its Anamanti Core is unreachable (never switches Macs); `"Auto"` = first responder. (3) **Shared backend data**: SQLite in **WAL** mode for cross-process sharing; embedded HelixDB is single-process, so a test instance sharing prod data runs `ANAMANTI_MEMORY_BACKEND=sqlite`. |
| **Follow-up listening — reopen the mic after every reply (2026-09-23; revised same day)** | After **every** spoken reply the device **auto-reopens the mic and listens for more input with no wake word**, then **sleeps if nothing is said**; that follow-up turn's LLM prompt is fed the **recent conversation history** for context. The listen window differs by reply: a **question** (`?`) waits **10 s** (`follow_up.question_wait_secs`), any other reply waits **5 s** (`follow_up.reply_wait_secs`). *(Superseded the initial "questions only" design — every turn now behaves the same, just with a different wait.)* Confirmed design choices: **(a) The Anamanti Core decides + times it** — after the full reply is assembled it always sends the follow-up signal (gated only by `follow_up.enabled`), choosing the wait by whether the reply's last non-whitespace char is `?`. **(b) Signaling reuses the `anamanti-*` device-action pattern**: a project-local **`anamanti-listen`** frame (Anamanti Core → device, `data.depth` + **`data.wait_secs`**) sent **before** the final `audio-stop` (the device ends its turn on the first `audio-stop`). **The Anamanti Core passes the wait time back in this frame.** **(c) The wait is enforced by the Anamanti Core's no-speech VAD** on the follow-up turn (the device has no VAD, so only the Mac can tell silence from a user mid-sentence): the device echoes `wait_secs` on the follow-up `audio-start` (`followup`/`followup_depth`/`wait_secs`), and the Anamanti Core sizes that turn's no-speech-finalize window to it. On silence it relays an empty transcript **plus an `audio-stop`** so the device leaves SPEAKING and sleeps promptly (no 15 s hang), and sends no new `anamanti-listen` — **silence is the terminator.** **(d) The device gates the mic reopen on its own TTS draining** (the `pending()==0` signal, same as `SpeakingDone`) before spawning the follow-up turn (a fresh Wyoming connection via the barge-in-restart path), so the mic never captures the reply's tail; it inherits the raised in-turn wake-word threshold. **(d.1) Speech-onset debounce (2026-09-24 fix):** the `pending()==0` drain gate frees when the *software* playback ring empties, but the OS/hardware output buffer can still play out a residual TTS tail, so the reopened follow-up mic sometimes caught that tail (or room echo). A single such chunk above `voice_rms_threshold` used to latch `speech_started` immediately, collapsing the full `wait_secs` no-speech window into the ~700 ms `end_silence` finalize and cutting the user off (seen in production: a 10 s follow-up that closed after ~1 s with an empty transcript). Fixed by debouncing the onset — `speech_started` now latches only after **`MIN_SPEECH_ONSET` (250 ms) of *consecutive* voiced audio** (`voiced_onset_step` in `orchestrator.rs`); any non-voiced chunk resets the run, so a lone transient can no longer shorten the wait. Real speech clears 250 ms in a single word. **(e) History = the last 5 turns within the last 10 minutes** (`follow_up.history_turns=5`, `follow_up.history_window_secs=600`), from `anamanti_chatlog.jsonl`, injected into follow-up turns **only** — ordinary wake-word turns stay single-turn (locked "single-turn v1" decision unchanged). **(f) `follow_up.max_chain` is now an optional safety ceiling, default `0 = unlimited`** — since silence terminates cleanly, genuine back-and-forth is not artificially capped; a non-zero value re-imposes a hard cap. **Config:** a `follow_up` block in `anamanti.json` (`enabled` on, `max_chain` 0, `question_wait_secs` 10, `reply_wait_secs` 5, `history_turns` 5, `history_window_secs` 600). **Known v1 limitations:** the `?` heuristic can misfire on rhetorical questions; two displays mid-conversation can interleave the recency-only history (a conversation token would fix it — deferred). |
| **System-1 fast-decision stage (2026-09-25)** | A pluggable **System-1 decision engine** runs on the Anamanti Core **before** memory recall and the LLM, resolving common intents ("show me the weather", timers, recipe nav, music transport, shopping-list add, simple smalltalk) in a **single non-autoregressive forward pass** and only **deferring** the hard turns to today's System-2 path. Targets two measured latency bottlenecks: the default **rig+tools engine defeats token streaming** (`llm/rig.rs:1840` — any tool present ⇒ full blocking completion, up to `MAX_TOOL_ROUNDS=4`), and **memory recall blocks before the LLM** with a network **OpenAI embedding round-trip** (`memory/backend.rs:97`). Both are skipped on a resolved turn. **Pluggable behind a trait like the LLM** (`DecisionEngine`, new `anamanti-core/src/system1/`, held on `RuntimeSettings` beside `llm`, `build_system1()` mirroring `build_llm()`, config-page-selectable + persisted). Uses the shared **`/v1/systemone` wire contract** (typed `choice`/`noul`/`score` answers, calibrated `answer_confidence`), so one HTTP client serves both a local **`laya-serve`** sidecar (the user's [Laya-Decision](https://github.com/GTC6244/Laya-Decision) Rust port) and **Jev on OpenRouter** (`typesafe/jev-1.13`); in-process `laya-decision` (candle/Metal) is an optional escape hatch. **Decided:** shipped default backend = **`laya-serve`** (local, keeps candle out of the Core binary; Jev = cloud fallback); safe merge default = **`none`** (no behavior change until enabled). It is a **router, not a slot extractor** — it answers a fixed routing question set (intent `choice` + `needs_full_understanding` `noul`) gated on a strict `min_confidence`; open-ended args (a non-home city, freeform requests) → Defer. Resolved intents reuse the **existing tools + `DeviceAction`s** (weather → `weather_lookup` → `DeviceAction::ShowWeather`, widget emitted **before** speech) and the existing action-relay loop — no new device protocol. Resolved turns still write the chat log, run inferred-memory capture, and emit the follow-up `listen` window. **Resolved replies are NOT wake-word-interruptible in v1** (the templated line is sub-second; matches the canned remember/forget reply). Seam: `respond_and_speak` after the `parse_command` short-circuit (`orchestrator.rs:605`) and before `build_context` (`:642`). New secret `OPENROUTER_API_KEY` (Jev only). Full design + phasing: [`system1-fast-decisions.md`](./system1-fast-decisions.md). **Status: implemented — M0 scaffold + M1 (`laya-serve`/`jev` HTTP client, `/v1/systemone`, weather intent) + M2 (runtime-swappable + persisted selection via `SharedSettings::apply_system1`; `/system1` config page) + M3 timer intent; default `none` (opt-in via `anamanti.json` or the config page). Deferred: the arg-heavy intents (shopping/recipe/music/smalltalk) — the typed classifier returns an intent, not free-form slots, so those need extra typed sub-questions + on-device QA — and `laya-embedded` (in-process candle). New secret `OPENROUTER_API_KEY` (jev). Toolchain pinned to rustc 1.92 via `rust-toolchain.toml` (deps require ≥1.91).** |
| **End-of-speech VAD — pluggable engine + Silero (2026-09-28)** | The Core's end-of-speech detector becomes **pluggable behind a `SpeechGate` seam** (`anamanti-core/src/vad/`), with the existing hand-rolled **energy/RMS gate as the committed default** and an opt-in **neural Silero VAD** engine (Silero **v4** `silero_vad.onnx` on **onnxruntime via the `ort` crate**, behind the `vad-silero` build feature — the M1 spike proved pure-Rust `tract` **cannot** load stock Silero v4/v5: both embed an ONNX `If` op tract's typed translation rejects, so the neural gate runs on onnxruntime, Core/Mac-side only. On-device testing then found the **v5 export scores ~0 under `ort`** and never fires, so the gate ships the **v4** model). **Why:** an amplitude threshold can't separate far-field speech from a room noise floor (`settings.rs:70-79`), forcing per-device `voice_rms_threshold` babysitting (stalls when too low, dropped quiet speech when too high — see the `TODO.md` AEC field war-story). Silero returns a **calibrated speech probability** that is robust to TV/music/background energy, cutting both failure modes and letting us **safely shorten the dominant latency term, the `end_silence` hangover** (700 ms → target 400–500 ms). **Honest scoping:** Silero is *latency-neutral by itself* (~sub-ms–1 ms/frame on M4, dwarfed by the post-`audio-stop` Whisper decode; one-time boot load) — accuracy is the primary payoff, the latency win comes from **retuning the hangover** on the better confidence, not the detector swap. **Only the per-frame voiced decision changes** — the onset debounce (`MIN_SPEECH_ONSET`), `speech_started` latch, hangover, `no_speech_finalize`, the silence-hallucination guard, and voiced-PCM-for-speaker-ID all stay. **Locked-decision impact:** generalizes *"energy VAD"* to *"pluggable, energy default"*; VAD stays **off-device and on the Core** (unchanged), no new device protocol. **Config:** a `vad` block (`engine: energy|silero`, `silero.{model_path,threshold,min_speech_ms,min_silence_ms}`), `deny_unknown_fields`, defaults preserving today's behavior; `engine=silero` with a missing model is a **hard boot error** (no silent fallback); model fetched by `scripts/fetch-vad-model.sh` (uncommitted, like the ggml models). **Merge-safe like `stt.engine`/`system1.backend`:** committed default stays **energy**; flipping to Silero is **gated on M4 + Echo Show far-field validation** (M3). **Phasing:** M0 seam + `EnergyGate` (pure refactor, no behavior change) → M1 `SileroGate` (onnxruntime/`ort` load of the v4 model, 1536-sample framing, `h`/`c` state threading, config block, boot check, host fixtures) → M2 runtime-settable threshold (mirrors `voice_rms_threshold` relay) + retuned/adaptive hangover → M3 on-device QA + default-flip decision. **Resolved risks (M1):** tract can't load Silero (→ ort) and the v5 export scores ~0 under ort (→ ship v4). Full design + phasing: [`VadSileroPlan.md`](./VadSileroPlan.md). **Status: M0 (seam + `EnergyGate`, behavior-preserving) + M1 (`SileroGate` on `ort`/onnxruntime, `vad` config block, `build_speech_gate` switch, boot-time model check + feature-off/missing-model hard errors, `scripts/fetch-vad-model.sh`) implemented + verified (clippy clean; 335 lib tests; loads model + boots `vad.engine=silero`) and **validated live on the Echo Show 2026-09-28** — "Hey Jarvis / what is the weather" detected (`speech_started=true`), transcript kept, weather spoken + shown; the **v5→v4 model switch** was the key on-device fix. Committed default stays energy; `vad-silero` is off by default. **M2 partial:** the `silero_threshold` **and** `vad_engine` are now live runtime settings (relayed on the device Wyoming path + config page, with a VAD section on the config page; energy⇄silero swap needs no restart, threshold clamped [0,1]) — verified via the config API + unit tests. Deferred: the M2 latency win (Silero-tuned shorter/adaptive `end_silence` hangover) and M3 (M4 + Echo Show far-field accuracy/latency validation, then the default-flip decision) — both need real speech + hardware.** **UPDATE 2026-09-30 — committed default flipped to Silero (ahead of the M3 gate).** Owner-directed after a live background-noise failure of the energy gate (room noise above `voice_rms_threshold` keeps the gate "voiced", so end-of-speech never fires and the turn stalls). Changes: `vad-silero` is now a **default Cargo feature** and `VadConfig::default().engine == Silero`, so a plain `cargo build` + boot runs Silero and **requires the v4 model on disk** (`scripts/fetch-vad-model.sh`; a `silero` boot with no model is a hard error). Energy/RMS is now the opt-in fallback (`vad.engine="energy"` or `--no-default-features`) and the auto-fallback when no model is loaded. The M3 far-field measurements are **still owed** to confirm the flip; revert path = drop `default = ["vad-silero"]` + restore `engine: Energy`. Deployed to local production (rebuilt with the feature + model fetched). |
| **Output personalities (2026-10-01)** | The assistant can speak in a selectable **personality** (overly enthusiastic, heavy sarcasm, depressed, 1940s gangster, Donald Trump — plus the implicit **normal = off**). **Pure output tuning:** a personality only changes how a reply *sounds* (tone, word choice, delivery); it never touches memory, recall, facts, or which tools are called. Architecture = an **on/off switch + a selected type**, both drivable **by voice** via a new always-registered **`set_personality`** rig tool ("talk like a 1940s gangster" / "go back to normal"), which emits a **Core-side** `DeviceAction::SetPersonality { name }` the orchestrator applies to `SharedSettings` (no device frame; resolves the name against the live catalog). Per turn the orchestrator appends a **trailing, fenced style block** to the system prompt (from the snapshot) plus a one-line list of available names + the active one so the model can switch. State + the **editable catalog** (`{key,label,description}`, seeded with the five built-ins) live in the settings overlay (`Personality` in `settings.rs`; persisted to `anamanti_settings.json`, **no `anamanti.json` seed**), changeable from the Core GUI **Personality tab** (`/personality`, full-record save via `apply_personality`, mirroring the Household tab). Changing it **never rebuilds the LLM**. Core-only — nothing crosses Wyoming, no Display UI. Not applied on System-1-resolved turns (those bypass the LLM). **Status: implemented** (settings model + prompt injection + `set_personality` tool + config tab). |
| **Recipe voice navigation + screen context (2026-09-25)** | The guided-recipe screen ([`RecipePlan.md`](./RecipePlan.md)) is now **drivable by voice**: switch tabs (*"show the ingredients"*), scroll the current pane (*"scroll down" / "back to the top"*), and close it — via a new **`recipe_control`** rig tool that emits **`navigate`/`scroll` sub-actions on the `anamanti-recipe` frame** (alongside the existing `show`/`dismiss`). To make this reliable, the device now sends **display context to the Core** — a **general, extensible** mechanism, not recipe-specific: each turn's **`audio-start` carries a `screen` block** discriminated by a `kind` string (today `kind:"recipe"` with title, active tab, scroll at-top/at-bottom, ingredient/step counts), which the orchestrator parses (`protocol::display_context` → a `DisplayContext` enum) and injects as a one-line note into that turn's system prompt (`display_context_line`, one arm per screen) so the model knows what's on screen and which tool to call. This is the **first device→Core context channel**; it piggybacks on `audio-start` (like the follow-up `depth`/`wait_secs` markers), not a new persistent uplink. **Adding another voice-controllable screen** (music, weather, photos) is a `DisplayContext` variant + a prompt-line arm + a device-side `set_<screen>_context` setter — the wire transport is unchanged. Touch (tab taps, drag-scroll) still works and stays in sync via the `RecipeController`/`AssistantState` (`recipeTab` + a scroll-command counter). Still visual-only otherwise — no spoken step read-out. |
| **Global voice font scale (2026-10-02)** | *"increase font"* / *"decrease font"* grows/shrinks **all** on-screen text as **one device-wide scale**, persisted across reboots (`AppSettings.fontScale`, clamped `[0.85, 1.6]`, step `0.1`). Flutter's `Text` reads the ambient `MediaQuery.textScaler`, so a **single `TextScaler.linear(fontScale)` override around `AmbientScreen`** scales every hardcoded `fontSize:` at once — no per-widget edits; the pushed `SettingsScreen` route is outside that subtree and stays at the default size. Recognizer = a new **`adjust_font`** rig tool (always advertised; `DeviceAction::AdjustFont` → the new **`anamanti-font`** frame, byte-identical in both `protocol.rs`, → `TurnUpdate::Font` → FRB `WakeWordEvent::fontAdjust` → the controller bumps + persists the scale). **Gated "only when context is provided by the widget"** by extending display context with an **orthogonal `screen.font` sibling** (like `screen.timers`): the device reports `{scalable, scale, at_min, at_max}` every turn; the orchestrator adds the tool's per-turn prompt line only when `scalable`, so a bare "increase font" with nothing on screen is declined. This display always shows text (conversation/cards during a turn, the idle/away clock otherwise), so `scalable` is reported true whenever the UI is up. Core `FontContext` stores `scale` as `u16` permille to keep `DeviceContext`/`DecisionRequest` `Eq`; an optional System-1 `font_increase`/`font_decrease` fast path (ground-truth veto on `font.scalable`) is left as an additive follow-up. Full design: [`FontScalePlan.md`](./FontScalePlan.md). **Pending on-device QA.** |

---

## 1. System Architecture

```
[ Echo Show 8 (LineageOS) ]                         [ M4 Mac Mini (Wyoming host) ]
┌──────────────────────────┐                        ┌─────────────────────────────┐
│  Flutter UI Layer        │                        │  Wyoming STT (Whisper/CoreML)│
│  (Ambient screen, live   │                        │            │                │
│   transcript, reply text)│                        │            ▼                │
└───────────▲──────────────┘                        │  LLM backend                │
            │ FRB v2 streams / callbacks            │  (pluggable: Ollama / API)  │
┌───────────▼──────────────┐                        │            │                │
│  Rust System Engine      │ ── TCP (Wyoming) ────► │            ▼                │
│  • cpal/oboe capture     │ ◄── transcript ─────── │  Wyoming TTS (Piper)        │
│  • openWakeWord (tract)  │ ◄── synthesize audio ─ │                             │
│  • Wyoming client        │                        └─────────────────────────────┘
│  • cpal/oboe playback    │
└──────────────────────────┘
        ▲ mDNS discovery of _wyoming._tcp on the LAN
```

**End-to-end flow:** idle listening → wake word fires on-device → open Wyoming
socket → stream 16 kHz PCM → receive live `transcript` events (render on screen)
→ silence detected → LLM produces a reply (streamed tokens rendered as text) →
Piper synthesizes speech → audio frames stream back → Rust plays them through the
Echo Show speakers → return to idle.

---

## 2. Technical Stack

- **Frontend:** Flutter — always-on landscape layout, smooth live text rendering,
  simple image/status grids, tuned for the 8-inch display.
- **Systems layer:** Rust cross-compiled for Android. **The Echo Show 8 (`crown`)
  LineageOS build is 32-bit**, so the device target is `armv7-linux-androideabi`
  (armeabi-v7a), not `aarch64` — see Phase 1 ABI note in §3.
- **Interop:** `flutter_rust_bridge` v2 (FRB) — zero-copy Dart↔Rust data flow,
  generated stream sinks and callbacks.
- **Audio capture + playback:** `cpal` crate on the native Android `oboe-shared`
  backend (16 kHz, mono, 16-bit PCM for capture; playback of returned TTS frames).
- **Local wake word:** `tract-onnx` executing an **openWakeWord** `.onnx` model,
  fully offline in a low-overhead Rust thread.
- **Transport:** **Wyoming Protocol** over async `tokio` TCP.
- **Discovery:** mDNS / Zeroconf (`_wyoming._tcp`) to locate the Mac Mini.
- **Mac Mini services:** Wyoming STT (Whisper / CoreML) → pluggable LLM
  Anamanti Core → Wyoming TTS (Piper).

---

## 3. Implementation Phases

### Phase 1 — Workspace Setup & Cross-Compilation ✅ (implemented 2026-09-15)
- Unified monorepo: `/lib` (Flutter) and `/rust` (systems layer). **Done** —
  Flutter app scaffolded (`anamanti_display`, Android-only), FRB v2 integrated via
  `flutter_rust_bridge_codegen integrate` (adds `/rust` cdylib + `rust_builder`
  cargokit plugin).
- Configure `flutter_rust_bridge_codegen` to watch and generate JNI bindings.
  **Done** — `flutter_rust_bridge.yaml` (`rust_input: crate::api`, output
  `lib/src/rust`). Hello-world API in `rust/src/api/engine.rs` (`engine_greeting`,
  `engine_version`); regenerate with `flutter_rust_bridge_codegen generate`.
- Install the NDK target: `rustup target add aarch64-linux-android`. **Done.**
- Validate a hello-world cross-compile onto the LineageOS target via `adb`.
  **Done and verified on real hardware.** Built, installed, and launched on the
  physical Echo Show 8; the on-screen greeting text is returned by the Rust engine
  over the FRB bridge (`android/arm`). Deploy command:
  `adb install -r build/app/outputs/flutter-apk/app-release.apk`.

> **⚠️ ABI correction (measured on-device):** the plan originally assumed
> **`aarch64-linux-android`**, but the real Echo Show 8 (`crown`) LineageOS build
> is **32-bit only** — `ro.product.cpu.abilist = armeabi-v7a,armeabi`, empty
> `abilist64`, **Android 11 (SDK 30)**. An arm64 APK fails to install with
> `INSTALL_FAILED_NO_MATCHING_ABIS`. **The real device target is
> `armv7-linux-androideabi` (armeabi-v7a).** Build with `flutter build apk
> --release --target-platform android-arm` (`rustup target add
> armv7-linux-androideabi`). Keep arm64 only for any future 64-bit device.

> **Toolchain pin (decision):** `flutter create` (Flutter 3.44) scaffolded
> **Gradle 9.1 / AGP 9.0**, but the bundled cargokit Gradle plugin uses the
> legacy AGP variant API + `project.exec` that Gradle 9 removed. Pinned
> `android/settings.gradle.kts` to **AGP 8.7.3 / Kotlin 2.1.0** and the wrapper to
> **Gradle 8.11.1**. Revisit if/when cargokit ships AGP-9 support. NDK in use:
> `28.2.13676358`; Rust `1.94.0`; FRB `2.11.1`.

### Phase 2 — Native Audio & Local Inference (Rust) ✅ (implemented 2026-09-15)
- Initialize `cpal` to pull raw 16 kHz / mono / 16-bit PCM from the mic array.
  **Done** — `rust/src/audio/capture.rs` opens the default input device, downmixes
  interleaved frames to mono `i16` in the (allocation-free) real-time callback,
  and pushes into the ring buffer. Device-native rate is preserved on capture and
  converted to 16 kHz downstream by a linear resampler (`audio/resample.rs`), since
  mic arrays rarely deliver exactly 16 kHz. Requires `RECORD_AUDIO` (added to the
  Android manifest) + the microphone feature.
- Feed the buffer stream through a pre-allocated ring buffer. **Done** —
  `rust/src/audio/ring_buffer.rs` wraps a single-producer/single-consumer
  `ringbuf::HeapRb` (allocated once, ~2 s at 48 kHz) so the RT capture callback and
  the inference loop exchange samples with only atomic index updates — no locks, no
  per-frame heap churn.
- Integrate `tract-onnx` + openWakeWord to score wake-word confidence continuously.
  **Done** — `rust/src/wakeword/detector.rs` runs the full openWakeWord 3-model
  chain on `tract-onnx`: melspectrogram (fixed 1280-sample chunks → 32-bin mel
  frames, `mel/10 + 2`) → feature/embedding (sliding 76-frame window, 8-frame hop →
  96-d embedding) → classifier (last 16 embeddings → confidence). Feeding fixed
  chunks gives every model a concrete input shape so `tract` fully optimizes the
  graph; rolling state is carried across calls for continuous streaming scoring.
- Keep evaluation fully offline in a low-overhead thread loop to respect the 1 GB
  RAM limit. **Done** — `rust/src/engine/mod.rs` owns one background thread that
  holds the `cpal` stream (which is `!Send`) and runs the drain→resample→score
  loop off the RT path. A process-wide start/stop lifecycle is exposed over FRB
  (`start_wake_word_engine` returns a `Stream<WakeWordEvent>`; `stop_wake_word_engine`,
  `is_wake_word_engine_running`). If the model files are absent the engine degrades
  to a capture-only mode that emits RMS levels, so the capture→ring→resample path
  stays observable. Verified on the host via `cargo check`/`cargo clippy` (clean)
  and a release `android-arm` APK cross-compile.

> **Backend note (measured):** the locked decision was "`cpal` on the `oboe`
> backend". The pinned `cpal` 0.18 no longer uses `oboe` — on Android it drives
> the NDK's **AAudio** input directly (via the `ndk` crate). This still honors the
> locked decision's intent (Rust `cpal` is the single audio layer that owns
> capture + playback); only the underlying Android backend changed from oboe →
> AAudio. No app-visible difference. **Consequence:** `cpal` links the NDK
> `-laaudio` library, which exists only at **API level ≥ 26**, so the app's
> `minSdk` was raised to **26** (`android/app/build.gradle.kts`); cargokit
> cross-compiles the Rust `.so` at `minSdk`, and a lower value fails to link with
> `unable to find library -laaudio`. The Echo Show 8 runs Android 11 (SDK 30), so
> 26 is a safe floor.

> **Dep note:** bumped `cc` (build-dependency, transitive via `tract-linalg`) from
> the Phase-1-locked `1.0.83` to `1.4.6`; `tract-linalg` 0.23.7's build script
> calls `cc`'s `cargo_warnings`, which the older `cc` predates. FRB event types use
> a flat struct + unit-only-enum tag (not a data-carrying enum) to avoid pulling
> the Dart `freezed` codegen dependency into the app.

### Phase 3 — Wyoming Client Core Logic (Rust) ✅ (implemented 2026-09-15)
- mDNS discovery of `_wyoming._tcp`; cache the resolved host/port. **Done** —
  `rust/src/wyoming/discovery.rs` browses `_wyoming._tcp.local.` via `mdns-sd`,
  returns the first resolved `WyomingEndpoint` (address/port/hostname) within a
  timeout, and always tears the daemon down. An `EndpointCache` holds the
  last-known host; `resolve()` prefers a fresh browse and falls back to the cache
  when the browse times out (the plan's "cached fallback"; full reconnect/back-off
  is Phase 5). No static IP is ever configured.
- Connection-state struct over `tokio::net::TcpStream`. **Done** —
  `rust/src/wyoming/client.rs` `WyomingConnection` owns the split buffered
  read/write halves plus the PCM format, sets `TCP_NODELAY`, and exposes
  `send_audio_start`/`send_chunk`/`send_audio_stop`/`read_event`. It is generic
  over the IO so the turn driver is testable against an in-memory duplex.
- **State machine.** **Done** — `rust/src/wyoming/state.rs` is a *pure* transition
  function (`Idle → Triggered → Streaming → Closing → Idle`; the plan's
  "Silence/Timeout" is `Closing`). Keeping it side-effect-free makes the whole turn
  lifecycle exhaustively unit-testable. The async driver `client::run_turn` binds
  it to a socket: on connect it sends `audio-start`, streams PCM pulled from an
  mpsc channel as `audio-chunk`s, and reads events back; a server `transcript`
  (server-side VAD's end-of-speech) or an idle timeout drives `audio-stop` +
  teardown.
- **Wire format.** **Done** — `rust/src/wyoming/protocol.rs` encodes/decodes the
  Wyoming framing (newline-delimited JSON header + length-prefixed `data` block +
  binary `payload`), matching the reference `rhasspy/wyoming` implementation so we
  interoperate with off-the-shelf servers (Phase 4).
- **Engine integration.** **Done** — `rust/src/engine/net.rs` bridges the `!Send`
  capture thread to the async client: a single-worker tokio runtime spawns a turn
  task on wake-word detection, and the inference loop forwards resampled 16 kHz PCM
  to it over a bounded channel (drops on overflow — never blocks the RT path).
  Turn progress surfaces to Dart as new `WakeWordEvent` kinds (`Connecting`,
  `Streaming`, `Transcript`, `Disconnected`).
- Wake-word evaluation keeps running **during** a turn (full-duplex). AEC is
  deferred for v1; the interim mitigation is wired here as a configurable
  `active_threshold` — the engine requires a higher wake-word confidence (never
  below the idle `threshold`) whenever a turn is active. **Done.**
- Verified: `cargo test` (25 tests: protocol round-trips, state-machine
  transitions, discovery cache fallback, a duplex mock-server turn, an idle
  timeout, and a real-`TcpStream` loopback dial), `cargo clippy -- -D warnings`
  (clean), `dart analyze` (clean after FRB regen), and a release `android-arm`
  APK cross-compile (25.5 MB) — so `tokio` + `mdns-sd` + `serde` all link on the
  32-bit device target.

> **Dep note (Phase 3):** added `tokio` (rt-multi-thread/net/io-util/sync/time/
> macros), `serde`+`serde_json` (Wyoming JSON framing), and `mdns-sd` 0.11
> (`_wyoming._tcp` discovery). `tokio` was already transitively present via FRB, so
> this only promotes it to a direct dependency. The Wyoming networking uses one
> worker thread to stay light on the ~1 GB budget. `minSdk` is unchanged (still 26,
> set in Phase 2); none of the new crates raise the floor.

### Phase 4 — Assistant Pipeline (Mac Mini) ✅ (implemented 2026-09-15)
Implemented as a new standalone Rust crate in `/mac` (`anamanti_core`,
binary `anamanti-core`) — the "brain". It is a Wyoming **server** to the
Echo Show and a Wyoming **client** to the downstream Whisper (STT) and Piper (TTS)
services, inserting the pluggable LLM + persistent memory between them.
- Wyoming STT (Whisper/CoreML) with **server-side VAD** emits the final transcript.
  **Done** — `anamanti-core/src/wyoming/stt.rs` (`SttSession`) opens a `transcribe` +
  `audio-start` stream to the Whisper service, forwards the device's PCM, and waits
  for the server's `transcript` (server-side VAD end-of-speech) before sending
  `audio-stop`. The Anamanti Core relays that transcript back to the device.
- **Pluggable LLM backend** consumes the transcript, streams reply tokens.
  **Done** — `anamanti-core/src/llm/` defines the `LlmBackend` trait
  (`respond(turn) -> ReplyStream` of reply-token fragments) with three config-
  selected implementations: local `ollama` (streaming NDJSON `/api/chat`), cloud
  `anthropic` (Claude Messages API over SSE via raw HTTP — Rust has no official
  Anthropic SDK), and an offline `mock` echo backend. The pipeline only ever holds
  a `dyn LlmBackend`, so backends swap without touching the turn logic.
- **Persistent memory store** (**SQLite** on the Mac) supplies context and records
  facts/preferences across sessions; feeds the LLM prompt. Policy: **explicit +
  inferred**. **Done** — `anamanti-core/src/memory/` is a `rusqlite` (bundled SQLite) store
  with an **FTS5** index. `parse_command` handles voice "remember…"/"forget…"/
  "forget that" (applied directly with a spoken confirmation, skipping the LLM);
  `infer_memories` auto-extracts facts/prefs from ordinary turns with deterministic
  heuristics (no embedding model, no extra LLM pass in v1). Relevant entries are
  FTS-searched each turn and injected into the LLM system prompt. `list`/`delete`/
  `forget_*`/`clear` back the settings UI (Phase 6).
- Wyoming TTS (Piper) synthesizes the reply into audio frames. **Done** —
  `anamanti-core/src/wyoming/tts.rs` (`TtsSession`) sends one `synthesize` request and reads
  back the `audio-start` → `audio-chunk`… → `audio-stop` stream.
- Frames stream back to the Echo Show over the Wyoming socket. **Done** —
  `anamanti-core/src/orchestrator.rs` (`Pipeline::run_turn`) drives the full turn and relays
  each TTS frame to the device (a device that closed early — Phase-3 clients end on
  the transcript — is tolerated, not an error). `anamanti-core/src/server.rs` accepts device
  connections and `anamanti-core/src/discovery.rs` advertises `_wyoming._tcp` over mDNS so the
  device discovers the host with no hardcoded IP (symmetric with the Phase-3 browse).
- Verified: `cargo test` (28 tests: Wyoming codec round-trips, STT/TTS session
  framing, Ollama-NDJSON + Claude-SSE stream parsing against loopback HTTP servers,
  memory FTS/commands/inference, and three full-pipeline integration tests that
  drive a mock device + mock Whisper/Piper through the real `Pipeline`),
  `cargo clippy --all-targets -- -D warnings` (clean), `cargo fmt --check` (clean),
  and a `--release` build of the `anamanti-core` binary.

> **Config note (Phase 4):** the Anamanti Core is environment-driven
> (`anamanti-core/src/config.rs`). `ANAMANTI_LLM_BACKEND=ollama|anthropic|openai|mock` selects
> the backend (default `ollama`, model `llama3.2`; `anthropic` needs
> `ANTHROPIC_API_KEY`, default model `claude-opus-5`; `openai` needs `OPENAI_API_KEY`,
> default model `gpt-4o-mini`, via `ANAMANTI_OPENAI_MODEL`/`ANAMANTI_OPENAI_MAX_TOKENS`;
> small `max_tokens` for spoken replies).
> `ANAMANTI_STT_ADDR` / `ANAMANTI_TTS_ADDR` point at the Whisper (10300) and Piper
> (10200) Wyoming servers, and the host advertises `_wyoming._tcp` on
> `ANAMANTI_BIND_ADDR` (default `0.0.0.0:10700`). Whisper and Piper themselves are
> **off-the-shelf Wyoming servers** (rhasspy ecosystem); the Anamanti Core is a
> client to them, not a reimplementation.

> **Dep note (Phase 4):** a host-only crate, independent of the device `/rust`
> crate (which is an Android `cdylib` linking `cpal`/`tract`/JNI and cannot be
> shared as a library on the Mac). Uses `tokio`, `serde`/`serde_json`, `reqwest`
> (rustls-tls; NDJSON + SSE streaming), `rusqlite` (bundled SQLite + FTS5),
> `mdns-sd` (advertise), `async-trait`/`async-stream`/`futures-util`, and
> `env_logger`. The Wyoming wire codec is re-implemented in `anamanti-core/src/wyoming/
> protocol.rs` (kept byte-identical to the device's, guarded by round-trip tests)
> rather than shared across the two crates.

### Phase 5 — Reactive UI & Playback (Flutter + Rust) ✅ (implemented 2026-09-15)
- Dart screen state consumes FRB `StreamSink` events (transcript + reply tokens).
  **Done** — `lib/src/engine/assistant_controller.dart` folds the single
  `Stream<WakeWordEvent>` into an observable `AssistantState` (phase, transcript,
  streamed reply, connection status). The engine stream is **injected** so
  widget tests drive synthetic events with no native `.so`.
- Modular responsive 8-inch layout; smooth token-by-token rendering. **Done** —
  `lib/src/ui/`: `ambient_screen.dart` (layered slideshow + scrim + conversation +
  clock + status), `conversation_view.dart` (transcript/reply bubbles with a
  streaming caret, reading-column capped for the 8-inch display),
  `status_indicator.dart`. Reply tokens accumulate into the reply bubble as they
  arrive (`WakeWordEventKind::ReplyToken`). The reply text stays on screen for as
  long as the audio is being read aloud: the device relays audio faster than
  real-time, so the turn returns to idle while the playback ring is still draining;
  Rust tracks ring occupancy (`PlaybackSink::pending()`) and emits a UI-only
  `SpeakingDone` when it empties (or a barge-in flushes it), which is when the UI
  removes the text (`AssistantState.audioPlaying` / `displayActive`).
- **Idle ambient screen:** photo slideshow from a chosen Google Photos/Drive folder
  (**on-device OAuth**). Slideshow keeps running when disconnected. **Done** —
  `lib/src/slideshow/photo_source.dart`: a pluggable `PhotoSource`
  (`LocalPhotoSource` offline ambient gradients + `GooglePhotoSource` seam for the
  Phase-6 on-device OAuth folder picker) driven by a timer-based
  `SlideshowController` that is fully decoupled from connectivity and falls back to
  local imagery if the source fails. *(Per Plan §4, the real Google consent flow +
  folder picker land in the Phase-6 settings screen; the slideshow talks to the
  `GooglePhotoSource` seam.)*
- **Resilience:** auto-reconnect with backoff via mDNS + a subtle **disconnected**
  status indicator; wake words queue until reconnected. **Done** — the controller
  restarts capture with exponential backoff on a fatal engine error/stop (the
  engine-side half); the network-side half is the Rust turn's per-attempt mDNS
  re-discovery + cached fallback. A `disconnected` reason other than "turn
  complete" flags the subtle **Offline** chip while the slideshow keeps running.
- Rust (`cpal`) plays returned TTS audio frames through the speakers. **Done** —
  `rust/src/audio/playback.rs` opens the default output device (AAudio on-device),
  resamples each TTS `audio-chunk` to the output rate, fans mono → N channels, and
  feeds a lock-free ring the RT output callback drains (silence on underrun). The
  `!Send` output stream lives on the engine thread beside capture; the `Send`
  `PlaybackSink` goes to the turn task.
- **Full-duplex barge-in:** a wake word during playback interrupts and starts a new
  turn; **raise the wake-word threshold during playback** to suppress self-triggers.
  **Done** — the device turn state machine gained a `Speaking` state (kept
  `is_active()`, so the higher `active_threshold` stays in force during playback).
  A mid-turn detection calls `Network::on_wake_word`, which flushes playback,
  interrupts the running turn over a channel, and the finishing turn spawns a fresh
  one (`pending_restart`).
- Verified: `cargo test` (device 32 tests incl. playback resample/channel-fan-out,
  the SPEAKING transition, a full transcript→reply-token→TTS-playback turn, and a
  barge-in interrupt; mac 28 tests incl. the reply-token relay), `cargo clippy
  --all-targets -D warnings` + `cargo fmt --check` (both crates clean), FRB
  regenerated, `flutter analyze` clean, `flutter test` (3 reactive-UI widget
  tests), and a release `android-arm` APK cross-compile (26.3 MB) — so cpal's
  AAudio **output** and `path_provider` link on the 32-bit device target.

> **Wire note (Phase 5):** the device↔Mac hop gained one project-local event,
> `reply-token` (`data.text`), so the Anamanti Core streams LLM reply fragments to
> the device for token-by-token rendering. It rides the existing Wyoming framing
> and is only on the device↔Anamanti Core hop (no off-the-shelf server sees it);
> the codec is kept byte-identical on both sides (`rust/` and `mac/`). TTS audio
> continues to use standard `audio-start`/`audio-chunk`/`audio-stop`.

### Phase 6 — Settings & Persistence ✅ (implemented 2026-09-15)
- Settings screen exposing: **LLM backend**, **TTS voice**, **wake word**, and
  **photo source** (with on-device Google auth + folder picker). **Done** —
  `lib/src/ui/settings_screen.dart` groups the four areas. Wake word + detection
  thresholds and photo source are **device-local** (persisted); LLM backend/model
  + TTS voice are **Anamanti Core-managed** (read/changed over the network). Device
  settings persist to a JSON file via `lib/src/settings/settings_store.dart`
  (`AppSettings`, `path_provider`; injectable file path for tests — no
  `shared_preferences` plugin). The Google photo source is wired as a testable
  **seam** (`GoogleAuthenticator` + `photoSourceFromSettings` in
  `photo_source.dart`): the default `StubGoogleAuthenticator` reports honestly that
  real OAuth needs a registered client ID (per Plan §4, the environment can't
  provision one), while the folder-name/link UI and the `GooglePhotoSource` path
  are in place for it.
- **Memory management** in settings: view/delete stored items; voice commands
  ("remember…", "forget that"). **Done** — `lib/src/ui/memory_screen.dart` lists
  the Anamanti Core's stored facts/preferences and deletes individual entries or
  clears all, over the control protocol. The **voice** half already lands on the
  Mac during a turn (Phase 4 `parse_command`); this is the settings-list half.
- Wire settings through FRB to the Rust engine and the Mac Anamanti Core. **Done** —
  two wiring paths:
  - *Device-local → Rust engine:* changing the wake word/thresholds rebuilds the
    `WakeWordConfig` (`buildWakeWordConfigFrom`) and restarts the engine from
    `main.dart`; the photo source change refreshes the slideshow.
  - *Device → Mac Anamanti Core:* a **project-local Wyoming control protocol** on
    the device↔Mac hop (symmetric with the Phase-5 `reply-token` extension). New
    `anamanti-*` frames (`anamanti-describe-settings` / `anamanti-set-settings` /
    `anamanti-list-memories` / `anamanti-delete-memory` / `anamanti-clear-memories`
    and their responses) ride the existing framing. The Anamanti Core's
    `Pipeline` now reads a per-turn snapshot of runtime-swappable
    `SharedSettings` (`anamanti-core/src/settings.rs`), so the LLM backend + Piper voice
    change between turns with **no restart**; `anamanti-core/src/control.rs` answers the
    frames from the memory store + settings, and `anamanti-core/src/server.rs` routes control
    vs. turn frames on one accept loop. The device speaks the protocol from
    `rust/src/wyoming/control.rs`, exposed to Flutter as async FRB functions
    (`rust/src/api/settings.rs`: `fetch_orchestrator_settings`,
    `update_orchestrator_settings`, `list_memories`, `delete_memory`,
    `clear_memories`).
- Verified: `cargo test` (mac 42 tests incl. `SharedSettings` swap/atomic-reject,
  the `control::respond` surface, and a loopback control-protocol integration test;
  device 39 tests incl. control round-trips against a loopback Anamanti Core and a
  real bundled-model load/score on `tract`),
  `cargo clippy --all-targets -- -D warnings` + `cargo fmt --check` (both crates
  clean), FRB regenerated, `flutter analyze` clean (app code), `flutter test`
  (15 tests: settings store round-trip, settings-screen apply/offline/navigation,
  memory view/delete, plus the Phase-5 reactive-UI tests), and a release
  `android-arm` APK cross-compile (31.6 MB, incl. the bundled wake-word models) —
  so the new FRB control functions + settings UI build and link on the 32-bit
  device target.

> **Wake-word models (Phase 6):** the openWakeWord ONNX chain is now **bundled**
> in the app (`assets/models/`: `melspectrogram.onnx`, `embedding_model.onnx`, and
> the `hey_jarvis.onnx` classifier — from the dscripka/openWakeWord v0.5.1 release).
> `lib/src/engine/model_assets.dart` unpacks them to the on-device model dir on
> first run (preserving any user-dropped models), so a fresh install detects the
> wake word with no manual `adb push`. The default wake word is now `hey_jarvis`
> (the bundled classifier); the other names in the settings list work once their
> `<name>.onnx` is dropped in. A device-crate test loads the real bundled chain on
> `tract` and scores a stream, guarding that the shipped models parse and their
> shapes line up. VAD runs off-device on the Anamanti Core, so **no `silero_vad.onnx` is bundled on the device** (the device runs no VAD). (Note: an *opt-in* Silero VAD engine is planned **Core-side** — see the *"End-of-speech VAD — pluggable engine + Silero"* decision row + [`VadSileroPlan.md`](./VadSileroPlan.md); that model would live on the Mac, not in the APK.)

> **Model selector (Phase 6 follow-up):** the assistant's model is chosen from a
> **drop-down of specific Anthropic / OpenAI models released in the last 12 months**
> (superseding the earlier free-text field). A new **OpenAI chat backend**
> (`anamanti-core/src/llm/openai.rs`, streaming `/v1/chat/completions`, `OPENAI_API_KEY`) sits
> behind the same `LlmBackend` trait next to `ollama`/`anthropic`/`mock`, so a picked
> GPT/o-series model drives the real turn. `anamanti-core/src/llm/catalog.rs`
> (`ModelCatalog`) builds the list: it live-queries each provider's `GET /v1/models`,
> filters to the trailing 12 months (Anthropic `created_at`, OpenAI `created` + a
> chat-model filter), caches ~1h, and falls back to a curated built-in list when a
> key is missing/offline. It is exposed to the device via `anamanti-list-models` →
> `anamanti-models` (new FRB `list_models` → `listModels`) and to the browser via
> `GET /models` on the config page. The chosen model flows through the existing
> `SharedSettings::apply` → `LlmFactory::build` path and persists to
> `anamanti_settings.json`; the settings screen shows a dropdown for cloud backends
> and keeps free text for local Ollama tags. Verified: mac `cargo test` (native +
> helix) + clippy/fmt + `--features rig` check, device `cargo test`
> (`list_models` round trip), FRB regen, `flutter analyze` + `flutter test`.

> **TTS voice dropdown (Phase 6 follow-up):** the settings TTS voice field is a
> dropdown of **installed** Piper voices, mirroring the model dropdown. On
> `anamanti-list-voices`, the Anamanti Core queries Piper's advertised catalog (a
> downstream Wyoming `describe` → `info`, parsed in `anamanti-core/src/wyoming/tts.rs`
> `describe_voices`/`parse_voice_catalog`) and, when `ANAMANTI_TTS_VOICES_DIR` is set
> (Piper co-located on the Mac), keeps only voices whose `<name>.onnx` is present on
> disk; unset → the full advertised list. Exposed via `anamanti-list-voices` →
> `anamanti-voices` (new FRB `list_voices` → `listVoices`, `VoiceInfo`/`VoiceOption`)
> and to the browser via `GET /voices` on the config page. Both the device screen
> and the config page show a "Server default" entry, preserve a hand-set voice not
> in the list, and fall back to a free-text field when the list is unavailable.
> Verified: mac
> `cargo test` + clippy, device `cargo test` (`list_voices` round trip) + clippy,
> FRB regen, `flutter analyze` + `flutter test` (voice dropdown + free-text fallback).

> **Anthropic auth mode (Phase 6 follow-up):** cloud Claude can authenticate with an
> **API key** *or* a **Claude subscription (OAuth)**. `anamanti-core/src/llm/anthropic_auth.rs`
> adds `AnthropicAuth` + an `AnthropicTokenProvider` that resolves a subscription
> Bearer token from `ANTHROPIC_OAUTH_TOKEN` (from `claude setup-token`) or a
> configurable token-printing command (`ANAMANTI_ANTHROPIC_TOKEN_CMD`, default the
> `ant` CLI), cached with a short TTL and refreshed on a 401; the subscription path
> sends `Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20` (no `x-api-key`).
> The mode is a per-turn `RuntimeSettings` field, rides the `anamanti-set-settings`
> frame + config page as `anthropic_auth`, persists to `anamanti_settings.json`, and
> is surfaced in the device settings screen (a selector shown for the Anthropic
> backend) and the config page. OpenAI stays API-key-only (no subscription→API path).
> `ANAMANTI_ANTHROPIC_AUTH=apikey|subscription` seeds the initial mode.

> **Debug GUI pages (Phase 6 follow-up):** the loopback config server
> (`anamanti-core/src/webconfig.rs`) now also serves four read-only inspection pages —
> `/chatlog` (recent turns), `/prompts` (the exact assembled LLM prompt per turn,
> captured to a separate `anamanti_promptlog.jsonl` via `memory::PromptLog`, env
> `ANAMANTI_PROMPTLOG_PATH`), `/sqlite` (the memory store rows), and `/helix`
> (HelixDB GraphRAG node counts + a sample of nodes, behind the read-only
> `memory::GraphView` seam; reports "disabled" on the SQLite backend). Each has a
> `*.json` data endpoint and a shared nav bar. Still no auth — loopback only.
> Verified: mac `cargo test` (native + helix) + clippy `-D warnings`, plus an
> end-to-end curl of every page/endpoint against a running mock-backend Anamanti Core.

> **Wire note (Phase 6):** the `anamanti-*` control frames are project-local to the
> device↔Anamanti Core hop (no off-the-shelf Wyoming server sees them), kept
> byte-identical in `rust/` and `mac/` `wyoming::protocol::types` and guarded by
> round-trip tests on both sides. Selecting the cloud (anthropic) backend at
> runtime still requires an API key on the Anamanti Core, but that key no longer has
> to be present at boot (see the runtime-key note below); if none is available the
> change is rejected **in-band** (the response's `ok:false` + message) and the
> previous backend stays in force, so a control error never drops the device
> connection or the turn.

### Camera proximity → screen brightness (post-Phase-6, 2026-09-21)

The Echo Show's **front camera is used as a proximity sensor** to brighten the idle
screen when someone approaches and dim it when the room is quiet — no ML, no face
recognition, no frames ever leave the device.

- **Boundary:** sensing in Rust, actuation in Flutter (per `agents.md`). A Kotlin
  `CameraBridge` (the visual twin of `MicBridge`) opens the front camera via Camera2
  at a tiny resolution (176×144, ~5 fps), pulls the packed **Y (luma)** plane from
  each frame, and pushes it over JNI to `rust/src/camera/bridge.rs`. The pure,
  unit-tested `camera/presence.rs` `PresenceDetector` measures the **mean absolute
  per-pixel luma delta** (frame-to-frame motion); it flips to *present* above a
  motion threshold and back to *absent* after a quiet **release window** (defaults
  2.5 / 20 s, both config-tunable via `WakeWordConfig`). Transitions emit a new
  `WakeWordEventKind::Presence` (`present: bool`) on the existing engine event stream.
- **Actuation (presentation):** `AssistantController` folds `Presence` into
  `AssistantState.userPresent`; `ScreenBrightnessController` crosses a
  `anamanti_display/brightness` `MethodChannel` to `MainActivity`, which sets the
  window `screenBrightness` (near 1.0 / away 0.25, tunable). Starts **bright** and
  stays bright if the camera is unavailable.
- **Away-mode UI:** `userPresent` also drives the ambient screen's presentation, not
  just the backlight — when nobody's in front (and no turn is active), everything
  fades out and only a **large, dimmed, centered clock** remains (`AmbientScreen`
  `offMode = !userPresent && !displayActive`); the slideshow, status chip, settings,
  timers, and wake-word hint all hide. Someone approaching flips it back to the full
  ambient screen. This makes presence visible independent of the (subtle, panel- and
  ambient-light-dependent) backlight change.
- **Lifecycle & failure modes:** the camera is opened alongside the engine and
  released on teardown; it is fully independent of the audio pipeline. It **degrades
  silently** — `CAMERA` permission denied, the **physical privacy shutter** closed
  (frames go black → no motion → treated as absent/quiet), or no front camera — never
  breaks capture, wake-word detection, or a turn. Known v1 limitation: a perfectly
  **still** person eventually reads as absent and the screen dims until the next
  movement (motion-only signal, by design/choice).
- **Permission:** `CAMERA` is a runtime-dangerous permission declared in the
  manifest (`uses-feature` `required=false`) and, like `RECORD_AUDIO`, granted
  out-of-band on the kiosk device: `adb shell pm grant com.anamanti.anamanti_display android.permission.CAMERA`.
- **Dim delay (settings, 2026-09-22):** the **release window** — how long the screen
  stays fully bright after the room goes quiet before dimming to the away-mode clock —
  is now a **device-local setting** (Settings → *Display* → "Stay bright before dim",
  5 s–5 min; default 20 s). Persisted in `AppSettings.dimDelaySecs`, mapped to
  `WakeWordConfig.proximityReleaseSecs`, and applied by restarting the engine on Save
  (so the `PresenceDetector` rebuilds with the new window).
- **Follow-ups (not done):** no settings-screen on/off toggle yet (on by default via
  `WakeWordDefaults.cameraProximity`); brightness levels + motion threshold are still
  compile-time defaults. Could later add an ambient-light (mean-luma) auto-brightness
  curve and/or a proximity on/off toggle.

### Tool calling & actions (post-Phase-6)

The assistant can call **tools** to fetch info and take actions. Mechanism: the
LLM is handed tool definitions (name + description + JSON-schema args); if it asks
for one, the Anamanti Core executes it, threads the result back, and the model
answers (`anamanti-core/src/llm/rig.rs`, up to `MAX_TOOL_ROUNDS`). Adding a capability = one
more tool in `Tools`.

- **Engine:** rig-core is **always compiled in** and is the **default** engine
  (`ANAMANTI_LLM_ENGINE=rig`); `native` opts back out to the tool-less HTTP backends.
  Tool turns answer **non-streaming** (rig 0.42's ollama streaming parser drops
  non-final tool calls) — fine, since the reply is spoken whole.
- **Info tools (Mac-side):** `internet_search`, on by default (`llm.web_search`).
  The default provider is now **Tavily** (`llm.search_provider="tavily"`, needs the
  `TAVILY_API_KEY` secret); keyless **DuckDuckGo** is the alternative. **Weather works** via
  this tool; it is grounded by a configured home location + units
  (`ANAMANTI_HOME_LOCATION` / `ANAMANTI_WEATHER_UNITS`), injected into the system
  prompt next to the wall-clock and speaker-identity lines (`location_line`).
  `calendar_lookup` reads connected web `.ics` calendars (`ANAMANTI_CALENDARS`).
  `directions_lookup` returns real distance, travel time, and **live traffic**
  between two places (`anamanti-core/src/directions/`, `DirectionsProvider` trait;
  v1 backend **Mapbox** via `MAPBOX_TOKEN` / `ANAMANTI_DIRECTIONS_PROVIDER`). The
  origin defaults to `ANAMANTI_HOME_LOCATION`; it is **voice-only** in v1 (spoken ETA
  + traffic note; an on-screen route/map is a deferred follow-up). Each info tool is
  advertised only when its config is present.
- **Shopping list (Cadora) — `shopping_list_add`:** adds items to the household's
  **shared Cadora shopping list** (the same backend NextHaul and the family apps use),
  so "add milk to the shopping list" writes to the real list. The Anamanti Core is a
  headless **voice front-end** on Cadora's built-in voice API
  (`anamanti-core/src/cadora/`, `GroceryController` trait): each add is one
  `POST /voice/command` (Bearer `vl_…` voice-link token) with the canonical
  `{action:"add_shopping_item", item, quantity}` body — our LLM has already parsed
  intent, so Cadora's NLU is skipped — and the server's spoken confirmation is relayed.
  It never touches Supabase/RLS directly (Cadora owns member attribution, list
  creation, dedupe). **Linking:** in the **NextHaul** app open **Settings → Voice &
  Integrations**, add a voice assistant to mint a **6-digit code** (`POST
  /voice/links/pair`); enter it on the config page (Household tab → "Shopping list
  (Cadora)"), which redeems it (`POST /voice/links/redeem`) for a durable `vl_…` token
  stored in `anamanti_settings.json` (0600) — the Anamanti Core is the "speaker" that
  redeems, exactly as NextHaul's own client expects. (A directly-pasted token is also
  accepted as a fallback.) Config seed:
  `cadora{base_url,link_token}` (base_url defaults to `https://cadora-server.fly.dev`).
  The tool is advertised only when linked; a change relinks/withdraws it live (no
  restart), exactly like `spotify_control`.
- **Phone messages (AppSaid) — `send_phone_message`:** pushes a message — often a
  Google Maps link — to a named family member's phone via **AppSaid**, a self-hosted
  HTML-push service (`anamanti-core/src/appsaid/`, `PhoneMessenger` trait, mirroring
  `GroceryController`). Each send is one `POST {worker_url}/v1/messages` with
  `{token, user, title, body, url, url_title}`: the tool resolves the spoken recipient
  name (case-insensitive) to that person's AppSaid `user_key`, builds a Maps link from a
  `place` when no explicit `url` is given, and speaks back `Sent to <name>.` Config seed
  `appsaid{worker_url, recipients{name:user_key}, default_recipient}`; the sender **app
  token is a SECRET** (`APPSAID_APP_TOKEN` env or config page → Household tab, stored
  0600, never in the JSON). The tool is advertised only when the worker URL + app token +
  at least one recipient are all present; a change re-advertises/withdraws it live (no
  restart), exactly like `spotify_control`.
- **Device actions (implemented, on-device):** the `set_timer` / `cancel_timer`
  tools emit a `DeviceAction` onto a per-turn `ActionSink` (`LlmTurn.actions`); the
  Anamanti Core's reply loop drains it (`drain_device_actions`) and relays an
  `anamanti-timer` frame on the turn socket. The device decodes it
  (`TurnUpdate::Timer`) and its long-lived `TimerManager`
  (`rust/src/engine/timer.rs`) owns each countdown (unlimited, outliving the turn
  socket + surviving a Mac disconnect), sounds a synthesized chime via the shared
  `PlaybackSink`, and emits `WakeWordEventKind.timer{Started,Finished,Cancelled}`.
  Flutter renders a countdown-chip overlay visible during idle + turns. The full
  add-a-capability process is documented in `architecture.md` §8.
- **Calendar / reminders:** calendar shipped (read-only web `.ics`); **reminders**
  designed, **deferred** — backend (macOS EventKit vs Google vs CalDAV) undecided.

> **Runtime provider keys (Phase 6 follow-up):** a cloud API key no longer has to be
> in the environment at launch. `SettingsUpdate` gains optional `anthropic_api_key` /
> `openai_api_key` fields (tri-state like the Tavily search key: absent = keep,
> `null` = clear, value = set); `SharedSettings::apply` builds the backend with a
> factory clone whose keys reflect the target, so a key entered at runtime enables the
> cloud backend **without a restart**. Keys live in `RuntimeSettings` (the env var is
> only the boot seed) and persist to `anamanti_settings.json` (0600); on load a
> persisted key overlays the env key only when present, so a pre-feature settings file
> can't wipe a working env key. Key **entry** is exposed only on the loopback config
> page (`webconfig.rs` — password fields shown per backend); the device/Wyoming
> control path never accepts a provider key, and both surfaces report only
> `anthropic_key_set` / `openai_key_set` (a boolean), never the key value. Endpoints
> (base URLs) stay boot-time env only — deferred. Verified: mac `cargo test`
> (107 unit + integration, incl. runtime-key enable + persist tests) + clippy `-D
> warnings` + fmt.

### In-app APK auto-updater (post-Phase-6, 2026-10-01)

Self-rolled over-the-air updates for the Display, pulling a signed APK +
`latest.json` from a **Cloudflare R2** custom domain — making updates first-party
instead of depending on the third-party **Obtainium** app. Full design, release
runbook, and follow-ups: [`UpdaterPlan.md`](./UpdaterPlan.md).

Decisions (confirmed with the owner this change):
- **Networking stays in Rust** behind the FRB boundary (honors the "Rust owns
  networking" locked decision): `check_for_update` fetches `latest.json`,
  `download_update` streams the APK to the cache dir and verifies a **SHA-256** as
  bytes arrive (`rust/src/update/`, `rust/src/api/updater.rs`). Client =
  **`ureq` + rustls/`ring` + `webpki-roots` + `sha2`**, deliberately **not** reqwest
  (its aws-lc-rs / native-tls don't reliably cross-compile for 32-bit armv7 Android).
- The **install** is native Kotlin (`PackageInstaller`), via a new
  `anamanti_display/updater` MethodChannel + `InstallReceiver` — the one capability
  with no Rust/Dart equivalent. Flutter owns the orchestration + UI
  (`UpdateController`, the `UpdateBanner`, and a Settings → **Updates** page with the
  base URL + auto-update toggle).
- **Keep the existing GitHub Releases + Obtainium path** too for now
  (`.github/workflows/release.yml`, updated to `--flavor selfUpdate`); retire it once
  the in-app path is hardware-proven.
- New **`distribution` build flavors**: `selfUpdate` (ships the updater) and
  `fdroid` (ships without it — no `REQUEST_INSTALL_PACKAGES`, UI hidden), same key +
  `applicationId`. **Every APK build/run now names a flavor** (`agents.md` updated).
- Suites green: Rust `cargo test` + `clippy -D warnings`; `dart analyze` +
  `flutter test` (incl. `test/update_controller_test.dart`); FRB codegen (pinned
  2.11.1). **Not yet verified on hardware** — see `UpdaterPlan.md` / `TODO.md`.

---

## 4. Resolved Details & Implementation Notes

All prior open questions are now decided (see the decision table). Notes for
implementers:

- **AEC / full-duplex:** ship full-duplex without dedicated AEC. Interim
  mitigation: **raise the wake-word confidence threshold while in SPEAKING** to
  suppress self-triggers from the device's own speaker. Only build real AEC
  (Rust-side WebRTC/speexdsp with the playback signal as reference, or Android
  hardware `AcousticEchoCanceler`) if self-triggering remains a problem.
- **Photo auth (two backends, selectable in settings):**
  - **Google Photos Ambient API** (the ideal): OAuth 2.0 for TVs and Limited Input
    devices (device-code + QR), fully on-device — sign-in QR → `createDevice` →
    album-picker QR → poll `mediaSourcesSet` → `listMediaItems` (`baseUrl` +
    `=w…-h…` + Bearer). Scope `photosambient.mediaitems` (the flow rejects
    `photosambient.tv`). **Gated: the Ambient API requires Google Photos Partner
    Program acceptance** — `createDevice` returns 403 until the project is accepted
    (a valid token/scope is not enough). Built + on-device; works once accepted.
  - **Google Drive** (interim): `drive.readonly` folder listing. The device-code
    flow rejects Drive scopes, so consent runs **once on the Mac — inside the
    Anamanti Core** (`anamanti-core/src/drive_consent.rs`, loopback+PKCE, a **"Desktop
    app"** client), driven from the config page **Photos tab** (`/drive`). The
    Anamanti Core holds the client id/secret + refresh token + folder ids
    (`DriveConfig` in `anamanti_settings.json`) and the device **pulls the bundle over
    Wyoming** (`anamanti-get-drive-token`), minting access tokens on-device. Reads a
    folder (owned or "Shared with me"); `drive.file` is unusable here (can't list a
    folder without an interactive Picker). *(Migrated 2026-09-22 from the standalone
    `tools/google_photo_consent.py` + adb-push + Settings "Import" flow.)*
  - Only the **TV (Ambient)** client creds are injected at build via
    `--dart-define-from-file=google_oauth.json` (gitignored): `GOOGLE_OAUTH_*`. The
    APK is **credential-free for Drive** (`AppSettings.driveConfigured` is a runtime
    check); the Mac's Drive client comes from `ANAMANTI_GOOGLE_DRIVE_CLIENT_ID/SECRET`
    (optional `_FOLDER_IDS`). Ruled out: the Photos Library API (no library read
    since March 2025). Implemented in `slideshow/ambient_photos.dart`,
    `drive_photos.dart`, `photo_source.dart`, settings, `main.dart`, and Anamanti Core
    `drive_consent.rs`/`webconfig.rs`/`control.rs`. Consent screens stay in
    **Testing** mode with household test users.
- **Persistent memory:** **SQLite** on the Mac. Schema holds structured facts +
  preferences with full-text search (no embedding model for v1). Policy is
  **explicit + inferred** — store on request and auto-extract facts/prefs from
  turns. UX: a memory list in **settings** (view/delete) plus **voice** commands
  ("remember…", "forget that"). Kept until cleared.
- **Resilience:** **auto-reconnect** with exponential backoff via mDNS. The idle
  photo slideshow keeps running while disconnected; a subtle **disconnected**
  indicator shows status; wake words queue until the socket is restored.

### Remaining risk to watch

- Whether threshold-tuning alone is enough to keep self-triggering tolerable
  during playback — measure on real hardware; AEC is the fallback.

---

## 5. AI Prompting Targets

- **Cargo setup:** "Generate a `Cargo.toml` for an Android target (aarch64) using
  `flutter_rust_bridge` v2, `cpal` with the oboe backend, `tokio` async, and
  `tract-onnx` for minimal-dependency neural-net execution."
- **Protocol interface:** "Write an async Rust Wyoming-protocol client using
  `tokio::io` that parses newline-delimited JSON frames and streams raw audio
  chunks immediately after the setup metadata, handling both `transcript` and
  synthesized-audio return frames."
- **mDNS discovery:** "Write a Rust routine to browse `_wyoming._tcp` via mDNS and
  resolve the Mac Mini's host/port, with a cached fallback."
- **LLM abstraction:** "Define a Rust/host-side trait for a pluggable LLM backend
  with a streaming `respond(transcript) -> token stream` method, implemented for
  both a local Ollama endpoint and a cloud API."
