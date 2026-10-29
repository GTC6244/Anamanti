# Bilingual Plan: English + Brazilian Portuguese (pt-BR)

**Target:** M4 Mac Mini (Anamanti Core). **The Anamanti Display is unchanged**: no
Display code or APK release is needed.
**Feature:** a **bilingual household**. Anyone can speak English or Brazilian Portuguese
on any turn, and the assistant **answers in the language that turn was spoken in**,
using a voice for that language. That covers the deterministic fast paths (memory
commands, Home Assistant, timers, time/date, weather, personality switch) and memory.

> **Status: implemented and validated end to end (2026-10-20) — all phases** (1a, 1b, 2,
> 3, 4) on this branch; **not yet deployed to production** (see §9 for the rollout
> steps). Live results, deviations from the plan and the one open owner question are in
> **§8**. Decision row in [`Plan.MD`](./Plan.MD) §0; design summary in
> [`architecture.md`](./architecture.md) §8.6. `DisplayUI.md` needs no change (the Display
> is untouched).

---

## 0. Confirmed decisions (owner, 2026-10-19)

| Question | Decision |
| --- | --- |
| **Reply language** | **The reply follows the language spoken on that turn.** It is not the speaker's preferred language and not the household default. Speaker preference and stickiness only break a tie when the spoken language really can't be determined (§2.2). |
| **STT engine** | **Keep whatever STT engine is currently selected. Bilingual mode must not require switching engines.** Production today is **ElevenLabs Scribe v2 Realtime** (`stt_engine="elevenlabs"`, persisted runtime setting). The engine stays runtime-swappable, so bilingual mode has to work on all three engines (§2.3), and ElevenLabs is the primary implementation and test target. |
| **Embedding model** | **Keep the memory embedder currently in use.** Production is the local **nomic-embed-text-v1.5** (`helix_path/active_backend` = `local`, store `nomic-768`). No new embedder, and no re-embedding. |
| **Both languages must keep working** | **Hard requirement.** Turning on bilingual mode must not break or degrade English in any way, and Portuguese must work across the same features. Every phase is gated on the **English non-regression** and **parity** checks in §3. |
| **Display** (2026-10-20) | **Leave the Display as is.** No Flutter localization, no Language setting, no clock/date format change; the UI chrome stays English. The only Portuguese on screen is the **streamed turn text** (live transcript + reply), which already flows through unchanged. |
| **Weather** (2026-10-20) | **Weather display content and the ambient push stay English.** The ambient icon/temperature, the full-screen forecast, and the `anamanti-weather` frame data (descriptions, day labels) are unchanged. *Interpretation:* the **spoken** answer to a weather question still follows the reply-language rule, so a pt question gets a pt spoken sentence while the on-screen forecast stays English. Other unprompted pushes (notifications, now-playing) are likewise untouched. |
| **Memory facts** (2026-10-20) | **Stored as spoken**, never translated (§2.8). |
| **System-1** (2026-10-20) | **System-1 will handle Portuguese.** pt turns skip System-1 only temporarily (Phase 1). Phase 2 is **required**: calibrate pt on the active backend and enable it, with pt replies. |

## 1. Why this needs code, not just config

What we have today is **single-language, pinned to English**. Production sets no
`stt.language`, so it defaults to `"en"` (`config.rs:384`), and ElevenLabs is opened
with `language_code=en` (`stt/elevenlabs.rs:183`). Portuguese speech is therefore forced
through English transcription right now. Beyond STT:

