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
//! It also serves a small set of **read-only debug pages** over the same socket
//! ([`DebugSources`]) so you can inspect what the assistant is doing from a
//! browser: the chat log, the exact prompt sent to the LLM, the SQLite memory
//! store, and the HelixDB GraphRAG store. These are strictly read-only.
//!
//! Routes:
//! - `GET /`         → the HTML config page.
//! - `GET /config`   → the live [`SettingsView`](crate::settings::SettingsView) as JSON.
//! - `GET /models`   → the selectable LLM models for the model dropdown.
//! - `GET /voices`   → the installed Piper voices for the TTS voice dropdown.
//! - `POST /config`  → apply a `{llm_backend?, llm_model?, tts_voice?}` change
//!   (same JSON shape as the `ambient-set-settings` control frame; a `tts_voice`
//!   of `null` clears the voice) and return the resulting settings.
//! - `GET /chatlog`     → debug page; `GET /chatlog.json?limit=N` → recent turns.
//! - `GET /prompts`     → debug page; `GET /prompts.json?limit=N` → recent LLM prompts.
//! - `GET /sqlite`      → debug page; `GET /sqlite.json`  → the memory store rows.
//! - `GET /helix`       → debug page; `GET /helix.json`   → GraphRAG node stats + sample.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::llm::anthropic_auth::AnthropicAuth;
use crate::llm::catalog::ModelCatalog;
use crate::memory::{chatlog, promptlog, GraphView, MemoryStore};
use crate::music::{ManagedProc, MusicHub};
use crate::orchestrator::ServiceConnector;
use crate::settings::{LlmEngine, SettingsUpdate, SharedSettings};

/// Read-only data sources the debug pages render (chat log, prompts, SQLite,
/// HelixDB). Cheaply cloneable — everything is behind an `Arc` or a small `PathBuf`.
#[derive(Clone)]
pub struct DebugSources {
    /// The SQLite memory store (rendered by `/sqlite`).
    pub memory: Arc<MemoryStore>,
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
}

/// Default cap on how many log records a debug page returns per request.
const DEFAULT_LOG_LIMIT: usize = 100;
/// Hard cap, so a hand-typed `?limit=` can't ask the server to buffer the world.
const MAX_LOG_LIMIT: usize = 1000;

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
  .nav { display: flex; gap: 0.4rem; flex-wrap: wrap; margin-bottom: 1.5rem;
         border-bottom: 1px solid rgba(128,128,128,0.3); padding-bottom: 0.75rem; }
  .nav a { text-decoration: none; padding: 0.3rem 0.7rem; border-radius: 6px;
           color: inherit; opacity: 0.75; }
  .nav a.active { background: rgba(128,128,128,0.18); opacity: 1; font-weight: 600; }
  .nav a:hover { opacity: 1; }
</style>
</head>
<body>
  <nav class="nav">
    <a href="/" class="active">Config</a>
    <a href="/music">Music</a>
    <a href="/chatlog">Chat log</a>
    <a href="/prompts">Prompts</a>
    <a href="/sqlite">SQLite</a>
    <a href="/helix">HelixDB</a>
  </nav>
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

/// Shared styling for the debug pages (chat log / prompts / SQLite / HelixDB).
/// Wider than the config form and table-oriented. Inlined, no build step.
const SHELL_STYLE: &str = r#"
  :root { color-scheme: light dark; }
  body { font: 14px/1.55 system-ui, sans-serif; max-width: 64rem; margin: 2rem auto; padding: 0 1rem; }
  h1 { font-size: 1.3rem; margin: 0 0 0.25rem; }
  h2 { font-size: 1.05rem; margin: 1.5rem 0 0.25rem; }
  .sub { opacity: 0.7; margin-top: 0; }
  .nav { display: flex; gap: 0.4rem; flex-wrap: wrap; margin-bottom: 1.5rem;
         border-bottom: 1px solid rgba(128,128,128,0.3); padding-bottom: 0.75rem; }
  .nav a { text-decoration: none; padding: 0.3rem 0.7rem; border-radius: 6px; color: inherit; opacity: 0.75; }
  .nav a.active { background: rgba(128,128,128,0.18); opacity: 1; font-weight: 600; }
  .nav a:hover { opacity: 1; }
  table { border-collapse: collapse; width: 100%; margin-top: 0.5rem; }
  th, td { text-align: left; vertical-align: top; padding: 0.4rem 0.6rem;
           border-bottom: 1px solid rgba(128,128,128,0.25); }
  th { font-weight: 600; white-space: nowrap; }
  td.pre { white-space: pre-wrap; word-break: break-word; }
  .muted { opacity: 0.6; }
  .toolbar { margin: 1rem 0; display: flex; gap: 0.6rem; align-items: center; flex-wrap: wrap; }
  button, select { font: inherit; padding: 0.35rem 0.7rem; cursor: pointer; }
  .badge { display: inline-block; padding: 0.05rem 0.45rem; border-radius: 4px;
           background: rgba(128,128,128,0.18); font-size: 0.82rem; }
  details { margin: 0.35rem 0; border-bottom: 1px solid rgba(128,128,128,0.2); padding-bottom: 0.35rem; }
  details > summary { cursor: pointer; }
  pre { white-space: pre-wrap; word-break: break-word; margin: 0.25rem 0 0.75rem;
        font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.85rem; }
