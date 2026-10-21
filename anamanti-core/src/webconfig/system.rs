//! Console-wide, read-mostly aggregators behind the restructured admin UI
//! (`plans/CoreWebUIPlan.md`): the Overview dashboard (`/overview.json`), the Keys &
//! Accounts table (`/keys.json`), the System page (`/system.json`, build + runtime +
//! the redacted effective boot config), the cloud entity-extraction toggle
//! (`/extraction/*`), and the **locality classifier** that tags every address-backed
//! stage as `local` (this Mac), `lan` (your network) or `cloud` (the internet).
//!
//! Nothing here ever returns a secret — only whether one is set.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::drive::drive_status_json;
use super::general::about_json;
use super::home_assistant::ha_status_json;
use super::household::{appsaid_status_json, cadora_status_json};
use super::music::spotify_status_json;
use super::tools::{places_status_json, stt_status_json, system1_status_json, weather_status_json};
use super::DebugSources;
use crate::app::AppServices;
use crate::config::{Config, LlmChoice, MemoryBackendChoice};
use crate::memory::chatlog;
use crate::music::GroupSelector;

// ------------------------------------------------------------------------------------
// Boot snapshot
// ------------------------------------------------------------------------------------

/// What the console needs to know about the boot configuration: the downstream
/// addresses the locality chips classify, identity, and a curated, **redacted** view
/// of the effective config for the System page. Built once in `main` from the resolved
/// [`Config`]; secrets (OAuth client secrets, refresh/link tokens) are never copied in.
#[derive(Clone, Debug, Default)]
pub struct BootInfo {
    /// Downstream Wyoming STT (faster-whisper) address, `host:port`.
    pub stt_addr: String,
    /// Downstream Wyoming TTS (Piper) address, `host:port`.
    pub tts_addr: String,
    /// The Ollama base URL the ollama backend is (re)built against.
    pub ollama_url: String,
    /// The config file this instance booted from, when known.
    pub config_file: Option<String>,
    /// Curated effective configuration, grouped by section (redacted).
    pub config: Value,
}

