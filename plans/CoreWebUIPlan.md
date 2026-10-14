# Anamanti Core Web UI — Polish & Restructure Plan

> **Scope:** the loopback admin console served by Anamanti Core
> (`anamanti-core/src/webconfig/` — `shell_head.html`, `shell.js`, the per-page
> `*.html` bodies, `tailwind/input.css` → `panel.css`, and `routes.rs`).
> **Goal:** turn it into a polished, professional console with **logical grouping**,
> **one screen per page at 1920×1080**, and a first-class **Local ⇄ Cloud** story,
> because many settings let the household move a pipeline stage from the Mac to a
> third-party service and back.
> **Status:** implemented 2026-10-14 (phases 0–6). The §8 questions were decided by the
> user: stock purple kept · LAN is its own tier · entity extraction disclosed **and**
> toggleable · boot-only settings stay read-only · prompts joined to turns via a
> `turn_id` · Personality stays its own page · "Sends:" copy chosen per option (see
> `ui.js` `LOCALITY`). Not done from the plan: per-key provenance tags on the System
> page (it shows the effective boot config, not where each value came from) and a TTS
> voice preview button. Nothing here changes a locked decision in
> `agents.md` (the console stays loopback-only, plain static HTML + vendored
> precompiled Tailwind, POSTs stay `application/json` via `postJSON`, routes stay in
> the one `routes.rs` table).

---

## 1. Critical review of the current UI

The visual base is reasonable: Material 3 tokens, light and dark themes, consistent
icons, a responsive drawer. The problems are in structure, density, and consistency.

### 1.1 Information architecture follows the code, not the user's tasks

The sidebar has **16 links** in four groups (Settings / Memory / Logs / System), and
the groups follow the source modules:

| Problem | Where |
|---|---|
| **"Config" is a catch-all.** It mixes LLM engine/backend/model, provider keys, web search, the Piper voice, and VAD. | `config.html` |
| **The speech pipeline is spread over three pages.** STT is on *Speech*, VAD and the TTS voice are on *Config*, and transcript fixes are on *Pronunciation*. The pipeline is one flow (mic → VAD → STT → fixes → … → TTS), but the UI never shows it as one. | `stt.html`, `config.html`, `pronunciations.html` |
| **Secrets live on seven pages.** Anthropic/OpenAI/Tavily are on Config. Mapbox, Visual Crossing and Places are on Tools. ElevenLabs is on Speech, OpenRouter on System-1, AppSaid on Household, Spotify on Music, and Drive on Photos. Nothing shows "which keys do I have, and what uses them". | many |
| **Some keys can't be set where they're needed.** Embeddings says "needs `OPENAI_API_KEY`", but you can only set that key on Config, and only after picking the OpenAI LLM backend (the field is hidden otherwise). | `embeddings.html`, `config.html` `renderKeyRows` |
| **Integrations are scattered.** Cadora and AppSaid are under *Household*, Spotify under *Music*, Drive under *Photos*, and Mapbox/Weather/Places under *Tools*. They are all third-party services that turn on a voice tool. | `household.html`, `music.html`, `drive.html`, `tools.html` |
| **Memory mixes one setting with two raw data dumps.** *Embeddings* is a setting; *SQLite* and *HelixDB* are table browsers. | `embeddings.html`, `sqlite.html`, `helix.html` |
| **"Notify" combines a device list with a test form.** The connected-devices list is the only place the console shows which displays are online, and it's hidden behind a debug-style page. | `notifications.html` |
| **There's no landing page.** `/` opens straight into the Config form. Nothing shows at a glance whether STT and TTS are reachable, which engines are active, how many displays are connected, how fast recent turns were, or what is running in the cloud. | `routes.rs` `/` → `CONFIG_BODY` |

### 1.2 The layout wastes a 1920-wide screen

- `.content-inner { max-width: 48rem }` (768 px) on single-card pages, widening to
  `82rem` (1312 px) only when the page contains a `.card-grid`. With the 216 px
  sidebar, a 1920 px viewport has about **1650 px** of content area, so single-card
  pages (System-1, Embeddings, Photos, SQLite, HelixDB, Chat log, Prompts) use less
  than half of it.
- `.card-grid` uses CSS multi-column masonry (`columns: 21rem`). Cards fill
  **top-to-bottom per column**, so the order depends on card heights and the window
  width. Related cards land in unrelated places, and there's no way to say "this card
  spans two columns" except `column-span: all`.
- Forms are single-column label-over-field stacks. The six Household address fields
  alone take about 450 px of height. The People card stacks five full-width inputs per
  person, so a family of four scrolls well past one screen.
- Data pages (Chat log, Helix, SQLite) render unbounded tables with full transcript,
  reply, and system-prompt text in cells. Nothing truncates, rows have no fixed
  height, and nothing scrolls inside a bounded region, so the page scrolls instead.

### 1.3 Every page saves and reports status differently

