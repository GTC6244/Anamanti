//! End-to-end tests of the rig backend against loopback provider servers (no real
//! network): the tool loop, dispatch, and device-action relay.

use std::sync::Arc;

use super::tools::test_support::*;
use super::*;
use crate::cadora::CadoraCommand;
use crate::llm::{collect_reply, DeviceAction};
use crate::music::{SearchKind, SpotifyCommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A recorded-requests handle from [`serve`].
type Requests = Arc<std::sync::Mutex<Vec<String>>>;

/// Serve a fixed sequence of raw `200 OK` HTTP responses (one per inbound
/// connection) with the given content type, recording each request.
fn serve(
    content_type: &'static str,
    bodies: Vec<String>,
) -> (String, Requests, tokio::task::JoinHandle<()>) {
    serve_with(
        bodies
            .into_iter()
            .map(|b| ("200 OK", content_type, b))
            .collect(),
    )
}

/// Like [`serve`], with a status line and content type per response.
fn serve_with(
    responses: Vec<(&'static str, &'static str, String)>,
) -> (String, Requests, tokio::task::JoinHandle<()>) {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let handle = tokio::spawn(async move {
        let listener = TcpListener::from_std(std_listener).unwrap();
        for (status, content_type, body) in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            let request = read_request(&mut sock).await;
            seen.lock().unwrap().push(request);
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        }
    });
    (format!("http://{addr}"), requests, handle)
}

