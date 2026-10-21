//! Pluggable LLM orchestrator (Plan.MD Phase 4, bullet 2; §5 "LLM abstraction").
//!
//! Every backend implements one trait — [`LlmBackend`] — whose `respond` returns a
//! **stream of reply tokens**. The pipeline (`orchestrator.rs`) never names a
//! concrete backend: it is handed a `Box<dyn LlmBackend>` chosen from config, so
//! local (Ollama / llama.cpp) and cloud (Claude) are swappable without touching
//! the turn logic. This is the locked "pluggable behind a trait" decision.
//!
//! Two engines implement it: [`rig`] (default; every provider with the tool set
//! and follow-up history) and the native plain-text clients ([`ollama`],
//! [`anthropic`], [`openai`]). What a backend can do is explicit data —
//! [`LlmBackend::capabilities`] — so a lost capability is warned about, not silent.

pub mod anthropic;
pub mod anthropic_auth;
pub mod catalog;
pub mod mock;
pub mod ollama;
pub mod openai;
pub mod rig;

use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};

/// A streamed reply: reply-token fragments in arrival order. Fragments concatenate
/// to the full reply text; each is rendered on the device as it lands (Phase 5)
/// and the concatenation is sent to Piper for synthesis (Phase 4).
pub type ReplyStream = Pin<Box<dyn Stream<Item = Result<String>> + Send>>;

/// A **device action** a tool asks the pipeline to relay to the Echo Show (Phase 2).
/// Info tools (web search) just return text to the model; *action* tools (timers)
/// additionally emit one of these onto the per-turn [`ActionSink`], which the
/// orchestrator drains and writes to the device as an `ambient-timer` frame. The
/// device owns the resulting state (unlimited concurrent timers + the countdown UI
/// and alarm), so actions are fire-and-forget from the Mac's side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceAction {
    /// Start a timer for `duration_secs` with an optional spoken `label`.
    StartTimer {
        label: Option<String>,
        duration_secs: u64,
    },
    /// Cancel timers matching `label`, or *all* timers when `label` is `None`.
    CancelTimer { label: Option<String> },
    /// Add (`delta_secs > 0`) or remove (`delta_secs < 0`) time on timers matching
    /// `label`, or on *all* timers when `label` is `None`. The device owns the running
    /// countdown, so the Mac relays only the signed delta and the device re-anchors the
    /// remaining time (floored at 1s, capped at the 24h max).
    AdjustTimer {
        label: Option<String>,
        delta_secs: i64,
    },
    /// Show a parsed recipe on the display's "recipe mode" screen (Overview /
    /// Ingredients / Steps tabs). The device owns the resulting screen state until
    /// dismissed, so this is fire-and-forget like the timer actions.
    ShowRecipe(crate::recipe::Recipe),
    /// Dismiss the recipe screen and return to the idle/ambient display.
    DismissRecipe,
    /// Show the full-screen weather forecast on the display (today's conditions with
    /// big imagery + a 7-day row). The device owns the resulting screen state until
    /// dismissed, so this is fire-and-forget like the recipe/timer actions.
    ShowWeather(crate::weather::WeatherReport),
    /// Dismiss the weather screen and return to the idle/ambient display.
    DismissWeather,
    /// Show the full-screen place card on the display (name, address, hours, rating,
    /// phone, a photo, …). The device owns the resulting screen state until dismissed,
    /// so this is fire-and-forget like the recipe/weather/timer actions.
    ShowPlace(crate::places::PlaceReport),
    /// Dismiss the place card and return to the idle/ambient display.
    DismissPlace,
    /// Open the live camera view of the named camera (`plans/CameraPlan.md`). Carries
    /// only the display name — the device fetches the stream URL over the control
    /// socket. Fire-and-forget: the device owns the view (and its auto-close).
    ShowCamera { name: String },
    /// Close the live camera view.
    DismissCamera,
    /// Drive the already-open recipe screen by voice — switch tab or scroll a pane.
    /// Fire-and-forget: the device owns the screen state.
    RecipeControl(RecipeNav),
    /// Change the assistant's output personality (pure output tuning) by spoken name,
    /// or "normal"/"off" to disable. Unlike the other actions this is **Core-side**:
    /// the orchestrator applies it to [`SharedSettings`](crate::settings::SharedSettings)
    /// (no device frame), so the next turn's prompt carries the new style.
    SetPersonality { name: String },
    /// Step the global on-screen font scale up or down. The device owns + persists the
    /// scale, so this is fire-and-forget like the other display actions.
    AdjustFont(FontDirection),
    /// Drive the music screen by voice — open the now-playing view, open the up-next
    /// queue, or close back to the ambient display. Fire-and-forget: the device owns the
    /// screen state (and whether anything is playing). Transport (play/pause/skip/volume)
    /// is the separate `spotify_control` tool, not this.
    MusicScreen(MusicNav),
    /// Switch which music widget the speaking display shows while the current music
    /// plays — the compact bar or the full-screen now-playing view. Relayed as an
    /// `anamanti-music` `screen` frame (`"fullscreen"` / `"compact"`) so that display
    /// switches at once; the `music_screen` tool has already recorded it as that
    /// display's choice in the [`MusicDisplayRouter`](crate::music::MusicDisplayRouter)
    /// so its later now-playing pushes agree. Other displays are unaffected.
    MusicDisplayStyle(crate::music::MusicDisplayStyle),
}