"#;

/// Client-side helpers shared by every debug page.
const SHELL_SCRIPT: &str = r#"
  function esc(s){ return (s==null?'':String(s)).replace(/[&<>]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;'}[c])); }
  function fmtTime(ts){ if(!ts) return ''; try { return new Date(ts*1000).toLocaleString(); } catch(e){ return String(ts); } }
  async function getJSON(url){ const r = await fetch(url); return r.json(); }
"#;

/// Nav bar markup with `active` highlighted (same links as the config page).
fn nav_html(active: &str) -> String {
    const LINKS: [(&str, &str); 6] = [
        ("/", "Config"),
        ("/music", "Music"),
        ("/chatlog", "Chat log"),
        ("/prompts", "Prompts"),
        ("/sqlite", "SQLite"),
        ("/helix", "HelixDB"),
    ];
    let items: String = LINKS
        .iter()
        .map(|(href, label)| {
            let cls = if *href == active {
                " class=\"active\""
            } else {
                ""
            };
            format!("<a href=\"{href}\"{cls}>{label}</a>")
        })
        .collect();
    format!("<nav class=\"nav\">{items}</nav>")
}

/// Wrap a page `body` in the shared HTML shell (head, style, nav, shared script).
fn page(active: &str, title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Ambient — {title}</title><style>{SHELL_STYLE}</style></head>\
         <body>{nav}<h1>{title}</h1>{body}<script>{SHELL_SCRIPT}</script></body></html>",
        nav = nav_html(active),
    )
}

/// `/chatlog` body — every completed turn, newest first.
const CHATLOG_BODY: &str = r#"<p class="sub">Every completed turn (transcript + reply), newest first.</p>
<div class="toolbar">
  <button onclick="load()">Refresh</button>
  <label>Show <select id="limit" onchange="load()">
    <option>50</option><option selected>100</option><option>250</option><option>1000</option>
  </select> records</label>
  <span id="meta" class="muted"></span>
</div>
<div id="out" class="muted">Loading…</div>
<script>
async function load(){
  const n = document.getElementById('limit').value;
  const out = document.getElementById('out');
  try{
    const j = await getJSON('/chatlog.json?limit=' + n);
    if(!j.ok){ out.textContent = j.message || 'Error'; return; }
    const recs = j.records || [];
    document.getElementById('meta').textContent = recs.length + ' shown';
    if(!recs.length){ out.innerHTML = '<p class="muted">No turns logged yet.</p>'; return; }
    let h = '<table><thead><tr><th>Time</th><th>Speaker</th><th>Model</th><th>User</th><th>Assistant</th><th>Memories</th></tr></thead><tbody>';
    for(const r of recs){
      const who = esc(r.speaker_name || r.speaker_id || '');
      const model = esc([r.llm_backend, r.model].filter(Boolean).join(' / '));
      const mems = (r.memories_written||[]).map(m => '<div>'+esc(m)+'</div>').join('') || '<span class="muted">—</span>';
      h += '<tr><td class="muted">'+esc(fmtTime(r.ts))+'</td><td>'+who+'</td><td><span class="badge">'+model+'</span></td>'
        +  '<td class="pre">'+esc(r.transcript)+'</td><td class="pre">'+esc(r.reply)+'</td><td>'+mems+'</td></tr>';
    }
    out.innerHTML = h + '</tbody></table>';
  }catch(e){ out.textContent = 'Request failed: ' + e; }
}
load();
</script>"#;

/// `/prompts` body — the exact assembled LLM prompt per turn, newest first.
const PROMPTS_BODY: &str = r#"<p class="sub">The exact prompt sent to the LLM each turn (system prompt + user message), newest first.</p>
<div class="toolbar">
  <button onclick="load()">Refresh</button>
  <label>Show <select id="limit" onchange="load()">
    <option>50</option><option selected>100</option><option>250</option><option>1000</option>
  </select> records</label>
  <span id="meta" class="muted"></span>
</div>
<div id="out" class="muted">Loading…</div>
<script>
async function load(){
  const n = document.getElementById('limit').value;
  const out = document.getElementById('out');
  try{
    const j = await getJSON('/prompts.json?limit=' + n);
    if(!j.ok){ out.textContent = j.message || 'Error'; return; }
    const recs = j.records || [];
    document.getElementById('meta').textContent = recs.length + ' shown';
    if(!recs.length){ out.innerHTML = '<p class="muted">No prompts logged yet. Prompts are recorded when the assistant answers a turn.</p>'; return; }
    let h = '';
    for(const r of recs){
      const model = esc([r.llm_backend, r.model].filter(Boolean).join(' / '));
      const who = esc(r.speaker_name || r.speaker_id || '');
      const preview = esc((r.user_message||'').slice(0,90));
      h += '<details><summary>'+esc(fmtTime(r.ts))+' — <span class="badge">'+model+'</span> '+who+' — '+preview+'</summary>'
        +  '<p class="muted">User message</p><pre>'+esc(r.user_message)+'</pre>'
        +  '<p class="muted">System prompt</p><pre>'+esc(r.system_prompt)+'</pre></details>';
    }
    out.innerHTML = h;
  }catch(e){ out.textContent = 'Request failed: ' + e; }
}
load();
</script>"#;

/// `/sqlite` body — the persistent memory store rows.
const SQLITE_BODY: &str = r#"<p class="sub">Persistent memory (SQLite): stored facts &amp; preferences.</p>
<div class="toolbar"><button onclick="load()">Refresh</button><span id="meta" class="muted"></span></div>
<div id="out" class="muted">Loading…</div>
<script>
async function load(){
  const out = document.getElementById('out');
  try{
    const j = await getJSON('/sqlite.json');
    if(!j.ok){ out.textContent = j.message || 'Error'; return; }
    document.getElementById('meta').textContent = j.count + ' rows · db: ' + esc(j.db_path) + ' · recall backend: ' + esc(j.memory_backend);
    const rows = j.memories || [];
    if(!rows.length){ out.innerHTML = '<p class="muted">No memories stored yet.</p>'; return; }
    let h = '<table><thead><tr><th>id</th><th>kind</th><th>source</th><th>speaker</th><th>created</th><th>content</th></tr></thead><tbody>';
    for(const m of rows){
      h += '<tr><td class="muted">'+esc(m.id)+'</td><td><span class="badge">'+esc(m.kind)+'</span></td><td>'+esc(m.source)+'</td>'
        +  '<td>'+esc(m.speaker_id||'household')+'</td><td class="muted">'+esc(fmtTime(m.created_at))+'</td><td class="pre">'+esc(m.content)+'</td></tr>';
    }
    out.innerHTML = h + '</tbody></table>';
  }catch(e){ out.textContent = 'Request failed: ' + e; }
}
load();
</script>"#;

/// `/helix` body — GraphRAG node counts + a sample of nodes per label.
const HELIX_BODY: &str = r#"<p class="sub">GraphRAG memory (embedded HelixDB): nodes built from turns &amp; memories. Entity names are editable — fix a spelling mistake with "Edit name".</p>
<div class="toolbar"><button onclick="load()">Refresh</button><span id="meta" class="muted"></span></div>
<div id="out" class="muted">Loading…</div>
<script>
// esc() handles &<>; also escape quotes for use inside an HTML attribute.
function attr(s){ return esc(s).replace(/"/g,'&quot;').replace(/'/g,'&#39;'); }
function nodeTable(label, rows){
  if(!rows || !rows.length) return '<h2>'+esc(label)+' <span class="muted">(0)</span></h2>';
  const editable = (label === 'Entity');
  const cols = Object.keys(rows[0]).filter(k => k !== '$id');
  let h = '<h2>'+esc(label)+' <span class="muted">('+rows.length+' shown)</span></h2><table><thead><tr><th>id</th>';
  for(const c of cols) h += '<th>'+esc(c)+'</th>';
  if(editable) h += '<th></th>';
  h += '</tr></thead><tbody>';
  for(const r of rows){
    h += '<tr><td class="muted">'+esc(r['$id'])+'</td>';
    for(const c of cols) h += '<td class="pre">'+esc(r[c])+'</td>';
    if(editable){
      const name = r['name']==null ? '' : String(r['name']);
      h += '<td><button data-rename="'+attr(name)+'">Edit name</button></td>';
    }
    h += '</tr>';
  }
  return h + '</tbody></table>';
}
async function renameEntity(oldName){
  const next = prompt('Correct the entity name:', oldName);
  if(next === null) return;                 // cancelled
  const newName = next.trim();
  if(!newName || newName === oldName) return;
  try{
    const res = await fetch('/helix/rename-entity', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ old_name: oldName, new_name: newName }),
    });
    const j = await res.json();
    if(!j.ok){ alert(j.message || 'Rename failed'); return; }
    alert('Fixed "'+oldName+'" → "'+newName+'": '+(j.entities||0)+' entity, '+(j.turns||0)+' turn, '+(j.memories||0)+' memory node(s).');
    load();
  }catch(e){ alert('Request failed: ' + e); }
}
async function load(){
  const out = document.getElementById('out');
  try{
    const j = await getJSON('/helix.json');
    if(!j.ok){ out.textContent = j.message || 'Error'; return; }
    if(!j.enabled){
      document.getElementById('meta').textContent = '';
      out.innerHTML = '<p class="muted">'+esc(j.message||'GraphRAG (HelixDB) is not active.')+'</p>';
      return;
    }
    const s = j.stats || {}; const by = s.by_label || {};
    document.getElementById('meta').textContent = (s.total||0) + ' nodes · store: ' + esc(j.helix_path);
    let h = '<p>'+Object.keys(by).map(k => esc(k)+': '+esc(by[k])).join(' · ')+'</p>';
    const nodes = j.nodes || {};
    for(const label of Object.keys(nodes)) h += nodeTable(label, nodes[label]);
    out.innerHTML = h;
    for(const btn of out.querySelectorAll('button[data-rename]')){
      btn.addEventListener('click', () => renameEntity(btn.getAttribute('data-rename')));
    }
  }catch(e){ out.textContent = 'Request failed: ' + e; }
}
load();
</script>"#;

