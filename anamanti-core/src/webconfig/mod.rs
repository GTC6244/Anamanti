//! Local HTTP **config page** served from the orchestrator (rollout scope
//! addition, `memory_and_provider_rollout.md`).
//!
//! Phase 6 already made the LLM backend / voice runtime-swappable via Wyoming
//! control frames from the device (`control.rs`). This adds a second front door
//! for the same [`SharedSettings`]: a tiny web page you can open from any browser
//! on the LAN to see and change the live settings without the device.
//!
//! **No authentication.** This is a convenience surface for the Mac itself: it must
//! be bound to loopback (the default). Every request is checked by
//! `check_request`: the `Host` header must name this server on a loopback address
//! (`127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>`) — defeating DNS
//! rebinding — and an `Origin` header, when present, must be that same loopback origin
//! — defeating cross-site POSTs from any other page open in a browser on the Mac.
//! POSTs must also carry `Content-Type: application/json` (a plain HTML form cannot set
//! it). Failures get `403` / `415` and never reach a handler. A consequence: the page
//! is not reachable from another machine even if `config_addr` is bound to a LAN
//! address.
//! It is deliberately hand-rolled over `tokio` TCP — the same style as the
//! Wyoming protocol here — so it pulls in no HTTP framework dependency.
//!
//! It also serves a small set of **read-only debug pages** over the same socket
//! ([`DebugSources`]) so you can inspect what the assistant is doing from a
//! browser: the chat log, the exact prompt sent to the LLM, the SQLite memory
//! store, and the HelixDB GraphRAG store. These are strictly read-only.
//!
//! Layout: this file owns the HTTP plumbing (request parsing, the request guard,
//! response writing) and the shared [`Reply`] / JSON-body helpers; `routes.rs` holds
//! the **one route table** (every `(method, path)` the page serves); the handlers live
//! in per-area submodules (`general`, `tools`, `household`, `drive`, `music`,
//! `memory`, `debug`). The page bodies are plain HTML files beside this one, compiled
//! in with `include_str!`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::app::AppServices;
use crate::memory::GraphView;
use crate::settings::SharedSettings;

mod debug;
mod drive;
mod general;
mod home_assistant;
mod household;
mod language;
mod memory;
mod music;
mod routes;
mod speakers;
mod system;
mod tools;

pub use system::BootInfo;

#[cfg(test)]
mod tests;

/// Read-only data sources the debug pages render (chat log, prompts, SQLite,
/// HelixDB) beyond the memory store itself (which comes from
/// [`AppServices::memory`]). Cheaply cloneable — everything is behind an `Arc` or a
/// small `PathBuf`.
#[derive(Clone, Default)]
pub struct DebugSources {
    /// Path to the append-only chat log JSONL (rendered by `/chatlog`).
    pub chatlog_path: PathBuf,
    /// Path to the append-only prompt log JSONL (rendered by `/prompts`).
    pub promptlog_path: PathBuf,
    /// The SQLite database path (shown for context on `/sqlite`).
    pub db_path: PathBuf,
    /// The HelixDB store root (shown for context on `/helix`).
    pub helix_path: PathBuf,
    /// Active memory retrieval backend label (`"sqlite"` or `"helix"`).
    pub memory_backend: String,
    /// Read-only view of the graph store when GraphRAG is live; `None` otherwise
    /// (feature off, SQLite backend, or init failed → `/helix` reports disabled).
    pub graph: Option<Arc<dyn GraphView>>,
    /// The boot-config snapshot (downstream addresses for the locality chips + the
    /// redacted effective config for the System page).
    pub boot: BootInfo,
}

/// The shared admin-console design system (Material 3 tokens + the console's layout
/// grid, locality chips, choice cards, drawers, tables). GENERATED from
/// `webconfig/tailwind/input.css` by `scripts/build-webui-css.sh` (the Tailwind CLI)
/// and committed; served verbatim at `GET /panel.css`. Do not hand-edit — edit the
/// Tailwind source + the page HTML, then re-run the script.
const PANEL_CSS: &str = include_str!("panel.css");

/// Client-side helpers shared by every page (`esc`/`fmtTime`/`getJSON`/`apiJSON`/
/// `postJSON` — every POST goes through `postJSON`, which sends the JSON content type
/// the server requires) plus the shell behavior (active-nav highlight, collapsible
/// rail, mobile drawer, app-bar status pills). Served at `GET /shell.js`.
const SHELL_SCRIPT: &str = include_str!("shell.js");

