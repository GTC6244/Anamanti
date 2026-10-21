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
//! set → jev), OPENROUTER_API_KEY (jev auth), SYSTEM1_MIN_CONFIDENCE (default 0.85).

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
        // a bare standalone sign-off thanks ends the session (any language)
        case("thank you", Resolve(endsess)),
        case("thanks", Resolve(endsess)),
        case("gracias", Resolve(endsess)),
        case("merci", Resolve(endsess)),
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
            timers: TimerContext {
                running: c.timers_running,
                next_remaining_secs: (c.timers_running > 0).then_some(300),
                labels: Vec::new(),
            },
            speaker: Default::default(),
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

// ---------------------------------------------------------------- Portuguese (pt-BR)
//
// plans/BilingualPlan.md §2.7: System-1 answers a Portuguese turn only for intents
// listed in `system1::PT_BR_READY`. Like the English harness, **precision is the bar**:
// an intent qualifies when, on the live backend at the production `min_confidence`, at
// least one of its pt cases resolves correctly and **nothing** resolves *into* it wrongly
// — no near-miss, no must-defer anchor, no other intent's utterance. A missed resolve
// (deferred) is only a recall cost: System-2 still answers it, in Portuguese.
// `calibrate_portuguese` prints that verdict per intent.
//
// Run (production jev on OpenRouter):
//   SYSTEM1_MODEL=typesafe/jev-1.13 SYSTEM1_BASE_URL=https://openrouter.ai/api/ \
//   OPENROUTER_API_KEY=sk-... cargo test --manifest-path anamanti-core/Cargo.toml \
//     --test system1_calibration calibrate_portuguese -- --ignored --nocapture

/// One pt case: the intent it exercises (`""` = a global must-defer anchor).
struct PtCase {
    intent: &'static str,
    c: Case,
}

fn pt(intent: &'static str, c: Case) -> PtCase {
    PtCase { intent, c }
}

/// The Portuguese corpus, mirroring the English §18 corpus.
fn pt_corpus() -> Vec<PtCase> {
    use Expect::*;
    let weather: &[&str] = &["weather"];
    let timer_start: &[&str] = &["timer"];
    let time: &[&str] = &["time"];
    let date: &[&str] = &["date"];
    let tcancel: &[&str] = &["timer_cancel", "stop_dismiss"];
    let tquery: &[&str] = &["timer_query"];
    let tadjust: &[&str] = &["timer_adjust"];
    let wdismiss: &[&str] = &["weather_dismiss", "stop_dismiss"];
    let rdismiss: &[&str] = &["recipe_dismiss", "stop_dismiss"];
    let pdismiss: &[&str] = &["place_dismiss", "stop_dismiss"];
    let endsess: &[&str] = &["end_session", "stop_dismiss"];
    let stop: &[&str] = &["stop_dismiss", "end_session"];
    vec![
        pt("weather", case("como está o tempo", Resolve(weather))),
        pt("weather", case("vai chover hoje", Resolve(weather))),
        pt(
            "weather",
            case(
                "qual é a previsão do tempo para o fim de semana",
                Resolve(weather),
            ),
        ),
        pt("weather", case("como estava o tempo ontem", Defer)),
        pt("weather", case("como está o tempo em Tóquio", Defer)),
        pt(
            "timer",
            case("coloca um timer de dez minutos", Resolve(timer_start)),
        ),
        pt(
            "timer",
            case("timer de noventa segundos", Resolve(timer_start)),
        ),
        pt(
            "timer",
            case("programa um timer de cinco minutos", Resolve(timer_start)),
        ),
        pt("time", case("que horas são", Resolve(time))),
        pt("time", case("você sabe que horas são", Resolve(time))),
        pt("time", case("que horas são em Londres", Defer)),
        pt("date", case("que dia é hoje", Resolve(date))),
        pt("date", case("qual é a data de hoje", Resolve(date))),
        pt(
            "timer_cancel",
            with_timer(case("cancela o timer", Resolve(tcancel)), 1),
        ),
        pt(
            "timer_cancel",
            with_timer(case("desliga o timer", Resolve(tcancel)), 1),
        ),
        pt(
            "timer_query",
            with_timer(case("quanto tempo falta", Resolve(tquery)), 1),
        ),
        pt(
            "timer_query",
            with_timer(case("quanto falta no timer", Resolve(tquery)), 1),
        ),
        pt(
            "timer_adjust",
            with_timer(case("adiciona cinco minutos ao timer", Resolve(tadjust)), 1),
        ),
        pt(
            "timer_adjust",
            with_timer(case("tira um minuto do timer", Resolve(tadjust)), 1),
        ),
        pt(
            "weather_dismiss",
            with_screen(
                case("fecha a previsão do tempo", Resolve(wdismiss)),
                "weather",
            ),
        ),
        pt(
            "weather_dismiss",
            with_screen(case("fecha a tela do tempo", Resolve(wdismiss)), "weather"),
        ),
        pt(
            "weather_dismiss",
            with_screen(case("pode fechar o tempo", Resolve(wdismiss)), "weather"),
        ),
        pt(
            "recipe_dismiss",
            with_screen(case("fecha a receita", Resolve(rdismiss)), "recipe"),
        ),
        pt(
            "recipe_dismiss",
            with_screen(case("pode fechar a receita", Resolve(rdismiss)), "recipe"),
        ),
        pt(
            "place_dismiss",
            with_screen(case("fecha o lugar", Resolve(pdismiss)), "place"),
        ),
        pt(
            "place_dismiss",
            with_screen(case("fecha esse cartão", Resolve(pdismiss)), "place"),
        ),
        pt("end_session", case("é só isso, obrigado", Resolve(endsess))),
        pt("end_session", case("mais nada", Resolve(endsess))),
        pt("end_session", case("obrigado", Resolve(endsess))),
        pt("end_session", case("valeu", Resolve(endsess))),
        // A thanks that continues into a request is NOT a sign-off: it must route to the
        // request (weather), never end the session.
        pt(
            "end_session",
            case(
                "obrigado, agora qual é a previsão do tempo",
                Resolve(weather),
            ),
        ),
        pt("stop_dismiss", case("para", Resolve(stop))),
        pt("stop_dismiss", case("deixa pra lá", Resolve(stop))),
        pt("stop_dismiss", case("cancela", Resolve(stop))),
        pt("stop_dismiss", case("esquece", Resolve(stop))),
        // global must-defer precision anchors
        pt(
            "",
            case(
                "quem foi o décimo sexto presidente dos Estados Unidos",
                Defer,
            ),
        ),
        pt(
            "",
            case("quanto é quinze por cento de duzentos e quarenta", Defer),
        ),
        pt("", case("coloca leite na lista de compras", Defer)),
        pt("", case("me acha uma receita de lasanha", Defer)),
        pt("", case("como eu chego no aeroporto", Defer)),
        pt("", case("me conta uma piada", Defer)),
        pt("", case("liga a luz da cozinha", Defer)),
    ]
}

