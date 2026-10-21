//! Everything that goes *back* to the device during a turn: Piper synthesis relayed
//! as one coalesced audio stream, the sentence segmenter, device-action frames, the
//! follow-up `anamanti-listen` + closing `audio-stop`, the barge-in watcher, and the
//! shared fast-reply path used by memory commands and System-1.

use std::future::Future;

use anyhow::Result;
use tokio::time::Instant;

use crate::audio_dump::TurnAudioDump;
use crate::config::FollowUpConfig;
use crate::llm::{DeviceAction, FontDirection, MusicNav, RecipeNav};
use crate::settings::{RuntimeSettings, SharedSettings};
use crate::wyoming::protocol::{self, types, AudioFormat, WyomingEvent};
use crate::wyoming::tts::TtsSession;
use crate::wyoming::{DynRead, DynWrite};

use super::{Pipeline, ServiceConnector, TurnCtx, TurnEvent};

/// The longest run of text (in characters) we let accumulate without terminal
/// punctuation before flushing it to TTS anyway. Bounds time-to-first-audio on a
/// long unpunctuated clause.
pub(crate) const TTS_MAX_CHUNK_CHARS: usize = 240;

/// How a reply's output phase ended, as seen from the device socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// Everything was sent.
    Completed,
    /// The device sent `anamanti-interrupt` (barge-in); the reply was abandoned.
    Interrupted,
    /// The device closed the socket (or a write to it failed) mid-reply.
    DeviceClosed,
}

/// A fully-formed fast-path reply (memory command or System-1 intent): an optional
/// device action to apply first, the spoken text, and whether to end the follow-up
/// chain (`end_session`: close on a bare `audio-stop`, no `anamanti-listen`).
#[derive(Debug, Clone)]
pub(crate) struct FastReply {
    pub action: Option<DeviceAction>,
    pub text: String,
    pub end_session: bool,
}

impl FastReply {
    /// A spoken reply that invites a follow-up like any other.
    pub fn say(text: impl Into<String>) -> Self {
        Self {
            action: None,
            text: text.into(),
            end_session: false,
        }
    }

    /// Apply `action` on the device before speaking.
    pub fn with_action(mut self, action: DeviceAction) -> Self {
        self.action = Some(action);
        self
    }

    /// End the follow-up chain after speaking.
    pub fn ending_session(mut self) -> Self {
        self.end_session = true;
        self
    }
}

/// What the barge-in watcher saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BargeIn {
    /// An `anamanti-interrupt` frame.
    Interrupt,
    /// The device closed the socket (or the read failed).
    Closed,
}

/// Watch the device→orchestrator half of the socket for a **barge-in** while the
/// assistant is replying. Resolves when the device sends an `anamanti-interrupt` frame
/// or closes the socket; any other stray frame during playback (e.g. the device's own
/// `audio-stop` from the STREAMING→SPEAKING edge) is consumed and ignored so it can't
/// be mistaken for a barge-in.
///
/// This is a single long-lived future (never re-created inside a `select!` arm), so
/// it is cancellation-safe: whenever it is dropped it is parked on a fresh
/// `read_event` with no partially-consumed frame.
pub(crate) async fn watch_for_barge_in(reader: &mut DynRead) -> BargeIn {
    loop {
        match protocol::read_event(reader).await {
            Ok(Some(ev)) if ev.is_interrupt() => return BargeIn::Interrupt,
            Ok(Some(_)) => continue, // stray frame during playback — ignore
            Ok(None) => return BargeIn::Closed,
            Err(e) => {
                log::debug!("barge-in watcher read error (treating as disconnect): {e:#}");
                return BargeIn::Closed;
            }
        }
    }
}