/// `/music` body — start/stop the music sibling processes and see snapserver status.
const MUSIC_BODY: &str = r#"<p class="sub">Start/stop the local music processes (snapserver, librespot, mpv) and see snapserver status. The orchestrator launches these; it never handles the audio.</p>
<div id="disabled" class="muted" style="display:none"></div>
<div id="panel" style="display:none">
  <h2>Processes</h2>
  <div class="toolbar">
    <button onclick="startall()">Start all</button>
    <button onclick="stopall()">Stop all</button>
    <button onclick="load()">Refresh</button>
    <span id="meta" class="muted"></span>
  </div>
  <table><thead><tr><th>Process</th><th>Status</th><th>PID</th><th></th><th>Log</th></tr></thead><tbody id="procs"></tbody></table>

  <h2>Web-URL player</h2>
  <div class="toolbar">
    <input id="url" type="text" placeholder="https://stream-url or file path" style="min-width:22rem; padding:0.35rem 0.6rem">
    <button onclick="play()">Play</button>
    <button onclick="stopweb()">Stop</button>
    <span id="webmsg" class="muted"></span>
  </div>

  <h2>Snapserver</h2>
  <div id="snap" class="muted">Loading…</div>
</div>
<script>
async function proc(key, action){
  try{
    const r = await fetch('/music/proc', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({proc:key, action})});
    const j = await r.json();
    if(!j.ok) document.getElementById('meta').textContent = j.message || 'Error';
  }catch(e){ document.getElementById('meta').textContent = 'Request failed: ' + e; }
  setTimeout(load, 500);
}
async function postAction(url){
  try{ const r = await fetch(url, {method:'POST'}); const j = await r.json();
    if(!j.ok) document.getElementById('meta').textContent = j.message || 'Error';
  }catch(e){ document.getElementById('meta').textContent = 'Request failed: ' + e; }
  setTimeout(load, 700);
}
async function startall(){ await postAction('/music/startall'); }
async function stopall(){ await postAction('/music/stopall'); }
async function play(){
  const url = document.getElementById('url').value.trim(); const msg = document.getElementById('webmsg');
  if(!url){ msg.textContent = 'Enter a URL.'; return; }
  try{
    const r = await fetch('/music/play', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({url})});
    const j = await r.json(); msg.textContent = j.ok ? 'Playing.' : (j.message || 'Error');
  }catch(e){ msg.textContent = 'Request failed: ' + e; }
  setTimeout(load, 500);
}
async function stopweb(){
  const msg = document.getElementById('webmsg');
  try{ const r = await fetch('/music/stopweb', {method:'POST'}); const j = await r.json(); msg.textContent = j.ok ? 'Stopped.' : (j.message || 'Error'); }
  catch(e){ msg.textContent = 'Request failed: ' + e; }
}
async function load(){
  let j;
  try{ j = await getJSON('/music/status.json'); }
  catch(e){ document.getElementById('meta').textContent = 'Request failed: ' + e; return; }
  const dis = document.getElementById('disabled'), panel = document.getElementById('panel');
  if(!j.enabled){
    dis.style.display = ''; panel.style.display = 'none';
    dis.innerHTML = 'Music routing is disabled. Restart the orchestrator with <code>AMBIENT_MUSIC=on</code> to enable this panel.';
    return;
  }
  dis.style.display = 'none'; panel.style.display = '';
  const procs = j.procs || [];
  document.getElementById('meta').textContent = procs.filter(p => p.running).length + ' of ' + procs.length + ' running';
  let h = '';
  for(const p of procs){
    const badge = p.running
      ? '<span class="badge" style="background:rgba(46,160,67,0.2)">running</span>'
      : '<span class="badge">stopped</span>';
    const btn = p.running ? '<button data-stop="'+esc(p.key)+'">Stop</button>' : '<button data-start="'+esc(p.key)+'">Start</button>';
    h += '<tr><td>'+esc(p.label)+'</td><td>'+badge+'</td><td class="muted">'+esc(p.pid||'')+'</td><td>'+btn+'</td><td class="muted pre">'+esc(p.log_path)+'</td></tr>';
  }
  document.getElementById('procs').innerHTML = h;
  for(const b of document.querySelectorAll('button[data-start]')) b.addEventListener('click', () => proc(b.getAttribute('data-start'), 'start'));
  for(const b of document.querySelectorAll('button[data-stop]'))  b.addEventListener('click', () => proc(b.getAttribute('data-stop'), 'stop'));
  const snap = document.getElementById('snap');
  const ss = j.snapserver || {};
  if(!ss.reachable){ snap.innerHTML = '<span class="muted">snapserver not reachable at '+esc(j.snapserver_addr||'')+' — start it above.</span>'; return; }
  const groups = ss.groups || [];
  if(!groups.length){ snap.innerHTML = '<span class="muted">Connected at '+esc(j.snapserver_addr||'')+'. No groups yet.</span>'; return; }
  let s = '<table><thead><tr><th>Group</th><th>Stream</th><th>Muted</th><th>Clients</th></tr></thead><tbody>';
  for(const g of groups){
    const clients = (g.clients||[]).map(c => esc(c.name||c.id) + ' (' + esc(c.volume) + '%' + (c.muted?' muted':'') + ')').join(', ');
    s += '<tr><td>'+esc(g.id)+'</td><td><span class="badge">'+esc(g.stream)+'</span></td><td>'+(g.muted?'yes':'no')+'</td><td class="pre">'+clients+'</td></tr>';
  }
  snap.innerHTML = s + '</tbody></table>';
}
load();
</script>"#;

