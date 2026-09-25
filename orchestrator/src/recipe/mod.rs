//! Recipe fetch + parse, exposed to the LLM as the `recipe_lookup` tool (wired in
//! `llm/rig.rs`) and rendered on the device as a 3-tab "recipe mode" screen.
//!
//! Flow: the user asks by dish name ("show me a recipe for carbonara"); the tool
//! searches the web for a source page (reusing the Tavily key the web-search tool
//! already uses), fetches that page's HTML, and parses a structured [`Recipe`] out
//! of its `schema.org/Recipe` **JSON-LD** — the machine-readable recipe block that
//! essentially every mainstream recipe site embeds. The parsed recipe is pushed to
//! the display as an `ambient-recipe` device action (see `llm/mod.rs`,
//! `DeviceAction::ShowRecipe`), and the tool returns a short spoken confirmation.
//!
//! The provider is abstracted behind the [`RecipeProvider`] trait so the tool is
//! testable offline (fixture HTML, no network), mirroring how
//! [`crate::directions::DirectionsProvider`] backs the directions tool. Parsing is a
//! set of pure functions ([`parse_recipe_html`] et al.) unit-tested directly against
//! fixture pages.
//!
//! **JSON-LD is the v1 parser.** A page with no usable `schema.org/Recipe` JSON-LD
//! yields a graceful "couldn't read the recipe" the model speaks. An LLM-based
//! fallback (hand the page text to a model to structure) is the planned next step —
//! see `plans/RecipePlan.md` — deferred here because wiring a parse-time model into
//! the tool set introduces a construction cycle worth designing deliberately.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A parsed recipe, structured for the display's Overview / Ingredients / Steps
/// tabs. All fields are plain strings / string lists so the type is `Eq` (it rides
/// inside [`crate::llm::DeviceAction`]) and marshals trivially to the device as
/// JSON. Optional fields are the empty string / empty list when absent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    /// The dish name (`schema.org` `name`).
    pub title: String,
    /// A one-line description (`description`), empty when the page has none.
    pub summary: String,
    /// The page the recipe was parsed from.
    pub source_url: String,
    /// A hero image URL (`image`), empty when none.
    pub image_url: String,
    /// Servings / yield as a speakable phrase (e.g. "4 servings"), empty when none.
    pub servings: String,
    /// Total time as a speakable phrase (e.g. "1 hour 25 minutes"), empty when none.
    pub total_time: String,
    /// Ingredient lines (`recipeIngredient`), in listed order.
    pub ingredients: Vec<String>,
    /// Instruction steps (`recipeInstructions`), flattened in order.
    pub steps: Vec<String>,
}

impl Recipe {
    /// Whether this parse is usable: it has a title and at least ingredients or
    /// steps. A JSON-LD block that yields nothing beyond a name is treated as a miss.
    fn is_usable(&self) -> bool {
        !self.title.trim().is_empty() && (!self.ingredients.is_empty() || !self.steps.is_empty())
    }
}

/// A source of recipes. Abstracted so the tool is testable offline and a second
/// parsing strategy (an LLM fallback, a dedicated extraction API) can be dropped in
/// behind the same seam later.
#[async_trait]
pub trait RecipeProvider: Send + Sync {
    /// Find (when `url` is `None`) and fetch + parse a recipe for `dish`. When `url`
    /// is given, fetch and parse that exact page instead of searching.
    async fn lookup(&self, dish: &str, url: Option<&str>) -> Result<Recipe>;
}

/// A [`RecipeProvider`] that discovers a source page via the **Tavily** search API
/// (the same key the web-search tool uses) and parses its `schema.org/Recipe`
/// JSON-LD. Needs a Tavily API key; without one the `recipe_lookup` tool is not
/// advertised (see [`from_search`]).
pub struct TavilyRecipeProvider {
    client: reqwest::Client,
    tavily_base: String,
    tavily_key: String,
}

impl TavilyRecipeProvider {
    pub fn new(tavily_key: impl Into<String>) -> Self {
        Self::with_base_url("https://api.tavily.com", tavily_key)
    }

