//! Pluggable **System-1 fast-decision** stage (plans/system1-fast-decisions.md).
//!
//! A System-1 decision engine runs on the Anamanti Core **before** memory recall and
//! the LLM. Given the turn's transcript (plus light context), it either **Resolves** a
//! common intent — which the orchestrator answers via existing tools + `DeviceAction`s,
//! skipping the blocking GraphRAG embedding round-trip *and* the rig+tools full
//! completion — or **Defers** the turn to today's "System-2" path unchanged.
//!
//! Like the LLM backend, the engine is chosen from config and hidden behind one trait
//! ([`DecisionEngine`]), so a local `laya-serve` sidecar or Jev/OpenRouter (both speak
//! the shared `/v1/systemone` contract) are swappable without touching the pipeline.
//!
//! Implemented: the [`NoDecision`] default (`backend = "none"`, which skips the stage
//! entirely), the `laya-serve` / `jev` HTTP backend ([`http::HttpDecider`]), the test
//! [`mock::MockDecider`], and the closed [`Intent`] set the orchestrator's fast-path
//! handlers serve. `laya-embedded` (in-process) is not implemented yet.

pub mod http;
pub mod mock;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

/// The closed set of intents System-1 knows how to route. The wire carries them as
/// their [`as_str`](Intent::as_str) labels (the `criteria` keys of the `/v1/systemone`
/// request, and [`Resolution::intent`]); the orchestrator parses a resolution back with
/// [`FromStr`](std::str::FromStr) and dispatches with an exhaustive `match`, so adding a
/// variant forces a decision about its handler. A config-supplied label that is not an
/// `Intent` can still be routed, but it never has a handler and always defers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intent {
    /// Current conditions or the forecast for home (fast path: the weather widget).
    Weather,
    /// Start a timer for a stated duration.
    Timer,
    /// The current time of day.
    Time,
    /// Today's date / day of the week.
    Date,
    /// Cancel the running timer(s).
    TimerCancel,
    /// How much time is left on the running timer.
    TimerQuery,
    /// Close the weather screen.
    WeatherDismiss,
    /// Close the recipe screen.
    RecipeDismiss,
    /// Close the place card.
    PlaceDismiss,
    /// The user is done: end the follow-up chain.
    EndSession,
    /// A bare "stop"/"cancel" whose target comes from live device state.
    StopDismiss,
    /// A question about a specific place. **Route-only:** it has a free-form slot (the
    /// place name), so it has no fast-path handler and always defers to the System-2
    /// LLM, which calls `places_lookup` (`plans/system1-fast-decisions.md` §15.2, Tier C).
    Place,
}

impl Intent {
    /// Every intent, in the order the default router offers them.
    pub const ALL: [Intent; 12] = [
        Intent::Weather,
        Intent::Timer,
        Intent::Time,
        Intent::Date,
        Intent::TimerCancel,
        Intent::TimerQuery,
        Intent::WeatherDismiss,
        Intent::RecipeDismiss,
        Intent::PlaceDismiss,
        Intent::EndSession,
        Intent::StopDismiss,
        Intent::Place,
    ];