| Pattern | Pages |
|---|---|
| One sticky **Apply** for the whole page | Config |
| One sticky **Save** plus a "Danger zone" card that is only a warning, with no action | Household, Personality, Pronunciation |
| A separate **Save** per card | Tools, Speech, System-1, Photos, Embeddings ("Switch") |
| Inline toolbar buttons with a free-floating `<span>` message | Household integrations, Music, Spotify |
| Browser `prompt()` / `alert()` | HelixDB "Edit name" |

Status is shown in at least five ways: `"active ✓"` text lines, `.badge good`
pills, `"(a key is set)"` hint text, `"Status: …"` HTML strings, and `.status ok/err`
bars. No status region is `aria-live`, so screen readers miss save results.

### 1.4 Local vs cloud is mostly hidden

Locality appears only as ad-hoc words inside `<option>` text: `ollama (local)`,
`anthropic (cloud)`, `ElevenLabs … (cloud)`, `jev (OpenRouter)`. Embeddings explains
it in a paragraph. Nothing tells the user:

- **which pipeline stages currently leave the house**, and **what data they send**
  (audio? transcripts? memory text? home address?);
- **where a stage falls back** when a key is missing. ElevenLabs with no key falls
  back to Wyoming, Visual Crossing to Open-Meteo, a failed embedder to SQLite FTS, and
  the energy VAD covers a missing Silero model;
- **that "LAN" is different from both.** Wyoming faster-whisper, Piper, Ollama, and
  laya-serve can run on this Mac (loopback) or on another machine on the network.
  `stt_addr`, `tts_addr`, `llm.ollama.url`, and `system1.base_url` decide which.

> ⚠️ **One cloud call is completely invisible.** GraphRAG **entity extraction**
> (`memory/graphrag_control.rs::build_extractor`) sends every ingested turn's text to
> Anthropic (`graphrag.extract_model`, default `claude-haiku-4-5`) **whenever
> `ANTHROPIC_API_KEY` is set**. That happens even when the LLM is Ollama and
> embeddings are local nomic. A user who picks "all local" in the UI today still
> sends memory text to the cloud. The redesign must show this, and ideally offer a
> toggle for it (see §8, open question 3).

### 1.5 Stale copy, wrong defaults, and small bugs (fix early)

| Issue | Location |
|---|---|
| The VAD card says Silero "needs a build with `--features vad-silero` … otherwise it falls back to energy", and lists Energy first. Silero has been the **default** since 2026-09-30 (`vad-silero` is a default feature). | `config.html` VAD card |
| The UI still says **"orchestrator"** in places (credentials desc, Tools sub, Notify sub/placeholder, Music sub). The product name is Anamanti Core. | `config.html`, `tools.html`, `notifications.html`, `music.html` |
| About hard-codes a "Drive support: yes — `ambient-get-drive-token`" row. The frame is `anamanti-get-drive-token`, and the row isn't build metadata anyway. | `about.html` |
| `fill()` falls back to `engine 'native'` and `search_provider 'duckduckgo'`, but the real defaults are `rig` and `tavily`. A missing field shows the wrong value, and **Apply** then writes it. | `config.html` `fill()` |
| The Spotify device placeholder still says "default **Ambient**". | `music.html` |
| The Pronunciation card headers have no icon, unlike every other card. Its "Danger zone" doesn't offer an action. | `pronunciations.html` |
| Every page redefines `$`, `show()`, and sometimes `esc()`. Household's `esc` escapes quotes but the shared one doesn't, so attribute escaping is inconsistent. | all pages |

### 1.6 Settings the console can't show

These exist in `anamanti.json` with **no UI, not even read-only**: speaker ID
(`speaker.*`), follow-up listening (`follow_up.*`), calendar subscriptions,
`system_prompt`, the Ollama URL, the STT whisper model/language/threads, music
ducking, `turn_timeout_secs`, `tool_cache`, and the Wyoming STT/TTS addresses. The
operator has to open the JSON file to find out how the running Core is configured.

---

## 2. Design goals and constraints

1. **One screen per page at 1920×1080.** The target is a full-HD browser window: a
   viewport of about **1920 × 950** after browser chrome, and about **1680 × 880**
   of content after the app bar and sidebar. *Settings* pages fit with no page
   scroll in their default state. *Data* pages (logs, memory) keep the page fixed and
   scroll **inside** a bounded table or detail pane.
2. **Group by user task**, not by source module.
3. **Make locality obvious everywhere.** Every option that can run locally or in the
   cloud carries a consistent chip, and the dashboard summarises it.
4. **One save model, one status model, one secret-field model**, used on every page.
5. **Stay within the locked stack:** static HTML bodies via `include_str!`, vendored
   Tailwind (`scripts/build-webui-css.sh`), no runtime CDN or Node, loopback guard
   intact, `postJSON` for every POST. Add small shared vanilla JS, not a framework.
6. **Degrade gracefully** at 1440 and 1280 widths (fewer columns) and keep the
   existing ≤760 px drawer behaviour. 1920 is the design target, not the only width.
