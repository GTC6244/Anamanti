use super::debug::*;
use super::drive::*;
use super::household::*;
use super::music::*;
use super::routes::{self, Handler};
use super::tools::*;
use super::*;
use crate::llm::catalog::ModelCatalog;
use crate::llm::mock::MockLlm;
use crate::memory::MemoryStore;
use crate::notify::NotificationService;
use crate::orchestrator::ServiceConnector;

/// Serve one synchronous route straight from the route table against `settings`
/// (no socket): `(status, content type, body)`; `404` when it is not a route.
fn route(
    method: &str,
    target: &str,
    body: &[u8],
    settings: &Arc<SharedSettings>,
) -> (&'static str, &'static str, Vec<u8>) {
    let svc = services_with(settings.clone(), None, debug());
    let path = target.split(['?', '#']).next().unwrap_or(target);
    let req = Request { target, path, body };
    let reply = match routes::lookup(method, path) {
        Some(Handler::Sync(handler)) => handler(&svc, &req),
        Some(Handler::Async(_)) => panic!("{method} {path} is async; drive it over a socket"),
        None => Reply::not_found(),
    };
    (reply.status, reply.content_type, reply.body)
}

/// POST a raw body to a synchronous JSON route and return the response payload.
fn post(settings: &Arc<SharedSettings>, path: &str, body: &[u8]) -> String {
    let (status, ctype, out) = route("POST", path, body, settings);
    assert_eq!((status, ctype), ("200 OK", "application/json"), "{path}");
    String::from_utf8(out).unwrap()
}

// Raw-body shims over the save routes, so the tests below go through the route table
// (body parsing included) under the handlers' names.
fn stt_save_json(s: &Arc<SharedSettings>, body: &[u8]) -> String {
    post(s, "/stt/save", body)
}
fn drive_save_json(s: &Arc<SharedSettings>, body: &[u8]) -> String {
    post(s, "/drive/save", body)
}
fn spotify_save_json(s: &Arc<SharedSettings>, body: &[u8]) -> String {
    post(s, "/spotify/save", body)
}
fn directions_save_json(s: &Arc<SharedSettings>, body: &[u8]) -> String {
    post(s, "/tools/save", body)
}
fn weather_save_json(s: &Arc<SharedSettings>, body: &[u8]) -> String {
    post(s, "/tools/weather/save", body)
}
fn cadora_save_json(s: &Arc<SharedSettings>, body: &[u8]) -> String {
    post(s, "/cadora/save", body)
}
fn appsaid_save_json(s: &Arc<SharedSettings>, body: &[u8]) -> String {
    post(s, "/appsaid/save", body)
}
fn appsaid_token_json(s: &Arc<SharedSettings>, body: &[u8]) -> String {
    post(s, "/appsaid/token", body)
}

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

fn notify() -> Arc<NotificationService> {
    Arc::new(NotificationService::new())
}

/// The config page's services around `settings` (in-memory store, mock voices).
fn services_with(
    settings: Arc<SharedSettings>,
    voices_dir: Option<std::path::PathBuf>,
    debug: DebugSources,
) -> AppServices {
    let pipeline = crate::orchestrator::Pipeline::with_settings(
        settings,
        Arc::new(MemoryStore::open_in_memory().unwrap()),
        "persona",
        std::time::Duration::from_secs(5),
    );
    AppServices::new(pipeline, connector(), catalog())
        .with_voices_dir(voices_dir)
        .with_notify(notify())
        .with_debug(debug)
}

fn debug() -> DebugSources {
    DebugSources {
        chatlog_path: std::env::temp_dir().join("wc_test_chatlog.jsonl"),
        promptlog_path: std::env::temp_dir().join("wc_test_promptlog.jsonl"),
        db_path: std::path::PathBuf::from(":memory:"),
        helix_path: std::path::PathBuf::from("anamanti_helix"),
        memory_backend: "sqlite".to_string(),
        graph: None,
        boot: super::BootInfo::default(),
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
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("Anamanti Core"));
    assert!(
        html.contains("Voice pipeline"),
        "the root is the Overview dashboard"
    );
}

/// Every page of the restructured console (plans/CoreWebUIPlan.md §4): served as
/// HTML inside the shared shell, titled, and carrying the full navigation.
const PAGES: &[(&str, &str)] = &[
    ("/", "Overview"),
    ("/voice", "Voice"),
    ("/intelligence", "Intelligence"),
    ("/personality", "Personality"),
    ("/household", "Household"),
    ("/integrations", "Integrations"),
    ("/devices", "Music &amp; Devices"),
    ("/memory", "Memory"),
    ("/conversations", "Conversations"),
    ("/keys", "Keys &amp; Accounts"),
    ("/system", "System"),
];

#[test]
fn every_page_renders_in_the_shell_with_the_full_nav() {
    for &(path, title) in PAGES {
        let (status, ctype, body) = route("GET", path, b"", &settings());
        assert_eq!(status, "200 OK", "{path}");
        assert!(ctype.starts_with("text/html"), "{path}");
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains(&format!("<h1>{title}</h1>")), "{path} title");
        assert!(html.contains("/ui.js"), "{path} loads the component kit");
        for &(href, _) in PAGES {
            assert!(
                html.contains(&format!("href=\"{href}\"")),
                "{path} nav is missing {href}"
            );
        }
        // No page falls back to the browser's modal dialogs.
        for dialog in ["alert", "prompt", "confirm"] {
            assert!(!calls(&html, dialog), "{path} calls {dialog}()");
        }
    }
}

/// Whether `src` calls the bare function `name(` (not as part of a longer identifier
/// such as `ui.confirm(` or `renderPrompt(`; `window.alert(` still counts).
fn calls(src: &str, name: &str) -> bool {
    let needle = format!("{name}(");
    src.match_indices(&needle).any(|(i, _)| {
        let before = &src[..i];
        let prev = before.chars().next_back();
        match prev {
            Some(c) if c.is_alphanumeric() || c == '_' => false,
            Some('.') => before.ends_with("window."),
            _ => true,
        }
    })
}

/// Serve one synchronous route and return its `(status, Location)`.
fn route_location(path: &str) -> (&'static str, Option<&'static str>) {
    let svc = services_with(settings(), None, debug());
    let req = Request {
        target: path,
        path,
        body: b"",
    };
    match routes::lookup("GET", path) {
        Some(Handler::Sync(handler)) => {
            let reply = handler(&svc, &req);
            (reply.status, reply.location)
        }
        _ => ("404 Not Found", None),
    }
}

