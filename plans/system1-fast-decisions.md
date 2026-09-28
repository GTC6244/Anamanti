# system1-fast-decisions.md

A **System-1 fast-decision stage** for the Anamanti Core turn pipeline: a pluggable,
non-autoregressive decision engine that runs *before* memory recall and the LLM, resolves
common intents in a single forward pass, and only **defers** the hard turns to today's
"System-2" path (GraphRAG recall → rig+tools LLM → Piper).

Read with [`agents.md`](../agents.md) (locked decisions), [`architecture.md`](./architecture.md)
(design, §4 state machine / turn flow), and [`Plan.MD`](./Plan.MD) (decision table). Feature
sibling of [`WeatherPlan.md`](./WeatherPlan.md) (the `weather_lookup` tool + `anamanti-weather`
frame this reuses).

> **Status:** implemented through M2 (HTTP `laya-serve`/`jev` backends + **live config-page
> selection & persistence**) plus the M3 timer intent (see §10). Default remains **disabled**
> (`system1.backend = "none"`), so builds are unchanged until opted in (via `anamanti.json` or the
> `/system1` config page). The remaining arg-heavy intents are documented follow-ups. Adding a
> pipeline stage is a structural change — see *Locked-decisions check* below.

---

## 1. Problem — the two latency bottlenecks this targets

Traced from the turn pipeline (`anamanti-core/src/orchestrator.rs`):

- **Bottleneck #1 — the default `rig` engine defeats token streaming.** Tools are seeded on
  every turn (`src/llm/rig.rs:1424`: timers always; weather/web-search enabled by default), and
  with *any* tool present, `respond` takes the **non-streaming** branch
  (`src/llm/rig.rs:1840`): it runs a full blocking `model.completion()` and yields the entire
  reply at once, looping up to `MAX_TOOL_ROUNDS = 4` (`src/llm/rig.rs:49`). Net effect:
  **time-to-first-audio ≈ full LLM generation time**, not first-token latency.
- **Bottleneck #3 — memory recall blocks before the LLM even starts.** `respond_and_speak`
  awaits `build_context` (`src/orchestrator.rs:642`) → `recall.recall(...)`
  (`src/orchestrator.rs:955`) → `HelixRecall::recall`, whose first step is a **network OpenAI
  embedding round-trip** (`src/memory/backend.rs:97`, `text-embedding-3-small`) before any vector
  search. A round-trip to OpenAI sits on the critical path of *every* turn.

