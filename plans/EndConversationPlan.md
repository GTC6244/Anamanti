# End-of-Conversation Intent

How to let the user **end a conversation with a vocal command** instead of relying
on silence.

Status: **design / options** (not yet built). Read alongside
[`architecture.md`](./architecture.md) §4 (state machine + follow-up listening) and
[`Plan.MD`](./Plan.MD).

---

## Problem

Today a conversation ends only when the **follow-up listen** window times out on
silence. After every reply the orchestrator sends `ambient-listen` with a
`wait_secs` window (10 s after a `?` reply, 5 s otherwise); the device reopens the
mic with **no wake word** and the chain continues as long as the user keeps
talking. The orchestrator sleeps the chain only on silence.

That breaks down when there is **a second human in the room**: the user is still
talking (to the person, not the device), so the chain never sees silence and never
sleeps — and the assistant may answer speech that wasn't aimed at it. Going silent
to end a turn is not useful mid-human-conversation.

**Goal:** end a conversation with one or more spoken commands — e.g.
*"Jarvis, I'm done now"* — by detecting the intent to end.

**Restated:** during a follow-up turn, decide whether an utterance is an intent to
**end** the conversation rather than a **new request** — without adding false
positives that cut off real turns.

---

## Options considered

### 1. Dismiss-phrase matcher on the transcript (dumbest thing that works)
After STT on a follow-up turn, run the transcript through a small list of end
phrases *before* it reaches the LLM: "I'm done", "that's all", "thanks Jarvis /
thank you", "goodbye", "never mind", "stop", "go away", "we're done here".
Normalize (lowercase, strip punctuation, fuzzy match) and require the utterance to
be **short and mostly the phrase**, so "thanks, now what's the weather" does not
trigger.
- **Pro:** ~20 lines, deterministic, zero latency, no model cost, unit-testable.
- **Con:** brittle to unlisted phrasing; the "mostly the phrase" guard is
  hand-tuned.
- Good as a **fast path** even alongside something smarter.

### 2. Wake-word-scoped command: "Jarvis, …" as an explicit close
"Jarvis, I'm done" is a strong signal *because* it re-addresses the assistant by
name mid-conversation, which people don't normally do. Treat a follow-up utterance
that **starts with the wake word** as a control channel: wake word + dismiss
phrase ⇒ end; otherwise it's a normal barge-in / new turn. The wake word is already
scored continuously on-device.
- **Pro:** near-zero false positives, matches the Jarvis mental model, works even
  while chatting with a human because the device is deliberately addressed.
- **Con:** user must remember the name; transcript must preserve the leading wake
  word (openWakeWord fires, but Whisper may not transcribe it cleanly).

### 3. Intent classifier as a cheap LLM pre-pass
Before the main turn, ask a tiny/fast model: *"Is this utterance the user ending
the conversation, continuing it, or addressing someone else? → {end, continue,
aside}"*. Uses the existing pluggable LLM trait / rig.
- **Pro:** robust to phrasing; the **aside** label directly handles "talking to
  another human" — drop those utterances without ending or answering.
- **Con:** adds a round-trip of latency + cost per follow-up turn; needs a
  fast/local model. Mitigate by gating behind #1 (only classify when ambiguous).

### 4. Main LLM emits an end-of-conversation control token / tool
Give the LLM an `end_conversation` tool (or a `<end/>` sentinel) plus a
system-prompt instruction: "If the user signals they're finished, give a short
sign-off and call `end_conversation`." The orchestrator watches the stream; if the
tool fires, it suppresses the follow-up `ambient-listen` so the chain sleeps after
the goodbye.
- **Pro:** **no extra model call** — reuses the turn already running; full
  conversation context, so it handles subtle/implicit endings and gives a natural
  goodbye. Fits existing tool-calling infra.
- **Con:** non-deterministic (LLM must *choose* to call it); only ends **after** it
  answers, so it can't silently ignore an aside to a human. Pair with #1 for
  hard-stop cases.

