# Places Plan: Place Cards on the Display

**Targets:** M4 Mac Mini (fetch, Anamanti Core) + Echo Show 8 (the place card)
**Feature:** ask about a place — a business, shop, restaurant, cafe, landmark — and a
full-screen card appears with its name, a photo, address, opening hours (with an
open-now chip), rating, phone, website, category, and price level.

> **Status:** **Implemented (2026-09-28), pending on-device QA.** Built and tested: the
> `places_lookup` / `close_places` rig tools + the Google Places (New) provider behind a
> `PlacesProvider` trait (Anamanti Core, new unit tests + a loopback integration test),
> the `anamanti-place` Wyoming frame (round-trip tested in both crates), the
> `ShowPlace`/`DismissPlace` FRB events + `set_place_context`, the `PlaceView` + the
> `PlaceData` parser (new Dart tests), and the **route-only System-1 `place` intent**.
> Suites green: Anamanti Core lib 320 + places tests, device-rust 96, Flutter 107; clippy
> `-D warnings` + `dart analyze` clean; FRB codegen clean. (The one pre-existing pipeline
> failure — `silent_turn_discards_stt_hallucination…` — is unrelated; see
> `system1-fast-decisions.md` §19.)
>
> Reads together with [`WeatherPlan.md`](./WeatherPlan.md) (the near-identical tool +
> frame + widget template this mirrors), [`system1-fast-decisions.md`](./system1-fast-decisions.md)
> (the route-only intent tier, §15.2), and [`architecture.md`](./architecture.md) §4 (frame catalog).

Places is **three coordinated pieces** split along the locked Rust/Flutter boundary,
plus a System-1 routing intent:

1. a `places_lookup` **rig tool** on the Anamanti Core (Google Places search + details),
2. a new **`anamanti-place` Wyoming frame** carrying the report to the device, and
3. a new **display "place card"** — the full-screen `PlaceView`.

Plus a **route-only System-1 `place` intent** so the fast decision engine recognizes a
place question and cleanly defers it to the System-2 LLM (which owns the free-form slot).

---

## 0. Confirmed decisions (2026-09-27)

| Question | Decision |
| --- | --- |
| **Data source** | The **Google Places API (New)** behind a `PlacesProvider` trait (injected, fixture-tested offline), mirroring `WeatherProvider`/`DirectionsProvider`. A **Text Search** (`places:searchText`) resolves the query to candidates; **Place Details** fetches the record. The API key is a secret (`GOOGLE_PLACES_API_KEY`), sent as the `X-Goog-Api-Key` **header** with a `X-Goog-FieldMask` to bound cost. **No keyless fallback** — mirrors `directions::from_token`: no key ⇒ the tool is not advertised. Runtime-settable on the config-page **Tools tab** (persisted 0600, rebuilds the tool live). |
| **Trigger** | **Voice, by place name.** Any "where is / when is X open / tell me about X" → the LLM calls `places_lookup` (biased toward the household `home_location`). |
| **Disambiguation** | **Disambiguate first.** A single match shows the card immediately; multiple comparable candidates make the tool return a spoken candidate list **without** a card, so the model asks which one and re-calls `places_lookup` with the chosen `place_id`. Rides the existing follow-up-listen loop; no new state or protocol. |
| **Photo** | **One hero photo, keyless.** The Core resolves the first photo via the photo-media endpoint with `skipHttpRedirect=true`, yielding a keyless `photoUri` (googleusercontent URL). Only that URL rides the frame; the device fetches it directly (like the recipe hero image), so the APK stays credential-free. A capped size + a graceful fallback icon respect the ~1 GB memory budget. |
| **Transport** | A new `anamanti-place` frame (mirrored byte-for-byte in both `protocol.rs`, `anamanti-weather` as the template) with `action` = `show` / `dismiss`. No ambient push, no persistent channel (unlike weather) — show/dismiss only. |
| **System-1** | A **route-only** `place` intent (`system1-fast-decisions.md` §15.2 Tier C): the classifier recognizes it, but the orchestrator has **no** fast-path handler, so the turn defers to System-2, which calls `places_lookup` with the extracted place name. Added to `default_intents()` + `intent_description`; no handler ⇒ the `_ => None` defer arm. |
| **Display context** | While the card is up, the device stamps `{kind:"place", place:{name,address}}` on the turn's `audio-start` (`set_place_context` FRB → `set_display_context`). The Core parses it (`DisplayContext::Place`/`PlaceScreen`) and appends a prompt line (`place_screen_line`), so "is it open Sunday?" / "close it" route in context. A bare "stop"/"dismiss" with the card up hits the `stop_dismiss` ladder → `place_dismiss`. |

