//! HTTP [`DecisionEngine`] over the shared **`/v1/systemone`** contract, covering both
//! a local **`laya-serve`** sidecar and **Jev on OpenRouter** — they speak the same
//! wire shape (Laya is "Jev-compatible"), so one client serves both by swapping the
//! base URL, optional bearer auth, and optional `model` field.
//!
//! The engine is a **router**: it asks a fixed typed question set (an `intent` choice
//! plus a `needs_full_understanding` noul) in one forward pass and maps the calibrated
//! answers to [`Decision::Resolve`] / [`Decision::Defer`] under a strict confidence
//! floor. It never extracts free-form slots — anything open-ended defers to System-2.
//!
//! Wire shapes (verified live against OpenRouter's `POST /api/v1/systemone`, which is
//! the shared System One contract the local `laya-serve` sidecar also speaks):
//! ```json
//! // request
//! { "model": "typesafe/jev-1.13",              // omitted for laya-serve
//!   "state": { "message": "<transcript>" },
//!   "questions": {
//!     "intent": { "type": "choice", "instructions": "...", "criteria": { "<id>": "<desc>" } },
//!     "needs_full_understanding": { "type": "noul", "instructions": "..." } } }
//! // response — a `choice` answer carries `choice` + `confidence` + `probabilities`;
//! // a `noul` answer carries only its `noul` probability (0..1), no confidence field.
//! { "model": "...", "answers": {
//!     "intent": { "type": "choice", "choice": "<id>", "probabilities": {..}, "confidence": 0.97 },
//!     "needs_full_understanding": { "type": "noul", "noul": 0.03 } },
//!   "usage": {..} }
//! ```

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Map, Value};

use super::{Decision, DecisionEngine, DecisionRequest, Resolution};

/// The `noul` question id used to detect turns that need System-2 reasoning.
const NEEDS_FULL: &str = "needs_full_understanding";

/// The `noul` question id that detects a specific, non-home place named in the turn
/// (e.g. "the weather **in Paris, France**"). System-1 is a router, not a slot
/// extractor — it can flag *that* a place was named but cannot hand back the string.
/// So a location intent that names a place is deferred to System-2, where the LLM
/// extracts it and calls the location-aware tool. Absent this, the fast-path handler
/// would silently answer for the home location (wrong place, fast).
const NAMES_PLACE: &str = "names_place";

/// Probability floor (P(true)) at which the `names_place` noul is treated as "a
/// specific place was named" — high enough to route to System-2 for extraction.
const NAMES_PLACE_THRESHOLD: f64 = 0.5;

/// The `choice` question id that classifies the turn's time frame (past / present /
/// future). Used as a **defer gate**: the Core has no fast historical-data path, so a
/// turn the model is confident concerns the PAST routes to System-2. present/future
/// proceed (weather resolves for both — the widget carries current + forecast). See
/// `plans/system1-fast-decisions.md` §16.
const TEMPORAL: &str = "temporal";

/// The temporal label that forces a defer.
const TEMPORAL_PAST: &str = "past";

/// Intents whose answer changes with the place named, so a turn that names a *specific*
/// place (high `names_place`) must defer to System-2 rather than fast-path against the home
/// default: `weather` (the fast handler only knows the home forecast) and `time`/`date`
/// (the wall clock is local — "what time is it in London" is a different timezone the fast
/// path can't compute). Verified live (jev): "what's the weather" resolves but "…in Tokyo"
/// defers; likewise "what time is it" resolves but "…in London" must defer.
const PLACE_SENSITIVE_INTENTS: &[&str] = &["weather", "time", "date"];

/// An HTTP System-1 engine speaking `/v1/systemone`.
pub struct HttpDecider {
    client: reqwest::Client,
    /// The fully-qualified endpoint, e.g. `http://127.0.0.1:8000/v1/systemone`.
    url: String,
    /// Model id sent in the body (`typesafe/jev-1.13` for OpenRouter); `None` for
    /// laya-serve, which serves whatever checkpoint it loaded.
    model: Option<String>,
    /// Bearer token (OpenRouter). `None` = no `Authorization` header.
    api_key: Option<String>,
    /// Short label for logs/settings.
    name: String,
    /// Confidence floor: both the intent choice and the `needs_full_understanding`
    /// answer must clear this, or the turn defers.
    min_confidence: f64,
    /// The intent labels this router may resolve (does not include the implicit
    /// `other` escape hatch).
    intents: Vec<String>,
}

