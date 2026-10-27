//! Signed-in access to the family's **Cadora** and **NextHaul** households: saved
//! recipes (search, ingredients, the display's recipe screen) and the NextHaul
//! shopping list.
//!
//! The voice-link token (`vl_…`, see the parent module) only authorizes Cadora's
//! `POST /cadora/voice/command`. Everything else on the shared API
//! (`api.getcadora.com`, Cloudflare Workers) wants a **user bearer token**: a 15-minute
//! ES256 JWT from identity-api, renewed with a rotating refresh token. So the Core
//! holds its **own identity**:
//!
//! - **Link** (config page, per app): the Core signs in anonymously once
//!   (`POST /identity/auth/anonymous`, no Turnstile for a native client), then redeems
//!   the household's 6-digit **family invite code** (`POST /identity/households/join`,
//!   the same code the apps share under "Invite with a code"). The Core becomes a
//!   household member named [`MEMBER_NAME`]. Households are shared across the apps by
//!   identity-api, so one identity can join a Cadora household, a NextHaul household,
//!   or both (a family using both apps may even use the same household).
//! - For **Cadora** the link also mints the Core its own voice link
//!   (`POST /cadora/voice/links`), so `calendar_add` / Cadora `shopping_list_add`
//!   keep using the voice API with no second code.
//! - **Each call** gets a fresh access token from [`HouseholdApps::access_token`],
//!   which refreshes when < 60 s remain. Refresh tokens are **single-use**: one hub
//!   instance lives for the whole process (built at boot, shared by every LLM
//!   rebuild, like the Home Assistant hub) and serializes refreshes, and every
//!   rotation is handed to the [`IdentitySink`] so the settings file always holds the
//!   newest refresh token.
//!
//! **Data.** Both apps sync offline-first; there are no per-entity REST reads. The
//! hub pulls `/{app}/sync/pull` incrementally into a small in-memory cache
//! (`recipes`, `recipe_items`, `list`, `list_item`) and searches that. Cadora pull
//! rows are raw `select *` (arrays/numbers can arrive as Postgres literals), so a
//! Cadora recipe's full detail is read from `GET /cadora/recipes/detail` instead;
//! Cadora's public **community/global** recipe tiers come from
//! `GET /cadora/recipes/discover`. NextHaul pull rows are `to_json` and used as is.
//! NextHaul shopping adds go through `POST /nexthaul/sync/push`, mirroring the app's
//! own add rule (the oldest list; a live unchecked row with the same normalized name
//! has its quantity bumped instead of being duplicated).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::{normalize_base_url, read_body, server_message, CadoraCommand, CadoraController};

/// The member name the Core joins a household as (shown in the apps' member lists).
pub const MEMBER_NAME: &str = "Anamanti";

/// `User-Agent` component label.
const COMPONENT: &str = "cadora-apps";

/// Per-request budget, so a stalled server can't hang a voice turn.
const TIMEOUT: Duration = Duration::from_secs(15);

/// Refresh the access token when fewer than this many seconds remain.
const REFRESH_MARGIN_SECS: i64 = 60;

/// A sync cache younger than this is used without pulling again (a search followed
/// by a show in the same turn costs one pull).
const PULL_FRESH: Duration = Duration::from_secs(20);

/// Upper bound on pull pages per refresh (1000 rows each).
const MAX_PULL_PAGES: usize = 50;

/// How long Cadora's community/global discovery list is reused.
const DISCOVER_TTL: Duration = Duration::from_secs(600);

/// Most mutations one sync push may carry (server limit).
const PUSH_BATCH: usize = 100;

/// Which family app a household link is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HouseholdApp {
    Cadora,
    #[serde(rename = "nexthaul")]
    NextHaul,
}

impl HouseholdApp {
    /// Both apps, in display order.
    pub const ALL: [HouseholdApp; 2] = [HouseholdApp::NextHaul, HouseholdApp::Cadora];

    /// The product name, for speech and the config page.
    pub fn label(self) -> &'static str {
        match self {
            HouseholdApp::Cadora => "Cadora",
            HouseholdApp::NextHaul => "NextHaul",
        }
    }

    /// The API path segment and identity `app` value.
    pub fn slug(self) -> &'static str {
        match self {
            HouseholdApp::Cadora => "cadora",
            HouseholdApp::NextHaul => "nexthaul",
        }
    }

    /// Parse a slug/label (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cadora" => Some(HouseholdApp::Cadora),
            "nexthaul" | "next_haul" | "next-haul" => Some(HouseholdApp::NextHaul),
            _ => None,
        }
    }
}

/// The Core's own identity-api user: persisted (0600 settings file) so a restart
/// resumes the session. The refresh token rotates on every use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreIdentity {
    pub user_id: String,
    pub refresh_token: String,
}

/// A household the Core has joined in one app.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedHousehold {
    /// The household id (uuid).
    pub id: String,
    /// The household's name, for the config page.
    #[serde(default)]
    pub name: String,
    /// The Core's member id in that household.
    #[serde(default)]
    pub member_id: Option<String>,
}

/// Receives every new [`CoreIdentity`] (sign-in and each refresh-token rotation) so
/// it can be persisted. Called with no hub lock held.
pub type IdentitySink = Arc<dyn Fn(CoreIdentity) + Send + Sync>;

/// The result of linking one app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkOutcome {
    pub app: HouseholdApp,
    pub household: LinkedHousehold,
    /// A freshly minted `vl_…` voice-link token (Cadora only; `None` if minting
    /// failed — the link still works for recipes).
    pub voice_token: Option<String>,
}

// ---------------------------------------------------------------------------
// Recipes: the seam the `saved_recipes` tool depends on
// ---------------------------------------------------------------------------

/// Where a saved recipe lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecipeTier {
    /// The household's own recipes.
    Household,
    /// Cadora's shared community collection.
    Community,
    /// Cadora's curated global collection.
    Global,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeHit {
    /// A stable reference the model can pass back (`nexthaul:<uuid>` / `cadora:<uuid>`).
    pub id: String,
    pub title: String,
    pub app: HouseholdApp,
    pub tier: RecipeTier,
    /// Ingredient count, when known.
    pub item_count: Option<usize>,
}

/// A saved recipe, ready for the display, plus the plain item names to put on a
/// shopping list (the parsed ingredient name, else the raw line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedRecipe {
    pub hit: RecipeHit,
    pub recipe: crate::recipe::Recipe,
    pub shopping_items: Vec<String>,
}

/// Search and read the household's saved recipes. Abstracted so the tool is
/// unit-testable without a network.
#[async_trait]
pub trait HouseholdRecipes: Send + Sync {
    /// Recipes matching `query` (best first; an empty query lists recent ones).
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<RecipeHit>>;
    /// The recipe a reference names: a hit `id`, else the best title match.
    /// `Ok(None)` when nothing matches.
    async fn recipe(&self, reference: &str) -> Result<Option<SavedRecipe>>;
    /// The linked apps' labels, for speech ("NextHaul and Cadora").
    fn sources(&self) -> String;
}

/// A shopping list the `shopping_list_add` tool (and recipe ingredient adds) write to.
#[async_trait]
pub trait ShoppingList: Send + Sync {
    /// Add one item to `list` (a list name; `None` = the default list); returns a
    /// speakable confirmation.
    async fn add(&self, item: &str, quantity: Option<u32>, list: Option<&str>) -> Result<String>;
    /// Add several items (e.g. a recipe's ingredients); returns a speakable summary.
    async fn add_many(&self, items: &[String], list: Option<&str>) -> Result<String>;
    /// What's on `list` (`None` = the default list), as a speakable summary.
    async fn read(&self, list: Option<&str>) -> Result<String>;
}

/// The Cadora shopping list over the voice API (the `vl_…` link).
pub struct VoiceShoppingList {
    controller: Arc<dyn CadoraController>,
    /// The signed-in hub, when Cadora is linked with a family code: reads the list
    /// (the voice API can only add).
    reader: Option<Arc<HouseholdApps>>,
}

impl VoiceShoppingList {
    pub fn wrap(controller: Arc<dyn CadoraController>) -> Arc<dyn ShoppingList> {
        Arc::new(Self {
            controller,
            reader: None,
        })
    }

    /// [`Self::wrap`], reading the list through `hub` when Cadora is signed in.
    pub fn wrap_with_reader(
        controller: Arc<dyn CadoraController>,
        hub: Option<Arc<HouseholdApps>>,
    ) -> Arc<dyn ShoppingList> {
        Arc::new(Self {
            controller,
            reader: hub.filter(|h| h.linked(HouseholdApp::Cadora)),
        })
    }
}

#[async_trait]
impl ShoppingList for VoiceShoppingList {
    // The voice API always adds to the household's default list; a list name is
    // ignored.
    async fn add(&self, item: &str, quantity: Option<u32>, _list: Option<&str>) -> Result<String> {
        self.controller
            .command(CadoraCommand::AddItem {
                item: item.to_string(),
                quantity,
            })
            .await
    }

    async fn add_many(&self, items: &[String], _list: Option<&str>) -> Result<String> {
        let items = dedupe_items(items);
        if items.is_empty() {
            bail!("there were no ingredients to add");
        }
        let mut added = Vec::new();
        let mut first_err = None;
        for item in &items {
            match self.add(item, None, None).await {
                Ok(_) => added.push(item.clone()),
                Err(e) => {
                    log::warn!("cadora: adding {item:?} failed: {e:#}");
                    first_err.get_or_insert(e);
                }
            }
        }
        if added.is_empty() {
            return Err(first_err.unwrap_or_else(|| anyhow!("nothing was added")));
        }
        let mut speech = added_summary(&added, &[], "Cadora shopping list");
        if added.len() < items.len() {
            speech.push_str(&format!(
                " {} couldn't be added.",
                count_phrase(items.len() - added.len(), "item")
            ));
        }
        Ok(speech)
    }

    async fn read(&self, list: Option<&str>) -> Result<String> {
        match &self.reader {
            Some(hub) => hub.read_list(HouseholdApp::Cadora, list).await,
            None => bail!(
                "I can't read the Cadora list over the voice link — link Cadora with a family \
                 invite code on the config page to read it"
            ),
        }
    }
}