For a huge share of real utterances ("show me the weather", "set a 10-minute timer", "next
step", "add milk to the list") neither cost buys anything: the answer is a known action, not a
memory-grounded generation. System-1 collapses those turns.

**What this does *not* fix** (out of scope here; tracked separately): the ~700 ms VAD end-silence
hangover, Whisper non-streaming transcription, and cold-start warmth — all of which happen
*before* a transcript exists, where a decision engine cannot help.

---

## 2. Goals

1. **Cut perceived latency on routine turns** by skipping both bottleneck #1 and #3 when a fast,
   high-confidence decision is available — ideally emitting the visual widget the instant the
   intent resolves, before speech.
2. **Make the decision engine pluggable and selectable, exactly like the LLM backend** — a trait
   with a config-chosen implementation (local vs cloud), runtime-swappable from the config page,
   defaulting to off.
3. **Support both engines the user runs**: the in-process **Laya-Decision** Rust crate
   (candle/Metal, no server) and **Jev** via OpenRouter (`typesafe/jev-1.13`) — which share one
   wire contract (`/v1/systemone`), so a single local `laya-serve` sidecar is a third, free
   option.
4. **Reuse existing machinery**: resolved intents drive the *existing* tools and
   `DeviceAction`s (e.g. `weather_lookup` → `DeviceAction::ShowWeather`,
   `src/llm/mod.rs:52`) and the *existing* action-relay loop (`src/orchestrator.rs:689`). No new
   device protocol.
5. **Preserve conversational coherence**: resolved turns still write the chat log, run inferred
   memory capture, and emit the follow-up `listen` window.
6. **Zero-regression opt-in**: `system1.backend = "none"` (default) reproduces today's behavior
   byte-for-byte; the whole feature is gated.

## 3. Non-goals

- Not replacing the LLM. System-1 **routes**; System-2 still answers everything it can't.
- Not a slot/entity extractor. Jev/Laya return **typed** answers (choice/noul/score), not
  free-form text — so open-ended arguments (an arbitrary city, a long freeform request) cause a
  **Defer**, not a guess.
- Not on the device. It runs on the Mac Core (respects the "Rust owns real-time on the 1 GB
  device" boundary; the Core is where STT/LLM/TTS already live).
- No custom model training in v1 (mirrors the wake-word "no custom training in v1" decision).
- Does not touch the VAD/STT/warmth latencies (separate work).

## 4. Outcomes & success metrics

- **Resolve path latency:** for a resolved intent, transcript→(widget + first audio) is bounded
  by one System-1 forward pass + one tool call + first-sentence Piper — **no** embedding
  round-trip, **no** rig completion. Target: local Laya decision < ~50 ms on M4 Metal (single
  non-autoregressive forward pass; to be measured).
- **Coverage:** ≥ N% of turns resolved by System-1 for the seeded intent set (weather, timers,
  recipe nav, music transport, shopping-list add, simple smalltalk), measured from the chat log.
- **Precision:** mis-resolve rate below a strict bar (mis-resolves are worse than a slow-correct
  answer). Gated by a calibrated confidence threshold; ambiguous → Defer.
- **No regression:** with `none`, all existing `cargo test`/pipeline integration tests pass
  unchanged.
- **Selectable like models:** `system1.backend` switches between `none | laya-embedded |
  http (laya-serve | jev-openrouter) | mock` at boot and from the config page, persisted to
  settings, same lifecycle as `llm.backend`.

---

## 5. Background — Jev and Laya-Decision (what they are, one contract)

Both are "System-1" decision models: given a **state** and a set of **typed questions**, they
return **typed, probabilistic** answers in a single shot — no generation, nothing to parse, no
reasoning trace.

**Question types** (identical across both):
- `choice` — pick one of N labels; returns the label + per-label probabilities.
- `noul` — boolean; returns `P(true)`.
- `score` — ordinal `0..N-1`; returns the expected value.
- Every answer carries `answer_confidence` (calibrated `max(p)` across all types) → **gate on one
  threshold**.

**Wire contract** (shared — Laya is explicitly "Jev-compatible"):
```
POST /v1/systemone
{
  "state":     { "message": "show me the weather" },
  "questions": { "intent": { "type": "choice", "instructions": "...", "criteria": { ... } } }
}
→ { "model": "...", "answers": { "intent": { "label": "...", "probabilities": {...},
     "answer_confidence": 0.97 } }, "usage": {...}, "routing": {...} }
```

**Jev (cloud):** `typesafe/jev-1.13` on OpenRouter's Decisions API. Needs an OpenRouter API key;
billed on **input tokens only** (output free). A network hop — fine as a fallback / no-GPU
option, but not the low-latency default for a home device.

**Laya-Decision (the user's repo, `GTC6244/Laya-Decision`, Apache-2.0):** a pure-Rust,
non-autoregressive port (candle; ModernBERT-large EN / mmBERT multilingual), matches upstream
PyTorch to `1e-4`. Three integration modes:
1. **In-process library** — crate `laya-decision` (imported as `laya`):
   ```rust
   use laya::agent::{Agent, LoadOptions};
   use laya::{triage_questions, State};
   let agent = Agent::load("convaiinnovations/laya",
                           LoadOptions { device: Some("metal".into()), ..Default::default() })?;
   let result = agent.system_one(&state, &questions)?; // SystemOneResult { answers, .. }
   ```
   or the auto-checkpoint `Router::with_defaults()?.predict(&state, &questions, &hints)`.
   Runs on **Apple-Silicon Metal (~4× vs CPU on an M4)** via the crate's `metal` feature;
   downloads the checkpoint from HF on first use. **No server, no Docker** — same ethos as the
   embedded HelixDB store.
2. **`laya-serve`** — a local HTTP sidecar exposing `POST /v1/systemone` + `GET /health`, env
   config (`LAYA_HOST/PORT/DEVICE/PRELOAD/MODELS/API_KEY/...`).
3. Since (2) is Jev-wire-compatible, **one HTTP client covers both** `laya-serve` and OpenRouter
   Jev by swapping base URL + auth + model id.

**Design consequence:** this is the same "local-or-cloud behind one trait" split you already have
for the LLM (ollama vs Claude/OpenAI). The shipped default (decided 2026-09-25) is the local
**`laya-serve` sidecar** — local + low-latency, keeps candle out of the Core binary, and the same
HTTP client points at OpenRouter Jev as the cloud fallback. In-process Laya-on-Metal is an
optional lower-latency escape hatch.

---

## 6. Architecture

### 6.1 The trait (mirrors `LlmBackend`)

New module `anamanti-core/src/system1/` with a trait shaped like `LlmBackend`
(`src/llm/mod.rs:135`) and held in `RuntimeSettings` next to `llm` (`src/settings.rs:694`):

```rust
// system1/mod.rs
#[async_trait]
pub trait DecisionEngine: Send + Sync {
    /// Short label for logs/settings, e.g. "laya-embedded", "jev", "none".
    fn name(&self) -> &str;

    /// Score a fixed question set over the turn state in one shot.
    /// Thin transport over the shared `/v1/systemone` contract — engines stay dumb;
    /// the routing POLICY (questions, thresholds, intent→handler) lives in the orchestrator.
    async fn decide(&self, state: &DecisionState, questions: &QuestionSet) -> Result<Answers>;
}

pub struct DecisionState {           // → the `state` object
    pub message: String,             // the transcript
    pub screen: Option<String>,      // display context label (so "next step" routes vs recipe)
    pub history: Vec<(String, String)>, // recent turns, for follow-up disambiguation
}
pub type QuestionSet = /* id → { type, instructions, criteria } */;
pub struct Answers   { /* id → { kind, label/p_true/expected, answer_confidence } */ }
```

Implementations:
- `system1/http.rs` — one `/v1/systemone` reqwest client; config picks base URL + auth +
  model id. Serves **both** a local **`laya-serve`** (`http://127.0.0.1:8000`,
  keyless/`LAYA_API_KEY`) and **Jev on OpenRouter** (`typesafe/jev-1.13`, `OPENROUTER_API_KEY`).
  Reuses the existing reqwest/secret-env plumbing. **`laya-serve` is the recommended enabled
  backend** (decided 2026-09-25): keeps candle out of the Core binary, still local + low-latency,
  and the same client trivially points at OpenRouter Jev as the no-GPU/cloud fallback.
- `system1/laya_embedded.rs` — wraps the `laya` crate's `Agent`/`Router` in-process (Metal).
  Loaded once at boot, shared behind `Arc` (like the LLM backends). Optional; lowest latency but
  compiles candle into the Core binary — kept as an escape hatch, not the default.
- `system1/mock.rs` — deterministic fixtures for tests (same role as `llm/mock.rs`).
- `NoDecision` (`"none"`) — always returns "defer"; the **safe merge default**, so the feature is
  a no-op until explicitly enabled.

### 6.2 Where it plugs in (the exact seam)

In `respond_and_speak` (`src/orchestrator.rs:586`), immediately **after** the existing
`parse_command` remember/forget fast-path (`:605`, which already proves the "resolve-and-return
before the LLM" shape) and **before** `build_context` (`:642`):

```
transcript
  → parse_command                       (existing remember/forget short-circuit, :605)
  → system1.decide(state, ROUTING_Qs)   (NEW; only if system1.backend != none)
       ├─ Resolve(intent) if answer_confidence ≥ system1.min_confidence
       │     run intent handler → reuse existing tool + DeviceAction (:52, :689),
       │     speak a short templated line, write chatlog + infer memories + emit
       │     follow-up `listen`, then RETURN.               ← skips #1 AND #3
       └─ Defer                                              ← unchanged: build_context (:642)
                                                               → rig+tools LLM (:704)
```

### 6.3 The routing question set (policy, provider-agnostic)

System-1 is a **router**, so we ask a small fixed `QuestionSet` and interpret typed answers.
Sketch:

```json
{
  "intent": { "type": "choice",
    "instructions": "What does the user want in `message`?",
    "criteria": {
      "weather":       "current conditions or forecast",
      "timer":         "start/cancel a timer or alarm",
      "recipe_nav":    "navigate/scroll the recipe already on screen",
      "music":         "play/pause/next/previous/volume",
      "shopping_add":  "add an item to the shopping list",
      "smalltalk":     "greeting/thanks/acknowledgement, no data needed",
      "other":         "anything that needs reasoning, memory, or open-ended understanding"
    }},
  "needs_full_understanding": { "type": "noul",
    "instructions": "Does answering require open-ended reasoning, personal memory, or details not implied by a simple command?" }
}
```

**Decision rule (in the orchestrator):**
- `Resolve(intent)` iff `intent != other`, `needs_full_understanding` is false, **and**
  `answer_confidence ≥ min_confidence` for both questions.
- Otherwise `Defer`.
- Open-ended slots (e.g. weather for a *non-home* city) are exactly what `needs_full_understanding`
  catches → Defer. Home-location weather ("show me the weather") resolves.

Each `intent` maps to a small handler that reuses existing code — e.g. `weather` →
`weather_lookup(home_location)` → `DeviceAction::ShowWeather` + a templated spoken summary; `timer`
→ parse duration deterministically (or Defer if unparseable) → `DeviceAction::StartTimer`;
`recipe_nav` → `DeviceAction::RecipeControl`; etc.

### 6.4 Worked example — "show me the weather"

1. Transcript lands; `parse_command` → no match.
2. `system1.decide` → `intent = weather (p≈0.97)`, `needs_full_understanding = false (p_true≈0.02)`.
   Both clear `min_confidence` → **Resolve(weather)**.
3. Handler calls the existing `weather_lookup` for `home_location`, emits
   `DeviceAction::ShowWeather(report)` on the existing action sink → the `anamanti-weather` frame
   opens the widget **immediately** (before speech).
4. Speak a short templated line ("Here's your forecast — 18 and clear."); write chatlog; emit the
   follow-up `listen` window.
5. **Skipped:** the OpenAI embedding recall (#3) and the full rig+tools completion (#1).

---

## 7. Config & selection (identical lifecycle to the LLM backend)

Mirror the LLM config surface: `LlmChoice` (`src/config.rs:110`), `build_llm()`
(`src/config.rs:1142`), and the settings/config-page persistence.

New JSON block in `anamanti.json` (all optional, defaults shown; add to `anamanti.example.json`):
```jsonc
"system1": {
  "backend": "none",              // none (default) | laya-serve | jev | laya-embedded
  "base_url": "http://127.0.0.1:8000",  // http backends: laya-serve (default host) or OpenRouter
  "openrouter_model": "typesafe/jev-1.13",  // jev backend only
  "device":  "metal",             // laya-embedded only: cpu | metal
  "model":   "convaiinnovations/laya",  // laya-embedded only: checkpoint / hub id
  "min_confidence": 0.85,         // strict; precision over recall
  "intents": ["weather","timer","recipe_nav","music","shopping_add","smalltalk"]
}
```

- New factory `build_system1() -> Result<Arc<dyn DecisionEngine>>` beside `build_llm()`; result
  stored on `RuntimeSettings` next to `llm`; **runtime-swappable + config-page selectable**,
  persisted to `anamanti_settings.json` (a persisted value wins over the JSON seed, same rule as
  the LLM fields).
- **Secret:** `OPENROUTER_API_KEY` — a new env-var secret for the Jev backend, following the
  existing pattern (boot seed + masked config-page entry, persisted to the `0600` settings file;
  never in either JSON file). Add it to the secrets list in `agents.md`.
- **Default `none`** ⇒ no behavior change on merge.

---

## 8. Coherence, safety, precision

- **Precision gate:** strict `min_confidence`; anything ambiguous defers. This is the primary
  safety knob. Start conservative, loosen with measured data.
- **Coherence:** resolved turns must still (a) append to `anamanti_chatlog.jsonl` for the GraphRAG
  ingester, (b) run `infer_memories` capture (cheap, local), and (c) emit the follow-up `listen`
  window — or conversation state drifts.
- **Barge-in:** resolved replies are **not** wake-word-interruptible in v1 (decided
  2026-09-25) — the templated line is a fraction of a second, so the interruptible window is
  negligible and not worth the complexity. This matches the existing canned remember/forget reply,
  which is also not interruptible. (Wake-word barge-in still works normally on the System-2 path.)
- **Failure = defer:** any `decide` error, timeout, or low confidence falls through to System-2.
  The engine never blocks a turn; a small per-decision timeout guards the HTTP backends.

## 9. Latency budget

- **laya-serve (default):** local HTTP round-trip to a sidecar doing one non-autoregressive
  forward pass (CPU or Metal via `LAYA_DEVICE`). Loopback + a single forward pass; measure, but
  expected to be small relative to what it replaces — an OpenAI embedding round-trip **plus** a
  full rig completion of up to 4 tool rounds. Keeps candle out of the Core binary.
- **laya-embedded (optional):** in-process forward pass on Metal (M4), no network, no extra
  process — the lowest-latency option; target < ~50 ms (measure). Escape hatch when the extra
  process isn't wanted, at the cost of compiling candle into the Core.
- **jev (OpenRouter):** one internet round-trip, input-token billed. Still far cheaper than the
  full System-2 path, but a WAN hop — the no-GPU / cloud fallback, not the home default.
- **Speculative option (later):** run `decide` in parallel with STT finalization / warm-up so its
  latency overlaps dead time.

## 10. Phasing & status

1. **M0 — scaffold, no-op. ✅ DONE.** Trait + `system1/` module + `NoDecision` default + config
   plumbing + `mock`; seam in `respond_and_speak` behind `backend != none`. 278 lib tests green,
   clippy clean, zero behavior change.
2. **M1 — `laya-serve` HTTP backend + weather end-to-end. ✅ DONE.** `system1/http.rs`
   (`/v1/systemone` client, pure `build_request`/`interpret` unit-tested) covers `laya-serve`
   (default) and `jev`; routing question set (`intent` choice + `needs_full_understanding` noul)
   under `min_confidence`; the `weather` handler fetches Open-Meteo, emits `DeviceAction::ShowWeather`
   **before** speech, speaks a templated summary, and invites a follow-up — skipping recall + the
   LLM. Integration test drives it with a **panicking LLM** to prove System-2 is skipped.
3. **M2 — Jev/OpenRouter backend + config-page integration. ✅ DONE.** The same HTTP client serves
   Jev via `base_url` (OpenRouter API root) + `openrouter_model` + `OPENROUTER_API_KEY`. System-1
   is now **runtime-swappable and persisted** like the LLM backend: the live selection lives in
   `RuntimeSettings.system1` (a `System1Runtime` bundle), rebuilt via `SharedSettings::apply_system1`
   (mirroring `apply_drive`/`apply_household`, so it never touches the LLM `apply()`), persisted in
   `anamanti_settings.json`, and seeded at boot in `shared_settings` (config seed → persisted
   overlay, with the OpenRouter key from env). A dedicated **config page** (`/system1`, nav
   "System-1") picks the backend/base-URL/model/min-confidence/key live via
   `/system1/status.json` + `/system1/save`. `laya-embedded` (in-process candle) remains the
   optional escape hatch and bails at boot.
4. **M3 — expand intents. ✅ weather + timer DONE; others DEFERRED with rationale.** Added the
   `timer` intent: a conservative, unit-tested duration parser (`system1::parse_duration_secs`)
   resolves a clear "set a timer for N …" to `DeviceAction::StartTimer` + a spoken confirmation, and
   **defers** cancels / free-form / out-of-range to System-2's timer tool. **Key finding that bounds
   the rest:** Jev/Laya are *typed classifiers* — they return an intent label, **not** free-form
   arguments. So intents whose arguments are **defaulted or absent** are System-1-eligible (weather →
   home location; timer → parsed number+unit), but intents needing free-form slot extraction
   (`shopping_add` → item text) or open sub-intent disambiguation + live device/screen state
   (`recipe_nav`, `music`) are **not** a good fit for a single classifier pass and are left to
   System-2. `smalltalk` is deferred too (the label alone can't tell "thanks" from "hello"; a canned
   reply would feel wrong). These could be revisited with additional typed sub-questions (e.g. a
   `music_action` choice) **plus** on-device QA — enumerated as follow-ups, not shipped blind.
5. **M4 — tuning + docs. ✅ Knobs in place; docs updated.** `min_confidence` and the intent list are
   config-tunable; per-intent thresholds and resolve-rate/precision telemetry remain open (§13).
6. **M5 — intent-set expansion (planned, decided 2026-09-27; §15–§18).** Adds the temporal question,
   device-context disambiguation, and the expanded Resolve set.
   - **M5.0 — Pre-req P0: device → Core context extension (§19). ✅ LANDED 2026-09-27.** Reports
     background timer state (running/remaining/labels) on every turn and threads the existing widget
     context + timers into `DecisionRequest`. Hard-gates `timer_query`, `timer_cancel`, and the
     `stop_dismiss` ladder.
   - **M5.1 ✅ LANDED 2026-09-27** — temporal question + resolve-by-tense matrix (§16): the
     `HttpDecider` now asks a 3-way `temporal` choice and **defers on a confident `past`** (no fast
     history path). Confidence-gated so an unsure temporal never over-defers a present query;
     present/future resolve (weather's widget already carries current + forecast, so no handler
     change). Pure `build_request`/`interpret` unit tests cover it.
   - **M5.2 ✅ LANDED 2026-09-27** — the no-slot intents + the `stop_dismiss` ladder. Added handlers
     for `time`/`date` (wall clock), `timer_cancel`/`timer_query` (gated on a running timer from the
     P0 context), `weather_dismiss`/`recipe_dismiss` (gated on that screen being foreground),
     `end_session` (brief ack + **no `listen` frame** → device returns to IDLE), and `stop_dismiss`
     (the deterministic priority ladder over device context, with the **ground-truth veto** → defer
     when nothing is active). A shared `speak_fast_reply` helper centralizes the commit-and-close
     tail. Pipeline tests drive each with a panicking LLM (System-2 skipped) or assert the veto
     defers. **Scoped out to M5.3 (needs a live model to calibrate):** the 3B "classifier sees the
     device state" enhancement (§17.4) — the ladder already owns the referent deterministically, so
     the classifier stays state-blind for now; folding the device summary into the request `state`
     lands with calibration. Ladder rungs 1 (alarm ringing) & 3 (media) remain parked (§19.7).
   - **M5.3 ◑ IN PROGRESS 2026-09-27** — **3B state-fold ✅ landed**: `build_request` folds a compact
     device summary (`screen` label + `timers_running`/`timer_remaining_secs`) into the request
     `state`, but **only when present**, so ordinary turns stay byte-identical (`state = {message}`).
     The classifier now *sees* device state (sharpening `stop_dismiss`/dismiss confidence) while the
     ladder still owns the referent. **Calibration harness ✅ landed**: `tests/system1_calibration.rs`
     — an `#[ignore]`d tool seeded with the §18 corpus that measures resolve precision/recall against
     a live backend (laya-serve or Jev) and reports where `min_confidence` should sit.
   - **M5.3 calibration RUN 2026-09-27 (live jev/OpenRouter):** the §18 corpus scores **22/22 at
     `min_confidence=0.85`** — 0 mis-resolves, 0 wrong-intent, 0 missed resolves. So a **single global
     0.85 threshold is sufficient; per-intent thresholds are NOT needed** (§13 Q4 answered). The run
     surfaced + fixed one real bug: `time`/`date` now defer on a named place (added to
     `PLACE_SENSITIVE_INTENTS`), so "what time is it in London" no longer answers the local time.
     **Remaining:** on-device QA on the Echo Show (a test Core is running from this worktree on :10702
     with the jev backend for exactly that).

## 11. Testing

- Mirror the LLM/TTS test style (in-memory fixtures, the duplex-pipe pattern used for the Wyoming
  clients).
- `mock` engine returns scripted `Answers` → assert Resolve vs Defer routing and that resolved
  turns still log/infer/emit-follow-up.
- Golden decisions for the seeded intents; a precision fixture set (utterances that **must**
  defer).
- An `--ignored` integration test against a running `laya-serve` / real checkpoint (like the
  existing model-dependent tests), off by default.

## 12. Locked-decisions check (read before coding)

`agents.md` locks the pipeline shape and says *"If a task seems to require changing one of these,
stop and confirm first"* and *"Do not add a second … interop mechanism … without confirmation"*,
plus *"record it in `Plan.MD` (decision table) and reflect structural changes in
`architecture.md`."* This feature:
- **Adds a pipeline stage** (System-1 before recall/LLM) — a structural change → needs a Plan.MD
  decision-table entry and explicit sign-off before implementation.
- Is **consistent with** the locked "pluggable behind a trait" decision (it copies that pattern)
  and does not add a second *audio* path or an IP-discovery fallback.
- Introduces a **new secret** (`OPENROUTER_API_KEY`) and a **new dependency** (`laya-decision`
  crate for the embedded backend) — both to be recorded.

Update on landing: `Plan.MD` (decision table + phase), `architecture.md` §4 (turn flow: the new
Resolve/Defer branch), `agents.md` (secrets list + the `system1` config block), and
`anamanti.example.json`.

## 13. Decisions made & open questions

**Decided (2026-09-25):**
- **Default backend = `laya-serve`** (local HTTP sidecar), with `jev`/OpenRouter as the cloud
  fallback and `laya-embedded` as an optional escape hatch. (Safe merge default stays `none`.)
- **Resolved replies are not barge-in-interruptible in v1** — the templated line is sub-second, so
  the interruptible window is negligible; matches the existing canned-reply behavior.

**Decided (2026-09-27) — intent-set expansion (see §15–§18; not yet implemented):**
- **Expand the Resolve set** beyond weather+timer to the no-slot / deterministically-parseable
  intents in §15 (`timer_cancel`, `timer_query`, `time`, `date`, `weather_dismiss`,
  `recipe_dismiss`, `end_session`, `stop_dismiss`). Free-form-slot intents (`internet_search`,
  `directions_lookup`, `recipe_lookup`, `shopping_add`) stay **route-only** (classify, never
  resolve).
- **`timer_query` is in scope** ("how much time is left?"), resolved **when the device reports an
  active timer** — answered straight from the timer context parameters (§17.1), so the Core needs
  no timer state of its own.
- **Add a 3-way `temporal` question** (past / present / future) to the one-shot set. Used as a
  **defer gate** (`past` → defer for every data intent — no fast historical fetch) and a **data
  selector** (weather `present`=current, `future`=forecast). See §16.
- **`end_session` intent** — an explicit "I'm done" that ends the follow-up chain by **skipping the
  `listen` frame** (sending `audio_stop` alone), returning the device to IDLE / wake-word-waiting.
- **Device-context disambiguation (3B hybrid + ground-truth veto).** Bare/overloaded commands
  ("stop", "never mind", "dismiss") map to a generic `stop_dismiss` label; the **orchestrator**
  resolves the referent from live device state via a deterministic **priority ladder**. The
  classifier *sees* the state (to sharpen confidence on vague phrasing) but never chooses the
  referent, and a `stop_dismiss` with no valid referent **defers** (ground-truth veto). See §17.
- **Device-context extension is committed (in scope, not merely a prerequisite).** `DisplayContext`
  today reports only the **foreground widget** (`recipe`/`weather`), so a timer running **behind**
  another screen is invisible. The device must report **active/ringing timers as context parameters
  on every turn regardless of the foreground widget** (plus soonest-remaining time for
  `timer_query`, and media state later). This is a device+protocol change that unblocks
  `timer_query`, `timer_cancel`, and the `stop_dismiss` ladder (§17.1).

**Open:**
1. **Checkpoint choice / size** for the home intent set (English vs multilingual; the
   `typed-decisions` checkpoint) and its footprint in the `laya-serve` process.
2. **`laya-serve` supervision** — do we auto-start/supervise the sidecar at boot (like the music
   snapserver/librespot supervision), or assume it's run separately?
3. **Timer/duration parsing** — deterministic parser on the Resolve path, or defer any non-trivial
   duration to System-2?
4. **Confidence calibration** — is one global `min_confidence` enough, or per-intent thresholds?
5. **Telemetry** — reuse `anamanti_promptlog`/chatlog, or a dedicated decision log for
   resolve-rate/precision tuning?

## 14. Testing & QA notes

### 14.1 Automated coverage (already green)
Run from the repo root (the `rust-toolchain.toml` selects rustc 1.92 automatically; set
`CARGO_TARGET_DIR` to a writable dir if the external build drive isn't mounted):

```bash
cargo clippy --manifest-path anamanti-core/Cargo.toml --all-targets -- -D warnings
cargo test  --manifest-path anamanti-core/Cargo.toml
```

What's covered:
- **Wire codec** (`src/system1/http.rs`): `build_request` shape (state + intent choice + `other` +
  `needs_full_understanding` noul; model omitted for laya-serve) and `interpret` — resolve on
  confident closed intent, defer on low confidence / needs-full / `other` / unknown / malformed.
- **Duration parser** (`src/system1/mod.rs::parse_duration_secs`): digits + number words, `a`/`an`,
  `half`/`quarter`, compound ("1 hour 30 minutes"); defers on no-unit / cancel / empty.
- **NoDecision + MockDecider** behaviour.
- **Pipeline fast paths** (`tests/pipeline.rs`): weather and timer each drive a full turn with a
  **panicking LLM** — proving System-2 is never called on a resolve — and assert the
  `anamanti-weather` / `anamanti-timer` frame + templated reply + final `audio-stop`.
- **Settings swap** (`src/settings.rs::apply_system1_*`): `none → laya-serve → none`, unknown
  backend rejected leaves the engine untouched, `system1_view` reports the choice.
- **Config page** (`src/webconfig.rs`): `/system1` renders with nav; `/system1/status.json` reports
  the default disabled engine.

### 14.2 Manual QA — NOT yet verified (do these before shipping)
These need a real sidecar / device and could not be exercised unattended:

1. **`laya-serve` end-to-end.** Install + run the sidecar, then point the Core at it:
   ```bash
   cargo install laya-decision-serve
   LAYA_PORT=8000 LAYA_DEVICE=metal LAYA_PRELOAD=1 laya-serve
   # sanity-check the contract directly:
   curl -s localhost:8000/v1/systemone -H 'content-type: application/json' -d '{
     "state":{"message":"show me the weather"},
     "questions":{"intent":{"type":"choice","instructions":"...","criteria":{"weather":"forecast","other":"else"}},
                  "needs_full_understanding":{"type":"noul","instructions":"..."}}}'
   ```
   Set `system1.backend="laya-serve"` in `anamanti.json` (or via the config page), set a home
   location, then say **"what's the weather"** and **"set a timer for ten minutes"**. Confirm the
   log shows `system1 (laya-serve) resolved intent ...` and the widget/timer appears.
   **Verify the response JSON keys actually match** what `interpret` reads
   (`answers.intent.choice`, `answers.<q>.answer_confidence`, `answers.needs_full_understanding.noul`)
   against your checkpoint — the parser was written from the README/source, not a live server.

2. **Config page (`http://<config_addr>/system1`).** Switch backend none↔laya-serve↔jev; confirm
   min-confidence/base-URL/model persist across a Core restart (written to
   `anamanti_settings.json`), the OpenRouter key field never echoes back, and an invalid backend
   surfaces the error inline without changing the live engine. Confirm the model/key rows show only
   for `jev`.

3. **Jev / OpenRouter. ✅ VERIFIED LIVE 2026-09-26.** Set `system1.backend="jev"`,
   `system1.base_url="https://openrouter.ai/api"`, `OPENROUTER_API_KEY=...`. The endpoint is the
   shared **System One** API: `POST https://openrouter.ai/api/v1/systemone` with `model:
   "typesafe/jev-1.13"` + bearer auth — confirmed against the live server and OpenRouter's OpenAPI
   spec (it is NOT the OpenAI chat-completions route). Two corrections landed from this verification:
   - **Response fields.** A `choice` answer carries `choice` + **`confidence`** + `probabilities`; a
     `noul` answer carries **only `noul`** (0..1), no confidence field. The parser previously read a
     nonexistent `answer_confidence`, so every turn scored 0.0 and silently deferred. Fixed in
     `system1/http.rs` (`choice_confidence` still falls back to `answer_confidence` for a laya-serve
     build that emits the older field).
   - **Weather needed a place.** Bare "what's the weather" scores `needs_full_understanding` noul
     ~0.94 (defer); "what's the weather **in <place>**" scores ~0.30 (resolve). Putting the location
     in a `state` field alone does **not** help (~0.87) — it must be in the question text. So the
     HTTP engine now retries once: a confident but deferred [`LOCATION_INTENTS`] intent (weather) is
     re-asked with the home location folded into the transcript. Two fast System One calls still beat
     a System-2 turn. Home location comes from `DecisionRequest.location` (`LiveHomeLocation`).

   Watch for a 4xx in the Core log; any error defers to System-2, so a misconfig degrades gracefully.

4. **On-device (Echo Show).** Confirm: the weather widget opens **before** the spoken line; the
   timer starts and counts down; the follow-up mic reopens after the reply (the fast path shares
   `emit_follow_up_and_stop`); and there is **no double-speak** if a handler's precondition fails
   mid-turn (e.g. weather with no home location should defer *before* anything is emitted).

5. **Deferral / precision spot-checks.** With a backend enabled, confirm these still go to
   System-2 (no wrong fast answer): "what's the weather in Tokyo" (non-home → `needs_full`),
   "cancel my timer" (no duration), "set a timer" (no duration), open-ended questions, and anything
   below `min_confidence`. Tune `min_confidence` up if you see mis-resolves.

### 14.3 Regression / safety
- **Default off:** with `system1.backend="none"` (the default), the seam is skipped entirely — a
  quick way to confirm no behaviour change is that the full suite passes and a normal turn is
  byte-identical to before.
- **Timeout:** `HttpDecider` uses a 4 s client timeout; a hung/absent sidecar defers rather than
  stalling the turn. Worth confirming by pointing `base_url` at a dead port and checking the turn
  still completes via System-2.
- **Latency:** measure a resolved turn vs a System-2 turn (transcript → first audio) to confirm the
  win; log timestamps around the seam if needed.

---

## 15. Expanded intent taxonomy (planned — decided 2026-09-27, not yet implemented)

System-1 is a **typed classifier** (choice / noul / score) — it returns a *label*, never free-form
text. So an intent is only **Resolve-able** (answer + skip System-2) when its arguments are
**absent, defaulted, or deterministically parseable**. Anything needing a free-form slot can be
**classified** but not **resolved**. That splits every candidate into three uses:

- **Resolve** — System-1 answers and returns (skips recall + LLM). The latency win.
- **Route-only** — System-1 can't answer, but the cheap label still (a) can trigger immediate
  "filler" speech while the slow tool runs, and (b) tells System-2 which tool to reach for. (The
  filler tie-in is the immediacy work; see the turn-pipeline filler discussion.)
- **Defer** — the `other` bucket, unchanged.

### 15.1 Tier A — Resolve (no free-form slot)

| Intent | Maps to | Args | Status |
|---|---|---|---|
| `weather` | `weather_lookup` → `ShowWeather` | home location (default) | ✅ shipped (M1). Named place → `names_place` noul → defer |
| `timer_start` | `StartTimer` | duration via `parse_duration_secs` | ✅ shipped (M3, as `timer`) |
| `timer_cancel` | `CancelTimer{None}` | none (cancel-all) | planned. Labeled cancel needs a slot → defer |
| `time` | spoken, `current_datetime_line()` | none | planned. No widget, no clock round-trip |
| `date` | spoken, from clock | none | planned |
| `weather_dismiss` | `DismissWeather` | none | planned; gate on `screen == weather` |
| `recipe_dismiss` | `DismissRecipe` | none | planned; gate on `screen == recipe` (your `close_recipe`) |
| `end_session` | `audio_stop` **without** a `listen` frame | none | planned. Ends the follow-up chain → device returns to IDLE / wake-word (see §15.3) |
| `stop_dismiss` | context ladder (§17) | none (referent from device state) | planned. Bare "stop"/"never mind"/"dismiss" |
| `timer_query` | spoken, from timer context params | none | planned. Resolved **only when the device reports an active timer** (§17.1); the remaining time comes from the context parameters, so no Core-side timer state is needed |

### 15.2 Tier C — Route-only (classify → filler + tool hint; System-2 answers)

Free-form slots, so **never resolved** — but these are the *slow* tools where an instant filler
("let me check…") pays off most: `internet_search`, `directions_lookup`, `recipe_lookup`,
`calendar_lookup` (some "today/tomorrow" sub-cases could graduate to Tier A), `shopping_add`.

### 15.3 `end_session` — how it ends the chain

`emit_follow_up_and_stop` (`orchestrator.rs:1716`) sends the `listen` frame **then** `audio_stop`.
The `end_session` handler simply **skips the `listen` frame** and sends `audio_stop` alone. The
device ends its turn on the first `audio_stop`, and with no `listen` it drops back to IDLE /
wake-word-waiting. Optional sub-second ack ("okay") or a silent close. Safest resolved only when
`followup_depth > 0` (mid-conversation) — a fresh wake-word turn has nothing to end.

## 16. Temporal classification (decided 2026-09-27 — 3-way)

> **Status: ✅ IMPLEMENTED 2026-09-27 (M5.1).** `HttpDecider` asks the 3-way `temporal` choice and
> `interpret` defers on a **confident `past`** (gated by `min_confidence`, so an unsure temporal never
> over-defers a present query). present/future resolve unchanged. Unit-tested in `system1/http.rs`.
> The question is optional on the wire — a checkpoint that doesn't answer it simply skips the gate.

A third question in the same one-shot forward pass (free):

```jsonc
"temporal": { "type": "choice",
  "instructions": "Does answering `message` concern the PAST (already happened / historical), the
                   PRESENT (now / current state), or the FUTURE (upcoming / forecast / scheduled)?
                   Choose 'present' if not time-bound.",
  "criteria": { "past": "...", "present": "...", "future": "..." } }
```

It is both a **defer gate** (we have no fast historical-data path) and a **data selector**
(current vs forecast). Not-time-bound commands classify as `present` and their intent ignores the
field.

**Rule:** `temporal == past` → **defer** for every data intent. Weather is the only intent where
both `present` and `future` resolve.

### 16.1 Resolve-by-tense matrix

| Intent | Past | Present | Future | Data source / note |
|---|---|---|---|---|
| `weather` | defer (no historical fetch) | **resolve** — current | **resolve** — forecast (`ShowWeather` already carries the 7-day row) | Open-Meteo; tense selects current vs forecast |
| `time` | defer | **resolve** (clock) | defer ("time in 3 hrs" = calc) | spoken |
| `date` | defer | **resolve** (clock) | defer ("date next Friday" = calc) | spoken |
| `timer_start` | n/a | n/a | **resolve** (inherently future) | tense not a gate |
| `timer_cancel` / `timer_query` | n/a | **resolve** | n/a | present-only |
| `calendar_lookup` *(if added)* | defer | resolve ("today") | resolve ("tomorrow/this week") | past → historical → System-2 |
| `end_session`, `stop_dismiss`, dismiss family | — | — | — | atemporal; ignore the field |

## 17. Device-context disambiguation (decided 2026-09-27 — 3B hybrid + ground-truth veto)

> **Status: ✅ ladder + veto IMPLEMENTED 2026-09-27 (M5.2).** `handle_stop_dismiss` runs the §17.3
> priority ladder over the P0 device context and returns `None` (→ defer) when no rung matches (the
> ground-truth veto). Rungs 2 (running timer), 4 (open screen), 5 (end an active follow-up) are live;
> rungs 1 (alarm ringing) & 3 (media) are parked (§19.7). **§17.4's "classifier sees the state" half
> ✅ landed (M5.3):** `build_request` folds a compact device summary into the request `state` (only
> when present), so the classifier is now context-aware; the ladder still owns the referent. The
> remaining M5.3 work is empirical — running the calibration harness to tune the threshold(s).

### 17.1 In-scope — extend the reported device context (committed 2026-09-27)
`DisplayContext` (`protocol.rs:690`) today reports **only the foreground widget** (`screen.kind` =
`recipe` / `weather`). That is the core limitation: a timer (or, later, music) can be **running in
the background** while a *different* widget — or the idle slideshow — is front-and-center, and the
Core is currently blind to it. So "how much time is left?" or "stop" can't be resolved even though a
timer plainly exists.

> **Note (background context, not just the foreground widget):** the device already tracks its
> timers for the countdown UI, so it must **report active/ringing timers as context parameters on
> every turn regardless of which widget is foreground** — the timer state is orthogonal to the
> `screen.kind`, not a variant of it. This unblocks `timer_query`, `timer_cancel`, and the
> `stop_dismiss` ladder when a timer is running behind another screen. Media state follows the same
> pattern when music lands.

Add to the `audio-start` `screen` block (device → Core), surfaced on `DecisionRequest`:

```jsonc
"device": {
  "screen": "recipe" | "weather" | "idle",              // foreground widget (existing)
  "timers": { "running": 2, "ringing": true,            // NEW — reported even when backgrounded
              "next_remaining_secs": 125,               //   soonest-to-fire, so timer_query can answer
              "labels": ["pasta"] },                    //   optional, for labeled cancel/query later
  "media":  { "state": "playing" | "paused" | "none" }  // NEW (when music lands)
}
```

### 17.2 Split phrasing from referent
- **Explicit phrasing routes directly**, no ladder: "cancel the timer" → `timer_cancel`, "pause the
  music" → music, "close the recipe" → `recipe_dismiss`, "that's all" → `end_session`.
- **Bare/overloaded words** ("stop", "cancel", "never mind", "dismiss", "done") → the generic
  **`stop_dismiss`** label; the orchestrator resolves the referent via the ladder below.

### 17.3 The priority ladder (deterministic; orchestrator owns the referent)
1. alarm **ringing** → cancel that timer *(loudest thing wins)*
2. else timer **running** → `timer_cancel`
3. else media **playing** → pause
4. else recipe/weather **screen up** → dismiss it
5. else in a **follow-up chain** (`followup_depth > 0`) → `end_session`
6. else → **defer**

### 17.4 The 3B hybrid contract
- **Model's job:** classify the intent (incl. the `stop_dismiss` family) **with** device state folded
  into `state`, so vague/anaphoric phrasing ("close this", "that's enough") resolves confidently. It
  does **not** name the referent.
- **Ladder's job:** given `stop_dismiss`, pick the referent from live context (§17.3).
- **Ground-truth veto (neutralizes the precision risk):** if the ladder finds **no valid referent**,
  the turn **defers** — even if the model was confident. The model can never conjure a stop action
  from nothing.
- **On disagreement** (model guessed "music", ladder says timer): ladder wins; log the divergence
  for calibration.

## 18. Calibration corpus (Tier A; temporal-tagged)

Precision-first: a mis-resolve on a ✗ is worse than a defer on a ✓. Sweep `min_confidence` to the
knee where the ✗ set is ~100% deferred. These double as `mock`/golden-decision fixtures (§11).

**`weather`**
- ✓ present: what's the weather · is it raining right now · current temperature
- ✓ future: will it rain today/tomorrow/this weekend · what's the forecast
- ✗ past → defer: what was the weather yesterday · how hot did it get last week
- ✗ defer: what's the weather in Tokyo *(`names_place`)*

**`timer_start`**
- ✓ set a timer for ten minutes · 90 second timer · set a timer for 1 hour 30 minutes
- ✗ defer: set a timer *(no duration)* · wake me up at 7am *(clock alarm)*

**`timer_cancel`**
- ✓ cancel my timer · stop the timer · turn off the alarm · cancel all timers
- ✗ defer: cancel the pasta timer *(labeled → slot)*

**`timer_query`** (resolved only when the device reports an active timer)
- ✓ (timer running) how much time is left · how long on my timer · is my timer still going
- ✗ defer: how much time is left *when no timer is active* · how long on the pasta timer *(labeled → slot)*

**`time` / `date`** (present only)
- ✓ what time is it · do you have the time · what's the date · what day is it
- ✗ defer: what time is it in London *(`names_place`)* · what's the date next Friday *(future calc)*

**`weather_dismiss` / `recipe_dismiss`** (gate on matching screen)
- ✓ close the weather · dismiss the forecast — *(weather screen up)*
- ✓ close the recipe · I'm done cooking — *(recipe screen up)*
- ✗ defer: close the recipe *when no recipe screen is up* · go back to the ingredients *(→ recipe_nav)*

**`end_session`** (safest when `followup_depth > 0`)
- ✓ that's all · I'm done · nothing else · goodbye · go to sleep · you can go now · we're good
- ✗ defer: stop *(bare — ladder territory)* · stop the timer *(→ timer_cancel)* · pause *(→ media)*

**`stop_dismiss`** (referent from device context)
- ✓ (timer running) stop · cancel that · that's enough → `timer_cancel`
- ✓ (alarm ringing) stop · turn it off · okay okay → cancel ringing timer
- ✓ (recipe up, nothing running) stop · dismiss · close this → `recipe_dismiss`
- ✓ (nothing active, in follow-up) stop · done → `end_session`
- ✗ defer: bare "stop" with **no** device context and **not** in a follow-up *(ground-truth veto)*

**Cross-cutting must-always-defer (precision anchors → `other`)**
- who was the 16th president · what's 15% of 240 · translate "thanks" to Spanish
- add milk to the list *(Tier C)* · directions to mom's *(Tier C)* · find me a lasagna recipe *(Tier C)*
- remember I'm allergic to peanuts *(handled by `parse_command` before System-1 — must never reach it)*

## 19. Implementation plan — Pre-req P0: device → Core context extension

> **Status: ✅ IMPLEMENTED 2026-09-27.** Both facts in §19.0 handled; device + Core changes below
> landed. Core: 316 lib tests + new `protocol` parser tests green; device: 95 tests + new
> `client`/`timer` tests green; `cargo clippy --all-targets -D warnings` clean on both crates; `cargo
> fmt` clean. **Deferred to M5.2:** §19.4 step 3 (folding the device summary into the classifier
> `state`) — it only matters once `stop_dismiss` exists and needs a live model to calibrate, so adding
> it now would change weather/timer resolution behavior with nothing consuming it. **Note:** one
> unrelated test (`tests/pipeline.rs::silent_turn_discards_stt_hallucination_and_ends_the_chain`) was
> **already failing on clean `origin/main`** before this work — an upstream "relay raw transcript
> pre-gate" change relays the raw STT text to the device before the no-speech gate; verified by
> running it against a stashed-clean tree. Left as-is per direction; tracked separately.

**This is the first build step** (gates §16 forecast-vs-current framing only loosely, but hard-gates
`timer_query`, `timer_cancel`, and the entire `stop_dismiss` ladder in §17). Land it before any new
intent handler. It is additive and self-contained: no new frame types, no new intents — just richer
turn context flowing device → Core.

### 19.0 Two facts this plan is built on (verified in-tree, 2026-09-27)
1. **The existing widget context is not wired into System-1 yet.** `orchestrator.rs:703` builds
   `DecisionRequest { screen: None /* M1 TODO */, history: Vec::new(), .. }` — the `screen` captured
   from the turn's `audio-start` (`orchestrator.rs:307`) is dropped. So P0 must thread the *existing*
   recipe/weather context too, not just the new timer fields. (The doc's earlier "populated in M1"
   note in §6.1 is aspirational; the code has stubs.)
2. **Timers carry no deadline or ringing state today.** `TimerEntry`
   (`anamanti-display/rust/src/engine/timer.rs:38`) holds only `{label, abort}`; a fired timer rings a
   finite bell and **removes itself** (`timer.rs:138`). So "how much time is left" needs a stored
   deadline, and a *persistent* "ringing" state does not exist. **Decision for P0:** report
   `running` + `next_remaining_secs` + `labels` (fully covers the user's "background timers as
   context"); **park `ringing`** (ladder rung 1) until/unless we add a latched/looping alarm UX — see
   §19.7. Rung 2 (`running` → `timer_cancel`) already covers "stop the timer".

### 19.1 Wire shape & compatibility
Timer/media state is **orthogonal to the foreground widget**, so it rides as sibling keys inside the
existing `audio-start` `screen` block (not a new `kind`, not a new frame):
```jsonc
"screen": {
  "kind": "recipe", "recipe": { ... },                 // existing foreground widget (optional/absent)
  "timers": { "running": 2, "next_remaining_secs": 125, // NEW — present even with no/other widget
              "labels": ["pasta"] }
  // "media": { "state": "playing" }                    // NEW later, when music lands
}
```
**Additive & backward-compatible:** an old Core ignores `timers`; an old device omits it and the new
Core reads it as empty. Guard both crates' round-trip tests (device `types` ↔ core `protocol` are
byte-identical for frames; this is *data*, but keep the tests green).

### 19.2 Device changes (`anamanti-display/rust`)
1. **Store the deadline.** Add `deadline: Instant` to `TimerEntry` (`engine/timer.rs:38`), set to
   `Instant::now() + Duration::from_secs(secs)` at insert (`timer.rs:140`).
2. **Snapshot method.** `TimerManager::snapshot() -> TimerSnapshot` iterating the live map:
   `running = len`, `next_remaining_secs = min(deadline - now).max(0)`, `labels = non-empty labels`.
   Cheap; takes the existing `Mutex`.
3. **Merge into `audio-start`.** `WyomingClient::send_audio_start` (`wyoming/client.rs:129-147`)
   currently inserts only `self.display_context` under `"screen"`. Extend it to also insert the
   per-turn timer snapshot as `screen.timers` — **and to emit a `screen` block even when
   `display_context` is `None`** but a timer is running (today it no-ops on an idle screen). Source the
   snapshot at turn start: the turn driver (the engine holding the `TimerManager`) computes
   `snapshot()` and hands it to the client via a new `set_timer_context(...)` just before
   `send_audio_start` (keeps `WyomingClient` decoupled from `TimerManager`).

### 19.3 Core changes (`anamanti-core`)
1. **Parse orthogonally.** In `wyoming/protocol.rs`, add `TimerContext { running: u32,
   next_remaining_secs: Option<u64>, labels: Vec<String> }` and a `device_context(data) ->
   DeviceContext { widget: Option<DisplayContext>, timers: TimerContext }`. Critically, parse
   `screen.timers` **independently of `screen.kind`** — the current `display_context()` early-returns
   `None` when `kind` is absent (`protocol.rs:732`), which would drop timers on an idle screen. Keep
   `display_context()` as-is for the System-2 prompt line; `device_context()` is the superset.
2. **Capture it.** In `orchestrator.rs:300-312`, replace the `display_context(&ev.data)` capture with
   `device_context(&ev.data)` so the turn carries both widget + timers.

### 19.4 Thread into System-1
1. **Extend `DecisionRequest`** (`system1/mod.rs`): keep `screen: Option<String>`, add
   `timers: TimerContext` (and later `media`).
2. **Populate it for real** (`orchestrator.rs:701`): set `screen` = a label derived from the widget
   (`"recipe"`/`"weather"`/absent) — closing the existing TODO — and `timers` = the parsed snapshot.
3. **Fold a compact device summary into the classifier `state`** (`system1/http.rs::build_request`)
   for the 3B hybrid (§17.4): e.g. `state.timers_running`, `state.timer_ringing` (false for now),
   `state.screen`. Deterministic handlers read the typed `DecisionRequest.timers`; the model only
   *sees* the summary to sharpen `stop_dismiss` confidence.

### 19.5 What P0 unblocks (built in later milestones, not here)
- `timer_query` → answered from `timers.next_remaining_secs`; **resolves only when `running > 0`**.
- `timer_cancel` bare-word path and the `stop_dismiss` ladder rungs 2 & 4 (running timer / open
  screen). Rung 1 (ringing) stays parked (§19.7).

### 19.6 Tests
- **Device:** `snapshot()` computes remaining + count (unit); `send_audio_start` emits `screen.timers`
  with and without a foreground widget (assert JSON shape).
- **Core:** `device_context` parses `timers` with `kind` present, with `kind` absent, and when
  `timers` is missing (→ empty); a golden `audio-start` fixture round-trips.
- **Pipeline:** a turn started with a running-timer context makes `DecisionRequest.timers.running > 0`
  reach the engine (extend the `mock` decider fixtures).

### 19.7 Parked (explicit follow-ups, not P0)
- **`ringing` / ladder rung 1** — needs a latched or looping alarm that persists until dismissed;
  today's finite bell + self-removal (`timer.rs:138`) has nothing to silence. Revisit with any
  "alarm keeps sounding until you say stop" UX.
- **`media` state** — lands with music transport; same orthogonal-field pattern.
- **Labeled timer query/cancel** — needs the free-form label slot → stays on System-2.
