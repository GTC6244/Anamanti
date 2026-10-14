# Speaker-Scoped Memory Plan — "who am I" must depend on *who is speaking*

> Status: **PLAN** (2026-10-14). Builds on [`speaker_id_plan.md`](./speaker_id_plan.md)
> (voiceprint identification + per-person memory) and
> [`SpeakerEnrollmentPlan.md`](./SpeakerEnrollmentPlan.md) (guided enrollment). Touches
> the Core only, apart from optional display polish in Phase 6. No wire-protocol break.
>
> **Revised 2026-10-14 after PR #130** (enroll an existing household member + "Add
> voice samples", shipped in device 1.0.17+18). See §4a for what that PR changes here.
> **Owner decisions recorded 2026-10-14** (§11, D1–D8). No open questions remain.

## 0. The problem

Some facts are only true **relative to the speaker**. "I am Brendan" is true when Brendan
says it and false when Kristina says it. Today the Core stores and recalls such facts as
if they were true for **everyone**, so Brendan and Kristina both get "Brendan" when they
ask "who am I?".

### What production actually shows (read-only inspection, 2026-10-14)

`anamanti_memory.sqlite` → `memories`:

| id | scope (`speaker_id`) | content |
|----|----------------------|---------|
| 1 | `NULL` | The user likes the words from songs |
| 2 | **`NULL`** | **The user's name is Brendan** |
| 3 | `spk-1791991005085-1` | The user's name is Christina |

`anamanti_chatlog.jsonl` (abridged):

```
13:22 household            "My name is Brendan."   → command → stored with speaker_id NULL
15:16 spk-…5085-1 (new)    "Ooh, how are you today?" → "Hey Brendan! …"     ← wrong person
15:17 spk-…5085-1          "My name is Christina." → cluster renamed Christina
15:17 household            "What's my name?"       → "Your name is Brendan!" ← wrong
```

`speakers` registry: **7 clusters, every one with `samples = 1`**. Only Christina is
labelled, and Brendan has no voiceprint at all. Christina matched her own cluster at
**0.44**, which is in the tentative band (`new_threshold 0.40 ≤ x < match 0.55`), so her
centroid never updated.

Household roster (`anamanti_settings.json`): **Brendon** (Father), **Kristina**
(Mother), Isabelle Adia, Mathew. STT heard the spoken names as "Brendan" and
"Christina", so neither spoken name matches the roster. The prompt said "You are
speaking with Christina." rather than reconciling to Kristina (Mother), and the
"Christina" voiceprint label is a spelling no household member has.

### Root causes (in the code)

1. **"Unknown speaker" and "shared household" are the same value.** In SQLite,
   `speaker_id = NULL` means "shared/household, visible to everyone"
   (`memory/mod.rs`, `search_scoped`). In Helix, the sentinel `"household"` is always
   in scope (`helix.rs::in_scope`). An unattributed turn gets
   `SpeakerContext::household()` (`speaker/mod.rs`), and `speaker_scope()` turns that
   into `None`. So **anything said by someone we couldn't identify becomes a fact about
   everyone.** Row 2 is exactly this.
2. **Short questions can't be identified.** `min_speech_samples` is 1200 ms
   (`SpeakerThresholds::default`). "Who am I?" and "What's my name?" are well under
   that, so they always resolve to `household`, and `household` can see the
   unattributed "Brendan" fact. The question that most needs a voiceprint is the one
   that never gets one.
3. **The person's name comes from memory text, not the voiceprint.**
   `speaker_identity_line` uses the registry name when it has one. When it doesn't, the
   prompt has no name, but recall still injects "The user's name is Brendan" under
   *"What you remember about this person"*, and the model believes it.
4. **Follow-up history is not speaker-scoped.** `recent_history()` is recency-only
   (chat-log tail), so one person's earlier turns can feed another person's prompt.
5. **Forget paths leak across people.** `forget_matching` / `forget_last` are
   unscoped, so Kristina can forget Brendan's facts. Forgetting a voiceprint
   (`unassign_speaker`, Helix `delete_speaker → HOUSEHOLD`) **promotes that person's
   private facts to household**, which is a privacy regression.
6. **System-1 is speaker-blind.** `DecisionRequest` carries `transcript`, `screen`
   and `timers` only, so no fast-path decision can depend on who is asking.
7. **Identification quality.** Clusters fragment (samples = 1 everywhere), and nobody
   has enrolled through the guided People flow. Even with 1–6 fixed, most turns would
   correctly come back as "unknown" until the household enrolls and the thresholds are
   calibrated (TODO.md §5 / 6b).
8. **Spoken names label voiceprints.** `MemoryCommand::NameSpeaker` →
   `registry.rename()` sets `labeled = 1` from an STT transcript, misspellings and
   all, without checking the roster. Since PR #130, enrollment **folds samples into
   any existing profile with the same name** (`find_by_name` → `absorb`). A
   spoken-name label can therefore capture a later enrollment: if Kristina's
   anonymous cluster had been spoken-labelled "Brendon", Brendon's guided enrollment
   would fold his samples into *her* voiceprint.