impl BootInfo {
    /// Snapshot the resolved boot config. `config_file` is the `--config` path, or the
    /// convention `anamanti.json` when it exists.
    pub fn from_config(c: &Config, config_file: Option<&Path>) -> Self {
        let path = |p: &Path| p.display().to_string();
        let set = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.is_empty());
        let llm = match &c.llm {
            LlmChoice::Mock => json!({ "backend": "mock" }),
            LlmChoice::Ollama { url, model } => {
                json!({ "backend": "ollama", "url": url, "model": model })
            }
            LlmChoice::Anthropic { model, max_tokens } => {
                json!({ "backend": "anthropic", "model": model, "max_tokens": max_tokens })
            }
            LlmChoice::OpenAI { model, max_tokens } => {
                json!({ "backend": "openai", "model": model, "max_tokens": max_tokens })
            }
        };
        let group = match &c.music.group {
            GroupSelector::Auto => "auto".to_string(),
            GroupSelector::GroupId(g) => format!("group:{g}"),
            GroupSelector::StreamId(s) => format!("stream:{s}"),
        };
        let config = json!({
            "identity": {
                "service_name": c.service_name,
                "instance_id": c.instance_id,
                "bind_addr": c.bind_addr.to_string(),
                "config_addr": c.config_addr.map(|a| a.to_string()),
            },
            "services": {
                "stt_addr": c.stt_addr.to_string(),
                "tts_addr": c.tts_addr.to_string(),
                "tts_voice": c.tts_voice,
                "tts_voices_dir": c.tts_voices_dir.as_deref().map(path),
                "tts_addrs": c.tts_addrs.iter().map(|(l, a)| (l.code().to_string(), a.to_string())).collect::<std::collections::BTreeMap<_, _>>(),
                // plans/BilingualPlan.md — the boot seed (Voice → Languages edits it live).
                "languages": c.language,
                "ollama_url": redact_url(&c.ollama_url),
            },
            "llm": {
                "boot_choice": llm,
                "engine": c.engine.as_str(),
                "anthropic_auth": c.anthropic_auth.as_str(),
                "anthropic_token_cmd": c.anthropic_token_cmd,
                "anthropic_max_tokens": c.anthropic_max_tokens,
                "openai_max_tokens": c.openai_max_tokens,
                "web_search": c.web_search,
                "search_provider": c.search_provider,
            },
            "prompt": {
                "system_prompt": c.system_prompt,
                "turn_timeout_secs": c.turn_timeout.as_secs(),
            },
            "speech": {
                "stt_engine": c.stt.engine.as_label(),
                "whisper_model": c.stt.model,
                "whisper_multilingual": c.stt.multilingual,
                "whisper_model_dir": path(&c.stt.model_dir),
                "whisper_model_path": c.stt.model_path.as_deref().map(path),
                "language": c.stt.language,
                "num_threads": c.stt.num_threads,
                "elevenlabs_model_id": c.stt.elevenlabs.model_id,
                "elevenlabs_base_url": redact_url(&c.stt.elevenlabs.base_url),
                "vad_engine": c.vad.engine.as_label(),
                "silero_model_path": path(&c.vad.silero.model_path),
                "silero_threshold": c.vad.silero.threshold,
            },
            "follow_up": {
                "enabled": c.follow_up.enabled,
                "max_chain": c.follow_up.max_chain,
                "question_wait_secs": c.follow_up.question_wait_secs,
                "reply_wait_secs": c.follow_up.reply_wait_secs,
                "history_turns": c.follow_up.history_turns,
                "history_window_secs": c.follow_up.history_window_secs,
            },
            "speaker": {
                "enabled": c.speaker.enabled,
                "model_path": c.speaker.model_path.as_deref().map(path),
                "match_threshold": c.speaker.match_threshold,
                "new_threshold": c.speaker.new_threshold,
                "min_speech_ms": c.speaker.min_speech_ms,
                "min_identify_ms": c.speaker.min_identify_ms,
                "short_clip_margin": c.speaker.short_clip_margin,
                "min_margin": c.speaker.min_margin,
                "clear_margin": c.speaker.clear_margin,
            },
            "memory": {
                "memory_backend": match c.memory_backend {
                    MemoryBackendChoice::Helix => "helix",
                    MemoryBackendChoice::Sqlite => "sqlite",
                },
                "embed_backend_seed": match c.graphrag.embed_backend {
                    crate::config::EmbedBackend::Local => "local",
                    crate::config::EmbedBackend::OpenAi => "openai",
                },
                "embed_model_path": path(&c.graphrag.embed_model_path),
                "embed_dims": c.graphrag.embed_dims,
                "extract_seed": if c.graphrag.extract_enabled { "auto" } else { "off" },
                "extract_model": c.graphrag.extract_model,
                "ingest_interval_secs": c.graphrag.ingest_interval.as_secs(),
                "recall_k": c.graphrag.recall_k,
            },
            "integrations": {
                "calendar_feeds": c.calendar_specs.iter()
                    .map(|s| json!({ "name": s.name, "url": redact_url(&s.url) }))
                    .collect::<Vec<_>>(),
                "calendar_cache_ttl_secs": c.calendar_cache_ttl.map(|d| d.as_secs()),
                "directions_provider": if c.directions_provider.is_empty() { "mapbox" } else { c.directions_provider.as_str() },
                "weather_enabled": c.weather.enabled,
                "weather_provider": c.weather.provider,
                "weather_refresh_secs": c.weather.refresh_interval_secs,
                "places_enabled": c.places.enabled,
                "places_provider": c.places.provider,
                "system1_backend": c.system1.backend,
                "system1_base_url": redact_url(&c.system1.base_url),
                "drive_client_id_set": set(&c.drive.client_id),
                "spotify_client_id_set": set(&c.spotify.client_id),
            },
            "music": {
                "enabled": c.music.enabled,
                "autostart": c.music.autostart,
                "snapserver_addr": c.music.snapserver_addr.to_string(),
                "duck_on_speech": c.music.duck_on_speech,
                "duck_percent": c.music.duck_percent,
                "group": group,
                "spotify_device_name": c.music.spotify_device_name,
            },
            "paths": {
                "db_path": path(&c.db_path),
                "chatlog_path": path(&c.chatlog_path),
                "promptlog_path": path(&c.promptlog_path),
                "helix_path": path(&c.helix_path),
                "settings_path": c.settings_path.as_deref().map(path),
                "audio_dump_dir": c.audio_dump_dir.as_deref().map(path),
            },
        });
        Self {
            stt_addr: c.stt_addr.to_string(),
            tts_addr: c.tts_addr.to_string(),
            ollama_url: c.ollama_url.clone(),
            config_file: config_file.map(path),
            config,
        }
    }
}

/// A URL safe to show: scheme + host[:port] + path, with any `user:pass@` userinfo,
/// query string and fragment removed (private `.ics` feed URLs and some service URLs
/// carry tokens there). Any long path segment (≥ 24 chars, typical of secret feed
/// ids) is elided too.
pub(super) fn redact_url(url: &str) -> String {
    let u = url.trim();
    let (scheme, rest) = match u.split_once("://") {
        Some((s, r)) => (format!("{s}://"), r),
        None => (String::new(), u),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let had_secret = authority.contains('@') || u.contains('?');
    let mut segs: Vec<String> = path.split('/').map(str::to_string).collect();
    let mut elided = false;
    for seg in segs.iter_mut() {
        if seg.len() >= 24 {
            *seg = "…".to_string();
            elided = true;
        }
    }
    let mut out = format!("{scheme}{host}{}", segs.join("/"));
    if had_secret || (elided && !out.ends_with('…')) {
        out.push_str(" (redacted)");
    }
    out
}

/// When the console's server started serving (≈ process start), for the uptime line.
static STARTED: OnceLock<Instant> = OnceLock::new();

/// Record the serve start (idempotent).
pub(super) fn mark_started() {
    STARTED.get_or_init(Instant::now);
}

// ------------------------------------------------------------------------------------
// Locality
// ------------------------------------------------------------------------------------

/// Where a stage runs, as the console's chips show it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tier {
    /// On this Mac (in-process or loopback).
    Local,
    /// Another machine on the private network.
    Lan,
    /// A public internet service.
    Cloud,
}