    /// The wire label (`"weather"`, `"timer_cancel"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Intent::Weather => "weather",
            Intent::Timer => "timer",
            Intent::Time => "time",
            Intent::Date => "date",
            Intent::TimerCancel => "timer_cancel",
            Intent::TimerQuery => "timer_query",
            Intent::WeatherDismiss => "weather_dismiss",
            Intent::RecipeDismiss => "recipe_dismiss",
            Intent::PlaceDismiss => "place_dismiss",
            Intent::EndSession => "end_session",
            Intent::StopDismiss => "stop_dismiss",
            Intent::Place => "place",
        }
    }

    /// The human-readable criterion the classifier sees for this intent.
    pub fn description(self) -> &'static str {
        match self {
            Intent::Weather => "current conditions or the forecast",
            Intent::Timer => "start a timer or alarm for a stated duration",
            Intent::Time => "the current time of day right now",
            Intent::Date => "today's date or the current day of the week",
            Intent::TimerCancel => "cancel or stop a running timer or alarm",
            Intent::TimerQuery => "ask how much time is left on a running timer",
            Intent::WeatherDismiss => "close or dismiss the weather screen",
            Intent::RecipeDismiss => "close or dismiss the recipe screen",
            Intent::PlaceDismiss => "close or dismiss the place card",
            Intent::Place => {
                "ask about a specific business, shop, restaurant, cafe, landmark, or point of \
                 interest — where it is, its address, hours, phone, or rating"
            }
            Intent::EndSession => {
                "the user is finished and wants no more replies — a sign-off or dismissal \
                 like \"that's all\", \"nothing else\", or \"goodbye\", including a bare \
                 standalone thanks used to close the conversation in any language \
                 (\"thank you\", \"thanks\", \"gracias\", \"merci\"); NOT stopping a timer \
                 or music, and NOT a thanks that is part of a continuing request \
                 (e.g. \"thanks, now what's the weather\")"
            }
            Intent::StopDismiss => {
                "a bare \"stop\", \"cancel\", \"never mind\", or \"dismiss\" with no named \
                 target (what it refers to depends on what is currently active)"
            }
        }
    }

    /// Whether the answer changes with a place named in the turn, so a turn that names a
    /// *specific* place must defer to System-2 rather than fast-path against home:
    /// `weather` (the fast handler only knows the home forecast) and `time`/`date` (the
    /// wall clock is local — "what time is it in London" is a different timezone).
    pub fn place_sensitive(self) -> bool {
        matches!(self, Intent::Weather | Intent::Time | Intent::Date)
    }
}

impl std::fmt::Display for Intent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A label that is not one of the built-in [`Intent`]s (e.g. a custom config entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownIntent(pub String);

impl std::fmt::Display for UnknownIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown System-1 intent `{}`", self.0)
    }
}

impl std::error::Error for UnknownIntent {}

impl std::str::FromStr for Intent {
    type Err = UnknownIntent;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Intent::ALL
            .into_iter()
            .find(|i| i.as_str() == s)
            .ok_or_else(|| UnknownIntent(s.to_string()))
    }
}

/// The built-in intents the router may resolve when the config leaves `intents` empty:
/// every [`Intent`]. An intent listed here still only resolves if the orchestrator's
/// handler preconditions hold (e.g. timer needs a parseable duration), otherwise the
/// turn defers to System-2; the route-only [`Intent::Place`] always defers.
pub fn default_intents() -> Vec<String> {
    Intent::ALL.iter().map(|i| i.as_str().to_string()).collect()
}

/// Parse a spoken timer/alarm duration into whole seconds, or `None` when the text has
/// no clear duration (e.g. "cancel my timer", "set a timer") — the caller then defers to
/// System-2, whose timer tool handles cancels and free-form phrasing. Deliberately
/// conservative: it only fires on an unambiguous `<number> <unit>` (repeatable, e.g.
/// "1 hour 30 minutes"). Supports digits and common number words; `a`/`an` = 1,
/// `half` = 0.5, `quarter` = 0.25 of the following unit.
pub fn parse_duration_secs(text: &str) -> Option<u64> {
    fn word_value(tok: &str) -> Option<f64> {
        Some(match tok {
            "one" => 1.0,
            "two" => 2.0,
            "three" => 3.0,
            "four" => 4.0,
            "five" => 5.0,
            "six" => 6.0,
            "seven" => 7.0,
            "eight" => 8.0,
            "nine" => 9.0,
            "ten" => 10.0,
            "eleven" => 11.0,
            "twelve" => 12.0,
            "thirteen" => 13.0,
            "fourteen" => 14.0,
            "fifteen" => 15.0,
            "twenty" => 20.0,
            "thirty" => 30.0,
            "forty" => 40.0,
            "fifty" => 50.0,
            "sixty" => 60.0,
            "ninety" => 90.0,
            "half" => 0.5,
            "quarter" => 0.25,
            _ => return None,
        })
    }
    fn unit_secs(tok: &str) -> Option<f64> {
        Some(match tok {
            "hour" | "hours" | "hr" | "hrs" => 3600.0,
            "minute" | "minutes" | "min" | "mins" => 60.0,
            "second" | "seconds" | "sec" | "secs" => 1.0,
            _ => return None,
        })
    }

    let normalized = text.to_lowercase().replace('-', " ");
    let mut total = 0.0f64;
    let mut current = 0.0f64;
    let mut have_current = false;
    let mut saw_unit = false;

    for raw in normalized.split_whitespace() {
        let tok = raw.trim_matches(|c: char| !c.is_alphanumeric());
        if tok.is_empty() {
            continue;
        }
        if let Ok(n) = tok.parse::<f64>() {
            current += n;
            have_current = true;
        } else if let Some(v) = word_value(tok) {
            current += v;
            have_current = true;
        } else if let Some(u) = unit_secs(tok) {
            // A unit with no explicit number means one ("a minute", "an hour", "half an
            // hour" → the `half` is the number, `an` is ignored, `hour` defaults nothing).
            let n = if have_current { current } else { 1.0 };
            total += n * u;
            current = 0.0;
            have_current = false;
            saw_unit = true;
        }
        // Any other token — the article "a"/"an", "set", "timer", "for", "and", … — is
        // ignored. (Counting "a" as a number would turn "a timer for 10 minutes" into 11.)
    }

    if !saw_unit {
        return None;
    }
    let secs = total.round();
    if secs < 1.0 {
        None
    } else {
        Some(secs as u64)
    }
}

