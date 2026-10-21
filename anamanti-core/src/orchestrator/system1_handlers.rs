//! The System-1 fast-decision fork (plans/system1-fast-decisions.md): ask the decision
//! engine, and on a confident [`Intent`] whose handler preconditions hold, answer the
//! turn here — skipping memory recall and the LLM.
//!
//! Each handler only *decides* (returns a [`FastReply`], or `None` to defer before
//! anything reaches the device, so deferring can never double-speak); the one output
//! path, [`Pipeline::speak_fast_reply`], applies the action, speaks, and closes the
//! turn under the barge-in watcher.

use std::time::Duration;

use tokio::time::Instant;

use crate::lang::{phrases, Lang};
use crate::llm::DeviceAction;
use crate::system1::{Decision, DecisionRequest, Intent, Resolution};
use crate::timer::MAX_TIMER_SECS;
use crate::wyoming::protocol::{DeviceContext, DisplayContext};

use super::speech_out::{Delivery, FastReply};
use super::{Pipeline, TurnCtx};

/// Wall-clock budget for the System-1 weather fetch on the *fast* path. The provider's
/// own client timeout bounds a single request, but a fast-path fetch can chain a
/// geocode *then* a forecast call — up to ~20 s of dead air before it gives up. That
/// defeats the point of the fast path: a slow/flaky provider should fail over to
/// System-2 promptly, not stall the user. Bound the whole fetch here; on timeout we
/// defer, exactly as we do on any other fetch error.
pub(crate) const SYSTEM1_WEATHER_BUDGET: Duration = Duration::from_millis(2500);

impl Pipeline {
    /// Run the System-1 fork for this turn. Skipped entirely for the default `none`
    /// engine. Returns `Some((reply, delivery))` when the turn was answered on the fast
    /// path, `None` to fall through to System-2 (defer, error, no handler, or a handler
    /// precondition that failed before anything was sent). Records the decision in the
    /// turn's timing.
    pub(crate) async fn run_system1(
        &self,
        ctx: &mut TurnCtx<'_>,
        transcript: &str,
        speaker: &crate::speaker::SpeakerContext,
    ) -> Option<(String, Delivery)> {
        let engine_handle = ctx.runtime.system1.engine.clone();
        let engine = engine_handle.name().to_string();
        if engine == "none" {
            return None;
        }
        let req = DecisionRequest {
            transcript: transcript.to_string(),
            // The foreground widget label ("recipe"/"weather"), so screen-relative
            // commands can route; `None` on an idle display.
            screen: ctx.device_ctx.widget_label().map(str::to_string),
            // Background timer state (running/remaining/labels), ground truth for the
            // timer_query / timer_cancel / stop_dismiss decisions (§17, §19).
            timers: ctx.device_ctx.timers.clone(),
            // Who is asking (decision D4) — resolution + household name only.
            speaker: crate::system1::SpeakerSummary {
                resolution: speaker.resolution.as_str().to_string(),
                name: speaker.reader().and(speaker.name.as_deref()).map(|n| {
                    ctx.runtime
                        .household
                        .member_matching_spoken(n)
                        .map_or_else(|| n.trim().to_string(), |m| m.name.trim().to_string())
                }),
            },
        };
        log::info!("system1 ({engine}) call: deciding on {transcript:?}");
        let t0 = Instant::now();
        let decision = engine_handle.decide(&req).await;
        let ms = t0.elapsed().as_millis();
        ctx.timing.system1_ms = Some(ms as u64);
        let r: Resolution = match decision {
            Ok(Decision::Resolve(r)) => r,
            Ok(Decision::Defer) => {
                ctx.timing.system1_outcome = Some("defer".to_string());
                log::info!("system1 ({engine}) DEFER in {ms}ms; falling through to System-2");
                return None;
            }
            Err(e) => {
                ctx.timing.system1_outcome = Some("error".to_string());
                log::warn!(
                    "system1 ({engine}) decide FAILED in {ms}ms ({e:#}); deferring to System-2"
                );
                return None;
            }
        };
        ctx.timing.system1_intent = Some(r.intent.clone());
        ctx.timing.system1_confidence = Some(r.confidence);
        log::info!(
            "system1 ({engine}) RESOLVE intent `{}` (conf {:.2}) in {ms}ms",
            r.intent,
            r.confidence,
        );
        let lang = ctx.lang.lang;
        let reply = match r.intent.parse::<Intent>() {
            // A turn in a language this intent isn't validated for defers to System-2,
            // which answers in the spoken language (plans/BilingualPlan.md §2.7).
            Ok(intent) if !crate::system1::ready_for(lang, intent) => {
                ctx.timing.system1_outcome = Some("lang-not-ready".to_string());
                log::info!(
                    "system1 ({engine}) intent `{}` isn't enabled for {lang} turns yet; \
                     deferring to System-2",
                    r.intent
                );
                return None;
            }
            Ok(intent) => self.fast_reply_for(intent, ctx, transcript).await,
            Err(_) => None, // a custom config label: routable, but no handler
        };
        let Some(reply) = reply else {
            ctx.timing.system1_outcome = Some("no-handler".to_string());
            log::info!(
                "system1 ({engine}) intent `{}` has no fast-path handler; deferring to System-2",
                r.intent
            );
            return None;
        };
        // Committed: own the turn from here (never fall through → no double-speak).
        ctx.timing.path = Some("system1".to_string());
        ctx.timing.system1_outcome = Some("resolve".to_string());
        let delivery = self.speak_fast_reply(ctx, &reply).await;
        log::info!(
            "system1 ({engine}) handled intent `{}` on the fast path",
            r.intent
        );
        Some((reply.text, delivery))
    }

