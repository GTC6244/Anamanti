//! Display-screen tools: the recipe screen (`recipe_lookup`, `recipe_control`,
//! `close_recipe`), the weather screen (`weather_lookup`, `close_weather`) and the
//! place card (`places_lookup`, `close_places`), and the music player (`music_screen`,
//! screen-only — playback is `spotify_control`). Each fetches through an injected
//! provider (testable offline), pushes the result to the display as a
//! [`DeviceAction`], and returns a short spoken confirmation.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Local;
use rig_core::completion::ToolDefinition;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{close_tool, definition, emit, parse_args, ToolCtx, VoiceTool};
use crate::cadora::{HouseholdRecipes, RecipeHit, RecipeTier, SavedRecipe, ShoppingList};
use crate::directions::LiveHomeLocation;
use crate::llm::{ActionSink, DeviceAction, MusicNav, RecipeNav};
use crate::music::{MusicDisplayRouter, MusicDisplayStyle};
use crate::recipe::RecipeProvider;

/// Tool name: show a recipe on the display.
pub(super) const RECIPE_LOOKUP: &str = "recipe_lookup";
/// Tool name: dismiss the recipe screen.
const CLOSE_RECIPE: &str = "close_recipe";
/// Tool name: navigate the already-open recipe screen (switch tab / scroll).
const RECIPE_CONTROL: &str = "recipe_control";
/// Tool name: the household's saved recipes (Cadora / NextHaul).
pub(super) const SAVED_RECIPES: &str = "saved_recipes";
/// Tool name: show the weather forecast on the display.
pub(super) const WEATHER_LOOKUP: &str = "weather_lookup";
/// Tool name: dismiss the weather screen.
const CLOSE_WEATHER: &str = "close_weather";
/// Tool name: look up a place and show its card on the display.
pub(super) const PLACES_LOOKUP: &str = "places_lookup";
/// Tool name: dismiss the place card.
const CLOSE_PLACES: &str = "close_places";
/// Tool name: drive the on-screen music player (show now-playing / up-next / close).
pub(super) const MUSIC_SCREEN: &str = "music_screen";

// ---------------------------------------------------------------------------
// Recipe
// ---------------------------------------------------------------------------

/// `recipe_lookup` (web recipes). `saved` = the household's saved recipes are also
/// available, which changes its guidance (saved recipes are checked first).
pub(super) fn recipe_lookup(provider: Arc<dyn RecipeProvider>, saved: bool) -> Arc<dyn VoiceTool> {
    Arc::new(RecipeLookup { provider, saved })
}

/// `close_recipe` and `recipe_control`: present whenever any recipe source is.
pub(super) fn recipe_screen_tools() -> Vec<Arc<dyn VoiceTool>> {
    vec![
        close_tool(
            CLOSE_RECIPE,
            "Close the recipe screen on the display and return to the idle screen. \
             Use when the user says they're done cooking or asks to close/dismiss the \
             recipe.",
            DeviceAction::DismissRecipe,
            "the recipe",
            "Okay, closing the recipe.",
        ),
        Arc::new(RecipeControl),
    ]
}

/// The recipe-screen half of the recipe guidance (navigation, questions, close),
/// shared by whichever recipe tool carries the guidance.
macro_rules! recipe_screen_guidance {
    () => {
        "When a recipe is already on screen (the turn \
         context will tell you, including which tab, scroll position and highlighted \
         step, plus the full ingredient list and steps), use `recipe_control` to \
         navigate it — switching to the Overview / Ingredients / Steps tab or scrolling \
         the current tab whenever the user says things like \"show the ingredients\", \
         \"go to the steps\", \"scroll down\", \"scroll back up\", or \"start over\". \
         For a specific step (\"go to step 4\", \"next step\", \"previous step\", \
         \"what's step 3\") ALWAYS use `recipe_control` `go_to_step` with the step \
         number — never scroll to find a step. Answer questions about the recipe on \
         screen (\"how many tomatoes do I need?\", \"how long does it bake?\") directly \
         from the ingredients and steps in the turn context — do not look the recipe \
         up again. After a `go_to_step`, read that step to the user word for word from \
         the numbered steps in the turn context (\"Step 4. Simmer for ten minutes.\"). \
         Use `close_recipe` when they say they're done cooking or ask to close the \
         recipe. Relay each tool's short spoken confirmation."
    };
}

/// `recipe_lookup`'s guidance when it is the only recipe source.
const RECIPE_GUIDANCE_WEB_ONLY: &str = concat!(
    "You can pull up cooking recipes on the display with the `recipe_lookup` tool. \
     Whenever the user asks for a recipe, how to make or cook a dish, or to show a \
     recipe for something, you MUST call `recipe_lookup` with the dish name — never \
     recite a full recipe as prose. ",
    recipe_screen_guidance!()
);

/// `recipe_lookup`'s guidance when the household's saved recipes come first (the
/// screen guidance then rides `saved_recipes`).
const RECIPE_GUIDANCE_WITH_SAVED: &str =
    "Use `recipe_lookup` to find a recipe online and show it on the display — but only \
     after `saved_recipes` found no saved recipe for the dish, or when the user asks for \
     a new or online recipe.";

/// Typed arguments for `recipe_lookup`.
#[derive(Debug, Deserialize)]
struct RecipeArgs {
    /// The dish to find a recipe for (e.g. "carbonara"). Required.
    dish: String,
    /// A specific recipe URL to use instead of searching (optional; the model
    /// usually only has a dish name).
    #[serde(default)]
    url: Option<String>,
}