/// The turn state handed to a decision engine — **light context only** (no memory
/// recall; that's exactly the cost System-1 exists to skip).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRequest {
    /// The final STT transcript for this turn.
    pub transcript: String,
    /// A short label for what the display is currently showing (e.g. `"recipe"`), so
    /// screen-relative commands ("scroll down") can route. `None` on an idle display.
    /// From the turn's `DisplayContext`; sent in the request `state` as `screen`.
    pub screen: Option<String>,
    /// Active-timer state the device reported this turn (running count / soonest
    /// remaining / labels), **orthogonal to `screen`** — a timer can run behind another
    /// widget. Ground truth for the `timer_query` / `timer_cancel` / `stop_dismiss`
    /// decisions (plans/system1-fast-decisions.md §17, §19). Empty when nothing runs.
    pub timers: crate::wyoming::protocol::TimerContext,
}

/// A resolved fast intent: the routed label plus its calibrated confidence. The
/// orchestrator parses `intent` into an [`Intent`] to pick a handler.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// The routed intent label, e.g. `"weather"`, `"timer"` (see [`Intent::as_str`]).
    pub intent: String,
    /// The engine's calibrated confidence in this resolution (`0.0..=1.0`).
    pub confidence: f64,
}

/// The outcome of a System-1 evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Confident, closed intent — answer it on the fast path (skips recall + LLM).
    Resolve(Resolution),
    /// Ambiguous / open-ended / low-confidence / disabled — fall through to System-2.
    Defer,
}

/// A swappable System-1 decision engine, selected from config like the LLM backend.
/// Implementations are `Send + Sync` so one instance is shared across concurrent turns
/// behind an `Arc`.
#[async_trait]
pub trait DecisionEngine: Send + Sync {
    /// A short label for logs/settings (e.g. `"none"`, `"laya-serve"`, `"jev"`).
    /// The pipeline skips the whole stage when this is `"none"`.
    fn name(&self) -> &str;

    /// Evaluate the turn. Errors (or low confidence) should surface as `Err`/`Defer`
    /// rather than blocking the turn — the caller always falls through to System-2.
    async fn decide(&self, req: &DecisionRequest) -> Result<Decision>;
}

/// The default no-op engine: always defers to System-2. Selected by `backend = "none"`
/// (the default), so the whole feature is inert until explicitly enabled.
pub struct NoDecision;

#[async_trait]
impl DecisionEngine for NoDecision {
    fn name(&self) -> &str {
        "none"
    }
    async fn decide(&self, _req: &DecisionRequest) -> Result<Decision> {
        Ok(Decision::Defer)
    }
}

/// The default engine (`NoDecision`) as a shared trait object.
pub fn none() -> Arc<dyn DecisionEngine> {
    Arc::new(NoDecision)
}