7. **Keep endpoints stable** where possible. The redesign is mostly front end; the
   backend additions (§6.2) are small, read-only aggregators.

---

## 3. Local ⇄ Cloud design system

### 3.1 Locality tiers

| Tier | Meaning | Chip | Colour (light/dark) |
|---|---|---|---|
| **On this Mac** | In-process, or a loopback (`127.0.0.1`/`localhost`) service | 🏠-style "house" glyph · `Local` | teal/green |
| **Your network** | A private-range LAN host (RFC 1918 / `.local`) | "router" glyph · `LAN` | blue |
| **Cloud** | A third-party internet service. Show `Cloud · keyless` when no account is needed. | "cloud" glyph · `Cloud` | amber |
| **Off** | Feature disabled | muted outline · `Off` | neutral |

Rules:

- Compute the tier from the **actual configured address** (`stt_addr`, `tts_addr`,
  `llm.ollama.url`, `system1.base_url`), not from the engine name. Ollama on another
  Mac is `LAN`. Do the classification server-side (§6.2) so all pages agree.
- Next to every cloud chip, a one-line **"Sends:"** note says what leaves the house,
  e.g. *"Sends: your transcript, recalled memories and home city to Anthropic."*
- If a stage **falls back**, show both tiers: `Cloud → falling back to LAN (no key)`.
  The chip turns into a warning variant.
- Use chips in only three places: **choice cards**, the **dashboard pipeline**, and
  **integration tiles**. Don't scatter them through prose.

### 3.2 Choice cards replace provider dropdowns