/// Cap on request bytes we buffer before the body — a config request is tiny; this
/// just bounds a misbehaving/hostile client on the (unauthenticated) socket.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Accept config-page connections forever, one task per connection. Returns only if
/// the listener itself fails.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    listener: TcpListener,
    settings: Arc<SharedSettings>,
    catalog: Arc<ModelCatalog>,
    connector: Arc<dyn ServiceConnector>,
    voices_dir: Option<PathBuf>,
    debug: DebugSources,
    music: Option<MusicHub>,
) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let settings = settings.clone();
        let catalog = catalog.clone();
        let connector = connector.clone();
        let voices_dir = voices_dir.clone();
        let debug = debug.clone();
        let music = music.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(
                stream, settings, catalog, connector, voices_dir, debug, music,
            )
            .await
            {
                log::debug!("config page connection {peer} ended: {e:#}");
            }
        });
    }
}

/// Read one HTTP request, route it, write one response, close. One request per
/// connection (`Connection: close`) — ample for a settings page.
#[allow(clippy::too_many_arguments)]
async fn handle(
    mut stream: TcpStream,
    settings: Arc<SharedSettings>,
    catalog: Arc<ModelCatalog>,
    connector: Arc<dyn ServiceConnector>,
    voices_dir: Option<PathBuf>,
    debug: DebugSources,
    music: Option<MusicHub>,
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

    // Debug/inspection data endpoints — need I/O and the debug sources, so they're
    // handled here (like `/models`) rather than in the pure `route` function.
    if method == "GET" {
        let json = match path {
            "/chatlog.json" => Some(chatlog_json(&debug, limit_param(&target))),
            "/prompts.json" => Some(prompts_json(&debug, limit_param(&target))),
            "/sqlite.json" => Some(sqlite_json(&debug)),
            "/helix.json" => Some(helix_json(&debug).await),
            _ => None,
        };
        if let Some(body) = json {
            return write_response(&mut stream, "200 OK", "application/json", body.as_bytes())
                .await;
        }
    }

    // The only debug mutation: rename a GraphRAG `Entity` (fix a misspelled fact).
    // Needs the async graph handle + request body, so it's handled here.
    if method == "POST" && path == "/helix/rename-entity" {
        let payload = helix_rename_json(&debug, &body).await;
        return write_response(
            &mut stream,
            "200 OK",
            "application/json",
            payload.as_bytes(),
        )
        .await;
    }

    // Music control endpoints — need async I/O (process spawn, snapserver JSON-RPC,
    // mpv IPC) and the `MusicHub`, so they're handled here like `/models`.
    if method == "GET" && path == "/music/status.json" {
        let payload = music_status_json(music.as_ref()).await.into_bytes();
        return write_response(&mut stream, "200 OK", "application/json", &payload).await;
    }
    if method == "POST" && path == "/music/proc" {
        let payload = music_proc_json(music.as_ref(), &body).await.into_bytes();
        return write_response(&mut stream, "200 OK", "application/json", &payload).await;
    }
    if method == "POST" && path == "/music/play" {
        let payload = music_play_json(music.as_ref(), &body).await.into_bytes();
        return write_response(&mut stream, "200 OK", "application/json", &payload).await;
    }
    if method == "POST" && path == "/music/stopweb" {
        let payload = music_stopweb_json(music.as_ref()).await.into_bytes();
        return write_response(&mut stream, "200 OK", "application/json", &payload).await;
    }
    if method == "POST" && path == "/music/startall" {
        let payload = music_startall_json(music.as_ref()).await.into_bytes();
        return write_response(&mut stream, "200 OK", "application/json", &payload).await;
    }
    if method == "POST" && path == "/music/stopall" {
        let payload = music_stopall_json(music.as_ref()).await.into_bytes();
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

/// Parse a `?limit=N` query parameter, clamped to `[1, MAX_LOG_LIMIT]`; absent or
/// unparseable falls back to [`DEFAULT_LOG_LIMIT`].
fn limit_param(target: &str) -> usize {
    target
        .split(['?', '#'])
        .nth(1)
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|pair| pair.strip_prefix("limit=")?.parse::<usize>().ok())
        .map(|n| n.clamp(1, MAX_LOG_LIMIT))
        .unwrap_or(DEFAULT_LOG_LIMIT)
}

/// `{ ok, records: [ChatLogRecord…] }` for the newest `limit` turns.
fn chatlog_json(debug: &DebugSources, limit: usize) -> String {
    match chatlog::read_tail(&debug.chatlog_path, limit) {
        Ok(records) => json!({ "ok": true, "records": records }).to_string(),
        Err(e) => json!({ "ok": false, "message": format!("{e:#}") }).to_string(),
    }
}

/// `{ ok, records: [PromptLogRecord…] }` for the newest `limit` prompts.
fn prompts_json(debug: &DebugSources, limit: usize) -> String {
    match promptlog::read_tail(&debug.promptlog_path, limit) {
        Ok(records) => json!({ "ok": true, "records": records }).to_string(),
        Err(e) => json!({ "ok": false, "message": format!("{e:#}") }).to_string(),
    }
}

/// `{ ok, count, db_path, memory_backend, memories: [...] }` — the whole memory store.
fn sqlite_json(debug: &DebugSources) -> String {
    match debug.memory.list() {
        Ok(items) => {
            let memories: Vec<Value> = items
                .iter()
                .map(|m| {
                    json!({
                        "id": m.id,
                        "kind": m.kind.as_str(),
                        "content": m.content,
                        "source": m.source.as_str(),
                        "created_at": m.created_at,
                        "speaker_id": m.speaker_id,
                    })
                })
                .collect();
            json!({
                "ok": true,
                "count": memories.len(),
                "db_path": debug.db_path.display().to_string(),
                "memory_backend": debug.memory_backend,
                "memories": memories,
            })
            .to_string()
        }
        Err(e) => json!({ "ok": false, "message": format!("{e:#}") }).to_string(),
    }
}

/// `{ ok, enabled, helix_path, stats, nodes }` — GraphRAG store overview, or
/// `{ ok, enabled: false, message }` when the graph backend is not live.
async fn helix_json(debug: &DebugSources) -> String {
    let Some(graph) = &debug.graph else {
        return json!({
            "ok": true,
            "enabled": false,
            "message": "GraphRAG (HelixDB) is not active. Start with AMBIENT_MEMORY_BACKEND=helix \
                        (binary built with the `helix` feature) to enable it.",
        })
        .to_string();
    };
    let stats = match graph.stats().await {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "message": format!("{e:#}") }).to_string(),
    };
    let nodes = match graph.sample(50).await {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "message": format!("{e:#}") }).to_string(),
    };
    json!({
        "ok": true,
        "enabled": true,
        "helix_path": debug.helix_path.display().to_string(),
        "stats": stats,
        "nodes": nodes,
    })
    .to_string()
}