| Area | Today | Bilingual problem |
| --- | --- | --- |
| Detected language | `SttEvent::Transcript(String)` (`stt/mod.rs:52`) carries text only. | The pipeline never learns which language was spoken. |
| TTS | One `runtime.tts_voice` (`speech_out.rs:351`); production uses Piper's default voice. | Piper voices are monolingual, so a pt reply read by an English voice sounds wrong. |
| LLM | `DEFAULT_SYSTEM_PROMPT` names no reply language. | The reply language has to be fixed *before* TTS picks a voice. |
| Fast paths | English string matchers: `memory/extract.rs` (`parse_command`, `infer_memories`), `home_assistant/matcher.rs`, `prompt.rs::detect_personality_switch`, `system1::parse_duration_secs`. | pt utterances miss them. |
| Canned replies | English literals: `system1_handlers.rs` ("Timer set for…", "Okay, I've cancelled your timer.", `%-I:%M %p`, "Today is…"), `home_assistant/mod.rs` ("Okay, …", `UNREACHABLE`), `weather/render.rs`, `weather::weather_description`, memory-command and personality confirmations. | **This one is a real bug once pt is transcribed:** production System-1 is `jev` (an OpenRouter LLM, `min_confidence` 0.85). It may resolve "que horas são" as `Time` and speak an **English** canned reply. |
| Memory | nomic-embed-text-v1.5 is English-centric (kept by decision). | Recall over pt turns, and across languages, is weaker. See §2.8 for how we measure and mitigate it. |
| Display | ~93 hard-coded English `Text('…')` strings. | **None, by decision (§0).** The chrome stays English; streamed pt transcript/reply text already renders. |

These parts are **language-agnostic and need no change:** the wake word (`hey_jarvis` is
a phrase, so it works whatever language follows), Silero VAD, ECAPA speaker ID, AEC, the
Wyoming wire format, follow-up `?` detection (Portuguese uses `?` too), and music.

## 2. Design: a per-turn language

Each turn resolves a **`TurnLang`** once, right after STT. Everything downstream reads
that one value: the prompt's language block, the TTS voice, canned replies, and provider
`languageCode`s.

### 2.1 Configuration (`anamanti.json`)

```jsonc
"language": {
  "enabled": ["en", "pt-BR"],   // ordered; 1 entry ⇒ single-language (today's behaviour)
  "default": "en"               // last-resort tie-break only (ambient pushes are always English, §0)
},
"tts_voices": {                 // per-language Piper voice; the legacy `tts_voice` seeds "en"
  "en":    null,                // null = Piper's default voice (what production uses today)
  "pt-BR": "pt_BR-faber-medium"
}
```

- **If the `language` block is absent, behaviour is byte-for-byte today's**
  (`stt.language`, `tts_voice`). That is also the rollback: remove the block and restart.
- With more than one language enabled, a pinned `stt.language` is ignored with a boot
  warning. The STT engine receives the *enabled set* instead (§2.3).
- `language.*` and `tts_voices` seed the runtime settings, can be edited on the config
  page (Voice → Languages), and persist to `anamanti_settings.json`, like `tts_voice`
  today.
- The `pt_BR` Piper voice must be installed on the Piper server. The Voice page already
  lists Piper's catalog and can filter it by language.
- `HouseholdMember` (`settings.rs:204`) gains an optional `language`, used **only** as a
  tie-break (§2.2).

### 2.2 New module `anamanti-core/src/lang/`

- `Lang` enum (`En`, `PtBr`) mapping to BCP-47 (`en`, `pt-BR`), ISO-639-1 (`en`, `pt`),
  ISO-639-3 (`eng`, `por`, which ElevenLabs may return), and the Piper locale
  (`en_US`, `pt_BR`).
- `resolve_turn_lang(hint, transcript, prev, speaker_pref, cfg) -> TurnLang`. Signals 1–2
  measure **what was actually spoken**; 3–5 only break ties when 1–2 give no answer:
  1. **The STT engine's detected language**, if it is in the enabled set. Lookalike
     codes (`es`, `gl`, `spa`, `glg`) map to `pt-BR` when pt is enabled.
  2. **Text language ID** on the transcript, restricted to the enabled set. Use the
     pure-Rust **`whatlang`** crate (small, no model files). Only trusted at ≥ 3 words and
     at its confidence bar.
  3. The previous turn's language in this follow-up chain.
  4. The identified speaker's preferred language.
  5. `language.default`.
- If signals 1 and 2 **disagree** and both are confident, signal 1 wins: the audio-based
  detection beats text ID.
- `TurnLang.source` (`stt|text|sticky|speaker|default`) is logged, recorded in the turn
  timing / prompt log, and shown on the Conversations page.