/// Finds a source page for a dish, parses it, and pushes the structured recipe to the
/// display ([`DeviceAction::ShowRecipe`]).
struct RecipeLookup {
    provider: Arc<dyn RecipeProvider>,
    /// `saved_recipes` is also advertised (check saved recipes first).
    saved: bool,
}

#[async_trait]
impl VoiceTool for RecipeLookup {
    fn name(&self) -> &'static str {
        RECIPE_LOOKUP
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            RECIPE_LOOKUP,
            "Find a cooking recipe for a dish and show it on the display's \
             recipe screen (Overview / Ingredients / Steps tabs). Use whenever \
             the user asks for a recipe, how to make/cook a dish, or to pull up \
             a recipe. Returns a short spoken confirmation to relay.",
            json!({
                "type": "object",
                "properties": {
                    "dish": {
                        "type": "string",
                        "description": "The dish to find a recipe for, e.g. \"carbonara\" or \
                                        \"chocolate chip cookies\"."
                    },
                    "url": {
                        "type": "string",
                        "description": "A specific recipe page URL to use instead of searching \
                                        (optional)."
                    }
                },
                "required": ["dish"]
            }),
        )
    }

    /// Covers the whole recipe screen (lookup, navigation, close).
    fn guidance(&self) -> Option<&'static str> {
        Some(if self.saved {
            RECIPE_GUIDANCE_WITH_SAVED
        } else {
            RECIPE_GUIDANCE_WEB_ONLY
        })
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: RecipeArgs = parse_args(args, RECIPE_LOOKUP)?;
        let sink = ctx.require_sink("no display is connected to show a recipe on right now")?;
        let recipe = self
            .provider
            .lookup(&args.dish, args.url.as_deref())
            .await?;
        let confirmation = crate::recipe::render_confirmation(&recipe);
        emit(
            Some(sink),
            DeviceAction::ShowRecipe(recipe),
            "the recipe could show",
        )?;
        Ok(confirmation)
    }
}

/// Typed arguments for `recipe_control`.
#[derive(Debug, Deserialize)]
struct RecipeControlArgs {
    /// One of the fixed navigation actions.
    action: String,
    /// The 1-based step number for `go_to_step`.
    #[serde(default)]
    step: Option<u32>,
}

/// Navigate the open recipe screen ([`DeviceAction::RecipeControl`]).
struct RecipeControl;

#[async_trait]
impl VoiceTool for RecipeControl {
    fn name(&self) -> &'static str {
        RECIPE_CONTROL
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            RECIPE_CONTROL,
            "Navigate the recipe screen that is already open on the display: switch \
             which tab is showing, scroll the current tab, or jump to a specific step \
             (which is scrolled into view and highlighted). Use ONLY when a recipe \
             is currently on screen (the turn context says so). To close it, use \
             close_recipe instead.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "show_overview",
                            "show_ingredients",
                            "show_steps",
                            "scroll_up",
                            "scroll_down",
                            "scroll_top",
                            "scroll_bottom",
                            "go_to_step"
                        ],
                        "description": "The navigation action: switch to the Overview / Ingredients / \
                                        Steps tab, scroll the current tab up/down a page or to the \
                                        top/bottom, or `go_to_step` to show and highlight one step. \
                                        For \"go to step 4\", \"next step\", \"previous step\" or \
                                        \"what's step 3\" ALWAYS use go_to_step (never scroll) — work \
                                        out the number from the highlighted step in the turn context."
                    },
                    "step": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The 1-based step number. Required for go_to_step; ignored otherwise."
                    }
                },
                "required": ["action"]
            }),
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: RecipeControlArgs = parse_args(args, RECIPE_CONTROL)?;
        let (nav, confirmation) = match args.action.as_str() {
            "show_overview" => (RecipeNav::TabOverview, "Showing the overview.".to_string()),
            "show_ingredients" => (
                RecipeNav::TabIngredients,
                "Showing the ingredients.".to_string(),
            ),
            "show_steps" => (RecipeNav::TabSteps, "Showing the steps.".to_string()),
            "scroll_up" => (RecipeNav::ScrollUp, "Scrolling up.".to_string()),
            "scroll_down" => (RecipeNav::ScrollDown, "Scrolling down.".to_string()),
            "scroll_top" => (RecipeNav::ScrollTop, "Back to the top.".to_string()),
            "scroll_bottom" => (
                RecipeNav::ScrollBottom,
                "Jumping to the bottom.".to_string(),
            ),
            "go_to_step" => match args.step.filter(|&n| n >= 1) {
                Some(n) => (
                    RecipeNav::Step(n),
                    format!(
                        "Step {n} is now shown and highlighted. Read step {n} to the user word \
                         for word from the recipe steps in the turn context, starting \
                         \"Step {n}.\" (if the context has no text for it, just say \
                         \"Here's step {n}.\")."
                    ),
                ),
                None => anyhow::bail!("recipe_control go_to_step needs a step number of 1 or more"),
            },
            other => anyhow::bail!("unknown recipe_control action `{other}`"),
        };
        emit(
            ctx.actions,
            DeviceAction::RecipeControl(nav),
            "the recipe could update",
        )?;
        Ok(confirmation)
    }
}

// ---------------------------------------------------------------------------
// Saved recipes (the household's Cadora / NextHaul recipes)
// ---------------------------------------------------------------------------