    /// Point Tavily at a specific API root (overridable for tests).
    pub fn with_base_url(base_url: impl Into<String>, tavily_key: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(StdDuration::from_secs(12))
            .user_agent("ambient-orchestrator/0.1 (recipe)")
            .build()
            .unwrap_or_default();
        Self {
            client,
            tavily_base: base_url.into().trim_end_matches('/').to_string(),
            tavily_key: tavily_key.into(),
        }
    }

    /// Ask Tavily for the best source URL for "<dish> recipe".
    async fn search_url(&self, dish: &str) -> Result<String> {
        let body = serde_json::json!({
            "api_key": self.tavily_key,
            "query": format!("{dish} recipe"),
            "max_results": 5,
            "search_depth": "basic",
        });
        let value: Value = self
            .client
            .post(format!("{}/search", self.tavily_base))
            .json(&body)
            .send()
            .await
            .context("POST Tavily /search for a recipe URL")?
            .error_for_status()
            .context("Tavily returned an error status")?
            .json()
            .await
            .context("parsing Tavily JSON")?;
        value
            .get("results")
            .and_then(Value::as_array)
            .and_then(|rs| rs.iter().find_map(|r| r.get("url").and_then(Value::as_str)))
            .map(str::to_string)
            .with_context(|| format!("no recipe source found for \"{dish}\""))
    }

    /// Fetch a page's HTML.
    async fn fetch_html(&self, url: &str) -> Result<String> {
        self.client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET recipe page {url}"))?
            .error_for_status()
            .with_context(|| format!("recipe page {url} returned an error status"))?
            .text()
            .await
            .with_context(|| format!("reading recipe page {url}"))
    }
}

#[async_trait]
impl RecipeProvider for TavilyRecipeProvider {
    async fn lookup(&self, dish: &str, url: Option<&str>) -> Result<Recipe> {
        let url = match url {
            Some(u) if !u.trim().is_empty() => u.trim().to_string(),
            _ => self.search_url(dish).await?,
        };
        let html = self.fetch_html(&url).await?;
        parse_recipe_html(&html, &url).with_context(|| {
            format!("found a page for \"{dish}\" but couldn't read a recipe from it")
        })
    }
}

/// Build the recipe provider from the web-search config. Returns `None` (tool not
/// advertised) unless the Tavily provider is selected with a usable key — recipe
/// lookup needs real web search to discover a source page, so it rides the same
/// Tavily key as the web-search tool (the keyless DuckDuckGo Instant Answer API
/// returns no source URLs). Mirrors [`crate::directions::from_token`].
pub fn from_search(provider: &str, api_key: Option<&str>) -> Option<Arc<dyn RecipeProvider>> {
    if provider.trim().to_lowercase() != "tavily" {
        return None;
    }
    let key = api_key.map(str::trim).filter(|s| !s.is_empty())?;
    log::info!("recipe_lookup enabled (Tavily source search + JSON-LD parse)");
    Some(Arc::new(TavilyRecipeProvider::new(key)))
}

// ===========================================================================
// JSON-LD parsing (pure, unit-tested against fixture HTML)
// ===========================================================================

/// Parse a `schema.org/Recipe` out of a page's embedded JSON-LD. Returns `None`
/// when the page has no usable recipe block. `source_url` is stamped onto the
/// result so the display can link back.
pub fn parse_recipe_html(html: &str, source_url: &str) -> Option<Recipe> {
    for block in extract_jsonld_blocks(html) {
        let Ok(value) = serde_json::from_str::<Value>(&block) else {
            continue;
        };
        if let Some(node) = find_recipe_node(&value) {
            let recipe = recipe_from_node(node, source_url);
            if recipe.is_usable() {
                return Some(recipe);
            }
        }
    }
    None
}

