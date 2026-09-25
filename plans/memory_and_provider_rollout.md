# Rig STT Anamanti Core — Rollout Tracker

_As of 2026-09-16_

> **Reality check (2026-09-16):** the `mac/` Anamanti Core crate already implements most of this plan — a pluggable `LlmBackend` (ollama / anthropic / mock) with runtime hot-swap (`settings.rs`, `config.rs`), concurrent SQLite + embedded-HelixDB memory (`memory/`), and token streaming (`ReplyToken`). The one genuinely missing item — and the requested scope addition — is the **HTTP config page**, now **delivered** (`anamanti-core/src/webconfig.rs`, wired in `main.rs`). The remaining open item is whether to actually adopt **rig-core** in place of the working hand-rolled abstraction (see Phase 1 note).

## Overview

Goal: drive STT-to-voice latency down while keeping the memory stacks and the LLM layer swappable. The whole build rests on two clean seams:

- **Memory ⟂ LLM** — SQLite (chat history) and HelixDB (vector/graph) sit behind a `MemoryProvider` interface. The LLM layer receives an assembled context only; it never imports a DB client. This is what lets Qwen and frontier APIs swap freely.
- **Provider ⟂ App** — Rig's `Tool` trait plus a `CompletionModel` abstraction behind `LlmRouter`. The app calls `router.agent()` and never knows whether Ollama or a frontier API answered.

Scope note: the Ollama web-search tool needs a running web server, so this rollout also stands up a small Anamanti Core-served **config page** (no auth) for changing settings live — see the Scope Addition section.

## Phase 1 — Rig Framework Foundation (Day 1)

> **Done — rig-core adopted behind the existing seam, now with tool calling.** `rig-core` v0.42 is wired in as an optional `rig` feature; `anamanti-core/src/llm/rig.rs` implements `LlmBackend` on rig's `CompletionModel`/streaming for both ollama and anthropic. Select at runtime with `ANAMANTI_LLM_ENGINE=rig` (`config.rs` → `LlmFactory.engine`); native HTTP backends remain the default fallback.
>
> **Tool-calling parity (beyond native — native has no tools):** the plan's `InternetSearch` is a real rig `PortableTool` with typed `SearchArgs`, backed by a pluggable `SearchProvider` (default keyless DuckDuckGo Instant Answer, no auth). `respond` runs a bounded negotiation loop (`MAX_TOOL_ROUNDS`): stream a pass, and if the model calls a tool, execute it, thread the result back as a tool-result message, and stream again — forwarding text deltas throughout. A no-tool turn still streams straight through in one pass. Enable with `ANAMANTI_WEB_SEARCH=1`. End-to-end test drives model → tool call → tool exec → streamed answer. Full suite **63 green**; native + helix+rig compile.
>
> **Live-validated against real Ollama (`qwen2.5:7b`, 2026-09-16)** via `examples/rig_smoke.rs`:
> - Plain streaming turn — correct answer, 7 chunks, ~5 s.
> - Tool-calling turn (`ANAMANTI_WEB_SEARCH=1`) — the model emitted `internet_search({"query":"DuckDuckGo company","max_results":1})` (confirmed in logs), the tool hit real DuckDuckGo, and the answer streamed from the result. Full model → tool → result → streamed-answer loop works on real services.
>
> Remaining before retiring the hand-rolled clients: live-soak the **anthropic** rig path (needs an API key), then flip the default engine and delete `ollama.rs`/`anthropic.rs`. The ollama rig path is validated; anthropic is unit-tested only.

One unified agent path that runs against either provider via a single env var.

| Task | Detail | Status |
| --- | --- | --- |
| Dependency injection | `Cargo.toml`: `rig-core`, `tokio` (full), `serde`/`serde_json`, search dep (`reqwest` + Tavily/Serper over `duckduckgo` for reliable structured results) | Not started |
| Provider abstraction | `LlmRouter` enum `{ Ollama, OpenAI, Anthropic }` from `LLM_PROVIDER`; returns a unified agent handle | Not started |
| Tool structuring | `InternetSearch` implements Rig's `Tool` trait; typed `SearchArgs { query, max_results }` with serde schema | Not started |

**Exit criteria:** `LLM_PROVIDER=ollama` and `=openai` both return a valid completion from the same call site; `InternetSearch` invokes end-to-end.

**Risk:** per-provider trait surfaces differ — wrap in a thin `AgentHandle` so provider quirks don't leak upward.

## Phase 2 — Hybrid Memory Context Pipeline (Days 2–3)

Assemble memory context concurrently and inject it without coupling the DBs to the LLM.

| Task | Detail | Status |
| --- | --- | --- |
| Context pre-fetch | In the STT receiver loop, `tokio::join!(sqlite_history(), helix_subgraph())` so SQLite reads and HelixDB traversal overlap — behind the `MemoryProvider` seam | Not started |
| System-prompt packaging | Merge results into a `MemoryContext` struct, render into the Rig agent `preamble` dynamically per turn | Not started |
| Latency guardrail | Anchored regex prefilter (`what is the weather`, `search for…`) that skips Pass-1 tool negotiation and calls search directly | Not started |

**Exit criteria:** context-assembly latency logged; guardrail demonstrably skips a negotiation round-trip; swapping provider needs zero memory-code changes (proves the seam).

**Risk:** regex over-matching → false tool calls. Keep the pattern list small, anchored, logged; it is a fast path, not the primary router.