/// The opening of the `saved_recipes` guidance, by (web recipes present, shopping
/// list present). Each variant ends with the recipe-screen guidance, which this tool
/// carries whenever it is advertised.
macro_rules! saved_recipes_guidance {
    ($first:expr, $shopping:expr) => {
        concat!(
            "The household's own saved recipes (from their NextHaul / Cadora apps) are \
             available through the `saved_recipes` tool. ",
            $first,
            " Use `search` when the user asks which recipes they have saved or to find \
             saved recipes (\"what chicken recipes do we have?\"), and `ingredients` to read \
             out what a saved recipe needs without showing it. ",
            $shopping,
            "Never recite a full recipe as prose. ",
            recipe_screen_guidance!()
        )
    };
}

macro_rules! saved_first_then_web {
    () => {
        "Whenever the user asks for a recipe, how to make or \
     cook a dish, or to show a recipe, FIRST call `saved_recipes` with action `show` and the \
     dish as `recipe`; only if it reports that no saved recipe matches (or they ask for a new \
     or online recipe) call `recipe_lookup`."
    };
}
macro_rules! saved_only {
    () => {
        "Whenever the user asks for a recipe, how to make or cook a dish, \
     or to show a recipe, call `saved_recipes` with action `show` and the dish as `recipe`; \
     if it reports that no saved recipe matches, say so and offer to search their saved \
     recipes."
    };
}
macro_rules! saved_shopping {
    () => {
        "To put a recipe's ingredients on the shopping list use \
     `add_ingredients_to_list` with the recipe (pass `items` to add only some of them) — \
     never call `shopping_list_add` once per ingredient. "
    };
}

/// `saved_recipes`. `shopping` enables `add_ingredients_to_list`; `web` = `recipe_lookup`
/// is the fallback for dishes the household hasn't saved.
pub(super) fn saved_recipes(
    recipes: Arc<dyn HouseholdRecipes>,
    shopping: Option<Arc<dyn ShoppingList>>,
    web: bool,
) -> Arc<dyn VoiceTool> {
    Arc::new(SavedRecipes {
        recipes,
        shopping,
        web,
    })
}

/// How many hits `search` reports.
const SAVED_SEARCH_LIMIT: usize = 8;

/// Typed arguments for `saved_recipes`.
#[derive(Debug, Deserialize)]
struct SavedRecipesArgs {
    action: String,
    /// Words to search saved recipes for (`search`).
    #[serde(default)]
    query: Option<String>,
    /// A recipe id from `search`, or a dish name / title.
    #[serde(default)]
    recipe: Option<String>,
    /// `add_ingredients_to_list`: only these ingredients (default: all).
    #[serde(default)]
    items: Option<Vec<String>>,
}

struct SavedRecipes {
    recipes: Arc<dyn HouseholdRecipes>,
    shopping: Option<Arc<dyn ShoppingList>>,
    web: bool,
}

/// "from NextHaul" / "from Cadora's community recipes".
fn recipe_source(hit: &RecipeHit) -> String {
    match hit.tier {
        RecipeTier::Household => format!("from {}", hit.app.label()),
        RecipeTier::Community => format!("from {}'s community recipes", hit.app.label()),
        RecipeTier::Global => format!("from {}'s recipe collection", hit.app.label()),
    }
}