## 1. Goals / non-goals

**Goals**
- A speaker-relative fact is stored against **the person whose voiceprint said it**.
  If the voiceprint can't be determined, it is stored as **Unknown speaker**, and
  Unknown-speaker facts are **never** presented as true for anyone.
- Prompt assembly presents "who you are talking to" and "what you know about them"
  **only from the voiceprint resolution**. Household-wide facts are presented
  separately and labelled as true for everyone.
- "Who am I?" / "What's my name?" is answered correctly per voice, and an
  unrecognised voice gets an honest "I don't recognise your voice yet."
- System-1 receives the speaker resolution so any personal fast intent can use it.
- Unknown-speaker facts can be reviewed later and **attributed** to the right
  person (or promoted to household) from the admin console.
- Every memory and every conversation turn in the Core UI is **tagged with the
  identified person** (live name, not a raw id), along with how confident the voice
  match was (§8).

**Non-goals**
- Diarization of multiple speakers inside one utterance (TODO §6c, still deferred).
- Better embeddings or a new model. Calibration is a prerequisite (Phase 0), not
  part of this plan's code.
- Authentication or security. A voiceprint is a personalisation signal, not a
  password, and nothing here gates sensitive actions on it.

## 2. Core model: three memory scopes, four speaker resolutions

### 2.1 Memory scope (where a fact lives / who it is true for)

| Scope | Meaning | Recalled for |
|-------|---------|--------------|
| `person(spk-…)` | True for one identified person | That person only |
| `household` | True for everyone in the home ("the wifi is FastNet", "we live in Cambridge") | Everyone, including unknown voices |
| `unknown` | Said by someone we could not identify; the fact is **speaker-relative** | **No one** (admin review queue only) |

The key change is that `household` becomes a **deliberate** scope, never a fallback.
`unknown` is the new fallback.

### 2.2 Speaker resolution (what the voiceprint said this turn)

Extend `SpeakerContext` with a `resolution` (replacing the `is_household()`
sentinel test):

| Resolution | When | Recall (read) | Write personal facts | Rename cluster via "my name is" |
|------------|------|---------------|----------------------|--------------------------------|
| `Identified` | cosine ≥ `match_threshold` | person + household | → person | only if the cluster is unlabelled |
| `Tentative` | `new_threshold ≤ cos < match` | person + household | → **unknown**, tagged `candidate = spk-…` | no (ask the user to enroll) |
| `NewCluster` | below `new_threshold`, long enough to create | household only | → **unknown**, tagged `candidate = new spk-…` | no (single-sample cluster) |
| `Unknown` | too short, identification off or failed | household only | → **unknown** | no |

The `candidate` tag preserves the best guess, so the admin review in Phase 5 can
offer one-click attribution. Recall for `Tentative` is allowed because reading the
wrong person's music preference is low-harm. Writes must be conservative because a
wrong write pollutes the store. That asymmetry is the main policy choice (decision
D1).

`SpeakerContext::household()` is renamed to `SpeakerContext::unknown()`, and the
`HOUSEHOLD_SPEAKER` sentinel becomes `UNKNOWN_SPEAKER = "unknown"` for *speakers*.
`"household"` survives only as a *memory scope*.

### 2.3 Which scope does a new fact get?

Classify every write (explicit "remember…", inferred `infer_memories`, LLM
entity-extraction `Memory` nodes) as speaker-relative or household:

- **Speaker-relative (default):** first-person singular ("I", "my", "me", "I'm"),
  or anything rendered as "The user …" (every `infer_memories` template is). These go
  to `person` or `unknown` per the resolution table.
