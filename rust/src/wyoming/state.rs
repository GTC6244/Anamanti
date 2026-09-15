//! The Wyoming client's turn state machine as a *pure* transition function
//! (Plan.MD §3, Phase 3; architecture.md §4).
//!
//! ```text
//! IDLE ──wake word──► TRIGGERED ──connected──► STREAMING ──end-of-speech──► CLOSING ──closed──► IDLE
//!   ▲                     │ connect failed         │ timeout                   │
//!   └─────────────────────┴────────────────────────┴───────────────────────────┘
//! ```
//!
//! Keeping the transitions pure (no sockets, no PCM, no runtime) means the whole
//! turn lifecycle is exhaustively unit-testable in microseconds. The async
//! driver in `client.rs` owns the side effects: it feeds this machine
//! [`ControlInput`]s and executes the [`Action`]s it returns. The per-chunk audio
//! hot path does *not* go through `on_input` (which would allocate a `Vec` per
//! chunk); it consults the cheap [`Session::is_streaming`] gate instead.

/// Where a single voice turn currently is. `Silence/Timeout` from the plan is the
/// [`SessionState::Closing`] drain state below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Wake-word scoring only; the TCP socket is dormant/closed.
    Idle,
    /// Wake word fired; a connection is being opened.
    Triggered,
    /// Connected; streaming PCM up while reading `transcript` events down.
    Streaming,
    /// Server-side VAD (or a timeout) ended the turn; draining `audio-stop` and
    /// tearing the socket down before returning to [`SessionState::Idle`].
    Closing,
}

impl SessionState {
    /// True whenever a turn is in flight (anything but [`SessionState::Idle`]).
    /// The engine uses this to raise the wake-word threshold during a turn
    /// (AEC-interim mitigation, architecture.md §4).
    pub fn is_active(self) -> bool {
        !matches!(self, SessionState::Idle)
    }
}

/// Control-plane inputs that drive turn transitions. Deliberately excludes the
/// per-chunk PCM flow (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlInput {
    /// The on-device wake-word detector fired.
    WakeWord,
    /// The TCP connection to the Wyoming host is established.
    Connected,
    /// The connection attempt failed (discovery or dial error).
    ConnectFailed,
    /// Server-side end-of-speech: a `transcript` arrived or VAD signalled stop.
    EndOfSpeech,
    /// No audio/events for too long; abandon the turn defensively.
    Timeout,
    /// The socket has been fully torn down.
    Closed,
    /// External request to stop the current turn (engine shutdown / reset).
    Stop,
}

/// A side effect the async driver must perform after a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Resolve the Wyoming host (mDNS/cache) and open the TCP socket.
    OpenConnection,
    /// Send the Wyoming `audio-start` header and begin streaming PCM.
    SendAudioStart,
    /// Send the Wyoming `audio-stop` footer (end of this turn's audio).
    SendAudioStop,
    /// Drop the socket and reset connection state back to idle.
    Close,
}

/// The turn state machine. Cheap to copy; holds only the current [`SessionState`].
#[derive(Debug, Clone)]
pub struct Session {
    state: SessionState,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    /// A fresh session parked in [`SessionState::Idle`].
    pub fn new() -> Self {
        Self {
            state: SessionState::Idle,
        }
    }

    /// The current turn state.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Whether captured PCM should currently be transmitted. The audio hot path
    /// gates on this instead of calling [`Session::on_input`], so streaming a
    /// chunk never allocates.
    pub fn is_streaming(&self) -> bool {
        matches!(self.state, SessionState::Streaming)
    }