/// The NextHaul shopping list over the signed-in sync API.
pub struct NextHaulShoppingList {
    hub: Arc<HouseholdApps>,
}

impl NextHaulShoppingList {
    pub fn wrap(hub: Arc<HouseholdApps>) -> Arc<dyn ShoppingList> {
        Arc::new(Self { hub })
    }
}

#[async_trait]
impl ShoppingList for NextHaulShoppingList {
    async fn add(&self, item: &str, quantity: Option<u32>, list: Option<&str>) -> Result<String> {
        let item = item.trim();
        if item.is_empty() {
            bail!("no item to add — say what to put on the shopping list");
        }
        let qty = quantity.filter(|q| *q >= 1).unwrap_or(1).min(99);
        let out = self
            .hub
            .nexthaul_add(&[(item.to_string(), qty)], list)
            .await?;
        let where_ = list_phrase(&out.list_name);
        Ok(if !out.bumped.is_empty() {
            format!(
                "{} was already on {where_}, so I bumped the quantity.",
                capitalize(item)
            )
        } else if qty > 1 {
            format!("Added {qty} {item} to {where_}.")
        } else {
            format!("Added {item} to {where_}.")
        })
    }

    async fn add_many(&self, items: &[String], list: Option<&str>) -> Result<String> {
        let items = dedupe_items(items);
        if items.is_empty() {
            bail!("there were no ingredients to add");
        }
        let pairs: Vec<(String, u32)> = items.iter().map(|i| (i.clone(), 1)).collect();
        let out = self.hub.nexthaul_add(&pairs, list).await?;
        Ok(added_summary(
            &out.added,
            &out.bumped,
            &format!("NextHaul {} list", out.list_name),
        ))
    }

    async fn read(&self, list: Option<&str>) -> Result<String> {
        self.hub.read_list(HouseholdApp::NextHaul, list).await
    }
}

/// "the NextHaul Groceries list" (names the list, so it's clear where an item went).
fn list_phrase(list_name: &str) -> String {
    if list_name.is_empty() {
        "the NextHaul shopping list".to_string()
    } else {
        format!("the NextHaul {list_name} list")
    }
}

/// Trim, drop blanks, and drop case-insensitive repeats, keeping order.
fn dedupe_items(items: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && seen.insert(normalize_name(s)))
        .collect()
}

fn count_phrase(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// "Added 5 items to the NextHaul shopping list: flour, eggs, milk, and 2 more."
fn added_summary(added: &[String], bumped: &[String], app: &str) -> String {
    const NAMED: usize = 4;
    let mut parts = Vec::new();
    if !added.is_empty() {
        let mut names: Vec<String> = added.iter().take(NAMED).cloned().collect();
        if added.len() > NAMED {
            names.push(format!("{} more", added.len() - NAMED));
        }
        parts.push(format!(
            "Added {} to the {app}: {}.",
            count_phrase(added.len(), "item"),
            join_list(&names)
        ));
    }
    if !bumped.is_empty() {
        let verb = if bumped.len() == 1 { "was" } else { "were" };
        parts.push(format!(
            "{} {verb} already on it, so I bumped the quantity.",
            capitalize(&join_list(bumped))
        ));
    }
    parts.join(" ")
}

/// "a", "a and b", "a, b, and c".
fn join_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// The app's name normalization: trim, lowercase, collapse whitespace.
pub fn normalize_name(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

// ---------------------------------------------------------------------------
// The hub
// ---------------------------------------------------------------------------

/// What [`HouseholdApps::nexthaul_add`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddOutcome {
    /// The list the items went to ("Groceries").
    pub list_name: String,
    /// Items inserted as new rows.
    pub added: Vec<String>,
    /// Items already on the list whose quantity was bumped.
    pub bumped: Vec<String>,
}

/// The Core's sign-in is dead (refresh token revoked / expired): only a new sign-in
/// recovers. Typed so linking can tell it apart from a transient network error.
#[derive(Debug)]
pub struct SessionExpired;

impl std::fmt::Display for SessionExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the Cadora / NextHaul sign-in expired — re-link on the config page \
             (Integrations → Cadora / NextHaul)",
        )
    }
}

impl std::error::Error for SessionExpired {}

/// The live access token and its expiry (unix seconds).
#[derive(Clone)]
struct Access {
    token: String,
    expires_at: i64,
}

#[derive(Default)]
struct HubState {
    /// `https://api.getcadora.com/cadora` (normalized; the voice API's base).
    cadora_base: String,
    /// `https://api.getcadora.com` (identity + nexthaul routes).
    origin: String,
    identity: Option<CoreIdentity>,
    access: Option<Access>,
    cadora: Option<LinkedHousehold>,
    nexthaul: Option<LinkedHousehold>,
}

impl HubState {
    fn household(&self, app: HouseholdApp) -> Option<&LinkedHousehold> {
        match app {
            HouseholdApp::Cadora => self.cadora.as_ref(),
            HouseholdApp::NextHaul => self.nexthaul.as_ref(),
        }
    }
}

/// The incremental sync cache for one app's household.
#[derive(Default)]
struct SyncCache {
    household: String,
    cursor: i64,
    epoch: i64,
    pulled_at: Option<Instant>,
    tables: HashMap<String, HashMap<String, Value>>,
}

/// The tables the cache keeps; everything else in a pull is dropped.
const CACHED_TABLES: [&str; 4] = ["recipes", "recipe_items", "list", "list_item"];

impl SyncCache {
    fn for_household(id: &str) -> Self {
        Self {
            household: id.to_string(),
            epoch: -1,
            ..Default::default()
        }
    }

    fn rows(&self, table: &str) -> impl Iterator<Item = &Value> {
        self.tables.get(table).into_iter().flat_map(|t| t.values())
    }
}

#[derive(Deserialize)]
struct PullPage {
    #[serde(default)]
    rows: HashMap<String, Vec<Value>>,
    #[serde(default)]
    removed: HashMap<String, Vec<Value>>,
    #[serde(default)]
    cursor: i64,
    #[serde(default)]
    epoch: i64,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    reset: bool,
}

/// A card from Cadora's community/global discovery.
#[derive(Clone)]
struct DiscoverCard {
    id: String,
    title: String,
    description: String,
    category: String,
    tier: RecipeTier,
    item_count: Option<usize>,
}

/// The Core's signed-in access to the Cadora / NextHaul households. One per process.
pub struct HouseholdApps {
    state: Mutex<HubState>,
    /// Serializes token refreshes (a refresh token is single-use).
    refresh_lock: tokio::sync::Mutex<()>,
    sink: Mutex<Option<IdentitySink>>,
    caches: tokio::sync::Mutex<HashMap<HouseholdApp, SyncCache>>,
    discover: tokio::sync::Mutex<Option<(Instant, Vec<DiscoverCard>)>>,
    /// NextHaul's learned + catalog categories, by normalized name (best-effort).
    categories: tokio::sync::Mutex<Option<HashMap<String, String>>>,
}

impl Default for HouseholdApps {
    fn default() -> Self {
        let base = normalize_base_url(None);
        Self {
            state: Mutex::new(HubState {
                origin: origin_of(&base),
                cadora_base: base,
                ..Default::default()
            }),
            refresh_lock: tokio::sync::Mutex::new(()),
            sink: Mutex::new(None),
            caches: tokio::sync::Mutex::new(HashMap::new()),
            discover: tokio::sync::Mutex::new(None),
            categories: tokio::sync::Mutex::new(None),
        }
    }
}

/// The API origin for a normalized Cadora base URL: the base minus its `/cadora`
/// route prefix (a base without one is used as the origin as is).
fn origin_of(cadora_base: &str) -> String {
    cadora_base
        .strip_suffix("/cadora")
        .unwrap_or(cadora_base)
        .to_string()
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// A random v4 UUID (client-generated row and mutation ids).
fn new_uuid() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// A stable per-install sync client id (idempotency key scope for pushes).
fn client_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(new_uuid)
}

/// The `{code, message}` error of a non-2xx response, as one line.
fn api_error(what: &str, status: reqwest::StatusCode, body: &Value) -> anyhow::Error {
    let code = body.get("code").and_then(Value::as_str).unwrap_or("");
    let msg = server_message(body).unwrap_or("request failed");
    if code.is_empty() {
        anyhow!("{what} failed ({status}): {msg}")
    } else {
        anyhow!("{what} failed ({status}, {code}): {msg}")
    }
}

fn str_field<'a>(row: &'a Value, key: &str) -> &'a str {
    row.get(key).and_then(Value::as_str).unwrap_or("")
}

fn bool_field(row: &Value, key: &str) -> bool {
    match row.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.as_str(), "t" | "true" | "1"),
        Some(Value::Number(n)) => n.as_i64() == Some(1),
        _ => false,
    }
}

fn num_field(row: &Value, key: &str) -> Option<f64> {
    match row.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A text-array column: a JSON array, or a Postgres array literal (`{a,"b c"}`) from
/// a raw `select *`.
fn text_array(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) => parse_pg_array(s),
        _ => Vec::new(),
    }
}

/// Parse a one-dimensional Postgres text-array literal.
fn parse_pg_array(s: &str) -> Vec<String> {
    let s = s.trim();
    let Some(inner) = s.strip_prefix('{').and_then(|r| r.strip_suffix('}')) else {
        return if s.is_empty() {
            Vec::new()
        } else {
            vec![s.to_string()]
        };
    };
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                was_quoted = true;
            }
            '\\' if quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ',' if !quoted => {
                if was_quoted || !cur.eq_ignore_ascii_case("null") {
                    out.push(std::mem::take(&mut cur));
                }
                cur.clear();
                was_quoted = false;
            }
            _ => cur.push(c),
        }
    }
    if !inner.is_empty() && (was_quoted || !cur.eq_ignore_ascii_case("null")) {
        out.push(cur);
    }
    out
}

/// Words for matching: lowercase alphanumeric runs.
fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Words that say nothing about which recipe.
const STOP_WORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "for",
    "of",
    "and",
    "with",
    "my",
    "our",
    "recipe",
    "recipes",
    "me",
    "show",
    "some",
    "saved",
    "family",
    "favorite",
    "favourite",
];