- **Household:** first-person plural or impersonal home facts ("our…", "we…", "the
  house…", "the wifi…", "the kids' school is…"). Explicit "remember that **we**…" /
  "remember for **everyone**…" also goes here. These are written as `household`
  regardless of resolution.
- **Ambiguous → speaker-relative.** It is safer to under-share than to tell Kristina
  she is Brendan.

Implemented as a small pure function `memory::classify_scope(content, transcript) ->
FactScope { Personal, Household }` with table-driven tests. Entity extraction
(`memory/extract.rs` / `entity.rs`) gets the same field in its JSON schema
(`"about": "speaker" | "household"`) so the extractor model labels it, with the
heuristic as a fallback.

## 3. Storage changes

### 3.1 SQLite `memories`

Add an explicit scope column instead of overloading `NULL`:

```sql
ALTER TABLE memories ADD COLUMN scope TEXT NOT NULL DEFAULT 'household';  -- person|household|unknown
ALTER TABLE memories ADD COLUMN candidate_speaker_id TEXT;                -- best guess for unknown rows
-- invariant: scope='person' ⇔ speaker_id IS NOT NULL
-- provenance (who said it, how sure we were) — drives the Core UI tagging in §8:
ALTER TABLE memories ADD COLUMN said_by_speaker_id TEXT;   -- the turn's resolved speaker (even for household-scope facts)
ALTER TABLE memories ADD COLUMN speaker_resolution TEXT;   -- identified|tentative|new|unknown at write time
ALTER TABLE memories ADD COLUMN speaker_confidence REAL;   -- cosine at write time
ALTER TABLE memories ADD COLUMN turn_id TEXT;              -- links the fact to its /conversations turn
```

`scope`/`speaker_id` answer *who the fact is true for*. The provenance columns answer
*who said it*. These differ for a household fact ("the wifi is FastNet", said by
Brendan) and for an unknown fact (said by a voice we couldn't confirm, best guess
Brendan). Legacy rows get `NULL` provenance, which the UI shows as "not recorded".

- `add_scoped(kind, content, source, MemoryScope)` where
  `enum MemoryScope { Person(String), Household, Unknown { candidate: Option<String> } }`.
  Dedupe stays per (kind, content, scope, speaker_id).
- `search_scoped(query, &SpeakerContext)` → `person(id) ∪ household` for
  Identified/Tentative, `household` only otherwise. **`unknown` is never returned by
  recall.**
- `forget_matching` / `forget_last` take the speaker. They only touch the speaker's
  own rows plus `unknown` rows they created this session. Household rows can only be
  forgotten when the request explicitly says so ("forget that **we**…") or from the
  admin console. Unknown speakers can only forget `unknown` rows from the current
  follow-up chain.
- `unassign_speaker` (voiceprint forgotten) → moves rows to **`unknown`**, not
  household.
- New: `list_unknown()`, `attribute(id, MemoryScope)` for the review queue.

**Migration (idempotent, in `from_conn`):** add the columns. Then classify every
legacy `speaker_id IS NULL` row with `classify_scope`: speaker-relative → `unknown`,
household-ish → `household`. Rows with a `speaker_id` → `person`. In production this
turns row 2 ("The user's name is Brendan") and row 1 ("The user likes the words from
songs") into `unknown`, which fixes Kristina's "who am I" immediately after deploy.

### 3.2 HelixDB graph (`memory/helix.rs`, `ingester.rs`)

- Add an `UNKNOWN = "unknown"` `User` sentinel next to `HOUSEHOLD`.
  `ensure_user(UNKNOWN)` at open.
- **`Turn` nodes are always speaker-relative** because they are raw transcripts. An
  unidentified turn is attributed to `unknown`, never `household`. The ingester maps
  the new `speaker_id = "unknown"`, **and legacy `"household"`/`""` chat-log
  records**, to `unknown`. Every pre-change `household` record was unattributed, not
  deliberately shared.
- `Memory` nodes carry the `scope` from §2.3.
- `in_scope(item, want)` becomes: `sid == want || sid == HOUSEHOLD` for a person, and
  `sid == HOUSEHOLD` for unknown/new. **`sid == UNKNOWN` is never in scope.**
- `delete_speaker(id)` → `reassign_speaker(id, UNKNOWN)` (privacy fix, mirrors SQLite).
- **One-time graph migration** (behind the same `helix_path/<label>-<dims>/` store
  versioning, run per backend store): rewrite `speaker_id = "household"` on existing
  `Turn` nodes to `unknown`, and re-point `SAID` edges. `reassign_speaker` currently
  refuses `from == HOUSEHOLD`, so add a dedicated `migrate_household_turns_to_unknown()`
  guarded by a marker file in the store dir. Re-ingesting from the chat log is an
  acceptable alternative because each store is rebuildable. Decide in implementation.
- `graphrag_control.rs:583` (`HOUSEHOLD_SPEAKER => delete_speaker`) is updated to the
  new sentinel.

### 3.3 Chat log / prompt log

`ChatLogRecord.speaker_id` gets `"unknown"` for unattributed turns. Add
`speaker_resolution` (`identified|tentative|new|unknown`) and `speaker_confidence`
(both `#[serde(default)]`), so `/conversations` can show *why* a turn was attributed
the way it was. Readers treat legacy `"household"` as `"unknown"`.

## 4. Identification changes (make short questions identifiable)

The goal is to get a resolution for "who am I?" without letting short clips create or
corrupt clusters.

1. **Identify-only floor for short utterances.** Keep `min_speech_ms` (1200) as the
   floor for *creating or updating* clusters. Add `speaker.min_identify_ms` (default
   ~600) for a **read-only match** via the existing `SpeakerService::identify_only`.
   It never creates a cluster, never updates a centroid, and never saves a clip. Below
   that floor → `Unknown`. Short-clip matches cap at `Tentative` unless they clear
   `match_threshold + short_clip_margin` (env-tunable, calibrated in Phase 0).
2. **Follow-up continuity.** In a follow-up chain on the same device connection
   (`ctx.followup_depth > 0`), a too-short turn inherits the previous turn's
   `Identified` speaker as `Tentative`, so it can read but not write. The inheritance
   expires at the end of the chain. It never upgrades `Unknown` to anything.
3. **No identity from text.** Guided enrollment (now roster-linked, §4a) is the way
   a voiceprint gets a name. "My name is X" by voice:
   - from a `Tentative`, `NewCluster` or `Unknown` speaker: no rename. It stores
     `"The user's name is X"` as `unknown` (candidate tagged) and replies along the
     lines of *"Nice to meet you, X. I don't know your voice yet. On the display, go
     to Settings → People → Add a voice and pick your name so I'll recognise you."*
   - from an `Identified` cluster with **no** name: the name is first reconciled
     against the roster (`member_matching`, plus a fuzzy/phonetic match so
     "Christina" → Kristina and "Brendan" → Brendon). The profile is labelled with the
     **canonical roster spelling** and `name_source = spoken`. It is never labelled
     with a raw transcript spelling that no member has. With no roster match it
     stays as an unknown fact, and the reply suggests enrolling.
   - from an `Identified` cluster that **already has** a name: no rename. If the name
     differs, the reply says *"I know your voice as Kristina"*, and a correction
     goes through People/admin.
   - A name already held by another profile is **never** reassigned. The user is
     told, and a merge suggestion is logged.

### 4a. Interaction with PR #130 (roster-linked enrollment + "Add voice samples")

PR #130 (merged 2026-10-14, device 1.0.17+18) added:
- **Enroll an existing household member**: pick a roster chip, and the voiceprint is
  saved under the member's **canonical** name. A new name is auto-added to the roster
  (`ensure_household_member`). If a profile already carries that name, the samples
  are **folded into it** (`find_by_name` + `absorb`) instead of creating a duplicate.
- **Add voice samples** (People ⋮): `enroll_into(session, speaker_id)` folds samples
  into a specific profile by id. The name is kept, and an anonymous cluster stays
  anonymous.

**What this changes in this plan:**

1. **Phase 0 is now a short runbook, not a gap.** Both people can enroll against
   their roster entry, so registry names equal roster names by construction. The
   identity line in §5 then always reconciles (relationship included), and the
   "name from registry, not memory text" rule in §5 is fully reliable for enrolled
   people. See the updated Phase 0 row in §9 for the concrete steps for this
   household (Christina → Kristina cleanup first).
2. **Name provenance is needed (new).** Fold-by-name makes a spoken-name label
   dangerous (root cause 8). Add `speakers.name_source TEXT`
   (`enrolled | admin | spoken`, legacy rows → `spoken`):
   - `enroll_finish` sets `enrolled`. A rename from People/the admin console sets
     `admin`. Voice "my name is" sets `spoken`.
   - `find_by_name` (the fold target) **only matches `enrolled`/`admin` profiles**. A
     `spoken`-named profile with the same name is not folded into. The enrollment
     creates a fresh profile, and the admin console offers *"merge Speaker
     'Brendon' (spoken) into Brendon (enrolled)?"*, reusing the existing coordinated
     merge (registry + SQLite + Helix + aliases).
   - Resolution: a match against a `spoken`-named profile caps at `Tentative`, so it
     can read but not write personal facts until the person enrolls. This is a
     small extension to the §2.2 table.
3. **Enrollment closes the loop on Unknown facts (new).** When an enrollment or "Add
   voice samples" commits into profile P (`EnrollFinishOutcome.speaker_id`):
   - any `unknown` memory whose `candidate_speaker_id` is P (or a cluster later
     merged into P) is **offered** for attribution in the review queue (§8.3), with
     a "Confirm all for Brendon" bulk action. It is not auto-moved, because a
     candidate is only a guess.
   - the display's enrollment result can carry an additive `pending_unknown: N`, and
     the success screen says *"I also have N things someone told me that might be
     yours. Review them on the Mac."* This is an optional additive field, no new
     frame.
4. **Enrollment and memory share one roster.** Enrollment now guarantees each
   enrolled voice is a household member. §7 ("send it to **me**" → the member
   matching the speaker) and the Core UI chips (§8, relationship from the roster)
   rely on this, and work for every enrolled person without extra mapping.
5. **Unchanged:** passive auto-clustering of unknown voices continues (owner choice).
   Thresholds are still uncalibrated, so the §4 short-clip floor still needs
   calibration. `enroll_into` on an anonymous cluster keeps it anonymous, so §8 still
   needs the "Speaker 3 (unnamed)" chip.

## 5. Prompt assembly (`orchestrator/prompt.rs`)

The prompt is split into clearly separated, resolution-driven blocks:

```
# Who is speaking
<identity line — from the VOICEPRINT REGISTRY name (+ household roster reconcile)>

# About this person            (Identified / Tentative only)
- <person-scoped recalled facts>

# Household facts (true for everyone here)
- <household-scoped recalled facts>
```

- **Identity line by resolution:**
  - `Identified` → today's line ("You are speaking with Brendan (son), who lives here.").
  - `Tentative` → "You are probably speaking with Brendan (voice match is not certain).
    Don't state their identity as fact. If they ask who they are, say you think it's
    Brendan but aren't sure."
  - `NewCluster` / `Unknown` → "You do not know who is speaking; their voice was not
    recognised. Do not guess their identity from the household list or anything else.
    If they ask who they are or what their name is, say you don't recognise their
    voice yet and suggest Settings → People → Add a voice on the display. Do not use any
    one person's personal details or preferences."
- **The name comes from the registry, never from memory text.** Recalled facts that
  assert identity (`"The user's name is …"`) are filtered out of the person block,
  because the registry label is authoritative. This also stops a stale or mis-scoped
  name fact from overriding the voiceprint.
- **Household roster line** stays, but its wording adds "this list is for contacting
  people; never use it to guess who is speaking."
- **Follow-up history** (`recent_history`) keeps **every** turn in the chain so a
  two-person conversation keeps its context (decision D3), but **labels each user
  message with its resolved speaker**: `[Brendon]: …`, `[Kristina (probably)]: …`,
  `[unknown speaker]: …`. The system prompt adds one line: "Earlier messages may be
  from other people in the home, labelled by name. Only the current speaker's own
  statements describe the current speaker; never apply another person's 'I am…' / 'I
  like…' to them." The current speaker's own earlier turns are labelled `[you]`, so
  the model can tell them apart. Labels use the live directory name (§8.1), so a
  later rename or merge is reflected.
- **Deterministic "who am I" fast path.** Add `MemoryCommand::WhoAmI` to
  `parse_command` ("who am I", "what's my name", "do you know who I am", "who's
  talking"). It is answered from the resolution **without the LLM**: Identified →
  "You're Brendan." / Tentative → "I think you're Brendan, but I'm not completely
  sure." / Unknown → "I don't recognise your voice yet…". This makes the headline
  bug deterministic and testable and doesn't rely on model obedience.
- The prompt log records the resolution, so the `/conversations` "matched prompt" view
  shows which blocks were present and why.

## 6. System-1 (`system1/`, `orchestrator/system1_handlers.rs`)

- `DecisionRequest` gains
  `speaker: SpeakerSummary { resolution, name: Option<String> }`. It never carries the
  `spk-…` id or any embedding.
- `http.rs` adds it to the request `state` as `speaker_known: bool`,
  `speaker_resolution`, and `speaker_name` (the canonical household name, `null` when
  unknown) for **every** engine, including the cloud `jev` (OpenRouter) engine
  (decision D4). Never the `spk-…` id, the confidence or any embedding. Update the
  `ui.js` LOCALITY "Sends:" copy for System-1 to include "the speaker's first name",
  so the Overview's "what leaves the house" panel stays honest.
- Handlers get `&SpeakerContext`. Today's intents (weather, timer, stop…) are
  household-neutral and don't change. The point is that the seam exists before any
  personal fast intent ("my calendar", "send this to me") is added. A personal intent
  with `Unknown` resolution must **Defer** to System-2, which will ask who they are.
- `who_am_i` is **not** a System-1 intent. The deterministic command above runs
  first, before System-1 (same position as other memory commands).

## 7. Other consumers of "me"

- **`send_phone_message`** (`appsaid`): "send it to me" resolves to the household
  member matching the `Identified` speaker's name. With `Tentative` it confirms
  ("Send to Brendan's phone?"). With `Unknown` it asks for a name and never falls
  back to `default_recipient` for "me". This needs the speaker passed into the tool
  context (the rig tool layer, `llm/rig/tools/mod.rs`).
- **Calendar / Cadora / anything future that says "my"** follows the same rule:
  personal resolution or ask.

## 8. Speaker tagging in the Core UI (memory + conversations)

Every memory and every turn in the admin console should show **who it belongs to and
how sure we were**, using the person's current name rather than a raw `spk-…` id.

**How the UI shows speakers today:**
- `/memory` (`memory.html`) shows the raw `speaker_id` (`spk-1791991005085-1`), or
  "household" for `NULL`. There is no name, and unknown and shared can't be told apart.
- `/conversations` (`conversations.html` `who()`) shows `speaker_name || speaker_id`
  **as logged at turn time**. A person named *after* a turn still shows as an id
  (Kristina's own "My name is Christina" turn has `speaker_name: null`), and every
  unattributed turn shows "household".
- Speakers merged away keep their old id in the UI, even though recall already
  resolves them through `speaker_aliases`.

### 8.1 Server side: one speaker-tag resolver

Add `webconfig::speakers::SpeakerDirectory`, built once per request from the
`speakers` registry + `speaker_aliases` + the household roster:

```rust
struct SpeakerTag {
    id: Option<String>,          // surviving spk-… after alias resolution; None for household/unknown
    kind: &'static str,          // "person" | "household" | "unknown"
    name: Option<String>,        // live registry label, reconciled to the household roster's canonical spelling
    label: String,               // what to print: "Brendan", "Speaker 3 (unnamed)", "Household", "Unknown speaker"
    relationship: Option<String>,// from the roster, e.g. "son"
    resolution: Option<String>,  // identified|tentative|new|unknown (turns; memory provenance)
    confidence: Option<f32>,
    candidate: Option<Box<SpeakerTag>>, // unknown rows: best-guess person ("maybe Brendan?")
    has_clips: bool,             // enables the play-voice button
}
```

Every JSON endpoint that carries a speaker adds a resolved `speaker` object (and
keeps the raw fields for compatibility):

| Endpoint | Adds |
|----------|------|
| `/sqlite.json` (memory list) | `speaker` (who it's true for: the scope), `said_by` (provenance tag), `turn_id` |
| `/chatlog.json` (conversations) | `speaker` (live name + resolution + confidence), `memories_written[]` each with its scope tag |
| `/prompts.json` | `speaker` |
| `/helix.json` + graph view | `User` nodes labelled with `label`, and `Turn`/`Memory` nodes carry `speaker` |
| new `/speakers.json` | the directory itself (for filter menus + chip colours) |

Names are resolved **live**, so renaming, merging or forgetting a voiceprint updates
every past turn and memory on the next page load without rewriting the append-only
chat log.

### 8.2 Shared front-end component

`ui.js` gains `ui.speakerChip(tag)`, used by every page:
- **Person:** the name, a stable colour per person (hashed from the id, kept in
  `/speakers.json` so it matches across pages), the relationship as a tooltip, and a
  play-voice icon when `has_clips`.
- **Unnamed cluster:** "Speaker 3 (unnamed)" with a link to the People section (the
  existing clip player + rename).
- **Household:** neutral "Household" chip (house icon).
- **Unknown:** a dashed-outline "Unknown speaker" chip. When a candidate exists it
  reads "Unknown · maybe Brendan?".
- **Resolution badge** on turns and provenance: ✓ identified / ~ probable / ★ new
  voice / ? unknown, plus the confidence (e.g. `~ 0.44`) on hover.

### 8.3 Memory page (`/memory`)

- **"For" column** (scope chip: who the fact is true for) and **"Said by" column**
  (provenance chip + resolution badge). For a household fact these differ, and for an
  unknown fact the "For" chip is Unknown while "Said by" shows the candidate.
- **Filters:** by person (names from `/speakers.json`), Household, Unknown, and
  "unreviewed unknown". Replaces today's raw-id `fSpeaker` select.
- **Detail drawer:** both chips, the source turn (link →
  `/conversations#turn=<turn_id>`) with its transcript, and the actions below.
- **Unknown-speaker review queue** (a tab or section, with a count badge in the page
  header): each unknown fact with its transcript, time and candidate, plus actions
  **Attribute to → [person]**, **Make household**, **Delete**. Attribution rewrites
  `scope`/`speaker_id` in SQLite and the matching Helix `Memory` node
  (`POST /memory/attribute`, JSON, loopback-only like every POST).
- **Graph view:** `User` nodes show names instead of ids, and the "unknown" user
  node is visually distinct (dashed).

### 8.4 Conversations page (`/conversations`)

- Each turn row shows the **speaker chip + resolution badge** (live name). This
  replaces `who()`, and the speaker filter uses names with explicit "Unknown speaker"
  / "Household" entries.
- **Turn detail** adds a "Speaker" block: resolution, confidence, the thresholds in
  force (`match`/`new`/`min_identify_ms`), whether identity was inherited from the
  follow-up chain (§4.2), and whether this was the identify-only short-clip path.
- **"Memories written this turn"** lists each fact with its **For** chip, so you can
  see at a glance that "The user's name is Brendan" landed on Brendan, Household or
  Unknown.
- The **matched prompt** view highlights the identity line and the "About this
  person" / "Household facts" blocks that were sent.
- **Correction (where applicable):** on a turn attributed wrongly or to Unknown,
  **"This was → [person]"** reattributes the turn and the memories it wrote. Because
  the chat log is append-only, this is stored as a `turn_attribution_overrides`
  table (`turn_id → speaker_id`) that the directory, the ingester (re-ingest of that
  turn) and `recent_history` all honour. Voiceprints are **never** updated from a
  correction (decision D6). The clip may be short or noisy, and only enrollment and
  "Add voice samples" train voiceprints.

### 8.5 Overview + display

- **Overview (`/`)**: the live pipeline's latest-turn card shows the speaker chip, and
  there is a small "N unknown-speaker memories to review" link when N > 0.
- **Display People screen** (`people_screen.dart`): optional polish showing a
  per-person memory count and an "unattributed memories: N, review on the Mac" hint.
  No new device frames. If anything is added, update `DisplayUI.md`.

## 9. Phasing (each phase lands and is tested independently)

| Phase | Scope | Fixes |
|-------|-------|-------|
| **0. Clean up, enroll, calibrate** (ops, no code; needs device ≥ 1.0.17+18) | (1) Delete the 6 single-sample anonymous clusters (People → forget). (2) Kristina: People → the "Christina" row → rename to **Kristina** *before* enrolling (decision D8), so the PR #130 fold-by-name lands in that profile and memory row 3 stays hers. (3) Brendon and Kristina: Settings → People → **Add a voice** → pick their **roster chip**. Use **Add voice samples** later to strengthen. (4) Measure same-speaker / cross-speaker cosine on far-field turns (`/conversations` resolution + confidence once §8 lands, the Core log until then). Set `match`/`new` and `min_identify_ms`. | Root cause 7 |
| **0b. Name provenance** (small code change, can land before Phase 1) | `speakers.name_source`, `find_by_name` restricted to `enrolled`/`admin`, spoken names reconciled to the roster's canonical spelling, never rename a named profile by voice. | Root cause 8 (protects Phase 0 enrollments from spoken-label capture) |
| **1. Scope model + SQLite** | `MemoryScope`, `scope`/`candidate_speaker_id` + provenance (`said_by_speaker_id`, resolution, confidence, `turn_id`) columns + migration, `classify_scope`, scoped `search`/`forget`, `unassign → unknown`, `SpeakerContext.resolution` + `unknown()` rename. | 1, 5 (SQLite). **Fixes prod "who am I" on deploy** (row 2 → unknown) |
| **2. Prompt + who-am-I fast path** | Resolution-driven identity line, person/household blocks, identity-fact filter, roster-line wording, `MemoryCommand::WhoAmI`, speaker-filtered follow-up history, no rename from non-Identified speakers. | 3, 4 |
| **3. Short-utterance identification** | `min_identify_ms` read-only match, short-clip margin, follow-up continuity (Tentative inheritance). | 2 |
| **4. Helix graph** | `UNKNOWN` user, `in_scope` change, ingester legacy mapping, `delete_speaker → unknown`, one-time `household → unknown` turn migration, extractor `about` field. | 1, 5 (GraphRAG) |
| **5. System-1 + tools** | `DecisionRequest.speaker`, locality-aware request state, handler signature, AppSaid "me" resolution. | 6, §7 |
| **6a. Speaker tagging (read-only)** | `SpeakerDirectory` + `/speakers.json`, resolved `speaker`/`said_by` in `/sqlite.json`, `/chatlog.json`, `/prompts.json`, `/helix.json`, `ui.speakerChip`, For/Said-by columns, name-based filters, turn Speaker block, per-turn memory chips. | Visibility. **Can land right after Phase 1**, since the provenance columns exist from then |
| **6b. Review + correction** | Unknown review queue + `POST /memory/attribute`, `turn_attribution_overrides` + "This was → [person]", Overview review link, optional display polish. | Recovery path for `unknown` + misattribution |

Phases 1 and 2 together are the minimum that makes the reported bug go away. Phase 0
should run in parallel, because without enrollment every turn will (correctly) be
"I don't recognise your voice".

## 10. Testing (mock-first, matches repo discipline)

- `memory`: scope isolation table test (person A / person B / household / unknown ×
  Identified/Tentative/New/Unknown readers). `unknown` is never recalled. Forget is
  scoped. Unassign goes to unknown. The migration classifies the legacy rows
  (`"The user's name is Brendan"` → unknown, `"The house wifi is X"` → household).
- `classify_scope`: table of utterances → Personal/Household.
- `prompt`: identity line per resolution. Identity facts are filtered from the person
  block. The unknown resolution contains the "do not guess" instruction and **no**
  personal block.
- `orchestrator` pipeline tests (mock embedder steering cosine): **the headline
  scenario.** A says "my name is Brendan" (Identified A), B asks "who am I"
  (Identified B, labelled Kristina) → "Kristina". An unknown short clip asking "who am
  I" → "I don't recognise your voice". "My name is X" from Unknown → stored unknown,
  no rename.
- `helix`: `in_scope` excludes unknown. Ingester maps legacy `household` records to
  unknown. The migration is idempotent.
- `system1`: request state contains `speaker_known`, `speaker_resolution` and
  `speaker_name` (null when unknown) for both local and cloud engines, and never the
  `spk-…` id.
- `recent_history`: a mixed two-speaker chain yields every turn, each labelled with
  the right live name, and the current speaker's turns marked `[you]`.
- `webconfig` (`tests.rs`): `SpeakerDirectory` resolves aliases (a merged id shows
  the survivor's name), a later rename shows on earlier turns, legacy `household`
  turns show as Unknown, unknown rows carry their candidate, and `/memory/attribute`
  rejects non-JSON / non-loopback requests and moves both stores. JSON shape tests
  for the new `speaker` / `said_by` objects.
- Hardware (Phase 0 + after 3): both voices, short and long questions, far-field. Log
  the resolution/confidence distribution from `/conversations`.

## 11. Decisions (answered by the owner, 2026-10-14)

| # | Question | Decision |
|---|----------|----------|
| **D1** | Where do personal facts from a `Tentative` (probable) match go? | **Unknown + candidate tag** (conservative), confirmed later in the review queue (§2.2, §8.3). Revisit after Phase 0 calibration. |
| **D2** | Legacy speaker-less (`NULL`) rows | **Auto-classify** with `classify_scope`: speaker-relative → `unknown`, house-wide → `household` (§3.1). Both production rows become `unknown`. |
| **D3** | Follow-up history across speakers | **Include other speakers' turns, labelled with their name** (`[Brendon]: …`, current speaker `[you]`), with a prompt guard against cross-applying "I am / I like" statements (§5). |
| **D4** | Speaker name to cloud System-1 | **Send it to every engine, including cloud `jev`**: `speaker_known`, `speaker_resolution`, canonical `speaker_name`. Never the `spk-…` id, confidence or embedding. The LOCALITY "Sends:" copy is updated (§6). |
| **D5** | Change the meaning of "household" (no speaker ⇒ `unknown`, household is deliberate) | **Approved.** Supersedes `speaker_id_plan.md` §3.2 (`NULL` = shared). Record as a `Plan.MD` decision row when Phase 1 lands. |
| **D6** | Use a corrected turn's clip to train the voiceprint | **No.** Corrections reattribute the turn and its memories only. Guided enrollment and "Add voice samples" are the only ways to train. |
| **D7** | Should "my name is X" still name a voiceprint? | **Yes, restricted** (§4.3): only an `Identified`, unnamed profile, only to a canonical roster name, `name_source = spoken`, never renames an already-named profile. |
| **D8** | Christina cleanup | **Rename the "Christina" voiceprint to Kristina** (before or as part of her enrollment), so memory row 3 stays hers and PR #130's fold-by-name adds her enrollment samples to that profile. |

## 12. Files touched (summary)

- `anamanti-core/src/memory/mod.rs`: `MemoryScope`, columns, migration, scoped
  search/forget, review-queue APIs, `classify_scope` (new `memory/scope.rs`).
- `anamanti-core/src/memory/extract.rs`: `MemoryCommand::WhoAmI`, scope on inferred
  facts. `memory/entity.rs`: `about` field.
- `anamanti-core/src/memory/backend.rs`, `helix.rs`, `ingester.rs`,
  `graphrag_control.rs`: scope-aware recall, `UNKNOWN` user, migration.
- `anamanti-core/src/speaker/mod.rs`: `SpeakerResolution`, `unknown()`, short-clip
  identify-only path, thresholds/config (`config.rs` + `anamanti.example.json`).
- `anamanti-core/src/speaker/registry.rs`: `name_source` column + migration,
  `find_by_name` restricted to enrolled/admin, `rename(…, NameSource)`.
- `anamanti-core/src/control.rs`: `enroll_finish` sets `name_source = enrolled`, and
  the optional `pending_unknown` count on the enroll result.
- `anamanti-core/src/household.rs` / `settings.rs`: fuzzy/phonetic `member_matching`
  variant for spoken names.
- `anamanti-core/src/orchestrator/{mod.rs,prompt.rs,system1_handlers.rs}`:
  resolution plumbing, prompt blocks, who-am-I fast path, history filter, naming rules.
- `anamanti-core/src/system1/{mod.rs,http.rs}`: `DecisionRequest.speaker`.
- `anamanti-core/src/llm/rig/tools/`, `appsaid/`: speaker-aware "me".
- `anamanti-core/src/memory/chatlog.rs`, `promptlog.rs`: resolution fields.
- `anamanti-core/src/webconfig/speakers.rs` (new): `SpeakerDirectory`, `SpeakerTag`,
  `/speakers.json`, `POST /memory/attribute`, turn overrides. Registered in
  `routes.rs`.
- `anamanti-core/src/webconfig/{debug.rs,memory.rs,system.rs}`: resolved `speaker` /
  `said_by` in the JSON endpoints.
- `anamanti-core/src/webconfig/{memory.html,conversations.html,overview.html,ui.js}`
  + `scripts/build-webui-css.sh` rerun: speaker chips, For/Said-by columns, filters,
  review queue, turn Speaker block, correction action, locality copy.
- `anamanti-core/src/memory/mod.rs`: provenance columns, `turn_attribution_overrides`
  table.
- Docs when implemented: `Plan.MD` (decision row), `architecture.md` (memory +
  speaker sections), `README.md`, `TODO.md` §5, `speaker_id_plan.md` (supersede §3.2
  NULL semantics), `agents.md` (feature-plan list), `DisplayUI.md` if the display
  changes.