- `Phrases` tables, one per `Lang`: static strings plus small formatters
  (`timer_set(secs)`, `time_now(t)`, `date_today(t)`, `human_duration(secs)`,
  `weather(report)`, …). **Every** canned reply moves here. The `En` table holds today's
  exact strings, so English output is unchanged. From now on: no spoken English literal
  outside `lang/`.

### 2.3 STT: same engine, made bilingual

`SttEvent::Transcript(String)` becomes `SttEvent::Transcript(Transcript { text, lang:
Option<String> })`. Each engine fills `lang` if it can. If it can't, text ID (§2.2 step 2)
decides, so every engine works.

**ElevenLabs Scribe v2 Realtime (production — primary target).** The realtime endpoint
accepts `language_code`, **`secondary_languages`** (array), and
**`include_language_detection`**. The `committed_transcript_with_timestamps` message
carries an optional **`language_code`** ("detected or specified language code"); the
plain `committed_transcript` carries only `text`. Work in `stt/elevenlabs.rs`:

- **Candidate A (preferred, verify first):** `language_code=<default>` +
  `secondary_languages=<other enabled>`. This *may* restrict recognition to our two
  languages, which suits short pt commands best and keeps English like today. The
  semantics are undocumented, so test it on the clip sets in §4.
- **Candidate B:** omit `language_code` (auto-detect) and set
  `include_language_detection=true` (+ `include_timestamps=true` if that is what yields
  `committed_transcript_with_timestamps`).
- Either way, the reader (today it waits only for `committed_transcript`,
  `elevenlabs.rs:271`) must also accept `committed_transcript_with_timestamps`, take
  `language_code` from it, and **not** treat a pair of messages for one commit as two
  transcripts.
- Pick A or B by measurement: English accuracy must match today's `language_code=en`
  (non-regression gate), pt accuracy must be acceptable, and short-command language
  detection must be correct.

**wyoming (faster-whisper) and whisper-rs** stay selectable, so they get bilingual
support too (Phase 1b, after ElevenLabs):

- Both require a **multilingual model**. An `.en` model cannot transcribe Portuguese.
  whisper-rs only resolves `ggml-<model>.en.bin` (`config.rs:373`) and
  `fetch-whisper-models.sh` only fetches `.en` models, so allow multilingual model files
  and fetch them. When the active engine is configured in a way that can't do bilingual,
  the config page warns (Voice → Languages).
- **whisper-rs:** in `decode()` (`whisper_local.rs:175`), mel → `lang_detect` → argmax
  **over the enabled set** → `full()` pinned to it. *Verify the API in our pinned
  whisper-rs.* Measure the extra encoder pass with the existing RTF log.
- **wyoming:** run the server with no `--language`, and read `data.language` from the
  `transcript` event if our faster-whisper version sends it (*verify*). Otherwise text ID
  decides.
- An `.en` model is slightly better at English than a multilingual one. The English gate
  in §3 applies here too.

`stt/normalize.rs` pronunciation rules gain an optional `lang` field, so an English
transcript fix can't rewrite Portuguese text. Rules with no `lang` behave as today.

### 2.4 Prompt + LLM

`prompt::build_system_prompt` appends this block **only in bilingual mode**:

```
# Language
The person spoke Brazilian Portuguese. Reply in Brazilian Portuguese (pt-BR) — Brazilian
vocabulary and spelling, not European Portuguese — unless they explicitly ask for another
language.
```

(For English turns, the block names English.) It sets an explicit target rather than
"mirror the user" because the TTS voice is chosen from `TurnLang` *before* the reply
streams. The base system prompt and personality style blocks stay English; the model
applies them in the reply language.

### 2.5 TTS voice per turn

`speech_out` picks `tts_voices[turn_lang]` (falling back to `tts_voice`). Safety net: run
`whatlang` on the **first complete reply sentence**. If it is confidently a *different*
enabled language (e.g. the user said "answer in English"), switch the voice for the rest
of the reply. Per-sentence switching within one reply is out of scope for v1.
**Pre-warm both voices at boot** (one tiny discarded `synthesize` each) so the first turn
in either language isn't slow.