impl Tier {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Tier::Local => "local",
            Tier::Lan => "lan",
            Tier::Cloud => "cloud",
        }
    }
}

/// Classify a bare host (name or IP literal, no port): loopback/`localhost` → Local;
/// RFC 1918 / CGNAT / link-local / ULA / `*.local` / dotless hostnames → LAN;
/// everything else → Cloud. No DNS lookups — a named public host is Cloud.
pub(super) fn classify_host(host: &str) -> Tier {
    let h = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if h.is_empty() || h == "localhost" || h.ends_with(".localhost") {
        return Tier::Local;
    }
    if let Ok(ip) = h.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => {
                let o = v4.octets();
                if v4.is_loopback() || v4.is_unspecified() {
                    Tier::Local
                } else if v4.is_private()
                    || v4.is_link_local()
                    || (o[0] == 100 && (o[1] & 0xC0) == 64)
                {
                    Tier::Lan
                } else {
                    Tier::Cloud
                }
            }
            IpAddr::V6(v6) => {
                let seg = v6.segments()[0];
                if v6.is_loopback() || v6.is_unspecified() {
                    Tier::Local
                } else if (seg & 0xfe00) == 0xfc00 || (seg & 0xffc0) == 0xfe80 {
                    Tier::Lan
                } else {
                    Tier::Cloud
                }
            }
        };
    }
    if h.ends_with(".local") || h.ends_with(".lan") || h.ends_with(".home.arpa") || !h.contains('.')
    {
        return Tier::Lan;
    }
    Tier::Cloud
}

