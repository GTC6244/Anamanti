//! Offline System-1 calibration harness (plans/system1-fast-decisions.md §18, M5.3).
//!
//! This is a **measurement tool, not a pass/fail gate** — it is `#[ignore]`d so it never
//! runs in CI (there is no live decision model there). Point it at a running backend to
//! measure resolve precision/recall on the §18 corpus and pick `min_confidence` (and to
//! answer the open question of whether a single global threshold suffices, or per-intent
//! thresholds are needed — §13 Q4).
//!
//! It exercises the **System-1 classifier** (`engine.decide`) — Resolve-vs-Defer and the
//! routed intent — NOT the orchestrator's `stop_dismiss` ladder (that is unit-tested in
//! `tests/pipeline.rs`). The 3B device-state fold is exercised by passing per-case screen
//! / timer context.
//!
//! Run (local laya-serve on :8000):
//!   cargo test --manifest-path anamanti-core/Cargo.toml --test system1_calibration \
//!     -- --ignored --nocapture
//! Run (OpenRouter Jev):
//!   SYSTEM1_MODEL=typesafe/jev-1.13 SYSTEM1_BASE_URL=https://openrouter.ai/api \
//!   OPENROUTER_API_KEY=sk-... cargo test ... --test system1_calibration -- --ignored --nocapture
//!
//! Env: SYSTEM1_BASE_URL (default http://127.0.0.1:8000), SYSTEM1_MODEL (empty → laya-serve,
//! set → jev), OPENROUTER_API_KEY (jev auth), SYSTEM1_MIN_CONFIDENCE (default 0.85),
//! SYSTEM1_HOME (default "Austin, Texas", grounds location intents).

use anamanti_core::system1::{build, Decision, DecisionRequest};
use anamanti_core::wyoming::protocol::TimerContext;

/// One labelled utterance. `expect` is the classifier-level expectation.
struct Case {
    utterance: &'static str,
    expect: Expect,
    /// Foreground widget label ("recipe"/"weather"), folded into the request state.
    screen: Option<&'static str>,
    /// Running-timer count for this turn's device context.
    timers_running: u32,
}

#[derive(Clone, Copy, PartialEq)]
enum Expect {
    /// Should resolve; the routed intent must be one of these labels.
    Resolve(&'static [&'static str]),
    /// Must defer to System-2 (a mis-resolve here is worse than a slow-correct answer).
    Defer,
}

fn case(utterance: &'static str, expect: Expect) -> Case {
    Case {
        utterance,
        expect,
        screen: None,
        timers_running: 0,
    }
}

fn with_screen(mut c: Case, screen: &'static str) -> Case {
    c.screen = Some(screen);
    c
}

fn with_timer(mut c: Case, running: u32) -> Case {
    c.timers_running = running;
    c
}

/// The §18 calibration corpus, classifier-level. Keep the ✗ (must-defer) set strict — it
/// anchors precision.
fn corpus() -> Vec<Case> {
    use Expect::*;
    let weather: &[&str] = &["weather"];
    let timer_start: &[&str] = &["timer"];
    let time: &[&str] = &["time"];
    let date: &[&str] = &["date"];
    let tcancel: &[&str] = &["timer_cancel", "stop_dismiss"];
    let tquery: &[&str] = &["timer_query"];
    let wdismiss: &[&str] = &["weather_dismiss", "stop_dismiss"];
    let rdismiss: &[&str] = &["recipe_dismiss", "stop_dismiss"];
    let endsess: &[&str] = &["end_session", "stop_dismiss"];

    vec![
        // weather (present/future resolve; past + named place defer)
        case("what's the weather", Resolve(weather)),
        case("is it going to rain today", Resolve(weather)),
        case("what's the forecast this weekend", Resolve(weather)),
        case("what was the weather yesterday", Defer),
        case("what's the weather in Tokyo", Defer),
        // timer start
        case("set a timer for ten minutes", Resolve(timer_start)),
        case("ninety second timer", Resolve(timer_start)),
        // "set a timer" is correctly CLASSIFIED as timer here; the orchestrator's handler
        // then defers because `parse_duration_secs` finds no duration (unit-tested in
        // tests/pipeline.rs). So classifier-level this is a resolve, not a defer.
        case("set a timer", Resolve(timer_start)),
        // clock
        case("what time is it", Resolve(time)),
        case("what's the date", Resolve(date)),
        case("what time is it in London", Defer),
        // timer cancel / query (with a running timer in context)
        with_timer(case("cancel my timer", Resolve(tcancel)), 1),
        with_timer(case("how much time is left", Resolve(tquery)), 1),
        // screen dismiss (with that screen up)
        with_screen(case("close the weather", Resolve(wdismiss)), "weather"),
        with_screen(case("close the recipe", Resolve(rdismiss)), "recipe"),
        // end session
        case("that's all thanks", Resolve(endsess)),
        case("nothing else", Resolve(endsess)),
        // must-defer precision anchors
        case("who was the sixteenth president", Defer),
        case("what's fifteen percent of two hundred forty", Defer),
        case("add milk to the shopping list", Defer),
        case("find me a lasagna recipe", Defer),
        case("how do I get to the airport", Defer),
    ]
}

