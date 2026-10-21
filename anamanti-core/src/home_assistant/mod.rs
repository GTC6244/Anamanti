//! Home Assistant integration (`plans/HomeAssistantPlan.md`, Part A).
//!
//! A [`HomeAssistant`] hub holds the connection to the household's HA instance and a
//! cached copy of its controllable devices ([`catalog::Catalog`]). It serves two
//! callers with the same resolver and the same direct REST calls:
//!
//! - the **fast path** (`orchestrator::home`), which runs before System-1 / memory
//!   recall / the LLM: [`matcher::parse`] recognizes a command's shape with pure string
//!   code, [`catalog::resolve`] finds the devices, and one `POST /api/services/…` call
//!   (~25 ms on the LAN) does it — no model involved; and
//! - the **`home_control` LLM tool**, the fallback for anything the fast path can't
//!   match with certainty.
//!
//! HA's own Assist / `conversation/process` is deliberately **not** used: on the
//! household's modest HA host it costs 0.5–0.7 s per new phrase and misreads some of
//! them (see the plan's §0 revision note).
//!
//! The long-lived access token is a secret: env `HOME_ASSISTANT_TOKEN` only, sent as a
//! bearer header (never a query parameter), and every HTTP error goes through
//! `crate::http`'s redaction.

pub mod catalog;
pub mod matcher;
pub mod pt;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub use catalog::{Catalog, Entity, Target};
pub use matcher::{HomeCommand, Verb};

/// `User-Agent` component tag for HA requests.
const COMPONENT: &str = "home_assistant";
/// Deadline for a service call (turn on/off, scene, brightness).
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// Deadline for a background / status catalog fetch.
const CATALOG_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long a turn may wait for a catalog when none is cached yet (cold start).
const COLD_FETCH_BUDGET: Duration = Duration::from_millis(800);
/// After a failed cold fetch, turns don't wait on HA again for this long — so an HA
/// outage never taxes ordinary turns that happen to look like commands.
const COLD_RETRY_BACKOFF: Duration = Duration::from_secs(30);
/// A cached catalog older than this is still served, but triggers a background refresh.
const CATALOG_TTL: Duration = Duration::from_secs(60);

/// Spoken when HA can't be reached or rejects a call.
pub const UNREACHABLE: &str = "Sorry, I couldn't reach Home Assistant.";

/// The Jinja template rendered by `POST /api/template`: every controllable entity as a
/// JSON array of `{id, name, area, state}`. One call, ~40 ms warm.
const CATALOG_TEMPLATE: &str = "[{%- for s in states if s.domain in \
['light','switch','scene','script','media_player','remote'] -%}\
{\"id\":{{ s.entity_id|tojson }},\"name\":{{ s.name|tojson }},\
\"area\":{{ (area_name(s.entity_id) or '')|tojson }},\"state\":{{ s.state|tojson }}}\
{{ ',' if not loop.last }}{%- endfor -%}]";

/// The `home_assistant` block of `anamanti.json`, resolved. The token is not here — it
/// is a secret, read from `HOME_ASSISTANT_TOKEN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeAssistantConfig {
    /// HA base URL, no trailing slash (e.g. `http://homeassistant.local:8123`).
    pub base_url: Option<String>,
    /// The room a display is in when it doesn't report one (so a bare "lights" means
    /// that room's lights).
    pub default_room: Option<String>,
    /// Name/id fragments of devices that must be named exactly — never swept up by a
    /// room-wide or "all" command. Default `["fireplace"]`.
    pub exact_only: Vec<String>,
    /// The dashboard the display widget opens (Part B), e.g. `lovelace/0`.
    pub dashboard_path: String,
}

impl Default for HomeAssistantConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            default_room: None,
            exact_only: vec!["fireplace".to_string()],
            dashboard_path: "lovelace/0".to_string(),
        }
    }
}

/// The REST surface the hub needs — a seam so tests run without a network.
#[async_trait]
pub trait HaApi: Send + Sync {
    /// Fetch the controllable-entity catalog.
    async fn fetch_catalog(&self) -> Result<Catalog>;
    /// Call `domain.service` with `data` (e.g. `homeassistant.turn_on`).
    async fn call_service(&self, domain: &str, service: &str, data: Value) -> Result<()>;
}

/// The live HA REST client.
pub struct RestApi {
    base_url: String,
    token: String,
}

impl std::fmt::Debug for RestApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the token.
        f.debug_struct("RestApi")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl RestApi {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        }
    }
}

