# SettingsReorgPlan.md — Display Settings reorganization

**Status:** APPROVED, revision 3 (2026-10-13). All owner decisions are recorded in §0;
implementation is in progress on branch `ws-d9b483-d9b4832b` (see §7 for per-phase status).
**Scope:** `anamanti-display/lib/src/ui/settings_screen.dart` (2,359 lines) plus the
screens it opens (People, Memory, Link a phone, Audio Diagnostics).
**Goals (from the request):**
1. Group settings more logically.
2. Have fewer settings categories.
3. Minimize scrolling; prefer direct touch navigation.

Related: [`DisplayUI.md`](./DisplayUI.md) §9–§10 (the current catalog),
[`FontScalePlan.md`](./FontScalePlan.md) (Settings is outside the scaled
`MediaQuery`, so it always renders at 1.0×. That keeps the height budget below fixed).

---

## 0. Owner decisions (2026-10-13)

| # | Question | Decision |
|---|---|---|
| 1 | Section names | Approved, **except Audio Diagnostics stays a top-level section** (it's used often right now). That makes **6 sections**: Assistant · Listening · **Audio Diagnostics** · Display · Household · Device. |
| 2 | Expert controls | A single **Advanced mode** switch hides them (instead of per-control "Fine-tune…" dialogs). |
| 3 | Save model | **Apply changes immediately.** The Save button goes away (see §3.5). |
| 4 | Household screens | **Buttons that open the existing screens** (People, Link a phone, Memory). No embedding for now. |
| 5 | Screen | 1280×800 px; `adb shell wm density` = **213** on the device (checked 2026-10-13), so the logical screen is **960×600 dp**. The budget in §1.2 holds. |
| 6 | Live "sensitivity while speaking" | **Add a live (no-restart) apply path**: `updateDiagnosticsTuning` gains `activeThreshold`. |
| 7 | Engine restart mid-conversation | **Acceptable inside Settings** (an in-flight turn is dropped); no defer-until-idle. |
| 8 | Advanced mode default | **Starts on.** `AppSettings.advancedMode` defaults to `true`; a household install can switch it off. |

---

## 1. Current state

### 1.1 Top-level menu: 13 entries in a single scrolling list

| # | Entry | Kind | What it actually holds |
|---|---|---|---|
| 1 | Assistant | inline page | **Device name**, **Core pin** ("Orchestrator"), LLM backend, Anthropic auth, model, TTS voice |
| 2 | Functions | inline page (read-only) | Status of each Core capability: cloud/local/off, plus missing-key warnings |
| 3 | People | pushed route | Voice-identified speakers + "Add a person" enrollment |
| 4 | Memory | pushed route | Facts / Conversations / Keywords tabs |
| 5 | Link a phone | pushed route | AppSaid pairing |
| 6 | Device Config | inline page | Wake word + 2 sensitivities + smoothing + capture gain + fire-on-peak + AudioRecord (+ NS/AGC/AEC), **and** Display (dim delay, font size) |
| 7 | Audio Diagnostics | custom page | Live meters + capture gain + idle sensitivity (**both duplicated from #6**) |
| 8 | Speech Processing | inline page | Playback buffer, processing cue (+ 2 sliders), **listening ring** (+ 4 sliders) |
| 9 | Speech Detection | inline page | Core VAD: engine, Silero threshold, end-of-speech wait, voice level |
| 10 | Background | inline page | Photo source + Google Photos / Drive linking |
| 11 | Updates | custom page | Auto-update toggle, update URL, check/install |
| 12 | Permissions | custom page | Android grants + Grant buttons |
| 13 | System | custom page | Switch Home app, open Android settings |

### 1.2 How much each page scrolls

Budget: the Echo Show 8 panel is 1280×800 px at **213 dpi (verified)**, which gives a
**960×600 dp** logical screen. After the
56 dp AppBar, roughly **540 dp** of content is visible. The estimates below use standard
Material heights (one-line tile 56, two-line tile 72, three-line ~88, slider row ~48).

| Page | Est. content height | Screens tall | Notes |
|---|---|---|---|
| **Menu** | ~950 dp | **~1.8** | Only ~7 of 13 entries visible; Updates/Permissions/System are always below the fold |
| **Device Config** | ~1,050 dp | **~2.0** | Worst offender: long 2–3 line switch subtitles |
| **Functions** | ~15 rows × 72 ≈ 1,100 dp | **~2.0** | Read-only list, single column |
| Speech Processing | ~520 dp (all toggles on) | ~1.0 | Borderline; grows when ring/cue are enabled |
| Permissions | ~600 dp | ~1.1 | Just over |
| Assistant | ~430 dp | < 1 | OK |
| Speech Detection / Background / Updates / System | < 450 dp | < 1 | OK, but each costs a full navigation hop for 2–4 controls |

Corroboration: `test/settings_screen_test.dart` needs **17** scroll/drag helpers just to
reach controls.

### 1.3 Navigation cost

All navigation goes through a hub menu: open Settings → *(scroll)* → tap category → back →
*(scroll)* → tap the next category. Moving between two related pages (for example
Device Config → Speech Detection when tuning the mic) takes **3–4 touches plus scrolling**.
The landscape panel shows one column of ~440 dp-wide content and leaves about half the width
empty.

---

## 2. Critique

### 2.1 Grouping problems
1. **Identity and connection sit inside "Assistant".** Device name and the Core pin are
   about *this display*, not the AI. They also work offline, unlike the rest of that page.
2. **"Device Config" mixes two unrelated things**: microphone/wake-word tuning and
   screen appearance (dim delay, font size).
3. **The listening ring sits under "Speech Processing"**, but it is a purely visual
   effect. It belongs with Display.
4. **End-of-speech appears in two places under near-identical names.**
   "End-of-speech silence (ms)" (Speech Processing) only drives the *on-device visual
   cue*. "End-of-speech wait (ms)" (Speech Detection) is the *real* Core VAD setting. Users
   will tune the wrong one.
5. **Settings for one task are spread over four categories.** Tuning the mic means
   Device Config + Audio Diagnostics + Speech Processing + Speech Detection.
6. **Duplicate controls:** capture gain and idle sensitivity are on both Device Config and
   Audio Diagnostics, two categories apart.
7. **Functions (read-only status) looks like an editable category** and is separated from
   the Assistant page it describes.
8. **Household items are three separate top-level entries:** People, Memory and Link a phone.
9. **Maintenance items are three separate entries:** Updates, Permissions and System.
   They are rarely used and always below the fold.

### 2.2 Interaction and touch problems
10. **Hub-and-spoke navigation**: there is no way to go sideways between categories.
11. **Single-column layout** on a wide landscape screen. This doubles page height for no
    reason.
12. **Long helper text in switch subtitles** (NS, AEC, AudioRecord, fire-on-peak) makes
    each tile 88–104 dp tall.
13. **Small enumerations use sliders or dropdowns**, which are harder to hit by touch than
    segmented buttons. Examples: the 8 dim presets, Energy/Silero, API key/Subscription,
    and the 3 photo sources.
14. **Keyboard-only fields on a kiosk**: the Drive folder-ID text field (the "Choose folders"
    picker already covers it) and the Update URL (a developer setting).
15. **Leaving Settings silently discards unsaved edits.** At the menu, `PopScope` lets
    back pop, and nothing warns about pending changes. **Save** also exits Settings
    entirely.

### 2.3 Bug found while planning
20. **Mic settings changed from Device Config don't take effect until the next engine
    restart.** `_onSettingsApplied` (`lib/main.dart`) restarts the engine only when
    fields in its `engineChanged` list change. The list leaves out **capture gain,
    AudioRecord capture, noise suppression, AGC and echo cancellation**, yet all five
    are passed into the engine config (`buildWakeWordConfigFrom`). So toggling, for
    example, noise suppression and saving does nothing until a later change (or a
    reboot) restarts the engine. The exception is capture gain set on the Audio
    Diagnostics page, which is pushed to the running engine live.
    **This must be fixed before "apply immediately"** (§3.5), or those switches will
    appear dead.

### 2.4 Stale or incorrect copy (small fixes; can ship independently)
16. "Orchestrator" label and messages → **"Anamanti Core"** (the project-wide rename).
17. VAD engine: the client-side fallback is `'energy'` and the subtitle/comments say Silero
    needs an opt-in feature. **Silero has been the Core default since 2026-09-30.**
18. AEC switch subtitle says "the Mac does AEC". Echo cancellation is actually done by the
    device's **audio-HAL shim** (`libamznaec_shim.so`).
19. The file header comment lists only 5 of the 13 categories.

---

## 3. Proposal: 6 sections, side rail, tabs that fit on one screen

### 3.1 Information architecture (13 → 6)

| Rail section | Tabs (each fits one screen) | Pulls from (today) |
|---|---|---|
| **Assistant** | **Brain** · **Capabilities** | Assistant (LLM, auth, model, voice) · Functions |
| **Listening** | **Wake word** · **Microphone** *(Advanced only)* · **End of speech** | Device Config (wake word + mic) · Speech Processing (playback buffer, cue timing) · Speech Detection |
| **Audio Diagnostics** | *(none; one full page)* | Audio Diagnostics, unchanged |
| **Display** | **Screen** · **Photos** | Device Config (dim, font) · Speech Processing (listening ring, processing cue) · Background |
| **Household** | *(none; three large buttons)* | People · Link a phone · Memory, each opening its existing screen |
| **Device** | **Connection** · **Updates** · **Maintenance** | Assistant (device name, Core pin) · Updates · Permissions + System (+ the Advanced mode switch) |

Each name answers a user question: *what does it think with* (Assistant), *how does it
hear me* (Listening), *is the mic working right now* (Audio Diagnostics), *what does it
look like* (Display), *who does it know* (Household), *which box is this and is it
healthy* (Device).

### 3.2 Shell layout

Replace the hub menu with a **`NavigationRail`** (left, ~80 dp) plus a content pane.
Settings opens directly on **Assistant → Brain**, so there is no menu page. Six rail
destinations at ~72 dp each (~432 dp) fit the 544 dp below the AppBar without scrolling.
Inside a section, a top **`TabBar`** (48 dp) switches tabs. Any setting is then **at most
2 touches** away, and there is no back-stack within Settings. The only pushed routes left
are the three Household screens and the photo-linking dialogs.

```
┌────┬──────────────────────────────────────────────────────────────┐
│ ←  │ Listening                         ⟳ Restarting listener…     │  AppBar 56
├────┼──────────────────────────────────────────────────────────────┤
│ 🤖 │  Wake word │ Microphone │ End of speech                      │  TabBar 48
│Asst├──────────────────────────────┬───────────────────────────────┤
│ 🎙 │ LEFT COLUMN (~430 dp)        │ RIGHT COLUMN (~430 dp)        │
│List│                              │                               │  ~496 dp
│ 📈 │                              │                               │  usable
│Diag│                              │                               │
│ 🖥 │                              │                               │
│Disp│                              │                               │
│ 👥 │                              │                               │
│Hous│                              │                               │
│ ⚙ │                              │                               │
│Dev │                              │                               │
└────┴──────────────────────────────┴───────────────────────────────┘
```

There is no Save button (§3.5). The AppBar's right side carries transient apply status:
"⟳ Restarting listener…", "✓ Applied", or "Couldn't reach the Core".

**Height budget per tab: ≤ ~496 dp per column** (~544 dp for sections without tabs). An
outer scroll view stays only as a safety net, and a test enforces that it never scrolls
at 960×600 (see §5).

### 3.3 Advanced mode

- New device-local field **`AppSettings.advancedMode`** (default **on**, decision #8; persisted like
  every other device-local setting). The switch lives in **Device → Maintenance**. While
  it is on, an "Advanced" chip shows in the AppBar so the mode is never a surprise.
- **Off (household view):** only the controls a non-developer should touch.
- **On:** expert controls appear **in place**, in the same tab and column, and the
  Listening → Microphone tab appears. They are not in dialogs.
- **Audio Diagnostics is always shown** in both modes, per decision #1.

| Shown always | Shown only in Advanced mode |
|---|---|
| LLM backend, Anthropic auth, model, TTS voice | "Offline echo (mock)" backend option |
| Wake word, sensitivity (idle), sensitivity (while speaking), capture gain | Smoothing window, fire on peak |
| — | **Microphone tab:** AudioRecord capture, noise suppression, AGC, echo cancellation, playback buffer |
| End-of-speech wait, instant processing cue on/off | VAD engine, Silero threshold, voice level threshold, cue silence + cue level |
| Dim delay, font size, listening ring on/off + reactivity | Ring attack, release, auto-range |
| Photo source, link/sync/choose folders | Raw Drive folder-ID text field |
| Device name, Core pin, auto-update switch, check/install | Update URL |
| Permissions, Android escape hatch, Advanced mode switch | — |

Because the advanced controls are mostly in lists of 1–4 items, the two-column layouts
leave room for them. Each tab's height budget is checked **with Advanced mode on**.

### 3.4 Fitting each tab on one screen

| Technique | Applies to |
|---|---|
| **Two columns** | Every editable tab |
| **Single-line rows**; long helper text moves to an ⓘ button that opens a small dialog | NS / AGC / AEC / AudioRecord / fire-on-peak / processing cue |
| **Segmented buttons** instead of sliders/dropdowns for ≤ 8 choices | Dim delay (30s…1h), VAD engine, Anthropic auth, photo source, LLM backend |
| **Compact status grid** (2–3 columns of chips) instead of a list | Capabilities, Permissions |
| **Disabled, not hidden**, for controls that depend on another switch | NS/AGC/AEC when AudioRecord is off; cue timing when the cue is off; ring dials when the ring is off. This keeps the layout from jumping. |

### 3.5 Apply changes immediately (no Save button)

Every control applies and persists as soon as it changes. Getting this right is the main
engineering work in the reorg:

1. **Fix bug #20 first.** Add capture gain, AudioRecord and NS/AGC/AEC to
   `engineChanged` in `_onSettingsApplied`. Otherwise immediate apply makes those switches
   look broken.
2. **Device-local settings** call `store.save` + `onApplied` on change.
   - **Sliders apply on `onChangeEnd`** (when the finger lifts). The UI still updates
     during the drag.
   - **Engine-restarting changes are debounced (~800 ms) and coalesced**, so flipping
     three switches in a row costs one engine restart, not three. Applies are
     **serialized**: `_onSettingsApplied` is async, so a new change waits for the
     in-flight apply and only the latest value is applied. The AppBar shows
     "⟳ Restarting listener…" while it runs.
   - **Sensitivity and capture gain skip the restart.** They go to the running engine
     through the existing `updateDiagnosticsTuning(gainDb, threshold)`, which is how
     Audio Diagnostics already applies them live. The "sensitivity while speaking"
     threshold had no live path, so `updateDiagnosticsTuning` gains an `activeThreshold`
     argument (decision #6; a small FRB change + regenerate).
   - Presentational settings (font, listening ring, dim delay) repaint immediately.
     Dim delay is currently in the engine-restart list because it feeds the proximity
     sensor. That's acceptable when debounced, but could later get a live path.
3. **Core (remote) settings** (LLM, model, voice, VAD) call `applySettings` on change,
   debounced (~500 ms), sending the full payload as today. **On failure:** a snackbar,
   and the control **reverts to the last value the Core confirmed**. When the Core is
   offline these controls are disabled under an "Assistant offline · Retry" banner, so
   nothing silently fails to apply.
4. **Text fields** (device name, Ollama model, Update URL, Drive IDs) apply on
   **submit or focus loss**, not on every keystroke. A device-name change restarts the
   engine.
5. **Photo linking flows** apply when they finish. The "Tap Save to apply" snackbars
   become "Linked" / "Synced".
6. Removed: the Save button, the `_saving` state, and the "Save pops Settings" behaviour.
   Also not needed: the planned unsaved-changes indicator and discard confirmation
   (critique #15 goes away by design).

### 3.6 Tab-by-tab layout

**Assistant → Brain**
- Left: LLM backend (segmented: Local · Claude · OpenAI, plus Mock in Advanced), Anthropic
  auth (segmented, only for Claude), Model (dropdown or text).
- Right: TTS voice (dropdown), a mini "Capabilities" health summary (e.g. "12 on · 1 key
  missing →", which jumps to the next tab).
- Offline: a single "Assistant offline · Retry" banner across the top. Controls are shown
  disabled rather than replaced by a guard tile, so the layout does not jump.

**Assistant → Capabilities** (read-only)
- 3-column grid of compact cards (name, provider · model, Cloud/Local/Off chip, or a red
  key-missing chip), grouped under the existing 5 headings. About 15 cards in 5 rows fit.
  Refresh moves into the AppBar.

**Listening → Wake word**
- Left: Wake word (dropdown), Sensitivity (idle), Sensitivity (while speaking).
- Right: Capture gain, a "Test it live →" link that jumps to the Audio Diagnostics rail
  section. *Advanced:* Smoothing window, Fire on peak (switch ⓘ).

**Listening → Microphone** *(Advanced mode only)*
- Left: AudioRecord capture (switch ⓘ), Playback buffer.
- Right: Noise suppression / AGC / Echo cancellation (single-line switches ⓘ, disabled
  when AudioRecord is off).

**Listening → End of speech** (resolves critique #4)
- Left, **"Core (decides when you're done)"**: End-of-speech wait. *Advanced:* VAD engine
  (segmented, Silero default), Silero threshold, Voice level.
- Right, **"On-screen cue"**: Instant processing cue (switch), with copy that says plainly
  that it only affects the visual cue. *Advanced:* cue silence + cue level.

**Audio Diagnostics** (rail section, no tabs)
- The existing `AudioDiagnosticsView`, unchanged in content. It is already two-column and
  no-scroll at 960 dp wide; with the 80 dp rail the pane is ~880 dp, so its column widths
  need a check (Phase 1). Its gain and sensitivity sliders write the same values as
  Listening → Wake word, and both apply live.

**Display → Screen**
- Left: Dim screen after (segmented 8 presets), Font size (slider + live preview text).
- Right: Listening ring (switch), Ring reactivity (slider). *Advanced:* attack, release,
  auto-range.

**Display → Photos**
- Source (segmented: Gradients · Google Photos · Drive). Below it is a card that changes
  with the chosen source: Link / Re-link (QR) for Google Photos; for Drive, a status line,
  [Sync from Core], and [Choose folders]. *Advanced:* the raw folder-ID field.

**Household** (rail section, no tabs)
- Three large touch cards in a row: **People** (recognize & train voices), **Link a
  phone**, **Memory**. Each pushes its existing screen unchanged (decision #4).

**Device → Connection**
- Left: Device name (+ ID), Anamanti Core (dropdown: Auto / discovered), live connection
  status (connected / offline + which Core).
- Right: app version, update status at a glance.

**Device → Updates**
- Auto-check switch, status + progress, [Check now] / [Install]. *Advanced:* Update URL.

**Device → Maintenance**
- Left: Permissions as a compact grid (Camera, Microphone with Grant buttons; others as
  chips), [Open app settings].
- Right: **Advanced mode** switch; kiosk escape hatch: [Switch Home app], [Open Android
  settings].

---

## 4. Control move map (old → new)

"(Adv)" = shown only in Advanced mode.

| Control (key) | Old category | New location |
|---|---|---|
| Device name (`settings-device-name`) | Assistant | Device → Connection |
| Orchestrator pin (`settings-orchestrator`) | Assistant | Device → Connection, relabelled "Anamanti Core" |
| LLM backend / auth / model / voice | Assistant | Assistant → Brain (mock backend: Adv) |
| Functions rows (`settings-function-*`) | Functions | Assistant → Capabilities |
| Wake word, sensitivities, capture gain | Device Config | Listening → Wake word |
| Smoothing window, fire on peak | Device Config | Listening → Wake word (Adv) |
| AudioRecord, NS/AGC/AEC | Device Config | Listening → Microphone (Adv tab) |
| Playback buffer (`settings-playback-buffer`) | Speech Processing | Listening → Microphone (Adv tab) |
| Processing cue switch | Speech Processing | Listening → End of speech, right "On-screen cue" |
| Cue silence / cue level | Speech Processing | Listening → End of speech, right (Adv) |
| End-of-speech wait | Speech Detection | Listening → End of speech, left "Core" |
| VAD engine, Silero threshold, voice level | Speech Detection | Listening → End of speech, left (Adv) |
| Audio Diagnostics view | Audio Diagnostics | **Audio Diagnostics** (rail section, unchanged) |
| Dim delay, font size | Device Config | Display → Screen |
| Listening ring + reactivity | Speech Processing | Display → Screen |
| Ring attack / release / auto-range | Speech Processing | Display → Screen (Adv) |
| Photo source + linking | Background | Display → Photos (folder-ID field: Adv) |
| People / Link a phone / Memory | top level ×3 | Household → three buttons opening the existing screens |
| Updates | Updates | Device → Updates (Update URL: Adv) |
| Permissions, System | Permissions, System | Device → Maintenance |
| *(new)* Advanced mode switch | — | Device → Maintenance |
| Save button (`settings-save`) | AppBar | **Removed** (apply immediately) |

All existing widget **keys are preserved** on the controls themselves. Navigation keys
change: `settings-menu-<category>` becomes `settings-rail-<section>` +
`settings-tab-<tab>`. The People, Memory and Link a phone keys
(`settings-menu-people`, `settings-menu-memory`, `settings-menu-pair-phone`) can stay on
the Household buttons.

---

## 5. Implementation plan (phased; each phase shippable)

**Phase 0: fixes (small, independent).**
- Bug #20: add capture gain, AudioRecord and NS/AGC/AEC to `engineChanged`.
- Copy items #16–#19: "Anamanti Core" labels, Silero default/subtitle, AEC subtitle,
  file header.

**Phase 1: shell + regroup.** Split `settings_screen.dart` (2.4k lines) into
`settings/` widgets: `settings_shell.dart` (rail, tabs, shared `AppSettings` + remote
state) plus one file per section (`assistant_section.dart`, `listening_section.dart`,
`display_section.dart`, `household_section.dart`, `device_section.dart`; Audio
Diagnostics reuses `AudioDiagnosticsView`) and shared row widgets (`_rangeSlider`,
`_section`, status chip). Move the controls per §4 and keep Save for now. This alone
removes the hub-menu scrolling and cross-category hopping.

**Phase 2: apply immediately.** Implement §3.5: debounced/serialized apply, live
sensitivity/gain, remote debounce + revert-on-failure, text fields on submit, AppBar
apply status. Remove Save.

**Phase 3: Advanced mode.** Add `AppSettings.advancedMode` (+ JSON round-trip), the switch
in Maintenance, the AppBar chip, and gating per §3.3.

**Phase 4: fit to screen.** Two-column layouts, segmented buttons, ⓘ help dialogs,
Capabilities and Permissions grids. Add a widget test that pumps every tab at
**960×600**, with Advanced mode **on and off**, and asserts the primary scrollable's
`maxScrollExtent == 0`. This guards against future regressions.

**Docs (same change as each phase, per `agents.md`):** update `DisplayUI.md` §9–§10,
the decision table in `Plan.MD`, and the settings line in `README.md`/`architecture.md`
if they name categories.

**Tests:** `test/settings_screen_test.dart` (navigation helpers; most of the 17 scroll
helpers go away; the 14 `settings-save` taps are replaced by apply-on-change assertions,
including debounce timing via `tester.pump(duration)`) and `test/permissions_test.dart`
(`settings-menu-permissions` → rail/tab keys). New tests: Advanced-mode gating, apply
serialization (three quick toggles → one restart), and remote revert on failure.

**Risk:** UI-only apart from Phase 0's restart-list fix and the optional
`activeThreshold` FRB argument. No Wyoming or persistence-format changes:
`advancedMode` is a new optional JSON field with a default, so older settings files load
unchanged.

---

## 6. Open items

None. All questions are answered in §0 (decisions #6–#8 resolved the last three).

---

## 7. Implementation status

| Phase | Status |
|---|---|
| 0 — fixes (bug #20, copy) | pending |
| 1 — shell + regroup | pending |
| 2 — apply immediately | pending |
| 3 — Advanced mode | pending |
| 4 — fit to screen | pending |
