# JEV / System-1 Decisions — how we use them

Anamanti's **System-1** stage is a fast, cheap *router* that runs before the full
System-2 LLM turn. It is backed by a **JEV**-family decision model reached over the
shared `/v1/systemone` wire contract (OpenRouter's `typesafe/jev-*`, or a local
`laya-serve` sidecar — same contract, swap base URL + auth + model id).

Implementation lives in [`anamanti-core/src/system1/http.rs`](./anamanti-core/src/system1/http.rs);
the design rationale is in [`plans/system1-fast-decisions.md`](./plans/system1-fast-decisions.md).

## What JEV is — and is not

JEV is a **classifier / scorer**, **not** a slot or entity extractor. Given a `state`
(the turn's `message`) and a set of **typed questions**, it returns one typed answer per
question in a single forward pass. The answer types are:

| Type     | Returns                                             | Example use |
|----------|-----------------------------------------------------|-------------|
| `choice` | one of N fixed labels + calibrated `confidence` + per-label `probabilities` | pick the intent |
| `noul`   | `P(true)` in `0..1` (a boolean belief, no confidence field) | yes/no signals |
| `score`  | a number                                            | magnitudes / ratings |

**Consequences of "not an extractor":**
- JEV can tell you *whether* the message names a place, but **cannot hand back the string
  `"Paris, France"`**. Anything that needs the actual free-text value (a location, a
  person, a query) must be extracted by System-2 (the LLM), not System-1.
- Routing policy (which questions, thresholds, intent→handler) lives in the **orchestrator**,
  not the model. The engine stays dumb: it just scores the questions we send.

## Wire contract

```
POST {base_url}/v1/systemone          # base_url is the API ROOT, e.g. https://openrouter.ai/api
Authorization: Bearer <OPENROUTER_API_KEY>   # OpenRouter only; laya-serve is keyless
{
  "model": "typesafe/jev-1.13",       # omit for laya-serve
  "state": { "message": "<transcript>" },
  "questions": {
    "<id>": { "type": "choice|noul|score", "instructions": "...", "criteria": { ... } },
    ...
  }
}
```

> ⚠️ `base_url` must be the **API root** (`https://openrouter.ai/api`). The client appends
> `/v1/systemone` itself. Configuring the full path doubles it → `404` → a decode error.

The response echoes an `answers` object keyed by question id, each carrying its typed
answer (`choice`+`confidence`, or `noul`, or `score`).

## Bias toward asking MANY questions in ONE call

**JEV bundles up to 64 questions per forward pass.** A single `/v1/systemone` call that
asks a dozen questions costs essentially the same as one that asks a single question — the
latency is one network round trip either way. So:

> **Prefer one rich call with many questions over several sequential calls.**
> Every extra round trip adds ~150–500 ms of dead air to the turn; every extra *question*
> in an existing call is nearly free. When you need more signal, **add a question**, don't
> **add a call**.

We removed an earlier defer-then-retry pattern (ask once, then re-ask with the home
location folded in) precisely because it cost two round trips for the common case. It is
now a single call that asks everything up front — see below.

## The routing question set we send today

One call carries three questions:

1. **`intent`** (`choice`) — pick the best-fit intent from the enabled labels (e.g.
   `weather`, `timer`) plus an `other` escape hatch. Must clear `min_confidence` (default
   0.85) to resolve.
2. **`needs_full_understanding`** (`noul`) — does the turn need open-ended reasoning,
   personal memory, or multi-step understanding? High ⇒ defer to System-2. **Note the
   wording:** a location-based command defaults to the user's *home* location when no place
   is named, so a *missing* location by itself must **not** raise this noul (that's what let
   us drop the retry call).
3. **`names_place`** (`noul`) — does the message name a *specific* place (city / country /
   landmark) as the location to act on, rather than implying the home area? High ⇒ we
   **cannot fast-path** (JEV flagged the place but can't extract it, and the fast-path
   handler only knows the home location), so we defer to System-2, where the LLM extracts
   the place and calls the location-aware tool (`weather_lookup(location=…)`).

### Decision policy (`interpret` + `decide`)

- `needs_full_understanding` ≥ 0.5 → **Defer** (System-2).
- `intent` = `other` / unknown / below `min_confidence` → **Defer**.
- Resolved **location intent** (in `LOCATION_INTENTS`) **and** `names_place` ≥ 0.5 →
  **Defer** (System-2 extracts the named place).
- Otherwise → **Resolve** the intent on the fast path (location intents use the home
  location default).

Net behaviour:
- *"What's the weather?"* → resolves on the fast path against the home location (~1.4 s).
- *"What's the weather in Paris?"* → defers to System-2 → correct city (~4 s, but right).

## Adding a new fast-path intent

1. Add its label + a short criterion in `intent_description` / the intent list.
2. If it's location-bearing, add it to `LOCATION_INTENTS` so a named place routes to
   System-2.
3. If you need an extra routing signal, **add a `noul`/`score`/`choice` question to the
   existing call** — do not introduce a second round trip.
4. Add a fast-path handler in the orchestrator keyed on the resolved intent.