/// POST `/helix/rename-entity` — body `{ "old_name": "...", "new_name": "..." }`.
/// Fixes a misspelled entity everywhere: the `Entity` node's `name` (id + edges
/// preserved) plus whole-word occurrences in `Turn.text` / `Memory.content`.
/// Returns `{ ok: true, entities, turns, memories, total }`, or `{ ok: false,
/// message }` when the graph is inactive, the request is malformed, the name was
/// found nowhere, or the target already names a different entity.
async fn helix_rename_json(debug: &DebugSources, body: &[u8]) -> String {
    let Some(graph) = &debug.graph else {
        return json!({ "ok": false, "message": "GraphRAG (HelixDB) is not active." }).to_string();
    };
    let data: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return json!({ "ok": false, "message": format!("invalid JSON: {e}") }).to_string()
        }
    };
    let old_name = data
        .get("old_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let new_name = data
        .get("new_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if old_name.is_empty() || new_name.is_empty() {
        return json!({
            "ok": false,
            "message": "rename requires non-empty `old_name` and `new_name`",
        })
        .to_string();
    }
    match graph.rename_entity(old_name, new_name).await {
        Ok(counts) => {
            let total = counts.get("total").and_then(Value::as_u64).unwrap_or(0);
            if total == 0 {
                json!({ "ok": false, "message": format!("no occurrences of {old_name:?} found") })
                    .to_string()
            } else {
                json!({
                    "ok": true,
                    "entities": counts.get("entities").cloned().unwrap_or(json!(0)),
                    "turns": counts.get("turns").cloned().unwrap_or(json!(0)),
                    "memories": counts.get("memories").cloned().unwrap_or(json!(0)),
                    "total": total,
                })
                .to_string()
            }
        }
        Err(e) => json!({ "ok": false, "message": format!("{e:#}") }).to_string(),
    }
}