Provider pickers with two to four options become **segmented choice cards**, one per
option, each with a name, locality chip, a one-line trade-off, requirements (key /
build feature / model on disk), and a disabled state with the reason ("whisper-rs
unavailable — needs the `stt-whisper-local` build feature"). This puts the local and
cloud options side by side, which is how the user thinks about the decision:

```
┌ Speech-to-text ───────────────────────────────────────────────────────────────────┐
│ ┌──────────────────────┐ ┌──────────────────────┐ ┌──────────────────────┐        │
│ │◉ Wyoming Whisper     │ │○ whisper-rs          │ │○ ElevenLabs Scribe   │        │
│ │  [LAN] 127.0.0.1:10300│ │  [Local] in-process  │ │  [Cloud] key ✓       │        │
│ │  Python faster-whisper│ │  Metal · base model  │ │  Sends: live audio   │        │
│ │  ● reachable          │ │  ⚠ not in this build │ │  Best accuracy       │        │
│ └──────────────────────┘ └──────────────────────┘ └──────────────────────┘        │
└───────────────────────────────────────────────────────────────────────────────────┘
```

Use them for the STT engine, VAD engine, LLM backend, System-1 backend, web search
provider, embedding backend, and weather provider.

### 3.3 Locality matrix (source of truth for the chips)

| Area | Option | Tier | Needs | Sends off-box | Fallback |
|---|---|---|---|---|---|
| VAD | Silero (default) | Local | model on disk | — | Energy |
| VAD | Energy | Local | — | — | — |
| STT | Wyoming faster-whisper | Local/LAN by `stt_addr` | server running | audio → that host | — |
| STT | whisper-rs | Local | build feature + model | — | — |
| STT | ElevenLabs Scribe v2 | Cloud | `ELEVENLABS_API_KEY` | live audio | Wyoming (no key) |
| Transcript fixes | Pronunciation rules | Local | — | — | — |
| Speaker ID | ECAPA-TDNN | Local | model path | — | off |
| System-1 | none | Off | — | — | — |
| System-1 | laya-serve | Local/LAN by `base_url` | sidecar | transcript | LLM |
| System-1 | jev (OpenRouter) | Cloud | `OPENROUTER_API_KEY` | transcript | LLM |
| LLM | Ollama | Local/LAN by `ollama.url` | server + model | prompt → that host | — |
| LLM | Anthropic (API key / subscription) | Cloud | key or OAuth token | transcript, memories, household context | — |
| LLM | OpenAI | Cloud | `OPENAI_API_KEY` | same as above | — |
| LLM | mock | Local | — | — | — |
| Web search | DuckDuckGo | Cloud · keyless | — | search query | — |
| Web search | Tavily | Cloud | `TAVILY_API_KEY` | search query | — |
| Memory store | SQLite / HelixDB | Local | — | — | FTS |
| Embeddings | nomic-embed-text-v1.5 | Local | ONNX on disk | — | SQLite FTS |
| Embeddings | OpenAI text-embedding-3-small | Cloud | `OPENAI_API_KEY` | memory text | SQLite FTS |
| **Entity extraction** | Anthropic `claude-haiku-4-5` | **Cloud (implicit!)** | runs whenever `ANTHROPIC_API_KEY` is set | **every turn's text** | none (vector-only) |
| TTS | Piper via Wyoming | Local/LAN by `tts_addr` | server running | reply text → that host | — |
| Personality | style catalog | Local | — | — | — |
| Weather | Visual Crossing | Cloud | key | home city | Open-Meteo |
| Weather | Open-Meteo | Cloud · keyless | — | home city | — |
| Places | Google Places (New) | Cloud | key | query + home address | tool hidden |
| Directions | Mapbox | Cloud | token | full home address | tool hidden |
| Calendar | .ics feeds | Cloud (per URL) | feed URLs | — (read-only fetch) | — |
| Shopping list | Cadora | Cloud | link code | item text | tool hidden |
| Phone messages | AppSaid | Cloud | app token + recipients | message text | tool hidden |
| Music | snapserver / librespot / mpv | Local | binaries | — | — |
| Music | Spotify control | Cloud | Premium + OAuth | playback commands | tool hidden |
| Photos | Google Drive | Cloud | OAuth client + consent | — (device pulls photos) | — |
| Notifications | Notify channel | LAN (to displays) | display connected | notification text | dropped |

Keep this table in the plan and mirror it in code as one JS map
(`locality.js`) plus the server-side classifier, so chips never disagree.

---

## 4. New information architecture

Sixteen links become **ten**, in four task groups, with a dashboard at `/`.

```
OVERVIEW
  ▸ Overview              /                 (NEW dashboard)

ASSISTANT
  ▸ Voice                 /voice            STT · VAD · TTS voice · Pronunciation · Speaker ID* · Follow-up*
  ▸ Intelligence          /intelligence     LLM model & engine · System-1 · Web search · Prompt*
  ▸ Personality           /personality      (kept; re-laid-out)

HOME
  ▸ Household             /household        Address & units · People
  ▸ Integrations          /integrations     Weather · Places · Directions · Calendar* · Shopping · Phone msgs · Spotify · Google Drive
  ▸ Music & Devices       /devices          Displays (notify) · test push · music processes · snapserver · URL player

MEMORY & ACTIVITY
  ▸ Memory                /memory           Recall settings (backend/embeddings/extraction) · Facts (SQLite) · Graph (Helix)
  ▸ Conversations         /conversations    Turns (chat log) + prompt for the selected turn + latency

SYSTEM
  ▸ Keys & Accounts       /keys             every secret / OAuth link in one table
  ▸ System                /system           About/build · effective boot config (read-only, redacted)
```

`*` = read-only at first (boot-config values shown in place, labelled "set in
`anamanti.json`"). Making them runtime-editable is a separate backend decision (§8).

### 4.1 Where every current control goes

| Today | Moves to |
|---|---|
| Config → Model & engine | Intelligence → **Model** |
| Config → Provider credentials | Intelligence → Model (inline, shown for the chosen backend) **and** Keys & Accounts |
| Config → Web search | Intelligence → **Web search** |
| Config → Voice (Piper) | Voice → **Text-to-speech** |
| Config → VAD | Voice → **End-of-speech (VAD)** |
| Speech (STT engine + ElevenLabs) | Voice → **Speech-to-text** (ElevenLabs settings in a drawer on its choice card) |
| Pronunciation | Voice → **Transcript fixes** (structured editor; raw JSON as a toggle) |
| System-1 | Intelligence → **Fast path (System-1)** |
| Personality | Personality (two-pane: catalog list + editor) |
| Household → Home, People | Household |
| Household → Cadora, AppSaid | Integrations tiles (AppSaid recipients link back to Household → People) |
| Tools → Mapbox, Weather, Places | Integrations tiles |
| Music → Spotify | Integrations tile (deep-linked from Music & Devices) |
| Music → processes, URL player, snapserver | Music & Devices |
| Photos (Drive) | Integrations tile → drawer with the folder picker |
| Notify | Music & Devices → **Displays** + "Send test" |
| Embeddings | Memory → **Recall settings** |
| SQLite | Memory → **Facts** tab |
| HelixDB | Memory → **Graph** tab |
| Chat log + Prompts | Conversations (master–detail) |
| About | System |

### 4.2 Backwards compatibility

- Keep every old GET route as a **302 to its new home + anchor**:
  `/stt → /voice#stt`, `/tools → /integrations`, `/drive → /integrations#drive`,
  `/chatlog → /conversations`, `/sqlite → /memory#facts`, and so on. Bookmarks and
  `agents.md` references keep working.
- **Keep every JSON/POST endpoint unchanged** (`/config`, `/stt/save`,
  `/tools/weather/save`, …). The new pages call the same APIs, so the backend risk is
  near zero.
- Update the nav assertions in `webconfig/tests.rs` (they check `href="/music"`,
  `/drive`, `/helix`, `/system1`, `/stt`). Add a test that every legacy route
  redirects.

---

## 5. Page designs at 1920×1080

Shared frame: a **56 px app bar** with the instance name, a connection pill
(`● 2 displays`), a locality summary pill (`4 local · 1 LAN · 2 cloud`), and a
theme toggle. A **232 px sidebar** that can collapse to a 64 px icon rail. A
**12-column content grid** with 24 px gutters and 24 px page padding, giving about
1640 px of usable width, filled edge to edge with no centred max-width on desktop. A
**page header** with the title, a one-line description, and right-aligned status
chips and actions.

### 5.1 Overview (NEW, `/`)

```
┌ Overview ────────────────────────────────── [● Core healthy] [Paul Family] [v1.4.2] ┐
│ ┌ Voice pipeline (live) ─────────────────────────────────────────────────── 12 col ┐ │
│ │  Mic → [VAD Silero·Local] → [STT Wyoming·LAN] → [Fixes·Local] → [System-1 Off] → │ │
│ │       [LLM Claude Sonnet·Cloud] → [Memory Helix·Local | Extract·Cloud ⚠] →        │ │
│ │       [TTS Piper·LAN] → Display                                                  │ │
│ │  Each node: status dot (reachable/fallback/error) · click → its settings section │ │
│ └──────────────────────────────────────────────────────────────────────────────────┘ │
│ ┌ Displays ─── 4 col ┐ ┌ Recent latency ─────── 5 col ┐ ┌ What leaves the house  3c ┐ │
│ │ ● Kitchen  ab:12…  │ │ p50 1.8 s · p95 3.4 s        │ │ Anthropic: transcript,    │ │
│ │ ● Den      cd:34…  │ │ stacked bar per stage (last  │ │  memories, city           │ │
│ │ [Send test push]   │ │ 20 turns) STT/S1/recall/LLM/ │ │ Visual Crossing: city     │ │
│ │                    │ │ TTS                          │ │ [Review →]                │ │
│ └────────────────────┘ └──────────────────────────────┘ └───────────────────────────┘ │
│ ┌ Integrations ── 6 col ─────────────────────┐ ┌ Last turns ───────────── 6 col ──┐ │
│ │ tile grid: Weather ✓ Places ✓ Directions ✕ │ │ 5 newest turns: time · speaker · │ │
│ │ Calendar ✓ Shopping ✓ Phone ✓ Spotify ⚠   │ │ "what's the weather" → reply…    │ │
│ │ Drive ✓  (✓ active, ⚠ needs attention)      │ │ [Open conversations →]           │ │
│ └─────────────────────────────────────────────┘ └──────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────────────────────────┘
```

Data comes from a new `/overview.json` aggregator (§6.2). Use a small inline SVG or
CSS bar chart for latency; follow the `dataviz` skill guidance (stage colours stay
consistent with the chat-log breakdown).

### 5.2 Voice (`/voice`)

```
┌ Voice ─── "How the assistant hears and speaks" ───────────── [Save changes] (dirty) ┐
│ ┌ Speech-to-text ──────────────────────── 8 col ┐ ┌ End-of-speech (VAD) ── 4 col ──┐ │
│ │ [choice cards: Wyoming | whisper-rs | 11Labs] │ │ [Silero·Local ◉] [Energy·Local]│ │
│ │ selected card → inline detail row:            │ │ Speech threshold ──●──── 0.50  │ │
│ │   ElevenLabs: key ●set [Replace] model, URL   │ │ (slider + number)              │ │
│ └───────────────────────────────────────────────┘ └────────────────────────────────┘ │
│ ┌ Transcript fixes ────────────────────── 8 col ┐ ┌ Text-to-speech ─────── 4 col ──┐ │
│ │ table: Canonical | Heard as (chips) | ✕       │ │ Piper · [LAN] 127.0.0.1:10200  │ │
│ │ Zehrs | zairs  zehr's  zares  + | ✕           │ │ Voice [en_US-amy-medium ▾]     │ │
│ │ [+ Add rule]          [Edit as JSON]          │ │ [▶ Preview]  (future)          │ │
│ │ (scrolls inside if > 8 rules)                 │ ├ Listening ──── read-only ──────┤ │
│ └───────────────────────────────────────────────┘ │ Follow-up on · 10 s / 5 s      │ │
│                                                   │ Speaker ID: off (anamanti.json)│ │
│                                                   └────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────────────────────────┘
```

The one **Save changes** button sends only the dirty sections to their existing
endpoints (`/stt/save`, `/config` for the voice and VAD fields,
`/pronunciations/save`).

### 5.3 Intelligence (`/intelligence`)

```
┌ Intelligence ─────────────────────────────────────────────── [Save changes] ────────┐
│ ┌ Language model ─────────────────────────────────────────────────────── 8 col ───┐ │
│ │ [Ollama·Local/LAN] [Anthropic·Cloud ◉] [OpenAI·Cloud] [Mock·Local]              │ │
│ │ Model [claude-sonnet-… ▾]  Engine (rig ◉ / native ⚠ no tools)  Auth [API key ▾]  │ │
│ │ Key ● set  [Replace] [Clear]     Sends: transcript, memories, household context  │ │
│ └──────────────────────────────────────────────────────────────────────────────────┘ │
│ ┌ Fast path (System-1) ────────── 4 col ┐ ┌ Web search ──── 4 col ┐ ┌ Prompt ─ 4c ┐ │
│ │ [Off ◉] [laya·Local] [jev·Cloud]      │ │ [on/off switch]       │ │ read-only   │ │
│ │ Min confidence ──●── 0.85             │ │ [DDG·keyless][Tavily] │ │ system      │ │
│ │ (URL/model/key shown per backend)     │ │ Key ● set [Replace]   │ │ prompt      │ │
│ └───────────────────────────────────────┘ └───────────────────────┘ │ preview +   │ │
│                                                                     │ "last sent →"│ │
│                                                                     └─────────────┘ │
└──────────────────────────────────────────────────────────────────────────────────────┘
```

The `native` engine shows its `llm_capabilities` warning as an inline caution
chip, not buried text.

### 5.4 Integrations (`/integrations`)

A **4×2 tile grid** that fits on one screen. Each tile shows: logo glyph, name, the
voice tool it enables (`weather_lookup`), the locality chip, a status pill (`Active` /
`Needs key` / `Not linked` / `Disabled in anamanti.json`), and a **Configure**
button. Configure opens a **right-hand drawer** (480 px) holding that tile's
existing form: key field, provider choice, OAuth "Link" button, Drive folder picker,
Cadora code entry, AppSaid advanced fields. The grid stays visible behind it, so the
page never scrolls.

```
┌ Weather ─────────┐ ┌ Places ──────────┐ ┌ Directions ──────┐ ┌ Calendar ────────┐
│ ☀ weather_lookup │ │ 📍 places_lookup │ │ ➤ directions_    │ │ 📅 calendar_     │
│ [Cloud] VisualX  │ │ [Cloud] Google   │ │ [Cloud] Mapbox   │ │ [Cloud] 2 feeds  │
│ ● Active         │ │ ● Active         │ │ ○ Needs token    │ │ ● Active (r/o)   │
│ [Configure]      │ │ [Configure]      │ │ [Configure]      │ │ [View]           │
└──────────────────┘ └──────────────────┘ └──────────────────┘ └──────────────────┘
┌ Shopping (Cadora)┐ ┌ Phone (AppSaid)  ┐ ┌ Spotify          ┐ ┌ Google Drive     ┐
│ ...              │ │ 3 recipients     │ │ ⚠ creds, unlinked│ │ 2 folders        │
└──────────────────┘ └──────────────────┘ └──────────────────┘ └──────────────────┘
```

A filter row (`All · Active · Needs attention · Cloud · Keyless`) sits above the grid.

### 5.5 Household (`/household`)

- **Left, 4 columns: Home.** A compact address form on a 2-column field grid
  (Address 1 full width; Address 2 | City; State | Postal; Country | Units), plus a
  "What uses this" note: *Directions & Places → full address · Weather & "here" →
  City + State only*.
- **Right, 8 columns: People.** An editable **table**, one row per person: Name ·
  Relationship · Emails (chip input) · Phones (chip input) · AppSaid (✓ linked /
  paste) · ✕. Removing a row asks for confirmation inline ("Remove Sam? Undo"), which
  replaces the "Danger zone" card. Ten or more people scroll inside the table.

### 5.6 Personality (`/personality`)

Two panes. On the left, a master switch, then the catalog as a selectable list with
an "active" radio on each row. On the right, an editor for the selected personality
(key, display name, style instruction in a larger textarea), plus a hint card:
"Say: *talk like a gangster* / *go back to normal*". Delete is confirmed inline.

### 5.7 Music & Devices (`/devices`)

- **Displays, 5 columns:** a connected-display list (name, id, connected-since if
  available) and the "Send test notification" form underneath.
- **Music processes, 7 columns:** process table (status pill, PID, start/stop, log
  path truncated with copy), Start all / Stop all, snapserver groups as compact
  cards, URL player row.
- When `music.enabled=false`, show the disabled card *only* in the music half.

### 5.8 Memory (`/memory`)

There are three tabs. Below the page header there's a stats strip: facts count,
graph nodes by label, active embedder, ingest offset or lag.

- **Recall settings:** choice cards for the store (Helix / SQLite FTS, read-only:
  boot config) and embeddings (nomic·Local / OpenAI·Cloud, with "OpenAI key ● set
  [Replace]" **inline**, which fixes the missing-key gap in §1.1). An **Entity
  extraction** card shows `Cloud · Anthropic claude-haiku-4-5 · active because
  ANTHROPIC_API_KEY is set`, with the privacy note.
- **Facts:** the SQLite table with a search box, kind/speaker filters, fixed-height
  rows (content clamped to two lines, full text in a side detail pane), and a
  sticky header with internal scroll.
- **Graph:** a label selector (Entity / Turn / Memory…) plus a table. Rename happens
  inline in the cell (Enter to save, with a toast showing affected counts), replacing
  `prompt()`/`alert()`.

### 5.9 Conversations (`/conversations`)

**Master–detail, 5 + 7 columns, full height:**

- On the left, a virtual-scrolling list of turns: time, speaker, model badge, the
  first line of the transcript, total latency pill (coloured by threshold), and a
  locality dot for whether the turn used cloud. There's a search box, a speaker
  filter, and a limit selector.
- On the right, the selected turn: transcript, reply, memories written, a
  **latency waterfall** (the existing `timingRows` drawn as horizontal bars), and a
  collapsible **Prompt** section (system prompt + user message from the prompt log,
  matched by timestamp; if there's no exact match, show the nearest prompt with
  "approximate match").

### 5.10 Keys & Accounts (`/keys`)

One table holds every secret and OAuth link. Columns: **Service · Tier chip · Status
(Set / Not set / Linked / Expired) · Source (env seed vs. set here) · Used by
(tools/stages) · Actions (Replace · Clear · Link · Test)**. Rows: Anthropic API key,
Anthropic subscription token, OpenAI, Tavily, ElevenLabs, OpenRouter, Mapbox, Visual
Crossing, Google Places, AppSaid, Spotify (OAuth), Google Drive (OAuth), Cadora
(link). The forms reuse the same endpoints as the in-context fields. Add a banner:
"Secrets are stored 0600 in `anamanti_settings.json` and never shown back."

### 5.11 System (`/system`)

- **Build:** version, branch, commit, build time (drop the stale Drive row).
- **Runtime:** instance id, service name, bind/config addresses, data paths, uptime.
- **Effective configuration:** a read-only, **redacted** tree of the boot config
  merged with runtime overrides. Each value is tagged `default` / `anamanti.json` /
  `set in console`, so the operator can stop opening the JSON file. Add a "Copy as
  JSON" button.

---

## 6. Implementation approach

### 6.1 Front end

| Piece | Plan |
|---|---|
| **Tokens** (`tailwind/input.css`) | Refresh the palette away from stock M3 purple to an Anamanti identity: warm neutral surfaces, one **accent** (e.g. a deep teal or hearth amber, decided in §8), and dedicated tokens `--loc-local`, `--loc-lan`, `--loc-cloud`, `--loc-off` (bg/fg/border pairs for light and dark). Base font 14 px with tabular numerals for metrics. Spacing on a 4 px scale. Keep the legacy variable names that page bodies reference inline until those pages are rewritten. |
| **Layout** | Replace `.content-inner` max-widths and masonry `.card-grid` with an explicit `.grid-12` and `col-span-*` helpers. Add `.page-fill` (height `calc(100vh - appbar)`, children use `min-h-0` + internal `overflow:auto`) for data pages. Collapsible sidebar rail, persisted in `localStorage`. |
| **Shared JS** (`shell.js` → split into `shell.js` + `ui.js`, both served like `/shell.js`) | `ui.toast(ok,msg)` (aria-live), `ui.dirty(form, onChange)` + save-bar orchestration (POSTs each dirty section to its endpoint, then reports per section), `ui.secretField(el,{set,onReplace,onClear})`, `ui.choiceCards(el, options, value)`, `ui.locChip(tier, detail)`, `ui.drawer(open/close)`, `ui.confirmInline(btn, msg)`, `ui.tabs(el)` with hash routing (`/memory#graph`), a single `$`/`esc`/`escAttr`. Delete the per-page copies. |
| **Locality map** (`locality.js` or a section of `ui.js`) | A static map of option → tier/needs/sends (§3.3). The live tier for address-based options comes from the server. |
| **Pages** | New bodies: `overview.html`, `voice.html`, `intelligence.html`, `integrations.html`, `devices.html`, `memory.html`, `conversations.html`, `keys.html`, `system.html`; rework `household.html` and `personality.html`. Remove the superseded bodies once their redirects land. |
| **CSS build** | Re-run `scripts/build-webui-css.sh` and commit `panel.css` with every page change (agents.md rule). Add the new `*.js` to the Tailwind `@source` list. |

### 6.2 Back end (small, read-only additions)

| Endpoint | Purpose |
|---|---|
| `GET /overview.json` | One call for the dashboard: active engine per stage + tier, reachability probes (STT/TTS Wyoming `describe` with a short timeout, Ollama `/api/tags`, laya `/health` if present), connected displays, integration statuses (reusing the existing `*/status.json` builders), the last N turns' timing summary, and the "sends off-box" list. |
| `GET /keys.json` | Status-only list of every secret/OAuth link (set/not set, source, used-by). **Never returns values.** Writes go through the existing per-feature endpoints. |
| `GET /system.json` | About + runtime + **redacted** effective config with a provenance tag per key. Extends `about_json()`. |
| Locality classifier | A Rust helper `locality::classify(addr) -> Local/Lan/Cloud` (loopback / RFC 1918 / link-local / `.local` → LAN). Its result goes into each relevant status JSON as `tier`. Unit-test it in `tests.rs`. |
| Legacy redirects | `get(old) → Reply::redirect(new)` rows in `routes.rs`. The loopback Host/Origin guard already runs before routing, so redirects inherit it. |
| Page routes | New `Reply::page(...)` rows; the nav moves into `shell_head.html`. |

There are no new POST endpoints in the first pass. Every write reuses an existing
handler, so the security surface (loopback guard, JSON content-type, CSRF/Origin
check) doesn't change.

### 6.3 Docs to update with the change (per `agents.md` working agreements)

`agents.md` and `README.md` refer to tabs by their old names ("Config tab", "Tools
tab", "Household tab → /household", "Speech tab (`/stt`)", "System-1 tab →
/system1", "Memory → Embeddings", "Photos tab", "Music tab", "Notify tab"). Update
them to the new pages/anchors in the same PR that moves each control. Record the IA
and locality decisions in the `Plan.MD` decision table, and note the console
structure in `architecture.md`.

---

## 7. Phased delivery

Each phase can ship on its own and keeps the console working.

| Phase | Deliverable | Notes |
|---|---|---|
| **0 — Quick fixes** (½ day) | All of §1.5: VAD copy and option order, "orchestrator" → "Anamanti Core", About row, `fill()` defaults, Spotify placeholder, Pronunciation icons. | No structural change; safe to land first. |
| **1 — Design system + shell** | New tokens and locality tokens, 12-column grid, `.page-fill`, collapsible rail, app-bar status pills, `ui.js` (toast, dirty/save bar, secret field, choice cards, chips, drawer, tabs, inline confirm), aria-live everywhere. Existing pages restyled but not yet regrouped. | Visual QA at 1920/1440/1280 + dark mode. |
| **2 — Backend aggregators** | Locality classifier, `/overview.json`, `/keys.json`, `/system.json`, tier fields in existing status JSON. Tests. | Read-only; no new writes. |
| **3 — IA restructure** | New nav; build Voice, Intelligence, Integrations (tiles + drawers), Music & Devices; legacy redirects; update nav tests. | Biggest user-visible step. Move controls page by page; delete old bodies only after the redirects work. |
| **4 — Overview + Keys + System** | Dashboard with pipeline strip, latency chart, "what leaves the house"; Keys & Accounts table; System page with the redacted effective config. | |
| **5 — Data pages** | Memory (tabs, filters, inline rename), Conversations (master–detail, latency waterfall, matched prompt). | Internal scroll; consider pagination params on `/chatlog.json` if 1000 records get heavy. |
| **6 — Polish & QA** | Keyboard order and focus rings, empty/error/loading states for every card, copy pass, `/browse` screenshot QA at 1920×1080 for every page (assert no page-level vertical scroll on settings pages), `/design-review` pass. | |

---

## 8. Open questions (need a decision)

1. **Brand accent:** keep M3 purple, or move to an Anamanti identity (deep teal /
   hearth amber)? This affects every token.
2. **Is "LAN" its own tier?** Recommended yes: whether Whisper, Piper, and Ollama
   run on the same Mac or elsewhere on the network matters for latency and
   troubleshooting, even though neither is "cloud".
3. **Entity extraction:** should the UI only *disclose* the implicit Anthropic
   extraction, or should we add a runtime toggle (`graphrag.extract`:
   `auto | off`, plus a future local extractor via Ollama)? A toggle needs a small
   backend change and a `Plan.MD` decision row.
4. **Boot-only settings** (speaker ID, follow-up, calendar feeds, system prompt,
   Ollama URL, STT model/language): read-only in v1 as proposed, or make some
   runtime-editable? Each one needs settings-overlay and persistence work in
   `settings.rs`.
5. **Prompts ↔ turns join:** is timestamp matching good enough, or should the chat
   log record a `turn_id` that the prompt log shares (cleaner, but a small schema
   change to both JSONL files)?
6. **Personality page:** keep it standalone (recommended; the catalog editor needs
   room), or fold it into Intelligence?
7. **"Sends:" copy** for each cloud option: confirm exactly which context goes to
   each provider (e.g. does the LLM prompt always include the home address, or only
   city/state?) before we print it in the UI.

---

## 9. Acceptance criteria

- At a **1920×1080** browser window (about 1920×950 viewport), every settings page
  (Overview, Voice, Intelligence, Personality, Household with ≤6 people, Integrations,
  Music & Devices, Keys, System) renders with **no page-level vertical scroll**. Data
  pages (Memory, Conversations) scroll only inside their table or detail panes.
- Every option that can run in more than one place shows a **locality chip** computed
  from the configured address. Every cloud option shows a **"Sends:"** line. Active
  fallbacks show as warnings.
- The Overview pipeline shows every stage's tier and health, **including entity
  extraction**.
- There's one save model (dirty-tracked save bar for forms, immediate buttons for
  actions), one secret-field component, one status/toast component (aria-live), and
  no `alert()`/`prompt()`.
- All legacy routes redirect. All existing JSON/POST endpoints are unchanged.
  `cargo test` (including updated webconfig nav/redirect tests) and `cargo clippy -D
  warnings` pass. `panel.css` is regenerated and committed.
- Light and dark themes both pass a contrast check (WCAG AA) for text and chips.
- `agents.md`, `README.md`, `architecture.md`, and `Plan.MD` reference the new page
  names.
