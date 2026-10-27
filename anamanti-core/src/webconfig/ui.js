// Anamanti Core admin console — UI component kit. Served at /ui.js, loaded right after
// /shell.js (which provides $/esc/escAttr/getJSON/postJSON). Plain vanilla JS, no
// framework, no build step. Exposes one global, `ui`.
//
//   ui.icon(name)                         → inline SVG string
//   ui.loc(tier, text?)                   → locality chip HTML (local | lan | cloud | off)
//   ui.status(kind, text)                 → status chip HTML (ok | warn | err | off | neutral)
//   ui.sends(text, tier?)                 → "Sends:" egress note HTML
//   ui.toast(ok, msg)                     → transient, aria-live toast
//   ui.choices(el, {name, options, value, cols}) → radio choice cards; {get, set, refresh}
//   ui.secret(el, {set, placeholder, clearable, onClear}) → secret field; {value, setState, reset}
//   ui.tags(el, values, {placeholder})    → chip input; {get, set}
//   ui.savebar(sections, {afterSave, onDiscard, canSave})                  → dirty-tracking save bar
//   ui.tabs(el)                           → tab bar with #hash routing
//   ui.drawer.open(id) / ui.drawer.close()
//   ui.confirm(btn, fn, label?)           → two-step inline confirm (no window.confirm)
//   ui.fmtMs / ui.fmtAgo / ui.fmtDuration / ui.reveal(hash)
//   ui.speakerChip(tag) / ui.resolutionBadge(tag) / ui.speakerTag(tag, {badge, hints, conf})
//                                         → speaker chips from a server SpeakerTag (/speakers.json)
//   ui.LOCALITY                           → the option → tier/sends map (plans/CoreWebUIPlan.md §3.3)

