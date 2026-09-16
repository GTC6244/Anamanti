//! Local HTTP **config page** served from the orchestrator (rollout scope
//! addition, `memory_and_provider_rollout.md`).
//!
//! Phase 6 already made the LLM backend / voice runtime-swappable via Wyoming
//! control frames from the device (`control.rs`). This adds a second front door
//! for the same [`SharedSettings`]: a tiny web page you can open from any browser
//! on the LAN to see and change the live settings without the device.
//!
//! **No authentication.** This is a convenience surface for a trusted home
//! network; bind it to loopback (the default) unless you understand the exposure.
//! It is deliberately hand-rolled over `tokio` TCP — the same style as the
//! Wyoming protocol here — so it pulls in no HTTP framework dependency.
//!
//! Routes:
//! - `GET /`         → the HTML config page.
//! - `GET /config`   → the live [`SettingsView`](crate::settings::SettingsView) as JSON.
//! - `POST /config`  → apply a `{llm_backend?, llm_model?, tts_voice?}` change
//!   (same JSON shape as the `ambient-set-settings` control frame; a `tts_voice`
//!   of `null` clears the voice) and return the resulting settings.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::settings::{SettingsUpdate, SharedSettings};

/// The single static page. Inlined so the module is self-contained and needs no
/// asset packaging. Plain HTML + a little `fetch` JS — no framework, no build step.
const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Ambient Orchestrator — Config</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 15px/1.5 system-ui, sans-serif; max-width: 32rem; margin: 3rem auto; padding: 0 1rem; }
  h1 { font-size: 1.3rem; margin-bottom: 0.25rem; }
  .sub { opacity: 0.7; margin-top: 0; }
  label { display: block; margin: 1rem 0 0.25rem; font-weight: 600; }
  input, select { width: 100%; padding: 0.5rem; font: inherit; box-sizing: border-box; }
  button { margin-top: 1.5rem; padding: 0.6rem 1.2rem; font: inherit; font-weight: 600; cursor: pointer; }
  .status { margin-top: 1rem; padding: 0.6rem 0.8rem; border-radius: 6px; min-height: 1.2rem; }
  .ok { background: rgba(46,160,67,0.15); }
  .err { background: rgba(248,81,73,0.15); }
  .hint { opacity: 0.6; font-weight: 400; font-size: 0.85rem; }
</style>
</head>
<body>
  <h1>Ambient Orchestrator</h1>
  <p class="sub">Runtime settings — changes apply live, no restart.</p>

  <label>LLM backend
    <select id="llm_backend">
      <option value="ollama">ollama (local)</option>
      <option value="anthropic">anthropic (cloud)</option>
      <option value="mock">mock (offline)</option>
    </select>
  </label>

  <label>Model <span class="hint">e.g. qwen2.5, llama3.2, claude-opus-5</span>
    <input id="llm_model" type="text" placeholder="(backend default)">
  </label>

  <label>Piper TTS voice <span class="hint">blank = server default</span>
    <input id="tts_voice" type="text" placeholder="(default)">
  </label>

  <button id="save">Apply</button>
  <div id="status" class="status"></div>

<script>
  const $ = (id) => document.getElementById(id);
  const status = $('status');

  function show(ok, msg) {
    status.textContent = msg;
    status.className = 'status ' + (ok ? 'ok' : 'err');
  }

  function fill(v) {
    $('llm_backend').value = v.llm_backend || 'ollama';
    $('llm_model').value = v.llm_model || '';
    $('tts_voice').value = v.tts_voice || '';
  }

  async function load() {
    try {
      const r = await fetch('/config');
      fill(await r.json());
      show(true, 'Loaded current settings.');
    } catch (e) {
      show(false, 'Could not load settings: ' + e);
    }
  }

  async function save() {
    const body = {
      llm_backend: $('llm_backend').value,
      llm_model: $('llm_model').value.trim() || null,
      tts_voice: $('tts_voice').value.trim() || null,
    };
    try {
      const r = await fetch('/config', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      const v = await r.json();
      if (v.ok) { fill(v); show(true, v.message || 'Applied.'); }
      else { show(false, v.message || 'Rejected.'); }
    } catch (e) {
      show(false, 'Request failed: ' + e);
    }
  }

  $('save').addEventListener('click', save);
  load();
</script>
</body>
</html>
"#;

/// Cap on request bytes we buffer before the body — a config request is tiny; this
/// just bounds a misbehaving/hostile client on the (unauthenticated) socket.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Accept config-page connections forever, one task per connection. Returns only if
/// the listener itself fails.
pub async fn serve(listener: TcpListener, settings: Arc<SharedSettings>) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let settings = settings.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, settings).await {
                log::debug!("config page connection {peer} ended: {e:#}");
            }
        });
    }
}

/// Read one HTTP request, route it, write one response, close. One request per
/// connection (`Connection: close`) — ample for a settings page.
async fn handle(mut stream: TcpStream, settings: Arc<SharedSettings>) -> Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];

    // Read until the end of the header block.
    let header_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_REQUEST_BYTES {
            return write_response(&mut stream, "413 Payload Too Large", "text/plain", b"too large")
                .await;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(()); // client closed before sending a full request
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let content_length = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0)
        .min(MAX_REQUEST_BYTES);

    // Read the body (already-buffered bytes plus whatever remains on the socket).
    let body_start = header_end + 4;
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    let (status, content_type, payload) = route(&method, &target, &body, &settings);
    write_response(&mut stream, status, content_type, &payload).await
}

