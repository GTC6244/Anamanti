# Memory Plan — GraphRAG Memory for the Ambient Assistant

Status: **IMPLEMENTED (v1)** — all four goals built and tested behind the
`helix` cargo feature (on by default). SQLite remains the default runtime
backend; HelixDB GraphRAG is opt-in via `AMBIENT_MEMORY_BACKEND=helix`. This
plan layers a HelixDB-backed GraphRAG memory + chat-log pipeline onto the
existing Mac-side Rust brain (`ambient_orchestrator`, `/mac`).

## Implementation status (2026-09-16)

**Built & tested (56 tests green, clippy clean, both feature configs build):**
- **Goal 2 — chat log:** `orchestrator/src/memory/chatlog.rs` — append-only JSONL, one
  record per turn, written by `orchestrator.rs::log_turn`. Always on.
- **Goal 3 — embeddings:** `orchestrator/src/memory/embed.rs` — `Embedder` trait,
  `OpenAiEmbedder` (`text-embedding-3-small`), `MockEmbedder` (offline tests).
  `orchestrator/src/memory/ingester.rs` — background batch ingester (drains JSONL →
  batch-embed → extract → upsert → commit offset sidecar).
- **Goal 1 — GraphRAG store:** `orchestrator/src/memory/helix.rs` — embedded HelixDB
  (`db::HelixDB`, in-process, `HelixDbSource::Disk`), schema
  (`User/Turn/Memory/Entity` + `SAID/MENTIONS/ABOUT/FOLLOWS/KNOWS`), vector
  indexes, idempotent upserts by `ext_id`. Entity extraction:
  `orchestrator/src/memory/entity.rs` (`AnthropicEntityExtractor` = Claude Haiku 4.5;
  `NoopEntityExtractor` when no key; `MockEntityExtractor` for tests).
  **Entity names are editable** to fix a misspelled fact: `HelixMemory::rename_entity`
  (exposed on the `GraphView` trait) fixes the name *everywhere it appears* — the
  `Entity` node's `name` property in place (node id + all `MENTIONS`/`ABOUT`/`KNOWS`
  edges preserved) **and** every whole-word occurrence in the free text of `Turn`
  (`text`) and `Memory` (`content`) nodes (case-sensitive, boundary-aware via
  `replace_whole_word`, so `"Sam"` never mangles `"Samsung"`). It refuses a target
  name already used by a different entity (would split that entity's edges), and
  returns a `RenameOutcome { entities, turns, memories }` tally. The stale vector
  `embedding` on a rewritten turn/memory is intentionally left as is (a one-token
  spelling fix barely moves it; re-embedding would need the embedder). Surfaced as
  an "Edit name" button on each `Entity` row of the `/helix` debug page
  (`POST /helix/rename-entity`, `orchestrator/src/webconfig.rs`).
- **Goal 4 — recall + inject:** `orchestrator/src/memory/backend.rs` — `Recall` trait
  (`SqliteRecall` default, `HelixRecall` = embed query → vector KNN + graph
  expansion). Wired into `orchestrator.rs::build_context`.
- **Wiring:** `config.rs` (all `AMBIENT_*` env vars below), `main.rs`
  (`build_graphrag_recall`, spawns ingester, tokio runtime with a 16 MiB worker
  stack — the engine's deep async types overflow the 2 MiB default).

**Verified end-to-end:** the real binary with `AMBIENT_MEMORY_BACKEND=helix`
opens the on-disk store, embeds via HTTP (proven against a fake OpenAI server),
runs the ingester, and upserts turns into the graph; integration tests drive the
full chat-log→ingest→recall path on the real embedded engine (mock
embedder/extractor). Graceful fallback to SQLite when `OPENAI_API_KEY` is absent.

**Needs your live keys to exercise against real providers (code + parsing done,
tested against local mock servers):** OpenAI embeddings (`OPENAI_API_KEY`),
Claude Haiku extraction (`ANTHROPIC_API_KEY`).

**Not yet done (deliberate v1 cuts):** SQLite→Helix backfill/migration tool
(`migrate.rs`); real speaker attribution (all turns → shared `household` user);
`Turn`-node pruning/retention job; per-connection session ids (v1 uses a
per-turn session id, so `FOLLOWS` chaining is best-effort). See "Deferred".

### Env vars (added)
`AMBIENT_MEMORY_BACKEND=sqlite|helix` (default `sqlite`) ·
`AMBIENT_CHATLOG_PATH` (default `ambient_chatlog.jsonl`) ·
`AMBIENT_HELIX_PATH` (default `ambient_helix`) ·
`AMBIENT_EMBED_MODEL` · `AMBIENT_EMBED_DIMS` (default 1536) ·
`AMBIENT_EXTRACT_MODEL` (default `claude-haiku-4-5`) ·
`AMBIENT_OPENAI_BASE_URL` · `AMBIENT_ANTHROPIC_BASE_URL` ·
`AMBIENT_INGEST_INTERVAL_SECS` (default 30) ·
plus `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`.

## Decisions locked (2026-09-15)

1. **Embeddings:** OpenAI `text-embedding-3-small` is the sole embedder. Cloud
   calls are accepted; **no local/offline fallback in v1.** (Owner accepts that
   conversation text leaves the device for embedding.)
2. **Coexistence:** HelixDB runs **alongside** SQLite behind a new
   `MemoryBackend` trait. SQLite stays the default; Helix is opt-in via
   `AMBIENT_MEMORY_BACKEND=helix`.
3. **Ingestion:** **batch in the background** — a task drains the JSONL chat log,
   embeds new records in batches, extracts entities, and upserts into Helix.
   Live turns never block on embedding/ingestion.
4. **Graph:** **full GraphRAG** with automatic entity/topic extraction. Because
   ingestion is batched, the extraction LLM call runs **in the background
   ingester**, not the live turn → richer graph at **zero added turn latency.**
5. **HelixDB deployment:** ~~embedded in-process~~ → **co-located local
   instance** (see Research below). HelixDB v3 has **no embeddable library
   API** — the published Rust crate is an HTTP client. So we run a local Helix
   instance on the Mac Mini and the orchestrator talks to it over
   `localhost:6969`. As local/private as embedding, minus the in-process part.
6. **Multi-user household:** the graph models **multiple `User` nodes**.
   **Speaker identification is future work** — until it exists, turns are
   attributed to a single `unknown`/shared user (see Q on interim behavior),
   and the schema is built so real speaker IDs can be attached later without a
   migration.

---

## 0. Where memory lives today (baseline)

The current memory is deliberately simple (Plan.MD Phase 4):

- **Store:** SQLite + FTS5, one row per fact/preference
  (`orchestrator/src/memory/mod.rs`, `MemoryStore`). Path from `AMBIENT_DB_PATH`
  (default `ambient_memory.sqlite`).
- **Write policy:** *explicit* voice commands ("remember…"/"forget…") via
  `parse_command`, plus *inferred* heuristic extraction via `infer_memories`
  (`orchestrator/src/memory/extract.rs`).
- **Read/inject:** `orchestrator.rs::build_context()` runs
  `memory.search(transcript, 8)` (keyword FTS) and appends the hits as a
  `- bullet` list onto the system prompt for that turn.
- **Management:** list/delete/clear over the Phase-6 control protocol
  (`control.rs`, `settings.rs`) and by voice.
- HTTP-to-cloud pattern to copy: `orchestrator/src/llm/anthropic.rs` (raw `reqwest`,
  API key from env). Config/env wiring: `orchestrator/src/config.rs`.

**These four goals extend that spine.** Every new piece needs to answer: does it
*replace* SQLite, or run *alongside* it?

---

## 1. Goal: HelixDB for GraphRAG

**What HelixDB gives us:** a Rust-native graph **+** vector database — nodes,
typed edges, and vector-indexed embeddings in one store, queried with HelixQL.
That combination is exactly what "GraphRAG" wants: semantic recall (vector KNN)
*plus* relationship traversal (this preference belongs to this person, was said
in this session, relates to this topic).

### Proposed graph model (v1)

Nodes:
- `User` — a household member. **Multiple** allowed. Until speaker ID exists, a
  single shared `household` user is used; schema leaves room to attach real
  speaker IDs later with no migration.
- `Turn` — one conversation exchange (transcript + reply + timestamp +
  session_id). Carries an embedding vector (transcript+reply).
- `Memory` — a durable fact/preference (mirrors today's SQLite rows). **Also
  carries an embedding vector** so facts are directly recallable.
- `Entity` — people, places, things, topics extracted from turns/memories by the
  background ingester (Claude Haiku 4.5).

Edges:
- `(User)-[:SAID]->(Turn)`
- `(Turn)-[:MENTIONS]->(Entity)`
- `(Memory)-[:ABOUT]->(Entity)`
- `(Turn)-[:DERIVED]->(Memory)` (provenance for inferred facts)
- `(Turn)-[:FOLLOWS]->(Turn)` (temporal chain within a session)
- `(User)-[:KNOWS]->(Entity)` (per-user affinity; lights up once speaker ID lands)

### Deployment (settled by research — see Research section)

HelixDB v3 is **not embeddable in-process.** The public Rust surface
(`helix-db` crate 3.0.0, imported `helix_db`) is a thin async HTTP client over
`reqwest`; the engine crate (`crates/db`) is an unpublished workspace member
that depends on path crates + a git-pinned fork of SlateDB, with no stable
public embed API.

**Chosen path:** run a **co-located local HelixDB instance** on the Mac Mini
(installed via the `helix` CLI, `helix start dev`, listening on
`localhost:6969`). The `HelixMemory` backend (`orchestrator/src/memory/helix.rs`) uses
the `helix-db` Rust SDK `Client` pointed at `http://localhost:6969` and runs
queries authored with the Rust `#[query]` DSL via `POST /v2/query` (no separate
build/deploy step in v3). Traffic never leaves the box, so this keeps the
local/private property — it just isn't in-process.

**Operational note:** the local Helix instance must be running for the `helix`
backend to work. Manage it the same way TODO.md already plans to run the
orchestrator (launchd/login item on the Mac). If Helix is unreachable, fall
back to the SQLite backend so turns still work.

### Integration seam

Introduce a `MemoryBackend` trait so `MemoryStore` (SQLite) and a new
`HelixMemory` are swappable behind one interface, selected by env
(`AMBIENT_MEMORY_BACKEND=sqlite|helix`), exactly like `AMBIENT_LLM_BACKEND`.
This keeps the orchestrator and control protocol unchanged and lets us ship
incrementally.

---

## 2. Goal: save chat logs to a file

- **Format:** append-only **JSONL**, one record per completed turn. Proposed
  fields: `{ id, ts, session_id, transcript, reply, memories_written[],
  llm_backend, model }`.
- **Location:** next to the DB (e.g. `AMBIENT_CHATLOG_PATH`, default
  `ambient_chatlog.jsonl`).
- **Write point:** end of `Orchestrator::handle_turn` once the reply is final,
  before/after TTS. Failure to log must **never** break a turn (log-and-continue).
- **Why a file at all:** it's the durable, human-auditable source of truth and
  the **ingestion queue** for embeddings (Goal 3) — decoupling capture from
  embedding means we can embed in batches and re-embed if the model changes.

---

## 3. Goal: OpenAI `text-embedding-3-small` for ingestion

- **Model:** `text-embedding-3-small`, 1536 dims (supports dimension
  reduction via `dimensions` param if we want smaller vectors in Helix).
- **Client:** new `orchestrator/src/memory/embed.rs`, raw `reqwest` to
  `POST https://api.openai.com/v1/embeddings`, key from `OPENAI_API_KEY`
  (same env pattern as `ANTHROPIC_API_KEY`). Supports batch input arrays.
- **What gets embedded:** each `Turn` (transcript, or transcript+reply — see Q),
  and optionally each `Memory`. Vector stored on the corresponding HelixDB node.
- **When (locked):** **batch, background.** A `MemoryIngester` task watches the
  JSONL log (tracking a committed offset/high-water mark), and for each new
  batch of turns:
  1. embeds them (batched array input → one OpenAI call per batch),
  2. runs **entity/topic extraction** (LLM) to produce `Entity` nodes + edges,
  3. upserts `Turn`/`Entity`/edges into HelixDB,
  4. advances the committed offset (crash-safe: re-runs are idempotent upserts).
- **Query-time embedding (online):** the *incoming transcript* is still embedded
  synchronously per turn for retrieval (Goal 4) — one small OpenAI call on the
  hot path. Accepted, since cloud embedding is approved.

> **Privacy note (acknowledged, not blocking):** conversation text is sent to
> OpenAI for embedding. Owner accepted this for v1; no offline fallback. Revisit
> if an offline mode is ever required.

---

## 4. Goal: pull context back from HelixDB and inject it

This replaces/augments `build_context()`. Per turn, given the transcript:

1. **Embed the query** — call `text-embedding-3-small` on the incoming
   transcript to get a query vector. (Online embed of the *query* is required
   even in batch mode, unless we keep a local embedder — see Q.)
2. **Vector recall** — HelixQL vector KNN over `Turn`/`Memory` embeddings →
   top-K semantically similar nodes.
3. **Graph expansion (the "Graph" in GraphRAG)** — from those seed nodes,
   traverse edges 1–2 hops (`MENTIONS`, `ABOUT`, `FOLLOWS`) to pull in related
   entities, co-mentioned facts, and adjacent turns the pure vector hit missed.
4. **Rank + budget** — merge, dedupe, score (vector similarity + graph
   proximity + recency), truncate to a token budget.
5. **Inject** — format the selected memories/turns as context and append to the
   system prompt, exactly where `build_context()` does today, so the LLM layer
   is untouched.

```
transcript
  → embed(query)                     [OpenAI]
  → helix.vector_search(qvec, K)     [HelixQL KNN]
  → helix.expand(seeds, hops)        [HelixQL traversal]
  → rank + token-budget
  → "Relevant memory:\n- …\n- …" appended to system prompt
  → LLM.respond(system_prompt, transcript)   [unchanged]
```

---

## Code surface (new / changed)

New modules under `orchestrator/src/memory/`:
- `backend.rs` — `MemoryBackend` trait; `MemoryStore` (SQLite, existing) and
  `HelixMemory` both implement it. Selected in `config.rs`.
- `helix.rs` — `helix-db` SDK `Client` (→ `localhost:6969`); schema + Rust
  `#[query]` functions for vector KNN + graph traversal + upserts.
- `embed.rs` — OpenAI `text-embedding-3-small` client (reqwest, batch input),
  `OPENAI_API_KEY`.
- `chatlog.rs` — append-only JSONL writer.
- `ingester.rs` — background task: JSONL → embed → Haiku entity extraction →
  Helix upsert; tracks committed offset.
- `migrate.rs` — one-time SQLite → Helix backfill.

Changed:
- `config.rs` — `AMBIENT_MEMORY_BACKEND`, plus paths/keys below.
- `orchestrator.rs` — `build_context()` calls the backend's graph retrieval;
  `handle_turn()` appends to the chat log.
- `Cargo.toml` — add `helix-db = "3"` (SDK client; reqwest already present).

New env vars (following the existing `AMBIENT_*` convention):
- `AMBIENT_MEMORY_BACKEND=sqlite|helix` (default `sqlite`)
- `AMBIENT_HELIX_URL` (default `http://localhost:6969`)
- `AMBIENT_CHATLOG_PATH` (default `ambient_chatlog.jsonl`)
- `OPENAI_API_KEY` (embeddings) · `ANTHROPIC_API_KEY` (Haiku extraction, reused)
- `AMBIENT_EXTRACT_MODEL` (default a Claude Haiku 4.5 id)

## Rollout (proposed order)

1. **Chat-log JSONL** (Goal 2) — pure add, zero risk, gives us data immediately.
2. **HelixDB spike** — install the `helix` CLI, `helix start dev` on `:6969`,
   define the schema + Rust `#[query]` functions, and prove a round-trip from
   the orchestrator via the `helix-db` SDK. *(Gate: confirm durable local
   persistence — see Remaining risks.)*
3. **Embedding client** (Goal 3) — `embed.rs` + `ingester.rs` batch job over the
   JSONL, incl. Haiku entity extraction.
4. **GraphRAG retrieval** (Goal 4) — `HelixMemory` behind the `MemoryBackend`
   trait; wire into `build_context` under `AMBIENT_MEMORY_BACKEND=helix`.
5. **Backfill** existing SQLite memories (`migrate.rs`).
6. Keep SQLite as the default/fallback until the Helix path is proven.

---

## Resolved

- Q1 privacy → **OpenAI, no local fallback.**
- Q2 coexistence → **alongside SQLite, `MemoryBackend` trait, Helix opt-in.**
- Q3 ingestion → **batch/background over JSONL.**
- Q4 graph → **full graph, entity extraction in the background ingester.**

- Q5 HelixDB deployment → ~~embedded in-process~~ **co-located local instance**
  (`localhost:6969`) via the `helix-db` Rust SDK. *(Corrected by research; no
  embeddable API exists — see Research section.)*
- Q6 users → **multi-user household**; speaker ID is future work; interim
  attribution to a shared/unknown user with a forward-compatible schema.
- Q7 retention → **keep JSONL logs forever** (source of truth) but **prune old
  `Turn` nodes/vectors** from Helix to bound the index.
- Q8 embed → **transcript + reply + durable `Memory` rows** all get vectors.
- Q9 interim speakers → all turns attributed to one shared `household` `User`.
- Q10 extraction model → **Claude Haiku 4.5** (Anthropic) in the background
  ingester, reusing the existing `orchestrator/src/llm/anthropic.rs` HTTP client pattern
  (needs `ANTHROPIC_API_KEY`).
- Q11 migration → **backfill** existing SQLite fact/preference rows into HelixDB
  as `Memory` nodes (embedded + entity-extracted) via a one-time import tool.

## SPIKE RESULT (2026-09-15) — embedded in-process HelixDB WORKS ✅

Overturns the "no embeddable API" finding below (that was true only of the
*published* crate). A throwaway crate depending on the **engine crate via git**
compiled and ran all GraphRAG primitives in-process — **no Docker, no server**:

```toml
db        = { git = "https://github.com/HelixDB/helix-db.git", package = "db",        rev = "6fde5bc8e3ceb7624124ad18d2ef6ef0bc5d60ff" }
helix-ast = { git = "https://github.com/HelixDB/helix-db.git", package = "helix-ast", rev = "6fde5bc8e3ceb7624124ad18d2ef6ef0bc5d60ff" }
```

Verified: **disk persistence** (`HelixDbSource::Disk { root, database }`, survives
reopen), **vector KNN** (`create_index_if_not_exists(IndexSpec::node_vector(label,
prop, dim, VectorDistanceMetric::Cosine, None))` then `vector_search_nodes`), and
**graph traversal** (`add_e` + `.n_with_label(..).out(Some("SAID"))`). API is
async: `HelixDB::open(src).await`, `db.query(QueryRequest::write|read(batch)).await`,
`db.close().await`. Builders: `helix_ast::{batch, graph::NodeRef, traversal::g,
value::PropertyInput, index::*}`. Vectors are node properties
(`PropertyInput::from(vec![f32;N])`); a NodeVector index MUST exist before
insert/search. Compile ~90s cold (slatedb fork + tantivy + foyer), fast after.

**Decision reverted to the original intent:** HelixDB is **embedded in-process**
(git dependency on the `db` engine crate), `HelixDbSource::Disk` for local
persistence on the Mac. No CLI, no Docker, no `:6969` server.

## Research: HelixDB integration surface (2026-09-15, verified via docs/repo)

Sources: `github.com/HelixDB/helix-db` README + `crates/` tree, `sdks/rust`
README, `crates.io/api/v1/crates/helix-db`.

- **No embeddable/in-process API.** The published Rust crate `helix-db` 3.0.0
  (imported `helix_db`, source `sdks/rust`, description "Library for working
  with HelixDB") is *"a thin async HTTP client over reqwest for running queries
  against a Helix instance."* `Client::new(None)` defaults to
  `http://localhost:6969`. TS/Python/Go SDKs likewise POST to `:6969`.
- **Engine is workspace-internal.** Crates: `ast, cli, db, graph-algorithms,
  metrics, planner, server, value-semantics`. The engine `crates/db` is
  `name="db"` `v0.1.0`, path-deps only, plus a **git-pinned fork of SlateDB** —
  not published, no stable public embed surface.
- **Storage engine v3 = SlateDB** (object-storage-backed LSM; `object_store`
  with `aws` feature) + **tantivy** for full-text. Runs locally but is
  cloud-first in design — confirm local persistence config in the spike.
- **Deployment model:** install the `helix` CLI
  (`curl -sSL https://install.helix-db.com | bash`), run `helix start dev`
  (native local instance on `:6969`, no Docker required for local dev).
- **Query model (good news):** queries authored with the Rust `#[query]` DSL and
  sent via `POST /v2/query` — **no separate compile/deploy step in v3.**
- **GraphRAG fit confirmed:** first-class `g().vector_search_nodes(...)` and
  `vector_search_edges(...)` plus graph traversal in the DSL. Vectors live as
  node/edge properties (e.g. `"embedding"`). Exactly what Goal 4 needs.

**Impact on the plan:** the only change is deployment — co-located local
instance instead of in-process. `helix.rs` uses the SDK `Client` over
`localhost`, schema/queries are Rust `#[query]` functions (not a separate
HelixQL deploy). Everything else (trait, ingester, retrieval flow) stands.

## Remaining risks / notes (post-implementation)

- **Worker stack size.** The embedded engine's async state machines need more
  than tokio's 2 MiB default; the binary builds its runtime with a 16 MiB worker
  stack and tests run on a 32 MiB-stack thread. Keep this if refactoring `main`.
- **Vector index is async.** A freshly created NodeVector index reports
  `index_not_found` until the background builder settles; `HelixMemory::init`
  flushes and polls until ready, and the ingester flushes after each batch.
- **Compile cost.** The `helix` feature pulls the engine (SlateDB fork + tantivy
  + foyer): ~90s cold, cached after. `--no-default-features` gives a lean SQLite
  build for fast iteration.
- **Durability on shutdown.** The orchestrator relies on SlateDB's flush cadence
  + the ingester's per-batch `flush_writer`; there is no explicit `close()` on
  Ctrl-C yet (the `Arc<HelixMemory>` is just dropped). Low risk, worth a follow-up.
- **Speaker attribution.** Multi-user graph is only as useful as speaker ID;
  until that lands, all turns attribute to the shared `household` user.

## Deferred (v1 cuts, safe to pick up later)

1. **SQLite → Helix backfill** (`migrate.rs`, plan Q11): import existing
   fact/preference rows as `Memory` nodes. Not built; start-fresh for now.
2. ~~**Speaker identification**: real per-user attribution + populating `KNOWS`
   per speaker.~~ **DONE (2026-09-17)** — see `speaker_id_plan.md`. Per-person
   voiceprint ID (passive + auto-cluster) now attributes each turn to a `User`
   node, scopes SQLite + GraphRAG memory per speaker, injects a "who am I
   speaking with" line into the prompt, and ships a device "People" settings
   screen to name/merge/forget voices. Only the on-hardware ONNX model swap +
   threshold calibration (Phase E) remains.
3. **Retention/pruning** of old `Turn` nodes+vectors (JSONL kept forever).
4. **Per-connection sessions** so `FOLLOWS` chains a whole conversation (v1 mints
   a session id per turn).
5. **Graceful shutdown flush** (`close()` on Ctrl-C).