/// `(host, port)` of a URL or a `host:port` address. A missing port defaults from the
/// scheme (`https`/`wss` → 443, else 80).
pub(super) fn host_port(addr: &str) -> Option<(String, u16)> {
    let a = addr.trim();
    let (default_port, rest) = if let Some(r) = a.strip_prefix("https://") {
        (443, r)
    } else if let Some(r) = a.strip_prefix("wss://") {
        (443, r)
    } else if let Some(r) = a.strip_prefix("http://") {
        (80, r)
    } else if let Some(r) = a.strip_prefix("ws://") {
        (80, r)
    } else {
        (80, a)
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?; // drop any userinfo
    if authority.is_empty() {
        return None;
    }
    if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        let port = after
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return Some((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            Some((host.to_string(), port.parse().unwrap_or(default_port)))
        }
        _ => Some((authority.to_string(), default_port)),
    }
}

/// The tier of a URL / `host:port`; unparseable → Cloud (the conservative answer).
pub(super) fn classify_addr(addr: &str) -> Tier {
    host_port(addr)
        .map(|(h, _)| classify_host(&h))
        .unwrap_or(Tier::Cloud)
}

/// Can we open a TCP connection to `addr` (URL or `host:port`) within `timeout`?
async fn reachable(addr: &str, timeout: Duration) -> bool {
    let Some((host, port)) = host_port(addr) else {
        return false;
    };
    let target = format!("{host}:{port}");
    let resolved: Option<SocketAddr> = tokio::task::spawn_blocking(move || {
        target.to_socket_addrs().ok().and_then(|mut it| it.next())
    })
    .await
    .ok()
    .flatten();
    let Some(sa) = resolved else {
        return false;
    };
    matches!(
        tokio::time::timeout(timeout, tokio::net::TcpStream::connect(sa)).await,
        Ok(Ok(_))
    )
}

// ------------------------------------------------------------------------------------
// /overview.json
// ------------------------------------------------------------------------------------

fn parse(s: String) -> Value {
    serde_json::from_str(&s).unwrap_or(Value::Null)
}

/// `GET /overview.json` — one call for the dashboard: every pipeline stage with its
/// engine, tier and health, the connected displays, the integration statuses, the
/// recent-turn latency summary. `probe=false` (`?probe=0`, used by the app-bar pills
/// on every page) skips the TCP reachability probes and reports health `unknown`.
pub(super) async fn overview_json(s: &AppServices, probe_services: bool) -> String {
    let settings = s.settings();
    let view = settings.view().to_json(true, None);
    let stt = parse(stt_status_json(settings));
    let sys1 = parse(system1_status_json(settings));
    let boot = &s.debug.boot;
    let probe = Duration::from_millis(600);

    let stt_engine = stt["engine"].as_str().unwrap_or("wyoming").to_string();
    let llm_backend = view["llm_backend"].as_str().unwrap_or("").to_string();
    let sys1_backend = sys1["backend"].as_str().unwrap_or("none").to_string();
    let sys1_url = sys1["base_url"].as_str().unwrap_or("").to_string();

    // Reachability probes (TCP connect), concurrently, only for the stages in use.
    let stt_probe = async {
        if probe_services && stt_engine == "wyoming" && !boot.stt_addr.is_empty() {
            Some(reachable(&boot.stt_addr, probe).await)
        } else {
            None
        }
    };
    let tts_probe = async {
        if !probe_services || boot.tts_addr.is_empty() {
            None
        } else {
            Some(reachable(&boot.tts_addr, probe).await)
        }
    };
    let ollama_probe = async {
        if probe_services && llm_backend == "ollama" && !boot.ollama_url.is_empty() {
            Some(reachable(&boot.ollama_url, probe).await)
        } else {
            None
        }
    };
    let laya_probe = async {
        if probe_services && sys1_backend == "laya-serve" && !sys1_url.is_empty() {
            Some(reachable(&sys1_url, probe).await)
        } else {
            None
        }
    };
    let (stt_up, tts_up, ollama_up, laya_up) =
        tokio::join!(stt_probe, tts_probe, ollama_probe, laya_probe);

    // ---- Stages -----------------------------------------------------------------
    let vad_engine = view["vad_engine"].as_str().unwrap_or("silero");
    let vad = json!({
        "key": "vad", "label": "End-of-speech", "engine": vad_engine,
        "tier": "local", "status": "ok", "href": "/voice#vad",
    });

    let el_key = stt["elevenlabs_key_set"].as_bool().unwrap_or(false);
    let stt_stage = match stt_engine.as_str() {
        "elevenlabs" if !el_key => json!({
            "key": "stt", "label": "Speech-to-text", "engine": "ElevenLabs Scribe",
            "tier": "cloud", "status": "fallback",
            "detail": "no API key — falling back to Wyoming",
            "fallback_tier": classify_addr(&boot.stt_addr).as_str(),
            "href": "/voice#stt",
        }),
        "elevenlabs" => json!({
            "key": "stt", "label": "Speech-to-text", "engine": "ElevenLabs Scribe",
            "tier": "cloud", "status": "ok", "href": "/voice#stt",
        }),
        "whisper-rs" => json!({
            "key": "stt", "label": "Speech-to-text", "engine": "whisper-rs",
            "tier": "local", "status": "ok", "detail": "in-process", "href": "/voice#stt",
        }),
        _ => json!({
            "key": "stt", "label": "Speech-to-text", "engine": "Wyoming Whisper",
            "tier": classify_addr(&boot.stt_addr).as_str(),
            "status": status_of(stt_up), "detail": boot.stt_addr, "href": "/voice#stt",
        }),
    };

    let sys1_stage = match sys1_backend.as_str() {
        "none" | "" => json!({
            "key": "system1", "label": "Fast path", "engine": "Off",
            "tier": "off", "status": "off", "href": "/intelligence#system1",
        }),
        "jev" => json!({
            "key": "system1", "label": "Fast path", "engine": "jev (OpenRouter)",
            "tier": "cloud",
            "status": if sys1["openrouter_key_set"].as_bool().unwrap_or(false) { "ok" } else { "needs_key" },
            "detail": if sys1["openrouter_key_set"].as_bool().unwrap_or(false) { "" } else { "no OpenRouter key" },
            "href": "/intelligence#system1",
        }),
        other => json!({
            "key": "system1", "label": "Fast path", "engine": other,
            "tier": classify_addr(&sys1_url).as_str(),
            "status": status_of(laya_up), "detail": sys1_url, "href": "/intelligence#system1",
        }),
    };

    let model = view["llm_model"].as_str().unwrap_or("").to_string();
    let llm_stage = match llm_backend.as_str() {
        "ollama" => json!({
            "key": "llm", "label": "Language model", "engine": format!("Ollama {model}").trim().to_string(),
            "tier": classify_addr(&boot.ollama_url).as_str(),
            "status": status_of(ollama_up), "detail": boot.ollama_url, "href": "/intelligence#llm",
        }),
        "mock" => json!({
            "key": "llm", "label": "Language model", "engine": "Mock",
            "tier": "local", "status": "ok", "detail": "offline echo", "href": "/intelligence#llm",
        }),
        other => json!({
            "key": "llm", "label": "Language model",
            "engine": format!("{} {model}", provider_name(other)).trim().to_string(),
            "tier": "cloud", "status": "ok", "href": "/intelligence#llm",
        }),
    };

    let web_search = view["web_search"].as_bool().unwrap_or(false);
    let search_stage = if web_search {
        let p = view["search_provider"].as_str().unwrap_or("tavily");
        let keyless = p == "duckduckgo";
        let key_ok = keyless || view["search_key_set"].as_bool().unwrap_or(false);
        json!({
            "key": "search", "label": "Web search",
            "engine": if keyless { "DuckDuckGo" } else { "Tavily" },
            "tier": "cloud", "keyless": keyless,
            "status": if key_ok { "ok" } else { "needs_key" },
            "detail": if key_ok { "" } else { "no Tavily key" },
            "href": "/intelligence#search",
        })
    } else {
        json!({
            "key": "search", "label": "Web search", "engine": "Off",
            "tier": "off", "status": "off", "href": "/intelligence#search",
        })
    };

    let (embed_stage, extract_stage) = match &s.graphrag {
        Some(ctrl) => {
            let st = ctrl.status().await;
            let ex = ctrl.extract_status();
            let embed = json!({
                "key": "embed", "label": "Memory recall",
                "engine": if st.active == "openai" { "HelixDB · OpenAI embeddings" } else { "HelixDB · nomic (local)" },
                "tier": if st.active == "openai" { "cloud" } else { "local" },
                "status": "ok", "href": "/memory#settings",
            });
            let extract = if ex.active {
                json!({
                    "key": "extract", "label": "Entity extraction",
                    "engine": format!("Anthropic {}", ex.model),
                    "tier": "cloud", "status": "ok", "href": "/memory#settings",
                })
            } else {
                json!({
                    "key": "extract", "label": "Entity extraction",
                    "engine": "Off", "tier": "off", "status": "off",
                    "detail": if ex.mode == "off" { "turned off" } else { "no Anthropic key at boot" },
                    "href": "/memory#settings",
                })
            };
            (embed, extract)
        }
        None => (
            json!({
                "key": "embed", "label": "Memory recall", "engine": "SQLite FTS",
                "tier": "local", "status": "ok", "href": "/memory#settings",
            }),
            json!({
                "key": "extract", "label": "Entity extraction", "engine": "Off",
                "tier": "off", "status": "off", "detail": "GraphRAG not active",
                "href": "/memory#settings",
            }),
        ),
    };

    let tts_stage = json!({
        "key": "tts", "label": "Text-to-speech",
        "engine": match view["tts_voice"].as_str() { Some(v) if !v.is_empty() => format!("Piper · {v}"), _ => "Piper".to_string() },
        "tier": classify_addr(&boot.tts_addr).as_str(),
        "status": status_of(tts_up), "detail": boot.tts_addr, "href": "/voice#tts",
    });

    let stages = vec![
        vad,
        stt_stage,
        sys1_stage,
        llm_stage,
        search_stage,
        embed_stage,
        extract_stage,
        tts_stage,
    ];

    // ---- Integrations ------------------------------------------------------------
    let integrations = integrations_summary(s);

    // ---- Displays ---------------------------------------------------------------
    let devices = s.notify.connected_devices();

    // ---- Recent turns -----------------------------------------------------------
    let speakers = super::speakers::SpeakerDirectory::build(s);
    let turns: Vec<Value> = chatlog::read_tail(&s.debug.chatlog_path, 20)
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let speaker_tag = speakers.turn_tag(&r);
            let t = r.timing.unwrap_or_default();
            json!({
                "id": r.id,
                "turn_id": r.turn_id,
                "ts": r.ts,
                "speaker": r.speaker_name.clone().unwrap_or_else(|| r.speaker_id.clone()),
                "speaker_tag": speaker_tag,
                "transcript": clip(&r.transcript, 140),
                "reply": clip(&r.reply, 160),
                "llm_backend": r.llm_backend,
                "model": r.model,
                "path": t.path,
                "stt_ms": t.stt_ms,
                "system1_ms": t.system1_ms,
                "fetch_ms": t.fetch_ms,
                "recall_ms": t.recall_ms,
                "prompt_ms": t.prompt_ms,
                "llm_ms": t.llm_ms,
                "tts_ms": t.tts_ms,
                "total_ms": t.total_ms,
            })
        })
        .collect();
    let mut totals: Vec<u64> = turns
        .iter()
        .filter_map(|t| t["total_ms"].as_u64())
        .collect();
    totals.sort_unstable();

    json!({
        "ok": true,
        "service_name": boot.config["identity"]["service_name"],
        "instance_id": boot.config["identity"]["instance_id"],
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": STARTED.get().map(|t| t.elapsed().as_secs()),
        "stages": stages,
        "integrations": integrations,
        "devices": devices,
        "turns": turns,
        // Unknown-speaker memories awaiting review on /memory#unknown.
        "unknown_memories": s.memory().list_unknown().map(|v| v.len()).unwrap_or(0),
        "latency": {
            "count": totals.len(),
            "p50_ms": percentile(&totals, 50),
            "p95_ms": percentile(&totals, 95),
        },
    })
    .to_string()
}