impl SavedRecipes {
    /// The recipe the args name (`recipe`, else `query`), or a "no match" error the
    /// model relays (and, with web recipes, falls back on).
    async fn resolve(&self, args: &SavedRecipesArgs) -> Result<SavedRecipe> {
        let reference = args
            .recipe
            .as_deref()
            .or(args.query.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("say which recipe (a dish name or a recipe id from search)")?;
        match self.recipes.recipe(reference).await? {
            Some(r) => Ok(r),
            None if self.web => anyhow::bail!(
                "No saved recipe matches \"{reference}\" in {}. Use recipe_lookup to find one \
                 online.",
                self.recipes.sources()
            ),
            None => anyhow::bail!(
                "No saved recipe matches \"{reference}\" in {}.",
                self.recipes.sources()
            ),
        }
    }

    async fn run(&self, args: SavedRecipesArgs, ctx: &ToolCtx<'_>) -> Result<String> {
        match args.action.as_str() {
            "search" => {
                let query = args
                    .query
                    .as_deref()
                    .or(args.recipe.as_deref())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let hits = self.recipes.search(&query, SAVED_SEARCH_LIMIT).await?;
                if hits.is_empty() {
                    return Ok(if query.is_empty() {
                        format!(
                            "There are no saved recipes in {} yet.",
                            self.recipes.sources()
                        )
                    } else {
                        format!(
                            "No saved recipes match \"{query}\" in {}.",
                            self.recipes.sources()
                        )
                    });
                }
                let lines: Vec<String> = hits
                    .iter()
                    .enumerate()
                    .map(|(i, h)| {
                        let count = h
                            .item_count
                            .filter(|n| *n > 0)
                            .map(|n| format!(", {n} ingredients"))
                            .unwrap_or_default();
                        format!(
                            "{}. {} ({}{count}) [id {}]",
                            i + 1,
                            h.title,
                            recipe_source(h),
                            h.id
                        )
                    })
                    .collect();
                Ok(format!(
                    "Saved recipes{}: {} — Tell the user the titles (not the ids); pass an id \
                     as `recipe` to show one.",
                    if query.is_empty() {
                        String::new()
                    } else {
                        format!(" matching \"{query}\"")
                    },
                    lines.join("; ")
                ))
            }
            "show" => {
                let sink =
                    ctx.require_sink("no display is connected to show a recipe on right now")?;
                let saved = self.resolve(&args).await?;
                let mut confirmation = format!(
                    "Here's the saved recipe for {} {}",
                    saved.recipe.title,
                    recipe_source(&saved.hit)
                );
                if !saved.recipe.servings.is_empty() {
                    confirmation.push_str(&format!(" — {}", saved.recipe.servings));
                }
                confirmation.push_str(". It's on the screen now.");
                if saved.recipe.steps.is_empty() {
                    confirmation.push_str(" It has no written steps, just the ingredients.");
                }
                emit(
                    Some(sink),
                    DeviceAction::ShowRecipe(saved.recipe),
                    "the recipe could show",
                )?;
                Ok(confirmation)
            }
            "ingredients" => {
                let saved = self.resolve(&args).await?;
                if saved.recipe.ingredients.is_empty() {
                    return Ok(format!(
                        "The saved recipe for {} has no ingredient list.",
                        saved.recipe.title
                    ));
                }
                Ok(format!(
                    "{} ({}) needs: {}.",
                    saved.recipe.title,
                    recipe_source(&saved.hit),
                    saved.recipe.ingredients.join("; ")
                ))
            }
            "add_ingredients_to_list" => {
                let shopping = self
                    .shopping
                    .as_ref()
                    .context("no shopping list is linked")?;
                let saved = self.resolve(&args).await?;
                let items: Vec<String> = match args.items.filter(|v| !v.is_empty()) {
                    Some(only) => only,
                    None => saved.shopping_items.clone(),
                };
                if items.is_empty() {
                    anyhow::bail!(
                        "the saved recipe for {} has no ingredients to add",
                        saved.recipe.title
                    );
                }
                shopping.add_many(&items).await.with_context(|| {
                    format!("adding the {} ingredients failed", saved.recipe.title)
                })
            }
            other => anyhow::bail!("unknown saved_recipes action `{other}`"),
        }
    }
}

#[async_trait]
impl VoiceTool for SavedRecipes {
    fn name(&self) -> &'static str {
        SAVED_RECIPES
    }

    fn definition(&self) -> ToolDefinition {
        let mut actions = vec!["search", "show", "ingredients"];
        if self.shopping.is_some() {
            actions.push("add_ingredients_to_list");
        }
        let mut description = String::from(
            "The household's saved recipes from their NextHaul / Cadora apps. `search` finds \
             saved recipes by words in the title or ingredients (an empty query lists recent \
             ones); `show` puts a saved recipe on the display's recipe screen (Overview / \
             Ingredients / Steps); `ingredients` returns a saved recipe's ingredient list to \
             read out",
        );
        if self.shopping.is_some() {
            description.push_str(
                "; `add_ingredients_to_list` puts a saved recipe's ingredients (or just the \
                 named `items`) on the household shopping list",
            );
        }
        description.push_str(". Returns a short result to relay.");
        definition(
            SAVED_RECIPES,
            &description,
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": actions,
                        "description": "What to do."
                    },
                    "query": {
                        "type": "string",
                        "description": "For search: words to look for, e.g. \"chicken\" or \"lasagna\"."
                    },
                    "recipe": {
                        "type": "string",
                        "description": "For show / ingredients / add_ingredients_to_list: a recipe id \
                                        from a search result, or the dish name, e.g. \"lasagna\"."
                    },
                    "items": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "For add_ingredients_to_list: only these ingredients (omit to \
                                        add them all)."
                    }
                },
                "required": ["action"]
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(match (self.web, self.shopping.is_some()) {
            (true, true) => saved_recipes_guidance!(saved_first_then_web!(), saved_shopping!()),
            (true, false) => saved_recipes_guidance!(saved_first_then_web!(), ""),
            (false, true) => saved_recipes_guidance!(saved_only!(), saved_shopping!()),
            (false, false) => saved_recipes_guidance!(saved_only!(), ""),
        })
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: SavedRecipesArgs = parse_args(args, SAVED_RECIPES)?;
        self.run(args, ctx).await
    }
}

// ---------------------------------------------------------------------------
// Music player
// ---------------------------------------------------------------------------

/// The `music_screen` tool. Advertised wherever `spotify_control` is (screen-only;
/// transport stays on `spotify_control`). `display` (when present) records a
/// `fullscreen` / `compact` request as the speaking display's choice for this music.
pub(super) fn music_screen(display: Option<Arc<MusicDisplayRouter>>) -> Arc<dyn VoiceTool> {
    Arc::new(MusicScreen { display })
}

/// Typed arguments for `music_screen`.
#[derive(Debug, Deserialize)]
struct MusicScreenArgs {
    /// One of the fixed screen actions.
    action: String,
}

/// Show / switch / close the on-screen music player ([`DeviceAction::MusicScreen`]).
/// The device decides whether anything is playing.
struct MusicScreen {
    display: Option<Arc<MusicDisplayRouter>>,
}

