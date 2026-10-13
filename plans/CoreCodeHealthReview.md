# Anamanti Core — Code Health Review

2026-10-13 · Living copy: [Claude doc](https://claude.ai/code/artifact/1e190124-ffbc-464c-a0b5-f608c1d703e7)

## Summary

The Core is in reasonable shape at its seams, but duplication has already turned into live bugs: about ten behaviour or security defects came out of a review that was looking for DRY, dead code and structure. The single most valuable fix is consolidating how the LLM tool set is rebuilt, because nine drifted copies of that code silently drop or resurrect tools today.

Scope: all of `anamanti-core/src` (about 41,000 lines of Rust across 66 files), reviewed in five slices: turn pipeline, LLM and System-1, config/settings/web admin, memory and speaker, and tool integrations. Findings were checked by grepping callers across `src/`, `tests/` and `examples/`; I re-verified the high-severity ones by hand. Nothing was edited or built.

What stands out:

- **Saving one setting can break an unrelated tool (A1).** Saving weather or places removes `send_phone_message`; pairing a phone drops runtime-entered Mapbox, Visual Crossing and Places keys; `places.enabled=false` is undone by any save.
- **Two security gaps are small to close.** The admin page accepts cross-site POSTs (A6), and the Visual Crossing key and Mapbox token can be sent to the LLM provider and written to logs inside error text (A7).
- **Engine choice silently decides features (A2, A3).** Claude-subscription and OpenAI turns get no tools and no history; on the rig engine, streaming never runs in production, so every reply waits for the full completion before TTS starts.
- **Speaker merge forgets history (A8).** The HelixDB graph keeps the dropped speaker's id, so the kept person's GraphRAG recall loses it.
- **Three big modules carry most of the structural debt:** `orchestrator.rs` (3,114 lines), `llm/rig.rs` (3,400) and `settings.rs` + `webconfig.rs` (7,158 together).
- **Dead code is modest:** no `#[allow(dead_code)]` anywhere, but about 30 unused items, two unread config keys, an example-only `aec` module, and stale feature-gate comments.

Issue ids (A = architecture, D = DRY, X = dead code) are used in the decision table at the end.

## Architectural difficulties

Twenty-eight issues, split into defects that misbehave today and structural problems that make change expensive. Paths are relative to `anamanti-core/src/`.

### Live bugs and security

| ID | Issue | Where | Impact |
| --- | --- | --- | --- |
| A1 | The LLM tool-set rebuild is copied 9 times and the copies have drifted. `apply_weather_tool` / `apply_places_tool` never set `factory.appsaid`; `apply_household` / `apply_appsaid` never re-derive directions, weather or places; every runtime copy hardcodes `places::from_key("google", …)` | `settings.rs:1719-2723`, `config.rs:2306-2339` | Saving weather or places removes `send_phone_message`. Pairing a phone drops runtime Mapbox / Visual Crossing / Places keys. `places.enabled=false` is undone by any save |
| A2 | Engine choice silently gates features: tools are attached only for rig + Anthropic API key or rig + Ollama. Subscription auth and OpenAI always get a native backend that sends only `[system, user]` | `settings.rs:1073-1195`, `llm/anthropic.rs:97`, `llm/openai.rs:54` | A Claude-subscription user gets no timers, weather or any device action, and follow-up turns lose context. Only one warning, for `web_search` |
| A3 | Rig's streaming branch is unreachable in production: `tools_from_config` always returns 4+ tools, so every turn takes the non-streaming `completion()` loop | `llm/rig.rs:2100-2139`, `2371-2409` | Every reply waits for the full completion before TTS starts; the first-token log never fires |
| A6 | Admin endpoints have no `Origin` / `Host` check and parse any body as JSON | `webconfig.rs:280-308` | Any web page open on the Mac can POST a settings change (swap the LLM, keys, AppSaid URL) or start OAuth flows; DNS rebinding can read status |
| A7 | Secrets in URL query strings reach error text: Visual Crossing `key=` and Mapbox `access_token=`; reqwest errors include the URL and tools format with `{e:#}` into LLM history and logs | `weather/mod.rs:482`, `directions/mod.rs:283,346`, `llm/rig.rs:2458` | On a 401 or timeout the key is sent to Anthropic/Ollama and written to the log. Places already uses a header |
| A8 | Speaker merge reassigns SQLite memories but never the HelixDB `Turn` / `Memory` nodes; recall filters on `speaker_id` | `control.rs:1043-1048`, `memory/helix.rs:1115-1133` | After a merge, the kept person's GraphRAG recall loses everything the dropped speaker said. Delete leaves orphans |
| A9 | No timeouts after speech capture: STT/TTS connect and reads, the LLM stream, Spotify (bare `Client::new()`), Cadora, web search, model catalog. Fast paths have no barge-in watcher | `wyoming/mod.rs:38`, `wyoming/tts.rs:117`, `music/spotify.rs:161`, `cadora/mod.rs:92` | One stalled service hangs a turn indefinitely and the user cannot interrupt it |
| A10 | Wyoming frame lengths are trusted: `vec![0u8; len]` straight from the peer header, unbounded `read_line`, server bound on `0.0.0.0` | `wyoming/protocol.rs:752-790` | Any LAN host can make the Core allocate gigabytes and crash |
| A11 | Settings persistence truncates the file in place (no temp file + rename); an unreadable file is ignored and then overwritten on the next save. Concurrent `apply_*` calls can lose updates | `settings.rs:935-975` | A crash mid-write wipes every page-entered secret and the household roster |
| A16 | Music is ducked on `Speaking` and restored only on `Finished`; an LLM or TTS error returns early. Duck and restore are separate spawned tasks that can race | `server.rs:119-141`, `orchestrator.rs:484-518` | House music stays quiet after a failed reply |
| A17 | The ambient weather push captures `imperial` once at boot; the voice path reads live units | `main.rs:313-322` vs `orchestrator.rs:2018` | Idle-screen weather and spoken weather disagree after a units change until restart |
| A18 | Calendar caches an empty result when every feed fails | `calendar/mod.rs:76-131` | Offline for a moment means "nothing on your calendar" for 5 minutes |
| A19 | Exhausting `MAX_TOOL_ROUNDS` ends the loop with no final completion and no log | `llm/rig.rs:2413-2472` | Empty or half answer with no explanation |
| A20 | Speaker identify-then-create takes four separate locks; `merge` is an UPDATE then DELETE without a transaction | `speaker/mod.rs:374-404`, `speaker/registry.rs:211-237` | Duplicate anonymous speakers under concurrent turns; half-applied merge on crash |
| A12 | The env → config → persisted overlay is hand-written per field and cannot tell "absent" from "cleared" | `config.rs:2023-2263`, `settings.rs:809-922, 1523-1562` | Spotify `redirect_url` is lost on restart; a key cleared on the page comes back from env |
| A13 | `model_catalog()` is built from env keys only, with its own token provider | `config.rs:1856-1869` | Keys entered on the page never reach the model dropdown |

### Structural

| ID | Issue | Where | Cost |
| --- | --- | --- | --- |
| A4 | `orchestrator.rs` is a god module: `respond_and_speak` is about 435 lines covering memory commands, System-1, recall, prompt, personality, streaming, TTS splitting and barge-in. 12 `too_many_arguments`; `followup_depth` is threaded through 22 call sites | `orchestrator.rs:820-1255` | Hard to change one stage without touching the rest; mixed live-settings and snapshot reads in one turn |
| A5 | Adding a tool touches 5-7 places: guidance text, definition, a `Tools` field, `Tools::new` (9 positional `Option`s), `tools_from_config` (12 args), a dispatch arm, and the settings rebuild sites. Three tool shapes coexist (`PortableTool`, plain structs, free functions) | `llm/rig.rs:57-2137` | The root cause of A1; tests pass `None` nine times |
| A21 | `webconfig.rs` is a hand-rolled 3,149-line HTTP server with routing split between `handle` and `route`, 45 `"200 OK"` literals and 18 copies of the invalid-JSON return | `webconfig.rs:245-639, 1884-2043` | Each new page edits two routers; no place to add auth (A6) once |
| A22 | The same service handles are threaded through four 9-argument signatures and cloned per connection | `server.rs:24-72`, `webconfig.rs:209`, `control.rs:64`, `main.rs:332-413` | Each new service edits four signatures |
| A23 | System-1 intents are bare strings matched in four places; `place_dismiss` is unreachable directly. Fast paths write `WyomingEvent`s directly instead of the `DeviceAction` mapping the tools use | `system1/mod.rs:36-50`, `system1/http.rs:200-232`, `orchestrator.rs:1520-1808` | Two ways to drive the display; the 24 h timer cap and confirmation wording already differ |
| A24 | The `GraphView` trait returns untyped JSON with Helix labels baked in; `SwappableRecall` hardcodes `"helix"` and `embeds_query = true` | `memory/graphview.rs:20-61`, `memory/graphrag_control.rs:65-74` | No alternative graph backend without impersonating Helix JSON |
| A14 | The whole chat log is read and parsed for each follow-up turn (`read_tail` reads from 0) and each ingester tick, with blocking `std::fs` | `memory/chatlog.rs:198-230`, `memory/ingester.rs:76-215` | Turn latency grows with the lifetime of the log |
| A15 | Speaker ID (mel features + ECAPA inference + SQLite + clip write) and SQLite recall run synchronously on tokio workers | `orchestrator.rs:477`, `speaker/mod.rs:367-416`, `memory/backend.rs:94` | Tens of ms of worker stall per turn |
| A25 | Helix recall runs the Turn KNN twice and filters by speaker after top-k; browse and ingest paths scan all nodes and pull every embedding, with N+1 entity lookups | `memory/helix.rs:515-949` | Wasted vector search per turn; a quiet speaker's recall can come back empty |
| A26 | Turn text is stored as `transcript\nreply` and parsed back with `split_once('\n')` in another module | `memory/ingester.rs:228`, `memory/helix.rs:1091-1104` | A transcript containing a newline is misclassified and mis-split |
| A27 | `HomeAddress` / `LiveHomeLocation` live in `directions`; home is geocoded on every directions call and by a different heuristic in weather | `directions/mod.rs:103-237, 333`, `weather/mod.rs:361` | Weather and places depend on directions; extra round trip per directions turn |
| A28 | `weather/mod.rs` (1,580 lines) holds two providers, two parsers, icon mapping, caching, factory and speech rendering; 86 hand-rolled `.and_then(Value::as_*)` chains across integrations | `weather/mod.rs` | Hard to read; fixed-schema APIs could use `Deserialize` structs |

## DRY opportunities

Fifteen duplications. Several have already diverged in behaviour (marked **drifted**), which is where DRY stops being cosmetic.

| ID | Duplication | Where | Consolidation |
| --- | --- | --- | --- |
| D1 | The Wyoming protocol exists in both crates; `read_event` is identical, 58 vs 56 event constants, and doc comments say "byte-identical to the device crate's…" | `wyoming/protocol.rs` (1,669 lines) and `anamanti-display/rust/src/wyoming/protocol.rs` (1,196) | A shared `anamanti-protocol` path-dependency crate. Also absorbs X6 |
| D2 | HTTP clients are built five ways. Six providers call `tuned_builder()` each (separate pools, so keep-alive never pays off; weather builds a new one every push tick). Spotify, AppSaid, catalog, Ollama and entity use bare `Client::new()`; drive consent builds four. `rig_tuned_client` copies the 300/60/30 constants. The `.send().context().error_for_status().json()` chain repeats everywhere | `http.rs`, `weather/mod.rs:293,461`, `places`, `directions`, `calendar`, `recipe`, `music/spotify.rs:161`, `drive_consent.rs` | Everyone uses `shared_client()`; a small `http::get_json(req, timeout, ctx)` that makes the timeout required (fixes A9) and redacts URLs (fixes A7) |
| D3 | Drive and Spotify OAuth consent are about 70% copy-paste (`pkce_pair`, `open_browser`, `wait_for_code`, token parsing). Neither sends or checks OAuth `state` | `drive_consent.rs`, `spotify_consent.rs` | One `oauth_loopback` module; per provider only endpoints and client-auth style |
| D4 | Two `parse_update` functions, documented as "identical semantics". **Drifted:** `search_api_key: ""` clears on the device path and is ignored on the web path; the device path accepts that secret despite claiming not to. Engine parsing and the settings JSON view are also written twice | `webconfig.rs:2075-2145`, `control.rs:749-842` | `SettingsUpdate::from_json(&Value, allow_secrets)` and `SettingsView::to_json()` in `settings.rs` |
| D5 | Tool plumbing in `rig.rs`: six `PortableTool` impls each with an error newtype, identical `definition()` and `invoke()` wrappers; 13 copies of "get sink, send `DeviceAction`, map error"; three identical `close_*` tools; 16 dispatch arms of the same shape | `llm/rig.rs:218-2051` | One crate-local `VoiceTool` trait + `HashMap<&str, Arc<dyn VoiceTool>>`; an `emit()` helper; a `close_tool(name, action, reply)` constructor. Goes with A5 |
| D6 | System-1 timer and weather handlers re-implement `speak_fast_reply`, which already takes an optional frame | `orchestrator.rs:1963-2091` vs `1640-1690` | Call `speak_fast_reply(…, Some(frame))` |
| D7 | Notify and weather push channels have identical connection registries (`Mutex<HashMap<u64, Sender>>`, register, deregister, prune-on-send); `server.rs` branches on `is_weather` three times | `notify.rs:68-140`, `weather/service.rs:34-91`, `server.rs:187-238` | A `ChannelRegistry<Meta>` both wrap |
| D8 | `ChatLog` and `PromptLog` are copy-paste JSONL writers; the millis-counter id scheme appears 3 times and `now_secs` 4 times | `memory/chatlog.rs`, `memory/promptlog.rs`, `speaker/clips.rs:153` | Generic `JsonlLog<T>` plus shared `ids::mint()` / `time::now_secs()` |
| D9 | Vector math and SQLite open. `cosine` is byte-identical in two speaker files; `l2_normalize` is re-implemented inline in memory. **Drifted:** memory and the speaker registry open the same `db_path` but only memory sets `journal_mode=WAL` | `speaker/registry.rs:269`, `speaker/mod.rs:431`, `memory/embed.rs:286`, `memory/mod.rs:109-161` | `crate::vecmath` and one `db::open()` that applies WAL + busy timeout |
| D10 | Provider clients repeated. `ANTHROPIC_VERSION` is defined 3 times; `entity.rs` hand-rolls an Anthropic Messages call; Tavily search has two clients with different timeouts | `memory/entity.rs:47-125`, `llm/anthropic.rs:19`, `llm/catalog.rs:51`, `llm/rig.rs:319`, `recipe/mod.rs:86` | `llm::anthropic::complete_text()`; one `TavilyClient` |
| D11 | Duration speech. **Drifted:** three humanizers handle 0 and hours differently (`rig.rs` says "0 seconds", `recipe` returns `""`); the 24 h timer cap is written twice | `llm/rig.rs:479,605`, `recipe/mod.rs:412`, `orchestrator.rs:1956,2562`, `directions/mod.rs:519` | One `speech::duration(secs, granularity)` and one constant |
| D12 | Provider selection and "advertise only with a key" gating are re-implemented per integration. **Drifted:** the admin page's "effective weather provider" shows Visual Crossing where `weather::from_config` would fall back to Open-Meteo | `weather/mod.rs:918`, `places/mod.rs:380`, `directions/mod.rs:418`, `recipe/mod.rs:165`, `webconfig.rs:907`, `control.rs:532` | `non_empty()` helper; each integration exposes its effective backend once |
| D13 | Calendar has a private TTL cache next to the generic `ToolCache`, whose only user is weather | `calendar/mod.rs:76-131`, `cache.rs` | Route calendar through `tool_cache.calendar_lookup`. Fixes A18 for free |
| D14 | Settings boilerplate: `SettingsView`, `System1View` and `SttView` each built twice; `as_deref().is_some_and(…)` 24 times; the snapshot → unlock → `persist` tail 14 times | `settings.rs` | `view_of()`, `is_set()`, `commit()` helpers |
| D15 | Small leaf duplication: PCM i16 decode and scaling in 4 places; `apiJSON()` redefined in 9 admin pages though `shell.js` has `getJSON` | `orchestrator.rs:2223`, `vad/mod.rs:51`, `vad/silero.rs:127`, `stt/whisper_local.rs:139`; `webconfig/*.html` | `audio::pcm` module; `apiJSON` / `postJSON` in `shell.js` |

The settings tool-rebuild duplication is listed once, as A1, because it is a live bug first.

## Unused and dead code

There is no `#[allow(dead_code)]` anywhere in the Core, so what remains is `pub` items the compiler cannot flag, unread config fields, and comments left over from removed feature gates. Every "no callers" claim was grepped across `src/`, `tests/` and `examples/`.

| ID | Item | Where | Evidence |
| --- | --- | --- | --- |
| X1 | The `aec` module and the `webrtc-audio-processing` dependency (feature `aec`) | `aec/mod.rs`, `lib.rs:15`, `Cargo.toml` | Used only by `examples/aec_spike.rs`; its docs still describe a "Phase 2 live path". Echo cancellation now ships in the device-side HAL shim |
| X2 | `Config::build_llm`, `Config::build_system1` | `config.rs:1585-1606, 1725-1749` | Only doc-comment mentions; both duplicate the live factories and `build_llm` hardcodes the Anthropic URL |
| X3 | System-1 leftovers: config keys `system1.device` and `system1.model`; `DecisionRequest.history` (always empty), `.location` (computed every turn, never read), `.font` (never read); `intent_description` entries for `recipe_nav`, `music`, `shopping_add`, `smalltalk` that no handler serves | `config.rs:617-634`, `orchestrator.rs:879-883`, `system1/http.rs:226-232` | `rg 'system1\.device'` finds nothing; `req.font` appears nowhere |
| X4 | `TurnOutcome::Disconnected` and `Pipeline::run_turn` | `orchestrator.rs:94-97, 318-350` | `run_turn_after_start` always returns `Completed`; `run_turn` is called only by `tests/pipeline.rs`, so the server's `Disconnected` arm is unreachable and tests exercise a different entry point than production |
| X5 | The rig streaming branch and the `Option` on `tools_from_config` | `llm/rig.rs:2137, 2371-2401` | Always `Some` with 4+ tools in production (see A3) |
| X6 | Device-side protocol helpers: `hello`, `hello_weather`, `notify_ack` + `ANAMANTI_NOTIFY_ACK`, `speak`, `decode`, `is_timer` / `is_recipe` / `is_weather` / `is_place` | `wyoming/protocol.rs:448-794` | Called only from tests in the Core; they belong in the shared crate (D1) |
| X7 | Integration leftovers: `SnapcastClient::set_group_muted`, `set_group_stream`; `MpvControl::socket_path`, `set_paused`; `IcalSubscription::new`; `spotify_consent::DEFAULT_CONSENT_PORT`; ignored params on `WeatherService::register` | `music/mod.rs:155-447`, `calendar/mod.rs:90`, `spotify_consent.rs:34`, `weather/service.rs:52` | No callers |
| X8 | Memory and speaker leftovers: `SpeakerService::min_speech_samples` (doc claims the orchestrator uses it), `SpeakerService::embed`, `promptlog::now_secs`, `HelixMemory::dimensions`, and the `mod helix_recall` wrapper kept only for a removed `helix` feature | `speaker/mod.rs:247-252`, `memory/promptlog.rs:130`, `memory/helix.rs:217`, `memory/backend.rs:107-170` | No callers outside the defining file |
| X9 | Over-wide `rig.rs` API: about 20 `pub` items (`*Args`, `*Error`, the search and lookup tool types, `build_search_provider`, `MAX_TOOL_ROUNDS`, tool-name constants) | `llm/rig.rs` | Only `RigBackend`, `tools_from_config` and `Tools` are used outside the file |
| X10 | Stale docs and gate leftovers: `rig.rs` says it is "behind a feature flag"; `settings.rs:1083` keeps `let _ = (engine, web_search, …)`; `LlmEngine`'s `#[default]` is `Native` while config defaults to `Rig`; `vad/energy.rs` calls energy "the committed default"; `stt/mod.rs` says it "ships exactly one implementation" and links a non-existent `with_stt_engine`; `system1/mod.rs` says "M0 scaffold"; three orchestrator doc blocks sit on the wrong function | various | Misleads readers and agents; all small edits |

## Decision table

The 53 issues group into 17 proposals. I recommend fixing rows 1–7 and 16 first: they are mostly small, and together they close every live bug and security gap except A2/A3, which need a product call (row 9). Fill in the Decision column (Fix now / Fix later / Leave) for each row.

| # | Proposal | Covers | Severity | Effort | Recommendation | Decision |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | One `rebuild_factory(&RuntimeSettings)` used by every `apply_*` and by boot; carry `places.enabled` / `provider` on the factory | A1, D14 | High | M | Fix now: live tool loss on ordinary saves | |
| 2 | Reject admin requests whose `Host` / `Origin` is not loopback; require JSON content type on POST | A6 | High | S | Fix now | |
| 3 | Redact URLs from errors before they become tool output or logs (`reqwest::Error::without_url`) | A7 | High | S | Fix now | |
| 4 | A `SpeakerService::merge` / `delete` that updates registry, SQLite, clips and the Helix graph; transactional identify-or-create | A8, A20 | High | M | Fix now | |
| 5 | Small correctness batch: frame-size caps, ducking restore guard, live weather units, don't cache calendar failures, final completion after the tool-round cap | A10, A16, A17, A18, A19 | Med | S each | Fix now, one PR | |
| 6 | Atomic settings write (temp + fsync + rename, keep a `.corrupt` copy) and one mutation lock held through build, swap and persist | A11 | Med | S–M | Fix now | |
| 7 | All integrations on `shared_client()` with a required per-request timeout; timeouts on STT/TTS/LLM streams | A9, D2 | Med | S–M | Fix now | |
| 8 | `VoiceTool` trait and a name-keyed registry; one `emit()` helper; drop `PortableTool` boilerplate; narrow `rig.rs` visibility and split the file | A5, D5, D12, X5, X9 | Med | M–L | Fix later, after row 1 | |
| 9 | Engine capabilities: either teach rig OAuth + OpenAI and retire native backends, or warn loudly when a backend cannot run tools. Stream the first rig round and fall back to `completion()` only on a tool call | A2, A3 | High | M | Needs your call on the native engine | |
| 10 | Generic settings overlay that keeps "cleared"; one `SettingsUpdate::from_json` / `SettingsView`; model catalog reads live keys | A12, A13, D4 | Med | M | Fix later | |
| 11 | Split `orchestrator.rs` (prompt, System-1 handlers, speech out, VAD loop); `TurnCtx`; `AppServices`; an `Intent` enum; fast paths emit `DeviceAction` | A4, A22, A23, D6, X4 | Med | L | Fix later, incrementally | |
| 12 | Shared `anamanti-protocol` crate for Core and Display | D1, X6 | Med | M–L | Leave until the next protocol change | |
| 13 | Memory performance: tail-read the chat log by byte offset, `spawn_blocking` for speaker ID and SQLite, one-pass Helix recall, store transcript and reply separately | A14, A15, A25, A26 | Med | M | Fix later; A15 is S and could go now | |
| 14 | Route table for `webconfig.rs`; one `oauth_loopback` module with `state`; `apiJSON` in `shell.js` | A21, D3, D15 | Med | M–L | Fix later | |
| 15 | Small helpers: `ChannelRegistry`, `JsonlLog<T>`, `vecmath`, `db::open` with WAL, `complete_text`, one Tavily client, one duration speaker, calendar on `ToolCache` | D7–D11, D13 | Low | S each | Fix opportunistically | |
| 16 | Dead-code and stale-doc sweep | X1–X4, X7, X8, X10 | Low | S | Fix now, one PR. Confirm X1 (`aec` removal) first | |
| 17 | Typed `GraphView`; move `HomeAddress` to `household`; split `weather/mod.rs` | A24, A27, A28 | Low | M | Leave | |