#[async_trait]
impl HaApi for RestApi {
    async fn fetch_catalog(&self) -> Result<Catalog> {
        let req = crate::http::post(format!("{}/api/template", self.base_url), COMPONENT)
            .bearer_auth(&self.token)
            .json(&json!({ "template": CATALOG_TEMPLATE }));
        let text = crate::http::send_text(req, CATALOG_TIMEOUT, "Home Assistant catalog").await?;
        Catalog::from_json(&text).context("parsing the Home Assistant catalog")
    }

    async fn call_service(&self, domain: &str, service: &str, data: Value) -> Result<()> {
        let url = format!("{}/api/services/{domain}/{service}", self.base_url);
        let req = crate::http::post(url, COMPONENT)
            .bearer_auth(&self.token)
            .json(&data);
        crate::http::send_ok(req, CALL_TIMEOUT, "Home Assistant service call").await?;
        Ok(())
    }
}

/// What a command will do, decided from the catalog before any call is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Turn a device or group on/off (scenes/scripts in it are activated on "on").
    Switch { on: bool, target: Target },
    /// Run one scene or script.
    Activate { entity: Entity },
    /// Set the brightness of the target's lights.
    Brightness { pct: u8, target: Target },
    /// Report whether the target is on.
    Status { target: Target },
    /// The named device(s) can't be reached by HA right now — say so, call nothing.
    Unavailable { target: Target },
}

/// Decide what `cmd` means against `catalog`. `None` = not a command this house can
/// carry out unambiguously → the caller falls through to System-1 / the LLM.
pub fn plan(
    catalog: &Catalog,
    cmd: &HomeCommand,
    room: Option<&str>,
    exact_only: &[String],
) -> Option<Plan> {
    let found = catalog::resolve(catalog, &cmd.target, room, exact_only);
    match cmd.verb {
        Verb::Status => Some(Plan::Status { target: found? }),
        Verb::Activate => {
            let target = found?;
            match target.entities.as_slice() {
                [e] if matches!(e.domain(), "scene" | "script") => {
                    Some(Plan::Activate { entity: e.clone() })
                }
                _ => None,
            }
        }
        Verb::Brightness(pct) => {
            let target = found?;
            let lights: Vec<Entity> = target
                .entities
                .iter()
                .filter(|e| e.domain() == "light" && e.available())
                .cloned()
                .collect();
            if lights.is_empty() {
                return None;
            }
            Some(Plan::Brightness {
                pct,
                target: Target {
                    entities: lights,
                    ..target
                },
            })
        }
        Verb::On | Verb::Off => {
            let on = cmd.verb == Verb::On;
            let Some(target) = found else {
                // "turn off main floor lights" → the scene "Main Floor lights off".
                let wanted = format!("{} {}", cmd.target, if on { "on" } else { "off" });
                // Exact scene name only — never a loose match ("lights off" must not run
                // "Main Floor lights off" just because it ends that way).
                let k = catalog::key(&wanted);
                let scenes: Vec<&Entity> = catalog
                    .entities
                    .iter()
                    .filter(|e| e.domain() == "scene" && catalog::key(&e.name) == k)
                    .collect();
                return match scenes.as_slice() {
                    [e] => Some(Plan::Activate {
                        entity: (*e).clone(),
                    }),
                    _ => None,
                };
            };
            // A scene/script has no "off".
            if !on
                && target
                    .entities
                    .iter()
                    .all(|e| matches!(e.domain(), "scene" | "script"))
            {
                return None;
            }
            if let [e] = target.entities.as_slice() {
                if matches!(e.domain(), "scene" | "script") {
                    return Some(Plan::Activate { entity: e.clone() });
                }
            }
            let reachable: Vec<Entity> = target
                .entities
                .iter()
                .filter(|e| e.available())
                .cloned()
                .collect();
            if reachable.is_empty() {
                return Some(Plan::Unavailable { target });
            }
            Some(Plan::Switch {
                on,
                target: Target {
                    entities: reachable,
                    ..target
                },
            })
        }
    }
}

/// The Home Assistant hub: the connection plus the cached device catalog. One instance
/// is built at boot and shared by the fast path and the `home_control` tool.
pub struct HomeAssistant {
    /// The live connection, swapped in place by [`Self::configure`] when the base URL
    /// or token changes on the config page (so the pipeline and the tool, which share
    /// this hub, both follow without a restart).
    conn: RwLock<Conn>,
    default_room: RwLock<Option<String>>,
    exact_only: Vec<String>,
    cache: RwLock<Option<(Instant, Arc<Catalog>)>>,
    /// Bumped on every reconnect; a refresh started under an older generation never
    /// stores its (old-server) catalog.
    generation: AtomicU64,
    refreshing: AtomicBool,
    cold_failed_at: Mutex<Option<Instant>>,
}