    /// Decide the fast-path reply for a resolved intent, or `None` to defer. Exhaustive
    /// over [`Intent`], so a new intent must choose a handler (or defer) here.
    async fn fast_reply_for(
        &self,
        intent: Intent,
        ctx: &mut TurnCtx<'_>,
        transcript: &str,
    ) -> Option<FastReply> {
        let lang = ctx.lang.lang;
        match intent {
            Intent::Weather => self.weather_reply(ctx).await,
            Intent::Timer => timer_reply(transcript, lang),
            // Clock queries answer from the wall clock (no tool, no network, no widget).
            // Always resolvable → always commit.
            Intent::Time | Intent::Date => Some(FastReply::say(spoken_clock_reply(intent, lang))),
            Intent::TimerCancel => timer_cancel_reply(&ctx.device_ctx, lang),
            Intent::TimerQuery => timer_query_reply(&ctx.device_ctx, lang),
            Intent::TimerAdjust => timer_adjust_reply(transcript, &ctx.device_ctx, lang),
            Intent::WeatherDismiss | Intent::RecipeDismiss | Intent::PlaceDismiss => {
                screen_dismiss_reply(intent, &ctx.device_ctx, lang)
            }
            // The user signalled they are done: a brief ack that ends the follow-up chain
            // (a bare `audio-stop`, no `listen`), returning the device to IDLE. Always
            // resolvable (a fresh "goodbye" turn just ends cleanly).
            Intent::EndSession => Some(FastReply::say(phrases::okay(lang)).ending_session()),
            Intent::StopDismiss => stop_dismiss_reply(&ctx.device_ctx, ctx.followup_depth, lang),
            // Route-only: the free-form place name needs the System-2 LLM.
            Intent::Place => None,
        }
    }

