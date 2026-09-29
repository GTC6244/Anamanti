# research.md

Novel and non-obvious uses of technology in **Anamanti** (the *anam an tí*, "soul
of the home" ambient voice assistant: Echo Show 8 + Rust/Flutter device ⇄ Mac Mini
"Anamanti Core").

This document catalogs the genuinely clever engineering in the project — the
workarounds for hardware constraints, the unusual protocol and ML integrations, the
latency/memory tricks, and the architectural seams that make local-vs-cloud and
on-device-vs-off-device tradeoffs swappable. It is distilled from the design docs
(`plans/architecture.md`, `plans/Plan.MD`, `plans/TODO.md`, and the feature/rollout
plans). Findings cite the component/file paths as written in those docs; the source
tree itself was not re-audited.

> **How to read this.** Each entry is *what it is → where it lives → why it's
> novel*. The recurring meta-theme, called out at the end, is **pluggable traits
> behind seams**, each with a committed cheap default and an opt-in heavier
> implementation, and **zero-regression opt-in defaults** so new subsystems merge
> inert until explicitly enabled.

---

## 1. Audio capture, echo cancellation, and barge-in

### 1.1 Device-side HAL echo-cancellation shim tapping the FPGA's DAC loopback
*`libamznaec_shim.so`, `LD_PRELOAD`ed into `android.hardware.audio.service`; outside this repo's build ([EchoShow8gen1-aec-shim](https://github.com/Brutus-GTC6245/EchoShow8gen1-aec-shim)); architecture.md §4, TODO.md §2.*

The Echo Show 8's FPGA capture stream `pcmC0D22c` (6-channel `S24_3LE` 16 kHz)
carries the four mics on ch0–3 **and a sample-aligned DAC loopback on ch4–5** — an
ideal, hardware-synchronized echo reference. A HAL shim interposes tinyalsa
`pcm_read` and runs **SpeexDSP linear AEC** using `avg(ch4,ch5)` as the far end,
*below* Android's audio framework, so `AudioRecord`/AudioFlinger never see the echo.
Measured **~16 dB, talker-preserving** cancellation; a barge-in "Hey Jarvis,
actually tell me about dogs" *over* a playing story produced a clean transcript
where the pre-shim attempt gave an empty one.

**Why it's novel:** rather than fight echo in-app, they discovered the device
exposes the perfect reference signal on dedicated ALSA channels and intercepted at
the HAL layer via `LD_PRELOAD`, reversibly (`.orig` backups, `persist.vendor.amznaec.*`
props). It's declared a device *prerequisite*, keeping the app build itself
credential- and complexity-free. A WebRTC-engine variant (38–50 dB) is a planned
upgrade requiring a full ROM tree.

### 1.2 In-app AEC investigated and deliberately rejected
*architecture.md §4, TODO.md §2, Plan.MD §4.*

Two in-app approaches were built and measured, then abandoned — the negative result
is itself the finding:
- **Platform `VOICE_COMMUNICATION` preset**: reachable and doesn't break the wake
  word, but *does not actually cancel* on this device (mic RMS ~0.1 during playback
  vs ~0.003 idle) — the effect attaches with no working render reference.
- **Software NLMS canceller**: >20 dB on host tests in isolation, but **net-negative
  once integrated**, because this design *flushes playback on barge-in* — there is no
  simultaneous echo to cancel, and the filter subtracts a phantom echo from the
  user's clean speech, corrupting the transcript.

**Why it's novel:** the insight that a flush-on-wake design structurally has *no
simultaneous echo*, so in-app AEC can only hurt, and real cancellation only makes
sense at the HAL layer with the hardware loopback (§1.1).

### 1.3 Flush-on-wake barge-in as a substitute for full duplex
*Device turn state machine; `anamanti-interrupt` frame; architecture.md §4.*

A mid-reply wake word *immediately* flushes the playback ring, sends an
`anamanti-interrupt` frame so the Core aborts in-flight LLM + TTS, and starts a fresh
turn. Because the Core relays audio faster than real time, the turn can reach IDLE
while audio is still draining — so the flush must silence audio even *after* the turn
has technically ended, and the explicit interrupt frame exists because otherwise the
Core would only learn of the interruption when the socket drops.

### 1.4 Self-correcting input sample rate — the "cpal says 48 kHz, HAL delivers 24 kHz" bug
*`anamanti-display/rust/src/engine/mod.rs`; wakeworddetection.md.*

Wake word scored ~0 on-device despite clear capture. Root cause: on the Echo Show 8
MediaTek AAudio HAL, `cpal`'s `default_input_config()` **reports 48 kHz but the
stream is delivered at ~24 kHz**, so the linear resampler emitted every block at 2×
speed (an octave high) and confidence collapsed. Isolation was elegant: the exact
16 kHz dump fed to the model scored ~0.0001, but *re-scoring the same dump as if it
were 8 kHz* (a 2× slowdown) scored 0.998 — proving the audio was exactly 2× too
fast. Fix: **measure the true input rate at startup** — drain a warm-up, count
samples into the ring over a fixed wall-clock window, snap to the nearest standard
rate (`measured 24283 Hz -> 24000 Hz`), and build the resampler from the measured
rate instead of cpal's number. A no-op where cpal is honest.

### 1.5 Mic-blanking self-trigger suppression that preserves frame cadence
*`anamanti-display/rust/src/wakeword/`; wakeworddetection.md (ported from VACA).*

Instead of muting or raising thresholds around the device's own short chime/error
sounds, blank the mic to **digital silence of the same length** — returning silence
frames rather than dropping them keeps the model's frame cadence and the AGC
envelope/noise-floor clean. It deliberately does *not* suppress during TTS/alarm
playback, so barge-in stays live. (Currently deferred in favor of the raised
`active_threshold`; documented to avoid dead code.)

---

## 2. Wake word (openWakeWord on pure-Rust tract)