#[async_trait]
impl VoiceTool for MusicScreen {
    fn name(&self) -> &'static str {
        MUSIC_SCREEN
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            MUSIC_SCREEN,
            "Control what the music player shows on the display (Spotify and radio share \
             it): open the full now-playing screen, open the up-next queue, close the \
             player back to the ambient screen, or switch which widget THIS display shows \
             while the current music plays — `fullscreen` (the big full-screen player) or \
             `compact` (the small bar). Use this for requests like \"show me \
             what's playing\", \"show the queue / what's up next\", \"hide the music\", \
             \"make the music full screen\" or \"use the small music player\". This only \
             changes the SCREEN — to play/pause/skip/change volume use spotify_control \
             instead.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["show", "show_queue", "close", "fullscreen", "compact"],
                        "description": "The screen action: `show` opens the now-playing view, \
                                        `show_queue` opens the up-next list, `close` returns to \
                                        the ambient display, `fullscreen` / `compact` switch \
                                        the widget this display shows while music plays."
                    }
                },
                "required": ["action"]
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You can also control what the music player SHOWS on the display with the \
             `music_screen` tool (separate from `spotify_control`, which changes playback). \
             Call `music_screen` with action=show when the user says things like \"show me \
             what's playing\" or \"pull up the music\", action=show_queue for \"what's up \
             next\" or \"show the queue\", and action=close for \"hide the music\" or \"close \
             the player\". To change which music widget this display shows while the music \
             plays, call it with action=fullscreen (\"make the music full screen\", \"use the \
             big player\") or action=compact (\"use the small music player\", \"shrink the \
             music\"); it applies to this display only (each display chooses separately) \
             until the music stops or something new is played. The turn context will \
             tell you when the music screen is already open and what's on it. Relay its short \
             spoken confirmation.",
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: MusicScreenArgs = parse_args(args, MUSIC_SCREEN)?;
        let (action, confirmation) = match args.action.as_str() {
            "show" => (
                DeviceAction::MusicScreen(MusicNav::NowPlaying),
                "Here's what's playing.",
            ),
            "show_queue" => (
                DeviceAction::MusicScreen(MusicNav::Queue),
                "Here's what's up next.",
            ),
            "close" => (
                DeviceAction::MusicScreen(MusicNav::Close),
                "Closing the music player.",
            ),
            "fullscreen" => (
                DeviceAction::MusicDisplayStyle(MusicDisplayStyle::Fullscreen),
                "Okay, the music is full screen.",
            ),
            "compact" => (
                DeviceAction::MusicDisplayStyle(MusicDisplayStyle::Compact),
                "Okay, I'll use the small music player.",
            ),
            other => anyhow::bail!("unknown music_screen action `{other}`"),
        };
        emit(ctx.actions, action.clone(), "the music screen could update")?;
        // A style switch is this display's choice for the current music: record it so its
        // now-playing pushes agree with the frame it is about to receive.
        if let (DeviceAction::MusicDisplayStyle(style), Some(display), Some(id)) =
            (&action, &self.display, ctx.device_id)
        {
            display.choose(id, *style);
        }
        Ok(confirmation.to_string())
    }
}

// ---------------------------------------------------------------------------
// Weather
// ---------------------------------------------------------------------------

/// `weather_lookup` and `close_weather`, in advertised order.
pub(super) fn weather_tools(
    provider: Arc<dyn crate::weather::WeatherProvider>,
    home_location: LiveHomeLocation,
    imperial: bool,
) -> Vec<Arc<dyn VoiceTool>> {
    vec![
        Arc::new(WeatherLookup {
            provider,
            home_location,
            imperial,
        }),
        close_tool(
            CLOSE_WEATHER,
            "Close the weather screen on the display and return to the idle screen. \
             Use when the user asks to close/dismiss the weather.",
            DeviceAction::DismissWeather,
            "the weather",
            "Okay, closing the weather.",
        ),
    ]
}

/// Typed arguments for `weather_lookup`.
#[derive(Debug, Deserialize)]
struct WeatherArgs {
    /// A place to get the weather for (optional; defaults to the household home
    /// location when omitted, so "what's the weather" works with no place named).
    #[serde(default)]
    location: Option<String>,
    /// When to get the forecast for (optional; defaults to right now). Accepts
    /// `today`/`now`, `tomorrow`, a weekday name ("Saturday"), or `YYYY-MM-DD`.
    #[serde(default)]
    when: Option<String>,
    /// Which display layout to show (optional; defaults to the hourly view). `"hourly"`
    /// shows the big-conditions panel + 10-hour row; `"week"` shows the separate 7-day
    /// forecast widget (7 columns of daily highs/lows).
    #[serde(default)]
    layout: Option<String>,
}

/// Fetches current conditions + a 7-day forecast for a place (defaulting to the
/// household home location) and pushes it to the display
/// ([`DeviceAction::ShowWeather`]).
struct WeatherLookup {
    provider: Arc<dyn crate::weather::WeatherProvider>,
    home_location: LiveHomeLocation,
    imperial: bool,
}

