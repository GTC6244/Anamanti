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
//! - `GET /models`   → the selectable LLM models for the model dropdown.
//! - `GET /voices`   → the installed Piper voices for the TTS voice dropdown.
//! - `POST /config`  → apply a `{llm_backend?, llm_model?, tts_voice?}` change
//!   (same JSON shape as the `ambient-set-settings` control frame; a `tts_voice`
//!   of `null` clears the voice) and return the resulting settings.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::llm::anthropic_auth::AnthropicAuth;
use crate::llm::catalog::ModelCatalog;
use crate::orchestrator::ServiceConnector;
use crate::settings::{LlmEngine, SettingsUpdate, SharedSettings};

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
  label.check { display: flex; align-items: center; gap: 0.5rem; font-weight: 600; }
  label.check input { width: auto; }
</style>
</head>
<body>
  <h1>Ambient Orchestrator</h1>
  <p class="sub">Runtime settings — changes apply live, no restart.</p>

  <label>Engine
    <select id="engine">
      <option value="native">native (HTTP clients)</option>
      <option value="rig">rig (agent framework + tools)</option>
    </select>
  </label>

  <label>LLM backend
    <select id="llm_backend">
      <option value="ollama">ollama (local)</option>
      <option value="anthropic">anthropic (cloud)</option>
      <option value="openai">openai (cloud)</option>
      <option value="mock">mock (offline)</option>
    </select>
  </label>

  <label id="anthropic_auth_row">Anthropic auth
    <select id="anthropic_auth">
      <option value="apikey">API key (ANTHROPIC_API_KEY)</option>
      <option value="subscription">Subscription — Claude OAuth (claude setup-token)</option>
    </select>
  </label>

  <label id="anthropic_key_row">Anthropic API key <span class="hint" id="anthropic_key_state"></span>
    <input id="anthropic_api_key" type="password" placeholder="(leave blank to keep current)">
  </label>

  <label id="openai_key_row">OpenAI API key <span class="hint" id="openai_key_state"></span>
    <input id="openai_api_key" type="password" placeholder="(leave blank to keep current)">
  </label>

  <label class="check">
    <input id="web_search" type="checkbox">
    Web search tool <span class="hint">(rig engine only)</span>
  </label>

  <label>Search provider
    <select id="search_provider">
      <option value="duckduckgo">DuckDuckGo (keyless; entity queries only)</option>
      <option value="tavily">Tavily (real web results; needs a key)</option>
    </select>
  </label>

  <label>Search API key <span class="hint" id="key_state"></span>
    <input id="search_api_key" type="password" placeholder="(leave blank to keep current)">
  </label>

  <label>Model <span class="hint" id="model_hint">last 12 months, per provider</span>
    <!-- Cloud backends (anthropic/openai): a dropdown of last-12-months models. -->
    <select id="llm_model_select"></select>
    <!-- Local/mock: a free-text model tag (e.g. an Ollama tag like qwen2.5). -->
    <input id="llm_model_text" type="text" placeholder="(backend default)" style="display:none">
  </label>

  <label>Piper TTS voice <span class="hint" id="voice_hint">blank = server default</span>
    <!-- When the orchestrator can list voices: a dropdown of installed Piper voices. -->
    <select id="tts_voice_select" style="display:none"></select>
    <!-- Fallback (voices unavailable): a free-text Piper voice name. -->
    <input id="tts_voice_text" type="text" placeholder="(default)">
  </label>

  <button id="save">Apply</button>
  <div id="status" class="status"></div>

