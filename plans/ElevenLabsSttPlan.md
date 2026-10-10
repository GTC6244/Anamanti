# ElevenLabsSttPlan.md

Add **ElevenLabs Scribe v2 Realtime** as a third, selectable Anamanti Core **STT**
engine, behind the existing `Transcriber` / `SttEngine` seam — alongside the
external `wyoming-faster-whisper` server and the in-process `whisper-rs`
(whisper.cpp) engine.

Read with [`architecture.md`](./architecture.md) §2.3 (Mac services),
[`python-to-rust-whisper.md`](./python-to-rust-whisper.md) (which introduced the STT
seam this plan reuses), and [`agents.md`](../agents.md) (locked decisions). This
plan touches STT only.

## Framing: the seam already exists

STT is **not** a hard-wired `if/else`. `python-to-rust-whisper.md` landed a
`Transcriber` / `SttEngine` trait seam in `anamanti-core/src/stt/mod.rs` that
mirrors the pluggable-LLM pattern, and both current engines implement it:

- `trait SttEngine` (`stt/mod.rs:51`) — the boot-lived factory; one instance held by
  the `Pipeline`, reused across turns. `async fn begin(&self, format: AudioFormat)
  -> Result<Box<dyn Transcriber>>`.
- `trait Transcriber` (`stt/mod.rs:65`) — one per-turn session: `forward_pcm(pcm)`,
  `read_event() -> Option<SttEvent>`, `finish()` (idempotent).
- `enum SttEvent { Transcript(String), Other }` (`stt/mod.rs:38`).

So "make STT pluggable like the other services" is **substantially already done**.
The remaining work is (a) adding an ElevenLabs engine implementation behind the
seam, and (b) closing the one structural gap vs. the LLM pattern: engine
construction currently lives inline in `main.rs:208-242` instead of in a
`Config::build_*` factory. This plan does both.

## Decisions (locked for this plan)

