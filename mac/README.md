# ambient_orchestrator — Mac Mini assistant pipeline (Phase 4)

The "brain" of the Ambient Smart Display voice assistant. It runs on the M4 Mac
Mini as a single Wyoming host the Echo Show discovers over mDNS, and wires:

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
| `AMBIENT_LLM_BACKEND` | `ollama` | `ollama` \| `anthropic` \| `mock` |
| `AMBIENT_OLLAMA_URL` | `http://127.0.0.1:11434` | Ollama endpoint |
| `AMBIENT_OLLAMA_MODEL` | `llama3.2` | Ollama model |
| `ANTHROPIC_API_KEY` | — | required for `anthropic` |
| `AMBIENT_ANTHROPIC_MODEL` | `claude-opus-5` | Claude model |
| `AMBIENT_ANTHROPIC_MAX_TOKENS` | `1024` | reply cap (spoken replies stay short) |
| `AMBIENT_STT_ADDR` | `127.0.0.1:10300` | Whisper Wyoming server |
| `AMBIENT_TTS_ADDR` | `127.0.0.1:10200` | Piper Wyoming server |
| `AMBIENT_TTS_VOICE` | — | optional Piper voice name |
| `AMBIENT_BIND_ADDR` | `0.0.0.0:10700` | device-facing bind address |
| `AMBIENT_DB_PATH` | `ambient_memory.sqlite` | memory database path |
| `AMBIENT_SERVICE_NAME` | `Ambient Orchestrator` | mDNS instance name |
| `AMBIENT_MEMORY_BACKEND` | `sqlite` | recall backend: `sqlite` (FTS) \| `helix` (GraphRAG) |
| `AMBIENT_CHATLOG_PATH` | `ambient_chatlog.jsonl` | append-only turn log (always written) |
| `AMBIENT_HELIX_PATH` | `ambient_helix` | embedded HelixDB on-disk store root |
| `OPENAI_API_KEY` | — | required for `helix` (embeddings) |
| `AMBIENT_EMBED_MODEL` | `text-embedding-3-small` | embedding model |
| `AMBIENT_EMBED_DIMS` | `1536` | embedding dimensionality |
| `AMBIENT_EXTRACT_MODEL` | `claude-haiku-4-5` | entity-extraction model (needs `ANTHROPIC_API_KEY`) |
| `AMBIENT_INGEST_INTERVAL_SECS` | `30` | background ingester cadence |
| `AMBIENT_OPENAI_BASE_URL` / `AMBIENT_ANTHROPIC_BASE_URL` | provider defaults | override API base (self-host/testing) |

### GraphRAG memory (embedded HelixDB)

The `helix` cargo feature (on by default) compiles HelixDB's engine crate
**in-process** — no server, no Docker. With `AMBIENT_MEMORY_BACKEND=helix` the
orchestrator writes each turn to the JSONL chat log, a background ingester
embeds new turns (OpenAI `text-embedding-3-small`) and extracts entities (Claude
Haiku) into a graph (`User/Turn/Memory/Entity` nodes; `SAID/MENTIONS/ABOUT/
FOLLOWS/KNOWS` edges), and recall does vector KNN + graph expansion, injected
into the LLM system prompt. Requires `OPENAI_API_KEY`; without it the
orchestrator logs a warning and falls back to SQLite FTS. Set
`AMBIENT_MEMORY_BACKEND=sqlite` to force pure FTS recall (the HelixDB engine is
always compiled in). See [`../memory_plan.md`](../memory_plan.md) for the full
design + status.

The offline `mock` backend needs no model server, so
`AMBIENT_LLM_BACKEND=mock cargo run` exercises the pipeline shape end to end (you
still need Whisper/Piper for real audio; the integration tests mock those too).

See [`../architecture.md`](../architecture.md) §2.3 and [`../Plan.MD`](../Plan.MD)
Phase 4 for design and scope.