/// The pre-restructure page routes keep working as redirects to their new homes.
#[test]
fn legacy_page_routes_redirect_to_their_new_homes() {
    for (old, new) in [
        ("/stt", "/voice#stt"),
        ("/pronunciations", "/voice#fixes"),
        ("/system1", "/intelligence#system1"),
        ("/tools", "/integrations"),
        ("/drive", "/integrations#drive"),
        ("/music", "/devices#music"),
        ("/notifications", "/devices#displays"),
        ("/embeddings", "/memory#settings"),
        ("/sqlite", "/memory#facts"),
        ("/helix", "/memory#graph"),
        ("/chatlog", "/conversations"),
        ("/prompts", "/conversations"),
        ("/about", "/system"),
    ] {
        assert_eq!(route_location(old), ("302 Found", Some(new)), "{old}");
    }
}

#[tokio::test]
async fn redirects_carry_a_location_header_on_the_wire() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = handle(stream, &services_with(settings(), None, debug())).await;
    });
    let mut client = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "GET /helix HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        addr.port()
    );
    client.write_all(req.as_bytes()).await.unwrap();
    let mut resp = String::new();
    client.read_to_string(&mut resp).await.unwrap();
    assert!(resp.starts_with("HTTP/1.1 302 Found\r\n"), "resp: {resp}");
    assert!(
        resp.contains("\r\nLocation: /memory#graph\r\n"),
        "resp: {resp}"
    );
}

#[test]
fn keys_json_lists_every_secret_without_values() {
    let s = settings();
    post(
        &s,
        "/tools/save",
        br#"{"mapbox_token":"pk.super-secret-token"}"#,
    );
    let (status, _c, body) = route("GET", "/keys.json", b"", &s);
    assert_eq!(status, "200 OK");
    let text = String::from_utf8_lossy(&body);
    assert!(
        !text.contains("pk.super-secret-token"),
        "a secret leaked: {text}"
    );
    let v: Value = serde_json::from_slice(&body).unwrap();
    let keys = v["keys"].as_array().unwrap();
    let mapbox = keys.iter().find(|k| k["id"] == "mapbox_token").unwrap();
    assert_eq!(mapbox["set"], true);
    assert_eq!(mapbox["field"]["endpoint"], "/tools/save");
    for id in [
        "anthropic_api_key",
        "openai_api_key",
        "search_api_key",
        "elevenlabs_api_key",
        "openrouter_api_key",
        "visualcrossing_key",
        "google_places_key",
        "app_token",
        "spotify",
        "drive",
        "cadora",
    ] {
        assert!(keys.iter().any(|k| k["id"] == id), "missing {id}");
    }
}

#[test]
fn system_json_reports_build_and_boot_config() {
    let (status, _c, body) = route("GET", "/system.json", b"", &settings());
    assert_eq!(status, "200 OK");
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["build"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["memory_backend"], "sqlite");
}

#[test]
fn boot_info_snapshot_is_redacted() {
    let mut c = crate::config::Config::default();
    c.drive.client_secret = Some("drive-secret-value".into());
    c.spotify.client_secret = Some("spotify-secret-value".into());
    c.cadora.link_token = Some("vl_cadora-secret".into());
    let b = BootInfo::from_config(&c, None);
    let text = b.config.to_string();
    for secret in [
        "drive-secret-value",
        "spotify-secret-value",
        "vl_cadora-secret",
    ] {
        assert!(!text.contains(secret), "boot snapshot leaked {secret}");
    }
    assert_eq!(b.stt_addr, c.stt_addr.to_string());
    assert!(b.config["identity"]["instance_id"].is_string());
}

