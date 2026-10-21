//! The STT pump loop: forward device PCM to the STT engine, run the end-of-speech
//! state machine over it (onset debounce, `speech_started` latch, end-silence
//! hangover, no-speech fallback), and return the transcript.

use std::time::Duration;

use anyhow::Result;
use tokio::time::{sleep_until, Instant};

use crate::audio::pcm;
use crate::stt::{SttEvent, Transcriber};
use crate::vad::SpeechGate;
use crate::wyoming::protocol::{types, WyomingEvent};

use super::{Pipeline, TurnCtx};

/// Minimum span of *consecutive* voiced audio (chunks the gate marks as speech)
/// required before we latch `speech_started` and switch a turn's finalize clock from
/// the `no_speech_finalize` window to the much shorter `end_silence` window. This
/// debounces the speech onset: a lone transient — the residual TTS tail heard right
/// after a follow-up mic reopens, room echo, or a brief noise — would otherwise
/// collapse a 10 s "wait for the user to answer" window into ~1 s and cut the user off
/// (observed in production as a follow-up turn that finalized with an empty
/// transcript). Real speech clears this in a single word; a blip does not.
pub(crate) const MIN_SPEECH_ONSET: Duration = Duration::from_millis(250);

/// How the listening phase of a turn ended.
pub(crate) enum Listen {
    /// STT returned a transcript (empty when the turn finalized on the no-speech window
    /// and the STT text was discarded as a likely hallucination). Carries the voiced PCM
    /// for speaker ID and the end-of-speech → transcript latency.
    Heard {
        transcript: String,
        /// The language the STT engine reported for this utterance, if any (raw engine
        /// code; see `crate::stt::Transcript`).
        stt_lang: Option<String>,
        voiced_pcm: Vec<i16>,
        stt_latency: Duration,
    },
    /// The device closed the socket before a transcript.
    DeviceClosed,
    /// Nothing arrived from the device or STT for `turn_timeout`.
    IdleTimeout,
}

