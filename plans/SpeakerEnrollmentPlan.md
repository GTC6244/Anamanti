# Speaker Enrollment Plan — active voice training + production voiceprints

Status: **IN PROGRESS** (started 2026-10-11). Builds directly on
[`speaker_id_plan.md`](./speaker_id_plan.md), which already delivered the passive,
auto-clustering per-person speaker-ID subsystem (Phases A–E code-complete). This
plan closes the two gaps that remain between "code exists" and "the owner can
recognize people by voice and train the system":

1. **A real, calibrated voiceprint model** — today a missing `speaker.model_path`
   silently falls back to a dev-only *mock* embedder and `speaker.enabled` is
   `false`. (Phase 1.)
2. **Active, guided enrollment** — today a voice is only clustered *passively* and
   named after the fact. There is no "add Dana, have her read a few phrases, then
   confirm the system recognizes her" flow. (Phase 2.)

Owner decisions captured 2026-10-11:
- **Enrollment UI lives on the Echo Show device** (it has the mic). The Mac config
  page is *not* getting voiceprint CRUD in this plan.
- **Passive auto-ID of strangers stays on.** Enrollment just produces a stronger,
  pre-named voiceprint; unknown voices still mint anonymous `Speaker N` clusters.

Everything here is **opt-in** behind `speaker.enabled` (default `false`), so a
deployment that leaves it off is byte-for-byte unchanged.

---

## What already exists (do not rebuild)

- `anamanti-core/src/speaker/` — `SpeakerEmbedder` trait, `OnnxSpeakerEmbedder`
  (ECAPA-TDNN via `tract-onnx`, always compiled), `MockSpeakerEmbedder`,
  `features.rs` (80-mel log-mel fbank front-end), `SpeakerRegistry` (SQLite
  voiceprint centroids: identify / cluster / rename / merge / delete),
  `SpeakerService` (`identify_and_attribute`).
- Per-turn identification already runs on **every turn's voiced PCM, post-STT,
  independent of the wake word** (`orchestrator.rs::identify_speaker`). This is
  exactly "identify during any speech that flows through the core."
- Per-person memory scoping + "you're talking to Dana" prompt line + household
  roster name reconciliation.
- A device **People** screen (`anamanti-display/lib/src/ui/people_screen.dart`) and
  the `anamanti-list/name/merge/delete-speaker` control frames.
- The `speaker{}` config block (`config.rs`: `enabled`, `model_path`,
  `match_threshold`, `new_threshold`, `min_speech_ms`, `embed_dims`).

The "fingerprint" is the 192-dim L2-normalized ECAPA-TDNN embedding; each person's
profile stores a running-mean centroid; matching is cosine similarity.

---

## Phase 1 — Real model + enable + calibrate

**Goal:** accurate passive recognition, turned on.

1. **Source an ECAPA-TDNN ONNX** (192-dim). Candidate: a VoxCeleb-trained ECAPA
   (SpeechBrain `spkrec-ecapa-voxceleb`, 3D-Speaker, or WeSpeaker) exported to
   ONNX. Pick one whose front-end is standard 80-mel / 25 ms / 10 ms so it matches
   `features.rs::FbankConfig`.
2. **⚠️ Lead risk — tract load.** Pure-Rust `tract-onnx` rejected Silero's `If` op
   (`VadSileroPlan.md`). ECAPA is a plain TDNN/CNN and *should* load, but this must
   be proven early. If tract rejects the graph, wire an `ort`/onnxruntime embedder
   variant (`ort` is already a pinned dep for `vad-silero` + local embeddings) —
   `OnnxSpeakerEmbedder` grows an `ort` backend selected by a cargo feature, same
   pattern as the VAD gate.
3. **`scripts/fetch-speaker-model.sh <dir>`** — twin of `fetch-vad-model.sh` /
   `fetch-embed-model.sh`; downloads the ONNX (+ verifies SHA). Model is **not**
   committed (lives under `models/`, like the Silero + embed models).