/// How well `query` matches a recipe: title words count most, then category /
/// description / ingredient words. `0` = no match.
fn match_score(query: &str, title: &str, extra: &str) -> u32 {
    let q: Vec<String> = words(query)
        .into_iter()
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
        .collect();
    if q.is_empty() {
        return 1;
    }
    let t = words(title);
    let e = words(extra);
    let stem = |w: &str| w.trim_end_matches('s').to_string();
    let has = |hay: &[String], w: &str| {
        let ws = stem(w);
        hay.iter()
            .any(|h| h == w || stem(h) == ws || (ws.len() >= 4 && h.starts_with(&ws)))
    };
    let mut score = 0;
    let mut title_hits = 0;
    for w in &q {
        if has(&t, w) {
            score += 10;
            title_hits += 1;
        } else if has(&e, w) {
            score += 3;
        } else {
            // Every query word must appear somewhere.
            return 0;
        }
    }
    if title_hits == q.len() {
        score += 5;
        if normalize_name(title) == q.join(" ") {
            score += 20;
        }
    }
    score
}

impl HouseholdApps {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Install the identity sink (the settings layer's persistence hook).
    pub fn set_identity_sink(&self, sink: IdentitySink) {
        *self.sink.lock().unwrap() = Some(sink);
    }

    /// Point the hub at the live settings: the Cadora base URL, the persisted
    /// identity, and the linked households. Called on boot and every settings
    /// rebuild. An identity for the **same user** keeps the in-memory session (it
    /// holds the newest, possibly not-yet-persisted refresh token); a different or
    /// absent identity replaces it. A changed household drops that app's cache.
    pub fn configure(
        &self,
        base_url: Option<&str>,
        identity: Option<&CoreIdentity>,
        cadora: Option<&LinkedHousehold>,
        nexthaul: Option<&LinkedHousehold>,
    ) {
        let mut s = self.state.lock().unwrap();
        let base = normalize_base_url(base_url);
        if base != s.cadora_base {
            s.origin = origin_of(&base);
            s.cadora_base = base;
            s.access = None;
        }
        let same_user = matches!(
            (&s.identity, identity),
            (Some(a), Some(b)) if a.user_id == b.user_id
        );
        if !same_user {
            s.identity = identity.cloned();
            s.access = None;
        }
        s.cadora = cadora.cloned();
        s.nexthaul = nexthaul.cloned();
    }

    /// Whether `app` has a usable signed-in household link.
    pub fn linked(&self, app: HouseholdApp) -> bool {
        let s = self.state.lock().unwrap();
        s.identity.is_some() && s.household(app).is_some()
    }

    /// The linked apps, in display order.
    pub fn linked_apps(&self) -> Vec<HouseholdApp> {
        HouseholdApp::ALL
            .into_iter()
            .filter(|a| self.linked(*a))
            .collect()
    }

    fn origin(&self) -> String {
        self.state.lock().unwrap().origin.clone()
    }

    fn cadora_base(&self) -> String {
        self.state.lock().unwrap().cadora_base.clone()
    }

    fn household(&self, app: HouseholdApp) -> Result<LinkedHousehold> {
        let s = self.state.lock().unwrap();
        if s.identity.is_none() {
            bail!("{} isn't linked — link it on the config page", app.label());
        }
        s.household(app)
            .cloned()
            .with_context(|| format!("{} isn't linked — link it on the config page", app.label()))
    }

    /// Store a fresh session and hand the identity to the sink.
    fn store_session(&self, body: &Value) -> Result<()> {
        let access = str_field(body, "access_token");
        let refresh = str_field(body, "refresh_token");
        let user_id = body
            .get("user")
            .and_then(|u| u.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if access.is_empty() || refresh.is_empty() || user_id.is_empty() {
            bail!("the sign-in response carried no session");
        }
        let expires_at = body
            .get("expires_at")
            .and_then(Value::as_i64)
            .or_else(|| {
                body.get("expires_in")
                    .and_then(Value::as_i64)
                    .map(|s| now_unix() + s)
            })
            .unwrap_or_else(|| now_unix() + 900);
        let identity = CoreIdentity {
            user_id: user_id.to_string(),
            refresh_token: refresh.to_string(),
        };
        {
            let mut s = self.state.lock().unwrap();
            s.identity = Some(identity.clone());
            s.access = Some(Access {
                token: access.to_string(),
                expires_at,
            });
        }
        let sink = self.sink.lock().unwrap().clone();
        if let Some(sink) = sink {
            sink(identity);
        }
        Ok(())
    }

    /// A valid access token, refreshing (serialized) when it is about to expire.
    pub async fn access_token(&self) -> Result<String> {
        if let Some(t) = self.cached_access() {
            return Ok(t);
        }
        let _guard = self.refresh_lock.lock().await;
        // Another caller may have refreshed while we waited.
        if let Some(t) = self.cached_access() {
            return Ok(t);
        }
        let (origin, refresh) = {
            let s = self.state.lock().unwrap();
            let id = s
                .identity
                .clone()
                .context("Cadora / NextHaul aren't linked — link them on the config page")?;
            (s.origin.clone(), id.refresh_token)
        };
        let resp = crate::http::send(
            crate::http::post(format!("{origin}/identity/auth/refresh"), COMPONENT)
                .json(&json!({ "refresh_token": refresh })),
            TIMEOUT,
            "refreshing the Cadora sign-in",
        )
        .await?;
        let (status, body) = read_body(resp, "the Cadora sign-in refresh").await?;
        if !status.is_success() {
            let code = str_field(&body, "code");
            if matches!(
                code,
                "invalid_refresh_token" | "session_expired" | "user_not_found"
            ) || status.as_u16() == 401
            {
                return Err(SessionExpired.into());
            }
            return Err(api_error("refreshing the Cadora sign-in", status, &body));
        }
        self.store_session(&body)?;
        self.cached_access()
            .context("the Cadora sign-in refresh returned no token")
    }

    fn cached_access(&self) -> Option<String> {
        let s = self.state.lock().unwrap();
        s.access
            .as_ref()
            .filter(|a| a.expires_at - now_unix() > REFRESH_MARGIN_SECS)
            .map(|a| a.token.clone())
    }

    /// Sign in anonymously when the Core has no identity yet.
    async fn ensure_identity(&self, app: HouseholdApp) -> Result<()> {
        let _guard = self.refresh_lock.lock().await;
        if self.state.lock().unwrap().identity.is_some() {
            return Ok(());
        }
        let origin = self.origin();
        let resp = crate::http::send(
            crate::http::post(format!("{origin}/identity/auth/anonymous"), COMPONENT)
                .json(&json!({ "app": app.slug() })),
            TIMEOUT,
            "signing in to Cadora",
        )
        .await?;
        let (status, body) = read_body(resp, "the Cadora sign-in").await?;
        if !status.is_success() {
            if status.as_u16() == 429 {
                bail!("too many sign-in attempts — wait a few minutes and try again");
            }
            return Err(api_error("signing in to Cadora", status, &body));
        }
        self.store_session(&body)
    }

    /// An authenticated request (fresh token) and its `(status, json body)`.
    async fn call(
        &self,
        method: Method,
        url: String,
        body: Option<&Value>,
        what: &str,
    ) -> Result<(reqwest::StatusCode, Value)> {
        let token = self.access_token().await?;
        let mut req = crate::http::request(method, url, COMPONENT).bearer_auth(token);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = crate::http::send(req, TIMEOUT, what).await?;
        read_body(resp, what).await
    }

    /// Link `app` with a household's 6-digit family invite code: sign in (once),
    /// join the household as [`MEMBER_NAME`], and for Cadora mint a voice link.
    pub async fn link(&self, app: HouseholdApp, code: &str) -> Result<LinkOutcome> {
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        if code.is_empty() {
            bail!("enter the 6-digit invite code from the {} app", app.label());
        }
        // A dead session can't be refreshed: start over with a new sign-in (the other
        // app, if linked under the old one, then asks to be re-linked too).
        if self.state.lock().unwrap().identity.is_some() {
            if let Err(e) = self.access_token().await {
                if e.downcast_ref::<SessionExpired>().is_some() {
                    log::warn!("cadora: stored sign-in expired; signing in again to link");
                    let mut st = self.state.lock().unwrap();
                    st.identity = None;
                    st.access = None;
                } else {
                    return Err(e);
                }
            }
        }
        self.ensure_identity(app).await?;
        let origin = self.origin();
        let (status, body) = self
            .call(
                Method::POST,
                format!("{origin}/identity/households/join"),
                Some(&json!({ "code": code, "member_name": MEMBER_NAME })),
                "joining the household",
            )
            .await?;
        if !status.is_success() {
            let msg = match (status.as_u16(), str_field(&body, "code")) {
                (429, _) => "too many attempts — wait a few minutes and try again".to_string(),
                (_, "invalid_code") => "that code is invalid or has expired — make a new one \
                     in the app (Settings → Invite with a code)"
                    .to_string(),
                _ => api_error("joining the household", status, &body).to_string(),
            };
            bail!("could not link {}: {msg}", app.label());
        }
        let hh = body.get("household").cloned().unwrap_or(Value::Null);
        let household = LinkedHousehold {
            id: str_field(&hh, "id").to_string(),
            name: str_field(&hh, "name").to_string(),
            member_id: body
                .get("member")
                .and_then(|m| m.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        if household.id.is_empty() {
            bail!(
                "could not link {}: the join response named no household",
                app.label()
            );
        }
        let voice_token = match app {
            HouseholdApp::Cadora => match self.mint_voice_link(&household).await {
                Ok(t) => Some(t),
                Err(e) => {
                    log::warn!("cadora: linked, but minting a voice link failed: {e:#}");
                    None
                }
            },
            HouseholdApp::NextHaul => None,
        };
        Ok(LinkOutcome {
            app,
            household,
            voice_token,
        })
    }

    /// Mint the Core its own Cadora voice link (`vl_…`) for `household`.
    async fn mint_voice_link(&self, household: &LinkedHousehold) -> Result<String> {
        let base = self.cadora_base();
        let mut body = json!({
            "household": household.id,
            "provider": "generic",
            "label": MEMBER_NAME,
        });
        if let Some(m) = &household.member_id {
            body["member"] = json!(m);
        }
        let (status, resp) = self
            .call(
                Method::POST,
                format!("{base}/voice/links"),
                Some(&body),
                "creating the Cadora voice link",
            )
            .await?;
        if !status.is_success() {
            return Err(api_error("creating the Cadora voice link", status, &resp));
        }
        resp.get("token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .context("the voice-link response carried no token")
    }

    /// Best-effort: leave `household` (removes the Core's member row) on unlink.
    pub async fn leave(&self, household: &LinkedHousehold) {
        let Some(mid) = &household.member_id else {
            return;
        };
        let origin = self.origin();
        let url = format!(
            "{origin}/identity/households/{}/members/{mid}",
            household.id
        );
        match self
            .call(Method::DELETE, url, None, "leaving the household")
            .await
        {
            Ok((s, _)) if s.is_success() => {}
            Ok((s, b)) => log::warn!(
                "cadora: leaving household {} failed: {}",
                household.id,
                api_error("leaving the household", s, &b)
            ),
            Err(e) => log::warn!("cadora: leaving household {} failed: {e:#}", household.id),
        }
    }

    // ---- sync cache --------------------------------------------------------------

    /// Bring `app`'s cache up to date (incremental pull), unless it is fresh and
    /// `force` is false. Runs `f` on the cache afterwards.
    async fn with_cache<R>(
        &self,
        app: HouseholdApp,
        force: bool,
        f: impl FnOnce(&SyncCache) -> R,
    ) -> Result<R> {
        let hh = self.household(app)?;
        let mut caches = self.caches.lock().await;
        let cache = caches
            .entry(app)
            .or_insert_with(|| SyncCache::for_household(&hh.id));
        if cache.household != hh.id {
            *cache = SyncCache::for_household(&hh.id);
        }
        let fresh = cache.pulled_at.is_some_and(|t| t.elapsed() < PULL_FRESH);
        if force || !fresh {
            self.pull_into(app, cache).await?;
        }
        Ok(f(cache))
    }

    async fn pull_into(&self, app: HouseholdApp, cache: &mut SyncCache) -> Result<()> {
        let origin = self.origin();
        for _ in 0..MAX_PULL_PAGES {
            let url = format!(
                "{origin}/{}/sync/pull?household={}&cursor={}&epoch={}&limit=1000",
                app.slug(),
                cache.household,
                cache.cursor,
                cache.epoch
            );
            let what = format!("reading the {} household", app.label());
            let (status, body) = self.call(Method::GET, url, None, &what).await?;
            if !status.is_success() {
                if str_field(&body, "code") == "not_a_member" || status.as_u16() == 403 {
                    bail!(
                        "Anamanti is no longer a member of the {} household — re-link it on \
                         the config page",
                        app.label()
                    );
                }
                return Err(api_error(&what, status, &body));
            }
            let page: PullPage = serde_json::from_value(body)
                .with_context(|| format!("parsing the {what} response"))?;
            if page.reset {
                cache.tables.clear();
            }
            for (table, rows) in page.rows {
                if !CACHED_TABLES.contains(&table.as_str()) {
                    continue;
                }
                let t = cache.tables.entry(table).or_default();
                for row in rows {
                    let id = str_field(&row, "id").to_string();
                    if !id.is_empty() {
                        t.insert(id, row);
                    }
                }
            }
            for (table, ids) in page.removed {
                if let Some(t) = cache.tables.get_mut(&table) {
                    for id in ids {
                        if let Some(id) = id.as_str() {
                            t.remove(id);
                        }
                    }
                }
            }
            cache.cursor = page.cursor;
            cache.epoch = page.epoch;
            if !page.has_more {
                break;
            }
        }
        cache.pulled_at = Some(Instant::now());
        Ok(())
    }

    // ---- recipes -----------------------------------------------------------------

    /// Household recipe cards for `app` from its sync cache:
    /// `(id, title, extra text, item count)`.
    async fn household_cards(
        &self,
        app: HouseholdApp,
    ) -> Result<Vec<(String, String, String, usize)>> {
        self.with_cache(app, false, |c| {
            let mut counts: HashMap<&str, usize> = HashMap::new();
            let mut item_words: HashMap<&str, String> = HashMap::new();
            for it in c.rows("recipe_items") {
                let rid = str_field(it, "recipe_id");
                *counts.entry(rid).or_default() += 1;
                let name = match str_field(it, "item") {
                    "" => str_field(it, "raw_text"),
                    s => s,
                };
                let e = item_words.entry(rid).or_default();
                e.push(' ');
                e.push_str(name);
            }
            c.rows("recipes")
                .filter(|r| !str_field(r, "title").trim().is_empty())
                .map(|r| {
                    let id = str_field(r, "id");
                    let extra = format!(
                        "{} {} {}",
                        str_field(r, "category"),
                        str_field(r, "description"),
                        item_words.get(id).map(String::as_str).unwrap_or("")
                    );
                    (
                        id.to_string(),
                        str_field(r, "title").trim().to_string(),
                        extra,
                        counts.get(id).copied().unwrap_or(0),
                    )
                })
                .collect()
        })
        .await
    }

    /// Cadora's community + global recipe cards (cached [`DISCOVER_TTL`]).
    async fn discover_cards(&self) -> Result<Vec<DiscoverCard>> {
        let mut guard = self.discover.lock().await;
        if let Some((at, cards)) = guard.as_ref() {
            if at.elapsed() < DISCOVER_TTL {
                return Ok(cards.clone());
            }
        }
        let base = self.cadora_base();
        let mut cards = Vec::new();
        for (tier_q, tier) in [
            ("community", RecipeTier::Community),
            ("global", RecipeTier::Global),
        ] {
            let (status, body) = self
                .call(
                    Method::GET,
                    format!("{base}/recipes/discover?tier={tier_q}"),
                    None,
                    "reading Cadora's recipe collection",
                )
                .await?;
            if !status.is_success() {
                return Err(api_error(
                    "reading Cadora's recipe collection",
                    status,
                    &body,
                ));
            }
            for r in body
                .get("recipes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let title = str_field(r, "title").trim();
                if title.is_empty() {
                    continue;
                }
                cards.push(DiscoverCard {
                    id: str_field(r, "id").to_string(),
                    title: title.to_string(),
                    description: str_field(r, "description").to_string(),
                    category: str_field(r, "category").to_string(),
                    tier,
                    item_count: r
                        .get("item_count")
                        .and_then(Value::as_u64)
                        .map(|n| n as usize),
                });
            }
        }
        *guard = Some((Instant::now(), cards.clone()));
        Ok(cards)
    }

    /// Every candidate recipe across the linked apps, with its match text.
    async fn candidates(&self) -> Result<Vec<(RecipeHit, String)>> {
        let mut out = Vec::new();
        let mut errors = Vec::new();
        for app in self.linked_apps() {
            match self.household_cards(app).await {
                Ok(cards) => out.extend(cards.into_iter().map(|(id, title, extra, n)| {
                    (
                        RecipeHit {
                            id: format!("{}:{id}", app.slug()),
                            title,
                            app,
                            tier: RecipeTier::Household,
                            item_count: Some(n),
                        },
                        extra,
                    )
                })),
                Err(e) => errors.push(e),
            }
        }
        if self.linked(HouseholdApp::Cadora) {
            match self.discover_cards().await {
                Ok(cards) => out.extend(cards.into_iter().map(|c| {
                    (
                        RecipeHit {
                            id: format!("cadora:{}", c.id),
                            title: c.title,
                            app: HouseholdApp::Cadora,
                            tier: c.tier,
                            item_count: c.item_count,
                        },
                        format!("{} {}", c.category, c.description),
                    )
                })),
                // Discovery is a bonus; household recipes still answer.
                Err(e) => log::warn!("cadora: recipe discovery failed: {e:#}"),
            }
        }
        if out.is_empty() {
            if let Some(e) = errors.into_iter().next() {
                return Err(e);
            }
        } else {
            for e in errors {
                log::warn!("cadora: reading recipes failed: {e:#}");
            }
        }
        Ok(out)
    }

    /// Rank candidates for `query`: score, then household before shared tiers,
    /// dropping same-titled repeats (a family using both apps may have both copies).
    fn rank(query: &str, cands: Vec<(RecipeHit, String)>, limit: usize) -> Vec<RecipeHit> {
        let tier_rank = |t: RecipeTier| match t {
            RecipeTier::Household => 0,
            RecipeTier::Community => 1,
            RecipeTier::Global => 2,
        };
        let mut scored: Vec<(u32, RecipeHit)> = cands
            .into_iter()
            .filter_map(|(hit, extra)| {
                let s = match_score(query, &hit.title, &extra);
                (s > 0).then_some((s, hit))
            })
            .collect();
        scored.sort_by(|(sa, a), (sb, b)| {
            sb.cmp(sa)
                .then(tier_rank(a.tier).cmp(&tier_rank(b.tier)))
                .then(a.title.cmp(&b.title))
        });
        let mut seen = HashSet::new();
        scored
            .into_iter()
            .map(|(_, h)| h)
            .filter(|h| seen.insert(normalize_name(&h.title)))
            .take(limit)
            .collect()
    }

    /// A NextHaul recipe from the cache.
    async fn nexthaul_recipe(&self, id: &str) -> Result<Option<(Value, Vec<Value>)>> {
        self.with_cache(HouseholdApp::NextHaul, false, |c| {
            let rec = c.tables.get("recipes")?.get(id)?.clone();
            let mut items: Vec<Value> = c
                .rows("recipe_items")
                .filter(|it| str_field(it, "recipe_id") == id)
                .cloned()
                .collect();
            items.sort_by_key(|it| num_field(it, "position").unwrap_or(0.0) as i64);
            Some((rec, items))
        })
        .await
    }

    /// A Cadora recipe: the household row from the cache (for the title etc.) or the
    /// discovery card, plus `GET /cadora/recipes/detail` for items and steps.
    async fn cadora_recipe(&self, hit: &RecipeHit, raw_id: &str) -> Result<(Value, Vec<Value>)> {
        let base = self.cadora_base();
        let (status, body) = self
            .call(
                Method::GET,
                format!("{base}/recipes/detail?id={raw_id}"),
                None,
                "reading the Cadora recipe",
            )
            .await?;
        if !status.is_success() {
            return Err(api_error("reading the Cadora recipe", status, &body));
        }
        let mut rec = if hit.tier == RecipeTier::Household {
            self.with_cache(HouseholdApp::Cadora, false, |c| {
                c.tables.get("recipes").and_then(|t| t.get(raw_id)).cloned()
            })
            .await?
            .unwrap_or_else(|| json!({}))
        } else {
            let cards = self.discover_cards().await.unwrap_or_default();
            cards
                .iter()
                .find(|c| c.id == raw_id)
                .map(|c| json!({ "description": c.description, "category": c.category }))
                .unwrap_or_else(|| json!({}))
        };
        if str_field(&rec, "title").is_empty() {
            rec["title"] = json!(hit.title);
        }
        rec["instructions"] = body.get("instructions").cloned().unwrap_or(json!([]));
        let mut items: Vec<Value> = body
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        items.sort_by_key(|it| num_field(it, "position").unwrap_or(0.0) as i64);
        Ok((rec, items))
    }

    // ---- NextHaul shopping ----------------------------------------------------------

    /// Best-effort category lookup: the household's learned categories, then the
    /// NextHaul catalog (name or search term). Loaded once.
    async fn category_for(&self, household: &str, norm: &str) -> Option<String> {
        let mut guard = self.categories.lock().await;
        if guard.is_none() {
            let mut map = HashMap::new();
            let origin = self.origin();
            if let Ok((s, body)) = self
                .call(
                    Method::GET,
                    format!("{origin}/nexthaul/catalog"),
                    None,
                    "reading the NextHaul catalog",
                )
                .await
            {
                if s.is_success() {
                    for it in body
                        .get("items")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let cat = str_field(it, "category");
                        if cat.is_empty() {
                            continue;
                        }
                        map.entry(normalize_name(str_field(it, "name")))
                            .or_insert_with(|| cat.to_string());
                        for term in text_array(it.get("search_terms")) {
                            map.entry(normalize_name(&term))
                                .or_insert_with(|| cat.to_string());
                        }
                    }
                }
            }
            if let Ok((s, body)) = self
                .call(
                    Method::GET,
                    format!("{origin}/nexthaul/prices/categories?household={household}"),
                    None,
                    "reading NextHaul categories",
                )
                .await
            {
                if s.is_success() {
                    if let Some(m) = body.get("categories").and_then(Value::as_object) {
                        for (k, v) in m {
                            if let Some(v) = v.as_str() {
                                // Learned household categories win over the catalog.
                                map.insert(normalize_name(k), v.to_string());
                            }
                        }
                    }
                }
            }
            *guard = Some(map);
        }
        guard.as_ref()?.get(norm).cloned()
    }

    /// Add `(item, quantity)` pairs to the NextHaul household's default (oldest)
    /// shopping list via sync push, bumping the quantity of a live unchecked row with
    /// the same normalized name instead of duplicating it.
    pub async fn nexthaul_add(
        &self,
        items: &[(String, u32)],
        list: Option<&str>,
    ) -> Result<AddOutcome> {
        let app = HouseholdApp::NextHaul;
        let hh = self.household(app)?;
        // Fresh state for list resolution + dedupe.
        let (picked, existing) = self
            .with_cache(app, true, |c| {
                let picked = pick_list(c, list);
                let list_id = picked
                    .as_ref()
                    .ok()
                    .and_then(|p| p.as_ref().map(|(id, _)| id.clone()));
                let existing: HashMap<String, (String, i64)> = match &list_id {
                    Some(lid) => c
                        .rows("list_item")
                        .filter(|it| {
                            str_field(it, "list_id") == lid
                                && !bool_field(it, "deleted")
                                && !bool_field(it, "archived")
                                && !bool_field(it, "checked")
                        })
                        .map(|it| {
                            let norm = match str_field(it, "name_normalized") {
                                "" => normalize_name(str_field(it, "text")),
                                n => n.to_string(),
                            };
                            let q = num_field(it, "quantity").unwrap_or(1.0) as i64;
                            (norm, (str_field(it, "id").to_string(), q))
                        })
                        .collect(),
                    None => HashMap::new(),
                };
                (picked, existing)
            })
            .await?;

        let mut mutations = Vec::new();
        let mut out = AddOutcome::default();
        let list_id = match picked.map_err(|e| anyhow!(e))? {
            Some((id, name)) => {
                out.list_name = name;
                id
            }
            None => {
                let id = new_uuid();
                mutations.push(json!({
                    "mutation_id": new_uuid(), "op": "insert", "table": "list", "id": id,
                    "fields": { "name": "Shopping", "type": "shopping" }
                }));
                out.list_name = "Shopping".to_string();
                id
            }
        };
        let mut bumps: HashMap<String, (String, i64)> = HashMap::new();
        let mut inserted: HashSet<String> = HashSet::new();
        for (text, qty) in items {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let norm = normalize_name(text);
            let qty = i64::from((*qty).clamp(1, 99));
            if let Some((row_id, q)) = existing.get(&norm) {
                let e = bumps.entry(norm).or_insert_with(|| (row_id.clone(), *q));
                e.1 += qty;
                out.bumped.push(text.to_string());
                continue;
            }
            if !inserted.insert(norm.clone()) {
                continue;
            }
            let mut fields = Map::new();
            fields.insert("list_id".into(), json!(list_id));
            fields.insert("text".into(), json!(text));
            fields.insert("name_normalized".into(), json!(norm));
            fields.insert("quantity".into(), json!(qty));
            fields.insert("mode".into(), json!("count"));
            if let Some(cat) = self.category_for(&hh.id, &norm).await {
                fields.insert("category".into(), json!(cat));
            }
            mutations.push(json!({
                "mutation_id": new_uuid(), "op": "insert", "table": "list_item",
                "id": new_uuid(), "fields": Value::Object(fields)
            }));
            out.added.push(text.to_string());
        }
        for (row_id, q) in bumps.into_values() {
            mutations.push(json!({
                "mutation_id": new_uuid(), "op": "patch", "table": "list_item",
                "id": row_id, "fields": { "quantity": q.min(999) }
            }));
        }
        if mutations.is_empty() {
            bail!("no item to add — say what to put on the shopping list");
        }
        self.push(app, &hh.id, mutations).await?;
        // The next read pulls our own writes back.
        if let Some(c) = self.caches.lock().await.get_mut(&app) {
            c.pulled_at = None;
        }
        Ok(out)
    }

    /// What's on one of `app`'s shopping lists (`list` = a name; default = the
    /// oldest list), as a speakable summary: unchecked items first, then how many are
    /// already checked off, plus the names of the other lists.
    pub async fn read_list(&self, app: HouseholdApp, list: Option<&str>) -> Result<String> {
        let label = app.label();
        self.with_cache(app, true, |c| {
            let (id, name) = match pick_list(c, list).map_err(|e| anyhow!(e))? {
                Some(p) => p,
                None => return Ok(format!("The {label} household has no shopping list yet.")),
            };
            let mut open: Vec<(String, String)> = Vec::new();
            let mut done = 0usize;
            for it in c.rows("list_item").filter(|it| {
                str_field(it, "list_id") == id
                    && !bool_field(it, "deleted")
                    && !bool_field(it, "archived")
            }) {
                if bool_field(it, "checked") {
                    done += 1;
                    continue;
                }
                let text = str_field(it, "text").trim();
                if text.is_empty() {
                    continue;
                }
                let q = num_field(it, "quantity").unwrap_or(1.0) as i64;
                let label = if q > 1 {
                    format!("{q} {text}")
                } else {
                    text.to_string()
                };
                open.push((str_field(it, "category").to_string(), label));
            }
            // Store order: grouped by category, then by name.
            open.sort_by(|a, b| {
                a.0.cmp(&b.0)
                    .then(a.1.to_lowercase().cmp(&b.1.to_lowercase()))
            });
            let names: Vec<String> = open.into_iter().map(|(_, l)| l).collect();
            let mut out = if names.is_empty() {
                format!("The {label} {name} list is empty.")
            } else {
                format!(
                    "The {label} {name} list has {}: {}.",
                    count_phrase(names.len(), "item"),
                    join_list(&names)
                )
            };
            if done > 0 {
                out.push_str(&format!(
                    " {} already checked off.",
                    count_phrase(done, "item")
                ));
            }
            let others: Vec<String> = shopping_lists(c)
                .into_iter()
                .map(|l| str_field(l, "name").to_string())
                .filter(|n| *n != name)
                .collect();
            if !others.is_empty() {
                out.push_str(&format!(" Other lists: {}.", join_list(&others)));
            }
            Ok(out)
        })
        .await?
    }

    /// `POST /{app}/sync/push` in batches; any rejected mutation fails the add.
    async fn push(&self, app: HouseholdApp, household: &str, mutations: Vec<Value>) -> Result<()> {
        let origin = self.origin();
        let what = format!("writing to the {} list", app.label());
        for batch in mutations.chunks(PUSH_BATCH) {
            let body = json!({
                "household": household,
                "client_id": client_id(),
                "mutations": batch,
            });
            let (status, resp) = self
                .call(
                    Method::POST,
                    format!("{origin}/{}/sync/push", app.slug()),
                    Some(&body),
                    &what,
                )
                .await?;
            if !status.is_success() {
                if str_field(&resp, "code") == "not_a_member" {
                    bail!(
                        "Anamanti is no longer a member of the {} household — re-link it on \
                         the config page",
                        app.label()
                    );
                }
                return Err(api_error(&what, status, &resp));
            }
            let bad: Vec<String> = resp
                .get("results")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|r| str_field(r, "status") != "ok")
                .map(|r| {
                    format!(
                        "{} {}",
                        str_field(r, "code"),
                        server_message(r).unwrap_or_default()
                    )
                    .trim()
                    .to_string()
                })
                .collect();
            if !bad.is_empty() {
                bail!(
                    "the {} list rejected the change: {}",
                    app.label(),
                    bad.join("; ")
                );
            }
        }
        Ok(())
    }
}

/// A household's shopping lists, oldest first (the apps' default-list rule).
fn shopping_lists(c: &SyncCache) -> Vec<&Value> {
    let mut lists: Vec<&Value> = c
        .rows("list")
        .filter(|l| matches!(str_field(l, "type"), "" | "shopping"))
        .collect();
    lists.sort_by(|a, b| {
        str_field(a, "created_at")
            .cmp(str_field(b, "created_at"))
            .then(str_field(a, "name").cmp(str_field(b, "name")))
    });
    lists
}

/// The list `name` asks for (exact, then word match; "list"/"shopping" ignored), or
/// the default (oldest) list when `name` is empty. `Ok(None)` = the household has no
/// list yet; `Err` = no list by that name (the message names the lists there are).
fn pick_list(c: &SyncCache, name: Option<&str>) -> Result<Option<(String, String)>, String> {
    let lists = shopping_lists(c);
    let pair = |l: &Value| {
        (
            str_field(l, "id").to_string(),
            str_field(l, "name").to_string(),
        )
    };
    let wanted: Vec<String> = words(name.unwrap_or(""))
        .into_iter()
        .filter(|w| {
            !matches!(
                w.as_str(),
                "list" | "shopping" | "grocery" | "the" | "my" | "our"
            )
        })
        .collect();
    if wanted.is_empty() {
        return Ok(lists.first().map(|l| pair(l)));
    }
    let wanted_joined = wanted.join(" ");
    if let Some(l) = lists
        .iter()
        .find(|l| normalize_name(str_field(l, "name")) == wanted_joined)
    {
        return Ok(Some(pair(l)));
    }
    if let Some(l) = lists.iter().find(|l| {
        let have = words(str_field(l, "name"));
        wanted.iter().all(|w| {
            have.iter()
                .any(|h| h.trim_end_matches('s') == w.trim_end_matches('s'))
        })
    }) {
        return Ok(Some(pair(l)));
    }
    // "groceries" / "grocery list" mean the default list when no list is called that.
    if wanted
        .iter()
        .all(|w| matches!(w.as_str(), "groceries" | "food"))
    {
        return Ok(lists.first().map(|l| pair(l)));
    }
    let names: Vec<String> = lists
        .iter()
        .map(|l| str_field(l, "name").to_string())
        .collect();
    Err(format!(
        "there's no list called \"{}\" — the lists are {}",
        name.unwrap_or("").trim(),
        join_list(&names)
    ))
}

/// A display [`crate::recipe::Recipe`] (+ shopping item names) from a recipe row and
/// its ordered item rows. Tolerates both JSON and Postgres-literal columns.
fn to_saved(hit: RecipeHit, rec: &Value, items: &[Value]) -> SavedRecipe {
    let servings = num_field(rec, "servings")
        .filter(|n| *n > 0.0)
        .map(|n| {
            let n = n.round() as i64;
            if n == 1 {
                "1 serving".to_string()
            } else {
                format!("{n} servings")
            }
        })
        .unwrap_or_default();
    let ingredients: Vec<String> = items
        .iter()
        .map(|it| str_field(it, "raw_text").trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let shopping_items: Vec<String> = items
        .iter()
        .filter_map(|it| {
            let s = match str_field(it, "item").trim() {
                "" => str_field(it, "raw_text").trim(),
                s => s,
            };
            (!s.is_empty()).then(|| s.to_string())
        })
        .collect();
    let title = match str_field(rec, "title").trim() {
        "" => hit.title.clone(),
        t => t.to_string(),
    };
    SavedRecipe {
        recipe: crate::recipe::Recipe {
            title,
            summary: str_field(rec, "description").trim().to_string(),
            source_url: str_field(rec, "source_url").trim().to_string(),
            image_url: str_field(rec, "image_url").trim().to_string(),
            servings,
            total_time: String::new(),
            ingredients,
            steps: text_array(rec.get("instructions"))
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        },
        shopping_items,
        hit,
    }
}

#[async_trait]
impl HouseholdRecipes for HouseholdApps {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<RecipeHit>> {
        let cands = self.candidates().await?;
        Ok(Self::rank(query, cands, limit.max(1)))
    }

    async fn recipe(&self, reference: &str) -> Result<Option<SavedRecipe>> {
        let reference = reference.trim();
        let cands = self.candidates().await?;
        let hit = match cands.iter().find(|(h, _)| h.id == reference) {
            Some((h, _)) => Some(h.clone()),
            None => Self::rank(reference, cands, 1).into_iter().next(),
        };
        let Some(hit) = hit else {
            return Ok(None);
        };
        let raw_id = hit
            .id
            .split_once(':')
            .map(|(_, id)| id.to_string())
            .unwrap_or_default();
        let (rec, items) = match hit.app {
            HouseholdApp::NextHaul => match self.nexthaul_recipe(&raw_id).await? {
                Some(v) => v,
                None => return Ok(None),
            },
            HouseholdApp::Cadora => self.cadora_recipe(&hit, &raw_id).await?,
        };
        Ok(Some(to_saved(hit, &rec, &items)))
    }

    fn sources(&self) -> String {
        let labels: Vec<String> = self
            .linked_apps()
            .into_iter()
            .map(|a| a.label().to_string())
            .collect();
        join_list(&labels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    type Handler = Arc<dyn Fn(&str, &str, &Value) -> (u16, Value) + Send + Sync>;
    /// One recorded request: `(method, path, body, authorization)`.
    type Call = (String, String, Value, String);

    /// A fake API: answers every request via `handler(method, path_and_query, body)`
    /// and records `(method, path, body, authorization)`.
    struct FakeApi {
        base: String,
        log: Arc<StdMutex<Vec<Call>>>,
    }

    impl FakeApi {
        async fn start(handler: Handler) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let log = Arc::new(StdMutex::new(Vec::new()));
            let log2 = log.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    let handler = handler.clone();
                    let log = log2.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut tmp = [0u8; 8192];
                        let (head, body) = loop {
                            let n = sock.read(&mut tmp).await.unwrap_or(0);
                            if n == 0 {
                                return;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                            let text = String::from_utf8_lossy(&buf).to_string();
                            if let Some(i) = text.find("\r\n\r\n") {
                                let head = text[..i].to_string();
                                let len = head
                                    .lines()
                                    .find_map(|l| {
                                        let l = l.to_ascii_lowercase();
                                        l.strip_prefix("content-length:")
                                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                    })
                                    .unwrap_or(0);
                                if buf.len() >= i + 4 + len {
                                    let body = String::from_utf8_lossy(&buf[i + 4..i + 4 + len])
                                        .to_string();
                                    break (head, body);
                                }
                            }
                        };
                        let mut first = head.lines().next().unwrap_or("").split(' ');
                        let method = first.next().unwrap_or("").to_string();
                        let path = first.next().unwrap_or("").to_string();
                        let auth = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .starts_with("authorization:")
                                    .then(|| l[14..].trim().to_string())
                            })
                            .unwrap_or_default();
                        let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                        let (status, resp) = handler(&method, &path, &body);
                        log.lock().unwrap().push((method, path, body, auth));
                        let resp = resp.to_string();
                        let out = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp}",
                            resp.len()
                        );
                        let _ = sock.write_all(out.as_bytes()).await;
                        let _ = sock.flush().await;
                    });
                }
            });
            Self {
                base: format!("http://{addr}"),
                log,
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.log.lock().unwrap().clone()
        }

        fn paths(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .map(|(m, p, _, _)| format!("{m} {}", p.split('?').next().unwrap()))
                .collect()
        }
    }

    fn session(n: u32) -> Value {
        json!({
            "access_token": format!("at{n}"),
            "refresh_token": format!("rt{n}"),
            "expires_at": now_unix() + 900,
            "user": { "id": "user-1" }
        })
    }

    fn hub_for(api: &FakeApi) -> Arc<HouseholdApps> {
        let hub = HouseholdApps::new();
        hub.configure(Some(&format!("{}/cadora", api.base)), None, None, None);
        hub
    }

    #[test]
    fn origin_strips_the_cadora_prefix() {
        assert_eq!(
            origin_of("https://api.getcadora.com/cadora"),
            "https://api.getcadora.com"
        );
        assert_eq!(origin_of("http://127.0.0.1:9"), "http://127.0.0.1:9");
    }

    #[test]
    fn pg_arrays_parse() {
        assert_eq!(
            parse_pg_array(r#"{"Boil the pasta.","Add 2, then stir",Serve}"#),
            vec!["Boil the pasta.", "Add 2, then stir", "Serve"]
        );
        assert_eq!(parse_pg_array("{}"), Vec::<String>::new());
        assert_eq!(parse_pg_array(r#"{"say \"hi\""}"#), vec![r#"say "hi""#]);
        assert_eq!(
            text_array(Some(&json!(["a", "b"]))),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn scoring_prefers_title_matches_and_requires_every_word() {
        assert!(
            match_score("lasagna", "Classic Lasagna", "")
                > match_score("lasagna", "Pasta bake", "lasagna sheets")
        );
        assert_eq!(match_score("beef tacos", "Chicken tacos", "tortillas"), 0);
        assert!(
            match_score("taco", "Fish Tacos", "") > 0,
            "plural/stem match"
        );
        assert!(
            match_score("my lasagna recipe", "Lasagna", "")
                > match_score("lasagna", "Lasagna soup", "")
        );
        assert_eq!(
            match_score("", "Anything", ""),
            1,
            "empty query lists everything"
        );
    }

    #[test]
    fn summaries_read_naturally() {
        let s = added_summary(
            &[
                "flour".into(),
                "eggs".into(),
                "milk".into(),
                "salt".into(),
                "sugar".into(),
                "butter".into(),
            ],
            &["vanilla".into()],
            "NextHaul shopping list",
        );
        assert_eq!(
            s,
            "Added 6 items to the NextHaul shopping list: flour, eggs, milk, salt, and 2 more. \
             Vanilla was already on it, so I bumped the quantity."
        );
        assert_eq!(
            dedupe_items(&["Milk".into(), " milk ".into(), "".into(), "eggs".into()]),
            vec!["Milk", "eggs"]
        );
    }

    #[test]
    fn uuids_are_v4_shaped() {
        let u = new_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert_ne!(new_uuid(), u);
    }

    #[tokio::test]
    async fn link_signs_in_joins_and_mints_a_cadora_voice_link() {
        let api = FakeApi::start(Arc::new(|m, p, b| match (m, p) {
            ("POST", "/identity/auth/anonymous") => (200, session(1)),
            ("POST", "/identity/households/join") => {
                (200, json!({"household":{"id":"h1","name":"The Smiths"},"member":{"id":"m1","role":"adult"}}))
            }
            ("POST", "/cadora/voice/links") => {
                assert_eq!(b["household"], "h1");
                assert_eq!(b["member"], "m1");
                (200, json!({"link":{"id":"l1"},"token":"vl_minted"}))
            }
            _ => (404, json!({"code":"not_found","message":"nope"})),
        }))
        .await;
        let hub = hub_for(&api);
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let seen2 = seen.clone();
        hub.set_identity_sink(Arc::new(move |id| seen2.lock().unwrap().push(id)));

        let out = hub.link(HouseholdApp::Cadora, " 123 456 ").await.unwrap();
        assert_eq!(out.household.id, "h1");
        assert_eq!(out.household.name, "The Smiths");
        assert_eq!(out.household.member_id.as_deref(), Some("m1"));
        assert_eq!(out.voice_token.as_deref(), Some("vl_minted"));
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![CoreIdentity {
                user_id: "user-1".into(),
                refresh_token: "rt1".into()
            }]
        );
        // The join carried the new session's access token.
        let join = api
            .calls()
            .into_iter()
            .find(|c| c.1 == "/identity/households/join")
            .unwrap();
        assert_eq!(join.3, "Bearer at1");
        assert_eq!(join.2["code"], "123456", "spaces are stripped");
        assert_eq!(join.2["member_name"], MEMBER_NAME);

        // A NextHaul link reuses the identity (no second sign-in) and mints nothing.
        let out = hub.link(HouseholdApp::NextHaul, "654321").await;
        assert!(out.is_ok());
        assert_eq!(out.unwrap().voice_token, None);
        let signins = api
            .paths()
            .iter()
            .filter(|p| p.ends_with("/anonymous"))
            .count();
        assert_eq!(signins, 1);
    }

    #[tokio::test]
    async fn a_dead_join_code_says_so() {
        let api = FakeApi::start(Arc::new(|_, p, _| match p {
            "/identity/auth/anonymous" => (200, session(1)),
            _ => (
                400,
                json!({"code":"invalid_code","message":"invalid or expired code"}),
            ),
        }))
        .await;
        let hub = hub_for(&api);
        let err = hub
            .link(HouseholdApp::NextHaul, "000000")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid or has expired"), "{err}");
    }

    #[tokio::test]
    async fn an_expired_token_refreshes_once_and_rotates_through_the_sink() {
        let refreshes = Arc::new(StdMutex::new(0u32));
        let r2 = refreshes.clone();
        let api = FakeApi::start(Arc::new(move |_, p, b| match p {
            "/identity/auth/refresh" => {
                assert_eq!(b["refresh_token"], "rt0");
                *r2.lock().unwrap() += 1;
                (200, session(2))
            }
            _ => (404, json!({})),
        }))
        .await;
        let hub = hub_for(&api);
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let seen2 = seen.clone();
        hub.set_identity_sink(Arc::new(move |id| seen2.lock().unwrap().push(id)));
        let id = CoreIdentity {
            user_id: "user-1".into(),
            refresh_token: "rt0".into(),
        };
        hub.configure(Some(&format!("{}/cadora", api.base)), Some(&id), None, None);

        // Concurrent callers share one refresh (the token is single-use).
        let (a, b) = tokio::join!(hub.access_token(), hub.access_token());
        assert_eq!(a.unwrap(), "at2");
        assert_eq!(b.unwrap(), "at2");
        assert_eq!(*refreshes.lock().unwrap(), 1);
        assert_eq!(seen.lock().unwrap()[0].refresh_token, "rt2");

        // Re-configuring with the (older) persisted identity of the same user keeps
        // the newer in-memory session.
        hub.configure(Some(&format!("{}/cadora", api.base)), Some(&id), None, None);
        assert_eq!(hub.access_token().await.unwrap(), "at2");
        assert_eq!(*refreshes.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn linking_with_a_dead_session_signs_in_again() {
        let api = FakeApi::start(Arc::new(|_, p, _| match p {
            "/identity/auth/refresh" => {
                (400, json!({"code":"invalid_refresh_token","message":"bad"}))
            }
            "/identity/auth/anonymous" => (200, session(5)),
            "/identity/households/join" => (
                200,
                json!({"household":{"id":"h2","name":"Home"},"member":{"id":"m2"}}),
            ),
            _ => (404, json!({})),
        }))
        .await;
        let hub = hub_for(&api);
        let dead = CoreIdentity {
            user_id: "old".into(),
            refresh_token: "dead".into(),
        };
        hub.configure(
            Some(&format!("{}/cadora", api.base)),
            Some(&dead),
            None,
            None,
        );
        let out = hub.link(HouseholdApp::NextHaul, "111111").await.unwrap();
        assert_eq!(out.household.id, "h2");
        let join = api
            .calls()
            .into_iter()
            .find(|c| c.1 == "/identity/households/join")
            .unwrap();
        assert_eq!(join.3, "Bearer at5", "joined with the fresh session");
    }

    #[tokio::test]
    async fn a_revoked_refresh_token_asks_to_relink() {
        let api = FakeApi::start(Arc::new(|_, _, _| {
            (400, json!({"code":"invalid_refresh_token","message":"bad"}))
        }))
        .await;
        let hub = hub_for(&api);
        let id = CoreIdentity {
            user_id: "u".into(),
            refresh_token: "rt".into(),
        };
        hub.configure(Some(&format!("{}/cadora", api.base)), Some(&id), None, None);
        let err = hub.access_token().await.unwrap_err();
        assert!(err.to_string().contains("re-link"), "{err}");
    }

    fn nexthaul_pull() -> Value {
        json!({
            "rows": {
                "recipes": [
                    {"id":"r1","title":"Grandma's Lasagna","description":"Sunday dinner.",
                     "servings":6,"instructions":["Make the sauce.","Layer.","Bake 45 min."],
                     "source_url":"https://x.example/lasagna","image_url":null,"category":"Dinner"},
                    {"id":"r2","title":"Banana Bread","description":null,"servings":null,
                     "instructions":[],"category":"Baking"}
                ],
                "recipe_items": [
                    {"id":"i2","recipe_id":"r1","raw_text":"1 lb ground beef","item":"ground beef","position":1},
                    {"id":"i1","recipe_id":"r1","raw_text":"12 lasagna noodles","item":"lasagna noodles","position":0},
                    {"id":"i3","recipe_id":"r2","raw_text":"3 ripe bananas","item":"bananas","position":0}
                ],
                "list": [
                    {"id":"L2","name":"Costco","type":"shopping","created_at":"2026-05-01T00:00:00Z"},
                    {"id":"L1","name":"Groceries","type":"shopping","created_at":"2026-01-01T00:00:00Z"}
                ],
                "list_item": [
                    {"id":"x1","list_id":"L1","text":"Milk","name_normalized":"milk","quantity":1,
                     "checked":false,"deleted":false,"archived":false},
                    {"id":"x2","list_id":"L1","text":"Eggs","name_normalized":"eggs","quantity":1,
                     "checked":true,"deleted":false,"archived":false}
                ],
                "meal": [{"id":"meal1","title":"ignored"}]
            },
            "removed": {}, "cursor": 10, "epoch": 0, "has_more": false, "reset": true
        })
    }

    fn nexthaul_api() -> Handler {
        Arc::new(|m, p, b| {
            let path = p.split('?').next().unwrap();
            match (m, path) {
                ("POST", "/identity/auth/refresh") => (200, session(1)),
                ("GET", "/nexthaul/sync/pull") => {
                    if p.contains("cursor=0") {
                        (200, nexthaul_pull())
                    } else {
                        (
                            200,
                            json!({"rows":{},"removed":{},"cursor":10,"epoch":0,"has_more":false,"reset":false}),
                        )
                    }
                }
                ("GET", "/nexthaul/catalog") => (
                    200,
                    json!({"items":[
                        {"name":"Ground beef","category":"Meat","search_terms":["minced beef"]}
                    ]}),
                ),
                ("GET", "/nexthaul/prices/categories") => {
                    (200, json!({"categories":{"lasagna noodles":"Pasta"}}))
                }
                ("POST", "/nexthaul/sync/push") => {
                    let results: Vec<Value> = b["mutations"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|m| json!({"mutation_id": m["mutation_id"], "status":"ok"}))
                        .collect();
                    (200, json!({"results": results, "seq": 11}))
                }
                _ => (404, json!({"code":"not_found","message":p})),
            }
        })
    }

    async fn nexthaul_hub(api: &FakeApi) -> Arc<HouseholdApps> {
        let hub = HouseholdApps::new();
        let id = CoreIdentity {
            user_id: "user-1".into(),
            refresh_token: "rt0".into(),
        };
        let hh = LinkedHousehold {
            id: "h1".into(),
            name: "Home".into(),
            member_id: Some("m1".into()),
        };
        hub.configure(
            Some(&format!("{}/cadora", api.base)),
            Some(&id),
            None,
            Some(&hh),
        );
        hub
    }

    #[tokio::test]
    async fn nexthaul_recipes_search_and_load_for_the_screen() {
        let api = FakeApi::start(nexthaul_api()).await;
        let hub = nexthaul_hub(&api).await;
        assert_eq!(hub.sources(), "NextHaul");

        let hits = hub.search("lasagna", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "nexthaul:r1");
        assert_eq!(hits[0].item_count, Some(2));
        // Ingredient words match too ("bananas" is only an ingredient of r2).
        assert_eq!(
            hub.search("bananas", 5).await.unwrap()[0].title,
            "Banana Bread"
        );
        assert_eq!(hub.search("", 5).await.unwrap().len(), 2);
        assert!(hub.search("sushi", 5).await.unwrap().is_empty());

        let saved = hub.recipe("grandma's lasagna").await.unwrap().unwrap();
        assert_eq!(saved.recipe.title, "Grandma's Lasagna");
        assert_eq!(saved.recipe.summary, "Sunday dinner.");
        assert_eq!(saved.recipe.servings, "6 servings");
        assert_eq!(
            saved.recipe.ingredients,
            vec!["12 lasagna noodles", "1 lb ground beef"]
        );
        assert_eq!(saved.recipe.steps.len(), 3);
        assert_eq!(saved.shopping_items, vec!["lasagna noodles", "ground beef"]);
        // By id as well.
        assert_eq!(
            hub.recipe("nexthaul:r2")
                .await
                .unwrap()
                .unwrap()
                .recipe
                .title,
            "Banana Bread"
        );
        assert!(hub.recipe("sushi").await.unwrap().is_none());
        // The cache is fresh: one pull served all of that.
        let pulls = api
            .paths()
            .iter()
            .filter(|p| p.ends_with("/sync/pull"))
            .count();
        assert_eq!(pulls, 1);
    }

    #[tokio::test]
    async fn nexthaul_add_targets_the_oldest_list_and_bumps_live_duplicates() {
        let api = FakeApi::start(nexthaul_api()).await;
        let hub = nexthaul_hub(&api).await;
        let out = hub
            .nexthaul_add(
                &[
                    ("Milk".into(), 1),
                    ("eggs".into(), 2),
                    ("lasagna noodles".into(), 1),
                    ("minced beef".into(), 1),
                    ("Lasagna Noodles".into(), 1),
                ],
                None,
            )
            .await
            .unwrap();
        assert_eq!(out.list_name, "Groceries");
        assert_eq!(out.bumped, vec!["Milk"]);
        assert_eq!(out.added, vec!["eggs", "lasagna noodles", "minced beef"]);

        let push = api
            .calls()
            .into_iter()
            .find(|c| c.1 == "/nexthaul/sync/push")
            .unwrap();
        assert_eq!(push.2["household"], "h1");
        let muts = push.2["mutations"].as_array().unwrap().clone();
        let inserts: Vec<&Value> = muts.iter().filter(|m| m["op"] == "insert").collect();
        assert_eq!(inserts.len(), 3);
        assert!(inserts
            .iter()
            .all(|m| m["table"] == "list_item" && m["fields"]["list_id"] == "L1"));
        // The checked "Eggs" row is not bumped: a fresh row with quantity 2.
        let eggs = inserts
            .iter()
            .find(|m| m["fields"]["text"] == "eggs")
            .unwrap();
        assert_eq!(eggs["fields"]["quantity"], 2);
        // Categories: learned household mapping, then the catalog's search terms.
        let noodles = inserts
            .iter()
            .find(|m| m["fields"]["text"] == "lasagna noodles")
            .unwrap();
        assert_eq!(noodles["fields"]["category"], "Pasta");
        let beef = inserts
            .iter()
            .find(|m| m["fields"]["text"] == "minced beef")
            .unwrap();
        assert_eq!(beef["fields"]["category"], "Meat");
        let patch = muts.iter().find(|m| m["op"] == "patch").unwrap();
        assert_eq!(patch["id"], "x1");
        assert_eq!(patch["fields"]["quantity"], 2);
    }

    #[tokio::test]
    async fn named_lists_are_targeted_and_read_back() {
        let api = FakeApi::start(nexthaul_api()).await;
        let hub = nexthaul_hub(&api).await;
        let list = NextHaulShoppingList::wrap(hub.clone());

        // Reading the default (oldest) list: unchecked items, the checked count and
        // the other lists.
        let said = list.read(None).await.unwrap();
        assert_eq!(
            said,
            "The NextHaul Groceries list has 1 item: Milk. 1 item already checked off. \
             Other lists: Costco."
        );
        assert_eq!(list.read(Some("the grocery list")).await.unwrap(), said);
        assert!(list
            .read(Some("costco list"))
            .await
            .unwrap()
            .starts_with("The NextHaul Costco list is empty."));
        let err = list.read(Some("hardware")).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "there's no list called \"hardware\" — the lists are Groceries and Costco"
        );

        // A named add goes to that list and says so.
        let said = list
            .add("paper towels", None, Some("Costco"))
            .await
            .unwrap();
        assert_eq!(said, "Added paper towels to the NextHaul Costco list.");
        let push = api
            .calls()
            .into_iter()
            .find(|c| c.1 == "/nexthaul/sync/push")
            .unwrap();
        assert_eq!(push.2["mutations"][0]["fields"]["list_id"], "L2");
    }

    #[tokio::test]
    async fn nexthaul_add_creates_a_list_when_the_household_has_none() {
        let api = FakeApi::start(Arc::new(|m, p, b| {
            let path = p.split('?').next().unwrap();
            match (m, path) {
                ("POST", "/identity/auth/refresh") => (200, session(1)),
                ("GET", "/nexthaul/sync/pull") => (200, json!({"rows":{},"removed":{},"cursor":1,"epoch":0,"has_more":false,"reset":true})),
                ("POST", "/nexthaul/sync/push") => {
                    let muts = b["mutations"].as_array().unwrap();
                    assert_eq!(muts[0]["table"], "list");
                    assert_eq!(muts[1]["fields"]["list_id"], muts[0]["id"]);
                    (200, json!({"results":[{"status":"ok"},{"status":"ok"}],"seq":2}))
                }
                _ => (404, json!({})),
            }
        }))
        .await;
        let hub = nexthaul_hub(&api).await;
        let list = NextHaulShoppingList::wrap(hub);
        let speech = list.add("bread", None, None).await.unwrap();
        assert_eq!(speech, "Added bread to the NextHaul Shopping list.");
    }

    #[tokio::test]
    async fn a_rejected_push_is_an_error_not_a_fake_success() {
        let api = FakeApi::start(Arc::new(|m, p, _| {
            let path = p.split('?').next().unwrap();
            match (m, path) {
                ("POST", "/identity/auth/refresh") => (200, session(1)),
                ("GET", "/nexthaul/sync/pull") => (200, nexthaul_pull()),
                ("POST", "/nexthaul/sync/push") => (200, json!({"results":[{"status":"rejected","code":"forbidden","message":"read-only"}],"seq":1})),
                _ => (200, json!({})),
            }
        }))
        .await;
        let hub = nexthaul_hub(&api).await;
        let err = hub
            .nexthaul_add(&[("bread".into(), 1)], None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("forbidden"), "{err}");
    }

    #[tokio::test]
    async fn cadora_recipes_use_detail_and_include_discovery() {
        let api = FakeApi::start(Arc::new(|m, p, _| {
            let path = p.split('?').next().unwrap();
            match (m, path) {
                ("POST", "/identity/auth/refresh") => (200, session(1)),
                ("GET", "/cadora/sync/pull") => (200, json!({"rows":{
                    "recipes":[{"id":"c1","title":"Chili","description":"Game day.","servings":"4",
                                "instructions":"{\"Brown the beef.\",\"Simmer 1 hour.\"}"}],
                    "recipe_items":[{"id":"ci1","recipe_id":"c1","raw_text":"1 lb beef","position":"0"}],
                    "event":[{"id":"e1"}]},
                    "removed":{},"cursor":5,"epoch":0,"has_more":false,"reset":true})),
                ("GET", "/cadora/recipes/discover") => {
                    if p.contains("tier=community") {
                        (200, json!({"recipes":[{"id":"g1","title":"Pad Thai","description":"Noodles.","item_count":9}]}))
                    } else {
                        (200, json!({"recipes":[]}))
                    }
                }
                ("GET", "/cadora/recipes/detail") => {
                    if p.contains("id=c1") {
                        (200, json!({"instructions":["Brown the beef.","Simmer 1 hour."],
                                     "items":[{"raw_text":"1 lb beef","item":"beef","position":0}]}))
                    } else {
                        (200, json!({"instructions":["Soak noodles."],
                                     "items":[{"raw_text":"8 oz rice noodles","item":null,"position":0}]}))
                    }
                }
                _ => (404, json!({})),
            }
        }))
        .await;
        let hub = HouseholdApps::new();
        let id = CoreIdentity {
            user_id: "user-1".into(),
            refresh_token: "rt0".into(),
        };
        let hh = LinkedHousehold {
            id: "h9".into(),
            name: "Home".into(),
            member_id: None,
        };
        hub.configure(
            Some(&format!("{}/cadora", api.base)),
            Some(&id),
            Some(&hh),
            None,
        );

        let all = hub.search("", 10).await.unwrap();
        assert_eq!(all.len(), 2);
        let chili = hub.recipe("chili").await.unwrap().unwrap();
        assert_eq!(chili.hit.tier, RecipeTier::Household);
        assert_eq!(chili.recipe.summary, "Game day.");
        assert_eq!(chili.recipe.servings, "4 servings");
        assert_eq!(
            chili.recipe.steps,
            vec!["Brown the beef.", "Simmer 1 hour."]
        );
        assert_eq!(chili.shopping_items, vec!["beef"]);
        let pad = hub.recipe("pad thai").await.unwrap().unwrap();
        assert_eq!(pad.hit.tier, RecipeTier::Community);
        assert_eq!(pad.recipe.summary, "Noodles.");
        assert_eq!(pad.shopping_items, vec!["8 oz rice noodles"]);
    }

    #[tokio::test]
    async fn voice_shopping_adds_many_items_one_by_one() {
        struct Rec(StdMutex<Vec<String>>);
        #[async_trait]
        impl CadoraController for Rec {
            async fn command(&self, cmd: CadoraCommand) -> Result<String> {
                if let CadoraCommand::AddItem { item, .. } = cmd {
                    if item == "saffron" {
                        bail!("nope");
                    }
                    self.0.lock().unwrap().push(item);
                }
                Ok("ok".into())
            }
        }
        let rec = Arc::new(Rec(StdMutex::new(Vec::new())));
        let list = VoiceShoppingList::wrap(rec.clone());
        let speech = list
            .add_many(
                &[
                    "rice".into(),
                    "saffron".into(),
                    "Rice".into(),
                    "peas".into(),
                ],
                None,
            )
            .await
            .unwrap();
        assert_eq!(*rec.0.lock().unwrap(), vec!["rice", "peas"]);
        assert_eq!(
            speech,
            "Added 2 items to the Cadora shopping list: rice and peas. 1 item couldn't be added."
        );
    }
}
