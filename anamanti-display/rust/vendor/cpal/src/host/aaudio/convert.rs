//! Time-conversion helpers for the AAudio backend.

extern crate ndk;

use crate::{Error, ErrorKind, StreamInstant};

/// Returns a [`StreamInstant`] for the current moment.
pub fn now_stream_instant() -> StreamInstant {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let res = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    assert_eq!(res, 0, "clock_gettime(CLOCK_MONOTONIC) failed");
    StreamInstant::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Projects a hardware timestamp anchor to the instant of a specific frame position.
///
/// ANAMANTI PATCH: unused while the two `*_stream_instant` helpers below avoid the
/// native `AAudioStream_getTimestamp` call (see note there). Kept (not deleted) to
/// minimise the diff against upstream cpal 0.18.2 and ease a future rebase.
#[allow(dead_code)]
fn stream_instant_from_anchor(
    anchor_frame: i64,
    anchor_nanos: i64,
    app_frame: i64,
    sample_rate: u32,
) -> StreamInstant {
    let offset_nanos =
        (app_frame as i128 - anchor_frame as i128) * 1_000_000_000 / sample_rate as i128;
    StreamInstant::from_nanos((anchor_nanos as i128 + offset_nanos).max(0) as u64)
}

/// Returns the [`StreamInstant`] for when the first frame of the current output callback will
/// be presented at the DAC.
///
/// ANAMANTI PATCH (crash fix): upstream cpal calls `stream.timestamp()` →
/// `AAudioStream_getTimestamp` on EVERY output callback. On the Echo Show 8 (gen1,
/// Android 11, no AAudio MMAP) the stream is serviced by the legacy AudioTrack path,
/// whose `aaudio::AudioStreamLegacy::getBestTimestamp()` hits a `LOG_ALWAYS_FATAL`
/// `abort()` during start/underrun/flush timestamp races (google/oboe#1489). That is a
/// native SIGABRT inside `libaaudio_internal.so` — it crashes the whole app before cpal
/// can see the `Err`, so the `match` fallback never helps. The app does not query this
/// timestamp (both our capture + playback callbacks discard `*CallbackInfo`), so we drop
/// the hardware query entirely and return the monotonic callback instant. This removes
/// the only call site of the aborting OS function. Revisit only if we ever need true
/// DAC-presentation timestamps AND the device gains a non-legacy (MMAP) audio path.
pub fn output_stream_instant(stream: &ndk::audio::AudioStream, sample_rate: u32) -> StreamInstant {
    let _ = (stream, sample_rate);
    now_stream_instant()
}

/// Returns the [`StreamInstant`] for when the first frame of the current input callback was
/// captured at the ADC.
///
/// ANAMANTI PATCH (crash fix): see `output_stream_instant`. The capture (AudioRecord)
/// callback thread reaches the same `getBestTimestamp` abort via `getTimestamp`, so we
/// likewise skip the native query and return the monotonic callback instant.
pub fn input_stream_instant(stream: &ndk::audio::AudioStream, sample_rate: u32) -> StreamInstant {
    let _ = (stream, sample_rate);
    now_stream_instant()
}

impl From<ndk::audio::AudioError> for Error {
    fn from(error: ndk::audio::AudioError) -> Self {
        use ndk::audio::AudioError::*;
        match error {
            Disconnected | Unavailable | NoService | InvalidHandle => {
                Error::with_message(ErrorKind::DeviceNotAvailable, error.to_string())
            }
            NoFreeHandles | NoMemory => {
                Error::with_message(ErrorKind::ResourceExhausted, error.to_string())
            }
            WouldBlock | Timeout => Error::with_message(ErrorKind::DeviceBusy, error.to_string()),
            InvalidFormat | InvalidRate => {
                Error::with_message(ErrorKind::UnsupportedConfig, error.to_string())
            }
            IllegalArgument | Null | OutOfRange => {
                Error::with_message(ErrorKind::InvalidInput, error.to_string())
            }
            Internal | InvalidState => {
                Error::with_message(ErrorKind::StreamInvalidated, error.to_string())
            }
            Unimplemented => {
                Error::with_message(ErrorKind::UnsupportedOperation, error.to_string())
            }
            _ => Error::with_message(ErrorKind::BackendError, error.to_string()),
        }
    }
}