    /// Apply a control input, mutate the state, and return the ordered side
    /// effects the driver must execute. Unhandled (input, state) pairs are no-ops
    /// — the machine is total, so a stray event can never panic the engine.
    pub fn on_input(&mut self, input: ControlInput) -> Vec<Action> {
        use Action::*;
        use ControlInput::*;
        use SessionState::*;

        let (next, actions): (SessionState, Vec<Action>) = match (self.state, input) {
            // IDLE: a wake word opens a connection; everything else is ignored.
            (Idle, WakeWord) => (Triggered, vec![OpenConnection]),

            // TRIGGERED: waiting for the socket.
            (Triggered, Connected) => (Streaming, vec![SendAudioStart]),
            // A failed dial drops straight back to idle; the driver reports the
            // disconnect. No socket exists yet, so nothing to close.
            (Triggered, ConnectFailed) => (Idle, vec![]),
            (Triggered, Timeout | Stop) => (Idle, vec![Close]),

            // STREAMING: server VAD or a timeout ends the turn; we drain.
            (Streaming, EndOfSpeech | Timeout) => (Closing, vec![SendAudioStop]),
            // The peer closed on us mid-stream — no audio-stop to send.
            (Streaming, Closed) => (Idle, vec![Close]),
            (Streaming, Stop) => (Closing, vec![SendAudioStop]),

            // CLOSING: draining `audio-stop`; the socket teardown returns us home.
            (Closing, Closed | Timeout | Stop) => (Idle, vec![Close]),

            // A wake word that fires while a turn is already active is ignored in
            // Phase 3 (barge-in restart during SPEAKING is Phase 5). Any other
            // unmatched pair is a benign no-op.
            _ => (self.state, vec![]),
        };

        self.state = next;
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::Action::*;
    use super::ControlInput::*;
    use super::SessionState::*;
    use super::*;

    #[test]
    fn happy_path_idle_to_streaming_to_idle() {
        let mut s = Session::new();
        assert_eq!(s.state(), Idle);
        assert!(!s.is_streaming());

        assert_eq!(s.on_input(WakeWord), vec![OpenConnection]);
        assert_eq!(s.state(), Triggered);
        assert!(s.state().is_active());

        assert_eq!(s.on_input(Connected), vec![SendAudioStart]);
        assert_eq!(s.state(), Streaming);
        assert!(s.is_streaming());

        // Server-side VAD ends the turn → drain audio-stop, then close.
        assert_eq!(s.on_input(EndOfSpeech), vec![SendAudioStop]);
        assert_eq!(s.state(), Closing);
        assert!(!s.is_streaming());

        assert_eq!(s.on_input(Closed), vec![Close]);
        assert_eq!(s.state(), Idle);
        assert!(!s.state().is_active());
    }

    #[test]
    fn connect_failure_returns_to_idle_without_closing() {
        let mut s = Session::new();
        s.on_input(WakeWord);
        assert_eq!(s.on_input(ConnectFailed), vec![]);
        assert_eq!(s.state(), Idle);
    }

    #[test]
    fn timeout_while_streaming_still_sends_audio_stop() {
        let mut s = Session::new();
        s.on_input(WakeWord);
        s.on_input(Connected);
        assert_eq!(s.on_input(Timeout), vec![SendAudioStop]);
        assert_eq!(s.state(), Closing);
    }

    #[test]
    fn peer_close_mid_stream_resets_without_audio_stop() {
        let mut s = Session::new();
        s.on_input(WakeWord);
        s.on_input(Connected);
        assert_eq!(s.on_input(Closed), vec![Close]);
        assert_eq!(s.state(), Idle);
    }

    #[test]
    fn duplicate_wake_word_while_active_is_ignored() {
        let mut s = Session::new();
        s.on_input(WakeWord);
        s.on_input(Connected);
        assert!(s.is_streaming());
        // A second trigger mid-turn does nothing in Phase 3.
        assert_eq!(s.on_input(WakeWord), vec![]);
        assert_eq!(s.state(), Streaming);
    }

    #[test]
    fn stop_from_streaming_drains_then_closes() {
        let mut s = Session::new();
        s.on_input(WakeWord);
        s.on_input(Connected);
        assert_eq!(s.on_input(Stop), vec![SendAudioStop]);
        assert_eq!(s.state(), Closing);
        assert_eq!(s.on_input(Closed), vec![Close]);
        assert_eq!(s.state(), Idle);
    }

    #[test]
    fn stray_events_in_idle_are_noops() {
        let mut s = Session::new();
        for input in [Connected, ConnectFailed, EndOfSpeech, Timeout, Closed, Stop] {
            assert_eq!(s.on_input(input), vec![]);
            assert_eq!(s.state(), Idle);
        }
    }

    #[test]
    fn full_turn_is_repeatable() {
        let mut s = Session::new();
        for _ in 0..3 {
            assert_eq!(s.on_input(WakeWord), vec![OpenConnection]);
            assert_eq!(s.on_input(Connected), vec![SendAudioStart]);
            assert_eq!(s.on_input(EndOfSpeech), vec![SendAudioStop]);
            assert_eq!(s.on_input(Closed), vec![Close]);
            assert_eq!(s.state(), Idle);
        }
    }
}
