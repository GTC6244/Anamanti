# Wake-Word Tuning Guide

Practical, field-tuning guide for the Anamanti Display wake word (openWakeWord via
`tract-onnx`). For the design rationale see [`wakeworddetection.md`](./wakeworddetection.md);
for the hardware tuning log see [`TODO.md`](./TODO.md).

All settings below live in **Settings → Listening section** on the device. The idle
threshold and capture gain can also be tuned **live, without restart** from the
**Audio Diagnostics screen** (which shows a live wake-score meter, smoothed score,
and threshold line).

---

## Key facts before you tune

- **There is no "female voice" knob.** The openWakeWord models are trained on a broad
  range of voices (incl. female). A missed voice means its speech produces **lower /
  briefer confidence peaks**, not that a pitch range is unsupported.
- **Capture gain does NOT affect detection.** openWakeWord's log-mel front-end is
  **amplitude/scale-invariant** (`wakeworddetection.md` §4.4). `captureGainDb` only
  changes the level meter and the PCM streamed to STT *after* a trigger. A very high
  gain can even hurt by clipping/distorting loud input.
- **Sensitivity vs. false triggers is a direct trade-off.** You cannot simultaneously
  maximize "picks up a soft voice" and "never fires randomly." Aim to **separate the
  score distributions** (front-end fixes), not just chase a threshold between two
  overlapping clouds.

---

## The settings that matter

| Setting | Default | More sensitive (pick up soft voice) | Fewer false hits |
|---|---|---|---|
| Idle `threshold` (Sensitivity) | `0.5` | Lower → `0.35–0.4` | Raise → `0.55–0.65` |
| `fireOnPeak` (Fire-on-peak) | `false` (window average) | ON (fires on peak) | OFF (averages out spurious spikes) |
| `smoothingWindow` | `2` | Lower → `1` | Raise → `3–4` |
| `activeThreshold` (while speaking) | `0.7` | Lower proportionally (keep > idle) | Raise → `0.8+` (suppress self-triggers) |
| `captureGainDb` | `0.0` | **Leave alone** for detection | Back off if cranked (clipping) |

Front-end levers (affect the audio reaching the model — use when scores themselves are
low/erratic, not just the threshold):

| Setting | Default | Notes |
|---|---|---|
| `micSource` | `6` (VOICE_RECOGNITION) | `6` = far-field analog path (recommended), `7` = VOICE_COMMUNICATION (more aggressive processing, can hurt), `1` = raw MIC. A/B `6` vs `1`. |
| `platformNs` (noise suppression) | `true` | Toggling **off** can raise a soft voice's score; keeping **on** can reject noise. Test both. |
| `platformAgc` | `true` | Auto gain control. |
| `platformAec` | `false` | In-app AEC off by design; echo cancellation comes from the device HAL shim. |

Fixed (not user-tunable): `DETECTION_COOLDOWN = 1500 ms` prevents rapid repeat fires,
so false hits are spaced-out singles, never machine-gun bursts.

---

## Problem 1 — a voice is getting missed (e.g. a quieter / female voice)

1. Open **Audio Diagnostics**. Have **that person** say the wake word ~10× at a normal
   distance/volume.
2. Read their **peak score** each time. (A well-captured, close setup hit 0.95–0.999 in
   the field — `wakeworddetection.md` §6.)
3. Set the idle `threshold` **just below their reliable peak floor** (e.g. floor ~0.45 →
   threshold ~0.4).
4. Turn **fire-on-peak ON**.
5. Still flaky? Drop **smoothing window to 1**, then try **platform NS off** and
   **mic source 1 vs 6**.
6. If peaks are very low/erratic (< ~0.3), it's a **capture/SNR problem**, not a
   threshold problem — fix the front-end (mic source, NS, distance, room noise), not the
   threshold.

---

## Problem 2 — random / false wake-word hits

First, identify **when** they happen (watch the meter or note the device state):

### A. False hits while speaking / music playing — *self-triggers*
The wake word stays active during playback (for barge-in), so the device can hear its
own output.
- **Raise `activeThreshold`** (default `0.7` → `0.8+`) — the bar specifically for
  during-a-turn.
- **Verify the AEC shim** (`libamznaec_shim.so`) is loaded on *this* device — if echo
  cancellation isn't working, playback bleeds into the mic.
- Confirm the **Core ducks music** while speaking.

### B. False hits while idle / in background noise — *environmental false accepts*
openWakeWord can fire on phonetically-similar speech and some noise.
- **Raise idle `threshold`** (`0.55–0.65`).
- **Turn fire-on-peak OFF** (averaging rejects brief spurious spikes).
- **Raise smoothing window** to `3–4`.
- Consider the **wake word choice** — some models false-trigger more than others in a
  given room (`hey_jarvis` is the bundled default).

---

## When sensitivity and false-rejection conflict

1. In Audio Diagnostics, note the **reliable peak scores of the wanted voice**.
2. Note what the **false hits score at**.
3. If the ranges **overlap**, no single threshold separates them cleanly. Fix the
   **front-end** instead: platform NS back on, test mic source, reduce room-noise
   sources. Separating the two score distributions beats chasing a threshold between
   overlapping clouds.

---

## Testing checklist

- [ ] Test on a **`--release` APK** — debug Rust makes tract-onnx ~3.6× slower on the
      32-bit device and starves the wake-word loop (masquerades as audio bugs).
- [ ] Tune on the **actual device**, with the **actual speaker(s)**, in the **actual room**.
- [ ] Use the **Audio Diagnostics** live meter + sliders (gain, idle threshold) — no
      restart needed; live-tuned values are re-seeded into the engine on reconnect.
- [ ] Change **one variable at a time**; re-measure peak scores after each.