/// The current connection: the `(base_url, token)` it was built from, and the client.
#[derive(Default)]
struct Conn {
    key: Option<(String, String)>,
    api: Option<Arc<dyn HaApi>>,
}

impl std::fmt::Debug for HomeAssistant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HomeAssistant")
            .field("configured", &self.is_configured())
            .field("default_room", &self.default_room())
            .field("exact_only", &self.exact_only)
            .finish_non_exhaustive()
    }
}

/// Build the (initially unconnected) hub from the boot config. The connection — base
/// URL, token, default room — comes from the live settings via [`HomeAssistant::configure`]
/// (seeded from `anamanti.json` + `HOME_ASSISTANT_TOKEN`, overridden by the config page).
pub fn from_config(cfg: &HomeAssistantConfig) -> Arc<HomeAssistant> {
    Arc::new(HomeAssistant::unconfigured(cfg.exact_only.clone()))
}

/// Trim and drop blank values.
fn non_blank(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

impl HomeAssistant {
    /// A hub already connected through `api` (tests, and anything that injects a client).
    pub fn new(api: Arc<dyn HaApi>, default_room: Option<String>, exact_only: Vec<String>) -> Self {
        let hub = Self::unconfigured(exact_only);
        *hub.conn.write().unwrap() = Conn {
            key: Some((String::new(), String::new())),
            api: Some(api),
        };
        *hub.default_room.write().unwrap() = default_room.filter(|r| !r.trim().is_empty());
        hub
    }

    /// A hub with no connection yet: the fast path and the tool stay off until
    /// [`Self::configure`] supplies a base URL + token.
    pub fn unconfigured(exact_only: Vec<String>) -> Self {
        Self {
            conn: RwLock::new(Conn::default()),
            default_room: RwLock::new(None),
            exact_only,
            cache: RwLock::new(None),
            generation: AtomicU64::new(0),
            refreshing: AtomicBool::new(false),
            cold_failed_at: Mutex::new(None),
        }
    }

    /// Point the hub at `base_url` with `token` (both required; either blank ⇒ off) and
    /// set the default room. Idempotent: an unchanged URL + token keeps the connection
    /// and its warm catalog; a change drops the cache and, inside a runtime, re-warms it.
    pub fn configure(
        self: &Arc<Self>,
        base_url: Option<&str>,
        token: Option<&str>,
        default_room: Option<&str>,
    ) {
        *self.default_room.write().unwrap() = non_blank(default_room).map(str::to_string);
        let key = match (non_blank(base_url), non_blank(token)) {
            (Some(u), Some(t)) => Some((u.trim_end_matches('/').to_string(), t.to_string())),
            _ => None,
        };
        {
            let mut conn = self.conn.write().unwrap();
            if conn.key == key {
                return;
            }
            conn.api = key
                .as_ref()
                .map(|(u, t)| Arc::new(RestApi::new(u.clone(), t.clone())) as Arc<dyn HaApi>);
            conn.key = key;
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
        *self.cache.write().unwrap() = None;
        *self.cold_failed_at.lock().unwrap() = None;
        if self.is_configured() {
            log::info!("home assistant: connection configured");
            if tokio::runtime::Handle::try_current().is_ok() {
                self.spawn_refresh();
            }
        } else {
            log::info!("home assistant: not configured (needs a base URL and a token)");
        }
    }

    /// Whether a base URL + token are set (the fast path and tool are live).
    pub fn is_configured(&self) -> bool {
        self.conn.read().unwrap().api.is_some()
    }

    fn api(&self) -> Option<Arc<dyn HaApi>> {
        self.conn.read().unwrap().api.clone()
    }

    /// The configured fallback room for displays that don't report one.
    pub fn default_room(&self) -> Option<String> {
        self.default_room.read().unwrap().clone()
    }

    /// Fetch the catalog now and cache it (unless the connection changed meanwhile).
    pub async fn refresh(&self) -> Result<Arc<Catalog>> {
        let generation = self.generation.load(Ordering::Acquire);
        let api = self.api().context("Home Assistant isn't configured")?;
        let catalog = Arc::new(api.fetch_catalog().await?);
        if self.generation.load(Ordering::Acquire) == generation {
            *self.cache.write().unwrap() = Some((Instant::now(), catalog.clone()));
        }
        Ok(catalog)
    }

    /// Warm the catalog in the background (call at boot so the first command is fast).
    pub fn prewarm(self: &Arc<Self>) {
        if self.is_configured() {
            self.spawn_refresh();
        }
    }

    fn spawn_refresh(self: &Arc<Self>) {
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return; // one in flight already
        }
        let me = self.clone();
        tokio::spawn(async move {
            match me.refresh().await {
                Ok(c) => log::debug!(
                    "home assistant: catalog refreshed ({} entities)",
                    c.entities.len()
                ),
                Err(e) => log::warn!("home assistant: catalog refresh failed: {e:#}"),
            }
            me.refreshing.store(false, Ordering::Release);
        });
    }

    /// Fetch the catalog now and report how it went — the config page's "Test
    /// connection". Never touches a device.
    pub async fn test_connection(&self) -> Result<(usize, usize, u128)> {
        let t0 = Instant::now();
        let catalog = tokio::time::timeout(CATALOG_TIMEOUT * 2, self.refresh())
            .await
            .context("Home Assistant didn't answer in time")??;
        Ok((
            catalog.entities.len(),
            catalog.areas().len(),
            t0.elapsed().as_millis(),
        ))
    }

    /// The catalog for a turn: the cache if present (refreshed in the background once
    /// stale), else a bounded cold fetch — skipped during a backoff after a failure, so
    /// an HA outage never slows unrelated turns.
    pub async fn catalog(self: &Arc<Self>) -> Option<Arc<Catalog>> {
        if !self.is_configured() {
            return None;
        }
        let cached = self.cache.read().unwrap().clone();
        if let Some((at, catalog)) = cached {
            if at.elapsed() > CATALOG_TTL {
                self.spawn_refresh();
            }
            return Some(catalog);
        }
        if let Some(at) = *self.cold_failed_at.lock().unwrap() {
            if at.elapsed() < COLD_RETRY_BACKOFF {
                return None;
            }
        }
        match tokio::time::timeout(COLD_FETCH_BUDGET, self.refresh()).await {
            Ok(Ok(catalog)) => Some(catalog),
            other => {
                if let Ok(Err(e)) = other {
                    log::warn!("home assistant: cold catalog fetch failed: {e:#}");
                } else {
                    log::warn!("home assistant: cold catalog fetch timed out");
                    // Let it finish in the background so the next turn is warm.
                    self.spawn_refresh();
                }
                *self.cold_failed_at.lock().unwrap() = Some(Instant::now());
                None
            }
        }
    }

    /// The fast path: `Some(spoken reply)` when `transcript` is a home command this
    /// house can carry out (or tried to — an HA error is spoken, not passed to the LLM);
    /// `None` to fall through to System-1 / the LLM.
    pub async fn handle_utterance(
        self: &Arc<Self>,
        transcript: &str,
        room: Option<&str>,
    ) -> Option<String> {
        let cmd = matcher::parse(transcript)?;
        self.run(&cmd, room).await
    }

    /// Plan and execute `cmd`; `None` when it doesn't resolve unambiguously.
    pub async fn run(self: &Arc<Self>, cmd: &HomeCommand, room: Option<&str>) -> Option<String> {
        let catalog = self.catalog().await?;
        let fallback = self.default_room();
        let room = room
            .filter(|r| !r.trim().is_empty())
            .or(fallback.as_deref());
        let plan = plan(&catalog, cmd, room, &self.exact_only)?;
        Some(self.execute(plan).await)
    }

    /// Carry out a [`Plan`] and return the spoken confirmation (or [`UNREACHABLE`]).
    pub async fn execute(self: &Arc<Self>, plan: Plan) -> String {
        match self.perform(&plan).await {
            Ok(done) => say_en(&done),
            Err(e) => {
                log::warn!("home assistant: {plan:?} failed: {e:#}");
                UNREACHABLE.to_string()
            }
        }
    }

    /// The Brazilian-Portuguese fast path (`plans/BilingualPlan.md` §2.6): parse with
    /// [`pt::parse`], try each English-shaped target candidate against the catalog, and
    /// act only when exactly one distinct target resolves (several rooms matching
    /// different devices is ambiguous → fall through, never guess). The reply is spoken
    /// in Portuguese using the user's own target phrase.
    pub async fn handle_utterance_pt(
        self: &Arc<Self>,
        transcript: &str,
        room: Option<&str>,
    ) -> Option<String> {
        let cmd = pt::parse(transcript)?;
        let catalog = self.catalog().await?;
        let fallback = self.default_room();
        let room = room
            .filter(|r| !r.trim().is_empty())
            .or(fallback.as_deref());
        let mut chosen: Option<Plan> = None;
        for target in &cmd.candidates {
            let hc = HomeCommand {
                verb: cmd.verb,
                target: target.clone(),
            };
            let Some(found) = plan(&catalog, &hc, room, &self.exact_only) else {
                continue;
            };
            match &chosen {
                None => chosen = Some(found),
                Some(prev) if plan_entities(prev) == plan_entities(&found) => {}
                Some(_) => {
                    log::info!(
                        "home fast path (pt): {transcript:?} matches several targets; falling through"
                    );
                    return None;
                }
            }
        }
        let plan = chosen?;
        Some(match self.perform(&plan).await {
            Ok(done) => pt::say(&done, &cmd),
            Err(e) => {
                log::warn!("home assistant: {plan:?} failed: {e:#}");
                pt::UNREACHABLE.to_string()
            }
        })
    }

    /// Run the service call(s) a [`Plan`] needs and report what happened (rendered into
    /// words by [`say_en`] / [`pt::say`]).
    async fn perform(self: &Arc<Self>, plan: &Plan) -> Result<Done> {
        let api = self.api().context("Home Assistant isn't configured")?;
        match plan {
            Plan::Switch { on, target } => {
                let ids: Vec<&str> = target
                    .entities
                    .iter()
                    .map(|e| e.entity_id.as_str())
                    .collect();
                let service = if *on { "turn_on" } else { "turn_off" };
                api.call_service("homeassistant", service, json!({ "entity_id": ids }))
                    .await?;
                self.note_state(&ids, if *on { "on" } else { "off" });
                Ok(Done::Switched {
                    on: *on,
                    target: target.clone(),
                })
            }
            Plan::Activate { entity } => {
                let domain = entity.domain();
                api.call_service(domain, "turn_on", json!({ "entity_id": entity.entity_id }))
                    .await?;
                Ok(Done::Activated {
                    entity: entity.clone(),
                })
            }
            Plan::Brightness { pct, target } => {
                let ids: Vec<&str> = target
                    .entities
                    .iter()
                    .map(|e| e.entity_id.as_str())
                    .collect();
                api.call_service(
                    "light",
                    "turn_on",
                    json!({ "entity_id": ids, "brightness_pct": pct }),
                )
                .await?;
                self.note_state(&ids, if *pct == 0 { "off" } else { "on" });
                Ok(Done::Brightness {
                    pct: *pct,
                    target: target.clone(),
                })
            }
            Plan::Status { target } => {
                // A status answer must be current: refetch (bounded), else use the cache.
                let fresh = tokio::time::timeout(CATALOG_TIMEOUT, self.refresh())
                    .await
                    .ok()
                    .and_then(Result::ok);
                let now: Vec<Entity> = target
                    .entities
                    .iter()
                    .map(|e| {
                        fresh
                            .as_ref()
                            .and_then(|c| c.entities.iter().find(|f| f.entity_id == e.entity_id))
                            .unwrap_or(e)
                            .clone()
                    })
                    .collect();
                Ok(Done::Status {
                    target: target.clone(),
                    now,
                })
            }
            Plan::Unavailable { target } => Ok(Done::Unavailable {
                target: target.clone(),
            }),
        }
    }

    /// Optimistically record a new state in the cache so a quick follow-up status
    /// question doesn't read the pre-command value.
    fn note_state(&self, ids: &[&str], state: &str) {
        let mut guard = self.cache.write().unwrap();
        if let Some((at, catalog)) = guard.as_ref() {
            let mut updated = (**catalog).clone();
            for e in &mut updated.entities {
                if ids.contains(&e.entity_id.as_str()) && e.available() {
                    e.state = state.to_string();
                }
            }
            *guard = Some((*at, Arc::new(updated)));
        }
    }

    /// A short spoken inventory for the LLM tool: rooms and named devices with states.
    pub async fn describe(self: &Arc<Self>) -> Option<String> {
        let catalog = self.catalog().await?;
        let mut lines = Vec::new();
        let mut areas = catalog.areas();
        areas.push(String::new());
        for area in areas {
            let items: Vec<String> = catalog
                .entities
                .iter()
                .filter(|e| e.area == area)
                .map(|e| format!("{} [{}] ({})", e.name, e.domain(), e.state))
                .collect();
            if !items.is_empty() {
                let room = if area.is_empty() { "No room" } else { &area };
                lines.push(format!("{room}: {}", items.join(", ")));
            }
        }
        Some(lines.join("\n"))
    }
}

/// What a performed [`Plan`] did — the input to the per-language reply renderers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    Switched { on: bool, target: Target },
    Activated { entity: Entity },
    Brightness { pct: u8, target: Target },
    Status { target: Target, now: Vec<Entity> },
    Unavailable { target: Target },
}