impl HttpDecider {
    fn build(
        name: impl Into<String>,
        base_url: &str,
        model: Option<String>,
        api_key: Option<String>,
        min_confidence: f64,
        intents: Vec<String>,
    ) -> Self {
        // Shared, keep-alive-tuned client so the pooled connection to the sidecar/
        // OpenRouter stays warm between turns (skips the DNS+TCP+TLS handshake). The
        // short 4 s budget — a hung/absent sidecar must not block the turn; on any
        // error the caller simply defers to System-2 — is applied per-request in
        // `post()` rather than on the shared client.
        let client = crate::http::shared_client();
        let url = format!("{}/v1/systemone", base_url.trim_end_matches('/'));
        Self {
            client,
            url,
            model,
            api_key,
            name: name.into(),
            min_confidence,
            intents,
        }
    }

    /// A client pointed at a local `laya-serve` sidecar (no auth, no model pin).
    pub fn laya_serve(base_url: &str, min_confidence: f64, intents: Vec<String>) -> Self {
        Self::build("laya-serve", base_url, None, None, min_confidence, intents)
    }

    /// A client pointed at OpenRouter's Jev decisions endpoint (bearer auth + model id).
    /// `base_url` should be the API root (e.g. `https://openrouter.ai/api`).
    pub fn jev(
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        min_confidence: f64,
        intents: Vec<String>,
    ) -> Self {
        Self::build(
            "jev",
            base_url,
            Some(model.to_string()),
            api_key,
            min_confidence,
            intents,
        )
    }
}

#[async_trait]
impl DecisionEngine for HttpDecider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn decide(&self, req: &DecisionRequest) -> Result<Decision> {
        // ONE System-One round trip carries the whole routing question set (intent
        // choice + `needs_full_understanding` + `names_place` nouls). System One bundles
        // many questions per pass (up to 64), so we ask everything at once rather than a
        // defer-then-retry sequence. A location intent with no place named resolves here
        // against the home default (see the `needs_full_understanding` wording); a named
        // place is routed to System-2 for extraction.
        let body = build_request(
            &req.transcript,
            self.model.as_deref(),
            &self.intents,
            &device_state_fields(req),
        );
        let value = self.post(&body).await?;
        let decision = interpret(&value, &self.intents, self.min_confidence);

        // A location intent that named a specific place can't be fast-pathed (System One
        // flags the place but can't extract the string, and the handler only knows the
        // home location) — defer to System-2 to extract it.
        if names_place_forces_defer(&decision, &value) {
            return Ok(Decision::Defer);
        }
        Ok(decision)
    }
}

impl HttpDecider {
    /// POST a `/v1/systemone` body and decode the JSON response, erroring on a non-2xx
    /// status so the caller defers to System-2.
    async fn post(&self, body: &Value) -> Result<Value> {
        let mut rb = self
            .client
            .post(&self.url)
            .timeout(Duration::from_secs(4))
            .json(body);
        if let Some(key) = &self.api_key {
            rb = rb.bearer_auth(key);
        }
        let resp = rb.send().await.context("posting to /v1/systemone")?;
        let status = resp.status();
        let value: Value = resp
            .json()
            .await
            .context("decoding /v1/systemone response")?;
        if !status.is_success() {
            anyhow::bail!("/v1/systemone returned {status}: {value}");
        }
        Ok(value)
    }
}