## Phase 3 — Dual-Mode Execution Routing (Days 4–5)

Local-first execution with a safe frontier failover.

| Task | Detail | Status |
| --- | --- | --- |
| Local fast track | Benchmark the Ollama `qwen2.5` agent across the full path (STT out → tool exec → final payload); capture p50/p95 per stage | Not started |
| Frontier failover | On local timeout, malformed tool-call, or nested-tool failure, intercept and re-route the same prompt to a frontier agent | Not started |
| Model pinning | Frontier model id lives in config, not code (names move fast); pull current Anthropic/OpenAI ids before wiring | Not started |

**Exit criteria:** a forced-failure test (kill Ollama / feed a nested-tool prompt) completes via frontier with a bounded, logged latency penalty and a reason code.

**Risk:** double-execution / cost blowup on spurious failover — cap at single retry, add a timeout budget and a per-session failover counter.

## Phase 4 — Streaming & STT Voice Sync (Days 6–7)

First-syllable audio before slow tool calls finish — collapse perceived latency.

| Task | Detail | Status |
| --- | --- | --- |
| Token streaming | Migrate `.prompt()` → Rig's `.stream_prompt()` | Not started |
| Chunk handling | Feed the token stream into a `tokio::sync::mpsc::channel`; TTS consumes chunks as they arrive, vocalizing opening tokens while a fetch is still in flight | Not started |
| Segmentation buffer | Buffer tokens into word/sentence boundaries between the mpsc receiver and TTS so fragments aren't spoken | Not started |

**Exit criteria:** measured time-to-first-audio drops well below time-to-full-response; streaming works on both local and frontier providers (validates the Phase-1 seam under streaming).

**Risk:** raw tokens aren't speakable and tool-calls can interrupt mid-stream — handle both in the segmentation stage.

**Suggested de-risk:** pull a thin slice of streaming forward into Phase 3 — it is what actually masks tool latency, so validate `stream_prompt()` on the local agent before polishing failover.

## Scope Addition — Anamanti Core Web Server + Config Page

Acknowledged scope creep, but cheap: the Ollama web-search tool already forces a web server into the Anamanti Core, so a config page piggybacks on infrastructure we have to build anyway. **No security** — a plain page served from the Anamanti Core that reads and writes config. Local/trusted-network use only; do not expose it publicly.

What the page configures (all Anamanti Core-side settings, editable without a restart where feasible):

| Setting | Maps to |
| --- | --- |
| Active provider | `LLM_PROVIDER` (ollama / openai / anthropic) |
| Model ids | local `qwen2.5` tag, frontier model id |
| Timeouts & failover | local timeout, retry cap, failover on/off |
| Guardrail patterns | the regex fast-path phrase list |
| Search provider | Tavily / Serper / DuckDuckGo + API key field |

Status — **delivered** in `anamanti-core/src/webconfig.rs` (dependency-free HTTP over tokio, matching the hand-rolled Wyoming style), wired into `main.rs`:

- `GET /` — the static HTML config page.
- `GET /config` — live settings as JSON.
- `POST /config` — apply `{engine?, llm_backend?, llm_model?, tts_voice?, web_search?}` (same JSON shape as the `anamanti-set-settings` control frame; `tts_voice: null` clears). **Engine (native/rig) and the web-search tool are now runtime-swappable from the page** — flip them live between voice tests, no restart. (Web search needs a rig-built binary and a search key; see below.)
- Binds to `127.0.0.1:8730` by default (no auth → loopback only); override or disable with `ANAMANTI_CONFIG_ADDR` (`off` to disable). Best-effort: a bind failure logs and disables the page, never stops the Anamanti Core.
- Backed directly by the existing `SharedSettings` — so edits **apply live** (atomic hot-swap, no restart), resolving the open decision below. 8 unit/socket tests added; full suite green on both `--no-default-features` and the default `helix` build.

Not yet on the page (settings that don't exist as runtime state yet): timeouts, failover caps, guardrail patterns, search provider/keys. These land when their backing config becomes runtime-swappable.

## Success Metrics & Open Questions

Cross-phase metrics:

- **Time-to-first-audio (TTFB)** — headline metric; set a target after the Phase-3 local baseline lands.
- **End-to-end p95** — STT-out → final payload, local vs frontier.
- **Failover rate & reason codes** — should stay low and explainable.
- **Decoupling proof** — a provider swap and a memory-backend swap each require zero cross-layer edits.

Open questions:

- [x] **Adopt rig-core?** — **done**, behind the `rig` feature + `ANAMANTI_LLM_ENGINE=rig`, native kept as fallback. Remaining: parity (tool-calling) before making it the default and removing the hand-rolled clients.
- [x] Search provider — **DuckDuckGo Instant Answer (keyless) as default**, behind a pluggable `SearchProvider` trait so Tavily/Serper can drop in later.
- [ ] Frontier fallback model — which id to pin in config (needs current Anthropic/OpenAI ids)?
- [ ] Guardrail phrase list — seed set for the regex fast path?
- [x] Config page apply model — **resolved: live hot-swap** via `SharedSettings`, loopback + no auth.
- [x] Config persistence — **done**: applied changes persist to `anamanti_settings.json` (0600, gitignored — holds the search key) and reload at boot, overlaying env defaults. Disable with `ANAMANTI_SETTINGS_PATH=off`.
