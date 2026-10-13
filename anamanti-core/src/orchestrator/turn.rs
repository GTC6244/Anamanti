//! Per-turn inputs: [`TurnStart`] (what the opening `audio-start` frame says) and
//! [`TurnCtx`] (everything a turn's stages share once it is running).

use std::time::Duration;

use crate::audio_dump::TurnAudioDump;
use crate::memory::TurnTiming;
use crate::settings::RuntimeSettings;
use crate::wyoming::protocol::{self, types, AudioFormat, DeviceContext, WyomingEvent};
use crate::wyoming::DynConnection;

use super::{ServiceConnector, TurnEvent};

/// Default no-speech window for an ordinary (wake-word) turn: if the user never
/// speaks, finalize (an empty transcript) after this long rather than hanging to the
/// turn timeout. A follow-up turn overrides this with the listen window it was given
/// (see [`TurnStart::no_speech_window`]).
pub(crate) const DEFAULT_NO_SPEECH_FINALIZE: Duration = Duration::from_secs(6);

/// What the device's opening `audio-start` frame says about the turn. The server
/// parses it with [`TurnStart::from_event`] (it reads the frame itself to tell a turn
/// from a control request) and hands it to
/// [`Pipeline::run_turn_after_start`](super::Pipeline::run_turn_after_start); tests do
/// exactly the same.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnStart {
    /// The PCM format the device will stream (16 kHz mono by default).
    pub format: AudioFormat,
    /// Follow-up chain depth: `0` for a wake-word turn, `n` for the n-th turn the
    /// device opened on its own after an `anamanti-listen`.
    pub followup_depth: u32,
    /// The listen window (seconds) the reply that triggered this follow-up chose;
    /// `0` on a wake-word turn.
    pub followup_wait_secs: u32,
    /// What the display is showing plus background state (timers, font scale).
    pub device_ctx: DeviceContext,
}

impl TurnStart {
    /// Parse an `audio-start` frame, or `None` for any other frame type.
    pub fn from_event(ev: &WyomingEvent) -> Option<Self> {
        if ev.event_type != types::AUDIO_START {
            return None;
        }
        Some(Self {
            format: protocol::audio_format(&ev.data).unwrap_or(AudioFormat::PCM_16K_MONO),
            followup_depth: protocol::followup_depth(&ev.data),
            followup_wait_secs: protocol::followup_wait_secs(&ev.data),
            device_ctx: protocol::device_context(&ev.data),
        })
    }

    /// How long to wait for the user to *start* speaking before finalizing (and, on
    /// silence, sleeping). A follow-up turn uses the window the reply that triggered it
    /// chose — 10 s after a question, 5 s otherwise; an ordinary turn keeps
    /// [`DEFAULT_NO_SPEECH_FINALIZE`].
    pub fn no_speech_window(&self) -> Duration {
        if self.followup_depth > 0 && self.followup_wait_secs > 0 {
            Duration::from_secs(self.followup_wait_secs as u64)
        } else {
            DEFAULT_NO_SPEECH_FINALIZE
        }
    }
}

/// Everything a running turn's stages share: one settings snapshot (so a concurrent
/// config swap never changes the backend/voice mid-reply — every setting a turn reads
/// comes from here, not from the live settings), the device socket, the downstream
/// connector, the device context, the event sink, the debug audio dump and the timing
/// record that ends up in the chat log.
pub(crate) struct TurnCtx<'a> {
    pub runtime: RuntimeSettings,
    pub device: &'a mut DynConnection,
    pub connector: &'a dyn ServiceConnector,
    pub device_ctx: DeviceContext,
    pub followup_depth: u32,
    pub on_event: &'a mut (dyn FnMut(TurnEvent) + Send),
    pub dump: Option<TurnAudioDump>,
    pub timing: TurnTiming,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_event_reads_an_audio_start_and_ignores_other_frames() {
        assert!(TurnStart::from_event(&WyomingEvent::audio_stop(0)).is_none());

        let plain = TurnStart::from_event(&WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0))
            .unwrap();
        assert_eq!(plain.format, AudioFormat::PCM_16K_MONO);
        assert_eq!(plain.followup_depth, 0);
        assert_eq!(plain.no_speech_window(), DEFAULT_NO_SPEECH_FINALIZE);

        let mut ev = WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0);
        ev.data["followup"] = serde_json::json!(true);
        ev.data["followup_depth"] = serde_json::json!(2);
        ev.data["wait_secs"] = serde_json::json!(10);
        let follow = TurnStart::from_event(&ev).unwrap();
        assert_eq!(follow.followup_depth, 2);
        assert_eq!(follow.no_speech_window(), Duration::from_secs(10));
    }
}