#[async_trait]
impl VoiceTool for WeatherLookup {
    fn name(&self) -> &'static str {
        WEATHER_LOOKUP
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            WEATHER_LOOKUP,
            "Get the weather and show it on the display's weather screen. Use \
             whenever the user asks about the weather, temperature, forecast, or \
             how hot/cold/rainy it is. If the user names no place, omit `location` \
             — it defaults to home. For a future day (e.g. \"weather on \
             Saturday\", \"will it rain tomorrow\") pass `when`; omit it for right \
             now. Set `layout` to \"week\" for a multi-day/weekly request (e.g. \
             \"7-day forecast\", \"what's the week look like\", \"forecast for the \
             rest of the week\") to show the 7-day view; otherwise omit it for the \
             default hourly view. Returns a short spoken confirmation to relay.",
            json!({
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "The place to get the weather for, e.g. \"Paris\" or \
                                        \"Denver, Colorado\". Omit for the user's home location."
                    },
                    "when": {
                        "type": "string",
                        "description": "The day to forecast: \"today\"/\"now\", \"tomorrow\", a \
                                        weekday name like \"Saturday\", or a date \"YYYY-MM-DD\". \
                                        Omit for right now."
                    },
                    "layout": {
                        "type": "string",
                        "enum": ["hourly", "week"],
                        "description": "The display layout: \"hourly\" (default) for the \
                                        big-conditions panel + 10-hour row, or \"week\" for the \
                                        7-day forecast view. Use \"week\" for weekly/multi-day \
                                        requests; omit otherwise."
                    }
                }
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You can pull up the weather on the display with the `weather_lookup` tool. \
             Whenever the user asks about the weather, the temperature, the forecast, or \
             whether it will rain/snow/be hot or cold, you MUST call `weather_lookup` (omit \
             `location` to use home) and relay its short spoken confirmation — never guess at \
             conditions or say you can't check the weather. When they ask about a future day \
             (\"tomorrow\", \"this weekend\", \"Saturday\"), pass `when`. Use `close_weather` \
             when they ask to close/dismiss the weather.",
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: WeatherArgs = parse_args(args, WEATHER_LOOKUP)?;
        let sink = ctx.require_sink("no display is connected to show the weather on right now")?;
        // An omitted/empty `location` means "use home" — the device's own location, which
        // gets the plain confirmation phrasing (no place name, no "on the screen" trailer).
        let explicit = args
            .location
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let is_home = explicit.is_none();
        let location = explicit
            .or_else(|| self.home_location.weather())
            .context("no location was given and no home location is set")?;
        let when = crate::weather::resolve_when(args.when.as_deref(), Local::now())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut report = self.provider.fetch(&location, self.imperial, when).await?;
        // A "week" request switches the display to the 7-day forecast widget; anything
        // else (including an omitted/unknown value) keeps the default hourly view.
        if args
            .layout
            .as_deref()
            .map(str::trim)
            .is_some_and(|l| l.eq_ignore_ascii_case("week"))
        {
            report.layout = "week".to_string();
        }
        let confirmation = crate::weather::render_confirmation(&report, is_home);
        emit(
            Some(sink),
            DeviceAction::ShowWeather(report),
            "the weather could show",
        )?;
        Ok(confirmation)
    }
}

// ---------------------------------------------------------------------------
// Places
// ---------------------------------------------------------------------------

/// `places_lookup` and `close_places`, in advertised order.
pub(super) fn places_tools(
    provider: Arc<dyn crate::places::PlacesProvider>,
    home_location: LiveHomeLocation,
) -> Vec<Arc<dyn VoiceTool>> {
    vec![
        Arc::new(PlacesLookup {
            provider,
            home_location,
        }),
        close_tool(
            CLOSE_PLACES,
            "Close the place card on the display and return to the idle screen. \
             Use when the user asks to close/dismiss the place.",
            DeviceAction::DismissPlace,
            "the place",
            "Okay, closing that.",
        ),
    ]
}

/// Typed arguments for `places_lookup`.
#[derive(Debug, Deserialize)]
struct PlacesArgs {
    /// The place to look up, e.g. "the Louvre" or "coffee shop downtown". Optional only
    /// so the model can re-call with just a `place_id` after disambiguating.
    #[serde(default)]
    query: Option<String>,
    /// A specific candidate id from a prior ambiguous lookup. When set, the tool skips
    /// the search and shows that exact place.
    #[serde(default)]
    place_id: Option<String>,
}

/// Text-searches for a business/point of interest (biased toward home) and — when the
/// match is unambiguous — pushes its card to the display ([`DeviceAction::ShowPlace`]).
/// When several candidates match it returns a spoken list **without** showing
/// anything, so the model asks which one and re-calls with the chosen `place_id`.
struct PlacesLookup {
    provider: Arc<dyn crate::places::PlacesProvider>,
    home_location: LiveHomeLocation,
}

impl PlacesLookup {
    /// Fetch a place's details, push the card, and return the spoken confirmation.
    async fn show(&self, place_id: &str, sink: &ActionSink) -> Result<String> {
        let report = self.provider.details(place_id).await?;
        let confirmation = crate::places::render_confirmation(&report);
        emit(
            Some(sink),
            DeviceAction::ShowPlace(report),
            "the place could show",
        )?;
        Ok(confirmation)
    }
}