/// A voice command for the music screen (relayed as an `anamanti-music` `screen` frame).
/// Emitted by the `music_screen` tool. Screen navigation only — playback transport is
/// `spotify_control`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MusicNav {
    /// Open the full-screen now-playing view.
    NowPlaying,
    /// Open the up-next queue list.
    Queue,
    /// Close any music screen and return to the compact overlay / ambient display.
    Close,
}

/// Which way to step the global font scale (relayed as an `anamanti-font` `adjust` frame).
/// Emitted by the `adjust_font` tool / the System-1 font intents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontDirection {
    /// Make the on-screen text larger.
    Increase,
    /// Make the on-screen text smaller.
    Decrease,
}

/// A voice navigation command for the open recipe screen (relayed as an
/// `anamanti-recipe` `navigate`/`scroll`/`step` frame). Emitted by the `recipe_control`
/// tool and the System-1 `recipe_step` fast path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeNav {
    /// Switch to the Overview tab.
    TabOverview,
    /// Switch to the Ingredients tab.
    TabIngredients,
    /// Switch to the Steps tab.
    TabSteps,
    /// Scroll the active pane up one page.
    ScrollUp,
    /// Scroll the active pane down one page.
    ScrollDown,
    /// Jump to the top of the active pane.
    ScrollTop,
    /// Jump to the bottom of the active pane.
    ScrollBottom,
    /// Jump to a step (1-based): switch to the Steps tab, scroll it into view and
    /// highlight it (relayed as an `anamanti-recipe` `step` frame).
    Step(u32),
}

/// The per-turn channel a tool pushes [`DeviceAction`]s onto. Unbounded so a tool's
/// synchronous `invoke` never blocks on the pipeline draining it; the pipeline
/// drains it while streaming the reply. `None` on an [`LlmTurn`] means the caller
/// cannot relay device actions (e.g. tests, or a turn with no device attached), so
/// action tools degrade to a spoken "I can't do that here".
pub type ActionSink = tokio::sync::mpsc::UnboundedSender<DeviceAction>;

/// One request to a backend: a system prompt (which carries the persistent-memory
/// context) plus the user's transcript for this turn. Ordinary turns are **single-turn**
/// (cross-session continuity comes from the memory store, not a message history); the
/// only exception is a **follow-up** turn — one the device auto-opened after a question
/// reply with no wake word — whose `history` carries the recent conversation so the
/// answer has context (see `plans/Plan.MD`, Follow-up listening).
#[derive(Debug, Clone)]
pub struct LlmTurn {
    pub system_prompt: String,
    pub user_message: String,
    /// Prior `(user, assistant)` turn pairs, oldest first, prepended to the message
    /// history for a follow-up turn. Empty for an ordinary single-shot turn. Only
    /// backends whose [`Capabilities::history`] is set replay it (the rig engine);
    /// the native plain-text backends ignore it.
    pub history: Vec<(String, String)>,
    /// Where action tools emit [`DeviceAction`]s for the pipeline to relay to the
    /// device. `None` disables device actions for this turn. Only backends whose
    /// [`Capabilities::tools`] is set read it (the rig engine).
    pub actions: Option<ActionSink>,
    /// The stable `device_id` of the display this turn came from, handed to the tools
    /// (e.g. so a play request shows full screen on the display that asked). `None`
    /// when the display doesn't identify itself.
    pub device_id: Option<String>,
}

