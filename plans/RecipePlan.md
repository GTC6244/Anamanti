# Recipe Mode Plan: Guided Recipes on the Display

**Targets:** M4 Mac Mini (fetch + parse) + Echo Show 8 (the guided cooking screen)
**Feature:** hands-free recipes — *"show me a recipe for carbonara"* → a full recipe
appears on the display with **Overview / Ingredients / Steps** tabs to cook along.

> **Status:** **Implemented (2026-09-24), pending on-device QA.** P1–P4 below are
> built and tested — the `recipe_lookup`/`close_recipe` rig tools + JSON-LD parser
> (Anamanti Core, 9 new unit tests), the `anamanti-recipe` Wyoming frame (round-trip
> tested in both crates), the FRB `ShowRecipe`/`DismissRecipe` events, and the
> 3-tab `RecipeView` + `RecipeController` wiring (6 new Dart tests). Full suites
> green: Anamanti Core 257, device-rust 85, Flutter 82; clippy + `dart analyze`
> clean. **Deferred:** the LLM parse **fallback** (JSON-LD is the shipping parser —
> see §3/§6) and **P5 on-device validation** (real dish → tabs on the Echo Show,
> needs the Mac services + a Tavily key). This plan reads together with
> [`Plan.MD`](./Plan.MD) (the tool-calling / rig engine decisions),
> [`architecture.md`](./architecture.md) (the design, incl. §4 Wyoming frames and
> the proactive-notification channel), and [`MusicPlan.md`](./MusicPlan.md) (the
> precedent for "a skill = a rig LLM tool", and for an Anamanti Core-owned feature
> that pushes state to the device).

Recipe mode is **three coordinated pieces** split along the locked Rust/Flutter
boundary, not one skill:

1. a `recipe_lookup` **rig tool** on the Anamanti Core (fetch + parse to JSON),
2. a new **`anamanti-recipe` Wyoming frame** carrying that structured JSON to the
   device (the notify banner's fixed 4 strings are too narrow), and
3. a new **display "recipe mode"** — a 3-tab screen the device renders.

---

## 0. Confirmed Decisions (2026-09-24)

| Question | Decision |
| --- | --- |
| **What is a "skill" here** | Same answer as Music: **an LLM tool in the rig engine** (`recipe_lookup`, a `PortableTool` in `anamanti-core/src/llm/rig.rs`), alongside `internet_search` / `spotify_control`. Not an Alexa skill, not a gstack dev-skill. |
| **Trigger** | **Voice, by dish name.** *"Show me a recipe for X"* → the tool web-searches (reusing the existing Tavily/DuckDuckGo `SearchProvider`) for a source URL, fetches it, and parses it. No spoken URLs; no phone/config-page push in v1 (left as a future add). |
| **Parsing engine** | **JSON-LD first, LLM fallback.** Try `schema.org/Recipe` JSON-LD (embedded by most recipe sites) for a fast, accurate parse; when it's absent or malformed, hand the page text to the LLM to emit the same structured JSON. "Parse via an agent" is the fallback, not the only path. |
| **Data shape** | One `Recipe` JSON payload: `{ id, title, summary, source_url, image_url?, servings?, total_time?, ingredients: [String], steps: [String] }`. Ingredients/steps are ordered string lists (v1: no per-ingredient quantity struct). |
| **Transport** | A **new `anamanti-recipe` Wyoming frame** (mirrored byte-for-byte in both `protocol.rs` files, `anamanti-timer` as the template). The notify path's `{id,priority,title,body}` banner stays untouched. |
| **Display surface** | A **new top-level "recipe mode"** on the device — a 3-tab view (**Overview / Ingredients / Steps**), bottom tab bar. Built new (there is no `TabBar`/mode-registry today); a `RecipeController extends ChangeNotifier` + immutable `RecipeState`, mirroring `NotificationController`. |
| **Mode lifecycle** | **Voice in; voice or touch out.** Voice opens it and it stays up indefinitely (you cook for 30+ min); dismiss by voice (*"done cooking" / "close the recipe"*) or an on-screen close control. No auto-timeout. |
| **Audio path** | None. Recipe mode is **visual only** in v1 — no step-by-step spoken read-out, no timers auto-created from step durations (both are future adds). The model still speaks a short confirmation ("Here's a carbonara recipe") via the normal Piper path. |

If a task seems to require changing one of these, stop and confirm first.

---

## 1. Topology

```
  "show me a recipe          ┌──────────────── Mac Mini (Anamanti Core) ───────────────┐
   for carbonara" ──STT──▶ LLM recognizes intent ──▶ recipe_lookup tool
                                                          │
                                    SearchProvider (Tavily/DDG) ──▶ source URL
                                                          │ reqwest GET
                                                          ▼
                                    parse: JSON-LD (serde_json) ──miss──▶ LLM parse
                                                          │
                                       Recipe { title, ingredients[], steps[], … }
                                                          │
                          tool returns short spoken confirmation to the model,
                          and emits a DeviceAction::ShowRecipe(Recipe)
                                                          │
                                    Anamanti Core drains action ──▶ WyomingEvent::recipe(...)
                          └───────────────────────────────┬────────────────────────────┘
                                                          │ anamanti-recipe frame (JSON)
                                                          ▼
  Echo Show 8:  Wyoming client decodes ──▶ FRB event ──▶ RecipeController ──▶ recipe mode
                (Overview / Ingredients / Steps tabs)   ── "done cooking" / touch ─▶ dismiss
```

**End-to-end flow:** wake word → STT → LLM intent → `recipe_lookup` (search →
fetch → JSON-LD-or-LLM parse) → the model speaks a one-line confirmation via Piper
**and** the tool pushes an `anamanti-recipe` frame → the device switches into recipe
mode and renders the tabs → the user cooks, then dismisses by voice or touch.

**Decoupling:** the tool (fetch + parse, returns a spoken confirmation) works and is
testable *before* any device UI exists — its unit tests assert the parsed `Recipe`
against fixture HTML. The device frame + tabs can land second.

---

## 2. Components & Ownership

| Piece | Where | Form |
| --- | --- | --- |
| Intent + fetch + parse | Anamanti Core (Mac) | New `recipe_lookup` `PortableTool` in `llm/rig.rs`, over a new injected `RecipeProvider` trait (twin of `DirectionsProvider`) so it unit-tests offline against fixture HTML |
| Web search for a source | Anamanti Core (Mac) | Reuse the existing `SearchProvider` (Tavily/DuckDuckGo) — no new search infra |
| HTML fetch | Anamanti Core (Mac) | Existing `reqwest` (add no HTML-parser dep if JSON-LD via `serde_json` suffices; a lightweight `scraper` is optional) |
| Structured push to device | Anamanti Core (Mac) | `DeviceAction::ShowRecipe(Recipe)` + new `WyomingEvent::recipe(...)` (`anamanti-recipe`), relayed in `drain_device_actions()` |
| Recipe frame decode | Device (Rust) | New arm in `anamanti-display/rust/src/wyoming/client.rs`; surfaced via a new FRB event (sidecar stream, like `NotifyEvent`, or a new `WakeWordEventKind` — see §4) |
| Recipe mode UI | Device (Flutter) | New `RecipeController extends ChangeNotifier` + `RecipeState` (copyWith) + a 3-tab screen; an opacity layer in `AmbientScreen`'s Stack or a `Navigator.push` route |
| Dismiss ("done cooking") | Anamanti Core → Device | A `dismiss` sub-action on the same frame (voice), plus an on-screen close button (touch) |

---

## 3. `recipe_lookup` tool (fetch + parse)

A single tool keeps the LLM surface small (same discipline as `spotify_control`'s
`action` enum). Lives in `anamanti-core/src/llm/rig.rs` (`RecipeLookup`, a real
`PortableTool`) over a new `anamanti-core/src/recipe/mod.rs` module holding the
`RecipeProvider` trait — injected, so it's unit-tested with a fake/fixture, no
network.

- **Args:** `dish: String` (the required search phrase, e.g. "carbonara"),
  optional `url: Option<String>` (if the model already has one, skip search).
- **Resolution:**
  1. If `url` is absent, call `SearchProvider::search(dish, 1)` to get a source URL.
  2. `reqwest` GET the page.
  3. **Parse — JSON-LD first:** extract `<script type="application/ld+json">`,
     find the `schema.org/Recipe` object, map `name`/`recipeIngredient`/
     `recipeInstructions`/`image`/`recipeYield`/`totalTime` into `Recipe`.
  4. **LLM fallback:** if no usable JSON-LD, pass the page's visible text to the
     LLM with a strict "emit this JSON schema" instruction and deserialize the
     result into `Recipe`. (This is the "via an agent" path.)
- **Effect (dual):** the tool **returns a short text confirmation** to the model
  ("Found a carbonara recipe — 4 servings, 25 minutes") so the assistant speaks it,
  **and** pushes `DeviceAction::ShowRecipe(recipe)` onto the turn's `ActionSink`
  (exactly how the timer tools emit `StartTimer`). The Anamanti Core drains that
  action after the reply and writes the `anamanti-recipe` frame.
- **Errors surface as speech:** nothing found, fetch failed, or unparseable become
  the tool result so the model apologizes aloud. The error type implements
  `std::error::Error` (rig requirement), mirroring `SearchError`/`DirectionsError`.
- **Guidance text:** a `tool_guidance` clause (like the search/directions ones)
  advertised only when the tool is enabled, nudging the model to call
  `recipe_lookup` for any "recipe for X / how do I make X" request.

**A dismiss action.** "Done cooking" / "close the recipe" maps to a second, cheap
tool action (or a distinct `recipe_dismiss` tool) that emits
`DeviceAction::DismissRecipe`, relayed as an `anamanti-recipe` frame with
`{ action: "dismiss" }`. The device's on-screen close button handles the touch case
locally (no round-trip).

---

## 4. Wire format & device plumbing

Follow the **`anamanti-timer` template** end to end (it is the existing
LLM-tool-driven, Anamanti Core→device structured push):

**Anamanti Core** (`anamanti-core/`):
- `wyoming/protocol.rs` — `types::ANAMANTI_RECIPE = "anamanti-recipe"`;
  `WyomingEvent::recipe(recipe)` / `::recipe_dismiss()` via `with_data(type, json!(...))`;
  an accessor + a `roundtrip` test.
- `llm/mod.rs` — `DeviceAction::{ShowRecipe(Recipe), DismissRecipe}` variants.
- `orchestrator.rs` — a `match` arm in `drain_device_actions()` mapping the action
  to the frame and `write_event`.

**Device** (`anamanti-display/rust/` + `anamanti-display/lib/`):
- `wyoming/protocol.rs` — the **same** `types::ANAMANTI_RECIPE` constant (the `types`
  block is kept byte-identical; round-trip tests in both crates guard it).
- `wyoming/client.rs` — dispatch the frame; decode helper `event.recipe()`.
- **FRB surface — recommended: a sidecar stream like notifications**, because a
  recipe is *not* tied to a voice turn's lifecycle (it must persist through many
  follow-up turns and idle). Add a `RecipeEvent` FRB struct + a
  `start_recipe_channel` StreamSink mirroring `start_notify_channel`/`NotifyEvent`,
  **or** extend the existing notify sidecar rather than the per-turn
  `WakeWordEvent`. (Do **not** hand-edit `frb_generated.rs`; regenerate via
  `flutter_rust_bridge_codegen generate`.)
  - FRB crosses only flat structs today, so the ingredients/steps **lists** either
    ride as a JSON-encoded string field decoded in the controller, or as a proper
    FRB struct with `Vec<String>` (preferred — FRB v2 supports it; confirm during
    codegen).