/// Race an output future (`drive`, which writes to the device's writer half) against
/// the barge-in watcher on its reader half. Returns `Err(barge_in)` when the user
/// interrupted (or the device dropped) first — `drive` is then dropped, which cancels
/// any in-flight LLM request and Piper synthesis — else `Ok(drive's output)`.
pub(crate) async fn with_barge_in<T>(
    reader: &mut DynRead,
    drive: impl Future<Output = T>,
) -> std::result::Result<T, BargeIn> {
    tokio::pin!(drive);
    let watch = watch_for_barge_in(reader);
    tokio::pin!(watch);
    tokio::select! {
        biased;
        seen = &mut watch => Err(seen),
        out = &mut drive => Ok(out),
    }
}

/// The device frame for a [`DeviceAction`], or `None` for a Core-side action
/// ([`DeviceAction::SetPersonality`]) that has no frame. The **single** mapping used by
/// both the LLM tools' action channel and the System-1 fast paths, so the two can never
/// drive the display differently.
pub(crate) fn device_action_event(action: &DeviceAction) -> Option<WyomingEvent> {
    Some(match action {
        DeviceAction::SetPersonality { .. } => return None,
        DeviceAction::StartTimer {
            label,
            duration_secs,
        } => WyomingEvent::timer_start(*duration_secs, label.as_deref()),
        DeviceAction::CancelTimer { label } => WyomingEvent::timer_cancel(label.as_deref()),
        DeviceAction::AdjustTimer { label, delta_secs } => {
            WyomingEvent::timer_adjust(*delta_secs, label.as_deref())
        }
        DeviceAction::ShowRecipe(recipe) => {
            WyomingEvent::recipe(serde_json::to_value(recipe).unwrap_or(serde_json::Value::Null))
        }
        DeviceAction::DismissRecipe => WyomingEvent::recipe_dismiss(),
        DeviceAction::ShowWeather(report) => WyomingEvent::weather_show(
            serde_json::to_value(report).unwrap_or(serde_json::Value::Null),
        ),
        DeviceAction::DismissWeather => WyomingEvent::weather_dismiss(),
        DeviceAction::ShowPlace(report) => WyomingEvent::place_show(
            serde_json::to_value(report).unwrap_or(serde_json::Value::Null),
        ),
        DeviceAction::DismissPlace => WyomingEvent::place_dismiss(),
        DeviceAction::RecipeControl(nav) => match nav {
            RecipeNav::TabOverview => WyomingEvent::recipe_navigate("overview"),
            RecipeNav::TabIngredients => WyomingEvent::recipe_navigate("ingredients"),
            RecipeNav::TabSteps => WyomingEvent::recipe_navigate("steps"),
            RecipeNav::ScrollUp => WyomingEvent::recipe_scroll("up"),
            RecipeNav::ScrollDown => WyomingEvent::recipe_scroll("down"),
            RecipeNav::ScrollTop => WyomingEvent::recipe_scroll("top"),
            RecipeNav::ScrollBottom => WyomingEvent::recipe_scroll("bottom"),
        },
        DeviceAction::MusicScreen(nav) => WyomingEvent::music_screen(match nav {
            MusicNav::NowPlaying => "now_playing",
            MusicNav::Queue => "up_next",
            MusicNav::Close => "hidden",
        }),
        DeviceAction::MusicDisplayStyle(style) => WyomingEvent::music_screen(style.as_str()),
        DeviceAction::AdjustFont(dir) => WyomingEvent::font_adjust(match dir {
            FontDirection::Increase => "increase",
            FontDirection::Decrease => "decrease",
        }),
    })
}

/// Apply one [`DeviceAction`]: a Core-side personality change goes to the live
/// settings (it takes effect on the next turn's snapshot); everything else is written
/// to the device as its frame. A write failure is returned for the caller to log —
/// the device dropping mid-turn is handled by the surrounding turn logic.
pub(crate) async fn apply_device_action<W>(
    action: &DeviceAction,
    writer: &mut W,
    settings: &SharedSettings,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    if let DeviceAction::SetPersonality { name } = action {
        match settings.set_active_personality(name) {
            Ok(label) => log::info!(
                "personality set to {}",
                label.as_deref().unwrap_or("normal (off)")
            ),
            Err(e) => log::warn!("set_personality ignored: {e}"),
        }
        return Ok(());
    }
    if let DeviceAction::MusicDisplayStyle(style) = action {
        // Persist first so the next periodic push to every display agrees with the
        // frame this display is about to receive.
        settings.set_music_display_style(*style);
        log::info!("music display style set to {}", style.as_str());
    }
    if let Some(event) = device_action_event(action) {
        protocol::write_event(writer, &event).await?;
    }
    Ok(())
}

