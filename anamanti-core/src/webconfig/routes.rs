//! The config page's **one route table**: every `(method, path)` it serves and the
//! handler for it. The request guard has already run (see `handle` in the parent
//! module) by the time a request is dispatched here; anything not in the table is a
//! `404`. Paths match exactly (the query string is stripped first) and methods match
//! case-sensitively.

use std::future::Future;
use std::pin::Pin;

use super::debug::{
    chatlog_json, helix_json, helix_rename_json, limit_param, prompts_json, sqlite_json,
};
use super::drive::{drive_folders_json, drive_link_json, drive_save_json, drive_status_json};
use super::general::{
    about_json, config_save, models_json, notifications_status_json, notifications_test_json,
    voices_json,
};
use super::home_assistant::{ha_save_json, ha_status_json, ha_test_json, ha_token_json};
use super::household::{
    appsaid_save_json, appsaid_status_json, appsaid_token_json, cadora_link_json, cadora_save_json,
    cadora_status_json, household_save_json, household_status_json, personality_save_json,
    personality_status_json, pronunciations_save_json, pronunciations_status_json,
};
use super::language::{language_save_json, language_status_json};
use super::memory::{embeddings_status_json, embeddings_switch_json};
use super::music::{
    music_display_json, music_display_save_json, music_nowplaying_json, music_play_json,
    music_proc_json, music_startall_json, music_status_json, music_stopall_json,
    music_stopweb_json, music_transport_json, music_volume_json, radio_lookup_json,
    radio_play_json, radio_save_json, radio_stations_json, radio_stop_json, spotify_link_json,
    spotify_save_json, spotify_status_json,
};
use super::speakers::{
    conversations_attribute_async, memory_attribute_async, memory_delete_async, speakers_json,
    SpeakerDirectory,
};
use super::system::{
    extraction_save_json, extraction_status_json, keys_json, overview_json, system_json,
};
use super::tools::{
    directions_save_json, directions_status_json, places_save_json, places_status_json,
    stt_save_json, stt_status_json, system1_save_json, system1_status_json, weather_save_json,
    weather_status_json,
};
use super::{
    view_json, with_json, Reply, Request, CONVERSATIONS_BODY, DEVICES_BODY, HOUSEHOLD_BODY,
    INTEGRATIONS_BODY, INTELLIGENCE_BODY, KEYS_BODY, MEMORY_BODY, OVERVIEW_BODY, PANEL_CSS,
    PERSONALITY_BODY, SHELL_SCRIPT, SYSTEM_BODY, UI_SCRIPT, VOICE_BODY,
};
use crate::app::AppServices;

pub(super) type BoxReply<'a> = Pin<Box<dyn Future<Output = Reply> + Send + 'a>>;