/// The console's UI component kit (toasts, dirty-tracking save bar, secret fields,
/// choice cards, locality chips + the locality map, drawers, tabs, inline confirm).
/// Served at `GET /ui.js`, loaded right after `/shell.js`.
const UI_SCRIPT: &str = include_str!("ui.js");

/// The static HTML shell wrapped around every page body: the `<head>` + the top app
/// bar + the navigation rail (`shell_head.html`, with a `{{TITLE}}` token), and the
/// closing tags (`shell_foot.html`). Authored as plain HTML so no page markup is
/// generated in Rust; compiled in so the single binary still serves the whole UI.
const SHELL_HEAD: &str = include_str!("shell_head.html");
const SHELL_FOOT: &str = include_str!("shell_foot.html");

// Page bodies. Each is a real HTML file beside this one, compiled in with
// `include_str!` — no runtime asset files, so the single copied release binary still
// serves the whole admin UI with nothing beside it. Information architecture:
// plans/CoreWebUIPlan.md §4.

/// `/` — the Overview dashboard (pipeline + locality, displays, latency, egress).
const OVERVIEW_BODY: &str = include_str!("overview.html");
/// `/voice` — STT engine, VAD, Piper voice, transcript fixes, listening (read-only).
const VOICE_BODY: &str = include_str!("voice.html");
/// `/intelligence` — LLM model/engine + credentials, System-1 fast path, web search,
/// the system prompt (read-only).
const INTELLIGENCE_BODY: &str = include_str!("intelligence.html");
/// `/personality` — the output-personality switch + catalog editor.
const PERSONALITY_BODY: &str = include_str!("personality.html");
/// `/household` — home address + units and the people roster.
const HOUSEHOLD_BODY: &str = include_str!("household.html");
/// `/integrations` — the third-party service tiles (weather, places, directions,
/// calendar, Cadora, AppSaid, Spotify, Google Drive) with per-tile drawers.
const INTEGRATIONS_BODY: &str = include_str!("integrations.html");
/// `/devices` — connected displays + test notification, and the music processes.
const DEVICES_BODY: &str = include_str!("devices.html");
/// `/memory` — recall settings (embeddings + entity extraction), facts, graph.
const MEMORY_BODY: &str = include_str!("memory.html");
/// `/conversations` — the turn log with per-turn latency and the matched prompt.
const CONVERSATIONS_BODY: &str = include_str!("conversations.html");
/// `/keys` — every secret / OAuth link in one table.
const KEYS_BODY: &str = include_str!("keys.html");
/// `/system` — build, runtime and the redacted effective boot config.
const SYSTEM_BODY: &str = include_str!("system.html");

/// Cap on request bytes we buffer before the body — a config request is tiny; this
/// just bounds a misbehaving/hostile client on the (unauthenticated) socket.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Wrap a page `body` in the shared static HTML shell: `SHELL_HEAD` (`<head>` +
/// `/panel.css` + the Material app bar + navigation drawer, with its `{{TITLE}}`
/// token filled in for both `<title>` and the page `<h1>`), the page body, then
/// `SHELL_FOOT` (closing tags). The nav highlight for the current path and the mobile
/// drawer are handled in `/shell.js` (loaded, blocking, just before the body), so the
/// shell needs no server-side state — `page()` is a pure concatenation and authors no
/// markup in Rust. `title` is always a static literal, so no escaping is required.
fn page(title: &str, body: &str) -> String {
    let head = SHELL_HEAD.replace("{{TITLE}}", title);
    format!("{head}{body}{SHELL_FOOT}")
}

/// One parsed request, as the route handlers see it (after the guard has passed).
struct Request<'a> {
    /// The full origin-form target, including any `?query`.
    target: &'a str,
    /// `target` without its query/fragment — what the route table matches on.
    path: &'a str,
    /// The request body (`Content-Length` bytes).
    body: &'a [u8],
}

/// One response: status line, content type and body (plus a `Location` for a
/// redirect).
struct Reply {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    location: Option<&'static str>,
}

impl Reply {
    /// `200 OK` with a JSON payload — the shape of nearly every route.
    fn json(payload: String) -> Self {
        Self {
            status: "200 OK",
            content_type: "application/json",
            body: payload.into_bytes(),
            location: None,
        }
    }