#[async_trait]
impl VoiceTool for PlacesLookup {
    fn name(&self) -> &'static str {
        PLACES_LOOKUP
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            PLACES_LOOKUP,
            "Look up a business or point of interest (its address, opening \
             hours, rating, phone number, website) and show a card for it on \
             the display. Use whenever the user asks where a place is, when \
             it's open, its hours/phone/rating, or to tell them about a \
             specific business, restaurant, shop, landmark, or attraction. \
             Pass the user's phrasing as `query`. If the tool returns several \
             options, ask the user which one they mean, then call again with \
             the same `query` plus the chosen `place_id`. Returns a short \
             spoken line to relay.",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The place to look up, e.g. \"the Louvre\" or \
                                        \"coffee shop downtown\". Include any location the \
                                        user mentioned."
                    },
                    "place_id": {
                        "type": "string",
                        "description": "Only when re-calling after the user picked one of \
                                        the options this tool returned: copy that option's \
                                        EXACT place_id from the list (an opaque id like \
                                        \"ChIJ…\" or \"places/…\"). Never invent or guess \
                                        an id from the name/address."
                    }
                },
                "required": ["query"]
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You can pull up information about a place on the display with the \
             `places_lookup` tool. Whenever the user asks where a business or landmark is, \
             when it's open, its hours, phone number, rating, or to tell them about a \
             specific restaurant, shop, cafe, or attraction, you MUST call `places_lookup` \
             with their phrasing as `query` — never guess at an address or hours. If it \
             returns several options, ask the user which one they mean and call again with \
             the same `query` plus the chosen `place_id`. Relay the tool's short spoken \
             confirmation. Use `close_places` when they ask to close/dismiss it.",
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: PlacesArgs = parse_args(args, PLACES_LOOKUP)?;
        let sink = ctx.require_sink("no display is connected to show a place on right now")?;

        // Second call: the model pinned a specific candidate → show it directly.
        if let Some(place_id) = args
            .place_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return self.show(place_id, sink).await;
        }

        let query = args
            .query
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .context("no place to look up was given")?;
        let bias = self.home_location.full();
        let candidates = self.provider.search(&query, bias.as_deref()).await?;
        match candidates.as_slice() {
            [] => Ok(format!("I couldn't find anything matching \"{query}\".")),
            [only] => self.show(&only.place_id, sink).await,
            // Ambiguous: ask which one (no card shown yet). The model re-calls with a
            // `place_id` from this list.
            many => Ok(crate::places::render_candidates(many)),
        }
    }
}

// ---------------------------------------------------------------------------
// Camera (plans/CameraPlan.md)
// ---------------------------------------------------------------------------

pub(super) const SHOW_CAMERA: &str = "show_camera";
const CLOSE_CAMERA: &str = "close_camera";

/// `show_camera` and `close_camera`, in advertised order. The LLM fallback for wordings
/// the camera fast path (`orchestrator::camera`) doesn't catch.
pub(super) fn camera_tools(hub: Arc<crate::camera::CameraHub>) -> Vec<Arc<dyn VoiceTool>> {
    vec![
        Arc::new(ShowCamera { hub }),
        close_tool(
            CLOSE_CAMERA,
            "Close the live camera view on the display and return to the idle screen. \
             Use when the user asks to close/dismiss/stop the camera.",
            DeviceAction::DismissCamera,
            "the camera",
            "Okay.",
        ),
    ]
}

/// Typed arguments for `show_camera`.
#[derive(Debug, Default, Deserialize)]
struct ShowCameraArgs {
    /// The camera's name; omitted/blank = the first configured camera.
    #[serde(default)]
    camera: Option<String>,
}

/// Opens the live view of a configured camera on the display the user is talking to
/// ([`DeviceAction::ShowCamera`]). The frame names the camera only; the display fetches
/// the stream itself.
struct ShowCamera {
    hub: Arc<crate::camera::CameraHub>,
}

impl ShowCamera {
    fn names(&self) -> Vec<String> {
        self.hub.cameras().iter().map(|c| c.name.clone()).collect()
    }
}