/// Pull the raw text of every `<script type="application/ld+json">…</script>` block
/// out of an HTML document. Deliberately regex/parser-free (no HTML crate): it scans
/// for the script tags on an ASCII-lowercased copy, whose byte offsets line up with
/// the original (ASCII-only case folding preserves length), and slices the original
/// so the JSON keeps its exact bytes.
fn extract_jsonld_blocks(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(rel) = lower[i..].find("<script") {
        let tag_start = i + rel;
        // End of the opening tag.
        let Some(gt_rel) = lower[tag_start..].find('>') else {
            break;
        };
        let content_start = tag_start + gt_rel + 1;
        let opening = &lower[tag_start..content_start];
        if opening.contains("application/ld+json") {
            if let Some(close_rel) = lower[content_start..].find("</script>") {
                let content_end = content_start + close_rel;
                out.push(html[content_start..content_end].trim().to_string());
                i = content_end + "</script>".len();
                continue;
            }
            break;
        }
        i = content_start;
    }
    out
}

/// Find the first node whose `@type` is (or includes) `Recipe`, descending into
/// arrays and `@graph` containers (both common JSON-LD shapes).
fn find_recipe_node(value: &Value) -> Option<&Value> {
    match value {
        Value::Array(items) => items.iter().find_map(find_recipe_node),
        Value::Object(map) => {
            if type_includes(value, "Recipe") {
                return Some(value);
            }
            if let Some(graph) = map.get("@graph") {
                return find_recipe_node(graph);
            }
            None
        }
        _ => None,
    }
}

/// Whether a node's `@type` equals `wanted` — `@type` may be a string or an array
/// of strings.
fn type_includes(node: &Value, wanted: &str) -> bool {
    match node.get("@type") {
        Some(Value::String(s)) => s == wanted,
        Some(Value::Array(items)) => items.iter().any(|v| v.as_str() == Some(wanted)),
        _ => false,
    }
}

/// Map a `schema.org/Recipe` node into a [`Recipe`].
fn recipe_from_node(node: &Value, source_url: &str) -> Recipe {
    Recipe {
        title: string_field(node, "name"),
        summary: string_field(node, "description"),
        source_url: source_url.to_string(),
        image_url: first_image(node.get("image")),
        servings: map_yield(node.get("recipeYield")),
        total_time: node
            .get("totalTime")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_duration)
            .map(humanize_secs)
            .unwrap_or_default(),
        ingredients: string_list(node.get("recipeIngredient"))
            .or_else(|| string_list(node.get("ingredients")))
            .unwrap_or_default(),
        steps: collect_steps(node.get("recipeInstructions")),
    }
}

/// A trimmed string field, empty when absent/non-string.
fn string_field(node: &Value, key: &str) -> String {
    node.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Extract a hero image URL from `image`, which may be a string, an `ImageObject`
/// (`{ "url": … }`), or an array of either.
fn first_image(value: Option<&Value>) -> String {
    fn one(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(_) => v.get("url").and_then(Value::as_str).map(str::to_string),
            Value::Array(items) => items.iter().find_map(one),
            _ => None,
        }
    }
    value.and_then(one).unwrap_or_default()
}