/// Drain every currently-pending [`DeviceAction`] from the per-turn channel and apply
/// it ([`apply_device_action`]). Non-blocking: it only takes actions already queued (a
/// tool's `invoke` runs during the LLM turn, so its action is enqueued before we get
/// here). Stops at the first write failure.
pub(crate) async fn drain_device_actions<W>(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<DeviceAction>,
    writer: &mut W,
    settings: &SharedSettings,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Ok(action) = rx.try_recv() {
        if let Err(e) = apply_device_action(&action, writer, settings).await {
            log::warn!("failed to relay device action to the device: {e:#}");
            break;
        }
    }
}

/// Close out a spoken turn: send the follow-up `anamanti-listen` frame (when enabled,
/// audio was produced, and the chain cap allows it) **before** the final `audio-stop`
/// (the device ends its turn on the first stop). Shared by the normal reply path and the
/// fast path so both invite follow-ups identically.
pub(crate) async fn emit_follow_up_and_stop<W>(
    writer: &mut W,
    follow_up: &FollowUpConfig,
    audio_started: bool,
    reply: &str,
    followup_depth: u32,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    let within_cap = follow_up.max_chain == 0 || followup_depth < follow_up.max_chain;
    if follow_up.enabled && audio_started && within_cap {
        let next_depth = followup_depth + 1;
        let wait_secs = if reply_is_question(reply) {
            follow_up.question_wait_secs
        } else {
            follow_up.reply_wait_secs
        };
        log::info!("follow-up: asking device to listen for {wait_secs}s (depth {next_depth})");
        protocol::write_event(writer, &WyomingEvent::listen(next_depth, wait_secs))
            .await
            .ok();
    }
    // Close the single coalesced device-facing audio stream.
    if audio_started {
        protocol::write_event(writer, &WyomingEvent::audio_stop(0))
            .await
            .ok();
    }
}

/// Whether a fully-assembled reply should trigger a follow-up listen: its last
/// meaningful character is a question mark. Trailing whitespace and a closing quote
/// or bracket are ignored (so `... right?"` still counts). A deliberately simple v1
/// heuristic — see plans/Plan.MD (Follow-up listening) for the rationale and known
/// misfires (e.g. rhetorical questions).
pub(crate) fn reply_is_question(reply: &str) -> bool {
    let trimmed = reply.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | '}' | '”' | '’' | '»')
    });
    trimmed.ends_with('?') || trimmed.ends_with('？')
}

/// Pull the next speakable chunk out of `pending`, draining it from the buffer.
///
/// A chunk is a complete sentence — text up to and including a `.`/`!`/`?` or a
/// newline — or, to bound latency on a long clause that never terminates, the
/// leading [`TTS_MAX_CHUNK_CHARS`] broken at the last whitespace. When `flush` is
/// set (end of the token stream) the entire remaining buffer is returned. Returns
/// `None` when there is nothing complete to speak yet.
pub(crate) fn take_speakable(pending: &mut String, flush: bool) -> Option<String> {
    if flush {
        let rest = pending.trim().to_string();
        pending.clear();
        return if rest.is_empty() { None } else { Some(rest) };
    }

    // End of the first sentence, if one has arrived.
    let boundary = pending
        .char_indices()
        .find(|(_, ch)| matches!(ch, '.' | '!' | '?' | '\n'))
        .map(|(i, ch)| i + ch.len_utf8());

    let cut = match boundary {
        Some(b) => b,
        None => {
            if pending.chars().count() < TTS_MAX_CHUNK_CHARS {
                return None;
            }
            // Overlong unterminated clause: break at the last space/newline within
            // the cap (both are single-byte, so `+ 1` stays on a char boundary).
            let cap = pending
                .char_indices()
                .nth(TTS_MAX_CHUNK_CHARS)
                .map(|(i, _)| i)
                .unwrap_or(pending.len());
            pending[..cap]
                .rfind([' ', '\n'])
                .map(|w| w + 1)
                .unwrap_or(cap)
        }
    };

    let chunk: String = pending.drain(..cut).collect();
    let chunk = chunk.trim().to_string();
    if chunk.is_empty() {
        // The drained span was only whitespace/punctuation — try the next boundary.
        return take_speakable(pending, false);
    }
    Some(chunk)
}