#[async_trait]
impl VoiceTool for ShowCamera {
    fn name(&self) -> &'static str {
        SHOW_CAMERA
    }

    fn definition(&self) -> ToolDefinition {
        let names = self.names().join(", ");
        definition(
            SHOW_CAMERA,
            &format!(
                "Show a live security-camera view on the display. Use when the user \
                 asks to see a camera, who is at the door, or what is happening \
                 outside. Available cameras: {names}. You cannot see the video \
                 yourself — never describe what the camera shows."
            ),
            json!({
                "type": "object",
                "properties": {
                    "camera": {
                        "type": "string",
                        "description": format!(
                            "Which camera to show; one of: {names}. Omit for the first one."
                        )
                    }
                }
            }),
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: ShowCameraArgs = super::parse_args_or_default(args, SHOW_CAMERA)?;
        let sink = ctx.require_sink("no display is connected to show the camera on right now")?;
        let want = args.camera.unwrap_or_default();
        let Some(cam) = self.hub.resolve(&want) else {
            return Ok(format!(
                "There's no camera called \"{}\". The cameras are: {}.",
                want.trim(),
                self.names().join(", ")
            ));
        };
        emit(
            Some(sink),
            DeviceAction::ShowCamera {
                name: cam.name.clone(),
            },
            "the camera could show",
        )?;
        Ok(format!("Here's the {}.", cam.name))
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{home, StaticWeather};
    use super::*;

    #[tokio::test]
    async fn show_camera_resolves_names_and_emits_only_the_name() {
        let tools = camera_tools(crate::camera::tests::hub());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let said = tools[0]
            .invoke(&json!({ "camera": "back yard" }), &ToolCtx::new(Some(&tx)))
            .await
            .unwrap();
        assert_eq!(said, "Here's the Back yard.");
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::ShowCamera {
                name: "Back yard".into()
            }
        );
        // Omitted → the first camera; null args are accepted.
        tools[0]
            .invoke(&Value::Null, &ToolCtx::new(Some(&tx)))
            .await
            .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::ShowCamera {
                name: "Front Door".into()
            }
        );
        // Unknown camera → a spoken list, nothing shown.
        let said = tools[0]
            .invoke(&json!({ "camera": "garage" }), &ToolCtx::new(Some(&tx)))
            .await
            .unwrap();
        assert!(said.contains("Front Door, Back yard"), "{said}");
        assert!(rx.try_recv().is_err());
        // The advertised definition lists the cameras but never the password.
        let def = serde_json::to_string(&tools[0].definition().parameters).unwrap()
            + &tools[0].definition().description;
        assert!(def.contains("Front Door") && !def.contains("p@ss"));
    }

    fn weather_tool() -> Arc<dyn VoiceTool> {
        weather_tools(Arc::new(StaticWeather), home("Austin, TX"), true).remove(0)
    }

    #[tokio::test]
    async fn weather_lookup_defaults_to_hourly_layout_and_passes_daily_through() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        weather_tool()
            .invoke(&json!({}), &ToolCtx::new(Some(&tx)))
            .await
            .unwrap();
        match rx.try_recv().unwrap() {
            DeviceAction::ShowWeather(report) => {
                // No `layout` argument → the default hourly view, at home.
                assert_eq!(report.layout, "");
                assert_eq!(report.location_label, "Austin, TX");
                // The provider's daily forecast rides through for the 7-day widget.
                assert_eq!(report.daily.len(), 1);
                assert_eq!(report.daily[0].high, 80);
            }
            other => panic!("expected ShowWeather, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn weather_lookup_week_layout_selects_the_7_day_widget() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        weather_tool()
            .invoke(&json!({ "layout": "Week" }), &ToolCtx::new(Some(&tx)))
            .await
            .unwrap();
        match rx.try_recv().unwrap() {
            // Case-insensitive: "Week" → the 7-day layout.
            DeviceAction::ShowWeather(report) => assert_eq!(report.layout, "week"),
            other => panic!("expected ShowWeather, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn display_tools_check_for_a_display_before_fetching() {
        let err = weather_tool()
            .invoke(&json!({}), &ToolCtx::new(None))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "no display is connected to show the weather on right now"
        );
    }

    #[tokio::test]
    async fn recipe_control_maps_actions_and_rejects_unknown_ones() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx::new(Some(&tx));
        let out = RecipeControl
            .invoke(&json!({ "action": "show_steps" }), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "Showing the steps.");
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::RecipeControl(RecipeNav::TabSteps)
        );
        let err = RecipeControl
            .invoke(&json!({ "action": "dance" }), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "unknown recipe_control action `dance`");
    }

    #[tokio::test]
    async fn recipe_control_go_to_step_needs_a_positive_step() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx::new(Some(&tx));
        let out = RecipeControl
            .invoke(&json!({ "action": "go_to_step", "step": 4 }), &ctx)
            .await
            .unwrap();
        assert!(out.starts_with("Step 4 is now shown"), "{out}");
        assert!(out.contains("Read step 4 to the user"), "{out}");
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::RecipeControl(RecipeNav::Step(4))
        );
        for bad in [
            json!({ "action": "go_to_step" }),
            json!({ "action": "go_to_step", "step": 0 }),
        ] {
            assert!(RecipeControl.invoke(&bad, &ctx).await.is_err(), "{bad}");
        }
        assert!(rx.try_recv().is_err(), "a rejected step emits nothing");
    }

    #[tokio::test]
    async fn music_screen_maps_actions_and_rejects_unknown_ones() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx::new(Some(&tx));
        let tool = MusicScreen { display: None };
        for (action, nav) in [
            ("show", MusicNav::NowPlaying),
            ("show_queue", MusicNav::Queue),
            ("close", MusicNav::Close),
        ] {
            tool.invoke(&json!({ "action": action }), &ctx)
                .await
                .unwrap();
            assert_eq!(rx.try_recv().unwrap(), DeviceAction::MusicScreen(nav));
        }
        for (action, style) in [
            ("fullscreen", MusicDisplayStyle::Fullscreen),
            ("compact", MusicDisplayStyle::Compact),
        ] {
            tool.invoke(&json!({ "action": action }), &ctx)
                .await
                .unwrap();
            assert_eq!(
                rx.try_recv().unwrap(),
                DeviceAction::MusicDisplayStyle(style)
            );
        }
        let err = tool
            .invoke(&json!({ "action": "teleport" }), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "unknown music_screen action `teleport`");
        // With no display attached a valid action reports so (and emits nothing).
        assert!(tool
            .invoke(&json!({ "action": "show" }), &ToolCtx::new(None))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn music_screen_style_is_the_speaking_displays_choice_only() {
        let router = Arc::new(MusicDisplayRouter::new());
        let tool = MusicScreen {
            display: Some(router.clone()),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx {
            actions: Some(&tx),
            device_id: Some("den"),
        };
        tool.invoke(&json!({ "action": "fullscreen" }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::MusicDisplayStyle(MusicDisplayStyle::Fullscreen)
        );
        let pins = crate::music::MusicDisplayPins::new();
        assert_eq!(
            router.style_for("den", &pins),
            MusicDisplayStyle::Fullscreen
        );
        assert_eq!(
            router.style_for("kitchen", &pins),
            MusicDisplayStyle::Compact
        );
        // Screen navigation is not a style choice.
        tool.invoke(&json!({ "action": "show" }), &ctx)
            .await
            .unwrap();
        assert!(!router.has_choice("kitchen"));
    }
}
