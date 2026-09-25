# Anamanti Core — Mac Mini assistant pipeline (Phase 4)

**Anamanti Core** (crate `anamanti_core`, binary `anamanti-core`) is the "brain" of
Anamanti. It runs on the M4 Mac Mini as a single Wyoming host the Echo Show
(running **Anamanti Display**) discovers over mDNS, and wires:

```
device ──audio──►  STT (Whisper)  ──transcript──►  LLM + memory  ──text──►  TTS (Piper)  ──audio──► device
        (Wyoming server)          (Wyoming client)                          (Wyoming client)
```

It is a Wyoming **server** to the device and a Wyoming **client** to the
off-the-shelf Whisper (STT, server-side VAD) and Piper (TTS) Wyoming servers.
Whisper and Piper are not implemented here — bring your own (rhasspy ecosystem).

## Layout

| Path | Role |
| --- | --- |
| `src/wyoming/` | Wyoming wire codec + STT/TTS client sessions |
| `src/llm/` | Pluggable `LlmBackend` trait — `ollama`, `anthropic` (Claude), `mock` |
| `src/memory/` | SQLite + FTS5 store (explicit + inferred); **optional embedded HelixDB GraphRAG** backend + chat log + OpenAI-embedding ingester |
| `src/orchestrator.rs` | `Pipeline::run_turn` — the full STT → LLM+memory → TTS turn |
| `src/server.rs` | Device-facing TCP accept loop |
| `src/discovery.rs` | mDNS advertisement of `_wyoming._tcp` |
| `src/config.rs` | Environment-driven configuration |

## Run

```bash
cargo run --release            # advertises _wyoming._tcp on :10700, serves turns
cargo test                     # unit + full-pipeline integration tests
cargo clippy --all-targets -- -D warnings
```

## Configuration (environment)

| Variable | Default | Meaning |
| --- | --- | --- |
| `ANAMANTI_LLM_BACKEND` | `ollama` | `ollama` \| `anthropic` \| `mock` |
| `ANAMANTI_OLLAMA_URL` | `http://127.0.0.1:11434` | Ollama endpoint |
| `ANAMANTI_OLLAMA_MODEL` | `llama3.2` | Ollama model |
| `ANTHROPIC_API_KEY` | — | required for `anthropic` |
| `ANAMANTI_ANTHROPIC_MODEL` | `claude-opus-5` | Claude model |
| `ANAMANTI_ANTHROPIC_MAX_TOKENS` | `1024` | reply cap (spoken replies stay short) |
| `ANAMANTI_STT_ADDR` | `127.0.0.1:10300` | Whisper Wyoming server |
| `ANAMANTI_TTS_ADDR` | `127.0.0.1:10200` | Piper Wyoming server |
| `ANAMANTI_TTS_VOICE` | — | optional Piper voice name |
| `ANAMANTI_BIND_ADDR` | `0.0.0.0:10700` | device-facing bind address |
| `ANAMANTI_DB_PATH` | `anamanti_memory.sqlite` | memory database path |
| `ANAMANTI_SERVICE_NAME` | `Anamanti Core` | mDNS instance name |
| `ANAMANTI_MEMORY_BACKEND` | `helix` | recall backend: `helix` (GraphRAG, default) \| `sqlite` (FTS) |
| `ANAMANTI_CHATLOG_PATH` | `anamanti_chatlog.jsonl` | append-only turn log (always written) |
| `ANAMANTI_HELIX_PATH` | `anamanti_helix` | embedded HelixDB on-disk store root |
| `OPENAI_API_KEY` | — | required for `helix` (embeddings) |
| `ANAMANTI_EMBED_MODEL` | `text-embedding-3-small` | embedding model |
| `ANAMANTI_EMBED_DIMS` | `1536` | embedding dimensionality |
| `ANAMANTI_EXTRACT_MODEL` | `claude-haiku-4-5` | entity-extraction model (needs `ANTHROPIC_API_KEY`) |
| `ANAMANTI_INGEST_INTERVAL_SECS` | `30` | background ingester cadence |
| `ANAMANTI_OPENAI_BASE_URL` / `ANAMANTI_ANTHROPIC_BASE_URL` | provider defaults | override API base (self-host/testing) |

### GraphRAG memory (embedded HelixDB)

HelixDB's engine crate is compiled **in-process** (always — there is no `helix`
cargo feature to toggle; no server, no Docker). With `memory_backend=helix` (the
default) the
Anamanti Core writes each turn to the JSONL chat log, a background ingester
embeds new turns (OpenAI `text-embedding-3-small`) and extracts entities (Claude
Haiku) into a graph (`User/Turn/Memory/Entity` nodes; `SAID/MENTIONS/ABOUT/
FOLLOWS/KNOWS` edges), and recall does vector KNN + graph expansion, injected
into the LLM system prompt. Requires `OPENAI_API_KEY`; without it the
Anamanti Core logs a warning and falls back to SQLite FTS. Set
`ANAMANTI_MEMORY_BACKEND=sqlite` to force pure FTS recall (the HelixDB engine is
always compiled in). See [`../memory_plan.md`](../memory_plan.md) for the full
design + status.

The offline `mock` backend needs no model server, so
`ANAMANTI_LLM_BACKEND=mock cargo run` exercises the pipeline shape end to end (you
still need Whisper/Piper for real audio; the integration tests mock those too).

See [`../architecture.md`](../architecture.md) §2.3 and [`../Plan.MD`](../Plan.MD)
Phase 4 for design and scope.