**Display UI** (`anamanti-display/lib/`):
- `RecipeController extends ChangeNotifier` (subscribes to the recipe channel via an
  injectable factory typedef, like `NotifyStreamFactory`), holding an immutable
  `RecipeState { Recipe? current; int tab }` with `copyWith`; owned/disposed in
  `_AmbientHomeState`, passed down by constructor.
- A `RecipeView` widget: a bottom bar of 3 buttons (Overview / Ingredients / Steps)
  driving an `IndexedStack` (no `TabBar` exists to reuse; `ValueNotifier<int>` or
  local `setState` for selection) + a close button that calls `controller.dismiss()`.
  - **Overview:** title, image, servings, total time, summary, source link.
  - **Ingredients:** the ordered `ingredients` list (checkable is a nice-to-have).
  - **Steps:** the ordered `steps` list, comfortably large for arm's-length reading.
- Show it as **a new opacity layer in `AmbientScreen`'s Stack** keyed off
  `recipe.current != null` (glanceable, consistent with the app's idiom), taking
  visual precedence over the slideshow while active. Landscape-first, big type
  (8-inch screen, arm's length).

---

## 5. Phases

1. **P1 — `recipe_lookup` tool, parse-only.** `RecipeProvider` trait + `RecipeLookup`
   `PortableTool` + JSON-LD parse + LLM fallback; returns the spoken confirmation.
   Unit-tested against fixture HTML (JSON-LD present *and* absent). No device changes
   yet. Registration in `Tools`, guidance, `tools_from_config`, `LlmFactory`.
2. **P2 — `anamanti-recipe` frame + DeviceAction.** Both `protocol.rs` (byte-identical
   `types`), `DeviceAction::{ShowRecipe,DismissRecipe}`, `drain_device_actions()`
   relay, device `client.rs` decode, round-trip tests both crates.
3. **P3 — FRB recipe channel.** New `RecipeEvent` struct + `start_recipe_channel`
   (or extend notify sidecar); regenerate bindings; `RecipeController` + `RecipeState`.
4. **P4 — Recipe mode UI.** The 3-tab `RecipeView`, wired as an `AmbientScreen`
   opacity layer; voice + touch dismiss.
5. **P5 — On-device validation.** Real dish → source → tabs on the Echo Show
   (release APK); confirm it survives follow-up turns and dismisses cleanly.

---

## 6. Open questions / dependencies

- **Source quality / picking a URL.** Web search returns one result; recipe sites
  vary wildly. Watch whether result #1 parses well; if not, fetch top-N and pick the
  first with valid JSON-LD.
- **JSON-LD coverage.** Most large recipe sites embed `schema.org/Recipe`, but some
  nest it in `@graph` or use arrays / `HowToStep` objects for instructions — the
  parser must handle those shapes before falling back to the LLM.
- **LLM-fallback cost/latency.** The fallback sends page text through the model; cap
  the input size and only trigger it on JSON-LD miss.
- **FRB list marshalling.** Confirm `Vec<String>` crosses cleanly at codegen; if not,
  JSON-string the lists and decode in `RecipeController`.
- **Dismiss vs. barge-in.** Ensure "done cooking" reliably routes to `recipe_dismiss`
  and not a generic reply; tighten guidance if the model narrates instead.
- **Memory budget.** No images cached unbounded on the device; load the single hero
  image at a capped size (respect the ~1 GB RAM boundary).

---

## 7. Non-goals (v1)

- Spoken step-by-step read-out or "next step" voice navigation (visual only for now).
- Auto-creating timers from step durations ("bake 20 min" → a timer) — natural P2.
- Phone/config-page "push this URL to the display" trigger — natural P2.
- Per-ingredient structured quantities/units, scaling servings, unit conversion.
- Saving / a recipe box / history — recipe mode is ephemeral in v1.
- A general HTML-scraping framework — this ships the minimum JSON-LD + LLM parse.

---

## 8. Docs to update on landing

Per the working agreements (keep the three docs consistent):
- `agents.md` — add `RecipePlan.md` to the "feature-specific plans branch off these"
  list, and note the `anamanti-recipe` frame in the boundaries/state section.
- `plans/architecture.md` §4 — document the `anamanti-recipe` frame and the recipe
  device mode.
- `plans/Plan.MD` — add the decision-table rows from §0 here.
- `plans/TODO.md` — track the P1–P5 phases.