### 5. Addressing / target detection (fixes the second-human root cause)
The deeper issue is that follow-up mode answers *everything* it hears. Add an "is
this addressed to me?" gate on follow-up turns (wake word present, second-person
imperative/question at the assistant, vs. conversational speech aimed elsewhere).
Unaddressed utterances are ignored (don't answer, don't reset the window); after
the window elapses with no addressed speech, the conversation ends naturally.
- **Pro:** fixes the actual complaint; "I'm done" becomes one ending among many;
  casual human chatter stops feeding the chain.
- **Con:** hardest to get right; false-negatives feel like the assistant "went
  deaf." Effectively #3 with an `aside` class, optionally weighted by the
  already-compiled-in ECAPA-TDNN speaker ID (weight by *who* is speaking).

### 6. Shorten the leash + easy re-entry (behavioral, not detection)
Sidestep detection: make exiting cheap. Tighten the follow-up window, add a visible
on-screen "listening…" indicator with a countdown, and let ending = just stop
talking *to the device*; re-entry = the wake word (instant). Optionally a tap
dismiss on the Echo Show touchscreen.
- **Pro:** no new failure modes, no model cost; display already renders live state,
  so the affordance is nearly free; tap-to-dismiss is a guaranteed escape hatch.
- **Con:** not a literal "vocal command"; a short window annoys if the user pauses
  mid-thought while genuinely addressing the assistant.

---

## Decisions taken (2026-09-24)

From discussion, the target and budget are settled:

- **Target case: explicit dismiss only.** Want a reliable spoken "I'm done"-style
  command. (Second-human / addressing-detection, #3 / #5, is *not* in scope now —
  revisit later if the second-human case proves common.)
- **Latency/cost budget: reuse the main turn.** No dedicated classifier call
  (rules out #3 as the primary mechanism). Let the main LLM emit an end
  tool/token; accept that it ends *after* it answers.

## Recommended design

**Primary — `end_conversation` tool on the main LLM (#4).**
Add a tool the LLM can call during its normal turn, plus a system-prompt line:
> "If the user indicates they're finished (e.g. 'that's all', 'I'm done',
> 'goodbye', 'thanks Jarvis'), give a brief sign-off and call `end_conversation`."

The orchestrator watches the reply stream; when the tool fires it **suppresses the
follow-up `ambient-listen`** for that turn so the chain sleeps after the goodbye.
No extra model call. Inherent caveat (accepted): it ends only *after* the assistant
answers — "Jarvis, I'm done" yields a short "Goodbye" then closes; it can't
silently swallow the utterance. A spoken sign-off is good Jarvis UX anyway.

**Guardrail — deterministic fast-path dismiss (#1 + #2).**
Before the turn hits the LLM, run the follow-up transcript through a tiny matcher:
- **Wake-word-prefixed** ("Jarvis, I'm done") ⇒ end immediately, canned sign-off,
  no LLM call. Near-zero false positives.
- A short **bare dismiss phrase** that *is* essentially the whole utterance ⇒ same.

Instant, zero-cost, deterministic exit for common phrasings; the LLM tool (#4)
catches everything else. The matcher must require the utterance to be **short and
mostly the phrase** so "thanks, now what's the weather" routes to a normal turn.

**Where it lives.** Both hooks sit in the orchestrator's **follow-up path** — the
code that decides whether to send `ambient-listen` with a `wait_secs` window after
a reply. The device side needs **no changes**: it already just obeys whether
`ambient-listen` arrives or not.

---

## Open questions (decide before implementation)

- **Exact dismiss-phrase list** for the deterministic matcher, and whether the
  wake word must lead.
- **Sign-off behavior** — always speak a short "Goodbye"/"Sure thing", or allow a
  silent close when the deterministic fast-path fires.

## Next steps

1. Read `architecture.md` §4 + the follow-up-listen implementation to pin the exact
   insertion point in the orchestrator.
2. Record the decisions above in `Plan.MD` (decision table) since this changes the
   locked follow-up-listen behavior.
3. Implement: the `end_conversation` tool (#4) + the deterministic matcher (#1/#2)
   in the follow-up path; suppress `ambient-listen` on end.