### 2.1 Full openWakeWord chain reimplemented on tract-onnx, offline and deterministic
*`anamanti-display/rust/src/wakeword/detector.rs`; Plan.MD Phase 2, wakeworddetection.md.*

All three openWakeWord models run on pure-Rust `tract-onnx`: melspectrogram
(1280-sample chunks → 32-bin mel, scale `x/10 + 2`) → embedding (sliding 76-frame
window, 8-frame hop → 96-d) → classifier (last 16 embeddings → confidence), with
constants byte-matching the VACA Kotlin/TFLite reference. Feeding *fixed* chunks
gives each model a concrete input shape so tract fully optimizes the graph; rolling
state carries across calls for continuous streaming. Pure Rust with no extra native
deps is the deliberate right call for a single-binary engine on the 32-bit / ~1 GB
device; TFLite-with-delegate is the noted escape hatch.

### 2.2 Moving-average + cooldown detection gate with an injected clock
*`anamanti-display/rust/src/engine/gate.rs`.*

`DetectionGate` fires on the **moving average of the last 3 block scores** (not a
single frame) clearing threshold, with a 1500 ms repeat-fire cooldown. The clock is
injected into `observe(..)`, so smoothing + debounce are unit-tested with no live
pipeline.

### 2.3 Amplitude-invariance → a digital "gain slider" is useless; the lever is analog
*wakeworddetection.md.*

openWakeWord's log-mel front-end is amplitude/scale-invariant: a clean "hey jarvis"
attenuated to 0.037× (far-field level) still scores 0.99 on the host. So a
post-capture digital `mic_gain` does nothing for *detection* — it only rescales the
RMS meter and risks clipping. The real far-field lever is the analog path
(`VOICE_RECOGNITION` preset + platform AGC/NS), which cpal 0.18's AAudio backend
opens with `inputPreset = 0` and exposes no hook to change; the residual weakness is
SNR / ADC quantization (~6 effective bits), not digital amplitude. A precise
diagnosis that rejects the obvious-but-useless fix.

### 2.4 Shared front-end makes multi-word detection nearly free
The expensive mel/embedding front-end is wake-word-independent; only the tiny
per-word classifier differs. Holding a map of classifiers over one shared front-end
adds concurrent wake words cheaply.

> **Note on capture gain, added later (TODO.md §2):** the *analog* PGA drop
> (80→40, by design in the AEC shim) lowered the streamed mic level ~15 dB and made
> the Core's energy VAD intermittently miss speech onset. Two orthogonal fixes: a
> device-side shim makeup gain (`persist.vendor.amznaec.gain_db`, applied *after*
> cancellation so the echo-suppression ratio is unchanged) and a root-free in-app
> `AppSettings.captureGainDb` applied to the resampled 16 kHz block before both the
> detector and the streamed PCM.

---

## 3. End-of-speech VAD (off-device, pluggable, Silero)

### 3.1 VAD lives on the Core, not the device — the mic-open device runs no VAD
*`anamanti-core/src/vad/`; architecture.md §2.3/§4; VadSileroPlan.md.*

The device streams continuously and runs *no* VAD; end-of-speech is decided on the
more capable Mac behind a pluggable `SpeechGate` seam. This deliberately centralizes
"the one place silence can be told from a user mid-sentence." For follow-up turns,
`wait_secs` from the `anamanti-listen` frame **sizes the Core's no-speech window**,
so the Mac adapts its own silence timeout to semantic context (a `?` reply waits
10 s, else 5 s) that only it knows.

### 3.2 `SpeechGate` swaps only the per-frame voiced decision, not the state machine
The onset debounce, `speech_started` latch, `end_silence` hangover,
`no_speech_finalize`, silence-hallucination guard, and voiced-PCM accumulation all
stay byte-for-byte intact; **one line** of the hot loop changes (`rms_i16_le(&pcm) >
threshold` → `gate.push(&pcm, mic_rate)`). `push` takes raw `&[u8]` so the energy
path stays zero-copy while the Silero gate decodes to f32 as it re-frames.

### 3.3 Silero can't run on tract — an ONNX `If` op forces onnxruntime
*`ort` crate; `vad-silero` feature.*

Pure-Rust tract (used elsewhere for the wake word and speaker embedder) **cannot
load stock Silero VAD**: both v4 and v5 embed an ONNX `If` control-flow op that
tract's typed translation rejects. So `SileroGate` runs on `ort`/onnxruntime
(~15 MB native lib), pinned and feature-gated so a default build pulls neither dep
nor binary. A concrete runtime-compatibility constraint that dictates the whole
engine choice.

### 3.4 Ship v4, not v5 — and validate against a known-*positive*
The v5 export scores a near-constant ~0 under `ort` (max ~0.003 on clean loud TTS),
discarding every utterance as no-speech; v4 scores clean speech ~0.999 and far-field
device speech correctly. The captured lesson: the original spike only tested a sine
wave (correctly ~0), which *couldn't distinguish "correct" from "always ~0"* —
**validate a neural model against a known-positive input, not just negatives.**

### 3.5 Stateless shared session, per-turn LSTM state, fixed-window re-framing
Silero v4 is functional — its LSTM state is explicit tensor I/O (`h`/`c`), so one
shared `Arc<Mutex<Session>>` is loaded once at boot and cloned into each per-turn
`SileroGate` carrying its *own* `h`/`c` + framing buffer (zeroed on `reset()`).
A small carry buffer re-frames the device's variable chunks into Silero's fixed
1536-sample (96 ms) windows; a chunk completing no frame returns the sticky last
decision.

### 3.6 Honest scoping — accuracy is the payoff, latency is a *follow-on*
Per-frame Silero inference is sub-ms–1 ms on the M4 (dwarfed by the Whisper decode),
so the detector swap is **latency-neutral by itself**. The latency win comes only
from *retuning the `end_silence` hangover* (700 ms → target 400–500 ms) on the more
trustworthy confidence — a confident "not speech" is credible in a way an energy dip
is not. `engine=silero` with a missing model is a **hard boot error** (fail loud, no
silent fallback that would mask misconfiguration).