### 2.6 Fast paths (still "pure string code, no I/O")

Each matcher gets a pt-BR phrase table and **tries all enabled languages** on every turn
instead of trusting `TurnLang`. A short command still works even if detection got the
language wrong. The **reply uses the language whose table matched**, which is by
definition the language spoken. The English tables are today's code, unchanged.

- **Memory commands** (`parse_command`): `lembra que` / `lembre que` / `lembre-se de` /
  `não esquece de`, `esquece isso` / `esqueça isso` / `esquece o último`, `esquece (de|o|a)
  …`. Plus **`infer_memories`** pt patterns ("meu/minha … favorito/a é …").
- **Home Assistant** (`matcher.rs`): `liga`/`ligar`/`acende`/`desliga`/`apaga`,
  `diminui`/`aumenta … para N por cento`, `a luz da varanda está acesa/ligada?`, scenes
  `ativa`/`roda …`. Handle `do/da/dos/das` + article targets ("as luzes do escritório"),
  fillers (`por favor`, `você pode`, `pode`), and pt number words. **Names:** HA names are
  likely English, so read HA **entity/area aliases** into the catalog and let pt aliases
  ("escritório") match. *Verify what `/api/template` returns for aliases.*
- **Personality switch:** `fala como`, `fale como`, `age como`, `finge que é`, `muda
  para`; off intent `volta ao normal`, `para com isso`. Plus pt aliases for catalog
  entries ("gângster", "sarcástico").
- **Durations** (`parse_duration_secs`, timer adjust): `dez minutos`, `um minuto e meio`,
  `meia hora`, `uma hora e quinze`, with gendered `um/uma`, `dois/duas`; adjust phrases
  `mais cinco minutos`, `tira um minuto`.

### 2.7 System-1

- **English turns: unchanged.** Same engine (`jev` in production), same intents,
  thresholds, and replies.
- **pt turns: System-1 is skipped in Phase 1 only** and they go straight to System-2.
  That closes the English-canned-reply bug (§1) before any localization exists, and the
  LLM answers in pt.
- **Phase 2 makes System-1 handle pt (required, per §0):** add pt cases for every intent
  (plus near-misses) to `tests/system1_calibration.rs`, run them on the active backend
  (`jev` in production), then enable System-1 for pt turns **per intent** as each passes
  at the same `min_confidence` English uses. An intent that doesn't pass yet stays on
  System-2 for pt and is tracked until it does. Replies come from `Phrases`. Time
  and date follow pt-BR conventions: "São 15h45." / "Hoje é segunda-feira, 19 de
  outubro." (hand table, no chrono `unstable-locales`). The intent descriptions stay
  English; they are classifier instructions, not spoken output.

### 2.8 Memory (current embedder kept)

- **No embedder change.** Recall stays on local nomic-embed-text-v1.5 (`nomic-768`). The
  store, ingester, and SQLite facts are untouched, with no re-embedding and no migration.
- **Facts are stored as spoken** (no translation; owner decision, §0). The entity-extraction prompt gets one
  line: "keep names and facts in the language they were stated in."
- pt `infer_memories` patterns (§2.6) so inferred capture works in both languages.
- **Known limitation to measure, not fix by default:** nomic is English-centric. pt→pt
  recall should be usable (shared names, numbers, places), but en↔pt cross-language
  recall will be weak. The Phase 3 eval (§4) measures this on a mixed chat log. If pt
  recall is unacceptable, raise it as a **separate owner decision**. (The per-signature
  stores would make any later switch lossless, but it is not part of this plan.)
- **English recall must not change.** Nothing in its path changes, so that holds
  automatically; the gate still checks it.

### 2.9 Places / weather / other providers