/// Read one whole HTTP request (headers + `Content-Length` body).
async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = sock.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf);
        if let Some(end) = text.find("\r\n\r\n") {
            let len = text[..end]
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if buf.len() >= end + 4 + len {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// An Ollama `/api/chat` record calling `name(args)`.
fn ollama_tool_call(name: &str, args: serde_json::Value) -> String {
    serde_json::json!({
        "model": "m", "created_at": "t",
        "message": {"role": "assistant", "content": "",
                    "tool_calls": [{"function": {"name": name, "arguments": args}}]},
        "done": true, "done_reason": "stop"
    })
    .to_string()
        + "\n"
}

/// An Ollama `/api/chat` final text record.
fn ollama_text(text: &str) -> String {
    serde_json::json!({
        "model": "m", "created_at": "t",
        "message": {"role": "assistant", "content": text},
        "done": true, "done_reason": "stop"
    })
    .to_string()
        + "\n"
}

/// Run one turn on a rig Ollama backend served by `bodies`; returns the reply.
async fn ollama_turn(tools: ToolDeps, bodies: Vec<String>, turn: LlmTurn) -> String {
    let (url, _requests, server) = serve("application/x-ndjson", bodies);
    let backend = RigBackend::ollama(&url, "test-model", Arc::new(Tools::build(tools))).unwrap();
    let reply = collect_reply(backend.respond(turn).await.unwrap())
        .await
        .unwrap();
    server.await.unwrap();
    reply
}

/// Round 0 the fake Ollama asks for `internet_search`; round 1 (after the tool result
/// is threaded back) it answers. The search result reaches the model.
#[tokio::test]
async fn rig_ollama_runs_search_tool_then_answers() {
    let (url, requests, server) = serve(
        "application/x-ndjson",
        vec![
            ollama_tool_call(
                "internet_search",
                serde_json::json!({"query": "weather in paris"}),
            ),
            ollama_text("It is sunny in Paris."),
        ],
    );
    let backend = RigBackend::ollama(
        &url,
        "test-model",
        Arc::new(tools_with_search("Sunny, 21C.")),
    )
    .unwrap();
    let reply = collect_reply(
        backend
            .respond(LlmTurn::new("sys", "what is the weather in paris"))
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    server.await.unwrap();
    assert_eq!(reply, "It is sunny in Paris.");
    assert!(requests.lock().unwrap()[1].contains("Sunny, 21C."));
}

#[tokio::test]
async fn rig_ollama_runs_calendar_tool_then_answers() {
    use chrono::{Duration, Local};
    let start = Local::now() + Duration::hours(2);
    let event = crate::calendar::CalEvent {
        calendar: "Work".into(),
        summary: "1:1".into(),
        start,
        end: Some(start + Duration::hours(1)),
        all_day: false,
        location: None,
        organizer: None,
        attendees: vec!["Alice".into()],
        description: None,
    };
    let deps = ToolDeps {
        calendar: Some(Arc::new(StaticCalendar(vec![event]))),
        ..Default::default()
    };
    let reply = ollama_turn(
        deps,
        vec![
            ollama_tool_call(
                "calendar_lookup",
                serde_json::json!({"when": "today", "person": "Alice"}),
            ),
            ollama_text("You have a 1:1 with Alice at 2 PM."),
        ],
        LlmTurn::new("sys", "am I seeing Alice today?"),
    )
    .await;
    assert_eq!(reply, "You have a 1:1 with Alice at 2 PM.");
}

/// The missing origin defaults to the configured home location.
#[tokio::test]
async fn rig_ollama_runs_directions_tool_with_home_default() {
    let provider = Arc::new(StaticDirections::default());
    let deps = ToolDeps {
        home_location: home("Home, Austin"),
        directions: Some((provider.clone(), true)),
        ..Default::default()
    };
    let reply = ollama_turn(
        deps,
        vec![
            ollama_tool_call(
                "directions_lookup",
                serde_json::json!({"destination": "the airport"}),
            ),
            ollama_text("About 20 minutes to the airport — traffic is heavy."),
        ],
        LlmTurn::new("sys", "how long to the airport?"),
    )
    .await;
    assert_eq!(reply, "About 20 minutes to the airport — traffic is heavy.");
    assert_eq!(
        provider.seen_origin.lock().unwrap().as_deref(),
        Some("Home, Austin")
    );
}

#[tokio::test]
async fn rig_ollama_runs_phone_tool_then_answers() {
    let messenger = Arc::new(StaticPhone::default());
    let deps = ToolDeps {
        appsaid: Some(messenger.clone()),
        ..Default::default()
    };
    let reply = ollama_turn(
        deps,
        vec![
            ollama_tool_call(
                "send_phone_message",
                serde_json::json!({"recipient": "Mom", "message": "the restaurant", "place": "Blue Bottle, Oakland"}),
            ),
            ollama_text("Sent it to Mom."),
        ],
        LlmTurn::new("sys", "text Mom the restaurant"),
    )
    .await;
    assert_eq!(reply, "Sent it to Mom.");
    let sent = messenger.last.lock().unwrap().clone().unwrap();
    assert_eq!(sent.recipient.as_deref(), Some("Mom"));
    assert_eq!(sent.message, "the restaurant");
    assert_eq!(sent.place.as_deref(), Some("Blue Bottle, Oakland"));
}

#[tokio::test]
async fn rig_ollama_runs_spotify_tool_then_answers() {
    let controller = Arc::new(StaticSpotify::default());
    let deps = ToolDeps {
        spotify: Some(controller.clone()),
        ..Default::default()
    };
    let reply = ollama_turn(
        deps,
        vec![
            ollama_tool_call(
                "spotify_control",
                serde_json::json!({"action": "play", "query": "Radiohead", "kind": "artist"}),
            ),
            ollama_text("Playing Radiohead now."),
        ],
        LlmTurn::new("sys", "play some Radiohead"),
    )
    .await;
    assert_eq!(reply, "Playing Radiohead now.");
    assert_eq!(
        controller.last.lock().unwrap().clone(),
        Some(SpotifyCommand::Play {
            query: Some("Radiohead".into()),
            kind: SearchKind::Artist
        })
    );
}

#[tokio::test]
async fn rig_ollama_runs_shopping_tool_then_answers() {
    let controller = Arc::new(StaticCadora::default());
    let deps = ToolDeps {
        cadora: Some(controller.clone()),
        ..Default::default()
    };
    let reply = ollama_turn(
        deps,
        vec![
            ollama_tool_call(
                "shopping_list_add",
                serde_json::json!({"item": "milk", "quantity": 2}),
            ),
            ollama_text("Done — 2 milk are on the list."),
        ],
        LlmTurn::new("sys", "add two milk to the shopping list"),
    )
    .await;
    assert_eq!(reply, "Done — 2 milk are on the list.");
    assert_eq!(
        controller.last.lock().unwrap().clone(),
        Some(CadoraCommand::AddItem {
            item: "milk".into(),
            quantity: Some(2),
        })
    );
}

#[tokio::test]
async fn rig_ollama_runs_calendar_add_then_answers() {
    let controller = Arc::new(StaticCadora::default());
    let deps = ToolDeps {
        cadora: Some(controller.clone()),
        ..Default::default()
    };
    let reply = ollama_turn(
        deps,
        vec![
            ollama_tool_call(
                "calendar_add",
                serde_json::json!({"title": "Pizza day", "date": "2026-10-23"}),
            ),
            ollama_text("Pizza day is on the calendar."),
        ],
        LlmTurn::new("sys", "friday is pizza day, put it on the calendar"),
    )
    .await;
    assert_eq!(reply, "Pizza day is on the calendar.");
    let last = controller.last.lock().unwrap().clone();
    match last {
        Some(CadoraCommand::AddEvent(event)) => {
            assert_eq!(event.title, "Pizza day");
            assert_eq!(event.starts_at, "2026-10-23");
            assert!(event.all_day);
        }
        other => panic!("expected an AddEvent, got {other:?}"),
    }
}

/// The fake Ollama asks for `set_timer`; the pipeline's action sink receives the
/// `StartTimer`, and the follow-up answer comes back.
#[tokio::test]
async fn rig_ollama_runs_set_timer_and_relays_the_action() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let reply = ollama_turn(
        ToolDeps::default(),
        vec![
            ollama_tool_call(
                "set_timer",
                serde_json::json!({"duration_seconds": 300, "label": "pasta"}),
            ),
            ollama_text("Your pasta timer is set for five minutes."),
        ],
        LlmTurn::new("sys", "set a 5 minute pasta timer").with_actions(tx),
    )
    .await;
    assert_eq!(reply, "Your pasta timer is set for five minutes.");
    assert_eq!(
        rx.try_recv().unwrap(),
        DeviceAction::StartTimer {
            label: Some("pasta".to_string()),
            duration_secs: 300
        }
    );
}

// ===========================================================================
// Turn loop over a scripted model: streaming, joining, the round cap
// ===========================================================================

/// One scripted round: text deltas and tool calls, in order.
type Script = Vec<RoundEvent>;

/// A [`RoundModel`] that replays scripted rounds and records every request.
struct ScriptedModel {
    rounds: std::sync::Mutex<std::collections::VecDeque<Script>>,
    /// Repeat this round forever once `rounds` runs out (for the cap tests).
    forever: Option<fn() -> Script>,
    streams: bool,
    honours_none: bool,
    requests: std::sync::Mutex<Vec<RoundRequest>>,
    stream_calls: std::sync::atomic::AtomicUsize,
}

impl ScriptedModel {
    fn new(rounds: Vec<Script>) -> Self {
        Self {
            rounds: std::sync::Mutex::new(rounds.into()),
            forever: None,
            streams: true,
            honours_none: true,
            requests: Default::default(),
            stream_calls: Default::default(),
        }
    }

    fn next(&self, req: RoundRequest) -> Script {
        self.requests.lock().unwrap().push(req);
        self.rounds
            .lock()
            .unwrap()
            .pop_front()
            .or_else(|| self.forever.map(|f| f()))
            .expect("scripted model ran out of rounds")
    }
}

#[async_trait]
impl RoundModel for ScriptedModel {
    fn streams_tool_calls(&self) -> bool {
        self.streams
    }
    fn honours_tool_choice_none(&self) -> bool {
        self.honours_none
    }
    async fn stream(&self, req: RoundRequest) -> Result<RoundStream> {
        self.stream_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let events = self.next(req);
        Ok(Box::pin(futures_util::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
    async fn complete(&self, req: RoundRequest) -> Result<(String, Vec<ToolCall>)> {
        let mut text = String::new();
        let mut calls = Vec::new();
        for event in self.next(req) {
            match event {
                RoundEvent::Text(t) => text.push_str(&t),
                RoundEvent::ToolCall(c) => calls.push(c),
            }
        }
        Ok((text.trim().to_string(), calls))
    }
}

fn text(t: &str) -> RoundEvent {
    RoundEvent::Text(t.to_string())
}

fn call(id: &str, name: &str, args: serde_json::Value) -> RoundEvent {
    RoundEvent::ToolCall(ToolCall::new(
        rig_core::message::ToolCallId::new_or_mint(id),
        rig_core::message::ToolFunction::new(name.to_string(), args),
    ))
}

fn timer_call() -> Script {
    vec![call(
        "c",
        "set_timer",
        serde_json::json!({"duration_seconds": 60}),
    )]
}

/// Run a turn on `model`, returning every yielded chunk.
async fn chunks(model: Arc<ScriptedModel>, turn: LlmTurn) -> Vec<String> {
    let backend = RigBackend::with_model(
        "scripted",
        model,
        0,
        Arc::new(Tools::build(ToolDeps::default())),
    );
    let mut stream = backend.respond(turn).await.unwrap();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.push(chunk.unwrap());
    }
    out
}

#[tokio::test]
async fn a_plain_text_turn_streams_token_by_token() {
    let model = Arc::new(ScriptedModel::new(vec![vec![
        text("Hel"),
        text("lo"),
        text(" there."),
    ]]));
    let out = chunks(model.clone(), LlmTurn::new("sys", "hi")).await;
    assert_eq!(out, vec!["Hel", "lo", " there."]);
    // Streamed, with the tools advertised and the guidance in the preamble.
    assert_eq!(
        model.stream_calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let req = &model.requests.lock().unwrap()[0];
    assert_eq!(req.tools.len(), 5);
    assert!(!req.forbid_tools);
    assert!(req
        .preamble
        .starts_with("sys\n\nYou can start, cancel, and adjust"));
}

#[tokio::test]
async fn a_tool_turn_yields_lead_in_tool_effect_and_answer_exactly_once() {
    let model = Arc::new(ScriptedModel::new(vec![
        vec![
            text("Sure,"),
            text(" setting it."),
            call(
                "call_1",
                "set_timer",
                serde_json::json!({"duration_seconds": 300, "label": "pasta"}),
            ),
        ],
        vec![text("Your pasta timer"), text(" is running.")],
    ]));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let out = chunks(
        model.clone(),
        LlmTurn::new("sys", "pasta timer, 5 minutes").with_actions(tx),
    )
    .await;
    assert_eq!(
        out,
        vec!["Sure,", " setting it.", " Your pasta timer", " is running."]
    );
    // The tool ran exactly once.
    assert_eq!(
        rx.try_recv().unwrap(),
        DeviceAction::StartTimer {
            label: Some("pasta".into()),
            duration_secs: 300
        }
    );
    assert!(rx.try_recv().is_err());
    // Round 2 saw the assistant's text + tool call, then the tool's result.
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let history = serde_json::to_string(&requests[1].messages).unwrap();
    assert!(history.contains("Sure, setting it."), "{history}");
    assert!(
        history.contains("Started a 5 minutes timer for pasta."),
        "{history}"
    );
}

#[tokio::test]
async fn a_non_streaming_provider_completes_tool_rounds_and_joins_text() {
    let mut model = ScriptedModel::new(vec![
        vec![
            text("One moment."),
            call("c", "cancel_timer", serde_json::json!({})),
        ],
        vec![text("All timers are cancelled.")],
    ]);
    model.streams = false;
    let model = Arc::new(model);
    let out = chunks(model.clone(), LlmTurn::new("sys", "cancel timers")).await;
    assert_eq!(out.concat(), "One moment. All timers are cancelled.");
    assert_eq!(
        model.stream_calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn hitting_the_round_cap_makes_one_final_round_with_tools_forbidden() {
    let mut model = ScriptedModel::new(Vec::new());
    model.forever = Some(timer_call);
    let model = Arc::new(model);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let out = chunks(model.clone(), LlmTurn::new("sys", "loop").with_actions(tx)).await;
    // The final round asked for tools again and said nothing → the spoken fallback.
    assert_eq!(out, vec![ROUND_CAP_FALLBACK]);
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), MAX_TOOL_ROUNDS + 1);
    assert!(requests[..MAX_TOOL_ROUNDS].iter().all(|r| !r.forbid_tools));
    let last = requests.last().unwrap();
    assert!(last.forbid_tools);
    assert_eq!(last.tools.len(), 5, "tools stay defined for the history");
}

#[tokio::test]
async fn the_final_round_answer_is_spoken_and_joined() {
    let mut rounds: Vec<Script> = (0..MAX_TOOL_ROUNDS).map(|_| timer_call()).collect();
    rounds[0].insert(0, text("Working on it."));
    rounds.push(vec![text("Here is what I found.")]);
    let mut model = ScriptedModel::new(rounds);
    model.honours_none = false;
    let model = Arc::new(model);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let out = chunks(model.clone(), LlmTurn::new("sys", "q").with_actions(tx)).await;
    assert_eq!(out.concat(), "Working on it. Here is what I found.");
    // A provider that ignores tool_choice gets no tools at all in the final round.
    let last = model.requests.lock().unwrap().last().unwrap().clone();
    assert!(last.tools.is_empty());
    assert!(!last.forbid_tools);
}

/// A tool whose error text embeds a secret query parameter.
struct LeakyTool;

#[async_trait]
impl tools::VoiceTool for LeakyTool {
    fn name(&self) -> &'static str {
        "leaky"
    }
    fn definition(&self) -> ToolDefinition {
        tools::definition("leaky", "test", serde_json::json!({"type": "object"}))
    }
    async fn invoke(&self, _args: &serde_json::Value, _ctx: &tools::ToolCtx<'_>) -> Result<String> {
        anyhow::bail!("GET https://x.test/v1?q=a&key=TOPSECRET failed")
    }
}

#[tokio::test]
async fn tool_errors_are_redacted_before_reaching_the_model() {
    let model = Arc::new(ScriptedModel::new(vec![
        vec![call("c", "leaky", serde_json::json!({}))],
        vec![text("Sorry.")],
    ]));
    let backend = RigBackend::with_model(
        "scripted",
        model.clone(),
        0,
        Arc::new(Tools::from_tools(vec![Arc::new(LeakyTool)])),
    );
    let reply = collect_reply(backend.respond(LlmTurn::new("s", "u")).await.unwrap())
        .await
        .unwrap();
    assert_eq!(reply, "Sorry.");
    let history = serde_json::to_string(&model.requests.lock().unwrap()[1].messages).unwrap();
    assert!(!history.contains("TOPSECRET"), "{history}");
    assert!(history.contains("tool error:"), "{history}");
    assert!(history.contains("key=REDACTED"), "{history}");
}

// ===========================================================================
// rig-core 0.42's real stream parsers (loopback servers)
// ===========================================================================

/// The bug the old non-streaming workaround guarded against: live Ollama emits tool
/// calls in a non-final `done:false` record. rig 0.42 must surface it from the stream.
#[tokio::test]
async fn rig_ollama_stream_surfaces_a_tool_call_from_a_non_final_chunk() {
    let body = [
        serde_json::json!({"model": "m", "created_at": "t",
            "message": {"role": "assistant", "content": "",
                "tool_calls": [{"function": {"name": "set_timer", "arguments": {"duration_seconds": 60}}}]},
            "done": false}),
        serde_json::json!({"model": "m", "created_at": "t",
            "message": {"role": "assistant", "content": ""},
            "done": true, "done_reason": "stop"}),
    ]
    .iter()
    .map(|v| v.to_string() + "\n")
    .collect::<String>();
    let (url, _requests, server) = serve("application/x-ndjson", vec![body]);
    let client = ollama::Client::builder()
        .http_client(rig_tuned_client())
        .api_key(ollama::OllamaApiKey::default())
        .base_url(&url)
        .build()
        .unwrap();
    let model = Model::Ollama(Arc::new(client.completion_model("m")));
    let req = RoundRequest {
        preamble: "sys".into(),
        messages: vec![Message::user("one minute timer")],
        tools: Tools::build(ToolDeps::default()).definitions().to_vec(),
        forbid_tools: false,
        max_tokens: 0,
    };
    let events: Vec<RoundEvent> = model
        .stream(req)
        .await
        .unwrap()
        .map(|e| e.unwrap())
        .collect()
        .await;
    server.await.unwrap();
    let calls: Vec<&ToolCall> = events
        .iter()
        .filter_map(|e| match e {
            RoundEvent::ToolCall(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1, "{events:?}");
    assert_eq!(calls[0].function.name, "set_timer");
    assert_eq!(calls[0].function.arguments["duration_seconds"], 60);
}

/// Anthropic Messages SSE: one `event:`/`data:` pair per JSON event.
fn anthropic_sse(events: &[serde_json::Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn anthropic_message_start() -> serde_json::Value {
    serde_json::json!({"type": "message_start", "message": {
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-haiku-4-5",
        "content": [], "stop_reason": null, "stop_sequence": null,
        "usage": {"input_tokens": 10, "output_tokens": 1}}})
}

fn anthropic_text_block(index: u64, deltas: &[&str]) -> Vec<serde_json::Value> {
    let mut out = vec![
        serde_json::json!({"type": "content_block_start", "index": index,
        "content_block": {"type": "text", "text": ""}}),
    ];
    for d in deltas {
        out.push(
            serde_json::json!({"type": "content_block_delta", "index": index,
            "delta": {"type": "text_delta", "text": d}}),
        );
    }
    out.push(serde_json::json!({"type": "content_block_stop", "index": index}));
    out
}

fn anthropic_end(stop_reason: &str) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": 12}}),
        serde_json::json!({"type": "message_stop"}),
    ]
}

/// Round 1 of a streamed Anthropic tool turn: lead-in text, then `set_timer` with its
/// arguments split across `input_json_delta`s.
fn anthropic_timer_round() -> String {
    let mut round = vec![anthropic_message_start()];
    round.extend(anthropic_text_block(0, &["On it", "."]));
    round.extend([
        serde_json::json!({"type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "set_timer", "input": {}}}),
        serde_json::json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": "{\"duration_seconds\":"}}),
        serde_json::json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": " 300, \"label\": \"tea\"}"}}),
        serde_json::json!({"type": "content_block_stop", "index": 1}),
    ]);
    round.extend(anthropic_end("tool_use"));
    anthropic_sse(&round)
}

/// A streamed Anthropic text-only round.
fn anthropic_text_round(deltas: &[&str]) -> String {
    let mut round = vec![anthropic_message_start()];
    round.extend(anthropic_text_block(0, deltas));
    round.extend(anthropic_end("end_turn"));
    anthropic_sse(&round)
}

/// Collect every chunk of a turn on `backend`, plus the device actions it emitted.
async fn turn_chunks(backend: &RigBackend, user: &str) -> (Vec<String>, Vec<DeviceAction>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut stream = backend
        .respond(LlmTurn::new("sys", user).with_actions(tx))
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.push(chunk.unwrap());
    }
    let mut actions = Vec::new();
    while let Ok(a) = rx.try_recv() {
        actions.push(a);
    }
    (out, actions)
}

/// A streamed Anthropic turn that says something, calls `set_timer` and then — after
/// the tool result — answers. The text is streamed delta by delta and nothing is
/// duplicated or lost.
#[tokio::test]
async fn rig_anthropic_streams_text_then_tool_call_then_answer() {
    let (url, requests, server) = serve(
        "text/event-stream",
        vec![
            anthropic_timer_round(),
            anthropic_text_round(&["Your tea timer", " is set."]),
        ],
    );
    let backend = RigBackend::anthropic(
        &url,
        "test-key",
        "claude-haiku-4-5",
        256,
        Arc::new(Tools::build(ToolDeps::default())),
    )
    .unwrap();
    let (out, actions) = turn_chunks(&backend, "tea timer").await;
    server.await.unwrap();
    assert_eq!(out, vec!["On it", ".", " Your tea timer", " is set."]);
    assert_eq!(
        actions,
        vec![DeviceAction::StartTimer {
            label: Some("tea".into()),
            duration_secs: 300
        }]
    );
    let requests = requests.lock().unwrap();
    // Both rounds streamed, advertised the tools, and authenticated with the key.
    for r in requests.iter() {
        assert!(r.contains("\"stream\":true"), "{r}");
        assert!(r.contains("\"set_timer\""), "{r}");
        assert!(r.to_lowercase().contains("x-api-key: test-key"), "{r}");
    }
    // Round 2 carries the assistant turn (text + tool_use) and the tool result.
    assert!(requests[1].contains("toolu_1"), "{}", requests[1]);
    assert!(requests[1].contains("On it."), "{}", requests[1]);
    assert!(
        requests[1].contains("Started a 5 minutes timer for tea."),
        "{}",
        requests[1]
    );
}

/// A tool call with no arguments at all (an empty `input_json_delta`) still
/// dispatches — `cancel_timer` and the close_* tools take none.
#[tokio::test]
async fn rig_anthropic_streams_an_argumentless_tool_call() {
    let mut round1 = vec![anthropic_message_start()];
    round1.extend([
        serde_json::json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_9", "name": "cancel_timer", "input": {}}}),
        serde_json::json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": ""}}),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
    ]);
    round1.extend(anthropic_end("tool_use"));
    let (url, _requests, server) = serve(
        "text/event-stream",
        vec![
            anthropic_sse(&round1),
            anthropic_text_round(&["Cancelled."]),
        ],
    );
    let backend = RigBackend::anthropic(
        &url,
        "k",
        "claude-haiku-4-5",
        256,
        Arc::new(Tools::build(ToolDeps::default())),
    )
    .unwrap();
    let (out, actions) = turn_chunks(&backend, "cancel").await;
    server.await.unwrap();
    assert_eq!(out.concat(), "Cancelled.");
    assert_eq!(actions, vec![DeviceAction::CancelTimer { label: None }]);
}

// ===========================================================================
// Subscription (OAuth) and OpenAI on rig; capabilities
// ===========================================================================

/// A token provider whose fetcher returns `tok-0`, `tok-1`, … on successive fetches.
fn counting_token_provider() -> Arc<crate::llm::anthropic_auth::AnthropicTokenProvider> {
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Arc::new(
        crate::llm::anthropic_auth::AnthropicTokenProvider::with_fetcher(Arc::new(move || {
            Ok(format!(
                "tok-{}",
                n.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ))
        })),
    )
}

/// A subscription turn on rig sends `Authorization: Bearer`, the OAuth beta header and
/// no `x-api-key`; on a 401 it refreshes the token and retries once — exactly as the
/// native subscription backend does — and the tools still run.
#[tokio::test]
async fn rig_anthropic_subscription_uses_bearer_auth_and_refreshes_on_401() {
    let (url, requests, server) = serve_with(vec![
        (
            "401 Unauthorized",
            "application/json",
            r#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#
                .to_string(),
        ),
        ("200 OK", "text/event-stream", anthropic_timer_round()),
        (
            "200 OK",
            "text/event-stream",
            anthropic_text_round(&["Tea timer set."]),
        ),
    ]);
    let backend = RigBackend::anthropic_subscription(
        &url,
        counting_token_provider(),
        "claude-haiku-4-5",
        256,
        Arc::new(Tools::build(ToolDeps::default())),
    )
    .unwrap();
    let (out, actions) = turn_chunks(&backend, "tea timer").await;
    server.await.unwrap();
    assert_eq!(out.concat(), "On it. Tea timer set.");
    assert_eq!(actions.len(), 1);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let lower: Vec<String> = requests.iter().map(|r| r.to_lowercase()).collect();
    // The first attempt used tok-0, was rejected, and the retry minted tok-1.
    assert!(
        lower[0].contains("authorization: bearer tok-0"),
        "{}",
        lower[0]
    );
    assert!(
        lower[1].contains("authorization: bearer tok-1"),
        "{}",
        lower[1]
    );
    assert!(
        lower[2].contains("authorization: bearer tok-1"),
        "{}",
        lower[2]
    );
    for r in &lower {
        assert!(!r.contains("x-api-key"), "{r}");
        assert!(r.contains("anthropic-beta: oauth-2025-04-20"), "{r}");
        assert!(r.contains("anthropic-version: 2023-06-01"), "{r}");
    }
}

/// One OpenAI Chat Completions SSE chunk.
fn openai_chunk(delta: serde_json::Value, finish: Option<&str>) -> String {
    let v = serde_json::json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1,
        "model": "gpt-4o-mini",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
    });
    format!("data: {v}\n\n")
}