/// `GET /music/status.json` — supervised process state + live snapserver status.
/// `{ ok, enabled, snapserver_addr, procs: [...], snapserver: { reachable, groups } }`.
async fn music_status_json(music: Option<&MusicHub>) -> String {
    let Some(hub) = music else {
        return json!({ "ok": true, "enabled": false }).to_string();
    };
    let procs = serde_json::to_value(hub.supervisor.status().await).unwrap_or_else(|_| json!([]));
    let snapserver = match hub.snapcast.get_status().await {
        Ok(status) => {
            let groups: Vec<Value> = status
                .groups
                .iter()
                .map(|g| {
                    let clients: Vec<Value> = g
                        .clients
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "name": c.host.name,
                                "volume": c.volume_percent(),
                                "muted": c.volume_muted(),
                            })
                        })
                        .collect();
                    json!({ "id": g.id, "stream": g.stream_id, "muted": g.muted, "clients": clients })
                })
                .collect();
            json!({ "reachable": true, "groups": groups })
        }
        Err(_) => json!({ "reachable": false }),
    };
    json!({
        "ok": true,
        "enabled": true,
        "snapserver_addr": hub.snapserver_addr,
        "procs": procs,
        "snapserver": snapserver,
    })
    .to_string()
}

/// `POST /music/proc` — body `{ proc, action }` starts/stops a managed process.
async fn music_proc_json(music: Option<&MusicHub>, body: &[u8]) -> String {
    let Some(hub) = music else {
        return json!({ "ok": false, "message": "music routing is disabled" }).to_string();
    };
    let data: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return json!({ "ok": false, "message": format!("invalid JSON: {e}") }).to_string()
        }
    };
    let Some(proc) = data
        .get("proc")
        .and_then(Value::as_str)
        .and_then(ManagedProc::from_key)
    else {
        return json!({ "ok": false, "message": "unknown proc" }).to_string();
    };
    let action = data.get("action").and_then(Value::as_str).unwrap_or("");
    let res = match action {
        "start" => hub.supervisor.start(proc).await,
        "stop" => hub.supervisor.stop(proc).await,
        other => Err(anyhow::anyhow!("unknown action {other:?}")),
    };
    match res {
        Ok(()) => json!({ "ok": true }).to_string(),
        Err(e) => json!({ "ok": false, "message": format!("{e:#}") }).to_string(),
    }
}