/// The entity ids a plan touches (to tell two candidate targets apart).
fn plan_entities(plan: &Plan) -> Vec<String> {
    let mut ids: Vec<String> = match plan {
        Plan::Switch { target, .. }
        | Plan::Brightness { target, .. }
        | Plan::Status { target }
        | Plan::Unavailable { target } => target
            .entities
            .iter()
            .map(|e| e.entity_id.clone())
            .collect(),
        Plan::Activate { entity } => vec![entity.entity_id.clone()],
    };
    ids.sort();
    ids
}

/// The English confirmation for a performed plan (the pre-bilingual strings, verbatim).
fn say_en(done: &Done) -> String {
    match done {
        Done::Switched { on, target } => {
            format!("{} {}.", target.label, if *on { "on" } else { "off" })
        }
        Done::Activated { entity } => {
            if entity.domain() == "script" {
                format!("Running {}.", entity.name)
            } else {
                format!("Okay, {}.", entity.name)
            }
        }
        Done::Brightness { pct, target } => format!("{} at {pct} percent.", target.label),
        Done::Status { target, now } => status_sentence(target, now),
        Done::Unavailable { target } => unavailable_sentence(target),
    }
}

/// "is" or "are" for a spoken label ("Porch lights are", "Samsung TV is").
fn be(label: &str) -> &'static str {
    match label.split_whitespace().last() {
        Some(w) if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") => "are",
        _ => "is",
    }
}