/// Strip Markdown decoration characters an LLM may emit (`* _ ` # ~`) so Piper does
/// not read them aloud (e.g. "asterisk asterisk"). Kept deliberately light: it only
/// removes standalone decoration glyphs, leaving words and punctuation intact.
pub(crate) fn sanitize_for_tts(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '*' | '`' | '#' | '_' | '~'))
        .collect()
}

impl Pipeline {
    /// Synthesize one chunk of reply text with Piper and relay its audio frames to
    /// the device over the same socket. A fresh downstream Piper connection is used
    /// per chunk so this works with any Wyoming TTS server (whether or not it keeps
    /// a connection open across `synthesize` requests).
    ///
    /// Returns `Ok(false)` if the device closed mid-relay — treated as a graceful
    /// stop (e.g. a barge-in that dropped the socket), not a turn failure — and
    /// `Ok(true)` otherwise. An empty/whitespace chunk is a no-op that returns
    /// `Ok(true)`. A stalled Piper surfaces as a [`TimedOut`](crate::wyoming::TimedOut)
    /// error (connect or per-read budget).
    pub(crate) async fn speak_chunk(
        &self,
        writer: &mut DynWrite,
        runtime: &RuntimeSettings,
        connector: &dyn ServiceConnector,
        text: &str,
        audio_started: &mut bool,
        dump: Option<&TurnAudioDump>,
    ) -> Result<bool> {
        let text = sanitize_for_tts(text);
        if text.trim().is_empty() {
            return Ok(true);
        }
        // The voice's language picks its Piper server (one per language keeps each voice
        // resident — wyoming-piper holds a single voice in memory). Single-language
        // installs have no per-language servers, so this is `connect_tts`.
        let voice_lang = if runtime.language.is_bilingual() {
            runtime
                .tts_voice
                .as_deref()
                .and_then(|v| crate::lang::Lang::ALL.into_iter().find(|l| l.owns_voice(v)))
        } else {
            None
        };
        let tts_conn = connector.connect_tts_for(voice_lang).await?;
        let mut tts = TtsSession::begin(tts_conn, &text, runtime.tts_voice.as_deref()).await?;

        // Piper announces its rate in each `audio-start`; track it so dumped TTS
        // (the far-end reference) is tagged with the right sample rate.
        let mut tts_rate = AudioFormat::default().rate;
        while let Some(ev) = tts.next_audio().await? {
            if ev.event_type == types::AUDIO_START {
                if let Some(fmt) = protocol::audio_format(&ev.data) {
                    tts_rate = fmt.rate;
                }
            }
            if ev.event_type == types::AUDIO_CHUNK {
                if let (Some(d), Some(pcm)) = (dump, ev.payload.as_deref()) {
                    d.push_tts(pcm, tts_rate);
                }
            }
            // Coalesce this sentence's Piper stream into the turn's single
            // device-facing stream: forward exactly one `audio-start`, relay every
            // `audio-chunk`, and swallow the per-sentence `audio-stop` (the turn
            // sends one final `audio-stop`). The device ends its turn on the first
            // `audio-stop` it sees, so a per-sentence stop would truncate the reply.
            let forward = match ev.event_type.as_str() {
                types::AUDIO_START => {
                    if *audio_started {
                        false
                    } else {
                        *audio_started = true;
                        true
                    }
                }
                types::AUDIO_STOP => false,
                _ => true,
            };
            if forward && protocol::write_event(writer, &ev).await.is_err() {
                log::info!("device closed before TTS playback finished; stopping relay");
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Commit a fully-formed fast-path reply and close the turn: apply the reply's
    /// device action first (a timer, a dismiss, the weather widget — through the same
    /// [`device_action_event`] mapping the LLM tools use), relay the reply token,
    /// synthesize it with Piper, then close. With `end_session` the turn ends on a bare
    /// `audio-stop` (no follow-up `listen`), so the device returns to IDLE / wake-word;
    /// otherwise the follow-up listen window is emitted like any other reply.
    ///
    /// Raced against the barge-in watcher, so the user can interrupt a fast reply just
    /// like a System-2 one. A TTS failure (including a stall) is logged and the turn
    /// still closes with the frames the device expects (the text was already shown).
    pub(crate) async fn speak_fast_reply(
        &self,
        ctx: &mut TurnCtx<'_>,
        reply: &FastReply,
    ) -> Delivery {
        let (reader, writer) = ctx.device.split_mut();
        let on_event = &mut *ctx.on_event;
        let runtime = &ctx.runtime;
        let connector = ctx.connector;
        let dump = ctx.dump.as_ref();
        let followup_depth = ctx.followup_depth;
        let mut tts_ms = None;

        let drive = async {
            if let Some(action) = &reply.action {
                if apply_device_action(action, writer, &self.settings)
                    .await
                    .is_err()
                {
                    return Delivery::DeviceClosed;
                }
            }
            on_event(TurnEvent::ReplyToken(reply.text.clone()));
            protocol::write_event(writer, &WyomingEvent::reply_token(&reply.text))
                .await
                .ok();
            on_event(TurnEvent::Speaking);
            let mut audio_started = false;
            let tts_start = Instant::now();
            let spoke = self
                .speak_chunk(
                    writer,
                    runtime,
                    connector,
                    &reply.text,
                    &mut audio_started,
                    dump,
                )
                .await;
            tts_ms = Some(tts_start.elapsed().as_millis() as u64);
            match spoke {
                Ok(true) => {}
                Ok(false) => return Delivery::DeviceClosed,
                Err(e) => {
                    log::warn!("fast reply: TTS failed ({e:#}); text still sent");
                    if !audio_started {
                        // No audio stream to close, but the device flipped to SPEAKING
                        // on the transcript: a bare `audio-stop` returns it to idle.
                        protocol::write_event(writer, &WyomingEvent::audio_stop(0))
                            .await
                            .ok();
                        return Delivery::Completed;
                    }
                }
            }
            if reply.end_session {
                // End the follow-up chain: no `listen` frame, just close the audio
                // stream so the device leaves SPEAKING and returns to IDLE.
                if audio_started {
                    protocol::write_event(writer, &WyomingEvent::audio_stop(0))
                        .await
                        .ok();
                }
            } else {
                emit_follow_up_and_stop(
                    writer,
                    &self.follow_up,
                    audio_started,
                    &reply.text,
                    followup_depth,
                )
                .await;
            }
            Delivery::Completed
        };
        let delivery = match with_barge_in(reader, drive).await {
            Ok(delivery) => delivery,
            Err(BargeIn::Interrupt) => {
                log::info!("barge-in: aborting the fast-path reply");
                Delivery::Interrupted
            }
            Err(BargeIn::Closed) => Delivery::DeviceClosed,
        };
        ctx.timing.tts_ms = tts_ms;
        delivery
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_a_complete_sentence() {
        let mut p = String::from("Hello there");
        assert_eq!(take_speakable(&mut p, false), None, "no terminator yet");
        assert_eq!(p, "Hello there", "buffer untouched");
    }

    #[test]
    fn flushes_each_complete_sentence_in_order() {
        let mut p = String::from("First one. Second one! Third?");
        assert_eq!(take_speakable(&mut p, false).as_deref(), Some("First one."));
        assert_eq!(
            take_speakable(&mut p, false).as_deref(),
            Some("Second one!")
        );
        assert_eq!(take_speakable(&mut p, false).as_deref(), Some("Third?"));
        assert_eq!(take_speakable(&mut p, false), None);
        assert!(p.is_empty());
    }

    #[test]
    fn splits_on_newlines_too() {
        let mut p = String::from("Line one\nleftover");
        assert_eq!(take_speakable(&mut p, false).as_deref(), Some("Line one"));
        assert_eq!(take_speakable(&mut p, false), None);
        assert_eq!(p, "leftover");
    }

    #[test]
    fn flush_returns_trailing_clause() {
        let mut p = String::from("no terminator here");
        assert_eq!(take_speakable(&mut p, false), None);
        assert_eq!(
            take_speakable(&mut p, true).as_deref(),
            Some("no terminator here")
        );
        assert_eq!(take_speakable(&mut p, true), None, "empty flush is None");
    }

    #[test]
    fn overlong_unterminated_clause_is_broken_at_whitespace() {
        // A long run with no sentence terminator flushes near the cap at a space
        // boundary, never mid-word, so time-to-first-audio stays bounded.
        let word = "word ";
        let mut p = word.repeat(TTS_MAX_CHUNK_CHARS); // far longer than the cap
        let chunk = take_speakable(&mut p, false).expect("caps long clause");
        assert!(chunk.chars().count() <= TTS_MAX_CHUNK_CHARS);
        assert!(chunk.starts_with("word"));
        assert!(!chunk.ends_with("wor"), "must not split mid-word");
    }

    #[test]
    fn sanitize_strips_markdown_decoration() {
        assert_eq!(
            sanitize_for_tts("**bold** and _em_ and `code`"),
            "bold and em and code"
        );
        assert_eq!(sanitize_for_tts("# Heading"), " Heading");
        assert_eq!(sanitize_for_tts("plain text, ok."), "plain text, ok.");
    }

    #[test]
    fn fast_path_actions_map_to_the_same_frames_as_before() {
        // The fast paths used to write these frames directly; routing them through the
        // shared DeviceAction mapping must leave the wire identical.
        assert_eq!(
            device_action_event(&DeviceAction::StartTimer {
                label: None,
                duration_secs: 600
            }),
            Some(WyomingEvent::timer_start(600, None))
        );
        assert_eq!(
            device_action_event(&DeviceAction::CancelTimer { label: None }),
            Some(WyomingEvent::timer_cancel(None))
        );
        assert_eq!(
            device_action_event(&DeviceAction::DismissWeather),
            Some(WyomingEvent::weather_dismiss())
        );
        assert_eq!(
            device_action_event(&DeviceAction::DismissRecipe),
            Some(WyomingEvent::recipe_dismiss())
        );
        assert_eq!(
            device_action_event(&DeviceAction::DismissPlace),
            Some(WyomingEvent::place_dismiss())
        );
        let report = crate::weather::WeatherReport::default();
        assert_eq!(
            device_action_event(&DeviceAction::ShowWeather(report.clone())),
            Some(WyomingEvent::weather_show(
                serde_json::to_value(&report).unwrap()
            ))
        );
        assert_eq!(
            device_action_event(&DeviceAction::SetPersonality {
                name: "normal".into()
            }),
            None
        );
    }

    #[tokio::test]
    async fn music_display_style_persists_and_relays_a_screen_frame() {
        use crate::music::MusicDisplayStyle;
        let settings = crate::settings::SharedSettings::fixed(
            std::sync::Arc::new(crate::llm::mock::MockLlm::default()),
            "mock",
            None,
        );
        let action = DeviceAction::MusicDisplayStyle(MusicDisplayStyle::Fullscreen);
        assert_eq!(
            device_action_event(&action),
            Some(WyomingEvent::music_screen("fullscreen"))
        );
        let mut wire: Vec<u8> = Vec::new();
        apply_device_action(&action, &mut wire, &settings)
            .await
            .unwrap();
        assert_eq!(
            settings.music_display_style(),
            MusicDisplayStyle::Fullscreen
        );
        assert_eq!(wire, WyomingEvent::music_screen("fullscreen").to_bytes());
    }
}