/// `POST /music/play` — body `{ url }` loads a URL in the mpv web player.
async fn music_play_json(music: Option<&MusicHub>, body: &[u8]) -> String {
    let Some(hub) = music else {
        return json!({ "ok": false, "message": "music routing is disabled" }).to_string();
    };
    let Some(mpv) = &hub.mpv else {
        return json!({ "ok": false, "message": "no mpv IPC configured (AMBIENT_MUSIC_WEB_IPC)" })
            .to_string();
    };
    let data: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return json!({ "ok": false, "message": format!("invalid JSON: {e}") }).to_string()
        }
    };
    let url = data.get("url").and_then(Value::as_str).unwrap_or("").trim();
    if url.is_empty() {
        return json!({ "ok": false, "message": "empty url" }).to_string();
    }
    match mpv.load(url).await {
        Ok(()) => json!({ "ok": true }).to_string(),
        Err(e) => json!({ "ok": false, "message": format!("{e:#}") }).to_string(),
    }
}

/// `POST /music/stopweb` — stop the mpv web player.
async fn music_stopweb_json(music: Option<&MusicHub>) -> String {
    let Some(hub) = music else {
        return json!({ "ok": false, "message": "music routing is disabled" }).to_string();
    };
    let Some(mpv) = &hub.mpv else {
        return json!({ "ok": false, "message": "no mpv IPC configured (AMBIENT_MUSIC_WEB_IPC)" })
            .to_string();
    };
    match mpv.stop().await {
        Ok(()) => json!({ "ok": true }).to_string(),
        Err(e) => json!({ "ok": false, "message": format!("{e:#}") }).to_string(),
    }
}

/// `POST /music/startall` — start all managed processes (skips a snapserver that is
/// already running).
async fn music_startall_json(music: Option<&MusicHub>) -> String {
    match music {
        Some(hub) => {
            hub.start_all().await;
            json!({ "ok": true }).to_string()
        }
        None => json!({ "ok": false, "message": "music routing is disabled" }).to_string(),
    }
}