impl Pipeline {
    /// Pump loop: forward device `audio-chunk`s to STT, detect end-of-speech, and
    /// return STT's `transcript`.
    ///
    /// wyoming-faster-whisper does **not** do streaming VAD — it transcribes the
    /// buffered utterance only once it receives `audio-stop`. The device, meanwhile,
    /// streams continuously and waits for the transcript before it stops. So the
    /// orchestrator is the only party that can close the loop: it runs its VAD gate
    /// ([`SpeechGate`]) over the incoming PCM and, once speech has been followed by a
    /// short trailing silence (`end_silence_ms` from the turn snapshot), sends
    /// `audio-stop` to STT to finalize the transcript. If no speech is ever detected it
    /// still finalizes after `no_speech_finalize` — for a follow-up turn that is the
    /// listen window ("wait 10 s after a question / 5 s otherwise, then sleep"), the
    /// only place that can tell silence from a user who is mid-sentence (the device
    /// runs no VAD).
    pub(crate) async fn stream_to_transcript(
        &self,
        ctx: &mut TurnCtx<'_>,
        stt: &mut dyn Transcriber,
        gate: &mut dyn SpeechGate,
        no_speech_finalize: Duration,
        mic_rate: u32,
    ) -> Result<Listen> {
        let end_silence = Duration::from_millis(ctx.runtime.end_silence_ms);
        let turn_start = Instant::now();
        let mut last_voice = turn_start;
        let mut speech_started = false;
        // Consecutive voiced audio accumulated since the last non-voiced chunk, used to
        // debounce the speech onset (see `MIN_SPEECH_ONSET`). Reset by any non-voiced
        // chunk so only *sustained* voice latches `speech_started`.
        let mut voiced_run = Duration::ZERO;
        // True once we've sent `audio-stop` to STT and are just awaiting the result.
        let mut finalized = false;
        // When end-of-speech was detected (audio-stop sent). Used to measure the STT
        // finalize latency (end-of-speech → transcript) for the per-turn timing record.
        let mut finalized_at: Option<Instant> = None;
        // Accumulated voiced PCM (samples from chunks the gate marked as speech), used
        // for the speaker embedding once the transcript arrives.
        let mut voiced_pcm: Vec<i16> = Vec::new();

        let mut deadline = Instant::now() + self.turn_timeout;
        loop {
            tokio::select! {
                biased;

                _ = sleep_until(deadline) => {
                    log::warn!("turn idle-timed-out waiting for STT transcript");
                    return Ok(Listen::IdleTimeout);
                }

                // Only read the device *before* we've finalized STT. `device.read()`
                // (read_line + read_exact) is NOT cancellation-safe, so if it were
                // still enabled once the STT transcript can arrive, the `sev` arm
                // winning the race would drop this future mid-PCM-payload and desync
                // the reader — the next `read_event` (barge-in watcher / next turn)
                // then reads binary as a header line ("stream did not contain valid
                // UTF-8"), spuriously aborting the reply. After `finalized` we ignore
                // device chunks anyway, so simply stop reading them: nothing is ever
                // mid-frame when the transcript lands. Leftover buffered chunks are
                // complete frames, harmlessly drained later.
                dev = ctx.device.read(), if !finalized => {
                    deadline = Instant::now() + self.turn_timeout;
                    match dev? {
                        Some(ev) if ev.event_type == types::AUDIO_CHUNK => {
                            let Some(chunk) = ev.payload else { continue };
                            // Capture the near-end mic for the debug corpus (the whole
                            // listening window, incl. pre/post-speech).
                            if let Some(d) = ctx.dump.as_ref() {
                                d.push_mic(&chunk, mic_rate);
                            }
                            let now = Instant::now();
                            let voiced = gate.push(&chunk, mic_rate);
                            if voiced {
                                last_voice = now;
                                // Keep the voiced samples for speaker ID.
                                voiced_pcm.extend(pcm::iter_i16_le(&chunk));
                            }
                            // Debounce the onset: only latch `speech_started` after
                            // MIN_SPEECH_ONSET of *consecutive* voiced audio, so a lone
                            // transient (TTS tail, echo, noise) can't collapse the
                            // no-speech window into the short end-silence one.
                            if !speech_started {
                                let prev_run = voiced_run;
                                let latched;
                                (voiced_run, latched) = voiced_onset_step(
                                    voiced,
                                    pcm::chunk_duration(chunk.len(), mic_rate),
                                    voiced_run,
                                );
                                if latched {
                                    speech_started = true;
                                    log::debug!("VAD: speech started");
                                } else if !voiced && prev_run > Duration::ZERO {
                                    log::debug!(
                                        "VAD: discarded {:?} voiced transient below \
                                         {:?} onset; no-speech window still open",
                                        prev_run, MIN_SPEECH_ONSET
                                    );
                                }
                            }
                            stt.forward_pcm(chunk).await?;

                            let ended = if speech_started {
                                now.duration_since(last_voice) >= end_silence
                            } else {
                                now.duration_since(turn_start) >= no_speech_finalize
                            };
                            if ended {
                                log::info!(
                                    "VAD: end-of-speech (speech_started={speech_started}, \
                                     prob={:.2}); finalizing STT",
                                    gate.prob()
                                );
                                stt.finish().await?;
                                finalized = true;
                                finalized_at = Some(Instant::now());
                            }
                        }
                        // The device sends `audio-stop` only after it sees the
                        // transcript; before then, ignore other frames.
                        Some(_) => {}
                        None => return Ok(Listen::DeviceClosed),
                    }
                }

                sev = stt.read_event() => {
                    deadline = Instant::now() + self.turn_timeout;
                    match sev? {
                        Some(SttEvent::Transcript(heard)) => {
                            let crate::stt::Transcript { text, lang: stt_lang } = heard;
                            log::info!(
                                "STT transcript {text:?} (speech_started={speech_started}{})",
                                stt_lang
                                    .as_deref()
                                    .map(|l| format!(", lang={l}"))
                                    .unwrap_or_default()
                            );

                            // Guard against STT hallucinations on silence. If the VAD
                            // never latched `speech_started`, this turn finalized on the
                            // no-speech timeout: everything we forwarded was silence/room
                            // noise. faster-whisper does NOT return an empty string on
                            // non-speech — it emits short training-set filler ("Thank
                            // you.", "I'm sorry.") — so any text here is not a real
                            // utterance, especially on a follow-up window (mic reopened
                            // with no wake word). Collapse it to an empty transcript,
                            // which the caller treats as "no speech": sleep, send
                            // `audio-stop`, and end the follow-up chain (no
                            // `anamanti-listen`). A discarded hallucination is NEVER
                            // relayed to the device. The trade-off is that a genuine
                            // utterance too quiet to clear the gate for
                            // `MIN_SPEECH_ONSET` is also dropped — consistent with the
                            // no-speech finalize, which treats too-quiet audio as silence.
                            if !speech_started {
                                if !text.trim().is_empty() {
                                    log::info!(
                                        "VAD: discarding STT transcript {text:?} from a \
                                         no-speech finalize (no speech detected; likely \
                                         Whisper hallucination on silence)"
                                    );
                                }
                                return Ok(Listen::Heard {
                                    transcript: String::new(),
                                    stt_lang: None,
                                    voiced_pcm: Vec::new(),
                                    stt_latency: Duration::ZERO,
                                });
                            }

                            // Apply the live STT pronunciation rewrite table (Mechanism B,
                            // plans/stt-vocabulary-biasing.md) to the real utterance now —
                            // before the device echo, the returned transcript, and every
                            // downstream consumer (speaker ID, memory, LLM, chat log) — so
                            // there is a single corrected source of truth. An empty table
                            // leaves the text unchanged.
                            let text = crate::stt::normalize(&text, &ctx.runtime.pronunciations);

                            // Diagnostic echo: relay exactly what STT produced to the
                            // device the instant it clears the VAD gate — BEFORE System-1
                            // or System-2 — so the device confirms STT is alive regardless
                            // of any downstream gating/decision. This is the only place
                            // the transcript is emitted; the caller never re-sends it.
                            ctx.device.send(&WyomingEvent::transcript(&text)).await.ok();
                            return Ok(Listen::Heard {
                                transcript: text,
                                stt_lang,
                                voiced_pcm: std::mem::take(&mut voiced_pcm),
                                stt_latency: finalized_at.map(|t| t.elapsed()).unwrap_or_default(),
                            });
                        }
                        Some(SttEvent::Other) => {} // voice-started / voice-stopped etc.
                        None => anyhow::bail!("STT service closed before returning a transcript"),
                    }
                }
            }
        }
    }
}

