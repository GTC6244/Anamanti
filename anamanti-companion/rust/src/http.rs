//! Tiny HTTP GET client for the Core's companion API.
//!
//! The companion listener is plain HTTP on the home LAN, GET-only, no auth, and it
//! **rejects any request carrying an `Origin` header** (403). `ureq` never adds one,
//! and we never set one. Every request has mandatory connect + overall timeouts and
//! no redirects. `ureq` is blocking, so calls run on tokio's blocking pool.

use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

/// Connect timeout: a LAN host answers in milliseconds; this bounds a dead address.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Whole-request timeout. `/overview.json` with probes takes up to ~0.6 s on the Core.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// Responses are small JSON documents; refuse anything absurd.
const MAX_BODY_BYTES: u64 = 2 * 1024 * 1024;

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirects(0)
            .user_agent(concat!("anamanti-companion/", env!("CARGO_PKG_VERSION")))
            .build()
    })
}

/// `http://<addr>:<port>` (IPv6 bracketed). `address` must be an IP literal — it
/// always comes from the mDNS resolution, never from user input.
pub fn base_url(address: &str, port: u16) -> Result<String> {
    let ip: IpAddr = address
        .parse()
        .with_context(|| format!("invalid Core address `{address}`"))?;
    Ok(format!("http://{}", SocketAddr::new(ip, port)))
}

/// Blocking GET of `url`, returning the body. Non-2xx statuses become errors that
/// carry the Core's `message` when the body is a JSON error envelope.
fn get_blocking(url: &str) -> Result<String> {
    use std::io::Read;
    match agent().get(url).set("Accept", "application/json").call() {
        Ok(resp) => {
            let mut body = String::new();
            resp.into_reader()
                .take(MAX_BODY_BYTES)
                .read_to_string(&mut body)
                .context("reading response body")?;
            Ok(body)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| body.trim().chars().take(200).collect());
            Err(anyhow!(
                "HTTP {code}{}{message}",
                if message.is_empty() { "" } else { ": " }
            ))
        }
        Err(ureq::Error::Transport(t)) => Err(anyhow!("could not reach the Core: {t}")),
    }
}

/// GET `path` from `base` (e.g. `http://192.168.1.50:8740`) off the async runtime.
pub async fn get(base: &str, path: &str) -> Result<String> {
    let url = format!("{base}{path}");
    tokio::task::spawn_blocking(move || get_blocking(&url))
        .await
        .map_err(|e| anyhow!("HTTP task failed: {e}"))?
}

/// GET and parse a JSON object, surfacing an in-band `{ok:false, message}` as an error.
pub async fn get_json(base: &str, path: &str) -> Result<Value> {
    let body = get(base, path).await?;
    let v: Value = serde_json::from_str(&body).with_context(|| format!("{path} is not JSON"))?;
    check_ok(&v, path)?;
    Ok(v)
}

/// `ok: false` → error with the Core's message. A missing `ok` is treated as success.
pub fn check_ok(v: &Value, what: &str) -> Result<()> {
    if v.get("ok").and_then(Value::as_bool) == Some(false) {
        let msg = v
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("request failed");
        return Err(anyhow!("{what}: {msg}"));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_server {
    //! An in-process one-shot HTTP server for tests: answers the first request with a
    //! canned status + body and hands back the raw request text.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    pub async fn serve_once(status: &str, body: &str) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let status = status.to_string();
        let body = body.to_string();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
            }
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
            String::from_utf8_lossy(&req).into_owned()
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::serve_once;
    use super::*;

    #[test]
    fn base_url_formats_v4_and_brackets_v6() {
        assert_eq!(
            base_url("192.168.1.50", 8740).unwrap(),
            "http://192.168.1.50:8740"
        );
        assert_eq!(
            base_url("2001:db8::1", 8740).unwrap(),
            "http://[2001:db8::1]:8740"
        );
        assert!(base_url("mac-mini.local", 8740).is_err());
    }

    #[tokio::test]
    async fn get_returns_body_and_never_sends_origin() {
        let (base, server) = serve_once("200 OK", r#"{"ok":true,"x":1}"#).await;
        let v = get_json(&base, "/overview.json?probe=0").await.unwrap();
        assert_eq!(v["x"], 1);
        let req = server.await.unwrap();
        assert!(
            req.starts_with("GET /overview.json?probe=0 HTTP/1.1\r\n"),
            "{req}"
        );
        assert!(
            !req.to_ascii_lowercase().contains("\norigin:"),
            "must never send Origin: {req}"
        );
    }

    #[tokio::test]
    async fn http_error_status_carries_core_message() {
        let (base, _server) = serve_once(
            "403 Forbidden",
            r#"{"ok":false,"message":"cross-origin request refused"}"#,
        )
        .await;
        let err = get(&base, "/timers.json").await.unwrap_err().to_string();
        assert!(err.contains("403"), "{err}");
        assert!(err.contains("cross-origin request refused"), "{err}");
    }

    #[tokio::test]
    async fn in_band_ok_false_is_an_error() {
        let (base, _server) =
            serve_once("200 OK", r#"{"ok":false,"message":"spotify down"}"#).await;
        let err = get_json(&base, "/music/nowplaying.json").await.unwrap_err();
        assert!(err.to_string().contains("spotify down"));
    }

    #[tokio::test]
    async fn unreachable_core_is_a_transport_error() {
        // Bind then drop to get a port nothing listens on.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let err = get(&format!("http://127.0.0.1:{port}"), "/timers.json")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("could not reach"), "{err}");
    }
}