---

## 4. Speech-to-text (two engines behind a seam)

### 4.1 In-process whisper.cpp as a *deploy* simplification, explicitly not speed
*`anamanti-core/src/stt/`; `Transcriber` seam; python-to-rust-whisper.md.*

Two interchangeable engines: `wyoming` (external `wyoming-faster-whisper` Python
server) and `whisper-rs` (whisper.cpp in-process, `spawn_blocking`, optional
Metal/CoreML). The docs are unusually honest: **the value is deleting the Python
process/port/venv and version-skew** (the Core becomes "one binary + a model file"),
saving ~5–30 ms localhost IPC and ~50–150 MB RAM — *not* a speed change, since the
model math is C++ in both. End-of-speech remains the Core's VAD either way, because
neither engine does streaming VAD.

### 4.2 Boot warm-up decode hides a one-time ~7.8 s Metal shader compile
First-ever Metal decode eats ~7.8 s of whisper.cpp shader/pipeline compilation, so
`WhisperEngine::open` runs a throwaway 1 s-silence `warm_up()` at boot — the cold
start never lands on a user's first turn (and the Metal cache persists across
restarts). Measured M4: base ~169 ms mean (20–50× real-time), small ~391 ms.

### 4.3 No second ONNX runtime; CoreML rejected on dependency grounds
`whisper-rs` uses ggml (not onnxruntime), deliberately avoiding a *second* general
ONNX runtime beside the pure-Rust `tract-onnx` speaker embedder. The
`stt-whisper-coreml` (ANE) path was rejected because it needs a Python
torch/coremltools step to generate the `*-encoder.mlmodelc` — reintroducing the
exact Python dependency this migration removes. A sharp architectural-consistency
call; Metal is a pure build flag with no extra assets.

### 4.4 Seam shaped for a true no-op — and a latency bug that was a VAD bug
The `Transcriber` trait deliberately mirrors the old two-arm `select!` (an
in-process engine implements `read_event` as `std::future::pending()` until
`finish`, then yields once) so Stage 1 is a true behavior-preserving no-op. On
device, turns that blanked as "timeout" were traced to the utterance RMS never
crossing `voice_rms_threshold` (so `speech_started` never latched and the
anti-hallucination guard discarded a *correct* decode) — a threshold-tuning problem,
not a model/cutover regression.

---

## 5. LLM, tool-calling, and streaming TTS

### 5.1 Sentence-chunked streaming TTS coalesced into one device audio stream
*`anamanti-core/src/orchestrator.rs`; architecture.md §4/§6.*

The Core segments the LLM token stream into sentences and synthesizes each with Piper
*as it forms*, so first audio arrives at ~first-sentence latency (~2 s) instead of
full-reply latency (~8 s). The subtlety: **the device ends its turn on the first
`audio-stop`**, so naive per-sentence streaming would emit multiple stops and end the
turn early. The many Piper bursts are therefore **multiplexed into one logical
Wyoming stream** (one `audio-start`, all chunks, one `audio-stop`).

### 5.2 `speakingDone` decoupled from turn-end via ring-buffer occupancy
Because the Core relays faster than real time, the turn reaches IDLE while audio
keeps draining. The device tracks `PlaybackSink::pending()` and emits a **UI-only**
`speakingDone` once the ring empties, so on-screen reply text stays up exactly as
long as audio actually plays; a barge-in flush zeroes the ring and triggers the same
signal. Turn-lifecycle state and playback state are genuinely decoupled.

### 5.3 Non-streaming completion for tool turns, re-streamed for TTS
*rig engine; `anamanti-core/src/llm/rig.rs`; architecture.md §8.1.*

Because a live `ollama` streaming parser *drops non-final tool calls*, tool turns use
a **non-streaming** completion (looping up to `MAX_TOOL_ROUNDS=4`) — trading
streaming for correctness — then recover incrementality by feeding the completed
reply back through the sentence-chunked TTS path (§5.1). No user-visible regression.
(This same "any tool present ⇒ full blocking completion" is precisely the latency
bottleneck System-1 was built to bypass — see §6.2.)

### 5.4 Streaming the first syllable to mask tool-call latency
*memory_and_provider_rollout.md.*

The token stream feeds an mpsc channel and TTS vocalizes opening tokens *while a tool
fetch is still in flight*, with a segmentation buffer batching to word/sentence
boundaries so un-speakable fragments aren't spoken. The framed insight: **streaming
(not failover polish) is what masks tool latency** — time-to-first-audio is the
headline metric.

### 5.5 Anchored-regex fast path skipping a whole tool-negotiation round-trip
A small, anchored, logged regex prefilter (`what is the weather`, `search for…`)
short-circuits Pass-1 tool negotiation and calls search directly, saving an LLM
round-trip — explicitly a fast path, not the router, with over-match risk bounded by
keeping the list small and anchored.

### 5.6 Live 12-month model catalog with curated offline fallback
*`anamanti-core/src/llm/catalog.rs`.*

`ModelCatalog` live-queries each provider's `GET /v1/models` (Anthropic `created_at`,
OpenAI `created` + a chat filter), keeps the trailing 12 months, caches ~1 h, and
falls back to a curated built-in list when a key is missing/offline — so the model
dropdown stays current without hardcoding IDs that go stale.

### 5.7 Anthropic *subscription* auth via a token-printing CLI
*`anamanti-core/src/llm/anthropic_auth.rs`.*

Cloud Claude can authenticate with an API key **or a Claude subscription (OAuth)**:
`AnthropicTokenProvider` resolves a Bearer token from `ANTHROPIC_OAUTH_TOKEN`
(`claude setup-token`) or a configurable token-printing command (default the `ant`
CLI), cached with a short TTL and refreshed on 401, sending `Authorization: Bearer` +
`anthropic-beta: oauth-2025-04-20` (no `x-api-key`). The same provider backs both
chat turns and the catalog, so listing and turns authenticate identically.