    /// `200 OK` with a page body wrapped in the shared shell.
    fn page(title: &str, body: &str) -> Self {
        Self {
            status: "200 OK",
            content_type: "text/html; charset=utf-8",
            body: page(title, body).into_bytes(),
            location: None,
        }
    }

    /// `200 OK` with a static asset.
    fn asset(content_type: &'static str, body: &str) -> Self {
        Self {
            status: "200 OK",
            content_type,
            body: body.as_bytes().to_vec(),
            location: None,
        }
    }

    /// A plain-text response (errors outside the JSON API: guard failures, 404, 413).
    fn text(status: &'static str, message: &str) -> Self {
        Self {
            status,
            content_type: "text/plain",
            body: message.as_bytes().to_vec(),
            location: None,
        }
    }

    /// `302 Found` to `location` — the legacy page routes (`/stt`, `/tools`, …) point
    /// at their new homes so bookmarks keep working.
    fn redirect(location: &'static str) -> Self {
        Self {
            status: "302 Found",
            content_type: "text/plain",
            body: format!("moved to {location}").into_bytes(),
            location: Some(location),
        }
    }

    fn not_found() -> Self {
        Self::text("404 Not Found", "not found")
    }

    /// The same body with a different status (e.g. `400` for a malformed `/config`).
    fn with_status(mut self, status: &'static str) -> Self {
        self.status = status;
        self
    }
}

/// Parse a request body as JSON. The one place the "invalid JSON" error is built: on
/// failure the `Err` is the ready-to-send `{ ok: false, message: "invalid JSON: …" }`
/// payload (the page handlers report it in-band with a `200`; `/config` sends it as a
/// `400`).
fn parse_json_body(body: &[u8]) -> std::result::Result<Value, String> {
    serde_json::from_slice(body)
        .map_err(|e| json!({ "ok": false, "message": format!("invalid JSON: {e}") }).to_string())
}

/// Run a JSON-body handler: parse the body and reply `200` with `handler`'s payload,
/// or `200` with the invalid-JSON payload when the body is malformed.
fn with_json(req: &Request<'_>, handler: impl FnOnce(&Value) -> String) -> Reply {
    Reply::json(match parse_json_body(req.body) {
        Ok(data) => handler(&data),
        Err(invalid) => invalid,
    })
}

/// A string field, trimmed; `None` when absent, not a string, or blank.
fn str_field(data: &Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Tri-state update for a stored value the page never shows back (secrets, creds):
/// a non-blank string sets it (`Some(Some(v))`); blank or absent leaves it unchanged
/// (`None`) — so a page reload posting an empty field never wipes it.
fn keep_or_set(data: &Value, key: &str) -> Option<Option<String>> {
    str_field(data, key).map(Some)
}

/// Tri-state update for a visible optional value: a present string sets it, a present
/// blank string clears it (`Some(None)`), an absent field leaves it unchanged (`None`).
fn set_or_clear(data: &Value, key: &str) -> Option<Option<String>> {
    data.get(key).and_then(Value::as_str).map(|s| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    })
}

/// Tri-state update for a secret with an explicit clear: `"clear": true` clears it,
/// otherwise as [`keep_or_set`].
fn secret_update(data: &Value, key: &str) -> Option<Option<String>> {
    if data.get("clear").and_then(Value::as_bool).unwrap_or(false) {
        Some(None)
    } else {
        keep_or_set(data, key)
    }
}

/// `{ ok: false, message }` — the in-band failure payload most handlers return.
fn fail(message: impl std::fmt::Display) -> String {
    json!({ "ok": false, "message": message.to_string() }).to_string()
}

/// The current settings as a JSON string, with an `ok`/`message` envelope that
/// mirrors the Wyoming `ambient-settings` response (both use
/// [`crate::settings::SettingsView::to_json`]).
fn view_json(settings: &SharedSettings, ok: bool, message: Option<&str>) -> String {
    settings.view().to_json(ok, message).to_string()
}

/// Accept config-page connections forever, one task per connection. Returns only if
/// the listener itself fails.
pub async fn serve(listener: TcpListener, services: Arc<AppServices>) -> Result<()> {
    system::mark_started();
    loop {
        let (stream, peer) = listener.accept().await?;
        let services = services.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, &services).await {
                log::debug!("config page connection {peer} ended: {e:#}");
            }
        });
    }
}