4. **Confirm the front-end contract** against the chosen model: mel count, frame/
   hop, window, and the input tensor layout (`[1, frames, mels]` vs
   `[1, mels, frames]`). Adjust `FbankConfig` / the `embed()` reshape if needed.
5. **Enable:** set `speaker.enabled=true` + `speaker.model_path` in a **test
   worktree** `anamanti.json` first, then the local-production folder.
6. **Calibrate** `match_threshold` / `new_threshold` / `min_speech_ms` on real
   Echo Show **far-field** captures; record EER at the chosen operating point.
   *(Hardware-gated — owed, like the Silero M3 far-field validation.)*

## Phase 2 — Active guided enrollment (device-driven)

**Goal:** Settings → People → **Add a person** → name → read a few phrases → the
system checks the samples are consistent and confirms it recognizes the voice.

**Audio path (respects "no second audio path"):** enrollment capture reuses the
ordinary wake-word turn with a **new display context `kind:"enroll"`**, exactly
mirroring the Audio-Diagnostics short-circuit (Plan.MD 2026-10-11). While the
enroll screen is foreground it stamps `screen:{kind:"enroll", session, name,
sample_index, phase}` on `audio-start`. The orchestrator parses
`DisplayContext::Enroll`, and in `run_turn_after_start` **short-circuits before
System-1 / LLM / TTS / memory**, routing the buffered voiced PCM to the enrollment
accumulator instead of `identify_and_attribute`. `widget_label()` → `None`, so an
enroll turn can never leak into System-1 or the LLM. **v1 is wake-word-gated per
sample** (say the wake word, then read the phrase); listen-chained auto-advance is
a deferred polish.

**Core side:**
- `SpeakerService` gains: `embed(pcm)`, `min_speech_samples()`, an in-memory
  per-session accumulator, `enroll_sample(session, pcm) -> EnrollSampleOutcome`
  (min-voiced check + inter-sample cosine *consistency* check → Accepted/Rejected),
  `enroll_finish(session, name) -> speaker_id` (average accepted embeddings → a
  **labeled** profile), `enroll_cancel(session)`, and read-only
  `identify_only(pcm)` for the verify step.