#[test]
fn extraction_status_reports_unavailable_without_graphrag() {
    let (status, _c, body) = route("GET", "/extraction/status.json", b"", &settings());
    assert_eq!(status, "200 OK");
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["available"], false);
    assert_eq!(v["active"], false);
    let out = post(&settings(), "/extraction/save", br#"{"enabled":false}"#);
    assert!(out.contains("\"ok\":false"), "{out}");
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
fn about_json_reports_the_build() {
    let (status, ctype, body) = route("GET", "/about.json", b"", &settings());
    assert_eq!(status, "200 OK");
    assert_eq!(ctype, "application/json");
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    assert!(v.get("git_sha").is_some());
    assert!(v.get("build_time").is_some());
}

#[test]
fn system1_status_reports_default() {
    let s = settings();
    // The status endpoint reports the default disabled engine.
    let json = system1_status_json(&s);
    assert!(json.contains("\"backend\":\"none\""), "status: {json}");
    assert!(json.contains("\"active\":false"), "status: {json}");
}

#[test]
fn stt_status_reports_default() {
    let s = settings();
    // The status endpoint reports the default Wyoming engine, no key.
    let json = stt_status_json(&s);
    assert!(json.contains("\"engine\":\"wyoming\""), "status: {json}");
    assert!(
        json.contains("\"elevenlabs_key_set\":false"),
        "status: {json}"
    );
}

#[test]
fn stt_save_switches_engine_live() {
    let s = settings();
    let out = stt_save_json(
        &s,
        br#"{"engine":"elevenlabs","elevenlabs_api_key":"xi-k"}"#,
    );
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["engine"], "elevenlabs");
    assert_eq!(v["elevenlabs_key_set"], true);
    assert_eq!(
        s.snapshot().stt.engine,
        crate::config::SttEngineKind::ElevenLabs
    );
    // An unknown engine is rejected without changing the live selection.
    let bad: Value = serde_json::from_str(&stt_save_json(&s, br#"{"engine":"nope"}"#)).unwrap();
    assert_eq!(bad["ok"], false);
    assert_eq!(
        s.snapshot().stt.engine,
        crate::config::SttEngineKind::ElevenLabs
    );
}

#[test]
fn drive_status_reports_unconfigured_by_default() {
    let v: Value = serde_json::from_str(&drive_status_json(&settings())).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["configured"], false);
    assert_eq!(v["linked"], false);
    assert_eq!(v["client_secret_set"], false);
    assert_eq!(v["folder_ids"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn drive_folders_json_refuses_when_not_linked() {
    // Unlinked: the guard returns before any network I/O, so this is offline-safe.
    let v: Value = serde_json::from_str(&drive_folders_json(&settings()).await).unwrap();
    assert_eq!(v["ok"], false);
    assert!(
        v["message"].as_str().unwrap().contains("Link Google Drive"),
        "expected a link-first hint, got {v}"
    );
}

#[test]
fn drive_save_sets_creds_and_folders_without_leaking_the_secret() {
    let s = settings();
    let body =
        br#"{"client_id":"cid.apps","client_secret":"gocspx-secret","folder_ids":"1AbC, 1XyZ"}"#;
    let out = drive_save_json(&s, body);
    // The status echo must never contain the secret.
    assert!(!out.contains("gocspx-secret"), "secret leaked: {out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["configured"], true, "client id + secret now set");
    assert_eq!(v["client_secret_set"], true);
    assert_eq!(v["linked"], false, "no refresh token yet");
    assert_eq!(v["folder_ids"][0], "1AbC");
    assert_eq!(v["folder_ids"][1], "1XyZ");
    // The live settings hold the real secret, but a status read never exposes it.
    let d = s.drive();
    assert_eq!(d.client_secret.as_deref(), Some("gocspx-secret"));
    assert!(!drive_status_json(&s).contains("gocspx-secret"));
}

#[test]
fn drive_save_blank_credential_leaves_the_stored_one_intact() {
    let s = settings();
    drive_save_json(&s, br#"{"client_id":"cid.apps","client_secret":"keep-me"}"#);
    // A later save with a blank secret (page reload) must not wipe it.
    drive_save_json(
        &s,
        br#"{"client_id":"cid.apps","client_secret":"","folder_ids":["1AbC"]}"#,
    );
    assert_eq!(s.drive().client_secret.as_deref(), Some("keep-me"));
    assert_eq!(s.drive().folder_ids, vec!["1AbC".to_string()]);
}

#[test]
fn spotify_status_reports_unconfigured_by_default() {
    let v: Value = serde_json::from_str(&spotify_status_json(&settings())).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["configured"], false);
    assert_eq!(v["linked"], false);
    assert_eq!(v["has_refresh_token"], false);
    // The device name defaults to the librespot device name.
    assert_eq!(v["device_name"], "Ambient");
}

#[test]
fn spotify_save_sets_creds_and_device_without_leaking_the_secret() {
    let s = settings();
    let body = br#"{"client_id":"spcid","client_secret":"sp-secret","device_name":"Kitchen"}"#;
    let out = spotify_save_json(&s, body);
    assert!(!out.contains("sp-secret"), "secret leaked: {out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["configured"], true, "client id + secret now set");
    assert_eq!(v["client_secret_set"], true);
    assert_eq!(v["linked"], false, "no refresh token yet");
    assert_eq!(v["device_name"], "Kitchen");
    // The live settings hold the real secret, but a status read never exposes it.
    assert_eq!(s.spotify().client_secret.as_deref(), Some("sp-secret"));
    assert!(!spotify_status_json(&s).contains("sp-secret"));
}

#[test]
fn spotify_save_blank_credential_leaves_the_stored_one_intact() {
    let s = settings();
    spotify_save_json(&s, br#"{"client_id":"spcid","client_secret":"keep-me"}"#);
    // A page reload posting a blank secret must not wipe the stored one.
    spotify_save_json(
        &s,
        br#"{"client_id":"spcid","client_secret":"","device_name":"Den"}"#,
    );
    assert_eq!(s.spotify().client_secret.as_deref(), Some("keep-me"));
    assert_eq!(s.spotify().device_label(), "Den");
}

#[test]
fn tools_save_sets_mapbox_token_without_leaking_it() {
    let s = settings();
    let before: Value = serde_json::from_str(&directions_status_json(&s)).unwrap();
    assert_eq!(before["token_set"], false);
    assert_eq!(before["tool_active"], false);
    let out = directions_save_json(&s, br#"{"mapbox_token":"pk.secret-token"}"#);
    assert!(!out.contains("pk.secret-token"), "token leaked: {out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["token_set"], true);
    assert_eq!(v["tool_active"], true);
    // The live settings hold the real token, but a status read never exposes it.
    assert!(!directions_status_json(&s).contains("pk.secret-token"));
    // A page reload posting a blank token must not wipe the stored one.
    directions_save_json(&s, br#"{"mapbox_token":""}"#);
    assert!(s.mapbox_token_set());
}

#[test]
fn weather_save_switches_provider_and_hides_the_key() {
    let s = settings();
    let before: Value = serde_json::from_str(&weather_status_json(&s)).unwrap();
    assert_eq!(before["provider"], "visualcrossing");
    assert_eq!(before["key_set"], false);

    // Set a Visual Crossing key — never echoed back.
    let out = weather_save_json(
        &s,
        br#"{"provider":"visualcrossing","visualcrossing_key":"vc-secret"}"#,
    );
    assert!(!out.contains("vc-secret"), "key leaked: {out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["provider"], "visualcrossing");
    assert_eq!(v["key_set"], true);
    assert!(s.visualcrossing_key_set());

    // Switch provider to Open-Meteo; the key is retained (blank field = keep).
    let v: Value =
        serde_json::from_str(&weather_save_json(&s, br#"{"provider":"openmeteo"}"#)).unwrap();
    assert_eq!(v["provider"], "openmeteo");
    assert!(s.visualcrossing_key_set());

    // A page reload posting a blank key must not wipe the stored one.
    weather_save_json(&s, br#"{"provider":"openmeteo","visualcrossing_key":""}"#);
    assert!(s.visualcrossing_key_set());

    // Explicit clear removes it.
    let v: Value = serde_json::from_str(&weather_save_json(
        &s,
        br#"{"provider":"visualcrossing","clear":true}"#,
    ))
    .unwrap();
    assert_eq!(v["key_set"], false);
    assert!(!s.visualcrossing_key_set());
}

#[test]
fn post_config_sets_anthropic_oauth_token_without_leaking_it() {
    let s = settings();
    assert!(!s.view().anthropic_oauth_token_set);
    let (status, _c, out) = route(
        "POST",
        "/config",
        br#"{"anthropic_oauth_token":"oauth-secret"}"#,
        &s,
    );
    assert_eq!(status, "200 OK");
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["anthropic_oauth_token_set"], true);
    // The token must never appear in a POST response or a later GET.
    assert!(!String::from_utf8_lossy(&out).contains("oauth-secret"));
    let (_s, _c, g) = route("GET", "/config", b"", &s);
    assert!(!String::from_utf8_lossy(&g).contains("oauth-secret"));
    assert!(s.view().anthropic_oauth_token_set);
}

#[tokio::test]
async fn spotify_link_without_credentials_is_a_clear_error() {
    // No client id/secret set → link returns a helpful message, never runs consent.
    let s = settings();
    let out = spotify_link_json(&s).await;
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["message"].as_str().unwrap().contains("client id"));
}

#[test]
fn cadora_status_reports_unlinked_by_default() {
    let v: Value = serde_json::from_str(&cadora_status_json(&settings())).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["linked"], false);
    // Falls back to the built-in Cadora server URL when unset.
    assert_eq!(v["base_url"], "https://cadora-server.fly.dev");
}

#[test]
fn cadora_save_sets_base_url() {
    let s = settings();
    cadora_save_json(&s, br#"{"base_url":"http://localhost:8080"}"#);
    assert_eq!(s.cadora().base_url_or_default(), "http://localhost:8080");
    // A blank base_url resets to the default.
    cadora_save_json(&s, br#"{"base_url":""}"#);
    assert_eq!(
        s.cadora().base_url_or_default(),
        "https://cadora-server.fly.dev"
    );
}

#[tokio::test]
async fn cadora_link_without_input_is_a_clear_error() {
    let s = settings();
    let out = cadora_link_json(&s, br#"{}"#).await;
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["message"].as_str().unwrap().contains("6-digit code"));
    assert!(!s.cadora().linked());
}

#[tokio::test]
async fn cadora_link_with_a_pasted_token_links_without_network() {
    // The primary path: the Cadora app mints a full `vl_…` token to copy/paste, so
    // linking stores it directly with no server round-trip.
    let s = settings();
    let out = cadora_link_json(&s, br#"{"token":"vl_pasted"}"#).await;
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["linked"], true);
    assert!(s.cadora().linked());
    // The token must never appear in the response.
    assert!(!out.contains("vl_pasted"));
}

#[test]
fn appsaid_status_reports_unconfigured_by_default() {
    let v: Value = serde_json::from_str(&appsaid_status_json(&settings())).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["token_set"], false);
    assert_eq!(v["tool_active"], false);
}

#[test]
fn appsaid_token_save_sets_it_without_leaking_it() {
    let s = settings();
    assert!(!s.appsaid_token_set());
    let out = appsaid_token_json(&s, br#"{"app_token":"appsaid-secret"}"#);
    assert!(s.appsaid_token_set());
    // The token must never appear in the status payload.
    assert!(!out.contains("appsaid-secret"));
    // A blank re-save keeps the stored token (a page reload never wipes it).
    appsaid_token_json(&s, br#"{"app_token":""}"#);
    assert!(s.appsaid_token_set());
}

#[test]
fn appsaid_save_sets_worker_url_and_default_recipient() {
    let s = settings();
    appsaid_save_json(
        &s,
        br#"{"worker_url":"https://appsaid.example.workers.dev/","default_recipient":"Mom"}"#,
    );
    let a = s.appsaid();
    // Trailing slash trimmed.
    assert_eq!(
        a.worker_url.as_deref(),
        Some("https://appsaid.example.workers.dev")
    );
    assert_eq!(a.default_recipient.as_deref(), Some("Mom"));
}

#[test]
fn invalid_json_is_a_400() {
    let (status, _c, _b) = route("POST", "/config", b"not json", &settings());
    assert_eq!(status, "400 Bad Request");
}

#[test]
fn household_page_and_status_are_served() {
    let s = settings();
    let (status, ctype, body) = route("GET", "/household", b"", &s);
    assert_eq!(status, "200 OK");
    assert!(ctype.starts_with("text/html"));
    assert!(String::from_utf8_lossy(&body).contains("Household"));

    let v: Value = serde_json::from_str(&household_status_json(&s)).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["members"].as_array().unwrap().len(), 0);
}

#[test]
fn household_save_round_trips_through_the_route() {
    let s = settings();
    let body = br#"{"address":{"address1":"  123 Main St ","address2":"","city":" Austin ",
          "state":"TX","postal":"78701","country":"USA"},"weather_units":"imperial",
        "members":[
          {"name":" Alice ","emails":["alice@example.com"," "],"phones":["+1 555 0001"],"appsaid":" gdksxryuunome6dp ","relationship":" parent "},
          {"name":"   ","emails":["ghost@example.com"]}
        ]}"#;
    let (status, ctype, out) = route("POST", "/household/save", body, &s);
    assert_eq!(status, "200 OK");
    assert_eq!(ctype, "application/json");
    let v: Value = serde_json::from_slice(&out).unwrap();
    // Address fields trimmed; blank address2 dropped to null.
    assert_eq!(v["address"]["address1"], "123 Main St");
    assert_eq!(v["address"]["address2"], Value::Null);
    assert_eq!(v["address"]["city"], "Austin");
    assert_eq!(v["address"]["state"], "TX");
    assert_eq!(v["address"]["postal"], "78701");
    assert_eq!(v["weather_units"], "imperial");
    let members = v["members"].as_array().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0]["name"], "Alice");
    assert_eq!(members[0]["emails"][0], "alice@example.com");
    assert_eq!(members[0]["emails"].as_array().unwrap().len(), 1);
    assert_eq!(members[0]["appsaid"], "gdksxryuunome6dp");
    assert_eq!(members[0]["relationship"], "parent");

    // It landed in the live settings, so the per-turn prompt snapshot sees it:
    // directions get the full combined address, weather gets City + State only.
    let h = s.household();
    assert_eq!(
        h.address.full_address().as_deref(),
        Some("123 Main St, Austin, TX 78701, USA")
    );
    assert_eq!(h.address.weather_location().as_deref(), Some("Austin, TX"));
    assert_eq!(h.members.len(), 1);
    assert_eq!(h.members[0].appsaid.as_deref(), Some("gdksxryuunome6dp"));
    // The member's AppSaid address makes them a send_phone_message recipient.
    assert_eq!(
        s.appsaid().recipients.get("Alice").map(String::as_str),
        Some("gdksxryuunome6dp")
    );
}

#[test]
fn pronunciations_save_round_trips_and_sanitizes_through_the_route() {
    let s = settings();
    // Blank canonical rule, a self/blank/duplicate variant, all exercised.
    let body = br#"{"rules":[
        {"canonical":"  Zehrs  ","variants":["zairs","  zairs  ","zay rz","Zehrs",""]},
        {"canonical":"","variants":["ignored"]},
        {"canonical":"Nope","variants":["   "]}
    ]}"#;
    let (status, ctype, out) = route("POST", "/pronunciations/save", body, &s);
    assert_eq!(status, "200 OK");
    assert_eq!(ctype, "application/json");
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["ok"], Value::Bool(true));
    let rules = v["rules"].as_array().unwrap();
    // Only the usable, sanitized rule survives.
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["canonical"], "Zehrs");
    let variants: Vec<&str> = rules[0]["variants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(variants, vec!["zairs", "zay rz"]);

    // It landed in the live settings, so the per-turn snapshot sees it.
    let live = s.pronunciations();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].canonical, "Zehrs");

    // GET status.json returns the same table.
    let (_s, _c, g) = route("GET", "/pronunciations/status.json", b"", &s);
    let gv: Value = serde_json::from_slice(&g).unwrap();
    assert_eq!(gv["rules"].as_array().unwrap().len(), 1);
    assert_eq!(gv["rules"][0]["canonical"], "Zehrs");
}

#[test]
fn pronunciations_save_rejects_invalid_json() {
    let s = settings();
    let (status, _c, out) = route("POST", "/pronunciations/save", b"not json", &s);
    assert_eq!(status, "200 OK");
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["ok"], Value::Bool(false));
    assert!(v["message"].as_str().unwrap().contains("invalid JSON"));
}

#[test]
fn normalize_target_reduces_absolute_form_to_path() {
    // Origin-form is unchanged.
    assert_eq!(normalize_target("/drive/save"), "/drive/save");
    assert_eq!(normalize_target("/config?_=1"), "/config?_=1");
    assert_eq!(normalize_target("/"), "/");
    // Absolute-form (proxied client) is reduced to origin-form.
    assert_eq!(
        normalize_target("http://127.0.0.1:8731/drive/save"),
        "/drive/save"
    );
    assert_eq!(
        normalize_target("https://host:8731/config?_=1"),
        "/config?_=1"
    );
    // Absolute URI with no path → root.
    assert_eq!(normalize_target("http://127.0.0.1:8731"), "/");
}

#[tokio::test]
async fn serves_a_proxied_absolute_form_post() {
    // A browser behind an HTTP proxy sends the absolute-form request target;
    // the server must route it the same as origin-form (regression: this used to
    // fall through to a 404 "not found", which broke the Drive config page).
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let s = settings();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        handle(stream, &services_with(s, None, debug()))
            .await
            .unwrap();
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    let body = br#"{"tts_voice":"en_US-amy-medium"}"#;
    // Absolute-form target, exactly as an HTTP proxy forwards it.
    let req = format!(
        "POST http://127.0.0.1:{port}/config HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len(),
        port = addr.port(),
    );
    client.write_all(req.as_bytes()).await.unwrap();
    client.write_all(body).await.unwrap();

    let mut resp = String::new();
    client.read_to_string(&mut resp).await.unwrap();
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "resp: {resp}");
    assert!(resp.contains("en_US-amy-medium"), "resp: {resp}");
}

#[test]
fn unknown_route_is_404() {
    let (status, ..) = route("GET", "/nope", b"", &settings());
    assert_eq!(status, "404 Not Found");
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
    let memory = MemoryStore::open_in_memory().unwrap();
    memory
        .add(
            MemoryKind::Fact,
            "The user likes tea",
            MemorySource::Explicit,
        )
        .unwrap();
    let v: Value = serde_json::from_str(&sqlite_json(&memory, &d)).unwrap();
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
    // memory_backend="sqlite" → tell the user how to turn HelixDB on.
    assert!(v["message"]
        .as_str()
        .unwrap()
        .contains("Set memory_backend"));
}

#[tokio::test]
async fn helix_json_explains_the_fallback_when_configured_for_helix() {
    // memory_backend="helix" but no graph → init failed and recall fell back.
    // The message must NOT tell the user to set a value that is already set;
    // it should point at the likely cause (OPENAI_API_KEY) and the log line.
    let mut d = debug();
    d.memory_backend = "helix".to_string();
    let v: Value = serde_json::from_str(&helix_json(&d).await).unwrap();
    assert_eq!(v["enabled"], false);
    let msg = v["message"].as_str().unwrap();
    assert!(msg.contains("fell back to SQLite FTS"), "got: {msg}");
    assert!(msg.contains("OPENAI_API_KEY"), "got: {msg}");
    assert!(!msg.contains("Set memory_backend"), "got: {msg}");
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
        handle(stream, &services_with(s, None, debug()))
            .await
            .unwrap();
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    let body = br#"{"tts_voice":"en_US-amy-medium"}"#;
    let req = format!(
        "POST /config HTTP/1.1\r\nHost: localhost:{}\r\nOrigin: http://localhost:{}\r\n\
         Content-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
        addr.port(),
        addr.port(),
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
        handle(stream, &services_with(s, None, debug()))
            .await
            .unwrap();
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(
            format!(
                "GET /models HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                addr.port()
            )
            .as_bytes(),
        )
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
        handle(stream, &services_with(s, Some(dir_for_task), debug()))
            .await
            .unwrap();
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(
            format!(
                "GET /voices HTTP/1.1\r\nHost: [::1]:{}\r\n\r\n",
                addr.port()
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    let mut resp = String::new();
    client.read_to_string(&mut resp).await.unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert!(resp.starts_with("HTTP/1.1 200 OK"), "resp: {resp}");
    assert!(resp.contains("en_US-amy-medium"), "resp: {resp}");
    assert!(!resp.contains("en_US-lessac-medium"), "resp: {resp}");
}

/// Serve exactly one raw request (built from the bound port) through `handle` and
/// return the raw response plus the settings it ran against.
async fn one_request(build: impl FnOnce(u16) -> String) -> (String, Arc<SharedSettings>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let s = settings();
    let server_settings = s.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = handle(stream, &services_with(server_settings, None, debug())).await;
    });
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(build(addr.port()).as_bytes())
        .await
        .unwrap();
    let mut resp = String::new();
    client.read_to_string(&mut resp).await.unwrap();
    (resp, s)
}

fn post_voice(port: u16, host: &str, extra: &str) -> String {
    let body = r#"{"tts_voice":"en_US-amy-medium"}"#;
    let host = host.replace("{port}", &port.to_string());
    format!(
        "POST /config HTTP/1.1\r\nHost: {host}\r\n{extra}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn rejects_a_non_loopback_host_with_403() {
    // DNS rebinding: the browser sends the attacker's hostname as Host.
    let (resp, s) = one_request(|p| {
        post_voice(
            p,
            "evil.example:{port}",
            "Content-Type: application/json\r\n",
        )
    })
    .await;
    assert!(resp.starts_with("HTTP/1.1 403 Forbidden"), "resp: {resp}");
    assert_eq!(s.view().tts_voice, None, "the handler never ran");

    // Right host, wrong port (another local service's name for us) — also refused.
    let (resp, _) =
        one_request(|_| "GET /config HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n".to_string()).await;
    assert!(resp.starts_with("HTTP/1.1 403 Forbidden"), "resp: {resp}");

    // No Host at all — refused.
    let (resp, _) = one_request(|_| "GET /config HTTP/1.1\r\n\r\n".to_string()).await;
    assert!(resp.starts_with("HTTP/1.1 403 Forbidden"), "resp: {resp}");
}

#[tokio::test]
async fn rejects_a_cross_origin_post_with_403() {
    let (resp, s) = one_request(|p| {
        post_voice(
            p,
            "127.0.0.1:{port}",
            "Origin: https://evil.example\r\nContent-Type: application/json\r\n",
        )
    })
    .await;
    assert!(resp.starts_with("HTTP/1.1 403 Forbidden"), "resp: {resp}");
    assert_eq!(s.view().tts_voice, None, "the handler never ran");

    // An opaque `null` origin (sandboxed iframe / file://) is not same-origin either.
    let (resp, _) = one_request(|p| {
        post_voice(
            p,
            "127.0.0.1:{port}",
            "Origin: null\r\nContent-Type: application/json\r\n",
        )
    })
    .await;
    assert!(resp.starts_with("HTTP/1.1 403 Forbidden"), "resp: {resp}");
}

#[tokio::test]
async fn rejects_a_post_without_a_json_content_type() {
    // A cross-site HTML form can only send form/text content types.
    let (resp, s) = one_request(|p| {
        post_voice(
            p,
            "127.0.0.1:{port}",
            "Content-Type: application/x-www-form-urlencoded\r\n",
        )
    })
    .await;
    assert!(
        resp.starts_with("HTTP/1.1 415 Unsupported Media Type"),
        "resp: {resp}"
    );
    assert_eq!(s.view().tts_voice, None, "the handler never ran");

    // Missing entirely — also refused.
    let (resp, _) = one_request(|p| post_voice(p, "127.0.0.1:{port}", "")).await;
    assert!(resp.starts_with("HTTP/1.1 415"), "resp: {resp}");
}

#[tokio::test]
async fn accepts_a_same_origin_loopback_post() {
    let (resp, s) = one_request(|p| {
        post_voice(
            p,
            "127.0.0.1:{port}",
            &format!("Origin: http://127.0.0.1:{p}\r\nContent-Type: application/json\r\n"),
        )
    })
    .await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "resp: {resp}");
    assert_eq!(s.view().tts_voice.as_deref(), Some("en_US-amy-medium"));
}

#[test]
fn check_request_accepts_only_loopback_authorities() {
    let h = |host: &str| vec![("host".to_string(), host.to_string())];
    assert!(check_request("GET", &h("127.0.0.1:8730"), 8730).is_ok());
    assert!(check_request("GET", &h("LOCALHOST:8730"), 8730).is_ok());
    assert!(check_request("GET", &h("[::1]:8730"), 8730).is_ok());
    assert!(check_request("GET", &h("127.0.0.1"), 8730).is_err());
    assert!(check_request("GET", &h("192.168.1.5:8730"), 8730).is_err());
    assert!(check_request("GET", &h("localhost.evil.example:8730"), 8730).is_err());
}

/// Every POST the admin pages make goes through `postJSON` (which sends the JSON
/// content type the server now requires); no page keeps a private `apiJSON` copy or
/// a raw `fetch(..., {method:'POST'})`.
#[test]
fn admin_pages_post_only_through_the_shared_json_helper() {
    assert!(SHELL_SCRIPT.contains("async function postJSON"));
    assert!(SHELL_SCRIPT.contains("'Content-Type':'application/json'"));
    for (name, body) in [
        ("ui.js", UI_SCRIPT),
        ("overview", OVERVIEW_BODY),
        ("voice", VOICE_BODY),
        ("intelligence", INTELLIGENCE_BODY),
        ("personality", PERSONALITY_BODY),
        ("household", HOUSEHOLD_BODY),
        ("integrations", INTEGRATIONS_BODY),
        ("devices", DEVICES_BODY),
        ("memory", MEMORY_BODY),
        ("conversations", CONVERSATIONS_BODY),
        ("keys", KEYS_BODY),
        ("system", SYSTEM_BODY),
    ] {
        assert!(
            !body.contains("function apiJSON"),
            "{name} redefines apiJSON"
        );
        assert!(!body.contains("method:'POST'"), "{name} has a raw POST");
        assert!(!body.contains("method: 'POST'"), "{name} has a raw POST");
    }
}

#[test]
fn web_and_device_settings_payloads_share_field_names() {
    // One view builder: the device response is the page payload (same keys).
    let s = settings();
    let page: Value = serde_json::from_str(&view_json(&s, true, Some("m"))).unwrap();
    let device = crate::control::respond(
        &crate::wyoming::protocol::WyomingEvent::new(
            crate::wyoming::protocol::types::DESCRIBE_SETTINGS,
        ),
        &MemoryStore::open_in_memory().unwrap(),
        &s,
        None,
    );
    let mut a: Vec<_> = page.as_object().unwrap().keys().cloned().collect();
    let mut b: Vec<_> = device.data.as_object().unwrap().keys().cloned().collect();
    a.sort();
    b.sort();
    assert_eq!(a, b);
    for key in [
        "ok",
        "message",
        "llm_backend",
        "llm_model",
        "anthropic_key_set",
        "openai_key_set",
        "anthropic_oauth_token_set",
        "anthropic_auth",
        "tts_voice",
        "engine",
        "web_search",
        "search_provider",
        "search_key_set",
        "end_silence_ms",
        "voice_rms_threshold",
        "silero_threshold",
        "vad_engine",
    ] {
        assert!(page.get(key).is_some(), "missing {key}");
    }
}

#[test]
fn post_config_blank_or_null_search_key_semantics() {
    let s = settings();
    route("POST", "/config", br#"{"search_api_key":"tvly-k"}"#, &s);
    // A blank key (page reload) leaves it; an explicit null clears it.
    route("POST", "/config", br#"{"search_api_key":""}"#, &s);
    assert!(s.view().search_key_set);
    route("POST", "/config", br#"{"search_api_key":null}"#, &s);
    assert!(!s.view().search_key_set);
}

// ---- Route inventory -------------------------------------------------------------

const HTML: &str = "text/html; charset=utf-8";
const JSON: &str = "application/json";
const TEXT: &str = "text/plain";

/// Every route the admin console serves, as `(method, path, body, status,
/// content type)` for one representative request (no network, no services). POSTs
/// are also listed with a malformed body to pin the invalid-JSON status. Requests
/// that are not routes (unknown path, wrong method, lower-case method) pin the 404.
const ROUTE_INVENTORY: &[(&str, &str, &str, &str, &str)] = &[
    // Static assets + pages.
    ("GET", "/", "", "200 OK", HTML),
    ("GET", "/index.html", "", "200 OK", HTML),
    ("GET", "/panel.css", "", "200 OK", "text/css; charset=utf-8"),
    (
        "GET",
        "/shell.js",
        "",
        "200 OK",
        "application/javascript; charset=utf-8",
    ),
    (
        "GET",
        "/ui.js",
        "",
        "200 OK",
        "application/javascript; charset=utf-8",
    ),
    ("GET", "/voice", "", "200 OK", HTML),
    ("GET", "/intelligence", "", "200 OK", HTML),
    ("GET", "/personality", "", "200 OK", HTML),
    ("GET", "/household", "", "200 OK", HTML),
    ("GET", "/integrations", "", "200 OK", HTML),
    ("GET", "/devices", "", "200 OK", HTML),
    ("GET", "/memory", "", "200 OK", HTML),
    ("GET", "/conversations", "", "200 OK", HTML),
    ("GET", "/keys", "", "200 OK", HTML),
    ("GET", "/system", "", "200 OK", HTML),
    // Legacy page routes → redirects.
    ("GET", "/stt", "", "302 Found", TEXT),
    ("GET", "/pronunciations", "", "302 Found", TEXT),
    ("GET", "/system1", "", "302 Found", TEXT),
    ("GET", "/tools", "", "302 Found", TEXT),
    ("GET", "/drive", "", "302 Found", TEXT),
    ("GET", "/music", "", "302 Found", TEXT),
    ("GET", "/notifications", "", "302 Found", TEXT),
    ("GET", "/embeddings", "", "302 Found", TEXT),
    ("GET", "/sqlite", "", "302 Found", TEXT),
    ("GET", "/helix", "", "302 Found", TEXT),
    ("GET", "/chatlog", "", "302 Found", TEXT),
    ("GET", "/prompts", "", "302 Found", TEXT),
    ("GET", "/about", "", "302 Found", TEXT),
    // JSON reads.
    ("GET", "/overview.json", "", "200 OK", JSON),
    ("GET", "/overview.json?probe=0", "", "200 OK", JSON),
    ("GET", "/keys.json", "", "200 OK", JSON),
    ("GET", "/system.json", "", "200 OK", JSON),
    ("GET", "/extraction/status.json", "", "200 OK", JSON),
    ("GET", "/about.json", "", "200 OK", JSON),
    ("GET", "/config", "", "200 OK", JSON),
    ("GET", "/config?_=1", "", "200 OK", JSON),
    ("GET", "/models", "", "200 OK", JSON),
    ("GET", "/voices", "", "200 OK", JSON),
    ("GET", "/household/status.json", "", "200 OK", JSON),
    ("GET", "/personality/status.json", "", "200 OK", JSON),
    ("GET", "/pronunciations/status.json", "", "200 OK", JSON),
    ("GET", "/drive/status.json", "", "200 OK", JSON),
    ("GET", "/drive/folders.json", "", "200 OK", JSON),
    ("GET", "/tools/status.json", "", "200 OK", JSON),
    ("GET", "/tools/weather/status.json", "", "200 OK", JSON),
    ("GET", "/tools/places/status.json", "", "200 OK", JSON),
    ("GET", "/stt/status.json", "", "200 OK", JSON),
    ("GET", "/system1/status.json", "", "200 OK", JSON),
    ("GET", "/embeddings/status.json", "", "200 OK", JSON),
    ("GET", "/notifications/status.json", "", "200 OK", JSON),
    ("GET", "/chatlog.json", "", "200 OK", JSON),
    ("GET", "/chatlog.json?limit=5", "", "200 OK", JSON),
    ("GET", "/prompts.json", "", "200 OK", JSON),
    ("GET", "/sqlite.json", "", "200 OK", JSON),
    ("GET", "/helix.json", "", "200 OK", JSON),
    ("GET", "/music/status.json", "", "200 OK", JSON),
    ("GET", "/spotify/status.json", "", "200 OK", JSON),
    ("GET", "/cadora/status.json", "", "200 OK", JSON),
    ("GET", "/appsaid/status.json", "", "200 OK", JSON),
    // Mutations, each with a well-formed body …
    ("POST", "/config", r#"{"tts_voice":"amy"}"#, "200 OK", JSON),
    ("POST", "/household/save", "{}", "200 OK", JSON),
    ("POST", "/personality/save", "{}", "200 OK", JSON),
    ("POST", "/pronunciations/save", "{}", "200 OK", JSON),
    ("POST", "/tools/save", "{}", "200 OK", JSON),
    ("POST", "/tools/weather/save", "{}", "200 OK", JSON),
    ("POST", "/tools/places/save", "{}", "200 OK", JSON),
    ("POST", "/stt/save", "{}", "200 OK", JSON),
    ("POST", "/system1/save", "{}", "200 OK", JSON),
    (
        "POST",
        "/embeddings/switch",
        r#"{"backend":"local"}"#,
        "200 OK",
        JSON,
    ),
    ("POST", "/notifications/test", "{}", "200 OK", JSON),
    (
        "POST",
        "/extraction/save",
        r#"{"enabled":true}"#,
        "200 OK",
        JSON,
    ),
    (
        "POST",
        "/helix/rename-entity",
        r#"{"old_name":"a","new_name":"b"}"#,
        "200 OK",
        JSON,
    ),
    (
        "POST",
        "/music/proc",
        r#"{"proc":"snapserver","action":"start"}"#,
        "200 OK",
        JSON,
    ),
    (
        "POST",
        "/music/play",
        r#"{"url":"http://x"}"#,
        "200 OK",
        JSON,
    ),
    ("POST", "/music/stopweb", "{}", "200 OK", JSON),
    ("POST", "/music/startall", "{}", "200 OK", JSON),
    ("POST", "/music/stopall", "{}", "200 OK", JSON),
    ("POST", "/drive/save", "{}", "200 OK", JSON),
    ("POST", "/drive/link", "{}", "200 OK", JSON),
    ("POST", "/spotify/save", "{}", "200 OK", JSON),
    ("POST", "/spotify/link", "{}", "200 OK", JSON),
    ("POST", "/cadora/save", "{}", "200 OK", JSON),
    ("POST", "/cadora/link", "{}", "200 OK", JSON),
    ("POST", "/appsaid/save", "{}", "200 OK", JSON),
    ("POST", "/appsaid/token", "{}", "200 OK", JSON),
    // … and with a malformed one (only /config answers 400; the page handlers
    // report the parse error in-band with a 200).
    ("POST", "/config", "not json", "400 Bad Request", JSON),
    ("POST", "/household/save", "not json", "200 OK", JSON),
    ("POST", "/personality/save", "not json", "200 OK", JSON),
    ("POST", "/pronunciations/save", "not json", "200 OK", JSON),
    ("POST", "/tools/save", "not json", "200 OK", JSON),
    ("POST", "/tools/weather/save", "not json", "200 OK", JSON),
    ("POST", "/tools/places/save", "not json", "200 OK", JSON),
    ("POST", "/stt/save", "not json", "200 OK", JSON),
    ("POST", "/system1/save", "not json", "200 OK", JSON),
    ("POST", "/embeddings/switch", "not json", "200 OK", JSON),
    ("POST", "/notifications/test", "not json", "200 OK", JSON),
    ("POST", "/extraction/save", "not json", "200 OK", JSON),
    ("POST", "/helix/rename-entity", "not json", "200 OK", JSON),
    ("POST", "/music/proc", "not json", "200 OK", JSON),
    ("POST", "/music/play", "not json", "200 OK", JSON),
    ("POST", "/drive/save", "not json", "200 OK", JSON),
    ("POST", "/spotify/save", "not json", "200 OK", JSON),
    ("POST", "/cadora/save", "not json", "200 OK", JSON),
    ("POST", "/cadora/link", "not json", "200 OK", JSON),
    ("POST", "/appsaid/save", "not json", "200 OK", JSON),
    ("POST", "/appsaid/token", "not json", "200 OK", JSON),
    // Not routes.
    ("GET", "/nope", "", "404 Not Found", TEXT),
    ("GET", "/household/save", "", "404 Not Found", TEXT),
    ("GET", "/music/stopall", "", "404 Not Found", TEXT),
    ("POST", "/household", "{}", "404 Not Found", TEXT),
    ("POST", "/models", "{}", "404 Not Found", TEXT),
    ("POST", "/chatlog.json", "{}", "404 Not Found", TEXT),
    ("PUT", "/config", "{}", "404 Not Found", TEXT),
    ("get", "/", "", "404 Not Found", TEXT),
];

/// Serve one raw request through the real connection handler (fresh services each
/// time) and split the response into `(status, content type, body)`.
async fn serve_raw(method: &str, path: &str, body: &str) -> (String, String, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut dbg = debug();
        dbg.chatlog_path = std::env::temp_dir().join("wc_inventory_missing_chatlog.jsonl");
        dbg.promptlog_path = std::env::temp_dir().join("wc_inventory_missing_prompts.jsonl");
        let _ = handle(stream, &services_with(settings(), None, dbg)).await;
    });
    let ctype = if method.eq_ignore_ascii_case("POST") {
        "Content-Type: application/json\r\n"
    } else {
        ""
    };
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{ctype}\
         Content-Length: {len}\r\n\r\n{body}",
        port = addr.port(),
        len = body.len(),
    );
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(req.as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    client.read_to_end(&mut resp).await.unwrap();
    let split = find(&resp, b"\r\n\r\n").expect("a complete response head");
    let head = String::from_utf8_lossy(&resp[..split]).to_string();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .strip_prefix("HTTP/1.1 ")
        .unwrap()
        .to_string();
    let ctype = lines
        .find_map(|l| l.strip_prefix("Content-Type: "))
        .unwrap_or_default()
        .to_string();
    (status, ctype, resp[split + 4..].to_vec())
}

/// Pins the status + content type of every route. When `WEBCONFIG_ROUTE_SNAPSHOT`
/// names a file, the full responses are also written there, so a refactor of the
/// router can be diffed byte-for-byte against the previous build.
#[tokio::test]
async fn every_route_keeps_its_status_and_content_type() {
    let mut snapshot = String::new();
    for &(method, path, body, status, ctype) in ROUTE_INVENTORY {
        let (got_status, got_ctype, got_body) = serve_raw(method, path, body).await;
        assert_eq!(got_status, status, "{method} {path} {body:?}");
        assert_eq!(got_ctype, ctype, "{method} {path} {body:?}");
        snapshot.push_str(&format!(
            "=== {method} {path} {body}\n{got_status} | {got_ctype}\n{}\n",
            String::from_utf8_lossy(&got_body)
        ));
    }
    if let Ok(out) = std::env::var("WEBCONFIG_ROUTE_SNAPSHOT") {
        std::fs::write(out, snapshot).unwrap();
    }
}

/// The route table and the inventory above agree exactly: every table entry is
/// unique and pinned by a non-404 inventory row, and every inventory row is a route
/// iff it expects something other than a 404.
#[test]
fn route_table_matches_the_inventory() {
    let mut seen = std::collections::HashSet::new();
    for r in routes::ROUTES {
        assert!(
            seen.insert((r.method, r.path)),
            "duplicate route {} {}",
            r.method,
            r.path
        );
        let pinned = ROUTE_INVENTORY.iter().any(|&(m, p, _, status, _)| {
            m == r.method && p.split('?').next() == Some(r.path) && status != "404 Not Found"
        });
        assert!(
            pinned,
            "{} {} is missing from ROUTE_INVENTORY",
            r.method, r.path
        );
    }
    for &(method, path, _, status, _) in ROUTE_INVENTORY {
        let path = path.split('?').next().unwrap();
        assert_eq!(
            routes::lookup(method, path).is_some(),
            status != "404 Not Found",
            "{method} {path}"
        );
    }
}

#[test]
fn tri_state_string_helpers() {
    let data = json!({ "set": "  v ", "blank": "  ", "num": 3 });
    assert_eq!(str_field(&data, "set").as_deref(), Some("v"));
    assert_eq!(str_field(&data, "blank"), None);
    assert_eq!(str_field(&data, "num"), None);
    assert_eq!(keep_or_set(&data, "set"), Some(Some("v".to_string())));
    assert_eq!(keep_or_set(&data, "blank"), None);
    assert_eq!(keep_or_set(&data, "absent"), None);
    assert_eq!(set_or_clear(&data, "set"), Some(Some("v".to_string())));
    assert_eq!(set_or_clear(&data, "blank"), Some(None));
    assert_eq!(set_or_clear(&data, "absent"), None);
    assert_eq!(secret_update(&data, "set"), Some(Some("v".to_string())));
    let clear = json!({ "set": "v", "clear": true });
    assert_eq!(secret_update(&clear, "set"), Some(None));
}