impl LlmTurn {
    pub fn new(system_prompt: impl Into<String>, user_message: impl Into<String>) -> Self {
        Self {
            system_prompt: system_prompt.into(),
            user_message: user_message.into(),
            history: Vec::new(),
            actions: None,
            device_id: None,
        }
    }

    /// Record which display this turn came from (see [`Self::device_id`]).
    pub fn with_device_id(mut self, device_id: Option<String>) -> Self {
        self.device_id = device_id;
        self
    }

    /// Attach the per-turn device-action sink so action tools (timers) can relay to
    /// the device.
    pub fn with_actions(mut self, actions: ActionSink) -> Self {
        self.actions = Some(actions);
        self
    }

    /// Seed the conversation history replayed before the current user message (a
    /// follow-up turn). Each entry is a `(user, assistant)` pair, oldest first.
    pub fn with_history(mut self, history: Vec<(String, String)>) -> Self {
        self.history = history;
        self
    }
}

/// What a backend can do with a turn. Engine/provider choice decides these, so they
/// are reported (config page) and warned about (build time) rather than silently
/// lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Capabilities {
    /// Runs the tool set (timers, weather, device actions, …).
    pub tools: bool,
    /// Replays [`LlmTurn::history`] on follow-up turns.
    pub history: bool,
    /// Streams reply tokens as they are generated (vs. one whole reply).
    pub streaming: bool,
}

impl Capabilities {
    /// A plain streaming text backend: no tools, no follow-up history.
    pub const TEXT_ONLY: Capabilities = Capabilities {
        tools: false,
        history: false,
        streaming: true,
    };

    /// Human-readable names of what this backend lacks (empty when nothing is
    /// lost), e.g. `["tools (timers, weather, device actions)", …]`.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.tools {
            out.push("tools (timers, weather, search, device actions)");
        }
        if !self.history {
            out.push("follow-up conversation history");
        }
        if !self.streaming {
            out.push("token streaming");
        }
        out
    }
}

/// A swappable LLM backend. Implementations are `Send + Sync` so one instance is
/// shared across concurrent turns behind an `Arc`.
#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// A short backend label for logs/settings (e.g. `"ollama"`, `"anthropic"`).
    fn name(&self) -> &str;

    /// What this backend does with a turn. Defaults to [`Capabilities::TEXT_ONLY`];
    /// a backend that runs tools or replays history overrides it.
    fn capabilities(&self) -> Capabilities {
        Capabilities::TEXT_ONLY
    }

    /// Produce a streaming reply to `turn`. Errors surface either here (request
    /// setup failed) or as an `Err` item inside the stream (mid-stream failure).
    async fn respond(&self, turn: LlmTurn) -> Result<ReplyStream>;
}

/// Drain a [`ReplyStream`] into the complete reply text. Used when a caller only
/// needs the final string (e.g. TTS synthesis, or tests).
pub async fn collect_reply(mut stream: ReplyStream) -> Result<String> {
    let mut out = String::new();
    while let Some(token) = stream.next().await {
        out.push_str(&token?);
    }
    Ok(out)
}

/// How long a native backend waits for a streamed request's response headers
/// (connect + the provider starting its reply; a cold local model can be slow).
pub(crate) const RESPONSE_START_TIMEOUT: Duration = Duration::from_secs(90);