/// `POST /music/stopall` — stop every process this orchestrator started.
async fn music_stopall_json(music: Option<&MusicHub>) -> String {
    match music {
        Some(hub) => {
            hub.stop_all().await;
            json!({ "ok": true }).to_string()
        }
        None => json!({ "ok": false, "message": "music routing is disabled" }).to_string(),
    }
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
        ("GET", "/chatlog") => (
            "200 OK",
            "text/html; charset=utf-8",
            page("/chatlog", "Chat log", CHATLOG_BODY).into_bytes(),
        ),
        ("GET", "/prompts") => (
            "200 OK",
            "text/html; charset=utf-8",
            page("/prompts", "Prompts", PROMPTS_BODY).into_bytes(),
        ),
        ("GET", "/sqlite") => (
            "200 OK",
            "text/html; charset=utf-8",
            page("/sqlite", "SQLite", SQLITE_BODY).into_bytes(),
        ),
        ("GET", "/helix") => (
            "200 OK",
            "text/html; charset=utf-8",
            page("/helix", "HelixDB", HELIX_BODY).into_bytes(),
        ),
        ("GET", "/music") => (
            "200 OK",
            "text/html; charset=utf-8",
            page("/music", "Music", MUSIC_BODY).into_bytes(),
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

    fn debug() -> DebugSources {
        DebugSources {
            memory: Arc::new(MemoryStore::open_in_memory().unwrap()),
            chatlog_path: std::env::temp_dir().join("wc_test_chatlog.jsonl"),
            promptlog_path: std::env::temp_dir().join("wc_test_promptlog.jsonl"),
            db_path: std::path::PathBuf::from(":memory:"),
            helix_path: std::path::PathBuf::from("ambient_helix"),
            memory_backend: "sqlite".to_string(),
            graph: None,
        }
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
    fn music_page_renders_with_nav() {
        let (status, ctype, body) = route("GET", "/music", b"", &settings());
        assert_eq!(status, "200 OK");
        assert!(ctype.starts_with("text/html"));
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("Web-URL player"), "music page body");
        assert!(html.contains("href=\"/music\""), "nav links music");
    }

    #[tokio::test]
    async fn music_status_reports_disabled_without_a_hub() {
        let v: Value = serde_json::from_str(&music_status_json(None).await).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["enabled"], false);
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

    #[test]
    fn debug_pages_render_with_nav() {
        for (path, marker) in [
            ("/chatlog", "Chat log"),
            ("/prompts", "System prompt"),
            ("/sqlite", "Persistent memory"),
            ("/helix", "GraphRAG"),
        ] {
            let (status, ctype, body) = route("GET", path, b"", &settings());
            assert_eq!(status, "200 OK", "{path}");
            assert!(ctype.starts_with("text/html"), "{path}");
            let html = String::from_utf8_lossy(&body);
            assert!(html.contains(marker), "{path} missing {marker}");
            // Every debug page carries the shared nav linking the others.
            assert!(html.contains("href=\"/helix\""), "{path} missing nav");
        }
    }

    #[test]
    fn limit_param_parses_and_clamps() {
        assert_eq!(limit_param("/chatlog.json"), DEFAULT_LOG_LIMIT);
        assert_eq!(limit_param("/chatlog.json?limit=25"), 25);
        assert_eq!(limit_param("/chatlog.json?limit=0"), 1);
        assert_eq!(limit_param("/chatlog.json?limit=99999"), MAX_LOG_LIMIT);
        assert_eq!(limit_param("/chatlog.json?x=1&limit=7"), 7);
    }

    #[test]
    fn sqlite_json_reports_memory_rows() {
        use crate::memory::{MemoryKind, MemorySource};
        let d = debug();
        d.memory
            .add(
                MemoryKind::Fact,
                "The user likes tea",
                MemorySource::Explicit,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&sqlite_json(&d)).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["count"], 1);
        assert_eq!(v["memory_backend"], "sqlite");
        assert_eq!(v["memories"][0]["content"], "The user likes tea");
        assert_eq!(v["memories"][0]["kind"], "fact");
    }

    #[tokio::test]
    async fn helix_json_reports_disabled_without_a_graph() {
        let v: Value = serde_json::from_str(&helix_json(&debug()).await).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["enabled"], false);
        assert!(v["message"].is_string());
    }

    #[tokio::test]
    async fn helix_rename_reports_disabled_without_a_graph() {
        let body = br#"{"old_name":"Portlnad","new_name":"Portland"}"#;
        let v: Value = serde_json::from_str(&helix_rename_json(&debug(), body).await).unwrap();
        assert_eq!(v["ok"], false, "no graph backend → rename cannot apply");
        assert!(v["message"].is_string());
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
            handle(stream, s, catalog(), connector(), None, debug(), None)
                .await
                .unwrap();
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
            handle(stream, s, catalog(), connector(), None, debug(), None)
                .await
                .unwrap();
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
            handle(
                stream,
                s,
                catalog(),
                connector(),
                Some(dir_for_task),
                debug(),
                None,
            )
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
