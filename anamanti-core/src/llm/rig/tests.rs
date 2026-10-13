//! End-to-end tests of the rig backend against loopback provider servers (no real
//! network): the tool loop, dispatch, and device-action relay.

use std::sync::Arc;

use super::tools::test_support::*;
use super::*;
use crate::cadora::GroceryCommand;
use crate::llm::{collect_reply, DeviceAction};
use crate::music::{SearchKind, SpotifyCommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve a fixed sequence of raw HTTP responses (one per inbound connection) with
/// the given content type, recording each request.
fn serve(
    content_type: &'static str,
    bodies: Vec<String>,
) -> (
    String,
    Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let handle = tokio::spawn(async move {
        let listener = TcpListener::from_std(std_listener).unwrap();
        for body in bodies {
            let (mut sock, _) = listener.accept().await.unwrap();
            let request = read_request(&mut sock).await;
            seen.lock().unwrap().push(request);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
    let controller = Arc::new(StaticGrocery::default());
    let deps = ToolDeps {
        grocery: Some(controller.clone()),
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
        Some(GroceryCommand::AddItem {
            item: "milk".into(),
            quantity: Some(2),
        })
    );
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