/// Pure request router: maps `(method, target, body)` to a response. Kept free of
/// I/O so it is unit-testable against a [`SharedSettings`].
fn route(
    method: &str,
    target: &str,
    body: &[u8],
    settings: &SharedSettings,
) -> (&'static str, &'static str, Vec<u8>) {
    let path = target.split(['?', '#']).next().unwrap_or(target);
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => (
            "200 OK",
            "text/html; charset=utf-8",
            INDEX_HTML.as_bytes().to_vec(),
        ),
        ("GET", "/config") => (
            "200 OK",
            "application/json",
            view_json(settings, true, None).into_bytes(),
        ),
        ("POST", "/config") => match serde_json::from_slice::<Value>(body) {
            Ok(data) => match settings.apply(&parse_update(&data)) {
                Ok(_) => (
                    "200 OK",
                    "application/json",
                    view_json(settings, true, Some("settings applied")).into_bytes(),
                ),
                Err(e) => (
                    "200 OK",
                    "application/json",
                    view_json(settings, false, Some(&format!("{e:#}"))).into_bytes(),
                ),
            },
            Err(e) => (
                "400 Bad Request",
                "application/json",
                json!({ "ok": false, "message": format!("invalid JSON: {e}") })
                    .to_string()
                    .into_bytes(),
            ),
        },
        _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
    }
}

/// The current settings as a JSON string, with an `ok`/`message` envelope that
/// mirrors the Wyoming `ambient-settings` response.
fn view_json(settings: &SharedSettings, ok: bool, message: Option<&str>) -> String {
    let v = settings.view();
    json!({
        "ok": ok,
        "message": message,
        "llm_backend": v.llm_backend,
        "llm_model": v.llm_model,
        "tts_voice": v.tts_voice,
    })
    .to_string()
}

/// Parse a [`SettingsUpdate`] from a JSON object. Identical semantics to the
/// Wyoming control path (`control::parse_update`): an absent key leaves that
/// setting unchanged; `tts_voice: null` clears the voice.
fn parse_update(data: &Value) -> SettingsUpdate {
    let string_field = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let tts_voice = match data.get("tts_voice") {
        None => None,                    // unchanged
        Some(Value::Null) => Some(None), // clear
        Some(v) => Some(v.as_str().filter(|s| !s.is_empty()).map(str::to_string)),
    };
    SettingsUpdate {
        llm_backend: string_field("llm_backend"),
        llm_model: string_field("llm_model"),
        tts_voice,
    }
}

/// Write a minimal HTTP/1.1 response and close the connection.
async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n",
        len = body.len(),
    );
    stream
        .write_all(header.as_bytes())
        .await
        .context("writing config response header")?;
    stream
        .write_all(body)
        .await
        .context("writing config response body")?;
    stream.flush().await.context("flushing config response")?;
    Ok(())
}

/// First index of `needle` in `haystack`, or `None`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::mock::MockLlm;

    fn settings() -> Arc<SharedSettings> {
        SharedSettings::fixed(Arc::new(MockLlm::default()), "mock", None)
    }

    #[test]
    fn get_root_serves_html() {
        let (status, ctype, body) = route("GET", "/", b"", &settings());
        assert_eq!(status, "200 OK");
        assert!(ctype.starts_with("text/html"));
        assert!(String::from_utf8_lossy(&body).contains("Ambient Orchestrator"));
    }

    #[test]
    fn get_config_reports_current_backend() {
        let (status, ctype, body) = route("GET", "/config?_=1", b"", &settings());
        assert_eq!(status, "200 OK");
        assert_eq!(ctype, "application/json");
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["llm_backend"], "mock");
    }

    #[test]
    fn post_config_applies_tts_voice_and_reports_it_back() {
        let s = settings();
        let body = br#"{"tts_voice":"en_US-amy-medium"}"#;
        let (status, _ctype, out) = route("POST", "/config", body, &s);
        assert_eq!(status, "200 OK");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["tts_voice"], "en_US-amy-medium");
        assert_eq!(s.view().tts_voice.as_deref(), Some("en_US-amy-medium"));
    }

    #[test]
    fn post_config_null_voice_clears_it() {
        let s = settings();
        route("POST", "/config", br#"{"tts_voice":"amy"}"#, &s);
        route("POST", "/config", br#"{"tts_voice":null}"#, &s);
        assert_eq!(s.view().tts_voice, None);
    }

    #[test]
    fn post_config_rejects_unknown_backend_without_dropping_settings() {
        let s = settings();
        let before = s.view();
        let (status, _c, out) = route("POST", "/config", br#"{"llm_backend":"gpt"}"#, &s);
        assert_eq!(status, "200 OK");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(s.view(), before, "a rejected change leaves settings intact");
    }

    #[test]
    fn invalid_json_is_a_400() {
        let (status, _c, _b) = route("POST", "/config", b"not json", &settings());
        assert_eq!(status, "400 Bad Request");
    }

    #[test]
    fn unknown_route_is_404() {
        let (status, ..) = route("GET", "/nope", b"", &settings());
        assert_eq!(status, "404 Not Found");
    }

    /// End-to-end over a real socket: exercises the HTTP request parsing in
    /// `handle` (header split, Content-Length body read), not just `route`.
    #[tokio::test]
    async fn serves_a_post_over_a_real_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let s = settings();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle(stream, s).await.unwrap();
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        let body = br#"{"tts_voice":"en_US-amy-medium"}"#;
        let req = format!(
            "POST /config HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        client.write_all(req.as_bytes()).await.unwrap();
        client.write_all(body).await.unwrap();

        let mut resp = String::new();
        client.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "resp: {resp}");
        assert!(resp.contains("en_US-amy-medium"), "resp: {resp}");
    }
}