/// Human-readable criteria for the built-in intents (M1: weather; M3 adds the rest).
/// Unknown/custom labels fall back to the label itself as their description.
fn intent_description(intent: &str) -> &str {
    match intent {
        "weather" => "current conditions or the forecast",
        "timer" => "start a timer or alarm for a stated duration",
        "time" => "the current time of day right now",
        "date" => "today's date or the current day of the week",
        "timer_cancel" => "cancel or stop a running timer or alarm",
        "timer_query" => "ask how much time is left on a running timer",
        "weather_dismiss" => "close or dismiss the weather screen",
        "recipe_dismiss" => "close or dismiss the recipe screen",
        "place" => {
            "ask about a specific business, shop, restaurant, cafe, landmark, or point of \
             interest — where it is, its address, hours, phone, or rating"
        }
        "end_session" => {
            "the user is finished and wants no more replies — a sign-off or dismissal \
             like \"that's all\", \"nothing else\", or \"goodbye\"; NOT stopping a timer \
             or music"
        }
        "stop_dismiss" => {
            "a bare \"stop\", \"cancel\", \"never mind\", or \"dismiss\" with no named \
             target (what it refers to depends on what is currently active)"
        }
        "recipe_nav" => "navigate or scroll the recipe already on screen",
        "music" => "play, pause, skip, or change music volume",
        "shopping_add" => "add an item to the shopping list",
        "smalltalk" => "a greeting, thanks, or acknowledgement needing no data",
        _ => "",
    }
}

/// The compact device-state fields folded into the request `state` for the 3B hybrid
/// (§17.4): the foreground widget label and any running-timer summary. Present only when
/// there is something to report, so an ordinary turn (idle screen, no timer) sends the
/// byte-identical `{ "message": … }` state it always did — the classifier only *sees*
/// device state when there is state, which is exactly when `stop_dismiss` / dismiss
/// phrasings need it. The deterministic ladder still owns the referent; this only sharpens
/// the classifier's confidence on vague phrasing.
pub fn device_state_fields(req: &DecisionRequest) -> Map<String, Value> {
    let mut m = Map::new();
    if let Some(screen) = req.screen.as_deref().filter(|s| !s.is_empty()) {
        m.insert("screen".to_string(), json!(screen));
    }
    if req.timers.running > 0 {
        m.insert("timers_running".to_string(), json!(req.timers.running));
        if let Some(secs) = req.timers.next_remaining_secs {
            m.insert("timer_remaining_secs".to_string(), json!(secs));
        }
    }
    m
}

/// Build the `/v1/systemone` request body: the transcript (plus any device-state fields
/// for the 3B hybrid) as `state`, and the fixed routing question set. Pure, so it is
/// unit-tested without a server.
pub fn build_request(
    transcript: &str,
    model: Option<&str>,
    intents: &[String],
    device_state: &Map<String, Value>,
) -> Value {
    let mut criteria = Map::new();
    for intent in intents {
        let desc = intent_description(intent);
        let desc = if desc.is_empty() {
            intent.as_str()
        } else {
            desc
        };
        criteria.insert(intent.clone(), json!(desc));
    }
    criteria.insert(
        "other".to_string(),
        json!("anything that needs reasoning, memory, or open-ended understanding"),
    );

    let questions = json!({
        "intent": {
            "type": "choice",
            "instructions": "What does the user want in `message`? Pick the single best fit.",
            "criteria": Value::Object(criteria),
        },
        NEEDS_FULL: {
            "type": "noul",
            "instructions": "Does answering `message` require open-ended reasoning, personal \
                             memory, or multi-step understanding? A location-based command \
                             (like weather) defaults to the user's home location when no \
                             place is named, so a missing location by itself does NOT count.",
        },
        NAMES_PLACE: {
            "type": "noul",
            "instructions": "Does `message` name a specific geographic place — a city, \
                             country, or landmark (e.g. \"Paris\", \"in Tokyo\") — as the \
                             location to act on, rather than implying the user's current \
                             home area?",
        },
        TEMPORAL: {
            "type": "choice",
            "instructions": "Does answering `message` concern the PAST (already happened / \
                             historical), the PRESENT (now / the current state), or the \
                             FUTURE (upcoming / a forecast / scheduled)? Choose 'present' \
                             if it is not time-bound.",
            "criteria": {
                "past": "already happened; historical; a previous time",
                "present": "now; the current state; or not time-bound",
                "future": "upcoming; a forecast; something scheduled",
            },
        },
    });

    let mut state = Map::new();
    state.insert("message".to_string(), json!(transcript));
    for (k, v) in device_state {
        state.insert(k.clone(), v.clone());
    }

    let mut body = Map::new();
    if let Some(m) = model {
        body.insert("model".to_string(), json!(m));
    }
    body.insert("state".to_string(), Value::Object(state));
    body.insert("questions".to_string(), questions);
    Value::Object(body)
}

