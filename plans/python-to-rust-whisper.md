# python-to-rust-whisper.md

Move the Anamanti Core's **STT** from the out-of-process **`wyoming-faster-whisper`
(Python)** server to an **in-process Rust engine** (`whisper-rs`, i.e. whisper.cpp).

Read with [`architecture.md`](./architecture.md) §2.3 (Mac services) and
[`agents.md`](../agents.md) (locked decisions). This plan touches STT only.

## Motivation (decided)

The move is **for deploy simplicity, not speed**. The current STT path is a
separate Python process the Core dials over Wyoming TCP (`stt_addr`, port 10300).
Inlining the engine deletes that process, its port, its `pip`/venv install, and the
version-skew risk — the Core becomes "one binary + a model file". Raw performance is
roughly a wash: the model math already runs in C++ (CTranslate2) today and in C++
(whisper.cpp) after, so the only measurable wins are ~5–30 ms of removed localhost
IPC per turn and ~50–150 MB of freed Python interpreter RAM (negligible on the Mac
Mini). whisper.cpp *can* additionally use CoreML/Metal on the M4 (CTranslate2 does
not), a potential inference speedup, but that is a bonus, not the goal.

## Decisions (locked for this plan)

- **Engine:** `whisper-rs` (Rust bindings to whisper.cpp). Keeps Whisper-class
  output (transcript the LLM sees is unchanged) and CoreML/Metal on the M4. ggml is
  self-contained — **no onnxruntime** (unlike a Sherpa path), so it sits alongside
  the existing pure-Rust `tract-onnx` speaker embedder without pulling a second
  general ONNX runtime.
- **Model size:** **configurable**; ship/fetch **both `base` and `small`** (English
  `.en` variants by default). Default to `base` (fast, fine for commands); `small`
  for accuracy when wanted. Selected at runtime via config.
- **Not in scope:** TTS (`wyoming-piper` stays Python for now), streaming partial
  transcripts, and any change to the **VAD**. The Core's energy VAD still decides
  end-of-speech and still drives finalization — the locked VAD decision is
  **untouched**.

## Guiding principle

**Refactor first, swap second.** Land the engine seam as a pure, behavior-identical
refactor (Stage 1, green tests, no new deps), *then* add the native engine behind
it (Stage 2+). Each stage is independently verifiable and reversible; the Wyoming
path stays selectable so rollback is a one-line config change.

## The seam

There is no `Stt` trait today — the only abstraction is `ServiceConnector`
(`orchestrator.rs`), which abstracts *connection acquisition*, not the engine.
Introduce a `Transcriber` trait in a new `anamanti-core/src/stt/` module whose method
set **mirrors the historical `SttSession`** so the pump loop's two-arm `select!`
(read device / read STT) and its cancellation-safety invariants are preserved
verbatim across engines:

```rust
pub enum SttEvent { Transcript(String), Other }   // Other = voice-started/stopped/…

#[async_trait]
pub trait Transcriber: Send {
    async fn forward_pcm(&mut self, pcm: Vec<u8>) -> Result<()>;
    async fn read_event(&mut self) -> Result<Option<SttEvent>>;
    async fn finish(&mut self) -> Result<()>;      // idempotent
}
```

Why mirror `SttSession` rather than a cleaner `accept`/`finalize` pair: the current
`stream_to_transcript` loop (`orchestrator.rs`) evaluates `end_silence` **only when a
device chunk arrives** and relies on the `sev = stt.read_event()` arm to deliver the
transcript (the test mock even emits it on the first chunk, before `audio-stop`).
Collapsing to a single-arm "pump then `finalize()`" loop would break that harness and
disturb the delicate cancellation-safety comments. Keeping the two-arm shape makes
Stage 1 a true no-op for the Wyoming engine. An in-process engine that produces its
transcript synchronously at `finish` implements `read_event` as *"pending until
finished, then return the decoded transcript once"*, which slots into the same loop:
after the VAD calls `finish()` and sets `finalized`, device reads are disabled and the
loop awaits `read_event`, which runs the (blocking, `spawn_blocking`) decode and
returns.

## Stages

### Stage 1 — Pure refactor (no new deps, behavior identical) ← executing now
- New module `anamanti-core/src/stt/mod.rs`: `Transcriber` trait, `SttEvent`, and
  `WyomingTranscriber` wrapping the existing `SttSession` 1:1.
- `orchestrator.rs`: `stream_to_transcript` takes `&mut dyn Transcriber`; the `sev`
  arm matches on `SttEvent`. `run_turn_after_start` builds a `WyomingTranscriber`
  from `connector.connect_stt()` instead of `SttSession::begin` directly.
