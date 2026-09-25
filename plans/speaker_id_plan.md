# Speaker ID Plan — Per-Person Identification in the Anamanti Core

Status: **IMPLEMENTED (Phases A–D + device "People" UI; Phase E code-complete
pending the real ONNX model + on-hardware calibration)** as of 2026-09-17. Picks
up `memory_plan.md` Deferred #2
("Speaker identification: real per-user attribution + populating `KNOWS` per
speaker"). Layers passive, local speaker recognition onto the Mac-side brain
(`anamanti_core`, `/mac`) so every turn is attributed to a person and the
LLM answers with per-person context ("you're talking to Sam; here's what you know
about Sam") instead of one undifferentiated `household` blob.

Opt-in and off by default: existing deployments behave identically until speaker
ID is turned on in config (`speaker.enabled`). The real ECAPA-TDNN ONNX embedder is
**always compiled in** — there is no longer a `speaker` build feature — so with no
`speaker.model_path` set it degrades to the dev-only mock embedder.

---

## Implementation status (2026-09-17)

**Built & tested (lean SQLite and default `helix` builds both green, clippy clean):**

- **Phase A — recognizer core** (`anamanti-core/src/speaker/{mod,embed,registry}.rs`):
  `SpeakerEmbedder` trait + deterministic `MockSpeakerEmbedder` (band-energy
  fingerprint), SQLite `SpeakerRegistry` (voiceprint centroids, identify /
  auto-cluster / rename / merge / delete), and the `SpeakerService` façade with
  dual-threshold match/new + min-speech floor. 14 unit tests.
- **Phase B — per-person SQLite path**: `memories.speaker_id` column + idempotent
  migration; `add_scoped` / `search_scoped` (own + shared); `Recall` trait takes a
  speaker scope; Anamanti Core buffers voiced PCM, identifies the speaker, threads
  it through writes/recall, injects the identity line into the system prompt, and
  logs it. End-to-end pipeline test drives two voices → separate memory + a named
  reply.
- **Phase C — naming + control**: voice `NameSpeaker` ("my name is …" / "call
  me …") renames the current profile and records the name fact; new `anamanti-*`
  control frames (`list-speakers`, `name-speaker`, `merge-speakers`,
  `delete-speaker`) handled in `control.rs` (merge also reassigns memory rows).
  Unit + control tests. **Device side done**: byte-identical frame types +
  `control.rs` client + FRB functions in `anamanti-display/rust/src/api/settings.rs` (regenerated
  bindings), and a Flutter **"People"** settings screen (`anamanti-display/lib/src/ui/people_screen.dart`)
  that lists recognized voices (named or anonymous "Speaker N") and names / merges /
  forgets them — 5 widget tests, `flutter analyze` clean.
- **Phase D — GraphRAG per-User**: `helix.rs` mints a `User` node per `speaker_id`
  (`SAID`/`KNOWS` originate from it), tags Turn/Memory nodes with `speaker_id`, and
  scopes `recall` to the speaker + shared; chat-log carries `speaker_id`/
  `speaker_name`; the ingester passes them through. Real-engine graphrag test green.
- **Phase E — real model**: pure-Rust log-mel fbank front-end
  (`speaker/features.rs`, unit-tested) + `OnnxSpeakerEmbedder` (`tract-onnx`, behind
  the opt-in `speaker` cargo feature) wired into `config.build_speaker_service`.
  **Remaining (hardware/model-gated):** drop in a real ECAPA-TDNN `.onnx`, confirm
  its input tensor layout + fbank params, and calibrate `MATCH`/`NEW`/
  `MIN_SPEECH_MS` against real Echo Show far-field captures. Until then, enabling
  `ANAMANTI_SPEAKER_ID=on` uses the mock embedder (dev only, logged loudly).

**Config:** `ANAMANTI_SPEAKER_ID=on|off` (default off) · `ANAMANTI_SPEAKER_MODEL_PATH`
· `ANAMANTI_SPEAKER_MATCH_THRESHOLD` · `ANAMANTI_SPEAKER_NEW_THRESHOLD` ·
`ANAMANTI_SPEAKER_MIN_SPEECH_MS` · `ANAMANTI_SPEAKER_EMBED_DIMS` (default 192).

---

## 1. Why / what changes for the user

Today the assistant is *household-scoped*. Two people share one memory pool:
Sam's "I like jazz" and Dana's "I hate jazz" both attach to the shared
`household` user (`helix.rs:49`), FTS recall mixes them, and the reply can't
address anyone by name. The graph was deliberately built with **multiple `User`
nodes** and `SAID`/`KNOWS` edges so speaker IDs could attach later "without a
migration" (`memory_plan.md` §"Proposed graph model", lines 116–133). This plan
supplies the missing recognizer and threads a `speaker_id` through the whole
turn.

**Chosen approach (owner-selected): passive + auto-cluster, local model.**

- *Passive* — a speaker embedding is computed from the utterance PCM the
  Anamanti Core already buffers; no "say your name first" friction.
- *Auto-cluster* — an utterance that matches no known profile mints a new
  anonymous persona (`Speaker 2`) on the spot. Personas are named later by voice
  ("my name is Dana") or from the settings screen.
- *Local model* — a small ECAPA-TDNN ONNX speaker-embedding model runs
  in-Anamanti Core. **Accuracy note:** locality is not the accuracy lever — open
  models (ECAPA-TDNN / WeSpeaker) match or beat commercial cloud speaker-ID APIs
  on VoxCeleb (EER ~1%); the real limiter is the Echo Show's noisy far-field
  audio, which degrades cloud and local equally. Local wins on privacy (raw voice
  never leaves the LAN — stronger than the existing text-embedding calls) and adds
  no per-turn network hop.

---

## 2. Design overview

One new subsystem (`anamanti-core/src/speaker/`) plus a `speaker_id` threaded through the
existing turn path:

```
device PCM ──▶ stream_to_transcript (buffers voiced PCM, already RMS-gated)
                     │  end-of-speech
                     ▼
          SpeakerEmbedder.embed(utterance_pcm) ──▶ 192-d L2-normed vector
                     │
                     ▼
          SpeakerRegistry.identify(vector)
                     │   ┌───────────────────────────────────────────┐
                     │   │ best cosine ≥ MATCH  → known speaker,       │
                     │   │                        update centroid      │
                     │   │ NEW ≤ best < MATCH   → tentative match,     │
                     │   │                        no centroid update   │
                     │   │ best < NEW           → new anonymous persona│
                     │   │ speech < MIN_MS      → `household` (unknown) │
                     │   └───────────────────────────────────────────┘
                     ▼
          SpeakerContext { speaker_id, name, is_new, confidence }
                     │
        ┌────────────┼─────────────────────────────┬──────────────────┐
        ▼            ▼                             ▼                  ▼
  system prompt   memory writes              recall scope        chat-log +
  ("talking to    (tagged speaker_id)     (this speaker first,   GraphRAG ingest
   Sam")                                    + shared facts)       (per-User node)
```

Identification runs **after** end-of-speech and **before** the LLM call. On the
M4 an ECAPA forward pass over a ~2 s utterance is tens of ms — negligible next to
STT+LLM. It is *not* on the streaming hot path.

### Key thresholds (env-tunable, calibrated in Phase E)

| Constant | Default | Meaning |
| --- | --- | --- |
| `MATCH` | 0.55 cosine | ≥ ⇒ confident match; update the profile centroid |
| `NEW` | 0.40 cosine | < ⇒ mint a new anonymous persona |
| `MIN_SPEECH_MS` | 1200 ms | below this much *voiced* audio ⇒ don't identify (→ `household`) |

Between `NEW` and `MATCH` we attribute to the best match for *this turn's context*
but do **not** mutate the stored centroid (avoids poisoning a profile with a
borderline sample). Defaults are placeholders; Phase E calibrates on real
far-field captures.

---

## 3. Data model

### 3.1 Speaker registry (SQLite)

New table in the existing memory DB (`ANAMANTI_DB_PATH`) so profiles and memories
share one file and one `Mutex<Connection>` discipline:

```sql
CREATE TABLE IF NOT EXISTS speakers (
    id          TEXT PRIMARY KEY,   -- 'spk-<short-uuid>'; stable, used everywhere
    name        TEXT,               -- NULL until named ("Speaker N" shown in UI)
    labeled     INTEGER NOT NULL,   -- 0 = auto-created anon, 1 = user-named
    centroid    BLOB NOT NULL,      -- little-endian f32[dims], running L2-normed mean
    dims        INTEGER NOT NULL,   -- guards against a model/dims change
    samples     INTEGER NOT NULL,   -- centroid support count (online mean weight)
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
```

Centroid update is an online mean over L2-normalized embeddings, renormalized:
`c' = normalize((c*samples + e) / (samples+1))`, `samples += 1`. Cheap, no vector
index needed — household speaker counts are tiny (single digits), so
`identify` is a linear scan comparing cosine against every centroid.

`household` stays a reserved sentinel `speaker_id` (not a row) for
unattributed/too-short turns, preserving today's behavior as the floor.

### 3.2 Per-person memory (SQLite `memories`)

Add a nullable column; NULL == shared/household (all existing rows migrate to
shared, so nothing is lost):

```sql
ALTER TABLE memories ADD COLUMN speaker_id TEXT;   -- NULL = shared/household
```

Run this idempotently in `from_conn` (guard with a `PRAGMA table_info` check or a
`CREATE TABLE`-with-column path for fresh DBs). `add`, `search`, and the
forget/list helpers gain a speaker scope (see §4.2).

### 3.3 GraphRAG (HelixDB)

No schema migration — the graph already allows multiple `User` nodes. Replace the
single `household_id` with a per-`speaker_id` lookup:

- `ensure_user(speaker_id, name)` — get-or-create a `User` node keyed by
  `speaker_id`, carrying `name` as a property (updatable on rename).
- `SAID` / `KNOWS` edges originate from that speaker's `User` node.
- The `household` sentinel maps to today's shared `User` node, so mixed
  attributed/unattributed histories coexist.

### 3.4 Chat log (JSONL)

Add two fields to `ChatLogRecord` (both `#[serde(default)]` so old logs still
parse):

```rust
#[serde(default)] pub speaker_id: String,          // "" or "household" for legacy
#[serde(default)] pub speaker_name: Option<String>,
```

---

## 4. Component-by-component build

### 4.1 New module `anamanti-core/src/speaker/` (Phase A)

- **`mod.rs`** — `SpeakerContext { speaker_id: String, name: Option<String>,
  is_new: bool, confidence: f32 }`, re-exports, and an `Arc`-shareable
  `SpeakerService` façade (`identify_and_attribute(&[i16]) -> SpeakerContext`)
  that owns the embedder + registry.
- **`embed.rs`** — `trait SpeakerEmbedder: Send + Sync { fn embed(&self,
  pcm_16k_mono: &[i16]) -> Result<Vec<f32>>; fn dims(&self) -> usize; }`.
  - `OnnxSpeakerEmbedder` — loads an ECAPA-TDNN `.onnx` via `ort` (onnxruntime;
    the engine has CoreML EP on the M4) or `tract` for a pure-Rust path. Does
    fbank feature extraction (80-dim log-mel) → forward → L2-normalize. Feature-
    gate the heavy dep as `speaker` (mirrors the `helix` feature so lean builds
    skip it).
  - `MockSpeakerEmbedder` — deterministic vector derived from coarse PCM stats
    (e.g. framed energy profile hashed into a fixed vector) so integration tests
    get **stable, separable** embeddings for two synthetic "voices" with no model
    or network. This is the workhorse for all Phase A–D tests.
- **`registry.rs`** — `SpeakerRegistry` over the shared SQLite connection:
  `identify(&[f32]) -> Option<(String, f32)>`, `create_cluster(&[f32]) ->
  String`, `update_centroid(&id, &[f32])`, `rename(&id, name)`, `list() ->
  Vec<SpeakerProfile>`, `delete(&id)`, `merge(keep, drop)` (fold centroids +
  reassign rows; used when the user names two clusters that are really one
  person). All synchronous, `Mutex`-guarded, matching `MemoryStore`.

Unit tests (Phase A): centroid online-mean stays L2-normed; two separable mock
voices identify distinctly and cluster stably across repeated turns; a third
voice mints a new persona; sub-`MIN_SPEECH_MS` input yields no cluster.

### 4.2 `anamanti-core/src/memory/` — scope by speaker (Phase B)

- **`mod.rs`** — schema migration (§3.2); `add(kind, content, source,
  speaker_id: Option<&str>)`; `search(query, speaker_id: Option<&str>, limit)`
  returns *this speaker's* rows plus shared (`speaker_id IS NULL`) rows, ranked
  by FTS; keep an all-speakers `list()` for the settings view and add a
  speaker-filtered variant for "what do you know about me". Dedupe key becomes
  `(kind, content, speaker_id)`.
- **`backend.rs`** — `Recall::recall(&self, transcript, speaker_id: Option<&str>,
  limit)`. `SqliteRecall` passes the scope through to `search`. `HelixRecall`
  forwards `speaker_id` to `HelixMemory::recall` (§4.4). *Signature change* —
  update the one caller (`orchestrator.rs::build_context`) and the two impls.

Tests: Sam-scoped recall returns Sam's "likes jazz" and shared facts but not
Dana's "hates jazz"; NULL/`household` scope preserves today's behavior.

### 4.3 `anamanti-core/src/orchestrator.rs` — thread identity through the turn (Phase B)

- **Buffer the utterance.** In `stream_to_transcript`, accumulate the voiced PCM
  (chunks already RMS-gated as `speech`) into a `Vec<i16>` and track voiced-ms.
  Return `(transcript, voiced_pcm, voiced_ms)` (widen the `Option<String>` return
  to a small struct).
- **Identify.** After `Transcript` and before `generate_reply`, if the speaker
  service is present and `voiced_ms >= MIN_SPEECH_MS`, call
  `SpeakerService::identify_and_attribute` → `SpeakerContext`; else
  `SpeakerContext::household()`. Emit a new `TurnEvent::Speaker(SpeakerContext)`
  for logging/UI.
- **Prompt.** In `generate_reply`, prepend an identity line to the system prompt:
  `"You are speaking with {name}."` (named) / `"You are speaking with a
  household member you haven't been introduced to yet."` (anon/`household`), then
  the existing `"What you remember about this user:\n{context}"` — now
  speaker-scoped.
- **Writes.** `apply_command` / `infer_memories` writes pass `Some(speaker_id)`
  (explicit "remember" and inferred facts attach to the current speaker; a
  `household` turn writes shared as before).
- **Log.** `log_turn` fills `speaker_id` / `speaker_name`.
- **Wiring.** `Pipeline` gains `Option<Arc<SpeakerService>>` via a
  `with_speaker(service)` builder (same pattern as `with_recall` /
  `with_chatlog`); `None` ⇒ current household behavior, so every existing test is
  untouched.

Tests: a two-voice mock turn sequence attributes each turn correctly; the system
prompt names a known speaker; a too-short turn falls back to `household`.

### 4.4 `anamanti-core/src/memory/helix.rs` + `ingester.rs` — per-User graph (Phase D)

- `helix.rs`: drop the single `household_id`; add `ensure_user(speaker_id,
  name)` (get-or-create by `speaker_id`, set/refresh `name`). `ingest_turn` and
  `ingest_memory` take `speaker_id` (+ optional `name`) and originate
  `SAID`/`KNOWS`/`ABOUT`-provenance edges from that user. `recall(query,
  speaker_id, k, max)` — bias the graph hop toward the speaker's `SAID` turns and
  `KNOWS` entities (union with the global vector KNN so cross-person facts still
  surface when relevant).
- `ingester.rs`: read `speaker_id`/`speaker_name` off each `ChatLogRecord` and
  pass them to the Helix upserts. `ext_id` idempotency is unchanged.

Tests (real embedded engine, mock embedder/extractor): two speakers' turns create
two `User` nodes; speaker-scoped recall prefers the right person's subgraph.

### 4.5 Naming: voice + control protocol (Phase C)

**Voice** (`memory/extract.rs`): add `MemoryCommand::NameSpeaker(String)` parsed
from lead phrases already adjacent to the existing rules — `"my name is "`,
`"i'm "`, `"i am "`, `"call me "`, `"this is "` (guard `this is` against
non-name tails). The Anamanti Core applies it by renaming the *current* turn's
`speaker_id` (and, if that speaker was `household`/anon, promotes the cluster to
`labeled`), replying "Nice to meet you, {name}." Also handle "who am I?" →
answer from the profile. Keep the existing `infer_memories` "my name is …" fact
capture — it still records the fact; the new command additionally sets the
profile name.

**Control frames** (`wyoming/protocol.rs` `types`, byte-identical in the device
crate `anamanti-display/rust/src/wyoming/protocol.rs` — the round-trip tests in both crates are
the guardrail):

- `anamanti-list-speakers` → `anamanti-speakers` (`{ ok, speakers: [{ id, name,
  labeled, samples, created_at }] }`)
- `anamanti-name-speaker` (`{ id, name }`) → reuse `anamanti-memory-result`-style
  `{ ok, message }`
- `anamanti-merge-speakers` (`{ keep, drop }`) and `anamanti-delete-speaker`
  (`{ id }`) → same result shape

`control.rs`: extend `is_control_request` + `respond` with these arms, taking the
`SpeakerRegistry` alongside `memory`/`settings` (thread it through
`handle_control` and `Pipeline::speaker()`). Pure-function `respond` stays
unit-testable.

**Device UI** (Phase C tail, `/rust` + Flutter): FRB functions in
`anamanti-display/rust/src/api/settings.rs` (`list_speakers`, `name_speaker`, `merge_speakers`,
`delete_speaker`) mirroring the existing memory settings calls, and a "People"
section in the Flutter settings screen showing named people + anonymous
`Speaker N` chips the user can rename or merge. This is the only cross-device
piece; it can land after the Mac-side A–D are green.

### 4.6 Config + wiring (Phase B/E)

`config.rs` + `main.rs`:

- `ANAMANTI_SPEAKER_ID=on|off` (default `off`) — gates building the
  `SpeakerService` and calling `Pipeline::with_speaker`.
- `ANAMANTI_SPEAKER_MODEL_PATH` — path to the ECAPA `.onnx`.
- `ANAMANTI_SPEAKER_MATCH_THRESHOLD`, `ANAMANTI_SPEAKER_NEW_THRESHOLD`,
  `ANAMANTI_SPEAKER_MIN_SPEECH_MS` — the §2 thresholds.
- `main.rs` builds `OnnxSpeakerEmbedder` (or logs and stays household if the model
  is missing — graceful degrade, mirroring the SQLite fallback when
  `OPENAI_API_KEY` is absent), opens the registry on the shared DB, and injects
  the service.

---

## 5. Phasing (each phase independently landable + tested)

- **Phase A — recognizer core.** `speaker/{mod,embed,registry}.rs` with the
  `MockSpeakerEmbedder` + SQLite registry. No pipeline changes. *Fully testable
  offline.*
- **Phase B — per-person SQLite path.** Memory schema + scoped `add`/`search`,
  `Recall` signature, Anamanti Core threading, system-prompt identity line,
  config gate. End-to-end with the mock embedder: two voices get separate memory
  + named replies. **This is the MVP that delivers "better context around
  answers."**
- **Phase C — naming.** Voice `NameSpeaker` + `who am I` + the speaker control
  frames (Mac side); then FRB + Flutter "People" UI.
- **Phase D — GraphRAG per-User.** Helix `ensure_user` + speaker-scoped
  ingest/recall + chat-log fields. Only affects `ANAMANTI_MEMORY_BACKEND=helix`.
- **Phase E — real model + calibration.** Ship the ECAPA `.onnx`, wire
  `OnnxSpeakerEmbedder`, calibrate `MATCH`/`NEW`/`MIN_SPEECH_MS` against real
  Echo Show far-field captures; document EER at the chosen operating point.

---

## 6. Testing strategy (matches the repo's mock-first discipline)

- **Deterministic mock embedder** — every A–D test runs with no model/network,
  producing separable, repeatable vectors for synthetic voices (same philosophy
  as `MockEmbedder`/`MockEntityExtractor`).
- **Registry unit tests** — identify/cluster/rename/merge/threshold behavior,
  online-mean invariants.
- **Memory scope tests** — per-speaker vs shared recall; legacy NULL rows.
- **Anamanti Core integration** — scripted two-voice turn sequences through the
  in-memory mock STT/TTS (`ServiceConnector`) asserting attribution, prompt
  identity line, and per-person recall.
- **Protocol round-trip** — new `anamanti-*speaker*` frames encode/decode
  identically in the Mac and device crates (the existing drift guardrail).
- **Helix integration** — real embedded engine, two `User` nodes, scoped recall.
- **Graceful-degrade** — `ANAMANTI_SPEAKER_ID=off` and "model missing" both
  reproduce exact pre-change household behavior (regression floor).

---

## 7. Risks & mitigations

- **Far-field accuracy / false merges.** Conservative dual thresholds (never
  update a centroid on a borderline match), `MIN_SPEECH_MS` floor, and
  user-facing merge/rename to correct mistakes. Calibrate in Phase E.
- **Cold start.** First-ever utterance always mints `Speaker 1`; the naming UX
  (voice + settings) turns anon clusters into people. No accuracy claim before a
  profile has a few samples.
- **Multiple speakers in one utterance (diarization).** Out of scope v1 — the
  whole turn is attributed to the single utterance-level embedding. Documented as
  a deferred cut; the schema doesn't preclude adding per-segment diarization
  later.
- **Latency.** Identification is post-STT and tiny on the M4; if ever a concern,
  it can run concurrently with the LLM's first token.
- **Privacy.** Voiceprints (centroids) never leave the Mac; they're derived
  vectors, not audio, and are user-deletable via the People UI and
  `anamanti-delete-speaker`. Raw utterance PCM is not persisted for speaker ID.
- **Barge-in / TTS self-trigger.** Short/low-SNR self-triggers fall under
  `MIN_SPEECH_MS` → `household`, so they don't pollute clusters.

---

## 8. Deferred (safe to pick up later)

1. **Diarization** — multiple speakers per turn / per session.
2. **Cross-session speaker linking** — merge suggestions from centroid proximity
   ("Speaker 3 sounds like Dana — merge?").
3. **Voiceprint re-enrollment / drift handling** as voices age or mic changes.
4. **Retention** of stale anon clusters that were never named.
5. **On-device (Echo Show) speaker ID** — kept on the Mac in v1 for one model and
   one registry; the device already streams the PCM the Mac needs.

---

## 9. Files touched (summary)

**New:** `anamanti-core/src/speaker/mod.rs`, `anamanti-core/src/speaker/embed.rs`,
`anamanti-core/src/speaker/registry.rs`, an ECAPA `.onnx` asset (Phase E).

**Changed (Mac):** `orchestrator.rs` (buffer PCM, identify, prompt, thread id),
`memory/mod.rs` (schema + scoped add/search), `memory/backend.rs` (`Recall`
signature), `memory/chatlog.rs` (speaker fields), `memory/ingester.rs` (pass
through), `memory/helix.rs` (per-`User` nodes + scoped recall),
`memory/extract.rs` (`NameSpeaker`), `control.rs` + `wyoming/protocol.rs`
(speaker control frames), `config.rs` + `main.rs` (env vars + wiring + graceful
degrade), `lib.rs` (module export).

**Changed (device, Phase C):** `anamanti-display/rust/src/wyoming/protocol.rs` (byte-identical
frame types), `anamanti-display/rust/src/api/settings.rs` (FRB speaker functions), Flutter
settings screen ("People" section).

**Docs:** `architecture.md` (§2.3 memory, §7 decisions), `memory_plan.md`
(retire Deferred #2), `Plan.MD` if a phase entry is warranted.