    /// The System-1 `weather` fast path: fetch the forecast for home, open the
    /// full-screen weather widget *before* speaking, then speak a short templated
    /// summary and invite a follow-up. Returns `None` (defer) on a precondition that
    /// fails before anything is emitted: no provider, no home location (from the turn
    /// snapshot), or the fetch errors / exceeds [`SYSTEM1_WEATHER_BUDGET`].
    async fn weather_reply(&self, ctx: &mut TurnCtx<'_>) -> Option<FastReply> {
        let provider = self.weather.as_ref()?; // weather disabled → defer
        let household = &ctx.runtime.household;
        let location = household.address.weather_location().unwrap_or_default();
        if location.trim().is_empty() {
            log::debug!("system1 weather: no home location configured; deferring to System-2");
            return None;
        }
        let imperial = crate::household::units_are_imperial(household.weather_units.as_deref());
        let t0 = Instant::now();
        let report = match tokio::time::timeout(
            SYSTEM1_WEATHER_BUDGET,
            provider.fetch(&location, imperial, crate::weather::ForecastWhen::Now),
        )
        .await
        {
            Ok(Ok(report)) => {
                ctx.timing.fetch_ms = Some(t0.elapsed().as_millis() as u64);
                log::info!(
                    "system1 weather: fetch OK in {}ms (location {location:?})",
                    t0.elapsed().as_millis(),
                );
                report
            }
            Ok(Err(e)) => {
                log::warn!(
                    "system1 weather fetch failed in {}ms ({e:#}); deferring to System-2",
                    t0.elapsed().as_millis(),
                );
                return None;
            }
            Err(_) => {
                log::warn!(
                    "system1 weather fetch exceeded {}ms budget; deferring to System-2",
                    SYSTEM1_WEATHER_BUDGET.as_millis(),
                );
                return None;
            }
        };
        // Spoken in the turn's language; the widget (and its frame data) stays English.
        let text = phrases::weather_summary(ctx.lang.lang, &report);
        // Widget first, so the screen changes the instant the intent resolves.
        Some(FastReply::say(text).with_action(DeviceAction::ShowWeather(report)))
    }
}

/// The System-1 `timer` fast path: parse an unambiguous duration from the transcript
/// and start the timer. `None` (defer) when no clear duration is present (e.g. "cancel
/// my timer", or a free-form request) or it is out of range (0 or over
/// [`MAX_TIMER_SECS`]), so the robust System-2 timer tool still handles those.
fn timer_reply(transcript: &str, lang: Lang) -> Option<FastReply> {
    let secs = crate::system1::parse_duration_in(lang, transcript)?; // unclear → defer
    if secs == 0 || secs > MAX_TIMER_SECS {
        log::debug!("system1 timer: duration {secs}s out of range; deferring to System-2");
        return None;
    }
    Some(
        FastReply::say(phrases::timer_set(lang, secs)).with_action(DeviceAction::StartTimer {
            label: None,
            duration_secs: secs,
        }),
    )
}

/// The System-1 `timer_cancel` fast path: cancel all running timers and confirm.
/// **Precondition (ground truth):** a timer must actually be running — otherwise there
/// is nothing to cancel, so defer to System-2 (which can tell the user there are no
/// timers). Only unlabeled "cancel all" resolves here; a labeled cancel needs a
/// free-form slot and stays on System-2.
fn timer_cancel_reply(device_ctx: &DeviceContext, lang: Lang) -> Option<FastReply> {
    if !device_ctx.timers.any_running() {
        return None; // nothing to cancel → defer
    }
    let text = phrases::timers_cancelled(lang, device_ctx.timers.running > 1);
    Some(FastReply::say(text).with_action(DeviceAction::CancelTimer { label: None }))
}

/// The System-1 `timer_query` fast path: answer "how much time is left?" from the
/// device-reported remaining time. **Precondition:** a timer must be running (else
/// defer). Labeled queries need a free-form slot and stay on System-2.
fn timer_query_reply(device_ctx: &DeviceContext, lang: Lang) -> Option<FastReply> {
    if !device_ctx.timers.any_running() {
        return None; // no timer → defer
    }
    Some(FastReply::say(phrases::timer_remaining(
        lang,
        device_ctx.timers.next_remaining_secs,
    )))
}