- `lib.rs`: `pub mod stt;`
- **Exit check:** `cargo test` green; the `tests/pipeline.rs` mock-Whisper harness
  passes unmodified; `cargo clippy -- -D warnings` clean. Zero behavioral change.

### Stage 2 — Add the local engine behind a feature flag ✅ DONE
- New dep `whisper-rs` (0.14, `default-features = false` → CPU) gated by cargo
  feature `stt-whisper-local`. `hound` added to dev-deps for WAV fixtures.
- `anamanti-core/src/stt/whisper_local.rs`: `WhisperEngine` (loads the ggml model
  once, `Arc`-shared, cloneable) + `WhisperLocal` impl of `Transcriber`.
  `forward_pcm` appends LE-i16 samples into a reusable buffer; `finish` (idempotent)
  runs a **single** decode on `tokio::task::spawn_blocking` (CPU-bound — off the
  async reactor); `read_event` stays `std::future::pending()` until `finish` then
  yields the transcript once — slotting into the existing two-arm `select!`.
- **Exit check met:** `tests/whisper_local.rs` (feature-gated; skips without the
  `ANAMANTI_TEST_WHISPER_MODEL`/`_WAV` env assets) drives the real decode through the
  `Transcriber` seam. Verified with `ggml-tiny.en` + `jfk.wav` →
  `"And so my fellow Americans ask not what your country can do for you…"` (1 test
  passed). Native whisper.cpp build, `cargo test`, `clippy -D warnings` (feature on),
  and `fmt` all green.

### Stage 3 — Config + wiring ✅ DONE
- `stt/mod.rs`: added the `SttEngine` factory trait (per-turn `begin(format) ->
  Box<dyn Transcriber>`); `WhisperSttEngine` adapter in `whisper_local.rs`.
- `orchestrator.rs`: `Pipeline` gained an opt-in `stt_engine: Option<Arc<dyn
  SttEngine>>` + `with_stt_engine(..)` builder. `run_turn_after_start` uses the
  attached engine when present, else falls back to the Wyoming-via-`ServiceConnector`
  path — **so every existing call site and test is unchanged** (they just don't call
  the new builder).
- `config.rs`: new `stt` block — `engine` (`wyoming` default | `whisper-rs`,
  unknown → hard error), `model` (`base` default | `small`), `model_dir`,
  `model_path` override, `language` (`""` ⇒ auto-detect), `num_threads`.
  `SttEngineKind` + `SttConfig::resolved_model_path()`. `stt_addr` kept for the
  Wyoming engine. Mirrors the `FileConfig`/`deny_unknown_fields`/merge pattern; 3
  unit tests added. Documented in `anamanti.example.json`.
- `main.rs`: builds + attaches `WhisperSttEngine` when `stt.engine = whisper-rs`
  (behind the feature; a clear boot error if the binary lacks the feature).
- Model fetch: `anamanti-core/scripts/fetch-whisper-models.sh` grabs **both**
  `ggml-base.en` and `ggml-small.en` into `model_dir`.
- **Verified:** default build/tests (279 lib + 4 + 4 + 14), feature build/tests, the
  live decode, and `clippy -D warnings` (both default and feature) + `fmt` all green.

### Stage 4 — Build & acceleration ✅ DONE (Metal verified; M4 pending)
- Added additive accel features over `stt-whisper-local`:
  - `stt-whisper-metal = ["stt-whisper-local", "whisper-rs/metal"]` — Apple GPU, a
    pure build flag, no extra assets. **Recommended accel.** Verified here: builds,
    initializes the Metal backend, decodes correctly (exit 0). *Validated on an M1
    Max dev box — confirm on the M4 Mini before relying on the speedup.*
  - `stt-whisper-coreml = ["stt-whisper-local", "whisper-rs/coreml"]` — ANE path,
    **offered but not recommended**: it needs a separately generated
    `*-encoder.mlmodelc`, produced today by a Python (torch/coremltools) conversion
    step — which re-introduces the very Python build dependency this project is
    removing. Prefer Metal unless ANE offload is specifically needed.
- Model placement documented in `agents.md` (deploy runbook) +
  `scripts/fetch-whisper-models.sh`. Core is Mac-only → no Android cross-compile.