/// One step of the speech-onset debounce. Folds the current chunk into the running
/// span of *consecutive* voiced audio and reports whether that span has now reached
/// [`MIN_SPEECH_ONSET`] (i.e. `speech_started` should latch). A non-voiced chunk
/// resets the run to zero, so only sustained voice — not a lone transient like the
/// residual TTS tail after a follow-up mic reopens — flips the turn out of its
/// no-speech window. Pure (takes no clock) so the debounce is unit-testable.
///
/// Returns `(updated_run, latched_now)`. Callers only invoke this while
/// `speech_started` is still false; once latched it stays latched.
pub(crate) fn voiced_onset_step(
    voiced: bool,
    chunk_dur: Duration,
    voiced_run: Duration,
) -> (Duration, bool) {
    if !voiced {
        return (Duration::ZERO, false);
    }
    let run = voiced_run.saturating_add(chunk_dur);
    (run, run >= MIN_SPEECH_ONSET)
}

#[cfg(test)]
mod tests {
    use super::{voiced_onset_step, MIN_SPEECH_ONSET};
    use std::time::Duration;

    #[test]
    fn onset_needs_sustained_voice_to_latch() {
        // Feed 20 ms voiced chunks; the onset must not latch until the accumulated
        // run reaches MIN_SPEECH_ONSET.
        let chunk = Duration::from_millis(20);
        let mut run = Duration::ZERO;
        let mut latched = false;
        let mut chunks = 0;
        while !latched {
            (run, latched) = voiced_onset_step(true, chunk, run);
            chunks += 1;
            assert!(chunks < 1000, "onset never latched");
        }
        // 250 ms / 20 ms = 13 chunks (12 chunks = 240 ms is still below threshold).
        assert_eq!(chunks, (MIN_SPEECH_ONSET.as_millis() as u64).div_ceil(20));
        assert!(run >= MIN_SPEECH_ONSET);
    }

    #[test]
    fn lone_transient_does_not_latch_and_resets() {
        // The production failure: one short voiced blip (the residual TTS tail after a
        // follow-up mic reopens) followed by silence. It must never latch speech, and
        // the run must reset so the no-speech window stays open.
        let (run, latched) = voiced_onset_step(true, Duration::from_millis(20), Duration::ZERO);
        assert!(!latched, "a single 20 ms blip must not count as speech");
        assert!(run < MIN_SPEECH_ONSET);

        // The following non-voiced chunk clears the run entirely.
        let (run, latched) = voiced_onset_step(false, Duration::from_millis(20), run);
        assert!(!latched);
        assert_eq!(run, Duration::ZERO, "non-voiced chunk resets the onset run");
    }

    #[test]
    fn gap_before_threshold_resets_progress() {
        // Voiced runs that are broken by silence before reaching the threshold must
        // restart from zero, so intermittent transients can't accumulate into a latch.
        let chunk = Duration::from_millis(100);
        let (run, latched) = voiced_onset_step(true, chunk, Duration::ZERO); // 100 ms
        assert!(!latched);
        let (run, _) = voiced_onset_step(false, chunk, run); // reset
        assert_eq!(run, Duration::ZERO);
        let (run, latched) = voiced_onset_step(true, chunk, run); // 100 ms again, not 200
        assert!(!latched);
        assert_eq!(run, Duration::from_millis(100));
    }
}