fn status_of(probe: Option<bool>) -> &'static str {
    match probe {
        Some(true) => "ok",
        Some(false) => "error",
        None => "unknown",
    }
}

fn provider_name(backend: &str) -> &str {
    match backend {
        "anthropic" => "Claude",
        "openai" => "OpenAI",
        other => other,
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn percentile(sorted: &[u64], p: usize) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((sorted.len() - 1) * p + 50) / 100;
    sorted.get(idx).copied()
}

/// One row per integration tile: `{key, name, tool, tier, keyless?, status, detail}`.
/// `status` is `active` / `attention` / `inactive` / `disabled`.
pub(super) fn integrations_summary(s: &AppServices) -> Vec<Value> {
    let settings = s.settings();
    let weather = parse(weather_status_json(settings));
    let places = parse(places_status_json(settings));
    let cadora = parse(cadora_status_json(settings));
    let appsaid = parse(appsaid_status_json(settings));
    let ha = parse(ha_status_json(settings));
    let spotify = parse(spotify_status_json(settings));
    let drive = parse(drive_status_json(settings));
    let feeds = s.debug.boot.config["integrations"]["calendar_feeds"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);

    let b = |v: &Value, k: &str| v[k].as_bool().unwrap_or(false);

    let weather_row = if !b(&weather, "enabled") {
        json!({ "status": "disabled", "detail": "disabled in anamanti.json" })
    } else if weather["effective"] == "visualcrossing" {
        json!({ "status": "active", "detail": "Visual Crossing" })
    } else if weather["provider"] == "visualcrossing" {
        json!({ "status": "attention", "detail": "no Visual Crossing key — using Open-Meteo" })
    } else {
        json!({ "status": "active", "detail": "Open-Meteo (keyless)", "keyless": true })
    };
    let mapbox = settings.mapbox_token_set();

    let mut rows = vec![
        merge(
            json!({ "key": "weather", "name": "Weather", "tool": "weather_lookup", "tier": "cloud" }),
            weather_row,
        ),
        json!({
            "key": "places", "name": "Places", "tool": "places_lookup", "tier": "cloud",
            "status": if b(&places, "tool_active") { "active" } else { "inactive" },
            "detail": if b(&places, "tool_active") { "Google Places" } else { "needs an API key" },
        }),
        json!({
            "key": "directions", "name": "Directions", "tool": "directions_lookup", "tier": "cloud",
            "status": if mapbox { "active" } else { "inactive" },
            "detail": if mapbox { "Mapbox" } else { "needs a Mapbox token" },
        }),
        json!({
            "key": "calendar", "name": "Calendar", "tool": "calendar_lookup", "tier": "cloud",
            "status": if settings.calendar_active() { "active" } else { "inactive" },
            "detail": if feeds == 0 { "no feeds in anamanti.json".to_string() } else { format!("{feeds} feed(s), read-only") },
            "readonly": true,
        }),
        json!({
            "key": "cadora", "name": "Shopping list", "tool": "shopping_list_add", "tier": "cloud",
            "status": if b(&cadora, "linked") { "active" } else { "inactive" },
            "detail": if b(&cadora, "linked") { "Cadora linked" } else { "not linked" },
        }),
        json!({
            "key": "appsaid", "name": "Phone messages", "tool": "send_phone_message", "tier": "cloud",
            "status": if b(&appsaid, "tool_active") { "active" } else if b(&appsaid, "token_set") { "attention" } else { "inactive" },
            "detail": match appsaid["recipients"].as_array().map(Vec::len).unwrap_or(0) {
                0 => "no recipients".to_string(),
                n => format!("{n} recipient(s)"),
            },
        }),
        json!({
            "key": "home_assistant", "name": "Home Assistant", "tool": "home_control",
            // Wherever the HA server is (normally the LAN), classified from its URL.
            "tier": match ha["base_url"].as_str().filter(|u| !u.is_empty()) {
                Some(url) => classify_addr(url).as_str(),
                None => "lan",
            },
            "status": if b(&ha, "active") { "active" } else if b(&ha, "configured") || b(&ha, "token_set") || ha["base_url"].as_str().is_some_and(|u| !u.is_empty()) { "attention" } else { "inactive" },
            "detail": if b(&ha, "active") {
                match ha["default_room"].as_str().filter(|r| !r.is_empty()) {
                    Some(room) => format!("connected · default room {room}"),
                    None => "connected".to_string(),
                }
            } else if b(&ha, "token_set") { "needs a URL".to_string() } else if ha["base_url"].as_str().is_some_and(|u| !u.is_empty()) { "needs a token".to_string() } else { "not configured".to_string() },
        }),
        json!({
            "key": "spotify", "name": "Spotify", "tool": "spotify_control", "tier": "cloud",
            "status": if b(&spotify, "linked") { "active" } else if b(&spotify, "configured") { "attention" } else { "inactive" },
            "detail": if b(&spotify, "linked") { "linked" } else if b(&spotify, "configured") { "credentials set, not linked" } else { "no credentials" },
        }),
        json!({
            "key": "drive", "name": "Google Drive photos", "tool": "slideshow", "tier": "cloud",
            "status": if b(&drive, "linked") { "active" } else if b(&drive, "configured") { "attention" } else { "inactive" },
            "detail": if b(&drive, "linked") {
                format!("{} folder(s)", drive["folder_ids"].as_array().map(Vec::len).unwrap_or(0))
            } else if b(&drive, "configured") { "configured, not linked".to_string() } else { "not configured".to_string() },
        }),
    ];
    for r in &mut rows {
        if r.get("keyless").is_none() {
            r["keyless"] = json!(false);
        }
    }
    rows
}

fn merge(mut a: Value, b: Value) -> Value {
    if let (Some(ao), Some(bo)) = (a.as_object_mut(), b.as_object()) {
        for (k, v) in bo {
            ao.insert(k.clone(), v.clone());
        }
    }
    a
}

// ------------------------------------------------------------------------------------
// /keys.json
// ------------------------------------------------------------------------------------

/// `GET /keys.json` — every secret / OAuth link the Core holds: whether it is set,
/// what uses it, its seed env var, and where to edit it. Never returns a value.
pub(super) fn keys_json(s: &AppServices) -> String {
    let settings = s.settings();
    let view = settings.view().to_json(true, None);
    let stt = parse(stt_status_json(settings));
    let sys1 = parse(system1_status_json(settings));
    let weather = parse(weather_status_json(settings));
    let places = parse(places_status_json(settings));
    let cadora = parse(cadora_status_json(settings));
    let appsaid = parse(appsaid_status_json(settings));
    let ha = parse(ha_status_json(settings));
    let spotify = parse(spotify_status_json(settings));
    let drive = parse(drive_status_json(settings));
    let b = |v: &Value, k: &str| v[k].as_bool().unwrap_or(false);
    let key = |id: &str,
               service: &str,
               kind: &str,
               set: bool,
               env: &str,
               used_by: &str,
               edit: &str,
               field: Value| {
        json!({
            "id": id, "service": service, "kind": kind, "set": set, "env": env,
            "used_by": used_by, "edit": edit, "field": field,
        })
    };
    let keys = vec![
        key(
            "anthropic_api_key",
            "Anthropic API key",
            "secret",
            b(&view, "anthropic_key_set"),
            "ANTHROPIC_API_KEY",
            "Claude LLM (API-key auth) · entity extraction (env at boot)",
            "/intelligence#llm",
            json!({ "endpoint": "/config", "name": "anthropic_api_key" }),
        ),
        key(
            "anthropic_oauth_token",
            "Anthropic subscription token",
            "secret",
            b(&view, "anthropic_oauth_token_set"),
            "ANTHROPIC_OAUTH_TOKEN",
            "Claude LLM (subscription auth)",
            "/intelligence#llm",
            json!({ "endpoint": "/config", "name": "anthropic_oauth_token" }),
        ),
        key(
            "openai_api_key",
            "OpenAI API key",
            "secret",
            b(&view, "openai_key_set"),
            "OPENAI_API_KEY",
            "OpenAI LLM · OpenAI embeddings (env at boot)",
            "/intelligence#llm",
            json!({ "endpoint": "/config", "name": "openai_api_key" }),
        ),
        key(
            "search_api_key",
            "Tavily API key",
            "secret",
            b(&view, "search_key_set"),
            "TAVILY_API_KEY",
            "Web search (Tavily)",
            "/intelligence#search",
            json!({ "endpoint": "/config", "name": "search_api_key" }),
        ),
        key(
            "elevenlabs_api_key",
            "ElevenLabs API key",
            "secret",
            b(&stt, "elevenlabs_key_set"),
            "ELEVENLABS_API_KEY",
            "Speech-to-text (ElevenLabs Scribe)",
            "/voice#stt",
            json!({ "endpoint": "/stt/save", "name": "elevenlabs_api_key", "clear": true }),
        ),
        key(
            "openrouter_api_key",
            "OpenRouter API key",
            "secret",
            b(&sys1, "openrouter_key_set"),
            "OPENROUTER_API_KEY",
            "Fast path (System-1 jev)",
            "/intelligence#system1",
            json!({ "endpoint": "/system1/save", "name": "openrouter_api_key" }),
        ),
        key(
            "mapbox_token",
            "Mapbox token",
            "secret",
            settings.mapbox_token_set(),
            "MAPBOX_TOKEN",
            "Directions",
            "/integrations#directions",
            json!({ "endpoint": "/tools/save", "name": "mapbox_token" }),
        ),
        key(
            "visualcrossing_key",
            "Visual Crossing API key",
            "secret",
            b(&weather, "key_set"),
            "VISUALCROSSING_API_KEY",
            "Weather",
            "/integrations#weather",
            json!({ "endpoint": "/tools/weather/save", "name": "visualcrossing_key", "clear": true }),
        ),
        key(
            "google_places_key",
            "Google Places API key",
            "secret",
            b(&places, "key_set"),
            "GOOGLE_PLACES_API_KEY",
            "Places",
            "/integrations#places",
            json!({ "endpoint": "/tools/places/save", "name": "google_places_key", "clear": true }),
        ),
        key(
            "app_token",
            "AppSaid app token",
            "secret",
            b(&appsaid, "token_set"),
            "APPSAID_APP_TOKEN",
            "Phone messages",
            "/integrations#appsaid",
            json!({ "endpoint": "/appsaid/token", "name": "app_token" }),
        ),
        key(
            "home_assistant_token",
            "Home Assistant token",
            "secret",
            b(&ha, "token_set"),
            "HOME_ASSISTANT_TOKEN",
            "Home Assistant",
            "/integrations#home_assistant",
            json!({ "endpoint": "/home-assistant/token", "name": "token", "clear": true }),
        ),
        key(
            "spotify",
            "Spotify",
            "oauth",
            b(&spotify, "linked"),
            "",
            if b(&spotify, "configured") {
                "Spotify control · credentials set"
            } else {
                "Spotify control"
            },
            "/integrations#spotify",
            Value::Null,
        ),
        key(
            "drive",
            "Google Drive",
            "oauth",
            b(&drive, "linked"),
            "",
            "Photo slideshow",
            "/integrations#drive",
            Value::Null,
        ),
        key(
            "cadora",
            "Cadora shopping list",
            "link",
            b(&cadora, "linked"),
            "",
            "Shopping list",
            "/integrations#cadora",
            Value::Null,
        ),
    ];
    json!({
        "ok": true,
        "settings_path": s.debug.boot.config["paths"]["settings_path"],
        "keys": keys,
    })
    .to_string()
}

// ------------------------------------------------------------------------------------
// /system.json
// ------------------------------------------------------------------------------------

/// `GET /system.json` — build metadata, runtime identity/uptime and the redacted
/// effective boot configuration.
pub(super) fn system_json(debug: &DebugSources) -> String {
    let about: Value = parse(about_json());
    json!({
        "ok": true,
        "build": about,
        "uptime_secs": STARTED.get().map(|t| t.elapsed().as_secs()),
        "config_file": debug.boot.config_file,
        "memory_backend": debug.memory_backend,
        "config": debug.boot.config,
    })
    .to_string()
}

// ------------------------------------------------------------------------------------
// /extraction/*
// ------------------------------------------------------------------------------------

/// `GET /extraction/status.json` — the cloud entity-extraction state.
pub(super) fn extraction_status_json(s: &AppServices) -> String {
    match &s.graphrag {
        Some(ctrl) => {
            let mut v = serde_json::to_value(ctrl.extract_status()).unwrap_or(Value::Null);
            v["ok"] = json!(true);
            v["available"] = json!(true);
            v.to_string()
        }
        None => json!({
            "ok": true,
            "available": false,
            "active": false,
            "mode": "off",
            "reason": "GraphRAG memory is not active (memory_backend is sqlite, or init failed at boot)",
        })
        .to_string(),
    }
}

/// `POST /extraction/save` — `{ "enabled": bool }` turns cloud entity extraction on
/// (auto) or off, live + persisted.
pub(super) fn extraction_save_json(s: &AppServices, data: &Value) -> String {
    let Some(ctrl) = &s.graphrag else {
        return super::fail("GraphRAG memory is not active");
    };
    let Some(on) = data.get("enabled").and_then(Value::as_bool) else {
        return super::fail("missing boolean 'enabled'");
    };
    ctrl.set_extraction(on);
    extraction_status_json(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_hosts_into_tiers() {
        assert_eq!(classify_host("127.0.0.1"), Tier::Local);
        assert_eq!(classify_host("localhost"), Tier::Local);
        assert_eq!(classify_host("[::1]"), Tier::Local);
        assert_eq!(classify_host("0.0.0.0"), Tier::Local);
        assert_eq!(classify_host("192.168.1.20"), Tier::Lan);
        assert_eq!(classify_host("10.0.0.5"), Tier::Lan);
        assert_eq!(classify_host("172.20.1.1"), Tier::Lan);
        assert_eq!(classify_host("100.100.1.1"), Tier::Lan);
        assert_eq!(classify_host("fd00::1"), Tier::Lan);
        assert_eq!(classify_host("mac-mini.local"), Tier::Lan);
        assert_eq!(classify_host("nas"), Tier::Lan);
        assert_eq!(classify_host("api.anthropic.com"), Tier::Cloud);
        assert_eq!(classify_host("8.8.8.8"), Tier::Cloud);
    }

    #[test]
    fn parses_urls_and_addrs() {
        assert_eq!(
            host_port("http://127.0.0.1:11434"),
            Some(("127.0.0.1".into(), 11434))
        );
        assert_eq!(
            host_port("https://api.openai.com/v1"),
            Some(("api.openai.com".into(), 443))
        );
        assert_eq!(
            host_port("192.168.1.9:10300"),
            Some(("192.168.1.9".into(), 10300))
        );
        assert_eq!(host_port("http://[::1]:8000/x"), Some(("::1".into(), 8000)));
        assert_eq!(
            host_port("wss://api.elevenlabs.io"),
            Some(("api.elevenlabs.io".into(), 443))
        );
        assert_eq!(classify_addr("http://127.0.0.1:8000"), Tier::Local);
        assert_eq!(classify_addr("http://studio.local:11434"), Tier::Lan);
        assert_eq!(classify_addr(""), Tier::Cloud);
    }

    #[test]
    fn redact_url_strips_secrets() {
        assert_eq!(
            redact_url("http://127.0.0.1:11434"),
            "http://127.0.0.1:11434"
        );
        assert_eq!(
            redact_url("https://user:pw@example.com/a?token=x"),
            "https://example.com/a (redacted)"
        );
        let feed = redact_url(
            "https://calendar.google.com/calendar/ical/abcdefghijklmnopqrstuvwxyz123/basic.ics",
        );
        assert!(!feed.contains("abcdefghijklmnopqrstuvwxyz123"), "{feed}");
        assert_eq!(
            redact_url("webcal://p01.icloud.com/published/2/MTIzNDU2Nzg5MDEyMzQ1Njc4OTAxMjM0"),
            "webcal://p01.icloud.com/published/2/…"
        );
    }

    #[test]
    fn percentile_and_clip() {
        assert_eq!(percentile(&[], 50), None);
        assert_eq!(percentile(&[5], 95), Some(5));
        assert_eq!(percentile(&[1, 2, 3, 4, 5], 50), Some(3));
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("abc", 3), "abc");
    }
}