---

## 6. System-1 "fast decisions" — dual-process cognition for a voice pipeline

*`anamanti-core/src/system1/`; `DecisionEngine` trait; system1-fast-decisions.md, Plan.MD.*

### 6.1 Kahneman's fast/slow thinking made into an architecture
An optional non-autoregressive **System-1** stage runs *before* memory recall and the
LLM, resolves common intents in a single forward pass, and only *defers* hard turns
to the **System-2** path (GraphRAG recall → rig+tools LLM → Piper). It reframes
intent handling not as "route to a skill" but as a **cognitive-load router**: the
cheap reflex answers what it can; the expensive deliberative system fires only when
reasoning or memory is actually needed.

### 6.2 It targets two *structural* latency bottlenecks the LLM path creates
Precisely: (a) because *any* tool is seeded on every turn, rig takes the
**non-streaming** branch (`llm/rig.rs:1840`) and loops up to `MAX_TOOL_ROUNDS=4`, so
time-to-first-audio ≈ *full* generation time; and (b) memory recall does a blocking
**OpenAI embedding round-trip** (`text-embedding-3-small`, `memory/backend.rs:97`) on
the critical path of *every* turn. A resolved turn skips both. Cleverly scoped: it
does *not* touch VAD hangover / Whisper / cold start, because those happen before a
transcript exists, where a decision engine can't help.