/// Map a `/v1/systemone` response to a [`Decision`] under the confidence floor. Pure,
/// so it is unit-tested against canned JSON. Any missing/al­ternate field defers safely.
pub fn interpret(resp: &Value, intents: &[String], min_confidence: f64) -> Decision {
    let answers = match resp.get("answers") {
        Some(a) => a,
        None => return Decision::Defer,
    };

    // The open-ended escape hatch: a high `needs_full_understanding` noul means the
    // turn needs System-2. A `noul` answer carries only the probability (0..1) — no
    // confidence field — so we gate on that probability alone; a missing answer defers.
    let nfu_p_true = answers
        .get(NEEDS_FULL)
        .and_then(|a| a.get("noul"))
        .and_then(Value::as_f64);
    if nfu_p_true.map(|p| p >= 0.5).unwrap_or(true) {
        return Decision::Defer; // needs System-2 (or the answer was missing)
    }

    // Temporal defer-gate (§16): no fast historical-data path, so a turn the model is
    // *confident* concerns the PAST goes to System-2. present/future proceed; an unsure
    // temporal never over-defers a present query (preserves weather/timer recall). The
    // question is optional — a checkpoint that doesn't answer it just skips this gate.
    if let Some(t) = answers.get(TEMPORAL) {
        let is_past = t.get("choice").and_then(Value::as_str) == Some(TEMPORAL_PAST);
        if is_past && choice_confidence(Some(t)) >= min_confidence {
            return Decision::Defer;
        }
    }

    let intent_ans = match answers.get("intent") {
        Some(a) => a,
        None => return Decision::Defer,
    };
    let label = match intent_ans.get("choice").and_then(Value::as_str) {
        Some(l) => l,
        None => return Decision::Defer,
    };
    if label == "other" || !intents.iter().any(|i| i == label) {
        return Decision::Defer;
    }
    let intent_conf = choice_confidence(Some(intent_ans));

    // Strict gate: the chosen intent must clear the floor.
    if intent_conf >= min_confidence {
        Decision::Resolve(Resolution {
            intent: label.to_string(),
            confidence: intent_conf,
        })
    } else {
        Decision::Defer
    }
}