(function(){
  const S = (d) => '<svg class="ico" viewBox="0 0 24 24" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' + d + '</svg>';
  const ICONS = {
    local: S('<path d="M3 10.5L12 3l9 7.5"/><path d="M5 9.5V20h14V9.5"/>'),
    lan: S('<rect x="3" y="14" width="18" height="7" rx="2"/><path d="M7 17.5h.01M11 17.5h.01"/><path d="M8.5 9.5a5 5 0 0 1 7 0M6 7a8.5 8.5 0 0 1 12 0"/>'),
    cloud: S('<path d="M18 10a4 4 0 0 1 0 8H7a5 5 0 1 1 1.5-9.8A5.5 5.5 0 0 1 18 10z"/>'),
    off: S('<circle cx="12" cy="12" r="9"/><path d="M5.6 5.6l12.8 12.8"/>'),
    check: S('<path d="M20 6L9 17l-5-5"/>'),
    alert: S('<path d="M10.3 3.9L1.8 18a2 2 0 0 0 1.7 3h16.9a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><path d="M12 9v4M12 17h.01"/>'),
    x: S('<path d="M18 6L6 18M6 6l12 12"/>'),
    info: S('<circle cx="12" cy="12" r="10"/><path d="M12 16v-4M12 8h.01"/>'),
    lock: S('<rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>'),
    send: S('<path d="M22 2L11 13"/><path d="M22 2l-7 20-4-9-9-4z"/>'),
    arrow: S('<path d="M5 12h14M13 6l6 6-6 6"/>'),
    ext: S('<path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/><path d="M15 3h6v6M10 14L21 3"/>'),
    refresh: S('<path d="M21 12a9 9 0 1 1-2.6-6.4L21 8"/><path d="M21 3v5h-5"/>'),
    plus: S('<path d="M12 5v14M5 12h14"/>'),
    trash: S('<path d="M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6"/>'),
    edit: S('<path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z"/>'),
    mic: S('<rect x="9" y="2" width="6" height="12" rx="3"/><path d="M5 10a7 7 0 0 0 14 0"/><path d="M12 19v3"/>'),
    display: S('<rect x="2" y="4" width="20" height="13" rx="2"/><path d="M8 21h8M12 17v4"/>'),
    search: S('<circle cx="11" cy="11" r="8"/><path d="M21 21l-4.3-4.3"/>'),
    copy: S('<rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>'),
    play: S('<path d="M6 4l14 8-14 8z"/>'),
    stop: S('<rect x="5" y="5" width="14" height="14" rx="2"/>'),
    sun: S('<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M2 12h2M20 12h2M5 5l1.5 1.5M17.5 17.5L19 19M19 5l-1.5 1.5M6.5 17.5L5 19"/>'),
    pin: S('<path d="M21 10c0 7-9 13-9 13s-9-6-9-13a9 9 0 0 1 18 0z"/><circle cx="12" cy="10" r="3"/>'),
    route: S('<path d="M3 11l19-9-9 19-2-8-8-2z"/>'),
    calendar: S('<rect x="3" y="4" width="18" height="18" rx="2"/><path d="M16 2v4M8 2v4M3 10h18"/>'),
    cart: S('<circle cx="9" cy="21" r="1"/><circle cx="20" cy="21" r="1"/><path d="M1 1h4l2.7 13.4a2 2 0 0 0 2 1.6h9.7a2 2 0 0 0 2-1.6L23 6H6"/>'),
    phone: S('<rect x="6" y="2" width="12" height="20" rx="2"/><path d="M11 18h2"/>'),
    music: S('<path d="M9 18V5l12-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="18" cy="16" r="3"/>'),
    photo: S('<rect x="3" y="3" width="18" height="18" rx="2"/><circle cx="8.5" cy="8.5" r="1.5"/><path d="M21 15l-5-5L5 21"/>'),
    key: S('<circle cx="7.5" cy="15.5" r="4.5"/><path d="M10.7 12.3L21 2M16 7l3 3"/>'),
    link: S('<path d="M10 13a5 5 0 0 0 7.5.5l3-3a5 5 0 0 0-7-7l-1.7 1.7"/><path d="M14 11a5 5 0 0 0-7.5-.5l-3 3a5 5 0 0 0 7 7l1.7-1.7"/>'),
    bolt: S('<path d="M13 2L3 14h9l-1 8 10-12h-9z"/>'),
    brain: S('<path d="M9.5 2a2.5 2.5 0 0 1 2.5 2.5V20a2 2 0 0 1-4 .5A2.5 2.5 0 0 1 5 18a2.5 2.5 0 0 1-1-4.5A3 3 0 0 1 5 8a2.5 2.5 0 0 1 4.5-6z"/><path d="M14.5 2A2.5 2.5 0 0 0 12 4.5V20a2 2 0 0 0 4 .5 2.5 2.5 0 0 0 3-2.5 2.5 2.5 0 0 0 1-4.5A3 3 0 0 0 19 8a2.5 2.5 0 0 0-4.5-6z"/>'),
    globe: S('<circle cx="12" cy="12" r="10"/><path d="M2 12h20M12 2a15 15 0 0 1 0 20M12 2a15 15 0 0 0 0 20"/>'),
    speaker: S('<path d="M11 5L6 9H2v6h4l5 4z"/><path d="M19 5a7 7 0 0 1 0 14M15.5 8.5a3.5 3.5 0 0 1 0 7"/>'),
    wave: S('<path d="M22 12h-4l-3 9L9 3l-3 9H2"/>'),
    text: S('<path d="M4 7V4h16v3M9 20h6M12 4v16"/>'),
    users: S('<path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M23 21v-2a4 4 0 0 0-3-3.9M16 3.1a4 4 0 0 1 0 7.8"/>'),
    home: S('<path d="M3 9l9-7 9 7v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/><path d="M9 22V12h6v10"/>'),
    layers: S('<path d="M12 2l9 5-9 5-9-5z"/><path d="M3 12l9 5 9-5M3 17l9 5 9-5"/>'),
    db: S('<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14a9 3 0 0 0 18 0V5"/><path d="M3 12a9 3 0 0 0 18 0"/>'),
    graph: S('<circle cx="18" cy="5" r="3"/><circle cx="6" cy="12" r="3"/><circle cx="18" cy="19" r="3"/><path d="M8.6 13.5l6.8 4M15.4 6.5l-6.8 4"/>'),
    clock: S('<circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/>'),
    server: S('<rect x="2" y="2" width="20" height="8" rx="2"/><rect x="2" y="14" width="20" height="8" rx="2"/><path d="M6 6h.01M6 18h.01"/>'),
    shield: S('<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/>'),
    smile: S('<circle cx="12" cy="12" r="10"/><path d="M8 14s1.5 2 4 2 4-2 4-2"/><path d="M9 9h.01M15 9h.01"/>'),
    chat: S('<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>'),
    file: S('<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/>'),
    filter: S('<path d="M22 3H2l8 9.5V19l4 2v-8.5z"/>'),
  };
  function icon(name){ return ICONS[name] || ''; }

  const TIER = {
    local: { short: 'Local', long: 'On this Mac' },
    lan: { short: 'LAN', long: 'Your network' },
    cloud: { short: 'Cloud', long: 'Internet service' },
    off: { short: 'Off', long: 'Disabled' },
  };
  // Locality chip. `text` overrides the label (e.g. "Cloud · keyless").
  function loc(tier, text, title){
    const t = TIER[tier] ? tier : 'off';
    return '<span class="chip loc-' + t + '" title="' + escAttr(title || TIER[t].long) + '">' + icon(t) + esc(text || TIER[t].short) + '</span>';
  }
  function status(kind, text){
    const k = ({ok:'ok', active:'ok', warn:'warn', attention:'warn', fallback:'warn', needs_key:'warn', err:'err', error:'err', off:'neutral', inactive:'neutral', disabled:'neutral', unknown:'neutral'})[kind] || 'neutral';
    const dot = ({ok:'ok', warn:'warn', err:'err'})[k] || 'off';
    return '<span class="chip ' + k + '"><span class="dot ' + dot + '"></span>' + esc(text) + '</span>';
  }
  function sends(text, tier){
    const t = tier || 'cloud';
    const ico = t === 'cloud' ? icon('send') : icon(t === 'lan' ? 'lan' : 'shield');
    return '<div class="sends ' + (t === 'cloud' ? '' : t) + '">' + ico + '<span>' + text + '</span></div>';
  }

  // ---------------------------------------------------------------- Locality map
  // One source of truth for every option that can run locally or in the cloud: its
  // tier (address-backed options get their live tier from the server), what it needs,
  // and — for cloud options — exactly what leaves the house. Copy decided per
  // plans/CoreWebUIPlan.md §8 (7): concrete nouns, no hedging.
  const LOCALITY = {
    'vad.silero': { tier: 'local', title: 'Silero', desc: 'Neural speech detector. Robust to background noise.', needs: 'Silero v4 model on disk', sends: 'Audio never leaves this Mac.' },
    'vad.energy': { tier: 'local', title: 'Energy', desc: 'Simple loudness gate. The fallback when no Silero model is loaded.', needs: '', sends: 'Audio never leaves this Mac.' },
    'stt.wyoming': { tier: 'addr', title: 'Wyoming Whisper', desc: 'External faster-whisper server over the Wyoming protocol.', needs: 'wyoming-faster-whisper running', sends: 'Microphone audio goes to the Whisper server at {addr}.' },
    'stt.whisper-rs': { tier: 'local', title: 'whisper-rs', desc: 'whisper.cpp in-process. No Python server.', needs: 'stt-whisper-local build + model', sends: 'Audio never leaves this Mac.' },
    'stt.elevenlabs': { tier: 'cloud', title: 'ElevenLabs Scribe', desc: 'Scribe v2 Realtime over WebSocket. High accuracy, needs internet.', needs: 'ELEVENLABS_API_KEY', sends: 'Sends: <b>live microphone audio</b> of every turn to ElevenLabs.' },
    'system1.none': { tier: 'off', title: 'Off', desc: 'Every turn goes to the language model.', needs: '', sends: '' },
    'system1.laya-serve': { tier: 'addr', title: 'laya-serve', desc: 'Laya-Decision sidecar classifies common intents before the LLM.', needs: 'sidecar at base URL', sends: 'Each transcript goes to the sidecar at {addr}.' },
    'system1.jev': { tier: 'cloud', title: 'jev (OpenRouter)', desc: 'Typed intent classifier hosted on OpenRouter.', needs: 'OPENROUTER_API_KEY', sends: 'Sends: <b>each transcript</b> to OpenRouter.' },
    'llm.ollama': { tier: 'addr', title: 'Ollama', desc: 'Open-weights models served by Ollama / llama.cpp.', needs: 'Ollama running + model pulled', sends: 'The full prompt goes to Ollama at {addr}.' },
    'llm.anthropic': { tier: 'cloud', title: 'Anthropic Claude', desc: 'Claude models via API key or a Claude subscription.', needs: 'API key or subscription token', sends: 'Sends: <b>your transcript, recalled memories, the household roster (names, emails, phones), home city/state and the time</b> to Anthropic.' },
    'llm.openai': { tier: 'cloud', title: 'OpenAI', desc: 'OpenAI chat models.', needs: 'OPENAI_API_KEY', sends: 'Sends: <b>your transcript, recalled memories, the household roster (names, emails, phones), home city/state and the time</b> to OpenAI.' },
    'llm.mock': { tier: 'local', title: 'Mock', desc: 'Offline echo backend for testing.', needs: '', sends: 'Nothing leaves this Mac.' },
    'search.duckduckgo': { tier: 'cloud', keyless: true, title: 'DuckDuckGo', desc: 'Keyless instant answers. Entity queries only.', needs: '', sends: 'Sends: <b>search queries</b> the model writes to DuckDuckGo.' },
    'search.tavily': { tier: 'cloud', title: 'Tavily', desc: 'Real web results built for agents.', needs: 'TAVILY_API_KEY', sends: 'Sends: <b>search queries</b> the model writes to Tavily.' },
    'embed.local': { tier: 'local', title: 'nomic-embed-text v1.5', desc: 'On-device embeddings via onnxruntime. 768-dim, no key.', needs: 'ONNX + tokenizer on disk', sends: 'Memory text never leaves this Mac.' },
    'embed.openai': { tier: 'cloud', title: 'OpenAI text-embedding-3-small', desc: 'Cloud embeddings. Separate vector store.', needs: 'OPENAI_API_KEY (env)', sends: 'Sends: <b>every turn’s text and each recall query</b> to OpenAI.' },
    'extract.anthropic': { tier: 'cloud', title: 'Entity extraction', desc: 'Builds people/place/topic nodes in the memory graph.', needs: 'ANTHROPIC_API_KEY (env)', sends: 'Sends: <b>every completed turn (transcript + reply)</b> to Anthropic for entity extraction.' },
    'tts.piper': { tier: 'addr', title: 'Piper', desc: 'Piper voices over Wyoming.', needs: 'Piper server running', sends: 'Reply text goes to the Piper server at {addr}.' },
    'weather.visualcrossing': { tier: 'cloud', title: 'Visual Crossing', desc: 'One-request forecasts. Needs an API key.', needs: 'VISUALCROSSING_API_KEY', sends: 'Sends: <b>your home city/state</b> to Visual Crossing.' },
    'weather.openmeteo': { tier: 'cloud', keyless: true, title: 'Open-Meteo', desc: 'Keyless forecasts. The automatic fallback.', needs: '', sends: 'Sends: <b>your home city/state</b> to Open-Meteo.' },
    'places.google': { tier: 'cloud', title: 'Google Places', desc: 'Address, hours, rating, phone and a photo.', needs: 'GOOGLE_PLACES_API_KEY', sends: 'Sends: <b>the place query and your home address</b> (for “near me”) to Google.' },
    'directions.mapbox': { tier: 'cloud', title: 'Mapbox', desc: 'Distance, travel time, live traffic.', needs: 'MAPBOX_TOKEN', sends: 'Sends: <b>your full home address and the destination</b> to Mapbox.' },
    'calendar.ics': { tier: 'cloud', title: 'Calendar feeds', desc: 'Read-only .ics / webcal subscriptions.', needs: 'feeds in anamanti.json', sends: 'Fetches your calendar feeds. Nothing is sent but the request.' },
    'cadora': { tier: 'cloud', title: 'Cadora / NextHaul', desc: 'Saved recipes, the household shopping list (NextHaul or Cadora) and the Cadora family calendar.', needs: 'family invite code', sends: 'Sends: <b>recipe searches, items and events you ask to add</b> to Cadora / NextHaul (api.getcadora.com).' },
    'appsaid': { tier: 'cloud', title: 'AppSaid', desc: 'Push a message or map link to a family phone.', needs: 'app token + recipients', sends: 'Sends: <b>the message text</b> to the recipient’s phone via AppSaid.' },
    'home_assistant': { tier: 'addr', title: 'Home Assistant', desc: 'Lights, switches, scenes and TV — common commands skip the LLM.', needs: 'URL + long-lived token', sends: 'Sends: <b>device commands and a device-list request</b> to Home Assistant at {addr}. Nothing goes to the cloud.' },
    'spotify': { tier: 'cloud', title: 'Spotify', desc: 'Voice control of Spotify Connect (Premium).', needs: 'client id/secret + consent', sends: 'Sends: <b>playback commands</b> to Spotify.' },
    'radio': { tier: 'cloud', title: 'Internet radio', desc: 'Curated live stations on the house speakers.', needs: 'music routing + mpv', sends: 'Streams each station you play from its broadcaster. <b>Look up</b> sends the stream URL / name you typed to Radio Browser.' },
    'drive': { tier: 'cloud', title: 'Google Drive', desc: 'Photo slideshow on the display’s idle screen.', needs: 'OAuth client + consent', sends: 'The display downloads photos from your chosen Drive folders.' },
  };
  // Resolve an option's tier ('addr' entries need the live tier from the server).
  function tierOf(key, liveTier){ const m = LOCALITY[key]; if (!m) return liveTier || 'off'; return m.tier === 'addr' ? (liveTier || 'local') : m.tier; }
  function sendsFor(key, addr, liveTier){
    const m = LOCALITY[key]; if (!m || !m.sends) return '';
    const t = tierOf(key, liveTier);
    return sends(m.sends.replace('{addr}', '<code>' + esc(addr || '?') + '</code>'), t === 'off' ? 'local' : t);
  }

  // Client-side mirror of the server classifier (for addresses typed in the page).
  function classify(addr){
    if (!addr) return 'cloud';
    let a = String(addr).trim().replace(/^[a-z]+:\/\//i, '').split(/[/?#]/)[0];
    a = a.replace(/^.*@/, '');
    let host = a.startsWith('[') ? a.slice(1, a.indexOf(']')) : a.replace(/:\d+$/, '');
    host = host.toLowerCase();
    if (!host || host === 'localhost' || host.endsWith('.localhost') || host === '::1' || host.startsWith('127.') || host === '0.0.0.0') return 'local';
    const m = host.match(/^(\d+)\.(\d+)\.(\d+)\.(\d+)$/);
    if (m) {
      const [o1, o2] = [+m[1], +m[2]];
      if (o1 === 10 || (o1 === 172 && o2 >= 16 && o2 <= 31) || (o1 === 192 && o2 === 168) || (o1 === 169 && o2 === 254) || (o1 === 100 && o2 >= 64 && o2 <= 127)) return 'lan';
      return 'cloud';
    }
    if (/^f[cd]/.test(host) || host.startsWith('fe80')) return 'lan';
    if (host.endsWith('.local') || host.endsWith('.lan') || host.endsWith('.home.arpa') || !host.includes('.')) return 'lan';
    return 'cloud';
  }

  // ---------------------------------------------------------------- Toasts
  function toast(ok, msg){
    let box = document.getElementById('toasts');
    if (!box) { box = document.createElement('div'); box.id = 'toasts'; box.className = 'toasts'; box.setAttribute('role', 'status'); box.setAttribute('aria-live', 'polite'); document.body.appendChild(box); }
    const t = document.createElement('div');
    t.className = 'toast ' + (ok ? 'ok' : 'err');
    t.innerHTML = icon(ok ? 'check' : 'alert') + '<div>' + esc(msg) + '</div>';
    box.appendChild(t);
    setTimeout(() => { t.classList.add('leaving'); setTimeout(() => t.remove(), 220); }, ok ? 3200 : 6500);
  }

  // ---------------------------------------------------------------- Choice cards
  // options: [{value, title, desc, tier, tierText, meta:[html], disabled, why}]
  function choices(el, cfg){
    const name = cfg.name || ('c' + Math.random().toString(36).slice(2));
    let opts = cfg.options || [];
    function render(value){
      el.className = 'choices' + (cfg.cols ? ' cols-' + cfg.cols : '');
      el.setAttribute('role', 'radiogroup');
      el.innerHTML = opts.map(o => {
        const id = name + '-' + String(o.value).replace(/[^a-z0-9_-]/gi, '_');
        const meta = [o.tier ? loc(o.tier, o.tierText) : ''].concat(o.meta || []).filter(Boolean).join('');
        return '<label class="choice' + (o.disabled ? ' disabled' : '') + '" for="' + id + '">'
          + '<input type="radio" id="' + id + '" name="' + escAttr(name) + '" value="' + escAttr(o.value) + '"' + (String(o.value) === String(value) ? ' checked' : '') + (o.disabled ? ' disabled' : '') + '>'
          + '<span class="choice-top"><span class="radio"></span><span class="choice-title">' + esc(o.title) + '</span></span>'
          + (o.desc ? '<span class="choice-desc">' + o.desc + '</span>' : '')
          + (o.disabled && o.why ? '<span class="why">' + esc(o.why) + '</span>' : '')
          + (meta ? '<span class="choice-meta">' + meta + '</span>' : '')
          + '</label>';
      }).join('');
    }
    render(cfg.value);
    if (cfg.onChange) el.addEventListener('change', (e) => { if (e.target.name === name) cfg.onChange(e.target.value); });
    return {
      get(){ const c = el.querySelector('input[name="' + name + '"]:checked'); return c ? c.value : null; },
      set(v){ render(v); },
      refresh(newOpts, v){ opts = newOpts; render(v === undefined ? this.get() : v); },
    };
  }

  // ---------------------------------------------------------------- Secret field
  // Never shows a stored value back: shows Set / Not set, with Replace (reveals an
  // input) and optional Clear. value() → the typed secret, or undefined (keep).
  function secret(el, cfg){
    cfg = cfg || {};
    el.classList.add('secret');
    el.innerHTML = '<span class="secret-state"></span>'
      + '<input class="secret-input" type="password" autocomplete="off" placeholder="' + escAttr(cfg.placeholder || 'Paste a new value') + '">'
      + '<button type="button" class="sm secret-replace">Replace</button>'
      + (cfg.clearable ? '<button type="button" class="sm ghost danger-text secret-clear">Clear</button>' : '');
    const input = el.querySelector('.secret-input');
    const stateEl = el.querySelector('.secret-state');
    let isSet = !!cfg.set;
    function paint(){
      stateEl.innerHTML = isSet ? '<span class="dot ok"></span>Set' : '<span class="dot off"></span><span class="muted">Not set</span>';
      el.querySelector('.secret-replace').textContent = isSet ? 'Replace' : 'Add';
      const c = el.querySelector('.secret-clear'); if (c) c.style.display = isSet ? '' : 'none';
    }
    el.querySelector('.secret-replace').addEventListener('click', () => { el.classList.add('editing'); input.focus(); });
    input.addEventListener('keydown', (e) => {
      if (e.key !== 'Escape') return;
      const had = input.value !== '';
      input.value = ''; el.classList.remove('editing');
      if (had) input.dispatchEvent(new Event('input', {bubbles:true}));
    });
    const clr = el.querySelector('.secret-clear');
    if (clr && cfg.onClear) confirm(clr, cfg.onClear, 'Clear?');
    paint();
    return {
      value(){ const v = input.value.trim(); return v ? v : undefined; },
      setState(v){ isSet = !!v; paint(); },
      reset(){ input.value = ''; el.classList.remove('editing'); },
    };
  }

  // ---------------------------------------------------------------- Tag input
  function tags(el, values, cfg){
    cfg = cfg || {};
    let list = (values || []).slice();
    el.classList.add('tags');
    function render(){
      el.innerHTML = list.map((v, i) => '<span class="tag">' + esc(v) + '<button type="button" data-i="' + i + '" aria-label="Remove ' + escAttr(v) + '">×</button></span>').join('')
        + '<input type="text" placeholder="' + escAttr(list.length ? '' : (cfg.placeholder || 'Add…')) + '">';
      const inp = el.querySelector('input');
      inp.addEventListener('keydown', (e) => {
        if ((e.key === 'Enter' || e.key === ',' || e.key === 'Tab') && inp.value.trim()) {
          if (e.key !== 'Tab') e.preventDefault();
          add(inp.value); render(); el.querySelector('input').focus(); changed();
        } else if (e.key === 'Backspace' && !inp.value && list.length) { list.pop(); render(); el.querySelector('input').focus(); changed(); }
      });
      inp.addEventListener('blur', () => { if (inp.value.trim()) { add(inp.value); render(); changed(); } });
    }
    function add(raw){ raw.split(',').map(s => s.trim()).filter(Boolean).forEach(v => { if (!list.includes(v)) list.push(v); }); }
    function changed(){ el.dispatchEvent(new Event('input', {bubbles:true})); }
    el.addEventListener('click', (e) => {
      const b = e.target.closest('button[data-i]');
      if (b) { list.splice(+b.dataset.i, 1); render(); changed(); return; }
      const inp = el.querySelector('input'); if (e.target === el && inp) inp.focus();
    });
    render();
    return { get(){ const inp = el.querySelector('input'); if (inp && inp.value.trim()) add(inp.value); return list.slice(); }, set(v){ list = (v || []).slice(); render(); } };
  }

  // ---------------------------------------------------------------- Save bar
  // sections: [{name, el, save: async () => ({ok, message})}]. Any input/change
  // inside a section's element marks it dirty; the floating bar saves only the dirty
  // sections (each to its own existing endpoint) and reports per section.
  function savebar(sections, opts){
    opts = opts || {};
    const dirty = new Set();
    let suspended = false;
    const bar = document.createElement('div');
    bar.className = 'savebar';
    bar.setAttribute('role', 'region');
    bar.setAttribute('aria-label', 'Unsaved changes');
    bar.innerHTML = '<span class="dot"></span><span class="sb-text">Unsaved changes</span>'
      + '<button type="button" class="ghost sm sb-discard">Discard</button>'
      + '<button type="button" class="primary sb-save">Save changes</button>';
    document.body.appendChild(bar);
    function paint(){
      const n = dirty.size;
      bar.classList.toggle('show', n > 0);
      bar.querySelector('.sb-text').textContent = n === 1
        ? 'Unsaved changes in ' + [...dirty][0]
        : 'Unsaved changes in ' + n + ' sections';
    }
    sections.forEach(s => {
      const mark = () => { if (!suspended) { dirty.add(s.name); paint(); } };
      s.el.addEventListener('input', mark);
      s.el.addEventListener('change', mark);
    });
    let saving = false;
    async function saveAll(){
      if (saving) return;           // a second click / Cmd-S while saving is a no-op
      // canSave() false ⇒ the page's data never loaded; saving would write defaults
      // (e.g. an empty roster) over the stored record.
      if (opts.canSave && !opts.canSave()) { toast(false, 'The page did not load correctly — reload before saving.'); return; }
      saving = true;
      const btn = bar.querySelector('.sb-save');
      btn.disabled = true; btn.textContent = 'Saving…';
      let failed = false;
      for (const s of sections) {
        if (!dirty.has(s.name)) continue;
        try {
          const r = await s.save();
          if (r && r.ok === false) { failed = true; toast(false, s.name + ': ' + (r.message || 'rejected')); continue; }
          dirty.delete(s.name);
          toast(true, s.name + ': ' + ((r && r.message) || 'saved'));
        } catch (e) { failed = true; toast(false, s.name + ': request failed — ' + e); }
      }
      btn.disabled = false; btn.textContent = 'Save changes';
      saving = false;
      paint();
      // Reload from the server only when everything saved: a reload after a failure
      // would overwrite the user's unsaved edits in the section that failed.
      if (!failed && opts.afterSave) { suspended = true; try { await opts.afterSave(); } finally { suspended = false; } }
    }
    bar.querySelector('.sb-save').addEventListener('click', saveAll);
    bar.querySelector('.sb-discard').addEventListener('click', async () => {
      dirty.clear(); paint();
      if (opts.onDiscard) { suspended = true; try { await opts.onDiscard(); } finally { suspended = false; } }
    });
    window.addEventListener('beforeunload', (e) => { if (dirty.size) { e.preventDefault(); e.returnValue = ''; } });
    document.addEventListener('keydown', (e) => { if ((e.metaKey || e.ctrlKey) && e.key === 's' && dirty.size) { e.preventDefault(); saveAll(); } });
    return {
      mark(name){ dirty.add(name); paint(); },
      clear(){ dirty.clear(); paint(); },
      isDirty(){ return dirty.size > 0; },
      // Run fn (e.g. a reload that rewrites inputs) without marking anything dirty.
      async quiet(fn){ suspended = true; try { return await fn(); } finally { suspended = false; } },
    };
  }

  // ---------------------------------------------------------------- Tabs
  // <div class="tabs" data-tabs> <button data-tab="facts">…</button> … </div>
  // panels: <section id="tab-facts" role="tabpanel">. Selection follows #hash.
  function tabs(el, onSelect){
    const btns = [...el.querySelectorAll('button[data-tab]')];
    el.setAttribute('role', 'tablist');
    function select(key, push){
      if (!btns.some(b => b.dataset.tab === key)) key = btns[0].dataset.tab;
      btns.forEach(b => {
        const on = b.dataset.tab === key;
        b.setAttribute('role', 'tab');
        b.setAttribute('aria-selected', on ? 'true' : 'false');
        const p = document.getElementById('tab-' + b.dataset.tab);
        if (p) { p.hidden = !on; p.setAttribute('role', 'tabpanel'); }
      });
      if (push) history.replaceState(null, '', '#' + key);
      if (onSelect) onSelect(key);
    }
    btns.forEach(b => b.addEventListener('click', () => select(b.dataset.tab, true)));
    window.addEventListener('hashchange', () => select(location.hash.slice(1), false));
    select(location.hash.slice(1), false);
    return { select };
  }

  // ---------------------------------------------------------------- Drawer
  const drawer = {
    open(id){
      let scrim = document.getElementById('drawerScrim');
      if (!scrim) {
        scrim = document.createElement('div'); scrim.id = 'drawerScrim'; scrim.className = 'drawer-scrim';
        scrim.addEventListener('click', () => drawer.close());
        document.body.appendChild(scrim);
      }
      const d = document.getElementById(id);
      if (!d) return;
      drawer._last = document.activeElement;
      d.classList.add('open'); scrim.classList.add('open');
      d.setAttribute('aria-hidden', 'false');
      // Focus the first VISIBLE control (panels for other items may be hidden).
      setTimeout(() => {
        const f = [...d.querySelectorAll('input, select, textarea, button:not(.icon-btn)')].find(e => e.offsetParent !== null && !e.disabled);
        if (f) f.focus();
      }, 230);
    },
    close(){
      document.querySelectorAll('.drawer.open').forEach(d => { d.classList.remove('open'); d.setAttribute('aria-hidden', 'true'); });
      const s = document.getElementById('drawerScrim'); if (s) s.classList.remove('open');
      if (drawer._last && drawer._last.focus) drawer._last.focus();
      if (drawer.onClose) drawer.onClose();
    },
  };
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && document.querySelector('.drawer.open')) drawer.close(); });

  // ---------------------------------------------------------------- Inline confirm
  function confirm(btn, fn, label){
    let armed = null;
    const original = () => btn.dataset.label;
    btn.dataset.label = btn.innerHTML;
    btn.addEventListener('click', async (e) => {
      e.preventDefault();
      if (!armed) {
        btn.innerHTML = esc(label || 'Confirm?');
        btn.classList.add('confirming');
        armed = setTimeout(() => { btn.innerHTML = original(); btn.classList.remove('confirming'); armed = null; }, 3500);
        return;
      }
      clearTimeout(armed); armed = null;
      btn.innerHTML = original(); btn.classList.remove('confirming');
      await fn();
    });
  }

  // ---------------------------------------------------------------- Formatting
  function fmtMs(v){
    if (v == null) return '—';
    if (v >= 1000) return (v / 1000).toFixed(v >= 10000 ? 1 : 2) + ' s';
    return v + ' ms';
  }
  function fmtAgo(ts){
    if (!ts) return '';
    const s = Math.max(0, Math.round(Date.now() / 1000 - ts));
    if (s < 60) return s + 's ago';
    if (s < 3600) return Math.round(s / 60) + 'm ago';
    if (s < 86400) return Math.round(s / 3600) + 'h ago';
    return Math.round(s / 86400) + 'd ago';
  }
  function fmtDuration(secs){
    if (secs == null) return '—';
    const d = Math.floor(secs / 86400), h = Math.floor(secs % 86400 / 3600), m = Math.floor(secs % 3600 / 60);
    return (d ? d + 'd ' : '') + (d || h ? h + 'h ' : '') + m + 'm';
  }
  // Scroll to + briefly highlight the element named by a #hash (deep links such as
  // /voice#stt from the Overview pipeline or a legacy redirect).
  function reveal(hash){
    const id = (hash || location.hash).replace(/^#/, '');
    if (!id) return;
    const el = document.getElementById(id);
    if (!el) return;
    el.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    el.style.transition = 'box-shadow .3s';
    el.style.boxShadow = '0 0 0 3px var(--accent)';
    setTimeout(() => { el.style.boxShadow = ''; }, 1400);
  }
  // Unwrap a `{ok:false, message}` reply into a thrown Error.
  async function req(p){
    const r = await p;
    if (r && r.ok === false) throw new Error(r.message || r.error || 'rejected');
    return r;
  }

  // ---------------------------------------------------------------- Speakers
  // A resolved speaker tag from the server (SpeakerScopedMemoryPlan.md §8.1:
  // {id, kind: person|household|unknown, name, label, relationship, resolution,
  // confidence, candidate, has_clips, name_source, color, inherited, short_clip})
  // rendered as a chip. Person: name + their stable colour, relationship on hover.
  // Unnamed voice: "Speaker N (unnamed)". Household: neutral house chip. Unknown:
  // dashed chip, "Unknown · maybe Brendon?" when there is a best guess.
  function speakerChip(tag){
    if (!tag) return '<span class="muted" title="Not recorded for older entries">not recorded</span>';
    const kind = tag.kind === 'person' || tag.kind === 'household' ? tag.kind : 'unknown';
    const tip = [];
    let cls = 'chip spk ' + kind, style = '', lead = '';
    if (kind === 'person') {
      if (!tag.name) { cls += ' unnamed'; tip.push('An unnamed voice. Name it on the display’s People screen.'); }
      if (tag.relationship) tip.push(tag.relationship);
      if (tag.name_source) tip.push('name ' + ({ enrolled: 'from enrollment', admin: 'set by hand', spoken: 'heard in speech' })[tag.name_source]);
      if (tag.has_clips) tip.push('voice clips on the display’s People screen');
      if (tag.id) tip.push(tag.id);
      if (tag.color) style = ' style="--spk:' + escAttr(tag.color) + '"';
      lead = '<span class="spk-dot"></span>';
    } else if (kind === 'household') {
      lead = icon('home'); tip.push('True for everyone in the home');
    } else {
      lead = icon('users');
      tip.push(tag.candidate ? 'Could not confirm the voice. Best guess: ' + tag.candidate.label : 'A voice that could not be identified');
    }
    return '<span class="' + cls + '"' + style + ' title="' + escAttr(tip.join(' · ')) + '">' + lead + '<span class="spk-name">' + esc(tag.label) + '</span></span>';
  }
  const RES = {
    identified: { mark: '✓', text: 'identified', tip: 'Voice identified' },
    tentative: { mark: '~', text: 'probable', tip: 'Probable match (below the match threshold)' },
    'new': { mark: '★', text: 'new voice', tip: 'A voice not heard before' },
    unknown: { mark: '?', text: 'unknown', tip: 'Voice could not be identified' },
    corrected: { mark: '✎', text: 'corrected', tip: 'Speaker corrected by hand on Conversations' },
  };
  // The resolution badge: ✓ identified / ~ probable / ★ new / ? unknown, with the
  // confidence on hover (and inline when `opts.conf`).
  function resolutionBadge(tag, opts){
    if (!tag || !tag.resolution || !RES[tag.resolution]) return '';
    const r = RES[tag.resolution];
    const conf = tag.confidence != null && tag.resolution !== 'unknown' ? Number(tag.confidence).toFixed(2) : null;
    const tip = r.tip + (conf != null ? ' · cosine ' + conf : '');
    return '<span class="chip res ' + escAttr(tag.resolution) + '" title="' + escAttr(tip) + '">' + r.mark + ' ' + esc(opts && opts.conf && conf != null ? conf : r.text) + '</span>';
  }
  // Chip + badge + the "inherited" / "short clip" hints, as one inline group.
  function speakerTag(tag, opts){
    if (!tag) return speakerChip(null);
    opts = opts || {};
    let h = speakerChip(tag);
    if (opts.badge !== false) h += resolutionBadge(tag, opts);
    if (opts.hints !== false) {
      if (tag.inherited) h += '<span class="spk-hint" title="Too short to identify; identity carried over from the follow-up conversation">inherited</span>';
      if (tag.short_clip) h += '<span class="spk-hint" title="Matched by the read-only short-clip path">short clip</span>';
    }
    return '<span class="spk-tag">' + h + '</span>';
  }

  // Turn-latency stages, in pipeline order, with one fixed colour each — shared by the
  // Overview latency chart and the Conversations waterfall so a stage reads the same
  // everywhere. Mid-saturation hues that hold contrast on both light and dark surfaces.
  const STAGES = [
    { key: 'stt_ms', label: 'STT', color: '#7c6cd9' },
    { key: 'system1_ms', label: 'Fast path', color: '#e0a03a' },
    { key: 'fetch_ms', label: 'Fetch', color: '#c9733a' },
    { key: 'recall_ms', label: 'Memory recall', color: '#2a9d8f' },
    { key: 'prompt_ms', label: 'Prompt build', color: '#8d99ae' },
    { key: 'llm_ms', label: 'LLM first token', color: '#4f7cff' },
    { key: 'tts_ms', label: 'TTS first audio', color: '#e76f8a' },
  ];

  // Replace declarative placeholders with rendered components:
  //   <i data-icon="mic"></i>                    → the shared SVG glyph
  //   <i data-loc="cloud" data-text="Cloud"></i> → a locality chip
  // Pages call ui.hydrate() synchronously at the top of their script (the markup
  // above it is already parsed); it is also run once on DOMContentLoaded.
  function hydrate(root){
    (root || document).querySelectorAll('i[data-icon]').forEach(el => { el.outerHTML = icon(el.dataset.icon); });
    (root || document).querySelectorAll('i[data-loc]').forEach(el => { el.outerHTML = loc(el.dataset.loc, el.dataset.text); });
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', () => hydrate());

  window.ui = { hydrate, icon, loc, status, sends, speakerChip, resolutionBadge, speakerTag, sendsFor, tierOf, classify, toast, choices, secret, tags, savebar, tabs, drawer, confirm, fmtMs, fmtAgo, fmtDuration, reveal, req, LOCALITY, TIER, STAGES };
})();