- `places_lookup`: `languageCode = turn_lang`.
- **Weather (§0): display stays English, spoken reply follows the turn.**
  - Unchanged: the ambient push, the `anamanti-weather` frame data, the forecast screen,
    `weather::weather_description`'s English table (which feeds the frame), and
    `when_label`.
  - New, spoken path only: on a pt turn, `render.rs` uses a pt template + a pt WMO
    description table in `Phrases` ("Agora está parcialmente nublado e 23 °C. A máxima
    prevista é 24 °C, com mínima de 10 °C. A previsão está na tela.").
  - Keep the two separate: the frame never reads `Phrases`, so an English screen can't
    pick up pt text.
- Directions notes, calendar, and recipes come from the LLM or providers and follow the
  reply language. A recipe/place card's *content* may therefore appear in pt; its chrome
  stays English.

### 2.10 Display (Anamanti Display): no change

- By decision (§0), **no Display code changes and no APK release.** The UI chrome, clock,
  and settings stay English.
- The streamed transcript and reply text appear in whichever language was spoken. That
  already works: the text travels as UTF-8 in the existing `transcript`/reply frames.
- **No Wyoming wire change**, so the Core ships alone; there is no lockstep redeploy.
- One on-device check (§4): pt diacritics (`ã õ ç é ê á í ó ú à`) render correctly in
  the conversation view and in recipe/place cards with the bundled fonts.
- Speaker-enrollment prompts on screen stay English; the Core's spoken enrollment lines
  come from `Phrases` in the enrolling turn's language.

### 2.11 Admin console

- **Voice → Languages:** enabled languages, default, a per-language Piper voice, and a
  warning when the active STT engine/model can't do bilingual.
- **Household:** per-person language (tie-break only).
- **Conversations:** a turn-language chip + `source`.

## 3. "Both languages keep working": acceptance gates (every phase)

A phase is done only when **all** of these pass:

1. **Single-language unchanged.** With no `language` block, every existing test passes
   unmodified and the English e2e corpus gives identical transcripts, paths
   (`command`/`home`/`system1`/`system2`), and canned replies.
2. **English non-regression in bilingual mode.** With `["en","pt-BR"]` enabled, on the
   English corpus (§4):
   - transcripts are at least as accurate as today's pinned `language_code=en`;
   - every English fast path still hits the same path with the same reply;
   - System-1 decisions are unchanged;
   - the English voice is used;
   - recall results match;
   - turn latency (end-of-speech → first audio) is within **+50 ms p50**.
3. **No wrong-language replies.** On both corpora, the reply's language (`whatlang` on
   the reply text) equals the spoken language, and the TTS voice matches. Target ≥ 98%,
   and **100% on ≥ 3-word utterances**.
4. **Portuguese parity.** Every capability works in pt (see the matrix below). In Phase 1
   a pt capability may be served by System-2 (correct, but slower); by the end of Phase 2
   each must take the same path as its English twin.
5. **Mixed chains.** A follow-up chain pt → en → pt answers each turn in its own language.

| Capability | English (today) | pt-BR after Phase 1 | pt-BR after Phase 2 |
| --- | --- | --- | --- |
| General Q&A, stories, tools via LLM | System-2 | System-2, pt reply + pt voice | same |
| Time / date / weather / timers | System-1 | System-2 (System-1 skipped) | System-1 + pt `Phrases` (weather screen stays English) |
| Home Assistant commands | fast path | LLM `home_control` tool | fast path + pt aliases |
| "Remember / forget" | fast path | LLM / inferred capture | fast path |
| Personality switch | fast path | `set_personality` tool | fast path |
| Memory recall | nomic | nomic (measured, §2.8) | same |
| Display | English chrome | English chrome; streamed pt text shown | same (by decision) |
| Ambient weather push | English | English | English (by decision) |

## 4. Testing

- **Corpora (no recordings needed; macOS has both voices):** an **English corpus** (every
  fast-path phrasing in the existing unit tests + ~30 System-2 questions + short
  commands like "stop", "never mind") and the matching **pt-BR corpus**:
  ```bash
  say -v Samantha -o /tmp/en/NN.wav --data-format=LEI16@16000 "what time is it"
  say -v Luciana  -o /tmp/pt/NN.wav --data-format=LEI16@16000 "que horas são"
  anamanti-core/scripts/e2e_wyoming_client.py 127.0.0.1 <test port> /tmp/pt/NN.wav
  ```
  Run on a **test Core** (distinct `bind_addr`/`config_addr`/`service_name`,
  `memory_backend="sqlite"` if it shares prod data) with the **ElevenLabs** engine, as
  in production. Capture transcript, `TurnLang`+source, path, reply, and voice per clip,
  and diff bilingual-on vs. single-language for the English gate.