If a task seems to require changing one of these, stop and confirm first.

---

## 1. Google Places API (New) usage

Header auth `X-Goog-Api-Key` + a `X-Goog-FieldMask` on every call.

1. **Text Search** — `POST https://places.googleapis.com/v1/places:searchText`,
   body `{ "textQuery": <query>, "maxResultCount": 5 }`, mask
   `places.id,places.displayName,places.formattedAddress`. The query folds in the home
   location as a "near …" hint when it carries no location of its own (`compose_query`).
2. **Place Details** — `GET .../v1/places/{id}`, mask covering `displayName`,
   `formattedAddress`, `regularOpeningHours`/`currentOpeningHours`, `rating`,
   `userRatingCount`, `nationalPhoneNumber`, `websiteUri`, `googleMapsUri`, `priceLevel`,
   `primaryTypeDisplayName`, `photos`.
3. **Photo** — `GET .../v1/{photo.name}/media?maxWidthPx=800&skipHttpRedirect=true`
   → `{ "photoUri": <keyless URL> }`.

`PlaceReport` stores strings/ints/`Option<bool>` (no `f64`) so it is `Eq` (it rides inside
`DeviceAction`) and JSON-marshals directly to the device.

---

## 2. Components & ownership

| Piece | Where | Form |
| --- | --- | --- |
| Fetch + parse | Core | `anamanti-core/src/places/mod.rs`: `PlacesProvider` trait + `GooglePlaces` (Text Search + Details + photo resolve), `PlaceReport`/`PlaceCandidate`, `from_key` (no key → `None`), `render_confirmation`/`render_candidates`. |
| The tool | Core | `PlacesLookup` / `close_places` in `llm/rig.rs` (over `PlacesConfig{provider, live home_location}`), registered in `Tools`/`dispatch`/`tools_from_config`; guidance clause. |
| Config | Core | `places{enabled,provider}` in `config.rs` + `anamanti.example.json`; the `GOOGLE_PLACES_API_KEY` secret. Runtime-settable via `SharedSettings::apply_places_tool` (persisted) + the config-page Tools tab (`/tools/places/status.json` + `/tools/places/save`, `tools.html`). |
| Frame | both crates | `anamanti-place` + `place_show`/`place_dismiss` constructors + `place_command()` decoder, byte-identical, round-trip tested. |
| Device decode | Device Rust | `TurnUpdate::Place` (`client.rs`) → `WakeWordEvent::{show,dismiss}_place` (`net.rs`). |
| System-1 | Core | route-only `place` intent (`system1/mod.rs::default_intents`, `system1/http.rs::intent_description`); no handler → defers. |
| UI | Flutter | `place_data.dart` (parse) + `PlaceView` (photo + details) + `AssistantState.place`/`placeActive` + `set_place_context` wiring in `main.dart`. |

---

## 3. Non-goals (v1)

- No ambient push / idle-screen presence (unlike weather) — the card is voice-in,
  voice/touch-out, no timeout.
- No multi-result carousel on the display — disambiguation is spoken, the card shows one.
- No directions/booking actions from the card (the `directions_lookup` tool is separate).
- No offline/cached place database — every lookup hits the live API (per-tool cache TTL
  could be added later, like weather).

## 4. On-device validation (pending)

Release APK per `agents.md`; set `GOOGLE_PLACES_API_KEY` + a `home_location`:
- "what are the hours for <cafe>" → spoken confirmation + the card with photo + hours;
  an ambiguous chain name → the assistant asks which one, then shows the chosen branch.
- "close it" and the on-screen close both dismiss.
- With System-1 enabled, confirm the turn classifies `place` and defers to System-2
  (which calls `places_lookup`), and that "is it open Sunday?" while the card is up
  routes in context.

## 5. Docs updated on landing

`agents.md` (secret + feature-plan list + the `places` config block), `architecture.md`
§4 (frame catalog + place device mode), `Plan.MD` (decision-table rows), `TODO.md`.