/// Read one HTTP request, check it, route it, write one response, close. One request
/// per connection (`Connection: close`) — ample for a settings page.
async fn handle(mut stream: TcpStream, services: &AppServices) -> Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];

    // Read until the end of the header block.
    let header_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_REQUEST_BYTES {
            let reply = Reply::text("413 Payload Too Large", "too large");
            return write_reply(&mut stream, &reply).await;
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
    let target = normalize_target(parts.next().unwrap_or("/"));

    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();

    // Same-origin loopback guard (A6), run once for every route: reject before reading
    // the body or touching any handler, so a hostile page / rebinding DNS name can
    // neither read nor mutate.
    let port = stream.local_addr().map(|a| a.port()).unwrap_or(0);
    if let Err((status, why)) = check_request(&method, &headers, port) {
        log::warn!("config page: rejected {method} {target}: {why}");
        return write_reply(&mut stream, &Reply::text(status, why)).await;
    }

    let content_length = header(&headers, "content-length")
        .and_then(|v| v.parse::<usize>().ok())
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

    let req = Request {
        target: &target,
        path: target.split(['?', '#']).next().unwrap_or(&target),
        body: &body,
    };
    let reply = routes::dispatch(services, &method, &req).await;
    write_reply(&mut stream, &reply).await
}

/// The value of header `name` (lowercase), if present.
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// Whether `authority` (a `Host` header value or the host part of an `Origin`) names
/// this server on a loopback address: `127.0.0.1:<port>`, `localhost:<port>` or
/// `[::1]:<port>` (case-insensitive).
fn is_loopback_authority(authority: &str, port: u16) -> bool {
    let a = authority.trim().to_ascii_lowercase();
    ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .any(|host| a == format!("{host}:{port}"))
}

/// The admin-page request guard (A6): `Host` must be this server's loopback
/// authority (blocks DNS rebinding), an `Origin`, when sent, must be the same loopback
/// origin (blocks cross-site requests from other pages in a browser on the Mac), and
/// every POST must declare `Content-Type: application/json`. Returns the HTTP status +
/// reason to send on rejection.
fn check_request(
    method: &str,
    headers: &[(String, String)],
    port: u16,
) -> std::result::Result<(), (&'static str, &'static str)> {
    const FORBIDDEN: &str = "403 Forbidden";
    match header(headers, "host") {
        Some(host) if is_loopback_authority(host, port) => {}
        Some(_) => return Err((FORBIDDEN, "forbidden: Host is not this loopback server")),
        None => return Err((FORBIDDEN, "forbidden: missing Host header")),
    }
    if let Some(origin) = header(headers, "origin") {
        let same = origin
            .strip_prefix("http://")
            .is_some_and(|auth| is_loopback_authority(auth, port));
        if !same {
            return Err((FORBIDDEN, "forbidden: cross-origin request"));
        }
    }
    if method.eq_ignore_ascii_case("POST") {
        let json = header(headers, "content-type").is_some_and(|ct| {
            ct.split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        });
        if !json {
            return Err((
                "415 Unsupported Media Type",
                "POST requires Content-Type: application/json",
            ));
        }
    }
    Ok(())
}

/// Write a minimal HTTP/1.1 response and close the connection.
async fn write_reply(stream: &mut TcpStream, reply: &Reply) -> Result<()> {
    let location = reply
        .location
        .map(|l| format!("Location: {l}\r\n"))
        .unwrap_or_default();
    let header = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         {location}\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n",
        status = reply.status,
        content_type = reply.content_type,
        len = reply.body.len(),
    );
    stream
        .write_all(header.as_bytes())
        .await
        .context("writing config response header")?;
    stream
        .write_all(&reply.body)
        .await
        .context("writing config response body")?;
    stream.flush().await.context("flushing config response")?;
    Ok(())
}

/// Reduce a request target to origin-form (`/path?query`). Clients behind an HTTP
/// **proxy** send the **absolute-form** target (RFC 7230 §5.3.2), e.g.
/// `POST http://127.0.0.1:8731/drive/save HTTP/1.1`; a server MUST accept it. Our
/// routing matches on the path, so strip any `scheme://authority` prefix down to the
/// first `/` (an absolute URI with no path becomes `/`). Origin-form is returned
/// unchanged.
fn normalize_target(target: &str) -> String {
    let rest = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https://"));
    match rest {
        Some(after_scheme) => match after_scheme.find('/') {
            Some(i) => after_scheme[i..].to_string(),
            None => "/".to_string(),
        },
        None => target.to_string(),
    }
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