<script>
  const $ = (id) => document.getElementById(id);
  const status = $('status');
  let MODELS = [];               // [{provider, id, label}] from /models
  let VOICES = [];               // [{name, language, label}] from /voices
  const CLOUD = ['anthropic', 'openai'];

  function show(ok, msg) {
    status.textContent = msg;
    status.className = 'status ' + (ok ? 'ok' : 'err');
  }

  // Show the right model control for the backend, and populate the dropdown with
  // that provider's last-12-months models (keeping `selected` even if off-list).
  function renderModel(backend, selected) {
    const sel = $('llm_model_select');
    const txt = $('llm_model_text');
    if (CLOUD.includes(backend)) {
      sel.style.display = '';
      txt.style.display = 'none';
      $('model_hint').textContent = 'last 12 months, per provider';
      const list = MODELS.filter((m) => m.provider === backend);
      sel.innerHTML = '<option value="">(backend default)</option>';
      let matched = !selected;
      for (const m of list) {
        const o = document.createElement('option');
        o.value = m.id; o.textContent = m.label || m.id;
        if (m.id === selected) { o.selected = true; matched = true; }
        sel.appendChild(o);
      }
      if (!matched) {   // a pinned model not in the fetched list — keep it visible
        const o = document.createElement('option');
        o.value = selected; o.textContent = selected + ' (pinned)'; o.selected = true;
        sel.appendChild(o);
      }
    } else {
      sel.style.display = 'none';
      txt.style.display = '';
      txt.value = selected || '';
      $('model_hint').textContent = 'e.g. qwen2.5, llama3.2';
    }
  }

  function currentModel() {
    const backend = $('llm_backend').value;
    const raw = CLOUD.includes(backend) ? $('llm_model_select').value : $('llm_model_text').value;
    return raw.trim() || null;
  }

  // Show a dropdown of installed voices when we have them, else a free-text field.
  // Keeps `selected` visible even if it isn't an installed voice.
  function renderVoice(selected) {
    const sel = $('tts_voice_select');
    const txt = $('tts_voice_text');
    if (VOICES.length) {
      sel.style.display = '';
      txt.style.display = 'none';
      $('voice_hint').textContent = 'installed voices';
      sel.innerHTML = '<option value="">(server default)</option>';
      let matched = !selected;
      for (const v of VOICES) {
        const o = document.createElement('option');
        o.value = v.name;
        o.textContent = v.language ? (v.label || v.name) + ' · ' + v.language : (v.label || v.name);
        if (v.name === selected) { o.selected = true; matched = true; }
        sel.appendChild(o);
      }
      if (!matched) {   // a voice not in the installed list — keep it visible
        const o = document.createElement('option');
        o.value = selected; o.textContent = selected + ' (not installed)'; o.selected = true;
        sel.appendChild(o);
      }
    } else {
      sel.style.display = 'none';
      txt.style.display = '';
      txt.value = selected || '';
      $('voice_hint').textContent = 'blank = server default';
    }
  }

  function currentVoice() {
    const raw = VOICES.length ? $('tts_voice_select').value : $('tts_voice_text').value;
    return raw.trim() || null;
  }

  // The Anthropic auth toggle only applies to the anthropic backend.
  function renderAuthRow(backend) {
    $('anthropic_auth_row').style.display = backend === 'anthropic' ? '' : 'none';
    renderKeyRows(backend);
  }

  // Show a provider's API-key field only when that backend is selected (and, for
  // Anthropic, only under API-key auth — subscription auth uses an OAuth token).
  function renderKeyRows(backend) {
    const auth = $('anthropic_auth').value;
    $('anthropic_key_row').style.display =
      (backend === 'anthropic' && auth === 'apikey') ? '' : 'none';
    $('openai_key_row').style.display = backend === 'openai' ? '' : 'none';
  }

  function fill(v) {
    $('engine').value = v.engine || 'native';
    $('llm_backend').value = v.llm_backend || 'ollama';
    $('anthropic_auth').value = v.anthropic_auth || 'apikey';
    $('anthropic_api_key').value = '';
    $('openai_api_key').value = '';
    $('anthropic_key_state').textContent = v.anthropic_key_set ? '(a key is set)' : '(no key set)';
    $('openai_key_state').textContent = v.openai_key_set ? '(a key is set)' : '(no key set)';
    renderAuthRow($('llm_backend').value);
    renderModel($('llm_backend').value, v.llm_model || '');
    renderVoice(v.tts_voice || '');
    $('web_search').checked = !!v.web_search;
    $('search_provider').value = v.search_provider || 'duckduckgo';
    $('search_api_key').value = '';
    $('key_state').textContent = v.search_key_set ? '(a key is set)' : '(no key set)';
  }

  async function loadModels() {
    try {
      const r = await fetch('/models');
      MODELS = (await r.json()).models || [];
    } catch (e) { MODELS = []; }
  }

  async function loadVoices() {
    try {
      const r = await fetch('/voices');
      VOICES = (await r.json()).voices || [];
    } catch (e) { VOICES = []; }
  }

  async function load() {
    await Promise.all([loadModels(), loadVoices()]);
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
      engine: $('engine').value,
      llm_backend: $('llm_backend').value,
      anthropic_auth: $('anthropic_auth').value,
      llm_model: currentModel(),
      tts_voice: currentVoice(),
      web_search: $('web_search').checked,
      search_provider: $('search_provider').value,
      // Only send a key when the user typed one; blank keeps the current key.
      search_api_key: $('search_api_key').value.trim() || undefined,
      anthropic_api_key: $('anthropic_api_key').value.trim() || undefined,
      openai_api_key: $('openai_api_key').value.trim() || undefined,
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

  // Switching backend resets the model to that backend's default + toggles auth row.
  $('llm_backend').addEventListener('change', () => {
    renderAuthRow($('llm_backend').value);
    renderModel($('llm_backend').value, '');
  });
  // Switching Anthropic auth toggles whether the API-key field is relevant.
  $('anthropic_auth').addEventListener('change', () => renderKeyRows($('llm_backend').value));
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
pub async fn serve(
    listener: TcpListener,
    settings: Arc<SharedSettings>,
    catalog: Arc<ModelCatalog>,
    connector: Arc<dyn ServiceConnector>,
    voices_dir: Option<PathBuf>,
) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let settings = settings.clone();
        let catalog = catalog.clone();
        let connector = connector.clone();
        let voices_dir = voices_dir.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, settings, catalog, connector, voices_dir).await {
                log::debug!("config page connection {peer} ended: {e:#}");
            }
        });
    }
}