- **Selection = runtime hot-swap from the config page** (revised 2026-10-09 at the
  user's request — the original boot-time plan is superseded). A config-page **Speech**
  tab (`/stt`) picks the engine (`wyoming`/`whisper-rs`/`elevenlabs`) + a masked
  ElevenLabs key, applied **live on the next turn, no restart** — exactly like the LLM,
  VAD, embeddings, and weather-provider controls. The live selection + ElevenLabs
  config/key live in `RuntimeSettings.stt` (`SttRuntime`), seeded from `stt.*` +
  `ELEVENLABS_API_KEY` and persisted to `settings_path`. The per-turn
  `Pipeline::build_stt_transcriber(&runtime)` picks the engine from the snapshot
  (mirrors VAD's `build_speech_gate`); the in-process whisper model is preloaded at boot
  **regardless** of the selected engine (mirrors the Silero model preload) so a later
  swap to `whisper-rs` is instant. An unavailable selection (whisper-rs with no model /
  feature; elevenlabs with no key) warns and falls back to the Wyoming path — mirroring
  the VAD gate's graceful fallback.
- **Final-only transcripts.** ElevenLabs streams `partial_transcript` (interim) and
  `committed_transcript` (final) events; we consume **only the committed transcript**
  and surface it as the single `SttEvent::Transcript`, exactly like the existing
  engines. Partials are mapped to `SttEvent::Other` and discarded. **No change to the
  `orchestrator.rs` pump loop or the Flutter display.** (Streaming interim text to the
  display is a possible future enhancement, not v1 — and only ElevenLabs could
  produce it.)
- **The Core's VAD stays the end-of-speech authority.** We connect with
  `commit_strategy=manual` and drive the commit from the existing VAD → `audio-stop`
  path (`Transcriber::finish()`), so ElevenLabs' own server-side VAD / turn
  prediction is **not** used. This keeps the locked VAD decision (`SpeechGate`,
  Silero default) completely untouched — ElevenLabs is a transcriber, not a
  segmenter.
- **Always compiled, no Cargo feature gate.** ElevenLabs is a pure-Rust network
  client with no native/toolchain dependency (unlike `whisper-rs`, which is feature-
  gated because of its C++/ggml build). It is therefore always compiled in, like the
  cloud LLM backends (`anthropic`/`openai`). It only activates when selected at
  runtime.
- **API key is a secret**, handled exactly like the other provider keys:
  `ELEVENLABS_API_KEY` env var as a boot seed, also settable (masked) from the Core
  config page and persisted to `settings_path` — never in either JSON file.
- **Per-turn WebSocket.** `begin()` opens a fresh WebSocket; it closes on `finish()`
  / drop — mirroring the per-turn `SttSession` / `WhisperLocal` lifetime. (A
  persistent, pre-warmed connection to shave the handshake off first-token latency is
  a noted optimization, not v1.)

## The ElevenLabs Realtime contract (as of 2026-10, verified against the docs)

- **Endpoint:** `wss://api.elevenlabs.io/v1/speech-to-text/realtime` (regional hosts
  exist: `api.us.`, `api.eu.residency.`, `api.in.residency.`, `api.sg.residency.` —
  the base host is a config knob, default the global one).
- **Auth:** `xi-api-key` request header (server-side; the single-use `token` query
  param is the client-side alternative and is not used here).
- **Query params (connection config):** `model_id=scribe_v2_realtime`,
  `audio_format=pcm_16000`, `language_code` (ISO 639), `commit_strategy=manual`,
  plus optional `vad_*` / `keyterms` / `include_timestamps` (unused in v1).
- **Client → server:** JSON `input_audio_chunk` frames —
  `{ "message_type": "input_audio_chunk", "audio_base_64": "<b64 LE-i16 PCM>",
  "commit": <bool>, "sample_rate": 16000 }`. A chunk with `"commit": true` finalizes
  the current segment.
- **Server → client:** JSON frames discriminated by `message_type` —
  `session_started`, `partial_transcript { text }`, `committed_transcript { text }`,
  `committed_transcript_with_timestamps`, plus a rich error taxonomy (`auth_error`,
  `quota_exceeded`, `rate_limited`, `commit_throttled`, `input_error`,
  `session_time_limit_exceeded`, …).
- **Audio match:** the Echo Show already streams 16 kHz, 16-bit, mono LE PCM — an
  exact match for `pcm_16000` / `sample_rate: 16000`, so **no resampling** is needed.

The mapping onto the trait is clean and loop-compatible:

| `Transcriber` method | ElevenLabs action |
|---|---|
| `begin(format)` | open WS (query params + `xi-api-key`), await `session_started` |
| `forward_pcm(pcm)` | send `input_audio_chunk` with `commit:false`, base64 of the chunk |
| `finish()` (idempotent) | send one `input_audio_chunk` with `commit:true` (empty/last audio) |
| `read_event()` | read WS frames: `partial_transcript` → `Other`; `committed_transcript` → `Transcript(text)`; error frames → `Err` |

Because `commit_strategy=manual`, the server emits a `committed_transcript` only
after our `finish()` commit — so `read_event()` behaves just like `WhisperLocal`:
interim/`Other` (or pending) until finalize, then one `Transcript` yielded once. This
slots into the existing two-arm `select!` with no loop changes.

## Files touched

- **New:** `anamanti-core/src/stt/elevenlabs.rs` — `ElevenLabsEngine` (`SttEngine`) +
  `ElevenLabsTranscriber` (`Transcriber`) + the wire message (de)serialization.
- `anamanti-core/src/stt/mod.rs` — `pub mod elevenlabs;` (unconditional).
- `anamanti-core/src/config.rs`:
  - `SttEngineKind::ElevenLabs` + `from_label` aliases (`"elevenlabs"`,
    `"eleven-labs"`, `"11labs"`).
  - `ElevenLabsSttConfig` sub-block on `SttConfig` (`model_id` default
    `scribe_v2_realtime`, `base_url` default `wss://api.elevenlabs.io`,
    `commit_strategy` default `manual`; `language` reuses the existing
    `SttConfig.language`). Mirror the `FileStt` / `deny_unknown_fields` / merge
    pattern + unit tests.
  - **New factory** `Config::build_stt(&self) -> Result<Option<Arc<dyn SttEngine>>>`,
    mirroring `build_llm` (`config.rs:1653`). `Wyoming` → `Ok(None)` (keeps the
    existing `ServiceConnector` fallback path untouched); `WhisperLocal` → `Some(...)`
    (moved out of `main.rs`); `ElevenLabs` → `Some(ElevenLabsEngine::new(...))`.
- `anamanti-core/src/main.rs:208-242` — replace the inline `match` with a single
  `if let Some(engine) = config.build_stt()? { pipeline = pipeline.with_stt_engine(engine); }`.
  Keep the clear "feature not compiled" boot error for `whisper-rs` inside
  `build_stt`.
- `anamanti-core/src/settings.rs` — register `ELEVENLABS_API_KEY` as a masked,
  persisted secret alongside the existing provider keys; thread it into `build_stt`.
- `anamanti-core/Cargo.toml` — add `tokio-tungstenite` (rustls TLS) + `futures-util`
  (if not already pulled); `base64` and `serde_json` are already present.
- Docs: `anamanti.example.json`, `agents.md` (Mac-services STT note + secret-env
  list + `stt` block), `architecture.md` §2.3, `Plan.MD` (decision row), `README.md`,
  `TODO.md`.

## Stages

**Guiding principle (inherited): refactor first, add engine second.** Each stage is
independently green and reversible; the default stays `wyoming`, so rollback is a
one-line config change at every step.

### Stage 1 — Factory refactor (no new deps, behavior identical) ✅ DONE
- Add `Config::build_stt()` and move the `main.rs` whisper/wyoming selection into it.
  Add `SttEngineKind::ElevenLabs` + the `ElevenLabsSttConfig` block, parsed and
  defaulted, but `build_stt` returns a clear "not yet implemented" error for the
  ElevenLabs arm.
- **Exit check:** `cargo test` + `cargo clippy -- -D warnings` green; the
  `tests/pipeline.rs` mock-Whisper harness passes unmodified; `wyoming` and
  `whisper-rs` behave exactly as before. Zero behavioral change.

### Stage 2 — The ElevenLabs engine behind the seam ✅ DONE
- Add `tokio-tungstenite` (rustls). Implement `elevenlabs.rs`: connection with query
  params + `xi-api-key`; `forward_pcm` base64 → `input_audio_chunk`; `finish` commit;
  `read_event` frame parsing (partials → `Other`, committed → `Transcript`, error
  frames → typed `Err`). Ensure `read_event` is **cancel-safe** inside the `select!`
  (tungstenite's `next()` is), and that `Drop` closes the socket cleanly (barge-in /
  `anamanti-interrupt` aborts the turn mid-stream).
- Wire `build_stt`'s ElevenLabs arm to construct `ElevenLabsEngine::new(api_key,
  config)`.
- **Unit test:** a local mock WebSocket server asserts the `input_audio_chunk`
  framing (incl. the commit flag) and that a server `committed_transcript` surfaces as
  the single `Transcript`; error frames become `Err`.
- **Exit check:** builds + unit tests + `clippy -D warnings` + `fmt` green on the
  default build (no new feature flag).

### Stage 3 — Config-page UI + runtime hot-swap ✅ DONE
- `RuntimeSettings.stt` (`SttRuntime`: engine, ElevenLabs model/base/key, language,
  `whisper_available`) seeded in `Config::shared_settings` from `stt.*` +
  `ELEVENLABS_API_KEY`, with the persisted overlay; persisted to `settings_path` via
  `persisted_snapshot` (the key plaintext, 0600). `SharedSettings::apply_stt` +
  `stt_view` (never returns the key) mirror `apply_weather_tool`.
- Pipeline: boot-preload the whisper engine via `Config::build_whisper_engine` +
  `Pipeline::with_whisper_engine` regardless of selection (mirrors `with_silero`);
  per-turn `build_stt_transcriber(&runtime)` replaces the fixed boot engine.
- Config page: a new **Speech** tab (`/stt`, `webconfig/stt.html`) with a
  `GET /stt/status.json` + `POST /stt/save` pair (engine select, masked key, model/base),
  "applies live" copy, and the `whisper-rs` option disabled when unavailable. Sidebar
  link added.

### Stage 4 — Live validation (M4 Mac Mini + real Echo Show) ⏸ PENDING HARDWARE
- Isolated test Core (ports 10701/8731, own data dir, mock or real LLM), `stt.engine=
  elevenlabs`. Validate: `session_started` handshake; a real device turn transcribes
  correctly end-to-end; the Core VAD fires and drives the commit; the anti-
  hallucination guard still discards empty/no-speech turns.
- Record **first-token / finalize latency** vs `whisper-rs` (base) and the Wyoming
  server, and note network-dependence (this is the first STT engine that needs the
  internet — unlike both current engines, which are LAN/in-process).
- Decide error/outage policy: on connect/auth/quota failure, **fail the turn with a
  spoken/visible error** (v1). Optional, deferred: automatic fallback to a local
  engine on connection failure.

### Stage 5 — Docs ◻ PARTIAL
- **Done:** `anamanti.example.json` (the `stt.elevenlabs` sub-block), `agents.md`
  (feature-plan pointer + Mac-services STT note), `Plan.MD` (decision row), this plan's
  decision log.
- **Remaining:** `architecture.md` §2.3 (add the third engine behind the seam), the
  `ELEVENLABS_API_KEY` row in the `agents.md` secret-env list + the deploy runbook,
  `README.md` Mac-services STT note, `TODO.md`. Fold in at cutover time.

## Rollout / rollback

All three engines ship in one binary; select per-instance via `stt.engine`. Run
ElevenLabs in a test worktree (ports 10701+) alongside prod on `wyoming`/`whisper-rs`;
flip an instance by editing its `anamanti.json` and restarting; roll back the same
way. No flag day, no default change (the committed default stays `wyoming`).

## Risks to watch

- **Network dependency.** First STT engine that requires the internet; a LAN-only or
  offline household loses STT entirely on this engine. Document it; keep a local
  engine configured as the fallback choice.
- **Latency / jitter.** Round-trip to ElevenLabs + per-turn WS handshake vs
  in-process whisper (~169 ms on M4). Measure before any cutover; consider the
  persistent/pre-warmed-socket optimization only if the handshake proves material.
- **Cancellation safety.** `read_event` lives in a `biased` two-arm `select!`; the WS
  read must be cancel-safe and the socket must close on drop so a barge-in abort
  doesn't leak a session. Mirror the care in the existing loop comments.
- **Commit semantics.** Confirm a `commit:true` chunk with empty/zero trailing audio
  reliably yields exactly one `committed_transcript`; handle `commit_throttled` /
  `insufficient_audio_activity` as a graceful empty transcript (reuses the existing
  no-speech guard), not a hard error.
- **New TLS/WS deps** (`tokio-tungstenite` + rustls) added to the crate — keep TLS
  backend consistent with what the HTTP clients already pull to avoid a second TLS
  stack.
- **Session limits / keepalive.** The API has a session time limit and a keepalive
  knob; the per-turn-connection model sidesteps both (sessions are seconds long), but
  note it if the persistent-connection optimization is ever taken.

## Decision log

- 2026-10-09 — **Config UI + runtime hot-swap landed (supersedes the boot-time plan).**
  At the user's request the engine is now selectable live from a config-page **Speech**
  tab (`/stt`) with no restart, matching the LLM/VAD/weather controls. Added
  `RuntimeSettings.stt` (`SttRuntime`, bundled so the widely-constructed struct gains one
  field) + `SttView`/`SttUpdate` + `apply_stt`/`stt_view` + persistence (`stt_*` fields on
  `PersistedSettings`, seeded in `shared_settings`); `SttEngineKind::as_label`; the
  whisper model preloads at boot via `Config::build_whisper_engine` +
  `Pipeline::with_whisper_engine` (replacing the per-engine `build_stt`), and the per-turn
  `Pipeline::build_stt_transcriber(&runtime)` picks the engine from the snapshot (ElevenLabs
  built per turn from the live key, graceful Wyoming fallback when a selection is
  unavailable). New `webconfig/stt.html` + `/stt` route + `/stt/status.json` + `/stt/save`
  + sidebar link. Tests: `apply_stt` roundtrip, `/stt` render, `/stt/save` live switch
  (+ the Stage-2 mock-WebSocket test). Green: `clippy --all-targets -D warnings`, `fmt`,
  396 lib tests + integration suites. Live M4/device validation still pending (Stage 4).
- 2026-10-09 — **Stages 1–2 landed + verified on a dev box.** `Config::build_stt`
  factory folds the old inline `main.rs` selection into the LLM-style pattern (Wyoming
  → `None`, whisper-rs/elevenlabs → `Some`); `SttEngineKind::ElevenLabs` +
  `stt.elevenlabs{model_id,base_url}` config block; `stt/elevenlabs.rs`
  (`ElevenLabsEngine`/`ElevenLabsTranscriber`) on `tokio-tungstenite` 0.24 (rustls,
  shares reqwest's TLS stack — only 3 small new crates). `ELEVENLABS_API_KEY` read in
  `build_stt` (env), fail-loud if absent. A mock-WebSocket-server unit test asserts the
  request shape (query params + `xi-api-key`), the commit framing, partial-discard, and
  the committed transcript. Green: default `cargo build`, `clippy --all-targets -D
  warnings`, `fmt`, 393 lib tests (incl. the new one), **and** the `stt-whisper-local`
  feature build (the moved factory code). Config-page secret UI + runtime hot-swap
  deferred (Stage 3 partial); live M4/device validation + the `architecture.md`/
  `README.md`/`TODO.md` doc rows pending (Stages 4–5).
- 2026-10-09 — Plan drafted. Engine = ElevenLabs **Scribe v2 Realtime** behind the
  existing `SttEngine`/`Transcriber` seam. Locked for v1: **boot-time** selection
  (not runtime hot-swap), **final-only** transcripts (no pump-loop/display changes),
  Core VAD remains the segmenter (`commit_strategy=manual`), **always-compiled** (no
  Cargo feature), `ELEVENLABS_API_KEY` as a masked/persisted secret, per-turn
  WebSocket. Factory refactor (`Config::build_stt`) folds the current inline
  `main.rs` selection into the LLM-style pattern. Default engine stays `wyoming`.