/// The System-1 `timer_adjust` fast path: add or remove time on the running timer.
/// **Preconditions (ground truth):** exactly one timer is running (so a bare, unlabeled
/// "add five minutes" is unambiguous — with zero or several running, or a labeled
/// request, defer to System-2's `adjust_timer` tool), and the transcript carries an
/// unambiguous signed delta ([`parse_timer_adjust`]). The signed magnitude is clamped to
/// the shared cap so the delta can't overflow the device's deadline math.
fn timer_adjust_reply(
    transcript: &str,
    device_ctx: &DeviceContext,
    lang: Lang,
) -> Option<FastReply> {
    if device_ctx.timers.running != 1 {
        return None; // nothing to adjust, or ambiguous which timer → defer
    }
    let delta = crate::system1::parse_timer_adjust_in(lang, transcript)?; // unclear → defer
    let magnitude = delta.unsigned_abs().min(MAX_TIMER_SECS);
    if magnitude == 0 {
        return None;
    }
    let delta_secs = if delta < 0 {
        -(magnitude as i64)
    } else {
        magnitude as i64
    };
    let text = phrases::timer_adjusted(lang, delta > 0, magnitude);
    Some(FastReply::say(text).with_action(DeviceAction::AdjustTimer {
        label: None,
        delta_secs,
    }))
}

/// The System-1 `weather_dismiss` / `recipe_dismiss` / `place_dismiss` fast path: close
/// the named screen and give a brief ack. **Precondition:** that screen must actually be
/// the foreground widget (ground truth from `device_ctx`), else defer — "close the
/// recipe" with no recipe up is meaningless and better handled by System-2.
fn screen_dismiss_reply(
    intent: Intent,
    device_ctx: &DeviceContext,
    lang: Lang,
) -> Option<FastReply> {
    let action = match (intent, device_ctx.widget.as_ref()) {
        (Intent::WeatherDismiss, Some(DisplayContext::Weather(_))) => DeviceAction::DismissWeather,
        (Intent::RecipeDismiss, Some(DisplayContext::Recipe(_))) => DeviceAction::DismissRecipe,
        (Intent::PlaceDismiss, Some(DisplayContext::Place(_))) => DeviceAction::DismissPlace,
        _ => return None,
    };
    Some(FastReply::say(phrases::okay(lang)).with_action(action))
}

/// The System-1 `stop_dismiss` fast path: a bare "stop"/"cancel"/"never mind" whose
/// target is not named. The **deterministic priority ladder** picks the referent from
/// live device state (ground truth) — the classifier never chooses it (§17). A rung
/// with no valid target falls through; if nothing matches, return `None` (the
/// **ground-truth veto**) so the turn defers to System-2 rather than guessing.
///
/// Rung 1 (alarm *ringing*) and rung 3 (media *playing*) are parked until those
/// signals are reported (§19.7); rung 2 (running timer), rung 4 (open screen), and
/// rung 5 (end an active follow-up) are live.
fn stop_dismiss_reply(
    device_ctx: &DeviceContext,
    followup_depth: u32,
    lang: Lang,
) -> Option<FastReply> {
    // Rung 2: a running timer → cancel it.
    if device_ctx.timers.any_running() {
        return timer_cancel_reply(device_ctx, lang);
    }
    // Rung 4: an open screen → dismiss it.
    let dismiss = match device_ctx.widget {
        Some(DisplayContext::Weather(_)) => Some(Intent::WeatherDismiss),
        Some(DisplayContext::Recipe(_)) => Some(Intent::RecipeDismiss),
        Some(DisplayContext::Place(_)) => Some(Intent::PlaceDismiss),
        // The music player has no System-1 dismiss intent — defer to the LLM, whose
        // `music_screen` / `spotify_control` tools cover close vs. stop-the-music.
        Some(DisplayContext::Music(_)) => None,
        // The diagnostics and enrollment screens are suppressing contexts that never
        // reach here (the turn short-circuits earlier), and have no voice dismiss.
        Some(DisplayContext::Diagnostics) | Some(DisplayContext::Enroll(_)) | None => None,
    };
    if let Some(intent) = dismiss {
        return screen_dismiss_reply(intent, device_ctx, lang);
    }
    // Rung 5: mid-conversation with nothing active → end the follow-up chain.
    if followup_depth > 0 {
        return Some(FastReply::say(phrases::okay(lang)).ending_session());
    }
    // Ground-truth veto: nothing to act on → defer to System-2.
    None
}