#[tokio::test]
#[ignore = "calibration tool: needs a live System-1 backend; run with --ignored"]
async fn calibrate_portuguese() {
    use std::collections::BTreeMap;
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
    let engine = build(backend, &base_url, &model, api_key, min_conf, vec![])
        .expect("build System-1 engine");
    println!("\n=== System-1 pt-BR calibration ({backend}, min_confidence={min_conf}) ===\n");

    // Per intent: correct resolves, precision failures (wrong resolves INTO it), and
    // recall misses (its cases that deferred).
    #[derive(Default)]
    struct Tally {
        correct: usize,
        wrong_into: Vec<String>,
        missed: Vec<String>,
    }
    let mut tally: BTreeMap<String, Tally> = BTreeMap::new();
    for pc in pt_corpus() {
        if !pc.intent.is_empty() {
            tally.entry(pc.intent.to_string()).or_default();
        }
    }
    for pc in pt_corpus() {
        let c = &pc.c;
        let req = DecisionRequest {
            transcript: c.utterance.to_string(),
            screen: c.screen.map(String::from),
            timers: TimerContext {
                running: c.timers_running,
                next_remaining_secs: (c.timers_running > 0).then_some(300),
                labels: Vec::new(),
            },
            speaker: Default::default(),
        };
        let decision = engine.decide(&req).await.unwrap_or(Decision::Defer);
        let got = match &decision {
            Decision::Resolve(r) => Some((r.intent.clone(), r.confidence)),
            Decision::Defer => None,
        };
        let note = match (c.expect, &got) {
            (Expect::Resolve(allowed), Some((i, _))) if allowed.contains(&i.as_str()) => {
                tally.entry(i.clone()).or_default().correct += 1;
                "ok".to_string()
            }
            (Expect::Resolve(_), Some((i, _))) | (Expect::Defer, Some((i, _))) => {
                let msg = format!("{:?} resolved as {i}", c.utterance);
                tally
                    .entry(i.clone())
                    .or_default()
                    .wrong_into
                    .push(msg.clone());
                format!("WRONG ✗ ({msg})")
            }
            (Expect::Resolve(_), None) => {
                if !pc.intent.is_empty() {
                    tally
                        .entry(pc.intent.to_string())
                        .or_default()
                        .missed
                        .push(c.utterance.to_string());
                }
                "missed (deferred → System-2)".to_string()
            }
            (Expect::Defer, None) => "ok (defer)".to_string(),
        };
        println!(
            "  [{:<17}] {:<52} -> {note}",
            got.as_ref()
                .map_or("defer".to_string(), |(i, c)| format!("{i} {c:.2}")),
            format!("\"{}\"", c.utterance),
        );
    }
    println!("\n--- per-intent verdict (READY ⇒ list it in system1::PT_BR_READY) ---");
    let mut ready = Vec::new();
    for (intent, t) in &tally {
        if t.correct > 0 && t.wrong_into.is_empty() {
            ready.push(intent.clone());
            println!(
                "  READY      {intent} ({} ok{})",
                t.correct,
                if t.missed.is_empty() {
                    String::new()
                } else {
                    format!(", missed: {:?}", t.missed)
                }
            );
        } else {
            println!(
                "  NOT READY  {intent} ({} ok; wrong into it: {:?}; missed: {:?})",
                t.correct, t.wrong_into, t.missed
            );
        }
    }
    println!("\nPT_BR_READY candidates: {ready:?}\n");
}