/// "<label> isn't/aren't responding right now."
fn unavailable_sentence(target: &Target) -> String {
    let verb = if target.group {
        "are"
    } else {
        be(&target.label)
    };
    format!("{} {verb}n't responding right now.", target.label)
}

/// Speak a status answer for `target` from the current entity states.
fn status_sentence(target: &Target, now: &[Entity]) -> String {
    let reachable: Vec<&Entity> = now.iter().filter(|e| e.available()).collect();
    let is_on = |e: &Entity| !matches!(e.state.as_str(), "off" | "idle" | "standby");
    if reachable.is_empty() {
        return unavailable_sentence(target);
    }
    if !target.group {
        // One named device (or a same-named pair in one room, e.g. a TV and its remote).
        let state = match reachable.as_slice() {
            [e] => e.state.replace('_', " "),
            _ if reachable.iter().any(|e| is_on(e)) => "on".to_string(),
            _ => "off".to_string(),
        };
        return format!("{} {} {state}.", target.label, be(&target.label));
    }
    let on = reachable.iter().filter(|e| is_on(e)).count();
    let total = reachable.len();
    let label = target.label.to_lowercase();
    if on == 0 {
        format!("{} are off.", target.label)
    } else if on == total {
        format!("{} are on.", target.label)
    } else {
        format!("{on} of {total} {label} are on.")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use catalog::tests::house;

    /// An offline [`HaApi`] that serves a fixed catalog and records service calls.
    #[derive(Default)]
    pub(crate) struct FakeApi {
        pub catalog: Catalog,
        pub fail: bool,
        pub calls: Mutex<Vec<(String, String, Value)>>,
    }

    #[async_trait]
    impl HaApi for FakeApi {
        async fn fetch_catalog(&self) -> Result<Catalog> {
            if self.fail {
                anyhow::bail!("down");
            }
            Ok(self.catalog.clone())
        }
        async fn call_service(&self, domain: &str, service: &str, data: Value) -> Result<()> {
            if self.fail {
                anyhow::bail!("down");
            }
            self.calls
                .lock()
                .unwrap()
                .push((domain.to_string(), service.to_string(), data));
            Ok(())
        }
    }

    pub(crate) fn hub_with(api: Arc<FakeApi>, room: Option<&str>) -> Arc<HomeAssistant> {
        Arc::new(HomeAssistant::new(
            api,
            room.map(str::to_string),
            vec!["fireplace".into()],
        ))
    }

    fn fake() -> Arc<FakeApi> {
        Arc::new(FakeApi {
            catalog: house(),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn room_command_turns_the_room_off_in_one_call() {
        let api = fake();
        let hub = hub_with(api.clone(), None);
        let said = hub
            .handle_utterance("turn off the office lights", None)
            .await;
        assert_eq!(said.as_deref(), Some("Office lights off."));
        let calls = api.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            (calls[0].0.as_str(), calls[0].1.as_str()),
            ("homeassistant", "turn_off")
        );
        assert_eq!(
            calls[0].2,
            json!({"entity_id": ["light.lights_office", "light.pot_lights_office", "switch.lights_office"]})
        );
    }

    #[tokio::test]
    async fn bare_lights_use_the_display_room_then_the_default_room() {
        let api = fake();
        let hub = hub_with(api.clone(), Some("Dining"));
        assert_eq!(
            hub.handle_utterance("lights off", Some("Office"))
                .await
                .as_deref(),
            Some("Office lights off.")
        );
        assert_eq!(
            hub.handle_utterance("lights off", None).await.as_deref(),
            Some("Dining lights off.")
        );
        let none = hub_with(fake(), None);
        assert_eq!(none.handle_utterance("lights off", None).await, None);
    }

    #[tokio::test]
    async fn all_lights_skips_the_fireplace_and_unavailable_devices() {
        let api = fake();
        let hub = hub_with(api.clone(), None);
        hub.handle_utterance("turn on all the lights", None)
            .await
            .unwrap();
        let calls = api.calls.lock().unwrap();
        let ids = calls[0].2["entity_id"].as_array().unwrap();
        assert!(ids
            .iter()
            .all(|v| !v.as_str().unwrap().contains("fireplace")));
        assert!(!ids.contains(&json!("light.bathroom_light"))); // unavailable
    }

    #[tokio::test]
    async fn scenes_scripts_and_brightness() {
        let api = fake();
        let hub = hub_with(api.clone(), None);
        assert_eq!(
            hub.handle_utterance("activate media room tv on", None)
                .await
                .as_deref(),
            Some("Okay, Media Room TV On.")
        );
        assert_eq!(
            hub.handle_utterance("turn off main floor lights", None)
                .await
                .as_deref(),
            Some("Okay, Main Floor lights off.")
        );
        assert_eq!(
            hub.handle_utterance("dim the office lights to 30 percent", None)
                .await
                .as_deref(),
            Some("Office lights at 30 percent.")
        );
        let calls = api.calls.lock().unwrap();
        assert_eq!(calls[0].0, "scene");
        // Brightness goes only to real lights (not the office light *switch*).
        assert_eq!(
            calls[2].2,
            json!({"entity_id": ["light.lights_office", "light.pot_lights_office"], "brightness_pct": 30})
        );
    }

    #[tokio::test]
    async fn status_questions_and_unavailable_devices() {
        let hub = hub_with(fake(), None);
        assert_eq!(
            hub.handle_utterance("is the samsung tv on", None)
                .await
                .as_deref(),
            Some("Samsung TV is off.")
        );
        assert_eq!(
            hub.handle_utterance("is the porch light on", None)
                .await
                .as_deref(),
            Some("Porch lights are off.")
        );
        assert_eq!(
            hub.handle_utterance("are the office lights on", None)
                .await
                .as_deref(),
            Some("2 of 3 office lights are on.")
        );
        assert_eq!(
            hub.handle_utterance("turn on the bedroom lights", None)
                .await
                .as_deref(),
            Some("Bedroom Lights aren't responding right now.")
        );
    }

    #[tokio::test]
    async fn non_commands_and_unknown_targets_fall_through() {
        let api = fake();
        let hub = hub_with(api.clone(), None);
        for s in [
            "what's the weather",
            "start a timer for ten minutes",
            "turn on the garage",
            "turn off the pot lights",
            // No room known: must not loosely match the "Main Floor lights off" scene.
            "lights off",
        ] {
            assert_eq!(hub.handle_utterance(s, None).await, None, "{s:?}");
        }
        assert!(api.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_ha_failure_is_spoken_and_backs_off() {
        let api = Arc::new(FakeApi {
            catalog: house(),
            fail: true,
            ..Default::default()
        });
        let hub = hub_with(api, None);
        // No catalog can be fetched → fall through (never blocks again during backoff).
        assert_eq!(hub.handle_utterance("porch lights off", None).await, None);
        assert!(hub.cold_failed_at.lock().unwrap().is_some());

        // With a warm cache but a failing call, the failure is spoken.
        let flaky = Arc::new(FakeApi {
            catalog: house(),
            fail: true,
            ..Default::default()
        });
        let hub = hub_with(flaky, None);
        *hub.cache.write().unwrap() = Some((Instant::now(), Arc::new(house())));
        assert_eq!(
            hub.handle_utterance("porch lights off", None)
                .await
                .as_deref(),
            Some(UNREACHABLE)
        );
    }

    #[tokio::test]
    async fn configure_needs_url_and_token_and_swaps_live() {
        let hub = from_config(&HomeAssistantConfig::default());
        assert!(!hub.is_configured());
        // Unconfigured: the fast path is silent (falls through), no I/O attempted.
        assert_eq!(hub.handle_utterance("porch lights off", None).await, None);
        hub.configure(Some("http://ha.local:8123/"), None, Some("Kitchen"));
        assert!(!hub.is_configured());
        hub.configure(Some("http://ha.local:8123/"), Some("  "), None);
        assert!(!hub.is_configured());
        hub.configure(Some("http://ha.local:8123/"), Some("t"), Some("Kitchen"));
        assert!(hub.is_configured());
        assert_eq!(hub.default_room().as_deref(), Some("Kitchen"));
        // A cached catalog survives an unchanged reconfigure but not a new token.
        *hub.cache.write().unwrap() = Some((Instant::now(), Arc::new(house())));
        hub.configure(Some("http://ha.local:8123"), Some("t"), Some("Office"));
        assert!(hub.cache.read().unwrap().is_some());
        assert_eq!(hub.default_room().as_deref(), Some("Office"));
        hub.configure(Some("http://ha.local:8123"), Some("t2"), None);
        assert!(hub.cache.read().unwrap().is_none());
        hub.configure(None, Some("t2"), None);
        assert!(!hub.is_configured());
    }

    /// A **read-only dry run against the live Home Assistant**: fetches the real catalog
    /// and prints what each phrase would do, without calling any service. Useful while
    /// relabelling devices/rooms in HA. Ignored by default; run with
    ///
    /// ```text
    /// HOME_ASSISTANT_URL=http://homeassistant.local:8123 HOME_ASSISTANT_TOKEN=… \
    ///   HA_ROOM=Kitchen HA_PHRASES="turn off the office lights;lights on" \
    ///   cargo test --lib live_dry_run -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "needs a live Home Assistant (HOME_ASSISTANT_URL + HOME_ASSISTANT_TOKEN)"]
    async fn live_dry_run() {
        let url = std::env::var("HOME_ASSISTANT_URL").expect("HOME_ASSISTANT_URL");
        let token = std::env::var("HOME_ASSISTANT_TOKEN").expect("HOME_ASSISTANT_TOKEN");
        let room = std::env::var("HA_ROOM").ok();
        let api = RestApi::new(url, token);
        let t0 = Instant::now();
        let catalog = api.fetch_catalog().await.expect("catalog");
        println!(
            "catalog: {} entities, {} rooms in {} ms; display room = {room:?}",
            catalog.entities.len(),
            catalog.areas().len(),
            t0.elapsed().as_millis()
        );
        let phrases = std::env::var("HA_PHRASES").unwrap_or_else(|_| {
            "turn off the office lights;lights on;turn on all the lights;porch lights off;\
             is the porch light on;are the office lights on;turn on the fireplace;\
             dim the dining lights to 40 percent;activate media room tv on;\
             turn off main floor lights;turn on the tv;turn off the pot lights;\
             what's the weather;set a timer for ten minutes"
                .to_string()
        });
        for phrase in phrases.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let outcome = match matcher::parse(phrase) {
                None => "not a home command → System-1 / LLM".to_string(),
                Some(cmd) => match plan(&catalog, &cmd, room.as_deref(), &["fireplace".into()]) {
                    None => format!("{cmd:?} → no unambiguous target → LLM"),
                    Some(p) => describe_plan(&p),
                },
            };
            println!("{phrase:45} → {outcome}");
        }
    }

    fn describe_plan(p: &Plan) -> String {
        let ids = |t: &Target| {
            t.entities
                .iter()
                .map(|e| e.entity_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        match p {
            Plan::Switch { on, target } => format!(
                "{} {} [{}]",
                target.label,
                if *on { "ON" } else { "OFF" },
                ids(target)
            ),
            Plan::Activate { entity } => format!("activate {}", entity.entity_id),
            Plan::Brightness { pct, target } => {
                format!("{} → {pct}% [{}]", target.label, ids(target))
            }
            Plan::Status { target } => format!("status of {} [{}]", target.label, ids(target)),
            Plan::Unavailable { target } => format!("{} unavailable", target.label),
        }
    }

    #[test]
    fn rest_api_debug_hides_the_token() {
        let api = RestApi::new("http://ha.local:8123", "super-secret");
        assert!(!format!("{api:?}").contains("super-secret"));
    }
}