- **Unit:** the resolver (each signal, disagreement rule, es/gl→pt mapping, short
  utterances); ElevenLabs message parsing (both committed variants, ISO-639-3 codes, no
  double transcript); each pt phrase table; `Phrases` formatters; prompt block present
  only in bilingual mode.
- **System-1:** pt calibration cases on `jev`.
- **Memory (Phase 3):** seed a mixed en/pt chat log, then check pt→pt, en→en (must equal
  today), and en↔pt recall@k.
- **Latency:** end-of-speech → first audio per language, and the first turn after boot
  (pre-warm on vs. off).
- **On device** (current release APK, no rebuild): pt diacritics render in the
  conversation view and in recipe/place cards; the weather screen stays English after a
  pt weather question.

## 5. Phases

| Phase | Delivers | Size |
| --- | --- | --- |
| **1a. Language foundation + ElevenLabs** | `lang/` + resolver; config/settings/Household field; `Transcript{text,lang}`; ElevenLabs bilingual (A/B chosen by measurement); `tts_voices` + first-sentence check + pre-warm; prompt block; **System-1 skipped for pt**; turn-language logging + Conversations chip; corpora + gate harness. **Result: production goes bilingual on its current engine.** | M |
| **1b. Other STT engines** | wyoming + whisper-rs bilingual (multilingual model resolution/fetch, detect-then-pin, `data.language`) + the config-page warning. | S–M |
| **2. Localized fast paths + System-1 in pt** | pt `Phrases` for all spoken canned replies (incl. the spoken weather sentence); pt matchers (§2.6); HA aliases; **pt System-1 calibration + per-intent enable (required)**. | L |
| **3. Memory in pt** | pt `infer_memories`; extraction-prompt line; recall eval on the current embedder, with the result reported to the owner. | S |
| **4. Polish** | Per-language `normalize` rules; `places` `languageCode`; console polish; wake-word threshold check with pt-accented "Hey Jarvis". | S |

(There is no Display phase, by decision §0. Every phase ships as a Core-only deploy.)

Every phase must pass the §3 gates before it ships to production.

## 6. Risks

- **Bilingual STT may cost English accuracy.** Unpinning ElevenLabs from `en` is the
  biggest risk to English. Mitigation: candidate A (`secondary_languages`) is tried
  first, gate 2 blocks any regression, and removing the `language` block rolls back.
- **Short-utterance misdetection** ("para", "ok", "stop"). Fast paths try both
  languages; for System-2 the tie-break order (§2.2) applies; and the result is logged
  for tuning.
- **pt recall on an English-centric embedder** (§2.8): measured in Phase 3 and
  escalated if needed.
- **HA naming:** pt commands only reach devices that have pt names/aliases in HA.
- **LLM drift to European Portuguese/Spanish:** the prompt names pt-BR explicitly.

## 7. Open questions

None. All resolved in §0 (2026-10-19/20). One interpretation to confirm when Phase 2
lands: "weather uses English" is read as **display + ambient push English, spoken answer
in the turn's language** (§0, §2.9).

## 8. Implementation status and measured results (2026-10-20)

### 8.1 What shipped (Core only)