- `SpeakerRegistry::create_labeled(name, centroid, samples)`.
- New frames (byte-identical in both crates' `protocol.rs`):
  `anamanti-enroll-finish` / `anamanti-enroll-cancel` (device→Core control) →
  `anamanti-enroll-result` (Core→device; carries phase / accepted / sample_index /
  samples / consistency / confidence / speaker_id / name / message). The **sample**
  and **verify** captures ride the audio turn (enroll context); the **finish** /
  **cancel** are control round-trips (no audio). The enroll-result from a capture
  turn is sent down the per-turn socket then `audio-stop`.
- `control.rs` handlers for finish/cancel; `orchestrator.rs` enroll short-circuit
  for sample/verify.

**Device side:**
- `protocol.rs` byte-identical enroll frame constants; `control.rs` client wrappers;
  FRB `set_enroll_context(active, session, name, sample_index, phase)`,
  `enroll_finish(session, name)`, `enroll_cancel(session)`; a `WakeWordEvent` enroll
  variant/fields so the enroll-result surfaces to Flutter.
- `SpeakerEnrollScreen` (Flutter) pushed from the People screen's new **Add person**
  button: name entry → guided phrase capture with per-sample accept/retry feedback →
  commit → verify ("recognized you as Dana, 0.82") → done. Plus per-person
  **Add more samples** (re-train) and **Test my voice** actions on the People screen.

## Phase 3 — Calibration, on-hardware QA, docs

- Far-field threshold calibration + enroll-at-natural-distance guidance.
- Docs: `architecture.md`, a `Plan.MD` decision row, `DisplayUI.md` (People + new
  enroll screen), refresh `speaker_id_plan.md` status, `agents.md` pointer; refresh
  the stale env-var config references (config is the JSON `speaker{}` block now).

---

## Deferred (safe to pick up later)

- Listen-chained enrollment auto-advance (no wake word between phrases).
- Mac config-page voiceprint CRUD mirror.
- Diarization (multiple speakers per utterance).
- On-device (Echo Show) embedding — recognition stays on the Mac.
- Voiceprint drift re-enrollment prompts; stale anon-cluster retention.

## Progress notes

- 2026-10-11: plan written; baseline Core build green.
- 2026-10-11: **Phase 1 landed (code + model proven).** Validated that pure-Rust
  `tract-onnx` loads + runs WeSpeaker `voxceleb_ECAPA512.onnx` (192-dim) — so **no
  `ort` fallback is needed** (the lead risk is retired). Added `scripts/fetch-speaker-model.sh`,
  per-utterance CMN in `features.rs`, and `examples/speaker_smoke.rs`. `embed_dims=192`
  default confirmed correct.
- 2026-10-11: **Phase 2 landed (active enrollment, end to end).**
  - Core: `SpeakerRegistry::create_labeled`; `SpeakerService` enrollment session
    accumulator + `enroll_sample`/`enroll_finish`/`enroll_cancel`/`identify_only`/`embed`;
    `DisplayContext::Enroll` (+ parse, `widget_label()`→None); orchestrator enroll
    short-circuit (`handle_enrollment`); `anamanti-enroll-finish/-cancel/-result` frames;
    `control.rs` handlers. Unit + protocol + control tests.
  - Device: byte-identical frames; `TurnUpdate::EnrollResult` + `WakeWordEvent.enrollResult`
    (+ FRB regen); `set_enroll_context` / `enroll_finish` / `enroll_cancel`; `AssistantState`
    `enrollSeq`/`enrollResultJson`; `SpeakerEnrollScreen` + People-screen "Add person" FAB,
    wired into **Settings → People**. Widget tests.
  - Also fixed a latent settings-menu bug surfaced by the new menu entry: the menu and
    category pages reused one `ListView` element (so a scrolled menu opened a category
    page already scrolled) — fixed with distinct `PageStorageKey`s.
  - Green: `cargo test` (416) + `cargo clippy` + `flutter test` (144) + `flutter analyze`.

- 2026-10-14: **Enroll an existing household member.** The device flow ("Add a voice") now
  offers the household roster as pickable chips alongside the new-name field. Core: the
  `anamanti-speakers` reply carries `members` (names only); `enroll_finish` resolves the name
  to the member's canonical spelling, auto-adds a new name to the household, and folds into
  an existing same-named voiceprint rather than duplicating it. Device: FRB
  `list_household_members`, `OrchestratorClient.listHouseholdMembers`. Tests: Core 559,
  device Rust 92, Flutter 179 — all green, clippy/analyze clean.

- 2026-10-14: **Add voice samples (re-train).** People ⋮ → "Add voice samples" opens the
  capture flow in add-samples mode (no name step; a phrase guide checklist that starts at a
  different point in the phrase list). `anamanti-enroll-finish` takes an optional `speaker_id`
  → `SpeakerService::enroll_into` folds into that profile by id. Tests: Core 560, device Rust
  93, Flutter 182 — green; clippy/analyze clean.

## Owed / not done (hardware-gated — needs the Echo Show + real voices)

1. **Threshold calibration.** `match_threshold`/`new_threshold`/`min_speech_ms` and the
   enrollment `ENROLL_CONSISTENCY_MIN` are placeholders. Capture real far-field audio of a
   few people, measure EER, and tune. The current defaults are conservative guesses.
2. **On-device QA of the live capture flow.** The enrollment capture path (wake word →
   phrase → `enroll-result`) is unit/widget-tested with synthetic events but not yet run on
   hardware. Verify the round trip, the verify step's confidence, and that the enroll turns
   never speak a reply.
3. **Enable it.** Fetch the model into the prod/test `models/` dir and set
   `speaker.enabled=true` + `speaker.model_path` in `anamanti.json` (test worktree first,
   then local production). It ships **off** by default — no behavior change until enabled.
4. **Deferred niceties:** listen-chained enrollment (no wake word between phrases), a Mac
   config-page voiceprint mirror, diarization.