/// A short spoken reply for the System-1 `time` / `date` clock intents, formatted from
/// the Mac's wall clock. `Time` → "It's 3:45 PM."; anything else → today's date.
fn spoken_clock_reply(intent: Intent, lang: Lang) -> String {
    let now = chrono::Local::now();
    match intent {
        Intent::Time => phrases::time_now(lang, &now),
        _ => phrases::date_today(lang, &now),
    }
}

/// A speakable English duration, e.g. `600` → "10 minutes" — the timer phrasing every
/// English confirmation now gets through [`phrases::duration`]. Kept as the test oracle's
/// subject so the legacy-equality check below still pins English output exactly.
#[cfg(test)]
fn human_duration(secs: u64) -> String {
    phrases::duration(Lang::En, secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wyoming::protocol::{PlaceScreen, TimerContext};

    fn ctx_with(widget: Option<DisplayContext>, running: u32) -> DeviceContext {
        DeviceContext {
            widget,
            timers: TimerContext {
                running,
                next_remaining_secs: (running > 0).then_some(90),
                labels: Vec::new(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn timer_reply_respects_the_shared_cap() {
        assert!(timer_reply("set a timer for 10 minutes", Lang::En).is_some());
        assert!(timer_reply("set a timer for 25 hours", Lang::En).is_none());
        assert!(timer_reply("set a timer", Lang::En).is_none());
    }

    #[test]
    fn timer_adjust_resolves_only_with_one_timer_and_a_clear_signed_delta() {
        // One running timer + a clear "add" → a positive delta and a spoken confirmation.
        let r = timer_adjust_reply(
            "add five minutes to the timer",
            &ctx_with(None, 1),
            Lang::En,
        )
        .unwrap();
        assert!(matches!(
            r.action,
            Some(DeviceAction::AdjustTimer {
                label: None,
                delta_secs: 300
            })
        ));
        assert_eq!(r.text, "Added 5 minutes to your timer.");
        // "take a minute off" → a negative delta.
        let r = timer_adjust_reply("take a minute off the timer", &ctx_with(None, 1), Lang::En)
            .unwrap();
        assert!(matches!(
            r.action,
            Some(DeviceAction::AdjustTimer {
                label: None,
                delta_secs: -60
            })
        ));
        assert_eq!(r.text, "Took 1 minute off your timer.");
        // No timer running → defer.
        assert!(timer_adjust_reply("add five minutes", &ctx_with(None, 0), Lang::En).is_none());
        // Several timers → ambiguous which one → defer.
        assert!(timer_adjust_reply("add five minutes", &ctx_with(None, 2), Lang::En).is_none());
        // One timer but no clear direction (absolute "set to") → defer.
        assert!(timer_adjust_reply(
            "change the timer to ten minutes",
            &ctx_with(None, 1),
            Lang::En
        )
        .is_none());
    }

    #[test]
    fn place_dismiss_routes_directly_when_the_card_is_open() {
        let place = Some(DisplayContext::Place(PlaceScreen::default()));
        let r = screen_dismiss_reply(Intent::PlaceDismiss, &ctx_with(place, 0), Lang::En).unwrap();
        assert!(matches!(r.action, Some(DeviceAction::DismissPlace)));
        // Nothing open → defer.
        assert!(screen_dismiss_reply(Intent::PlaceDismiss, &ctx_with(None, 0), Lang::En).is_none());
    }

    #[test]
    fn stop_ladder_prefers_a_running_timer_then_a_screen_then_ending_the_chain() {
        let r = stop_dismiss_reply(&ctx_with(None, 1), 0, Lang::En).unwrap();
        assert!(matches!(
            r.action,
            Some(DeviceAction::CancelTimer { label: None })
        ));
        let place = Some(DisplayContext::Place(PlaceScreen::default()));
        let r = stop_dismiss_reply(&ctx_with(place, 0), 0, Lang::En).unwrap();
        assert!(matches!(r.action, Some(DeviceAction::DismissPlace)));
        let r = stop_dismiss_reply(&ctx_with(None, 0), 2, Lang::En).unwrap();
        assert!(r.end_session && r.action.is_none());
        assert!(stop_dismiss_reply(&ctx_with(None, 0), 0, Lang::En).is_none());
    }

    #[test]
    fn human_duration_reads_naturally() {
        assert_eq!(human_duration(600), "10 minutes");
        assert_eq!(human_duration(5400), "1 hour 30 minutes");
        assert_eq!(human_duration(90), "1 minute 30 seconds");
        assert_eq!(human_duration(0), "0 seconds");
    }

    /// The humanizer this module used before it moved onto `crate::speech::duration`
    /// (D11), kept as the oracle: the timer confirmations must read exactly as before.
    fn legacy_human_duration(secs: u64) -> String {
        let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
        let unit = |n: u64, singular: &str| {
            if n == 1 {
                format!("1 {singular}")
            } else {
                format!("{n} {singular}s")
            }
        };
        let mut parts = Vec::new();
        if h > 0 {
            parts.push(unit(h, "hour"));
        }
        if m > 0 {
            parts.push(unit(m, "minute"));
        }
        if s > 0 {
            parts.push(unit(s, "second"));
        }
        if parts.is_empty() {
            return "0 seconds".to_string();
        }
        parts.join(" ")
    }

    #[test]
    fn human_duration_matches_the_legacy_output_exactly() {
        // Every second up to 2 h 1 min, then a sweep out past the 24 h timer cap.
        let secs = (0..=7_260).chain((7_261..=200_000).step_by(37));
        for s in secs {
            assert_eq!(human_duration(s), legacy_human_duration(s), "{s}s");
        }
    }

    #[test]
    fn portuguese_turns_get_portuguese_fast_replies() {
        let r = timer_reply("coloca um timer de dez minutos", Lang::PtBr).unwrap();
        assert_eq!(r.text, "Timer de 10 minutos iniciado.");
        assert!(matches!(
            r.action,
            Some(DeviceAction::StartTimer {
                duration_secs: 600,
                ..
            })
        ));
        // An English phrasing on a pt turn has no pt duration → defer (System-2).
        assert!(timer_reply("set a timer", Lang::PtBr).is_none());
        let r =
            timer_adjust_reply("tira um minuto do timer", &ctx_with(None, 1), Lang::PtBr).unwrap();
        assert_eq!(r.text, "Tirei um minuto do timer.");
        let r = timer_cancel_reply(&ctx_with(None, 2), Lang::PtBr).unwrap();
        assert_eq!(r.text, "Tudo bem, cancelei os timers.");
        let r = timer_query_reply(&ctx_with(None, 1), Lang::PtBr).unwrap();
        assert_eq!(r.text, "Faltam um minuto e 30 segundos.");
        let r = stop_dismiss_reply(&ctx_with(None, 0), 2, Lang::PtBr).unwrap();
        assert_eq!(r.text, "Tudo bem.");
        assert!(spoken_clock_reply(Intent::Date, Lang::PtBr).starts_with("Hoje é "));
    }
}