| Piece | Where |
| --- | --- |
| `Lang`, `LanguageSettings`, `TurnLang`, resolver, prompt block, setup warnings | `src/lang/mod.rs` |
| Short-phrase table, function-word cross-check, trigram text ID (`whatlang`) | `src/lang/detect.rs` |
| Every spoken canned reply, en + pt-BR (English byte-identical, pinned by tests) | `src/lang/phrases.rs` |
| Per-turn resolution, voice switching, first-sentence reply check | `src/orchestrator/language.rs` |
| STT `Transcript { text, lang }`; ElevenLabs bilingual session; whisper-rs detection; Wyoming `data.language` | `src/stt/` |
| Memory commands / inferred capture / scope words / identity guard in pt | `src/memory/{extract,scope}.rs`, `prompt.rs` |
| Home Assistant pt grammar + English-shaped catalog candidates + pt replies | `src/home_assistant/pt.rs` |
| Personality switch in pt (incl. infinitive forms) | `src/orchestrator/prompt.rs` |
| pt timer/duration parsers; per-intent System-1 gate `PT_BR_READY` | `src/system1/mod.rs` |
| Config `language`, `tts_voices`, `tts_addrs`, `stt.multilingual`; persisted overlay; Household `language` | `config.rs`, `settings.rs` |
| Config page: Voice → Languages card, Household language column, Conversations chip, System view | `src/webconfig/` |
| Boot log + warnings; per-language voice warm-up | `src/main.rs` |
| Tests: `tests/bilingual.rs` (10 pipeline turns), unit tests per module, `calibrate_portuguese`, whisper-rs bilingual test, `examples/bilingual_recall.rs` | |

### 8.2 Deviations from the plan (each found by measurement)

1. **ElevenLabs = candidate A** (`language_code=<default>` + `secondary_languages` +
   `include_language_detection`). Probed live: English transcripts identical to the pinned
   `en` session; Portuguese correct. Candidate B (auto-detect) already changed English output
   ("What time is it？" with a full-width `？`), so it was rejected.
2. **whisper-rs: decode-then-detect, not detect-then-pin.** whisper.cpp's detection is a
   second encoder pass (+~310 ms on every turn with `small` on Metal, which fails the +50 ms
   English gate). Now the utterance is decoded pinned to the default and the language is read
   from one decoder step over the existing encoder state, then re-decoded only when another
   language wins. Measured: **English +0–26 ms** (median ~+9), Portuguese ~+400 ms.
3. **Two extra language signals ahead of / alongside the engine label** (§2.2 grew):
   - a **short-phrase table** (whole utterance: "para", "obrigado", "stop", "thanks", …;
     disjoint lists). whisper.cpp labelled a lone "para" as English.
   - a **function-word cross-check** that overrules a disagreeing engine label. Live,
     ElevenLabs labelled the *English* "How do you say good morning in Portuguese?" as
     Portuguese, and the turn was answered in Portuguese. The transcript's function words
     (how/you/in) now settle it. The first-reply-sentence voice check uses it too: live, an
     English answer to "responde em inglês…" was read by the Portuguese voice before this.
4. **Per-language Piper servers (`tts_addrs`).** wyoming-piper 2.5.2 keeps only the most
   recently used voice in memory, so on one shared server **every language switch reloads
   the model**: measured +450–515 ms warm and ~1 s cold on the first sentence. With a second
   Piper for pt-BR (same voice folder), alternating en/pt turns stay at **50–139 ms**
   time-to-first-audio. Without `tts_addrs` everything still works; it's just slower on
   switches. The plan's "pre-warm both voices" is implemented too (needed for a split setup;
   it ends on the default language for a shared one).
5. **Portuguese personality triggers include infinitives** — ElevenLabs often returns
   "Voltar ao normal." for "volta ao normal".
6. **System-1 gate is precision-first.** An intent is enabled for pt when at least one pt case
   resolves correctly and **nothing** resolves into it wrongly; a missed resolve only costs
   latency (System-2 answers in pt). (The `recipe_step` / `recipe_ingredient` intents that arrived from `main` afterwards are
   English-only until calibrated in pt.) `PT_BR_READY` = timer, time, date, timer_cancel,
   timer_query, timer_adjust, recipe_dismiss, end_session, stop_dismiss (stable across two
   calibration runs on `jev`). **Not enabled:** `weather` ("fecha a previsão do tempo" was
   classified as a weather request), `weather_dismiss` and `place_dismiss` (no pt phrasing
   resolved).