#[tokio::test]
#[ignore = "calibration tool: needs a live System-1 backend; run with --ignored"]
async fn calibrate_against_corpus() {
    let base_url =
        std::env::var("SYSTEM1_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8000".to_string());
    let model = std::env::var("SYSTEM1_MODEL").unwrap_or_default();
    let api_key = std::env::var("OPENROUTER_API_KEY").ok();
    let backend = if model.is_empty() {
        "laya-serve"
    } else {
        "jev"
    };
    let min_conf: f64 = std::env::var("SYSTEM1_MIN_CONFIDENCE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.85);
    let home = std::env::var("SYSTEM1_HOME").unwrap_or_else(|_| "Austin, Texas".to_string());

    let engine = build(backend, &base_url, &model, api_key, min_conf, vec![])
        .expect("build System-1 engine");

    println!(
        "\n=== System-1 calibration ({backend} @ {base_url}, min_confidence={min_conf}) ===\n"
    );

    let cases = corpus();
    assert!(!cases.is_empty(), "corpus must not be empty");

    // Confusion tallies. "resolve" = the positive class.
    let (mut correct, mut mis_resolve, mut missed_resolve, mut wrong_intent) = (0, 0, 0, 0);

    for c in &cases {
        let req = DecisionRequest {
            transcript: c.utterance.to_string(),
            screen: c.screen.map(String::from),
            history: Vec::new(),
            location: Some(home.clone()),
            timers: TimerContext {
                running: c.timers_running,
                next_remaining_secs: (c.timers_running > 0).then_some(300),
                labels: Vec::new(),
            },
        };
        let decision = engine.decide(&req).await.unwrap_or(Decision::Defer);
        let (got_resolve, got_intent) = match &decision {
            Decision::Resolve(r) => (true, r.intent.clone()),
            Decision::Defer => (false, String::new()),
        };

        let verdict = match c.expect {
            Expect::Defer => {
                if got_resolve {
                    mis_resolve += 1;
                    "MIS-RESOLVE ✗ (should defer)"
                } else {
                    correct += 1;
                    "ok (defer)"
                }
            }
            Expect::Resolve(allowed) => {
                if !got_resolve {
                    missed_resolve += 1;
                    "missed (deferred)"
                } else if allowed.contains(&got_intent.as_str()) {
                    correct += 1;
                    "ok (resolve)"
                } else {
                    wrong_intent += 1;
                    "WRONG-INTENT ✗"
                }
            }
        };
        println!(
            "  [{:<24}] {:<40} -> {}",
            if got_resolve {
                got_intent
            } else {
                "defer".to_string()
            },
            format!("\"{}\"", c.utterance),
            verdict
        );
    }

    let total = cases.len();
    println!("\n--- summary ---");
    println!("  correct:        {correct}/{total}");
    println!("  mis-resolves:   {mis_resolve}  (defer cases wrongly resolved — the precision bar)");
    println!("  wrong-intent:   {wrong_intent}");
    println!("  missed resolves:{missed_resolve}  (resolve cases that deferred — recall)");
    println!(
        "\nTune SYSTEM1_MIN_CONFIDENCE up until mis-resolves reach 0, then read recall.\n\
         If one global threshold can't zero mis-resolves without gutting recall, that is the\n\
         signal for per-intent thresholds (§13 Q4).\n"
    );

    // The harness itself (not the model) is what we assert: every case produced a decision.
    assert_eq!(
        correct + mis_resolve + wrong_intent + missed_resolve,
        total,
        "every case must be classified"
    );
}