/// Read one HTTP request, route it, write one response, close. One request per
/// connection (`Connection: close`) — ample for a settings page.
async fn handle(
    mut stream: TcpStream,
    settings: Arc<SharedSettings>,
    catalog: Arc<ModelCatalog>,
    connector: Arc<dyn ServiceConnector>,
    voices_dir: Option<PathBuf>,
) -> Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];

    // Read until the end of the header block.
    let header_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_REQUEST_BYTES {
            return write_response(
                &mut stream,
                "413 Payload Too Large",
                "text/plain",
                b"too large",
            )
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

    // The model list needs an async catalog fetch and the voice list an async Piper
    // `describe`, so both are handled here rather than in the pure `route` function.
    let path = target.split(['?', '#']).next().unwrap_or(&target);
    if method == "GET" && path == "/models" {
        let payload = models_json(&catalog).await.into_bytes();
        return write_response(&mut stream, "200 OK", "application/json", &payload).await;
    }
    if method == "GET" && path == "/voices" {
        // Reuse the exact device-facing logic: Piper's catalog intersected with the
        // installed voices when a voices dir is configured.
        let ev = crate::control::voices_response(connector.as_ref(), voices_dir.as_deref()).await;
        let payload = ev.data.to_string().into_bytes();
        return write_response(&mut stream, "200 OK", "application/json", &payload).await;
    }

    let (status, content_type, payload) = route(&method, &target, &body, &settings);
    write_response(&mut stream, status, content_type, &payload).await
}