/// How a route produces its reply.
pub(super) enum Handler {
    /// Answered from in-memory state, no I/O.
    Sync(fn(&AppServices, &Request<'_>) -> Reply),
    /// Needs I/O (a provider call, the graph store, a process, an OAuth consent, …).
    Async(for<'a> fn(&'a AppServices, &'a Request<'a>) -> BoxReply<'a>),
}

pub(super) struct Route {
    pub method: &'static str,
    pub path: &'static str,
    pub handler: Handler,
}

const fn get(path: &'static str, handler: Handler) -> Route {
    Route {
        method: "GET",
        path,
        handler,
    }
}

const fn post(path: &'static str, handler: Handler) -> Route {
    Route {
        method: "POST",
        path,
        handler,
    }
}

use Handler::{Async, Sync};

/// Every route the config page serves.
pub(super) static ROUTES: &[Route] = &[
    // Shared static assets, served from the same server (reused by every page via
    // <link>/<script> in the shell) instead of being inlined into each response.
    get(
        "/panel.css",
        Sync(|_, _| Reply::asset("text/css; charset=utf-8", PANEL_CSS)),
    ),
    get(
        "/shell.js",
        Sync(|_, _| Reply::asset("application/javascript; charset=utf-8", SHELL_SCRIPT)),
    ),
    get(
        "/ui.js",
        Sync(|_, _| Reply::asset("application/javascript; charset=utf-8", UI_SCRIPT)),
    ),
    // Pages (information architecture: plans/CoreWebUIPlan.md §4).
    get("/", Sync(|_, _| Reply::page("Overview", OVERVIEW_BODY))),
    get(
        "/index.html",
        Sync(|_, _| Reply::page("Overview", OVERVIEW_BODY)),
    ),
    get("/voice", Sync(|_, _| Reply::page("Voice", VOICE_BODY))),
    get(
        "/intelligence",
        Sync(|_, _| Reply::page("Intelligence", INTELLIGENCE_BODY)),
    ),
    get(
        "/personality",
        Sync(|_, _| Reply::page("Personality", PERSONALITY_BODY)),
    ),
    get(
        "/household",
        Sync(|_, _| Reply::page("Household", HOUSEHOLD_BODY)),
    ),
    get(
        "/integrations",
        Sync(|_, _| Reply::page("Integrations", INTEGRATIONS_BODY)),
    ),
    get(
        "/devices",
        Sync(|_, _| Reply::page("Music &amp; Devices", DEVICES_BODY)),
    ),
    get("/memory", Sync(|_, _| Reply::page("Memory", MEMORY_BODY))),
    get(
        "/conversations",
        Sync(|_, _| Reply::page("Conversations", CONVERSATIONS_BODY)),
    ),
    get(
        "/keys",
        Sync(|_, _| Reply::page("Keys &amp; Accounts", KEYS_BODY)),
    ),
    get("/system", Sync(|_, _| Reply::page("System", SYSTEM_BODY))),
    // Legacy page routes → their new homes (bookmarks + older docs keep working).
    get("/stt", Sync(|_, _| Reply::redirect("/voice#stt"))),
    get(
        "/pronunciations",
        Sync(|_, _| Reply::redirect("/voice#fixes")),
    ),
    get(
        "/system1",
        Sync(|_, _| Reply::redirect("/intelligence#system1")),
    ),
    get("/tools", Sync(|_, _| Reply::redirect("/integrations"))),
    get(
        "/drive",
        Sync(|_, _| Reply::redirect("/integrations#drive")),
    ),
    get("/music", Sync(|_, _| Reply::redirect("/devices#music"))),
    get(
        "/notifications",
        Sync(|_, _| Reply::redirect("/devices#displays")),
    ),
    get(
        "/embeddings",
        Sync(|_, _| Reply::redirect("/memory#settings")),
    ),
    get("/sqlite", Sync(|_, _| Reply::redirect("/memory#facts"))),
    get("/helix", Sync(|_, _| Reply::redirect("/memory#graph"))),
    get("/chatlog", Sync(|_, _| Reply::redirect("/conversations"))),
    get("/prompts", Sync(|_, _| Reply::redirect("/conversations"))),
    get("/about", Sync(|_, _| Reply::redirect("/system"))),
    // Console-wide aggregators (read-only) + the entity-extraction toggle.
    get(
        "/overview.json",
        Async(|s, r| {
            Box::pin(async move {
                let probe = !r.target.contains("probe=0");
                Reply::json(overview_json(s, probe).await)
            })
        }),
    ),
    get("/keys.json", Sync(|s, _| Reply::json(keys_json(s)))),
    get(
        "/system.json",
        Sync(|s, _| Reply::json(system_json(&s.debug))),
    ),
    get(
        "/extraction/status.json",
        Sync(|s, _| Reply::json(extraction_status_json(s))),
    ),
    post(
        "/extraction/save",
        Sync(|s, r| with_json(r, |d| extraction_save_json(s, d))),
    ),
    get("/about.json", Sync(|_, _| Reply::json(about_json()))),
    // Config tab: the live settings, the model + voice dropdowns.
    get(
        "/config",
        Sync(|s, _| Reply::json(view_json(s.settings(), true, None))),
    ),
    post("/config", Sync(|s, r| config_save(s.settings(), r))),
    get(
        "/models",
        Async(|s, _| Box::pin(async move { Reply::json(models_json(&s.catalog).await) })),
    ),
    get(
        "/voices",
        Async(|s, _| Box::pin(async move { Reply::json(voices_json(s).await) })),
    ),
    // Household tab (+ the Cadora and AppSaid links), Personality, Pronunciation.
    get(
        "/household/status.json",
        Sync(|s, _| Reply::json(household_status_json(s.settings()))),
    ),
    post(
        "/household/save",
        Sync(|s, r| with_json(r, |d| household_save_json(s.settings(), d))),
    ),
    get(
        "/personality/status.json",
        Sync(|s, _| Reply::json(personality_status_json(s.settings()))),
    ),
    post(
        "/personality/save",
        Sync(|s, r| with_json(r, |d| personality_save_json(s.settings(), d))),
    ),
    get(
        "/pronunciations/status.json",
        Sync(|s, _| Reply::json(pronunciations_status_json(s.settings()))),
    ),
    // Voice → Languages (plans/BilingualPlan.md §2.11).
    get(
        "/language/status.json",
        Sync(|s, _| Reply::json(language_status_json(s.settings()))),
    ),
    post(
        "/language/save",
        Sync(|s, r| with_json(r, |d| language_save_json(s.settings(), d))),
    ),
    post(
        "/pronunciations/save",
        Sync(|s, r| with_json(r, |d| pronunciations_save_json(s.settings(), d))),
    ),
    get(
        "/cadora/status.json",
        Sync(|s, _| Reply::json(cadora_status_json(s.settings()))),
    ),
    post(
        "/cadora/save",
        Sync(|s, r| with_json(r, |d| cadora_save_json(s.settings(), d))),
    ),
    // Redeems a pairing code against the Cadora server (network).
    post(
        "/cadora/link",
        Async(|s, r| {
            Box::pin(async move { Reply::json(cadora_link_json(s.settings(), r.body).await) })
        }),
    ),
    get(
        "/appsaid/status.json",
        Sync(|s, _| Reply::json(appsaid_status_json(s.settings()))),
    ),
    post(
        "/appsaid/save",
        Sync(|s, r| with_json(r, |d| appsaid_save_json(s.settings(), d))),
    ),
    post(
        "/appsaid/token",
        Sync(|s, r| with_json(r, |d| appsaid_token_json(s.settings(), d))),
    ),
    // Home Assistant (plans/HomeAssistantPlan.md).
    get(
        "/home-assistant/status.json",
        Sync(|s, _| Reply::json(ha_status_json(s.settings()))),
    ),
    post(
        "/home-assistant/save",
        Sync(|s, r| with_json(r, |d| ha_save_json(s.settings(), d))),
    ),
    post(
        "/home-assistant/token",
        Sync(|s, r| with_json(r, |d| ha_token_json(s.settings(), d))),
    ),
    post(
        "/home-assistant/test",
        Async(|s, _| Box::pin(async move { Reply::json(ha_test_json(s.settings()).await) })),
    ),
    // Tools tab (directions / weather / places credentials), Speech, System-1.
    get(
        "/tools/status.json",
        Sync(|s, _| Reply::json(directions_status_json(s.settings()))),
    ),
    post(
        "/tools/save",
        Sync(|s, r| with_json(r, |d| directions_save_json(s.settings(), d))),
    ),
    get(
        "/tools/weather/status.json",
        Sync(|s, _| Reply::json(weather_status_json(s.settings()))),
    ),
    post(
        "/tools/weather/save",
        Sync(|s, r| with_json(r, |d| weather_save_json(s.settings(), d))),
    ),
    get(
        "/tools/places/status.json",
        Sync(|s, _| Reply::json(places_status_json(s.settings()))),
    ),
    post(
        "/tools/places/save",
        Sync(|s, r| with_json(r, |d| places_save_json(s.settings(), d))),
    ),
    get(
        "/stt/status.json",
        Sync(|s, _| Reply::json(stt_status_json(s.settings()))),
    ),
    post(
        "/stt/save",
        Sync(|s, r| with_json(r, |d| stt_save_json(s.settings(), d))),
    ),
    get(
        "/system1/status.json",
        Sync(|s, _| Reply::json(system1_status_json(s.settings()))),
    ),
    post(
        "/system1/save",
        Sync(|s, r| with_json(r, |d| system1_save_json(s.settings(), d))),
    ),
    // Photos tab (Google Drive).
    get(
        "/drive/status.json",
        Sync(|s, _| Reply::json(drive_status_json(s.settings()))),
    ),
    post(
        "/drive/save",
        Sync(|s, r| with_json(r, |d| drive_save_json(s.settings(), d))),
    ),
    get(
        "/drive/folders.json",
        Async(|s, _| Box::pin(async move { Reply::json(drive_folders_json(s.settings()).await) })),
    ),
    post(
        "/drive/link",
        Async(|s, _| Box::pin(async move { Reply::json(drive_link_json(s.settings()).await) })),
    ),
    // Music tab (process supervisor, Snapcast, mpv) + Spotify.
    get(
        "/music/status.json",
        Async(|s, _| {
            Box::pin(async move { Reply::json(music_status_json(s.music.as_ref()).await) })
        }),
    ),
    post(
        "/music/proc",
        Async(|s, r| {
            Box::pin(async move { Reply::json(music_proc_json(s.music.as_ref(), r.body).await) })
        }),
    ),
    post(
        "/music/play",
        Async(|s, r| {
            Box::pin(async move { Reply::json(music_play_json(s.music.as_ref(), r.body).await) })
        }),
    ),
    post(
        "/music/stopweb",
        Async(|s, _| {
            Box::pin(async move { Reply::json(music_stopweb_json(s.music.as_ref()).await) })
        }),
    ),
    post(
        "/music/volume",
        Async(|s, r| {
            Box::pin(async move { Reply::json(music_volume_json(s.music.as_ref(), r.body).await) })
        }),
    ),
    post(
        "/music/startall",
        Async(|s, _| {
            Box::pin(async move { Reply::json(music_startall_json(s.music.as_ref()).await) })
        }),
    ),
    post(
        "/music/stopall",
        Async(|s, _| {
            Box::pin(async move { Reply::json(music_stopall_json(s.music.as_ref()).await) })
        }),
    ),
    get(
        "/radio/stations.json",
        Sync(|s, _| Reply::json(radio_stations_json(s.settings()))),
    ),
    post(
        "/radio/save",
        Sync(|s, r| with_json(r, |d| radio_save_json(s.settings(), d))),
    ),
    post(
        "/radio/lookup",
        Async(|_, r| Box::pin(async move { Reply::json(radio_lookup_json(r.body).await) })),
    ),
    post(
        "/radio/play",
        Async(|s, r| {
            Box::pin(async move {
                Reply::json(radio_play_json(s.settings(), &s.now_playing, r.body).await)
            })
        }),
    ),
    post(
        "/radio/stop",
        Async(|s, _| Box::pin(async move { Reply::json(radio_stop_json(s.settings()).await) })),
    ),
    get(
        "/music/nowplaying.json",
        Async(|s, _| {
            Box::pin(async move {
                Reply::json(music_nowplaying_json(s.settings(), &s.now_playing).await)
            })
        }),
    ),
    get(
        "/music/display.json",
        Sync(|s, _| Reply::json(music_display_json(s.settings()))),
    ),
    post(
        "/music/display",
        Sync(|s, r| {
            with_json(r, |d| {
                music_display_save_json(s.settings(), &s.now_playing, d)
            })
        }),
    ),
    post(
        "/music/transport",
        Async(|s, r| {
            Box::pin(async move {
                Reply::json(music_transport_json(s.settings(), &s.now_playing, r.body).await)
            })
        }),
    ),
    get(
        "/spotify/status.json",
        Sync(|s, _| Reply::json(spotify_status_json(s.settings()))),
    ),
    post(
        "/spotify/save",
        Sync(|s, r| with_json(r, |d| spotify_save_json(s.settings(), d))),
    ),
    post(
        "/spotify/link",
        Async(|s, _| Box::pin(async move { Reply::json(spotify_link_json(s.settings()).await) })),
    ),
    // Memory → Embeddings (live GraphRAG backend hot-swap).
    get(
        "/embeddings/status.json",
        Async(|s, _| {
            Box::pin(async move { Reply::json(embeddings_status_json(s.graphrag.as_ref()).await) })
        }),
    ),
    post(
        "/embeddings/switch",
        Async(|s, r| {
            Box::pin(async move {
                Reply::json(embeddings_switch_json(s.graphrag.as_ref(), r.body).await)
            })
        }),
    ),
    // Notify tab (Approach A, visual-only).
    get(
        "/notifications/status.json",
        Sync(|s, _| Reply::json(notifications_status_json(&s.notify))),
    ),
    post(
        "/notifications/test",
        Sync(|s, r| Reply::json(notifications_test_json(&s.notify, r.body))),
    ),
    // Debug views (read-only) + the one debug mutation.
    get(
        "/chatlog.json",
        Sync(|s, r| {
            let dir = SpeakerDirectory::build(s);
            Reply::json(chatlog_json(&s.debug, &dir, limit_param(r.target)))
        }),
    ),
    get(
        "/prompts.json",
        Sync(|s, r| {
            let dir = SpeakerDirectory::build(s);
            Reply::json(prompts_json(&s.debug, &dir, limit_param(r.target)))
        }),
    ),
    get(
        "/sqlite.json",
        Sync(|s, _| {
            let dir = SpeakerDirectory::build(s);
            Reply::json(sqlite_json(s.memory(), &s.debug, &dir))
        }),
    ),
    get(
        "/helix.json",
        Async(|s, _| {
            Box::pin(async move {
                let dir = SpeakerDirectory::build(s);
                Reply::json(helix_json(&s.debug, Some(&dir)).await)
            })
        }),
    ),
    // Speaker tagging + the memory review / turn correction actions
    // (SpeakerScopedMemoryPlan.md §8).
    get("/speakers.json", Sync(|s, _| Reply::json(speakers_json(s)))),
    post(
        "/memory/attribute",
        Async(|s, r| {
            Box::pin(async move {
                Reply::json(match super::parse_json_body(r.body) {
                    Ok(data) => memory_attribute_async(s, &data).await,
                    Err(invalid) => invalid,
                })
            })
        }),
    ),
    post(
        "/memory/delete",
        Async(|s, r| {
            Box::pin(async move {
                Reply::json(match super::parse_json_body(r.body) {
                    Ok(data) => memory_delete_async(s, &data).await,
                    Err(invalid) => invalid,
                })
            })
        }),
    ),
    post(
        "/conversations/attribute",
        Async(|s, r| {
            Box::pin(async move {
                Reply::json(match super::parse_json_body(r.body) {
                    Ok(data) => conversations_attribute_async(s, &data).await,
                    Err(invalid) => invalid,
                })
            })
        }),
    ),
    post(
        "/helix/rename-entity",
        Async(|s, r| {
            Box::pin(async move { Reply::json(helix_rename_json(&s.debug, r.body).await) })
        }),
    ),
];

/// The handler for `method` + `path`, if it is a route.
pub(super) fn lookup(method: &str, path: &str) -> Option<&'static Handler> {
    ROUTES
        .iter()
        .find(|r| r.method == method && r.path == path)
        .map(|r| &r.handler)
}

/// Route one (already guarded) request to its handler; `404` when it is not a route.
pub(super) async fn dispatch(services: &AppServices, method: &str, req: &Request<'_>) -> Reply {
    match lookup(method, req.path) {
        Some(Sync(handler)) => handler(services, req),
        Some(Async(handler)) => handler(services, req).await,
        None => Reply::not_found(),
    }
}