### 6.3 Typed-classifier engines with one shared `/v1/systemone` wire contract
Both engines answer **state + typed questions** in one shot — `choice`
(label + probabilities), `noul` (P(true)), `score` (ordinal EV) — each carrying a
calibrated `answer_confidence = max(p)`, so gating is a single threshold. **Jev**
(`typesafe/jev-1.13` on OpenRouter's Decisions API) and the user's **Laya-Decision**
(pure-Rust candle port of ModernBERT/mmBERT, matches PyTorch to 1e-4, run as a local
`laya-serve` sidecar) are wire-compatible, so one HTTP client serves local-or-cloud
by swapping base URL + auth + model id — reusing the exact `LlmBackend` pattern, held
on `RuntimeSettings`, config-selectable and persisted, **default `none`** so it's a
byte-for-byte no-op until opted in.

### 6.4 `needs_full_understanding` — the open-slot guard that makes a classifier safe
Because these classifiers return a *label, never free-form text*, a boolean question
— "does answering require open-ended reasoning, personal memory, or details not
implied by a simple command?" — catches open slots (weather for a *non-home* city)
and forces a Defer instead of a guess. This is the non-obvious bridge that lets a
classifier sit safely in front of an LLM.

### 6.5 The three-way split: Resolve / Route-only / Defer
The bounding empirical result: intents are Resolve-able only when args are absent,
defaulted, or deterministically parseable (weather → home location, timer → parsed
duration). Free-form-slot intents (`shopping_add`, `recipe_lookup`, `directions`,
`place`) become **route-only** — classified for a filler line + tool hint, but never
resolved (an intentional "classify-but-never-resolve" tier, e.g. `place`'s `_ => None`
defer arm). Everything else Defers.

### 6.6 Ground-truth veto for overloaded commands ("stop", "never mind")
The model emits a generic `stop_dismiss` label but **never names the referent**; a
deterministic priority ladder in the orchestrator (alarm ringing → running timer →
media → open screen → in-follow-up → defer) picks it from live device state. The
safety net: if the ladder finds no valid referent, the turn defers **even if the
model was confident** — "the model can never conjure a stop action from nothing." The
model *sees* device state (folded into `state` only when present, so ordinary turns
stay byte-identical) to sharpen confidence, but the ladder owns truth.

### 6.7 One temporal question doing double duty; one global threshold
A single `temporal` choice (past/present/future) in the same forward pass acts as
both a **defer-gate** (`past` → defer every data intent, no fast historical fetch)
and a **data-selector** (present vs. future → weather current vs. forecast).
Calibration found **one global `min_confidence=0.85` suffices** (22/22, zero
mis-resolves) — empirically answering whether per-intent thresholds were needed
(they weren't). The resolve path still emits the widget *before* speech
(`DeviceAction::ShowWeather` opens first) and shares `emit_follow_up_and_stop`, so
chat-log, inferred-memory, and follow-up all still happen. A "confident-but-deferred
location" case is re-asked with the home location folded into the *question text*
(putting it in a `state` field alone didn't move the score).

---

## 7. Memory: embedded GraphRAG with a graceful FTS floor

*`anamanti-core/src/memory/`; architecture.md §2.3; memory_plan.md, memory_and_provider_rollout.md.*

### 7.1 Embedded, in-process HelixDB via a git-pinned *engine* crate
The published `helix-db` crate is only a thin HTTP client to `localhost:6969`
(no public embed API), and the team's own research concluded they'd need a
co-located server. A throwaway spike **overturned** that by depending directly on the
unpublished workspace `db` engine crate via git rev, which compiled and ran all
GraphRAG primitives **in-process** with `HelixDbSource::Disk` — no Docker, no CLI, no
server, no port (`memory/helix.rs`). Novel because it uses an internal, path-dep-only
crate (pulling a git-pinned SlateDB fork + tantivy + foyer) as an embedded
graph+vector library entirely on the LAN.

### 7.2 GraphRAG recall decoupled from the write path, with a clean FTS fallback
SQLite is the **store of record**; the graph is a *derived index that can fail
without data loss*. Every completed turn appends to `anamanti_chatlog.jsonl`; a
background ingester embeds it into a `User/Turn/Memory/Entity` graph, so recall is
vector-KNN + a graph hop. If `OPENAI_API_KEY` is absent or the graph won't open,
recall **falls back to SQLite FTS with writes unaffected** (behind a `memory::Recall`
seam). Concurrent hybrid pre-fetch overlaps the two DB latencies via
`tokio::join!(sqlite_history(), helix_subgraph())`.

### 7.3 JSONL chat log as a crash-safe, idempotent ingestion queue
The append-only JSONL is simultaneously the human-auditable source of truth *and* the
embedding queue, with a committed offset / high-water-mark sidecar. This decouples
capture from embedding (batch embeddings; re-embed if the model changes), and re-runs
after a crash are safe because upserts are idempotent by `ext_id`. Turn logging is
log-and-continue so it can never break a turn.

### 7.4 Entity extraction moved off the hot path → richer graph at zero turn latency
The entity/topic-extraction LLM call (Claude Haiku 4.5) runs in the *background*
ingester, not the live turn, so a full extraction pass adds **zero** user-facing
latency. Only the incoming query transcript is embedded synchronously (one small
OpenAI call on the hot path).

### 7.5 `rename_entity` — boundary-aware fact correction that deliberately keeps stale vectors
On the `GraphView` trait, fixing a misspelled entity rewrites the `Entity` node's
name in place (preserving id + all edges) *and* every whole-word occurrence in
`Turn.text` / `Memory.content`, case-sensitive and boundary-aware
(`replace_whole_word`, so "Sam" never mangles "Samsung"). It refuses a name already
used by a different entity (which would split that entity's edges). Cleverly it
**intentionally leaves the stale embedding** — a one-token spelling fix barely moves
the vector, and re-embedding would require the embedder. Surfaced as an "Edit name"
button on the `/helix` debug page.

### 7.6 Forward-compatible multi-User schema shipped before speaker ID existed
The graph shipped with multiple `User` nodes and `SAID`/`KNOWS` edges from day one,
attributing all turns to a shared `household` sentinel, so real speaker IDs could
attach later "without a migration." The bet paid off — speaker ID later attached
per-speaker `User` nodes with no schema change.

### 7.7 A 16 MiB worker stack to survive deep async state machines
The embedded engine's async types overflow tokio's default 2 MiB worker stack, so
`main.rs` builds its runtime with a **16 MiB** worker stack (tests on a 32 MiB
thread) — a non-obvious runtime failure mode of embedding a heavy async DB
in-process, and an async NodeVector index that reports `index_not_found` until it
settles (so `init` flushes and polls until ready).

---

## 8. Speaker identification (passive, local, privacy-first)

*`anamanti-core/src/speaker/`; ECAPA-TDNN on tract-onnx; architecture.md §2.3, speaker_id_plan.md.*

### 8.1 Passive auto-clustering — no "say your name" friction
An embedding is computed from the voiced PCM the Core already buffers (already
RMS-gated); an utterance matching no profile **mints a new anonymous persona**
(`Speaker 2`) on the spot, named later by voice ("my name is Dana") or in settings.
Identification runs *after* end-of-speech and *before* the LLM — off the streaming hot
path, tens of ms on the M4 — and scopes memory writes/recall + the prompt to that
person, with a shared "household" floor for unknowns.

### 8.2 Dual-threshold match/new with a deliberate no-update band
`MATCH` (0.55 cosine) ⇒ update centroid; `NEW` (0.40) ⇒ mint persona; **between the
two** ⇒ attribute to the best match *for this turn only, without mutating the stored
centroid* (avoids poisoning a profile with a borderline far-field sample); below
`MIN_SPEECH_MS` (1200 ms voiced) ⇒ fall back to `household`. Conservative by design to
prevent false merges on noisy audio.

### 8.3 Online-mean centroids, linear scan, no vector index
Centroids are an online mean over L2-normalized embeddings, renormalized
(`c' = normalize((c*n + e)/(n+1))`). Household counts are single-digit, so `identify`
is a plain linear cosine scan — right-sized engineering that skips an index because N
is tiny.

### 8.4 Locality is *not* the accuracy lever — privacy is
Open ECAPA-TDNN/WeSpeaker models match or beat commercial cloud speaker-ID on
VoxCeleb (EER ~1%); the real limiter is the Echo Show's noisy far-field audio, which
degrades cloud and local equally. Local wins purely on **privacy** (raw voice never
leaves the LAN — stronger than the existing text-embedding calls) and adds no per-turn
network hop. Voiceprints are derived vectors, not audio, and are user-deletable; raw
PCM is never persisted for speaker ID. The front end is a hand-written pure-Rust
80-dim log-mel fbank → tract-onnx, keeping the offline/no-onnxruntime property.

### 8.5 Deterministic mock embedder for the whole offline test suite
`MockSpeakerEmbedder` derives a stable, *separable* vector from coarse framed-energy
PCM stats, so every Phase A–D test exercises distinct-voice clustering with no model
or network — the same mock-first discipline used for the text embedder and entity
extractor.

---

## 9. Networking, discovery, and the extended Wyoming protocol

### 9.1 Project-local `anamanti-*` frames riding piggyback on standard Wyoming framing
*architecture.md §3/§4/§8.*

A whole family of private frames (`anamanti-interrupt`, `-listen`, `-timer`, `-speak`,
`-recipe`, `-weather`, `-place`, `-notify`, `-hello`, `-get-drive-token`, and the
control frames) ride the *existing* newline-JSON + PCM Wyoming framing. Off-the-shelf
Whisper/Piper Wyoming servers simply ignore frames they don't recognize, and an
unknown `kind` is ignored so "a newer device never breaks an older Core" — extending
a standard streaming protocol with private semantics without forking it, forward- and
backward-compatible across the LAN hop.

### 9.2 Wire codec deliberately re-implemented (not shared) across the two crates
*architecture.md §2.3/§8.3.*

The device crate is an Android `cdylib` and can't be shared as a Mac library, so the
Wyoming codec is **re-implemented in both crates and kept byte-identical, guarded by
round-trip tests on each side** — a pragmatic, explicitly-tested "byte-identical"
contract instead of accidental drift.

### 9.3 Pinned routable-IPv4 mDNS with manual re-advertise on interface change
*`anamanti-core/src/discovery.rs`; architecture.md §5.*

The Core advertises a *single pinned routable IPv4* (resolved via the routing table),
not `mdns-sd`'s `addr_auto` — which would leak the Mac's IPv6 link-locals and hand
the device an unreachable address. But pinning sacrifices mdns-sd's auto-refresh, so
the Core **subscribes to the daemon's `IpAdd`/`IpDel` monitor events and re-registers
with the fresh primary IPv4** on any change (Wi-Fi↔Ethernet failover, DHCP renew,
en0↔en1) — manually re-implementing address refresh without a restart. A subtle
correctness fix where the obvious path is wrong.

### 9.4 Strict `instance_id` pinning derived from the git branch code; `role=core` filter
*architecture.md §5/§7.*

Cores advertise TXT `role=core` + `instance_id`, resolved `env → working-dir git
branch code → sanitized service name`. Devices filter by `role=core` (so raw
Whisper/Piper servers on the same `_wyoming._tcp` are never connected) and pin
*strictly*: a pinned display stays **offline** if its Core is unreachable, never
silently switching Macs (`"Auto"` = first responder). The branch-code fallback means
a test binary run from a worktree **auto-names itself by its branch**, so prod + test
Cores run side by side with no manual config.

### 9.5 Storage asymmetry shapes the multi-instance topology
*architecture.md §5.*

Many displays share one Core (per-connection reply routing isolates them; memory +
settings are one household pool). Cross-instance sharing rides SQLite **WAL** mode —
but two processes writing the same embedded HelixDB graph is unsupported, so a test
instance sharing prod data must set `memory_backend=sqlite` (degrading recall to FTS
precisely for the shared-data case). A concrete constraint that shapes deployment.

### 9.6 Proactive push via a device-dialed persistent sidecar channel ("Approach A")
*`rust/src/wyoming/notify.rs`; architecture.md §4/§7.*

Every normal frame rides a socket the *device* opened for a turn, so the Core can
only reply. To push *unprompted* visual notifications, the device opens a **second**
long-lived Wyoming connection (a **third** for the ambient weather clock chip),
registers via `anamanti-hello` with a `role`, and holds it open with capped backoff;
the Core parks the read loop and pushes `anamanti-notify` / `anamanti-weather current`
down it. Chosen over a reverse connection (no new trust direction, nothing new for
NAT/firewalls; reuses the exact mDNS + `instance_id` pin) and explicitly over a
"doorbell/long-poll" — whose only saving, idle-socket cost, is free on a mains-powered
display.

### 9.7 Timers as device-owned, fire-and-forget state that survives socket/Mac loss
*`rust/src/engine/timer.rs`; `anamanti-timer`/`anamanti-speak`; architecture.md §4/§8.3.*

An LLM `set_timer` tool emits a `DeviceAction` → `anamanti-timer` frame; the **device**
then owns the state — unlimited concurrent countdowns, UI, and alarm — so a timer
keeps running after the turn's socket closes and even if the Mac disconnects (the Mac
tool "runs inside the LLM loop, which has no socket," so state ownership *must* move
to the device). On fire it rings a local bell, then, if the Mac is reachable, requests
the spoken "Time's up" via `anamanti-speak` (a device-initiated dial *outside* any
voice turn); bell-only when offline. Durable state (device) split cleanly from
optional voice (Mac).

---

## 10. Display features — structured UI pushed to the device

### 10.1 The repeated triad: a rig tool + a Wyoming frame + a device UI mode
*RecipePlan.md, WeatherPlan.md, PlacesPlan.md; architecture.md §8.*

Each display feature is a `PortableTool` on the Core + a new frame carrying
structured JSON + a new Flutter UI mode, split along the *locked Rust/Flutter
boundary* (Rust owns fetch/parse/logic; Flutter owns presentation). Each frame is a
template for the next (`anamanti-timer` → `-recipe` → `-weather` → `-place`), kept
byte-identical in both crates' `protocol.rs`. A tool has a **dual effect**: it returns
a short spoken confirmation to the model *and* pushes `DeviceAction::Show…` onto the
turn's `ActionSink`, drained after the reply into the frame.

### 10.2 The device→Core display-context channel, piggybacked on `audio-start`
*architecture.md §4; RecipePlan.md; Plan.MD.*

The device stamps a `{kind:...}` `screen` block onto **each turn's own
`audio-start`**, parsed into a `DisplayContext` enum that injects a one-line note into
that turn's system prompt — so a voice turn taken *while looking at a screen* knows
what's on it ("close it" → `close_weather`; "what about tomorrow" → same-place
`weather_lookup`). It travels *with* the utterance, so the model always sees the exact
on-screen state for the turn it's answering and nothing goes stale — **no separate
persistent context uplink**. Deliberately general: a new screen adds a `DisplayContext`
variant + a prompt-line arm + a `set_<screen>_context` setter. This is the same
channel that later feeds System-1's ground-truth veto (§6.6). (A P0 refinement threads
*background timer* snapshot as sibling keys in the same block, so a timer running
behind another screen is visible to the Core and `timer_query` is answered straight
from context — the Core needs no timer state of its own.)

### 10.3 Sidecar StreamSinks for lifecycle-decoupled screens; two transports for one frame
A recipe/weather screen must persist across many follow-up turns and idle, so its
lifecycle is *not* tied to a voice turn — it uses a **sidecar FRB stream**
(`start_recipe_channel` / `WeatherPush`) rather than the per-turn `WakeWordEvent`.
Weather goes further: one `anamanti-weather` frame flows over **two transports** — the
per-turn socket (voice-triggered full screen) *and* a persistent `role=weather`
channel where a `WeatherService` broadcasts current conditions every ~30 min to the
idle-clock chip with no voice turn.

### 10.4 JSON-LD-first parsing, LLM only as a fallback
Recipe parsing tries `schema.org/Recipe` JSON-LD (embedded by most recipe sites) for
a fast, accurate, offline-testable parse, and only hands page text to the LLM (with a
strict "emit this JSON" instruction) when JSON-LD is absent/malformed. "Parse via an
agent" is the fallback, not the path — capping cost/latency to the JSON-LD-miss case.

### 10.5 Data shapes engineered to be `Eq` so they ride inside `DeviceAction`
A recurring low-level trick: `WeatherReport` uses *integer* temps and `PlaceReport`
stores strings/ints/`Option<bool>` with **no `f64`**, specifically so the structs are
`Eq` and can live inside the `DeviceAction` enum and JSON-marshal directly to the
device.

### 10.6 Keyless hero-image resolution keeps the shared APK credential-free
Places resolves the hero photo to a **keyless `photoUri`** (Google Places API New,
`skipHttpRedirect=true`) so only that URL rides the frame and the device fetches it
directly — the `GOOGLE_PLACES_API_KEY` never leaves the Mac (cost bounded by an
`X-Goog-FieldMask` on every call). Disambiguation adds *no* new state: multiple
candidates → the tool returns a spoken candidate list (no card), the model asks which
and re-calls with the chosen `place_id`, riding the existing follow-up loop.

### 10.7 Process-wide generic TTL tool cache shared across three call sites
Every `WeatherProvider` is wrapped in a generic `cache::ToolCache` keyed on the
outgoing call args `(location, units)`; being process-wide, the **System-1 fast path,
the `weather_lookup` tool, and the ambient push all share hits** — a forecast fetched
by one is free for the others. Per-entry TTL with an explicit "read-only tools only"
rule (`tool_cache` map; `0` disables; mutating tools like timers/shopping-list must
not cache).

---

## 11. Camera-as-proximity-sensor (zero ML)

*`rust/src/camera/presence.rs` + Kotlin `CameraBridge`; architecture.md §2.1, Plan.MD.*

The Echo Show's **front camera is repurposed as a motion/proximity sensor with no
ML**. A Kotlin `CameraBridge` (the visual twin of the `AudioRecord` `MicBridge`) opens
Camera2 at a tiny 176×144 / ~5 fps, pulls only the packed **Y (luma)** plane, and
pushes it over JNI to Rust, where a pure, unit-tested `PresenceDetector` measures the
**mean absolute per-pixel luma delta** (frame-to-frame motion) — flipping to *present*
above a threshold and back to *absent* after a quiet release window. Sensing (Rust) is
cleanly split from actuation (Flutter `ScreenBrightnessController` via `MethodChannel`
brightens on approach, dims to a large centered clock when the room is quiet). No
frames ever leave the device; it degrades silently on permission-denied / **privacy
shutter closed** / no camera (screen just stays bright). Privacy-safe presence from
luma motion alone, cheap enough for the constrained CPU.

---

## 12. House-wide music: a hard control/data-plane split (Snapcast)

*snapcast_routing_plan.md, MusicPlan.md; architecture.md §9.*

### 12.1 "PCM never enters the Anamanti Core"
Music originates on the Mac, not the ~1 GB Echo Show (no on-device system-audio
capture, no second device audio path). Sources — **librespot** (Spotify Connect, the
shared "Ambient" device) and an **mpv/ffmpeg web-URL player** — each write raw PCM to
a **named snapfifo**; snapserver (on the Mac) reads the fifos and fans out to
snapclients. The Core issues *only* control (Web API / mpv IPC / snapserver JSON-RPC)
and **never touches audio bytes**, keeping the latency-sensitive voice path
uncontaminated. Two heterogeneous streams (44.1 kHz native / 48 kHz web) are resampled
per-stream to one client format so all rooms play in lockstep (~1 s buffered latency
accepted; FLAC on the wire for low CPU).

### 12.2 Ducking via Snapcast *group volume*, hooked to the turn state machine
Rather than mixing or touching audio, a `MusicDucker` lowers the Snapcast **group
volume over JSON-RPC** (`Group.SetVolume`) on `THINKING/SPEAKING` and restores on
`IDLE` (wired at the `server.rs on_event` seam). Group volume ducks *both* Spotify and
web streams uniformly, regardless of who started playback; idempotent
attenuate-and-restore.

### 12.3 Echo Show as a bundled `snapclient` in a separate OS process
The device runs a prebuilt `snapclient` binary bundled in the existing app but `exec`'d
by a Kotlin foreground service as a **separate OS process** — *not* linked into the
Rust engine, *not* a second APK. This keeps the app's single locked `cpal` path
(capture + TTS) untouched, isolates crashes/RAM, and lets **Android's AudioFlinger mix
the two processes' output**. Per-device toggle defaults **off**, because the device
playing continuous music with no AEC worsens wake-word self-triggering — so the raised
SPEAKING-threshold lever is reused: hold the higher wake-word threshold whenever the
device snapclient is in the active group.

### 12.4 Spotify as a control-plane-only tool over un-synthesizable audio
`spotify_control` (a rig tool) drives the Spotify Web API (search → play/queue,
targeting the librespot `device_id` resolved by name), while **librespot** produces the
actual DRM'd PCM into the snapfifo. The insight: Spotify audio *can't* go through
Piper/Wyoming, so the assistant only ever **controls, never carries** it. Errors
surface **as speech** (returned as the tool result so the model apologizes aloud, "is
it powered on?"). A `MusicSupervisor` runs snapserver/librespot/mpv as managed child
processes (`kill_on_drop`, per-process logs), and a loopback Authorization-Code + PKCE
consent flow (fixed pre-registered redirect) **rebuilds the LLM live** (`apply_spotify`)
so the tool activates the instant consent completes — no restart.

---

## 13. Conversation-end detection (the "second human in the room")

*EndConversationPlan.md.*

The non-obvious problem statement: follow-up mode ends only on *silence*, which never
comes when the user keeps talking — **to another person** — so the assistant may
answer speech not aimed at it. This reframes end-detection as an **addressing
problem**. The chosen design adds no dedicated classifier or extra model call:
- An **`end_conversation` tool the main LLM can call during its normal turn**; the
  Core watches the stream and, when it fires, *suppresses the follow-up
  `anamanti-listen` frame* so the chain sleeps after the goodbye — full context,
  natural sign-off, zero extra inference. Accepted tradeoff: it can only end *after*
  it answers.
- A deterministic **wake-word-prefixed fast path**: "Jarvis, I'm done" is a strong
  signal *precisely because re-addressing the assistant by name mid-conversation is
  unusual* → near-zero false positives, instant, zero-cost (guarded so "thanks, now
  what's the weather" still routes to a normal turn).

Both hooks live entirely in the Core's follow-up path — **the device needs no
changes** (it just obeys whether `anamanti-listen` arrives).

---

## 14. Shared platform tricks worth their own mention

- **Single flat FRB `StreamSink` tagged by a unit-only enum** — one
  `Stream<WakeWordEvent>` covers the whole turn lifecycle as a flat struct with
  defaulted payload fields tagged by a unit-only `WakeWordEventKind`, so the FRB
  boundary needs **no `freezed` codegen** and there's exactly one stream to manage;
  the transcript/reply/phase split is deferred Dart-side in `AssistantController`
  (architecture.md §3).
- **SPSC pre-allocated lock-free ring buffer** (`ringbuf::HeapRb`, allocated once)
  decouples the real-time capture callback from wake-word and network consumers with
  only atomic index updates — no per-frame heap churn; downmix-to-mono happens in the
  RT callback but resampling is moved *off* it. Directly serves the ~1 GB RAM / no-GC
  constraint that motivated Rust (architecture.md §2.1/§6).
- **Runtime-swappable settings via a per-turn snapshot** — the Pipeline reads a
  snapshot of `SharedSettings` at each turn boundary (lock-free within a turn, hot
  swap between turns), so backend/voice/model/household/location changes take effect
  next turn with **no restart**; invalid changes (e.g. a cloud backend with no key)
  are rejected **in-band** without dropping the socket, and a `LiveHomeLocation` shared
  handle propagates one location edit to both prompt grounding and the directions-tool
  origin without rebuilding the backend (architecture.md §8).
- **Threat-model-driven secret placement** — provider keys can be entered at runtime
  but *only* on the loopback config page (127.0.0.1, no auth); the device/Wyoming
  control path never accepts a key and is told only a `*_key_set` boolean, so cloud
  secrets never live on the shared household screen. Tri-state key field (absent =
  keep, `null` = clear, value = set); persisted 0600 (architecture.md §3 Phase 6).
- **Read-only debug pages with per-turn prompt capture + stage latency breakdown** —
  the loopback server exposes `/chatlog`, `/prompts` (the *exact* assembled LLM prompt
  per turn, captured to `anamanti_promptlog.jsonl`), `/sqlite`, `/helix` (GraphRAG
  stats behind a `GraphView` seam that reports "disabled" on FTS). Click-to-expand
  chatlog rows show a per-turn `TurnTiming` breakdown (STT finalize → System-1 decision
  → fast-path fetch → System-2 recall/prompt/first-token → TTS first chunk); timing
  fields are `Optional` + `skip_serializing_if`, so pre-feature log lines still parse
  (architecture.md §3 Phase 6).
- **Installed-voice intersection** — the TTS voice dropdown intersects Piper's
  *advertised* catalog with the `<name>.onnx` files actually on disk, so it never
  offers a voice that would fail at synth time (architecture.md §3 Phase 6).
- **Anamanti Core as a fourth thin "voice front-end" on Cadora** — the shopping-list
  tool `POST`s a canonical `{action:"add_shopping_item",...}` over a `vl_…` voice-link
  token (minted by a 6-digit pairing code), never touching Supabase/RLS; the LLM has
  already parsed intent, so Cadora's own NLU is skipped ("one brain, thin front-ends";
  Plan.MD, agents.md).

---

## 15. The two cross-cutting meta-patterns

Almost every tradeoff above is enabled by two disciplines applied consistently:

1. **Pluggable traits behind seams, with a cheap committed default + an opt-in heavy
   implementation.** The full roster: `Transcriber` (wyoming / whisper-rs),
   `SpeechGate` (energy / Silero), `Recall` + `GraphView` (HelixDB GraphRAG / SQLite
   FTS), `LlmBackend` (ollama / anthropic / openai / mock) and `llm.engine`
   (rig / native), the System-1 `DecisionEngine` (laya-serve / jev / mock / none),
   `WeatherProvider` (Visual Crossing / Open-Meteo), `PlacesProvider`,
   `DirectionsProvider` (Mapbox), `CalendarSource`, `GroceryController` (Cadora),
   `SpotifyController`, `PhotoSource`/`GoogleAuthenticator`, and the speaker embedder.
   Each seam is what makes a given local-vs-cloud or on-device-vs-off-device decision a
   one-line swap.

2. **Zero-regression opt-in defaults.** New subsystems merge *inert*: System-1 default
   `none`, `stt.engine` stays `wyoming`, `vad.engine` stays `energy`, the device
   snapclient defaults off, in-app capture gain defaults to 0. A committed default is
   byte-for-byte unchanged until a flip is gated on hardware validation (M4 Mac Mini /
   Echo Show far-field). Combined with **inject-a-trait-with-a-fake** unit testing
   (mock providers, mock JSON-RPC/mpv sockets, deterministic mock embedders) so
   network features test offline, and **failure-degrades-gracefully** as a rule (any
   System-1 error/low-confidence defers to System-2; a missing key drops a tool from
   the advertised set rather than erroring; a lost graph falls back to FTS).
</content>
</invoke>