/// Read the `names_place` noul's P(true), or 0.0 when the answer is absent.
fn names_place_prob(resp: &Value) -> f64 {
    resp.get("answers")
        .and_then(|a| a.get(NAMES_PLACE))
        .and_then(|a| a.get("noul"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

/// Whether a resolved decision must be re-routed to System-2 because the turn named a
/// specific place a [place-sensitive intent](PLACE_SENSITIVE_INTENTS) can't fast-path.
/// System-1 flags *that* a place was named but can't extract the string, and the fast-path
/// handler only knows the home location / local clock — so resolving here would answer for
/// the wrong place (wrong forecast, or the wrong timezone).
fn names_place_forces_defer(decision: &Decision, resp: &Value) -> bool {
    match decision {
        Decision::Resolve(r) => {
            PLACE_SENSITIVE_INTENTS.contains(&r.intent.as_str())
                && names_place_prob(resp) >= NAMES_PLACE_THRESHOLD
        }
        _ => false,
    }
}

/// Read a `choice`/`score` answer's calibrated `confidence` (0.0 when absent). The
/// OpenRouter Jev `/v1/systemone` response names this field `confidence`; a local
/// `laya-serve` build that instead emits `answer_confidence` is also accepted. `noul`
/// answers have no confidence field and are gated on their probability instead.
fn choice_confidence(answer: Option<&Value>) -> f64 {
    answer
        .and_then(|a| a.get("confidence").or_else(|| a.get("answer_confidence")))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intents() -> Vec<String> {
        vec!["weather".to_string(), "timer".to_string()]
    }

    #[test]
    fn build_request_shapes_state_and_questions() {
        let body = build_request(
            "show me the weather",
            Some("typesafe/jev-1.13"),
            &intents(),
            &Map::new(),
        );
        assert_eq!(body["model"], json!("typesafe/jev-1.13"));
        assert_eq!(body["state"]["message"], json!("show me the weather"));
        assert_eq!(body["questions"]["intent"]["type"], json!("choice"));
        // criteria carries every intent plus the `other` escape hatch.
        let crit = &body["questions"]["intent"]["criteria"];
        assert!(crit.get("weather").is_some());
        assert!(crit.get("timer").is_some());
        assert!(crit.get("other").is_some());
        assert_eq!(body["questions"][NEEDS_FULL]["type"], json!("noul"));
        // The temporal choice carries past/present/future.
        assert_eq!(body["questions"][TEMPORAL]["type"], json!("choice"));
        let tcrit = &body["questions"][TEMPORAL]["criteria"];
        assert!(tcrit.get("past").is_some());
        assert!(tcrit.get("present").is_some());
        assert!(tcrit.get("future").is_some());
        // laya-serve omits the model.
        let no_model = build_request("hi", None, &intents(), &Map::new());
        assert!(no_model.get("model").is_none());
        // With no device state, `state` is exactly `{ message }` (byte-identical to before).
        assert_eq!(no_model["state"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn device_state_fields_reports_screen_and_timers_only_when_present() {
        use crate::wyoming::protocol::TimerContext;
        // Idle screen, no timer → empty (ordinary turn stays byte-identical).
        let bare = DecisionRequest {
            transcript: "stop".into(),
            screen: None,
            history: Vec::new(),
            location: None,
            timers: TimerContext::default(),
        };
        assert!(device_state_fields(&bare).is_empty());

        // A recipe on screen with two timers running folds both into the state.
        let ctx = DecisionRequest {
            transcript: "stop".into(),
            screen: Some("recipe".into()),
            history: Vec::new(),
            location: None,
            timers: TimerContext {
                running: 2,
                next_remaining_secs: Some(90),
                labels: vec![],
            },
        };
        let fields = device_state_fields(&ctx);
        assert_eq!(fields.get("screen"), Some(&json!("recipe")));
        assert_eq!(fields.get("timers_running"), Some(&json!(2)));
        assert_eq!(fields.get("timer_remaining_secs"), Some(&json!(90)));

        // The fields ride inside the request `state` alongside `message`.
        let body = build_request("stop", None, &intents(), &fields);
        assert_eq!(body["state"]["message"], json!("stop"));
        assert_eq!(body["state"]["screen"], json!("recipe"));
        assert_eq!(body["state"]["timers_running"], json!(2));
    }

    /// Add a `temporal` choice answer to a response built by `resp`.
    fn with_temporal(mut resp: Value, label: &str, conf: f64) -> Value {
        resp["answers"][TEMPORAL] = json!({
            "type": "choice", "choice": label, "probabilities": {}, "confidence": conf,
        });
        resp
    }

    #[test]
    fn interpret_defers_on_confident_past() {
        // A past-tense turn defers even with a confident, closed intent (no fast history).
        let past = with_temporal(resp("weather", 0.97, 0.02), "past", 0.95);
        assert_eq!(interpret(&past, &intents(), 0.85), Decision::Defer);
    }

    #[test]
    fn interpret_ignores_low_confidence_past() {
        // Unsure temporal must NOT over-defer a confident present-ish query.
        let unsure = with_temporal(resp("weather", 0.97, 0.02), "past", 0.60);
        assert!(matches!(
            interpret(&unsure, &intents(), 0.85),
            Decision::Resolve(_)
        ));
    }

    #[test]
    fn interpret_resolves_present_and_future() {
        for tense in ["present", "future"] {
            let r = with_temporal(resp("weather", 0.97, 0.02), tense, 0.99);
            assert!(
                matches!(interpret(&r, &intents(), 0.85), Decision::Resolve(_)),
                "temporal={tense} should resolve"
            );
        }
    }

    /// A System One response in the real OpenRouter/laya-serve shape: the `choice`
    /// answer carries `confidence`; the `noul` answer carries only its probability.
    fn resp(intent: &str, intent_conf: f64, nfu_p: f64) -> Value {
        json!({
            "model": "typesafe/jev-1.13",
            "answers": {
                "intent": {
                    "type": "choice", "choice": intent,
                    "probabilities": {}, "confidence": intent_conf,
                },
                "needs_full_understanding": {
                    "type": "noul", "noul": nfu_p,
                },
            },
            "usage": {}
        })
    }

    /// A location intent that names a specific place resolves in `interpret` but is
    /// re-routed to System-2 by `names_place_forces_defer` (evaluated on the original
    /// transcript only, so the home-augmented retry can still resolve). No place named,
    /// or a non-location intent, is unaffected.
    #[test]
    fn names_place_reroutes_only_resolved_location_intents() {
        // Weather naming a place: interpret resolves, but the gate forces a defer.
        let mut naming = resp("weather", 0.99, 0.02);
        naming["answers"][NAMES_PLACE] = json!({ "type": "noul", "noul": 0.98 });
        let d = interpret(&naming, &intents(), 0.85);
        assert!(matches!(d, Decision::Resolve(_)));
        assert!(names_place_forces_defer(&d, &naming));

        // Weather, no place named → stays on the fast path.
        let mut homey = resp("weather", 0.99, 0.02);
        homey["answers"][NAMES_PLACE] = json!({ "type": "noul", "noul": 0.05 });
        let d = interpret(&homey, &intents(), 0.85);
        assert!(matches!(d, Decision::Resolve(_)));
        assert!(!names_place_forces_defer(&d, &homey));

        // A non-location intent (timer) that happens to name a place is not re-routed.
        let mut timer = resp("timer", 0.99, 0.02);
        timer["answers"][NAMES_PLACE] = json!({ "type": "noul", "noul": 0.98 });
        let d = interpret(&timer, &intents(), 0.85);
        assert!(!names_place_forces_defer(&d, &timer));

        // A deferred decision is never force-deferred (nothing to re-route).
        assert!(!names_place_forces_defer(&Decision::Defer, &naming));
    }

    #[test]
    fn interpret_resolves_confident_closed_intent() {
        let d = interpret(&resp("weather", 0.97, 0.02), &intents(), 0.85);
        match d {
            Decision::Resolve(r) => {
                assert_eq!(r.intent, "weather");
                assert!((r.confidence - 0.97).abs() < 1e-9);
            }
            Decision::Defer => panic!("expected Resolve"),
        }
    }

    #[test]
    fn interpret_defers_on_low_confidence() {
        assert_eq!(
            interpret(&resp("weather", 0.60, 0.02), &intents(), 0.85),
            Decision::Defer
        );
    }

    #[test]
    fn interpret_defers_when_needs_full_understanding() {
        assert_eq!(
            interpret(&resp("weather", 0.97, 0.90), &intents(), 0.85),
            Decision::Defer
        );
    }

    #[test]
    fn interpret_defers_on_other_or_unknown_intent() {
        assert_eq!(
            interpret(&resp("other", 0.99, 0.01), &intents(), 0.85),
            Decision::Defer
        );
        assert_eq!(
            interpret(&resp("philosophy", 0.99, 0.01), &intents(), 0.85),
            Decision::Defer
        );
    }

    #[test]
    fn interpret_accepts_legacy_answer_confidence_from_laya_serve() {
        // A laya-serve build that emits `answer_confidence` instead of `confidence`
        // on the choice answer still resolves.
        let legacy = json!({
            "answers": {
                "intent": { "type": "choice", "choice": "weather",
                            "probabilities": {}, "answer_confidence": 0.96 },
                "needs_full_understanding": { "type": "noul", "noul": 0.02 },
            }
        });
        assert!(matches!(
            interpret(&legacy, &intents(), 0.85),
            Decision::Resolve(_)
        ));
    }

    #[test]
    fn interpret_defers_on_malformed_response() {
        assert_eq!(interpret(&json!({}), &intents(), 0.85), Decision::Defer);
        assert_eq!(
            interpret(&json!({"answers": {}}), &intents(), 0.85),
            Decision::Defer
        );
    }
}
