//! Memory management over Wyoming control frames — exactly what the Display's Memory
//! screen does (`anamanti-display/rust/src/wyoming/control.rs`): one short-lived TCP
//! connection to the Core's Wyoming port per request, one `anamanti-*` request frame,
//! one reply frame. Frame types and framing come from `anamanti-protocol`.

use std::net::SocketAddr;
use std::time::Duration;

use anamanti_protocol::{read_event, types, write_event, WyomingEvent};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::net::TcpStream;

use crate::api::companion::MemoryEntry;
use crate::decode;

/// Hard cap on one control round trip; bounds a peer that accepts but never replies.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// One request/response round trip over a fresh connection to `addr`.
async fn round_trip(addr: SocketAddr, request: WyomingEvent) -> Result<WyomingEvent> {
    tokio::time::timeout(CONTROL_TIMEOUT, async {
        let stream = TcpStream::connect(addr)
            .await
            .with_context(|| format!("connecting to the Core at {addr}"))?;
        stream.set_nodelay(true).ok();
        let (read_half, mut writer) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        write_event(&mut writer, &request)
            .await
            .context("sending control request")?;
        read_event(&mut reader)
            .await
            .context("reading control response")?
            .ok_or_else(|| anyhow!("the Core closed the connection without responding"))
    })
    .await
    .map_err(|_| anyhow!("the Core at {addr} did not respond within {CONTROL_TIMEOUT:?}"))?
}

/// Reply must be the expected frame type; anything else is a protocol error.
fn expect(ev: WyomingEvent, kind: &str) -> Result<WyomingEvent> {
    if ev.event_type == kind {
        Ok(ev)
    } else {
        Err(anyhow!(
            "unexpected reply `{}` (wanted `{kind}`)",
            ev.event_type
        ))
    }
}

/// `anamanti-list-memories` → `anamanti-memories`.
pub async fn list(addr: SocketAddr) -> Result<Vec<MemoryEntry>> {
    let resp = expect(
        round_trip(addr, WyomingEvent::new(types::LIST_MEMORIES)).await?,
        types::MEMORIES,
    )?;
    decode::memories(&resp.data)
}

/// `anamanti-delete-memory {id}` → `anamanti-memory-result`; `true` if a row went.
pub async fn delete(addr: SocketAddr, id: i64) -> Result<bool> {
    let request = WyomingEvent::with_data(types::DELETE_MEMORY, json!({ "id": id }));
    let resp = expect(round_trip(addr, request).await?, types::MEMORY_RESULT)?;
    result_count(&resp.data).map(|n| n > 0)
}

/// `anamanti-clear-memories` → `anamanti-memory-result`; the number removed.
pub async fn clear(addr: SocketAddr) -> Result<u32> {
    let resp = expect(
        round_trip(addr, WyomingEvent::new(types::CLEAR_MEMORIES)).await?,
        types::MEMORY_RESULT,
    )?;
    result_count(&resp.data)
}

/// `{ok, count, message?}` → count, or the Core's error message. `ok:false` with
/// `count:0` and no message is a plain "nothing removed" (delete of a gone id).
fn result_count(data: &Value) -> Result<u32> {
    let ok = data.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let count = data
        .get("count")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u32;
    if !ok {
        if let Some(msg) = data.get("message").and_then(Value::as_str) {
            return Err(anyhow!("{msg}"));
        }
    }
    Ok(if ok { count } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    /// A fake Core: accept one connection, read one request, answer with `response`,
    /// and hand back the request it saw.
    async fn fake_core(response: WyomingEvent) -> (SocketAddr, JoinHandle<WyomingEvent>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (r, mut w) = sock.into_split();
            let mut reader = BufReader::new(r);
            let req = read_event(&mut reader).await.unwrap().unwrap();
            write_event(&mut w, &response).await.unwrap();
            req
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn list_round_trip() {
        let (addr, server) = fake_core(WyomingEvent::with_data(
            types::MEMORIES,
            json!({ "ok": true, "entries": [
                { "id": 5, "kind": "fact", "content": "dog is Rex", "source": "explicit", "created_at": 7 }
            ]}),
        ))
        .await;
        let entries = list(addr).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "dog is Rex");
        assert_eq!(server.await.unwrap().event_type, types::LIST_MEMORIES);
    }

    #[tokio::test]
    async fn delete_round_trip_sends_id() {
        let (addr, server) = fake_core(WyomingEvent::with_data(
            types::MEMORY_RESULT,
            json!({ "ok": true, "count": 1 }),
        ))
        .await;
        assert!(delete(addr, 42).await.unwrap());
        let req = server.await.unwrap();
        assert_eq!(req.event_type, types::DELETE_MEMORY);
        assert_eq!(req.data["id"], json!(42));
    }

    #[tokio::test]
    async fn delete_of_missing_id_is_false_not_error() {
        let (addr, _s) = fake_core(WyomingEvent::with_data(
            types::MEMORY_RESULT,
            json!({ "ok": false, "count": 0 }),
        ))
        .await;
        assert!(!delete(addr, 9).await.unwrap());
    }

    #[tokio::test]
    async fn clear_round_trip_returns_count_and_surfaces_errors() {
        let (addr, server) = fake_core(WyomingEvent::with_data(
            types::MEMORY_RESULT,
            json!({ "ok": true, "count": 3 }),
        ))
        .await;
        assert_eq!(clear(addr).await.unwrap(), 3);
        assert_eq!(server.await.unwrap().event_type, types::CLEAR_MEMORIES);

        let (addr, _s) = fake_core(WyomingEvent::with_data(
            types::MEMORY_RESULT,
            json!({ "ok": false, "count": 0, "message": "database is locked" }),
        ))
        .await;
        assert!(clear(addr)
            .await
            .unwrap_err()
            .to_string()
            .contains("database is locked"));
    }

    #[tokio::test]
    async fn wrong_reply_type_is_an_error() {
        let (addr, _s) = fake_core(WyomingEvent::new(types::INFO)).await;
        assert!(list(addr).await.is_err());
    }

    #[tokio::test]
    async fn refused_connection_is_an_error() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        assert!(list(addr).await.is_err());
    }
}