/// Map `recipeYield` (a number, a string like "4 servings", or an array) into a
/// speakable phrase. A bare number becomes "N servings".
fn map_yield(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => format!("{n} servings"),
        Some(Value::Array(items)) => items
            .iter()
            .find_map(|v| match v {
                Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
                Value::Number(n) => Some(format!("{n} servings")),
                _ => None,
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// A list of trimmed strings from a string-or-array field, or `None` when absent.
fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    match value? {
        Value::String(s) => Some(vec![s.trim().to_string()]),
        Value::Array(items) => {
            let list: Vec<String> = items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            (!list.is_empty()).then_some(list)
        }
        _ => None,
    }
}

/// Flatten `recipeInstructions` into ordered step strings. Handles the four shapes
/// real sites use: a single newline-delimited string; an array of plain strings; an
/// array of `HowToStep` objects (`{ "text": … }`); and `HowToSection` objects whose
/// `itemListElement` holds the steps.
fn collect_steps(value: Option<&Value>) -> Vec<String> {
    fn push_from(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => {
                for line in s.split('\n') {
                    let line = line.trim();
                    if !line.is_empty() {
                        out.push(line.to_string());
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    push_from(item, out);
                }
            }
            Value::Object(_) => {
                if type_includes(v, "HowToSection") {
                    if let Some(items) = v.get("itemListElement") {
                        push_from(items, out);
                    }
                } else if let Some(text) = v
                    .get("text")
                    .or_else(|| v.get("name"))
                    .and_then(Value::as_str)
                {
                    let text = text.trim();
                    if !text.is_empty() {
                        out.push(text.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Some(v) = value {
        push_from(v, &mut out);
    }
    out
}

/// Parse an ISO-8601 duration (`PT1H25M`, `PT45M`, `PT30S`, `P0DT0H25M`) into whole
/// seconds. Only the time components (H/M/S) are used; a leading day count is
/// ignored (recipes never exceed a day). Returns `None` when unparseable.
fn parse_iso8601_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    if !s.starts_with('P') {
        return None;
    }
    // Only look at the time portion (after 'T').
    let time = s.split_once('T').map(|(_, t)| t)?;
    let mut secs: u64 = 0;
    let mut num = String::new();
    let mut saw_any = false;
    for ch in time.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else {
            let n: u64 = num.parse().ok()?;
            num.clear();
            match ch {
                'H' => secs += n * 3600,
                'M' => secs += n * 60,
                'S' => secs += n,
                _ => return None,
            }
            saw_any = true;
        }
    }
    (saw_any && num.is_empty()).then_some(secs)
}

/// Render whole seconds as a short, speakable phrase ("25 minutes",
/// "1 hour 25 minutes", "45 seconds").
fn humanize_secs(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let mut parts = Vec::new();
    let unit = |n: u64, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    if h > 0 {
        parts.push(unit(h, "hour"));
    }
    if m > 0 {
        parts.push(unit(m, "minute"));
    }
    if s > 0 && h == 0 {
        parts.push(unit(s, "second"));
    }
    parts.join(" ")
}

/// A short spoken confirmation for the model to relay once a recipe is on screen.
pub fn render_confirmation(recipe: &Recipe) -> String {
    let mut out = format!("Here's a recipe for {}", recipe.title);
    let mut extras = Vec::new();
    if !recipe.servings.is_empty() {
        extras.push(recipe.servings.clone());
    }
    if !recipe.total_time.is_empty() {
        extras.push(recipe.total_time.clone());
    }
    if !extras.is_empty() {
        out.push_str(&format!(" — {}", extras.join(", ")));
    }
    out.push_str(". It's on the screen now.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const JSONLD_PAGE: &str = r##"<!doctype html><html><head>
        <script type="application/ld+json">
        {"@context":"https://schema.org","@type":"Recipe",
         "name":"Spaghetti Carbonara",
         "description":"A Roman classic.",
         "image":["https://img.example/carbonara.jpg"],
         "recipeYield":"4 servings",
         "totalTime":"PT25M",
         "recipeIngredient":["200g spaghetti","2 eggs","100g pancetta"],
         "recipeInstructions":[
            {"@type":"HowToStep","text":"Boil the pasta."},
            {"@type":"HowToStep","text":"Fry the pancetta."},
            {"@type":"HowToStep","text":"Toss with egg off the heat."}]}
        </script></head><body>ignored</body></html>"##;

    #[test]
    fn iso8601_durations() {
        assert_eq!(parse_iso8601_duration("PT25M"), Some(1500));
        assert_eq!(parse_iso8601_duration("PT1H30M"), Some(5400));
        assert_eq!(parse_iso8601_duration("P0DT0H25M0S"), Some(1500));
        assert_eq!(parse_iso8601_duration("PT45S"), Some(45));
        assert_eq!(parse_iso8601_duration("garbage"), None);
    }

    #[test]
    fn humanize() {
        assert_eq!(humanize_secs(1500), "25 minutes");
        assert_eq!(humanize_secs(5400), "1 hour 30 minutes");
        assert_eq!(humanize_secs(3600), "1 hour");
        assert_eq!(humanize_secs(0), "");
    }

    #[test]
    fn parses_howtostep_jsonld() {
        let r = parse_recipe_html(JSONLD_PAGE, "https://example.com/carbonara").unwrap();
        assert_eq!(r.title, "Spaghetti Carbonara");
        assert_eq!(r.summary, "A Roman classic.");
        assert_eq!(r.source_url, "https://example.com/carbonara");
        assert_eq!(r.image_url, "https://img.example/carbonara.jpg");
        assert_eq!(r.servings, "4 servings");
        assert_eq!(r.total_time, "25 minutes");
        assert_eq!(r.ingredients.len(), 3);
        assert_eq!(
            r.steps,
            vec![
                "Boil the pasta.",
                "Fry the pancetta.",
                "Toss with egg off the heat."
            ]
        );
    }

    #[test]
    fn parses_graph_and_string_instructions_and_yield_number() {
        let page = r##"<script type="application/ld+json">
        {"@context":"https://schema.org","@graph":[
          {"@type":"WebPage","name":"blog"},
          {"@type":["Recipe","Thing"],"name":"Pancakes",
           "image":{"url":"https://img/pan.jpg"},
           "recipeYield":6,
           "recipeIngredient":["flour","milk","egg"],
           "recipeInstructions":"Mix.\nCook.\nServe."}]}
        </script>"##;
        let r = parse_recipe_html(page, "u").unwrap();
        assert_eq!(r.title, "Pancakes");
        assert_eq!(r.image_url, "https://img/pan.jpg");
        assert_eq!(r.servings, "6 servings");
        assert_eq!(r.steps, vec!["Mix.", "Cook.", "Serve."]);
        assert_eq!(r.total_time, "");
    }

    #[test]
    fn parses_howtosection() {
        let page = r##"<script type="application/ld+json">
        {"@type":"Recipe","name":"Cake",
         "recipeIngredient":["flour"],
         "recipeInstructions":[
           {"@type":"HowToSection","itemListElement":[
             {"@type":"HowToStep","text":"Prep."},
             {"@type":"HowToStep","text":"Bake."}]}]}
        </script>"##;
        let r = parse_recipe_html(page, "u").unwrap();
        assert_eq!(r.steps, vec!["Prep.", "Bake."]);
    }

    #[test]
    fn no_recipe_block_is_a_miss() {
        assert!(parse_recipe_html("<html><body>no recipe here</body></html>", "u").is_none());
        // A Recipe with only a name (no ingredients/steps) is not usable.
        let thin = r##"<script type="application/ld+json">{"@type":"Recipe","name":"x"}</script>"##;
        assert!(parse_recipe_html(thin, "u").is_none());
    }

    #[test]
    fn confirmation_mentions_title_and_facts() {
        let r = parse_recipe_html(JSONLD_PAGE, "u").unwrap();
        let c = render_confirmation(&r);
        assert!(c.contains("Spaghetti Carbonara"));
        assert!(c.contains("4 servings"));
        assert!(c.contains("25 minutes"));
    }

    /// Serve one raw HTTP response on a fresh loopback port, then close.
    fn serve_once(content_type: &str, body: String) -> (String, tokio::task::JoinHandle<()>) {
        let ct = content_type.to_string();
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let listener = TcpListener::from_std(std_listener).unwrap();
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                ct, body.len(), body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn fetches_explicit_url_and_parses() {
        // With an explicit url, lookup skips search and fetches+parses that page.
        let (base, server) = serve_once("text/html", JSONLD_PAGE.to_string());
        let provider = TavilyRecipeProvider::with_base_url("http://unused.invalid", "tok");
        let recipe = provider
            .lookup("carbonara", Some(&format!("{base}/carbonara")))
            .await
            .unwrap();
        assert_eq!(recipe.title, "Spaghetti Carbonara");
        assert_eq!(recipe.ingredients.len(), 3);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn search_url_reads_first_tavily_result() {
        let (base, server) = serve_once(
            "application/json",
            r#"{"results":[{"url":"https://site/best-carbonara"},{"url":"https://other"}]}"#
                .to_string(),
        );
        let provider = TavilyRecipeProvider::with_base_url(base, "tok");
        let url = provider.search_url("carbonara").await.unwrap();
        assert_eq!(url, "https://site/best-carbonara");
        server.await.unwrap();
    }
}
