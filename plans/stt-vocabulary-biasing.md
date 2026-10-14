# STT Vocabulary Biasing & Pronunciation Normalization

Status: **Mechanism A proposed; Mechanism B implemented** (2026-10-11). Additive, off by
default. Does **not** touch the Stage‑5 cutover in `python-to-rust-whisper.md`, nor the
locked `stt.engine` default (`wyoming`).

> **Implementation note (Mechanism B).** Built as a **live, GUI-editable runtime
> setting** (config page → persisted `anamanti_settings.json` overlay → per-turn
> settings snapshot), mirroring the Personality subsystem — *not* a boot-config
> `stt.pronunciations` block as originally sketched below. This makes the table editable
> from the config page with no restart and keeps `config.rs`'s `stt` block untouched.
> Delivered in: `stt/normalize.rs` (matcher + rules + tests), `settings.rs`
> (`RuntimeSettings`/`PersistedSettings` field + `pronunciations()`/
> `apply_pronunciations()`), `config.rs` (seed/overlay), `webconfig.rs` +
> `webconfig/pronunciations.html` (a JSON-textarea editor at `/pronunciations`), and
> `orchestrator.rs` (applied to the transcript inside `stream_to_transcript`, just before
> the device echo, so there is one corrected source of truth). Normalization runs only
> past the no-speech hallucination guard.

## Problem

Whisper mis-transcribes domain-specific proper nouns — store/brand names, coined
product names, local place names — because it is an end-to-end acoustic→text model
with **no pronunciation lexicon** you can edit (unlike Kaldi/Vosk). Example target:
the grocery chain **“Zehrs”** (pronounced *zay-rz*) comes back as “Zairs”, “Zehr’s”,
“Zares”, “zay rz”, etc.

Two independent, composable levers address this. Ship both; they stack.

## Guardrails (locked decisions — do not relitigate)

- STT stays on the Mac Core; engine default stays `wyoming` (python-to-rust-whisper.md).
- This plan is **additive and off by default**: absent config ⇒ byte-for-byte current
  behavior. No change to VAD, to the anti-hallucination guard, or to Stage‑5 gating.
- `.en` models / English-only decode assumption is unchanged.
- Wake word is out of scope (locked: “no custom training in v1”).

---

## Mechanism A — `initial_prompt` biasing (spelling bias)

Whisper accepts a decoder-context string that biases **spelling/word choice**. You put
the **correctly spelled** terms in it (NOT phonetic notation — a literal `zay-rz` in the
prompt biases Whisper to *emit* that string). Soft, ~224-token cap; over-stuffing hurts.

Example config value:

```
"This conversation may mention store and brand names like Zehrs, Fortinos, and Loblaws."
```

### Engine support

- **`whisper-rs` (in-process):** supported directly via `FullParams::set_initial_prompt`.
  This is the primary target.
- **Wyoming / faster-whisper:** the standard Wyoming `transcribe` event (we send
  `json!({})` in `wyoming/stt.rs`) has **no `initial_prompt` field**; faster-whisper
  supports it on its Python API but the protocol doesn't carry it. **Out of scope for the
  Wyoming path** — document the limitation; biasing applies only when
  `stt.engine = whisper-rs`. (Mechanism B below covers the Wyoming path.)

### Changes

1. **`config.rs`**
   - Add `pub initial_prompt: Option<String>` to `SttConfig` (default `None`).
   - Add `pub initial_prompt: Option<String>` to `FileStt`.
   - In `from_file`, resolve with `nonempty(fc.stt.initial_prompt)` so `""`/whitespace ⇒
     `None` (consistent with the `language` empty-string handling).
2. **`stt/whisper_local.rs`**
   - Thread the prompt into `WhisperEngine` (new field, set in `open`), into `begin`'s
     `WhisperLocal`, and into the `decode(...)` free fn signature.
   - In `decode`, after `set_language`: `if let Some(p) = initial_prompt { params.set_initial_prompt(p); }`.
   - `warm_up` passes `None` (no need to bias the silence decode).
3. **`main.rs`** (`WhisperEngine::open` call site ~L220): pass
   `config.stt.initial_prompt.clone()`.

No behavior change when `initial_prompt` is unset.

---

## Mechanism B — pronunciation normalization (deterministic rewrite)

Where the *zay-rz* knowledge actually lives. A small, config-driven rewrite table maps
known acoustic mis-hearings → the canonical term, applied to the final transcript. This
is **engine-agnostic** (works for Wyoming *and* whisper-rs) because it hooks the single
choke point where every transcript surfaces: `orchestrator.rs` ~L670,
`Some(SttEvent::Transcript(text))`.

