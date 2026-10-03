# Font Scale Plan: Voice "increase/decrease font"

**Targets:** Echo Show 8 (the display) + M4 Mac Mini (Anamanti Core)
**Feature:** hands-free readability — *"increase font"* / *"decrease font"* grows or
shrinks **all** on-screen text by one step, as a single device-wide scale that persists
across reboots.

> The global `TextScaler` wrap point and the `anamanti-font` frame are catalogued in
> [`DisplayUI.md`](./DisplayUI.md) and [`architecture.md`](./architecture.md) §4 — update
> them when this changes.

> **Status: implemented (2026-10-02).** Both crates + Flutter build; `anamanti-core` and
> device-rust font tests + the Flutter `font_scale_test.dart` are green. Pending on-device
> QA (say "increase font" with a card/chat up vs. a bare idle screen; confirm persistence
> and that `SettingsScreen` stays at default size).

---

## 0. Confirmed decisions

| Question | Decision |
| --- | --- |
| **Scope of scaling** | **One global scale** applied to **everything with text** on the ambient screen. Flutter's `Text` reads `textScaler` from the ambient `MediaQuery`, so a single override around `AmbientScreen` scales every hardcoded `fontSize:` at once — no per-widget edits. |
| **Persistence** | Device-wide and **persisted** — `AppSettings.fontScale` (device-local JSON), so it survives reboots. |
| **Range / step** | Clamp `[0.85, 1.6]`, additive step `0.1` (exact at-min/at-max edges). Consts in `app_settings.dart`. |
| **Gate ("only when context is provided")** | The device reports a `screen.font` context each turn; the command resolves only when `scalable` is true. This display always shows text (conversation + cards during a turn, the idle/away clock otherwise), so `scalable` is reported `true` whenever the UI is up — the gate only blocks a genuinely text-free state. Kept as a predicate so a future surface can opt out. |
| **Settings excluded** | `SettingsScreen` is a separate `Navigator` route, outside the scaled `MediaQuery` subtree, so it stays at the default size automatically. |
| **Recognizer** | The **`adjust_font` rig tool** (System-2) is the shipping path — it works with the default `rig` engine. Optional System-1 `font_increase`/`font_decrease` fast intents are an additive follow-up (System-1 is off by default). |
| **Core scale representation** | `FontContext.scale_permille: u16` (1100 = 1.1×) so `DeviceContext` / `DecisionRequest` keep their `Eq` derives. |

---

## 1. Topology

```
  "increase font" ──STT──▶ Anamanti Core
        turn's audio-start carried screen.font {scalable, scale, at_min, at_max}
                           │
     System-2: adjust_font tool (advertised; per-turn prompt line added only when
       scalable) ──▶ DeviceAction::AdjustFont(Increase|Decrease)
                           │ drain ──▶ WyomingEvent::font_adjust("increase")
                           ▼ anamanti-font frame
  Echo Show: FontCommand::Adjust ──▶ WakeWordEventKind.fontAdjust ──▶
     AssistantController.onFontAdjust ──▶ AppSettings.fontScale += step (clamped, saved)
     ──▶ setState ──▶ root MediaQuery TextScaler re-scales all text
     ──▶ updateFontScale ──▶ set_font_context (next turn reports the new scale)
```

---

## 2. Components & files

**Device — apply + persist (Flutter):**
- `lib/src/settings/app_settings.dart` — `fontScale` field (+ `kFontScaleMin/Max/Step`).
- `lib/main.dart` — wraps `AmbientScreen` in a `MediaQuery(textScaler: TextScaler.linear(fontScale))`; `_onFontAdjust` bumps + clamps + persists + `updateFontScale`.
- `lib/src/engine/assistant_controller.dart` — `FontContextSink` typedef; `onFontAdjust` / `setFontContext` / `_fontScale`; the `fontAdjust` event case; `_pushFontContext` (pushed at `start()` + `updateFontScale`).

**Device — report + decode (Rust):**
- `rust/src/engine/mod.rs` — `FONT_CONTEXT` OnceLock + `set_font_context` / `font_context`.
- `rust/src/api/engine.rs` — `set_font_context` FRB fn; `WakeWordEventKind::FontAdjust` + `font_adjust` ctor (direction rides the generic `recipe_action` field).
- `rust/src/wyoming/client.rs` — `font_context` field stamped as `screen.font`; `TurnUpdate::Font`.
- `rust/src/wyoming/protocol.rs` — `types::FONT` (byte-identical), `FontCommand`, `font_adjust` builder + `font_command` decode.
- `rust/src/engine/net.rs` — `set_font_context` at turn start; `TurnUpdate::Font → WakeWordEvent::font_adjust`.

**Core (Rust):**
- `src/wyoming/protocol.rs` — `types::FONT`, `font_adjust` builder, `FontContext` + `font_context` parse, `DeviceContext.font`.
- `src/llm/mod.rs` — `DeviceAction::AdjustFont(FontDirection)`.
- `src/orchestrator.rs` — drain arm → `font_adjust` frame; `font_context_line` prompt gate; `DecisionRequest.font`.
- `src/llm/rig.rs` — `adjust_font` tool (`ADJUST_FONT`, always advertised), guidance gated on `has(ADJUST_FONT)` + the per-turn prompt line.
- `src/system1/mod.rs` — `DecisionRequest.font` (ground truth for the optional fast intents).

**FRB:** regenerate with `flutter_rust_bridge_codegen generate` after the Rust signature
changes; never hand-edit `frb_generated.*`.

---

## 3. Tests
- Core `wyoming::protocol` — `font_adjust` round-trip; `font_context` parses orthogonally;
  cross-crate `FONT` constant equality (device side).
- Device `wyoming::protocol` / `client` — `font_command` decode; `audio_start` stamps
  `screen.font` even on an idle screen.
- Core `llm::rig` — `adjust_font` advertised; invoke → `DeviceAction::AdjustFont`; unknown
  action / no-sink error; guidance mentions the gate.
- Flutter `test/font_scale_test.dart` — `fontAdjust` event → `onFontAdjust`; font context
  pushed at start + on scale change; `AppSettings.fontScale` round-trip / clamp / default.

## 3a. Manual override — Settings slider

A touch alternative to the voice commands: **Settings → Device Config → Display → "Font
size"** (`_rangeSlider`, key `settings-font-scale`), shown as a percentage across
`[85%, 160%]` in 5% stops (finer than the 0.1 voice step). It edits the same
`AppSettings.fontScale`; on **Save**, `main.dart`'s `_onSettingsApplied` re-reads it (the
root `MediaQuery` re-scales via `setState`) and mirrors it to the controller
(`updateFontScale`) so the next turn's font context reports the new value. Voice and slider
are the same persisted value, so they stay in sync.

## 4. Non-goals (v1)
- Per-widget font sizes (one global scale only).
- Scaling the pushed `SettingsScreen` route.
- The System-1 fast path (left as an additive follow-up; the rig tool ships the behavior).

## 5. Docs updated on landing
`agents.md` (feature-plans list), `architecture.md` §4 (`screen.font` sibling +
`anamanti-font` frame), `Plan.MD` (decision rows), `DisplayUI.md` (the global `TextScaler`
wrap + SettingsScreen exclusion).