/// Build a decision engine from a backend label + settings. The single construction
/// point shared by the boot path ([`crate::config::Config::shared_settings`]) and the
/// runtime config-page swap ([`crate::settings::SharedSettings::apply_system1`]), so both
/// agree on what each backend name means. An empty `intents` list falls back to
/// [`default_intents`].
///
/// - `none`/`off`/`""` → [`NoDecision`] (disabled)
/// - `mock` → a deferring [`mock::MockDecider`]
/// - `laya-serve` → local HTTP sidecar (`base_url`)
/// - `jev` → OpenRouter (`base_url` API root + `model` + `api_key`)
/// - `laya-embedded` → not yet implemented (bails)
pub fn build(
    backend: &str,
    base_url: &str,
    model: &str,
    api_key: Option<String>,
    min_confidence: f64,
    intents: Vec<String>,
) -> Result<Arc<dyn DecisionEngine>> {
    use http::HttpDecider;
    let intents = if intents.is_empty() {
        default_intents()
    } else {
        intents
    };
    match backend.to_lowercase().as_str() {
        "none" | "off" | "" => Ok(none()),
        "mock" => Ok(Arc::new(mock::MockDecider::defer())),
        "laya-serve" | "laya_serve" => Ok(Arc::new(HttpDecider::laya_serve(
            base_url,
            min_confidence,
            intents,
        ))),
        "jev" => Ok(Arc::new(HttpDecider::jev(
            base_url,
            model,
            api_key,
            min_confidence,
            intents,
        ))),
        other @ ("laya-embedded" | "laya_embedded") => anyhow::bail!(
            "system1.backend `{other}` (in-process candle) is not implemented yet — it is the \
             optional escape hatch. Use `laya-serve` (local sidecar) or `jev` for now."
        ),
        other => anyhow::bail!(
            "unknown system1.backend `{other}` (expected none/mock/laya-serve/jev/laya-embedded)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(t: &str) -> DecisionRequest {
        DecisionRequest {
            transcript: t.to_string(),
            screen: None,
            timers: Default::default(),
        }
    }

    #[test]
    fn intent_labels_round_trip_and_unknown_labels_are_rejected() {
        for intent in Intent::ALL {
            assert_eq!(intent.as_str().parse::<Intent>(), Ok(intent));
            assert!(
                !intent.description().is_empty(),
                "{intent} needs a criterion"
            );
        }
        assert_eq!(
            "music".parse::<Intent>(),
            Err(UnknownIntent("music".to_string()))
        );
        assert!(Intent::Weather.place_sensitive());
        assert!(Intent::Time.place_sensitive() && Intent::Date.place_sensitive());
        assert!(!Intent::Timer.place_sensitive());
    }

    #[test]
    fn default_intents_cover_every_intent_including_place_dismiss() {
        let defaults = default_intents();
        assert_eq!(defaults.len(), Intent::ALL.len());
        assert!(defaults.iter().any(|i| i == "place_dismiss"));
    }

    #[tokio::test]
    async fn no_decision_always_defers() {
        let e = NoDecision;
        assert_eq!(e.name(), "none");
        assert_eq!(
            e.decide(&req("show me the weather")).await.unwrap(),
            Decision::Defer
        );
    }

    #[test]
    fn parse_duration_handles_common_phrasings() {
        assert_eq!(parse_duration_secs("set a timer for 10 minutes"), Some(600));
        assert_eq!(parse_duration_secs("10 minute timer"), Some(600));
        assert_eq!(
            parse_duration_secs("set a timer for 1 hour 30 minutes"),
            Some(5400)
        );
        assert_eq!(parse_duration_secs("half an hour"), Some(1800));
        assert_eq!(parse_duration_secs("a minute"), Some(60));
        assert_eq!(parse_duration_secs("90 seconds"), Some(90));
        assert_eq!(parse_duration_secs("twenty five minutes"), Some(1500));
        assert_eq!(parse_duration_secs("2 hrs"), Some(7200));
    }

    #[test]
    fn parse_duration_defers_when_unclear() {
        // No unit → defer (the LLM's timer tool handles these).
        assert_eq!(parse_duration_secs("set a timer"), None);
        assert_eq!(parse_duration_secs("cancel my timer"), None);
        assert_eq!(parse_duration_secs("what's the weather"), None);
        assert_eq!(parse_duration_secs(""), None);
    }
}