### Config schema (new `stt.pronunciations` block)

```json
"stt": {
  "pronunciations": [
    { "canonical": "Zehrs", "variants": ["zairs", "zehr's", "zares", "zay rz", "zayers"] }
  ]
}
```

- `canonical`: the string to emit.
- `variants`: literal case-insensitive, whole-word alternates to rewrite to `canonical`.
- You generate `variants` *from* the pronunciation (“what spellings would Whisper produce
  for *zay-rz*?”). This is where the phonetics are encoded.

### Matching rules (keep it boringly deterministic for v1)

- Case-insensitive, Unicode-whole-word match (word boundaries; preserve surrounding
  punctuation/spacing). Multi-word variants (e.g. `"zay rz"`) match as a phrase.
- Preserve the sentence; replace only the matched span with `canonical` verbatim.
- Apply all rules in config order; a single left-to-right pass (document that rules don't
  cascade into each other to avoid surprise).
- **Explicitly not in v1:** fuzzy/edit-distance or phonetic (Soundex/Metaphone) matching.
  Leave a note that a `fuzzy`/`phonetic` mode is the natural v2 extension.

### Changes

1. **New module `anamanti-core/src/stt/normalize.rs`**
   - `pub struct PronRule { canonical: String, variants: Vec<String> }`
   - `pub struct Normalizer { /* precompiled variant→canonical, longest-first */ }`
     with `pub fn normalize(&self, text: &str) -> String` and
     `pub fn is_empty(&self) -> bool`. Precompile at construction (no per-turn regex
     build). An empty normalizer returns input unchanged (identity).
   - Unit tests live here (see Testing).
2. **`config.rs`**
   - Add `pub pronunciations: Vec<PronRule>` to `SttConfig` (default empty).
   - Add a `FileStt.pronunciations: Vec<FilePronRule>` (default empty), with a
     `FilePronRule { canonical: String, variants: Vec<String> }` using
     `deny_unknown_fields`. Trim/drop blank canonicals and blank variants in `from_file`.
3. **`orchestrator.rs`**
   - Build the `Normalizer` once (from `config.stt.pronunciations`) when the pipeline is
     constructed; store it on `Pipeline` (an `Arc<Normalizer>` or plain field).
   - In `stream_to_transcript`, apply `normalize` to `text` **after** the
     `speech_started` hallucination guard and **before** the diagnostic echo to the device
     (so the on-screen transcript and everything downstream see the corrected text). Skip
     the call when the normalizer is empty (zero-cost default path).

### Why the orchestrator choke point (not inside each engine)

Both engines deliver their transcript as `SttEvent::Transcript` through the same
`select!` arm. Normalizing there gives one implementation covering both engines, keeps
the engine modules decode-only, and guarantees the device echo shows the corrected text.

---

## Testing

- **`normalize.rs` units:** single-word rewrite (`"zairs"`→`"Zehrs"`), case-insensitivity,
  multi-word phrase (`"zay rz"`), whole-word only (no rewrite inside `"zehrsville"`),
  punctuation preserved (`"...zairs?"`→`"...Zehrs?"`), empty normalizer = identity,
  multiple rules, no-match passthrough.
- **`config.rs` units:** `stt.initial_prompt` and `stt.pronunciations` parse; blanks drop
  to `None`/empty; `deny_unknown_fields` still rejects typos; absent block ⇒ defaults.
- **`whisper_local.rs`:** existing decode tests still pass with prompt `None`; add one
  asserting the prompt is forwarded (guarded by `stt-whisper-local`).
- **Orchestrator:** extend an existing `stream_to_transcript` test so a configured rule
  rewrites a faked transcript, and the hallucination guard still fires first (normalize
  must not resurrect a discarded no-speech transcript).

## Validation / rollout

1. Land additive + off by default; CI green; defaults produce no diff in behavior.
2. On the M4 Mac Mini, set a real `stt.pronunciations` table (start with `Zehrs`) and,
   for the whisper-rs path, an `initial_prompt`; confirm corrected transcripts on-screen.
3. Document both knobs in the config page / README `stt` section.

## Open questions

1. Should `initial_prompt` be runtime-swappable via the config page/settings overlay, or
   boot-config only for v1? (Proposed: boot-config only; revisit with the settings UI.)
2. Do we want the normalization table surfaced in the config page editor in v1, or
   JSON-only? (Proposed: JSON-only first.)
3. Should normalization also run on the **LLM-facing** copy only, leaving the raw device
   echo untouched for debugging? (Proposed: correct both — single source of truth — but
   the raw transcript is still in the logs via the existing `STT transcript {text:?}`.)