/// How long a native streamed reply may go without a single byte before it is
/// abandoned. Resets on every chunk, so a long reply is never cut.
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Send a native backend's streaming request, bounding the wait for the response
/// headers by [`RESPONSE_START_TIMEOUT`] (a whole-request timeout would cut the
/// stream). Transport errors carry no URL or secret.
pub(crate) async fn send_streaming(
    req: reqwest::RequestBuilder,
    what: &str,
) -> Result<reqwest::Response> {
    match tokio::time::timeout(RESPONSE_START_TIMEOUT, req.send()).await {
        Ok(sent) => sent.map_err(|e| crate::http::redacted(format!("{what}: request failed"), e)),
        Err(_) => anyhow::bail!(
            "{what}: no response within {}s",
            RESPONSE_START_TIMEOUT.as_secs()
        ),
    }
}

/// Turn a byte stream (an HTTP response body) into a stream of complete text lines,
/// splitting on `\n`. Both streaming LLM wire formats used here are line-oriented:
/// Ollama emits newline-delimited JSON, Claude emits SSE (`event:`/`data:` lines).
/// A trailing partial line (no terminating `\n`) is yielded at end-of-stream. A body
/// that goes silent for [`STREAM_IDLE_TIMEOUT`] ends the stream with an error.
pub(crate) fn line_stream(
    body: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
) -> impl Stream<Item = Result<String>> + Send + 'static {
    line_stream_with_idle(body, STREAM_IDLE_TIMEOUT)
}

fn line_stream_with_idle(
    body: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    idle: Duration,
) -> impl Stream<Item = Result<String>> + Send + 'static {
    async_stream::try_stream! {
        let mut buf: Vec<u8> = Vec::new();
        futures_util::pin_mut!(body);
        loop {
            let Ok(next) = tokio::time::timeout(idle, body.next()).await else {
                Err(anyhow::anyhow!("LLM stream stalled: no data for {}s", idle.as_secs()))?;
                break;
            };
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|e| crate::http::redacted("reading the LLM stream".into(), e))?;
            buf.extend_from_slice(&chunk);
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                // Drop the trailing '\n' (and a '\r' if present).
                let end = line.len().saturating_sub(1);
                let end = if end > 0 && line[end - 1] == b'\r' { end - 1 } else { end };
                yield String::from_utf8_lossy(&line[..end]).into_owned();
            }
        }
        if !buf.is_empty() {
            yield String::from_utf8_lossy(&buf).into_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn line_stream_splits_on_newlines_including_trailing_partial() {
        let chunks = vec![
            Ok(bytes::Bytes::from_static(b"one\ntw")),
            Ok(bytes::Bytes::from_static(b"o\r\nthree")),
        ];
        let body = futures_util::stream::iter(chunks);
        let lines: Vec<String> = line_stream(body)
            .map(|l| l.unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(lines, vec!["one", "two", "three"]);
    }

    #[tokio::test]
    async fn line_stream_errors_when_the_body_goes_silent() {
        let body = futures_util::stream::iter(vec![Ok(bytes::Bytes::from_static(b"one\n"))])
            .chain(futures_util::stream::pending());
        let mut lines = Box::pin(line_stream_with_idle(body, Duration::from_millis(50)));
        assert_eq!(lines.next().await.unwrap().unwrap(), "one");
        let err = lines.next().await.unwrap().unwrap_err();
        assert!(err.to_string().contains("stalled"), "{err}");
        assert!(lines.next().await.is_none());
    }

    #[tokio::test]
    async fn send_streaming_reports_refused_connections_without_the_url() {
        let base = crate::http::test_support::refused_base();
        let err = send_streaming(
            crate::http::shared_client().post(format!("{base}/api/chat?key=SECRET")),
            "POST /api/chat to Ollama",
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.starts_with("POST /api/chat to Ollama: request failed"),
            "{text}"
        );
        assert!(!text.contains("SECRET"), "{text}");
    }

    #[tokio::test]
    async fn collect_reply_concatenates_tokens() {
        let stream: ReplyStream = Box::pin(futures_util::stream::iter(vec![
            Ok("Hello".to_string()),
            Ok(", ".to_string()),
            Ok("world".to_string()),
        ]));
        assert_eq!(collect_reply(stream).await.unwrap(), "Hello, world");
    }
}