/// The selectable models as a JSON string (`{ "ok": true, "models": [...] }`).
async fn models_json(catalog: &ModelCatalog) -> String {
    let models: Vec<Value> = catalog
        .models()
        .await
        .into_iter()
        .map(|m| json!({ "provider": m.provider, "id": m.id, "label": m.label }))
        .collect();
    json!({ "ok": true, "models": models }).to_string()
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
    let engine = match v.engine {
        LlmEngine::Native => "native",
        LlmEngine::Rig => "rig",
    };
    json!({
        "ok": ok,
        "message": message,
        "llm_backend": v.llm_backend,
        "llm_model": v.llm_model,
        "anthropic_key_set": v.anthropic_key_set,
        "openai_key_set": v.openai_key_set,
        "anthropic_auth": v.anthropic_auth.as_str(),
        "tts_voice": v.tts_voice,
        "engine": engine,
        "web_search": v.web_search,
        "search_provider": v.search_provider,
        "search_key_set": v.search_key_set,
        "end_silence_ms": v.end_silence_ms,
        "voice_rms_threshold": v.voice_rms_threshold,
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
    let engine =
        data.get("engine")
            .and_then(Value::as_str)
            .map(|e| match e.to_lowercase().as_str() {
                "rig" | "rig-core" | "rigcore" => LlmEngine::Rig,
                _ => LlmEngine::Native,
            });
    let web_search = data.get("web_search").and_then(Value::as_bool);
    // Key: absent/empty = leave unchanged (so a page reload never wipes it);
    // explicit JSON null = clear.
    let search_api_key = match data.get("search_api_key") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => match v.as_str() {
            Some(s) if !s.is_empty() => Some(Some(s.to_string())),
            _ => None,
        },
    };
    let anthropic_auth = data
        .get("anthropic_auth")
        .and_then(Value::as_str)
        .map(AnthropicAuth::from_label);
    // Provider API keys, same tri-state as the search key: absent/empty = leave
    // unchanged (a page reload never wipes a stored key); explicit JSON null = clear.
    let key_field = |key: &str| match data.get(key) {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => match v.as_str() {
            Some(s) if !s.is_empty() => Some(Some(s.to_string())),
            _ => None,
        },
    };
    SettingsUpdate {
        llm_backend: string_field("llm_backend"),
        llm_model: string_field("llm_model"),
        anthropic_api_key: key_field("anthropic_api_key"),
        openai_api_key: key_field("openai_api_key"),
        anthropic_auth,
        tts_voice,
        engine,
        web_search,
        search_provider: string_field("search_provider"),
        search_api_key,
        end_silence_ms: data.get("end_silence_ms").and_then(Value::as_u64),
        voice_rms_threshold: data.get("voice_rms_threshold").and_then(Value::as_f64),
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

    fn catalog() -> Arc<ModelCatalog> {
        // No keys → the static fallback list, so the test needs no network.
        Arc::new(ModelCatalog::new(
            "http://unused",
            None,
            "http://unused",
            None,
        ))
    }

    /// A connector whose `connect_tts` answers a Wyoming `describe` with a fixed
    /// voice catalog, so the `/voices` route can be exercised without a real Piper.
    struct VoiceConnector;

    #[async_trait::async_trait]
    impl ServiceConnector for VoiceConnector {
        async fn connect_stt(&self) -> Result<crate::wyoming::DynConnection> {
            anyhow::bail!("stt not used in config-page tests")
        }

        async fn connect_tts(&self) -> Result<crate::wyoming::DynConnection> {
            use crate::wyoming::protocol::{types, write_event, WyomingEvent};
            let (client, server) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                let (r, w) = tokio::io::split(server);
                let mut reader = tokio::io::BufReader::new(r);
                let mut writer = w;
                let _ = crate::wyoming::protocol::read_event(&mut reader).await;
                let info = WyomingEvent::with_data(
                    types::INFO,
                    json!({ "tts": [{ "voices": [
                        { "name": "en_US-amy-medium", "languages": ["en_US"], "description": "amy (medium)" },
                        { "name": "en_US-lessac-medium", "languages": ["en_US"], "description": "lessac (medium)" },
                    ]}]}),
                );
                let _ = write_event(&mut writer, &info).await;
            });
            let (r, w) = tokio::io::split(client);
            Ok(crate::wyoming::DynConnection::from_io(r, w))
        }
    }

    fn connector() -> Arc<dyn ServiceConnector> {
        Arc::new(VoiceConnector)
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
    fn post_config_toggles_engine_and_web_search() {
        let s = settings();
        let (status, _c, out) = route(
            "POST",
            "/config",
            br#"{"engine":"rig","web_search":true}"#,
            &s,
        );
        assert_eq!(status, "200 OK");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["engine"], "rig");
        assert_eq!(v["web_search"], true);
        assert!(s.view().web_search);
    }

    #[test]
    fn post_config_sets_search_provider_and_key_without_leaking_it() {
        let s = settings();
        let (status, _c, out) = route(
            "POST",
            "/config",
            br#"{"search_provider":"tavily","search_api_key":"tvly-secret"}"#,
            &s,
        );
        assert_eq!(status, "200 OK");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["search_provider"], "tavily");
        assert_eq!(v["search_key_set"], true);
        // The key value must never appear in a POST response or a later GET.
        assert!(!String::from_utf8_lossy(&out).contains("tvly-secret"));
        let (_s, _c, g) = route("GET", "/config", b"", &s);
        assert!(!String::from_utf8_lossy(&g).contains("tvly-secret"));
        assert!(s.view().search_key_set);
    }

    #[test]
    fn post_config_sets_anthropic_key_and_selects_the_backend_without_leaking_it() {
        let s = settings(); // mock backend, no env anthropic key
        assert!(!s.view().anthropic_key_set);
        // A runtime key + backend switch in one POST enables the cloud backend with
        // no restart (mirrors entering the key on the page and choosing anthropic).
        let (status, _c, out) = route(
            "POST",
            "/config",
            br#"{"llm_backend":"anthropic","anthropic_api_key":"sk-secret"}"#,
            &s,
        );
        assert_eq!(status, "200 OK");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["llm_backend"], "anthropic");
        assert_eq!(v["anthropic_key_set"], true);
        // The key must never appear in a POST response or a later GET.
        assert!(!String::from_utf8_lossy(&out).contains("sk-secret"));
        let (_s, _c, g) = route("GET", "/config", b"", &s);
        assert!(!String::from_utf8_lossy(&g).contains("sk-secret"));
        assert!(s.view().anthropic_key_set);
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
            handle(stream, s, catalog(), connector(), None).await.unwrap();
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

    /// `GET /models` returns the catalog JSON (static fallback here, no network).
    #[tokio::test]
    async fn serves_the_model_catalog() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let s = settings();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle(stream, s, catalog(), connector(), None).await.unwrap();
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /models HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();

        let mut resp = String::new();
        client.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "resp: {resp}");
        assert!(resp.contains("claude-opus-5"), "resp: {resp}");
        assert!(resp.contains("gpt-4o-mini"), "resp: {resp}");
    }

    /// `GET /voices` returns the installed-voice list, filtered to a voices dir.
    #[tokio::test]
    async fn serves_the_voice_catalog_filtered_to_installed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A voices dir with only amy installed → lessac is filtered out even though
        // the mock Piper advertises both.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ambient-webcfg-voices-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("en_US-amy-medium.onnx"), b"").unwrap();

        let s = settings();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dir_for_task = dir.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle(stream, s, catalog(), connector(), Some(dir_for_task))
                .await
                .unwrap();
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /voices HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();

        let mut resp = String::new();
        client.read_to_string(&mut resp).await.unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert!(resp.starts_with("HTTP/1.1 200 OK"), "resp: {resp}");
        assert!(resp.contains("en_US-amy-medium"), "resp: {resp}");
        assert!(!resp.contains("en_US-lessac-medium"), "resp: {resp}");
    }
}