### Stage 5 — Cutover & cleanup ⏸ GATED ON HARDWARE (default intentionally NOT flipped)
The committed default stays `stt.engine = wyoming`. Flipping it now would break the
next deploy: the default binary has `stt-whisper-local` off, so `main.rs` hard-errors
at boot; and even with the feature it needs model files present. The flip must follow
on-device validation, which cannot happen from a dev box.

**Cutover checklist (run on the M4 Mac Mini):**
1. `anamanti-core/scripts/fetch-whisper-models.sh "<prod>/models"` (both base+small).
2. Build the prod binary with accel: `cargo build --release --features
   stt-whisper-metal` (see the deploy runbook in `agents.md`).
3. In the prod `anamanti.json`, set `"stt": { "engine": "whisper-rs", "model":
   "base", "model_dir": "models" }`. Restart per the runbook.
4. Validate a real device turn end-to-end (accuracy + latency vs the Wyoming server);
   compare `base` vs `small`.
5. Once satisfied, change the **committed** `SttConfig::default().engine` to
   `WhisperLocal` **and** make the native build the default (decide: feature on by
   default, or ship a prod build profile that enables it) — these two must land
   together so a default build still boots.
6. Keep the Wyoming path one release cycle as a fallback engine, then remove
   (`SttSession`, the STT half of `ServiceConnector`/`TcpConnector`, `stt_addr`).
- Decision: **keep Wyoming one cycle** after cutover, then delete.

### Stage 6 — Docs ✅ DONE
- `TODO.md` — STT no longer requires `wyoming-faster-whisper` (in-process option
  noted); added the M4 cutover task.
- `architecture.md` §2.3 (two engines behind the seam) + VAD bullet + decision-table
  row.
- `Plan.MD` — corrected the stale "server-side VAD" row; added a dated decision-table
  entry for the in-process engine.
- `agents.md` — Mac-services note + deploy-runbook build step (model fetch + feature).
- `README.md` — Mac-services row + STT notes; "server-side VAD" → Core-side.
- Code comments — corrected the stale "server-side VAD" doc comments in
  `wyoming/stt.rs` and `orchestrator.rs`.

## Rollout / rollback

Feature flag + config engine selector means both engines ship; run `whisper-rs` in a
test worktree alongside prod, flip the default only after parity, roll back with a
one-line config change. No flag day.

## Risks to watch

- **Blocking the async runtime** with synchronous decode → Stage 2 `spawn_blocking`
  (the real gotcha).
- **Native build dep** (whisper.cpp/ggml) added to a crate that was pure-Rust ONNX.
- **Short-clip hallucination** — Whisper's weakness on <1 s clips; the existing guard
  (`stream_to_transcript`: discard transcript if `speech_started` never latched)
  already covers it — keep it.
- **Model file** in the deploy folder — size + `0600` alongside the config.

## Decision log

- 2026-09-25 — Engine = `whisper-rs`; models = both `base` + `small`, configurable.
  Move justified by deploy simplification, not speed (accepted).
- 2026-09-25 — Stage 1 (Transcriber seam, pure refactor) + Stage 2 (in-process
  `whisper-rs` engine behind `stt-whisper-local`) landed and verified end-to-end
  (`whisper-rs` 0.14.4, CPU build).
- 2026-09-25 — Stage 3 (config `stt` block + `SttEngine` factory + `main.rs` wiring
  + model-fetch script) landed and verified. The in-process engine is now selectable
  via `stt.engine = "whisper-rs"`; default remains `wyoming`.
- 2026-09-25 — Stage 4 (Metal/CoreML accel features; Metal verified on an M1 Max dev
  box, CoreML offered-not-recommended due to its Python conversion step) + Stage 6
  (docs sweep) landed. **Stage 5 cutover is intentionally NOT executed**: the
  committed default stays `wyoming` because flipping it needs M4 on-device validation
  (and would otherwise break a default build's boot). Cutover checklist recorded in
  Stage 5 above + `TODO.md`. Code + docs complete; only the hardware-gated flip and
  the eventual Wyoming-path removal remain.
- 2026-09-26 — **Functional validation on real device audio (M1 Max MacBook Pro).**
  A test Core (`stt.engine=whisper-rs`, `base`, Metal, ports 10701/8731, mock LLM)
  transcribed two live turns from an Echo Show over Wyoming correctly
  (`"What's the weather today?"` ×2); Core-side energy VAD fired as expected; the
  transcript rendered on the device. TTS was absent by design (no Piper running →
  expected `connecting to TTS ... Connection refused`). **Still pending:** the same
  run on the **M4 Mac Mini** for the real decode-latency number, a full turn with
  Piper + Anthropic, and then the Stage-5 default flip.