7. Wyoming faster-whisper 3.8.1 does **not** send `transcript.data.language` (the protocol
   field exists; the server doesn't fill it), so on that engine the short-phrase table,
   function words and trigram ID decide.

### 8.3 Live end-to-end gates (test Cores: ElevenLabs STT, Claude Haiku, `jev`, Piper)

Two isolated test Cores ran side by side (production untouched): **A** single-language
(no `language` block) and **B** bilingual, driven by a device emulator with 14 English
(`say -v Samantha`) and 15 Portuguese (`say -v Luciana`) clips covering every path. The
reply audio was re-transcribed with ElevenLabs to check the *spoken* language, and the test
Piper's debug log recorded the voice per sentence.

| Gate | Result |
| --- | --- |
| 1. Single-language unchanged | Unit + pipeline suites unchanged (`cargo test` fully green after merging `main` 2026-10-21); `tests/bilingual.rs` pins byte-identical English with no `language` block. |
| 2. English in bilingual mode | 14/14 same path and same System-1 decision as Core A; every canned reply identical (differences were only the clock minute / live temperature); English voice on every English sentence. One transcript differed by capitalisation (`good`/`Good`) — re-probed: identical audio gives identical text under both configs (ElevenLabs streaming variance). **Latency:** median STT 152 ms (B) vs 179 ms (A); median total 513 vs 529 ms. |
| 3. Reply language = spoken language | All answerable turns correct after the fixes in §8.2 (incl. "how do you say good morning in Portuguese" → English, "responde em inglês" → English with the English voice). Two "mismatches" were the *checking* method on one-word replies ("Okay." re-transcribed as Korean/Hungarian, "Timer de 10 minutos iniciado." as Spanish) — the Piper log confirms the right voice for each. |
| 4. Portuguese parity | Every English path has its pt twin on the same path (time/date/timer → System-1, memory commands → command, jokes/questions/personality → System-2/fast path), except weather (System-2 by the gate in §8.2 #6, still answered in pt). |
| 5. Mixed chains | Alternating en/pt turns each answered in their own language; latency flat with `tts_addrs` (§8.2 #4). |

**Known limitation (not bilingual-specific):** a very short single word ("para", ~0.35 s of
synthetic speech) didn't clear the VAD's 250 ms minimum-speech onset, so the turn ended as
silence — the existing rule, and the same for an equally short English word.

### 8.4 Memory recall on the current embedder (Phase 3, measured, not changed)

`examples/bilingual_recall.rs`, production nomic-embed-text-v1.5 (local), mixed store of 24
facts (12 en + the same 12 in pt):

| Direction | recall@1 | recall@3 |
| --- | --- | --- |
| en question → en fact | 12/12 | 12/12 |
| pt question → pt fact | 12/12 | 12/12 |
| en question → pt fact | 0/12 | 6/12 |
| pt question → en fact | 0/12 | 2/12 |

Same-language recall is perfect. **Cross-language recall is weak**: a fact stored in English
is usually not found by a Portuguese question (and vice versa). The SQLite FTS fallback is
lexical, so it has the same limit. **Open owner question:** accept this, or switch the
embedder (e.g. a multilingual model, or `openai`) — per-signature stores make a switch
lossless, but it's outside this plan's decisions.

## 9. Production rollout (not done — needs a go-ahead)

1. Install `pt_BR-faber-medium.onnx` + `.onnx.json` (rhasspy/piper-voices) into the
   production Piper voice folder (`Anamanti Core/services/models/piper`).
2. Start a second Piper on, say, `127.0.0.1:10201` with the same `--data-dir` and
   `--voice pt_BR-faber-medium` (and a launchd entry alongside the existing one).
3. Add to production `anamanti.json`:
   `"language": {"enabled": ["en","pt-BR"], "default": "en"}`,
   `"tts_voices": {"pt-BR": "pt_BR-faber-medium"}`,
   `"tts_addrs": {"pt-BR": "127.0.0.1:10201"}` — or set Languages on the Voice page.
4. Redeploy the Core with the runbook in agents.md. The Display needs nothing.
5. Rollback: remove the `language` block (or untick Português on the Voice page).