#[tokio::test]
async fn rig_openai_streams_a_tool_call_then_the_answer() {
    let round1 = [
        openai_chunk(
            serde_json::json!({"role": "assistant", "content": null, "tool_calls": [
                {"index": 0, "id": "call_1", "type": "function",
                 "function": {"name": "set_timer", "arguments": ""}}]}),
            None,
        ),
        openai_chunk(
            serde_json::json!({"tool_calls": [
                {"index": 0, "function": {"arguments": "{\"duration_seconds\": 120}"}}]}),
            None,
        ),
        openai_chunk(serde_json::json!({}), Some("tool_calls")),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let round2 = [
        openai_chunk(
            serde_json::json!({"role": "assistant", "content": "Two minutes"}),
            None,
        ),
        openai_chunk(serde_json::json!({"content": ", starting now."}), None),
        openai_chunk(serde_json::json!({}), Some("stop")),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let (url, requests, server) = serve("text/event-stream", vec![round1, round2]);
    let backend = RigBackend::openai(
        &url,
        "sk-test",
        "gpt-4o-mini",
        200,
        Arc::new(Tools::build(ToolDeps::default())),
    )
    .unwrap();
    let (out, actions) = turn_chunks(&backend, "two minute timer").await;
    server.await.unwrap();
    assert_eq!(out, vec!["Two minutes", ", starting now."]);
    assert_eq!(
        actions,
        vec![DeviceAction::StartTimer {
            label: None,
            duration_secs: 120
        }]
    );
    let requests = requests.lock().unwrap();
    for r in requests.iter() {
        assert!(r.starts_with("POST /v1/chat/completions"), "{r}");
        assert!(
            r.to_lowercase().contains("authorization: bearer sk-test"),
            "{r}"
        );
        // The cap is sent as max_completion_tokens (as the native client did).
        assert!(r.contains("\"max_completion_tokens\":200"), "{r}");
        assert!(!r.contains("\"max_tokens\""), "{r}");
    }
    assert!(requests[1].contains("call_1"), "{}", requests[1]);
}

#[test]
fn rig_backends_report_full_capabilities_and_native_ones_text_only() {
    let tools = || Arc::new(Tools::build(ToolDeps::default()));
    let full = crate::llm::Capabilities {
        tools: true,
        history: true,
        streaming: true,
    };
    let rig: Vec<RigBackend> = vec![
        RigBackend::ollama("http://127.0.0.1:1", "m", tools()).unwrap(),
        RigBackend::anthropic("http://127.0.0.1:1", "k", "m", 1, tools()).unwrap(),
        RigBackend::anthropic_subscription(
            "http://127.0.0.1:1",
            counting_token_provider(),
            "m",
            1,
            tools(),
        )
        .unwrap(),
        RigBackend::openai("http://127.0.0.1:1", "k", "m", 1, tools()).unwrap(),
    ];
    for b in &rig {
        assert_eq!(b.capabilities(), full, "{}", b.name());
        assert!(b.capabilities().missing().is_empty());
    }
    let native: Vec<Box<dyn LlmBackend>> = vec![
        Box::new(crate::llm::ollama::OllamaBackend::new("http://x", "m")),
        Box::new(crate::llm::anthropic::AnthropicBackend::new(
            "http://x", "k", "m", 1,
        )),
        Box::new(crate::llm::openai::OpenAiBackend::new(
            "http://x", "k", "m", 1,
        )),
    ];
    for b in &native {
        let caps = b.capabilities();
        assert_eq!(caps, crate::llm::Capabilities::TEXT_ONLY, "{}", b.name());
        assert_eq!(caps.missing().len(), 2);
    }
}
