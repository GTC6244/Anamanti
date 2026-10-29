//! Runtime-swappable orchestrator settings (Plan.MD Phase 6).
//!
//! Phase 4 bound the LLM backend and Piper voice once, from the environment. Phase
//! 6 makes them **changeable at runtime** from the on-device settings screen: the
//! device sends a project-local Wyoming control frame (see `wyoming::protocol`
//! `ambient-set-settings`) and the orchestrator rebuilds the selected backend and
//! swaps it in place, without a restart.
//!
//! [`SharedSettings`] holds the live [`RuntimeSettings`] behind an `RwLock` so the
//! (cheaply cloneable) [`crate::orchestrator::Pipeline`] takes a fresh snapshot at
//! the start of each turn while a concurrent control request can swap the backend.
//! An [`LlmFactory`] carries the immutable credentials/endpoints needed to
//! (re)build any backend from a `(backend, model)` pair, so a swap never has to
//! re-read the environment.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::appsaid::{AppSaidClient, PhoneMessenger};
use crate::cadora::{normalize_base_url, CadoraController, CadoraVoiceApi};
use crate::calendar::CalendarSource;
use crate::directions::DirectionsProvider;
use crate::llm::anthropic_auth::{AnthropicAuth, AnthropicTokenProvider};
use crate::llm::{
    anthropic::AnthropicBackend, mock::MockLlm, ollama::OllamaBackend, openai::OpenAiBackend,
    LlmBackend,
};
use crate::music::{
    MusicDisplayPins, MusicDisplayRouter, MusicDisplayStyle, RadioCatalog, RadioPlayer,
    RoutedSpotify, SpotifyController, SpotifyWebApi,
};

/// Which implementation drives the local/cloud LLM backends: the hand-rolled HTTP
/// clients, or the rig-core agent framework (selected at runtime with
/// `llm.engine="rig"`; required for the tool set, including web search).
///
/// The default is [`LlmEngine::Rig`], matching the config-file default
/// (`llm.engine` unset ⇒ rig).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LlmEngine {
    /// Hand-rolled `ollama.rs` / `anthropic.rs` / `openai.rs` HTTP clients: plain
    /// streamed text — no tools and no follow-up history (warned at build time).
    Native,
    /// rig-core agents (`llm.engine="rig"`), always compiled in: every backend
    /// (Ollama, Anthropic API key or subscription, OpenAI) with the full tool set.
    #[default]
    Rig,
}

impl LlmEngine {
    /// Canonical lowercase label.
    pub fn as_str(self) -> &'static str {
        match self {
            LlmEngine::Native => "native",
            LlmEngine::Rig => "rig",
        }
    }

    /// Parse a label; anything other than `rig` is `Native`.
    pub fn from_label(label: &str) -> Self {
        match label.to_lowercase().as_str() {
            "rig" | "rig-core" | "rigcore" => LlmEngine::Rig,
            _ => LlmEngine::Native,
        }
    }
}

/// Default trailing-silence (ms) the energy VAD waits for after speech before
/// finalizing the STT transcript. A mild reduction from the historical 900 ms to
/// cut end-of-turn latency; A/B-tunable from the device settings screen.
pub const DEFAULT_END_SILENCE_MS: u64 = 700;

/// Default RMS (i16 units) above which an incoming chunk counts as speech rather than
/// room noise. This is a compromise across units, expected to be tuned per-device
/// (A/B-tunable from the device settings screen / config page). Too low (the original 120)
/// sits inside some Echo Shows' far-field idle noise floor (~100–200 i16), so every frame
/// reads as speech, end-of-speech never fires, and the turn stalls. Too high (450) discards
/// genuine but quiet far-field speech: on-device QA found a unit whose speech ran below 450,
/// so real utterances were dropped as "silence" and the turn returned nothing. 180 clears
/// the low end of the noise-floor range while still catching quieter speech; raise it in a
/// noisy room, lower it for a very quiet mic.
pub const DEFAULT_VOICE_RMS_THRESHOLD: f64 = 180.0;

/// Default Silero speech-probability gate (0.0..1.0). Used when neither the config
/// file's `vad.silero.threshold` nor a persisted value is present. A frame counts as
/// speech when its Silero probability is `>= this`.
pub const DEFAULT_SILERO_THRESHOLD: f32 = 0.5;

fn default_end_silence_ms() -> u64 {
    DEFAULT_END_SILENCE_MS
}

fn default_voice_rms_threshold() -> f64 {
    DEFAULT_VOICE_RMS_THRESHOLD
}

fn default_anthropic_auth() -> String {
    AnthropicAuth::ApiKey.as_str().to_string()
}

/// True when an optional string holds a non-blank value. The one "is this secret /
/// field set?" test used by every view and status readout.
pub fn is_set(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|s| !s.trim().is_empty())
}

/// A copy of `v` with blank values normalized to `None`.
fn non_blank(v: &Option<String>) -> Option<String> {
    v.clone().filter(|s| !s.trim().is_empty())
}

/// Overlay a persisted optional value onto its boot seed (env secret or config-file
/// value). The persisted file distinguishes three states:
/// - **absent / `null`** ⇒ never set on the page: keep the seed (so adding an env key
///   later still takes effect, and older files keep the env fallback);
/// - **`""`** ⇒ explicitly **cleared** on the page: stays cleared across restarts even
///   when the seed is present;
/// - **a value** ⇒ that value wins.
pub fn overlay_seeded(seed: Option<String>, persisted: Option<String>) -> Option<String> {
    match persisted {
        None => seed,
        Some(v) if v.trim().is_empty() => None,
        Some(v) => Some(v),
    }
}

/// The persisted form of a seeded optional value — the inverse of [`overlay_seeded`].
/// A set value is written as-is. An unset value is written as `""` (the "cleared"
/// marker) only when the boot seed had a value, so the clear survives a restart;
/// otherwise it is written as `null`, which keeps the seed fallback open.
fn persist_seeded(live: &Option<String>, seed: Option<&Option<String>>) -> Option<String> {
    if is_set(live) {
        return live.clone();
    }
    match seed {
        Some(s) if is_set(s) => Some(String::new()),
        _ => None,
    }
}

/// Google Drive photo-slideshow credentials + linkage, owned by the orchestrator.
///
/// The interim idle-screen photo source is Google Drive (`drive.readonly`). The
/// orchestrator is the single source of truth: it runs the one-time OAuth consent
/// (see [`crate::drive_consent`], driven from the config page) and holds the
/// "Desktop app" OAuth **client id/secret** plus the resulting **refresh token**
/// and the chosen **folder ids**. The device pulls this whole bundle over Wyoming
/// (`ambient-get-drive-token`) and mints Drive access tokens on-device — so the
/// tablet APK ships with no baked-in credentials.
///
/// Plaintext secrets (0600 settings file); keep on a trusted machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DriveConfig {
    /// "Desktop app" OAuth client id (loopback consent). `None`/empty = unset.
    #[serde(default)]
    pub client_id: Option<String>,
    /// "Desktop app" OAuth client secret. `None`/empty = unset.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Long-lived refresh token minted by the consent flow. `None` = not linked.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Drive folder ids the slideshow reads images from.
    #[serde(default)]
    pub folder_ids: Vec<String>,
    /// OAuth scope granted (informational; `drive.readonly` by default).
    #[serde(default)]
    pub scope: Option<String>,
}

impl DriveConfig {
    /// True when both OAuth client credentials are present — i.e. the device can
    /// refresh Drive access tokens. Mirrors the old build-time `kGoogleDriveConfigured`.
    pub fn configured(&self) -> bool {
        is_set(&self.client_id) && is_set(&self.client_secret)
    }

    /// True when configured *and* a refresh token exists — Drive is fully linked.
    pub fn linked(&self) -> bool {
        self.configured() && is_set(&self.refresh_token)
    }
}

/// A requested change to the Drive config. Absent fields are left unchanged; a
/// `Some(None)` clears a value, `Some(Some(v))` sets it (empty strings clear).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DriveUpdate {
    pub client_id: Option<Option<String>>,
    pub client_secret: Option<Option<String>>,
    pub refresh_token: Option<Option<String>>,
    pub folder_ids: Option<Vec<String>>,
    pub scope: Option<Option<String>>,
}

/// A person who lives in the home — the canonical household directory the
/// orchestrator holds so tools and the LLM prompt have grounded context about who
/// is here and how to reach them. Contact details are PII; they live in the `0600`
/// settings file, so keep it on a trusted machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HouseholdMember {
    /// The person's name as the assistant should address them. Required (an empty
    /// name drops the member on save).
    pub name: String,
    /// Email addresses for this person (any number). Empty = none known.
    #[serde(default)]
    pub emails: Vec<String>,
    /// Phone numbers for this person (any number). Empty = none known.
    #[serde(default)]
    pub phones: Vec<String>,
    /// This person's **AppSaid address** (their public `user_key`) — the destination the
    /// `send_phone_message` tool pushes to. `None`/empty = not linked (they won't appear
    /// as a phone-message recipient). Set either by pasting it here on the config page, or
    /// by the device "Link a phone" pairing flow (which claims a code and fills it in).
    /// The household is the single source of truth for phone-message recipients.
    #[serde(default)]
    pub appsaid: Option<String>,
    /// Optional relationship / role note (e.g. "parent", "kid", "roommate").
    #[serde(default)]
    pub relationship: Option<String>,
    /// Preferred language code (`en` / `pt-BR`) — a **tie-break only**
    /// (`plans/BilingualPlan.md` §2.2): replies follow the language spoken, and this is
    /// consulted only when a turn is too short for the STT engine or text ID to tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// Canonical household + home information the orchestrator provides as context:
/// **where** the home is (grounds an unqualified "here" for weather / nearby
/// questions) and **who** lives in it (names + contact details). The location seeds
/// from the config file's home_location / weather_units at boot and then becomes
/// editable from the config dashboard; the roster is dashboard-only.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Household {
    /// The home's physical location captured as discrete fields (Address 1/2, City,
    /// State/Prov, Zip/Postal, Country). Directions + nearby-places use the full combined
    /// address; weather + the prompt "here" line use only City + State/Prov. All-blank =
    /// unset (the prompt omits the location line).
    #[serde(default)]
    pub address: crate::household::HomeAddress,
    /// **Legacy** single-line location from older config/persisted files that predate the
    /// discrete-field split. Read-only compatibility: `sanitized()` migrates a present
    /// value into `address.city` and clears this. New writes leave it `None`.
    #[serde(default)]
    pub location: Option<String>,
    /// Preferred measurement units ("imperial" / "metric"). `None` = model's choice.
    #[serde(default)]
    pub weather_units: Option<String>,
    /// The people who live in the home.
    #[serde(default)]
    pub members: Vec<HouseholdMember>,
}

impl Household {
    /// The roster member whose name matches `name` (case-insensitive, trimmed), or
    /// `None`. Used to reconcile an identified speaker (speaker_id_plan.md) with the
    /// canonical household directory so the prompt can note who they are.
    pub fn member_matching(&self, name: &str) -> Option<&HouseholdMember> {
        let needle = name.trim();
        if needle.is_empty() {
            return None;
        }
        self.members
            .iter()
            .find(|m| m.name.trim().eq_ignore_ascii_case(needle))
    }

    /// The roster member a **spoken** name most plausibly refers to — for a name taken
    /// from an STT transcript, which often misspells it ("Christina" for *Kristina*,
    /// "Brendan" for *Brendon*). Tries an exact (case-insensitive) match on the full
    /// name, then on each member's first name, then a sound-alike match
    /// ([`crate::household::spoken_name_distance`]) — returning a member only when
    /// exactly one is closest, so an ambiguous name never picks the wrong person.
    pub fn member_matching_spoken(&self, name: &str) -> Option<&HouseholdMember> {
        if let Some(m) = self.member_matching(name) {
            return Some(m);
        }
        let needle = name.trim();
        if needle.is_empty() {
            return None;
        }
        let first =
            |m: &HouseholdMember| m.name.split_whitespace().next().unwrap_or("").to_string();
        let by_first: Vec<&HouseholdMember> = self
            .members
            .iter()
            .filter(|m| first(m).eq_ignore_ascii_case(needle))
            .collect();
        if by_first.len() == 1 {
            return Some(by_first[0]);
        }
        let mut best: Option<(usize, &HouseholdMember)> = None;
        let mut tied = false;
        for m in &self.members {
            let d = crate::household::spoken_name_distance(needle, &m.name)
                .min(crate::household::spoken_name_distance(needle, &first(m)));
            match best {
                Some((b, _)) if d > b => {}
                Some((b, _)) if d == b => tied = true,
                _ => {
                    best = Some((d, m));
                    tied = false;
                }
            }
        }
        match best {
            Some((d, m)) if !tied && d <= crate::household::spoken_name_tolerance(needle) => {
                Some(m)
            }
            _ => None,
        }
    }

    /// True when nothing is configured — no address, units, or members. (Checks the
    /// legacy single-line `location` too, since it may still hold a pre-migration value.)
    pub fn is_empty(&self) -> bool {
        self.address.is_empty()
            && self.location.as_deref().is_none_or(str::is_empty)
            && self.weather_units.as_deref().is_none_or(str::is_empty)
            && self.members.is_empty()
    }

    /// A cleaned copy: strings trimmed, blanks dropped, and any member without a
    /// name removed (so a half-filled form row never becomes a nameless entry). Also
    /// **migrates the legacy** single-line `location`: when the discrete `address` is
    /// empty but `location` is set, the value moves into `address.city` (which preserves
    /// the old weather behavior of passing the whole string to the provider) and the
    /// legacy field is cleared.
    pub fn sanitized(&self) -> Household {
        let clean_opt = |v: &Option<String>| {
            v.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let clean_list = |v: &[String]| {
            v.iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        };
        let mut address = self.address.sanitized();
        let legacy = clean_opt(&self.location);
        if address.is_empty() {
            if let Some(loc) = legacy {
                address.city = Some(loc);
            }
        }
        Household {
            address,
            location: None,
            weather_units: clean_opt(&self.weather_units),
            members: self
                .members
                .iter()
                .map(|m| HouseholdMember {
                    name: m.name.trim().to_string(),
                    emails: clean_list(&m.emails),
                    phones: clean_list(&m.phones),
                    appsaid: clean_opt(&m.appsaid),
                    relationship: clean_opt(&m.relationship),
                    // Canonical code, or dropped when blank/unknown.
                    language: m
                        .language
                        .as_deref()
                        .and_then(crate::lang::Lang::parse)
                        .map(|l| l.code().to_string()),
                })
                .filter(|m| !m.name.is_empty())
                .collect(),
        }
    }

    /// The AppSaid recipient roster derived from members that have a non-blank AppSaid
    /// address: `display name → user_key`. This is the **single source of truth** for
    /// `send_phone_message` recipients — the orchestrator rebuilds the messenger from
    /// this whenever the household (or AppSaid worker/token) changes.
    pub fn appsaid_roster(&self) -> std::collections::BTreeMap<String, String> {
        self.members
            .iter()
            .filter_map(|m| {
                let key = m
                    .appsaid
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())?;
                let name = m.name.trim();
                (!name.is_empty()).then(|| (name.to_string(), key.to_string()))
            })
            .collect()
    }

    /// Set (or clear, with an empty `user_key`) a member's AppSaid address by name
    /// (case-insensitive). **Auto-adds** the person as a new member if none matches, so a
    /// paired or seeded contact is always in the household. Returns the canonical stored
    /// name (existing members keep their original spelling).
    pub fn set_member_appsaid(&mut self, name: &str, user_key: &str) -> String {
        let name = name.trim();
        let key = user_key.trim();
        let value = (!key.is_empty()).then(|| key.to_string());
        if let Some(m) = self
            .members
            .iter_mut()
            .find(|m| m.name.trim().eq_ignore_ascii_case(name))
        {
            m.appsaid = value;
            return m.name.clone();
        }
        self.members.push(HouseholdMember {
            name: name.to_string(),
            appsaid: value,
            ..Default::default()
        });
        name.to_string()
    }
}

/// Spotify voice-control credentials + linkage, owned by the orchestrator.
///
/// Drives the rig-engine `spotify_control` tool (`crate::llm::rig`) over the
/// Spotify Web API (`crate::music::spotify`). The orchestrator holds the Premium
/// account's app **client id/secret** plus the **refresh token** minted by the
/// one-time consent flow ([`crate::spotify_consent`], driven from the config page
/// Music tab). Unlike Drive, this is used by the LLM tool set, so a change rebuilds
/// the backend so the tool is advertised/withdrawn live.
///
/// Plaintext secrets (0600 settings file); keep on a trusted machine. Requires
/// Spotify Premium.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotifyConfig {
    /// Spotify app client id. `None`/empty = unset.
    #[serde(default)]
    pub client_id: Option<String>,
    /// Spotify app client secret. `None`/empty = unset.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Long-lived refresh token minted by consent. `None` = not linked.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// librespot Connect device to target (default `"Ambient"` when unset).
    #[serde(default)]
    pub device_name: Option<String>,
    /// OAuth **redirect URL** the consent flow listens on and sends to Spotify. `None`/
    /// empty ⇒ the built-in default (`http://127.0.0.1:8888/callback`). Must exactly
    /// match a Redirect URI registered in the Spotify app.
    #[serde(default)]
    pub redirect_url: Option<String>,
    /// OAuth scope granted (informational).
    #[serde(default)]
    pub scope: Option<String>,
}

impl SpotifyConfig {
    /// True when both app credentials are present (consent can run).
    pub fn configured(&self) -> bool {
        is_set(&self.client_id) && is_set(&self.client_secret)
    }

    /// True when configured *and* a refresh token exists — the tool can be built.
    pub fn linked(&self) -> bool {
        self.configured() && is_set(&self.refresh_token)
    }

    /// The device name to target, defaulting to `"Ambient"` (matches librespot).
    pub fn device_label(&self) -> String {
        self.device_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Ambient")
            .to_string()
    }

    /// The OAuth redirect URL to use for consent, defaulting to the built-in loopback
    /// callback ([`crate::spotify_consent::DEFAULT_REDIRECT_URL`]) when unset.
    pub fn redirect_url_or_default(&self) -> String {
        self.redirect_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(crate::spotify_consent::DEFAULT_REDIRECT_URL)
            .to_string()
    }

    /// Build the live controller when fully linked, else `None` (so the
    /// `spotify_control` tool simply isn't advertised).
    pub fn controller(&self) -> Option<Arc<dyn SpotifyController>> {
        if !self.linked() {
            return None;
        }
        Some(Arc::new(SpotifyWebApi::new(
            self.client_id.clone().unwrap_or_default(),
            self.client_secret.clone().unwrap_or_default(),
            self.refresh_token.clone().unwrap_or_default(),
            self.device_label(),
        )))
    }
}

/// Cadora shared-household **shopping list** linkage, owned by the orchestrator.
///
/// Drives the rig-engine `shopping_list_add` tool (`crate::llm::rig`) over Cadora's
/// voice API (`crate::cadora`). The orchestrator holds the durable `vl_…` voice-link
/// token minted by redeeming a spoken 6-digit pairing code (config page → Household
/// tab). Like Spotify, this is used by the LLM tool set, so a change rebuilds the
/// backend so the tool is advertised/withdrawn live.
///
/// Plaintext token (0600 settings file); keep on a trusted machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CadoraConfig {
    /// Cadora app-server base URL. `None`/empty ⇒ the built-in default.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Long-lived `vl_…` voice-link token from the pairing redeem. `None` = not linked.
    #[serde(default)]
    pub link_token: Option<String>,
}

impl CadoraConfig {
    /// The base URL to target: the built-in Cadora API when unset, and also when a
    /// stale seed/persisted value still names the retired Fly server
    /// (`crate::cadora::normalize_base_url`).
    pub fn base_url_or_default(&self) -> String {
        normalize_base_url(self.base_url.as_deref())
    }

    /// True once a voice-link token exists — the tool can be built.
    pub fn linked(&self) -> bool {
        is_set(&self.link_token)
    }

    /// Build the live controller when linked, else `None` (so the `shopping_list_add`
    /// tool simply isn't advertised).
    pub fn controller(&self) -> Option<Arc<dyn CadoraController>> {
        if !self.linked() {
            return None;
        }
        Some(Arc::new(CadoraVoiceApi::new(
            self.base_url_or_default(),
            self.link_token.clone().unwrap_or_default(),
        )))
    }
}

/// A requested change to the Spotify config (tri-state per field, like `DriveUpdate`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpotifyUpdate {
    pub client_id: Option<Option<String>>,
    pub client_secret: Option<Option<String>>,
    pub refresh_token: Option<Option<String>>,
    pub device_name: Option<Option<String>>,
    pub redirect_url: Option<Option<String>>,
    pub scope: Option<Option<String>>,
}

/// A requested change to the Cadora shopping-list config (tri-state per field, like
/// [`SpotifyUpdate`]). Applied by [`SharedSettings::apply_cadora`], which rebuilds the
/// backend so the `shopping_list_add` tool is advertised/withdrawn live.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CadoraUpdate {
    pub base_url: Option<Option<String>>,
    pub link_token: Option<Option<String>>,
}

/// AppSaid phone-push linkage, owned by the orchestrator.
///
/// Drives the rig-engine `send_phone_message` tool (`crate::llm::rig`) over AppSaid's
/// HTML-push API (`crate::appsaid`). The orchestrator holds the worker URL, an optional
/// default recipient, the secret sender **app token**, and the recipient roster — which
/// is **derived from the household** (each member's `appsaid` address), not stored
/// independently. Like Spotify/Cadora, a change here (or to the household) rebuilds the
/// backend so the tool is advertised/withdrawn live.
///
/// The worker URL + default seed from the config file's `appsaid` block; the
/// `app_token` is a SECRET seeded from `APPSAID_APP_TOKEN` (never from the JSON config),
/// config-page settable, and persisted here in plaintext (0600 settings file) — keep it
/// on a trusted machine. A legacy `appsaid.recipients` config seed is folded into the
/// household at boot (each becomes a member), after which the household is authoritative.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AppSaidConfig {
    /// AppSaid worker base URL (e.g. `https://appsaid.you.workers.dev`). `None`/empty
    /// ⇒ the tool isn't advertised.
    #[serde(default)]
    pub worker_url: Option<String>,
    /// Secret sender app token. Seeded from `APPSAID_APP_TOKEN`, config-page settable,
    /// persisted 0600. Never seeded from `anamanti.json`.
    #[serde(default)]
    pub app_token: Option<String>,
    /// Recipients: display name → AppSaid `user_key`. **Derived** from the household
    /// (each member's `appsaid` address) — the household is the source of truth; this is
    /// re-computed on every household/AppSaid apply and at boot (folding the legacy
    /// config seed into the household first). Ordered for stable output; lookups are
    /// case-insensitive.
    #[serde(default)]
    pub recipients: std::collections::BTreeMap<String, String>,
    /// Optional default recipient name used when the model omits `recipient`.
    #[serde(default)]
    pub default_recipient: Option<String>,
}

impl AppSaidConfig {
    /// True once a sender app token is present.
    pub fn token_set(&self) -> bool {
        is_set(&self.app_token)
    }

    /// The cleaned recipient roster as `(display_name, user_key)` pairs, dropping
    /// blanks. Preserves the configured casing for the spoken confirmation.
    fn clean_recipients(&self) -> Vec<(String, String)> {
        self.recipients
            .iter()
            .filter_map(|(name, key)| {
                let name = name.trim();
                let key = key.trim();
                (!name.is_empty() && !key.is_empty()).then(|| (name.to_string(), key.to_string()))
            })
            .collect()
    }

    /// The configured recipient display names (for status readouts).
    pub fn recipient_names(&self) -> Vec<String> {
        self.clean_recipients()
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// True when the tool would be advertised: worker URL + app token + at least one
    /// recipient all present.
    pub fn active(&self) -> bool {
        self.messenger().is_some()
    }

    /// Build the live messenger when fully configured (worker URL + app token + at
    /// least one recipient), else `None` (so the `send_phone_message` tool simply
    /// isn't advertised).
    pub fn messenger(&self) -> Option<Arc<dyn PhoneMessenger>> {
        let worker_url = self
            .worker_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        let token = self
            .app_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        let recipients = self.clean_recipients();
        if recipients.is_empty() {
            return None;
        }
        let default = self
            .default_recipient
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Some(Arc::new(AppSaidClient::new(
            worker_url, token, recipients, default,
        )))
    }
}

/// A requested change to the AppSaid config (tri-state per field, like
/// [`CadoraUpdate`]). Applied by [`SharedSettings::apply_appsaid`], which rebuilds the
/// backend so the `send_phone_message` tool is advertised/withdrawn live. The recipient
/// roster is **not** set here — it lives on the household members (each member's
/// `appsaid` address) and is re-derived on apply; link a phone via
/// [`SharedSettings::add_appsaid_recipient`] or the household editor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AppSaidUpdate {
    pub worker_url: Option<Option<String>>,
    pub app_token: Option<Option<String>>,
    pub default_recipient: Option<Option<String>>,
}

/// Home Assistant connection settings (`plans/HomeAssistantPlan.md`): the base URL,
/// the secret long-lived access **token**, and the default display room. Seeded at boot
/// from `anamanti.json` (`home_assistant.base_url` / `default_room`) and the
/// `HOME_ASSISTANT_TOKEN` env secret; editable on the config page (Integrations → Home
/// Assistant) and persisted here — the token in plaintext in the 0600 settings file,
/// never echoed back. A change reconfigures the shared hub live (fast path + tool).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HomeAssistantSettings {
    /// HA base URL, e.g. `http://homeassistant.local:8123`. `None` ⇒ integration off.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Secret long-lived access token. `None` ⇒ integration off.
    #[serde(default)]
    pub token: Option<String>,
    /// The room a bare "turn off the lights" means when the display doesn't report one.
    #[serde(default)]
    pub default_room: Option<String>,
}

impl HomeAssistantSettings {
    /// True once a token is stored (never the value).
    pub fn token_set(&self) -> bool {
        is_set(&self.token)
    }

    /// True when both a base URL and a token are present.
    pub fn configured(&self) -> bool {
        is_set(&self.base_url) && self.token_set()
    }
}

/// A requested change to the Home Assistant settings (tri-state per field, like
/// [`AppSaidUpdate`]). Applied by [`SharedSettings::apply_home_assistant`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HomeAssistantUpdate {
    pub base_url: Option<Option<String>>,
    pub token: Option<Option<String>>,
    pub default_room: Option<Option<String>>,
}

/// A requested change to the directions tool config. `mapbox_token` is tri-state:
/// `None` = leave unchanged; `Some(None)`/`Some(Some(""))` = clear; `Some(Some(v))` =
/// set. Applied by [`SharedSettings::apply_directions`], which rebuilds the backend so
/// the `directions_lookup` tool is advertised/withdrawn live.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DirectionsUpdate {
    pub mapbox_token: Option<Option<String>>,
}

/// A requested change to the weather tool config. `provider` selects the forecast
/// backend (`visualcrossing`/`openmeteo`); `None` leaves it unchanged. `visualcrossing_key`
/// is tri-state: `None` = leave unchanged; `Some(None)`/`Some(Some(""))` = clear;
/// `Some(Some(v))` = set. Applied by [`SharedSettings::apply_weather_tool`], which rebuilds
/// the `weather_lookup` tool and retargets the ambient push live.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WeatherToolUpdate {
    pub provider: Option<String>,
    pub visualcrossing_key: Option<Option<String>>,
}

/// A requested change to the places tool config. `google_places_key` is tri-state:
/// `None` = leave unchanged; `Some(None)`/`Some(Some(""))` = clear; `Some(Some(v))` =
/// set. Applied by [`SharedSettings::apply_places_tool`], which rebuilds the backend so
/// the `places_lookup` tool is advertised/withdrawn live.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlacesToolUpdate {
    pub google_places_key: Option<Option<String>>,
}

/// A single output "personality" the assistant can speak in. **Pure output tuning:**
/// the `description` is appended to the per-turn system prompt as a trailing style
/// instruction (how the reply *sounds*) and never touches memory, recall, or tools.
/// `key` is the stable id the voice tool + config page select by; `label` is the human
/// name shown in the GUI and the spoken guidance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonalityDef {
    pub key: String,
    pub label: String,
    pub description: String,
}

/// The live personality selection plus the editable catalog of personalities. Persisted
/// inside `anamanti_settings.json` (GUI-editable on the Personality tab) and read from
/// the per-turn snapshot. `enabled == false` or an empty/"normal" `active` ⇒ the
/// assistant speaks normally (no style instruction appended). Changing it never rebuilds
/// the LLM — it only changes a string appended to the next turn's prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Personality {
    /// Master on/off switch (toggled by the `set_personality` voice tool / config page).
    #[serde(default)]
    pub enabled: bool,
    /// The selected personality key (empty / "normal" ⇒ normal voice).
    #[serde(default)]
    pub active: String,
    /// The editable catalog. Seeded with the built-ins on first run.
    #[serde(default = "default_personality_defs")]
    pub definitions: Vec<PersonalityDef>,
}

impl Default for Personality {
    fn default() -> Self {
        Self {
            enabled: false,
            active: String::new(),
            definitions: default_personality_defs(),
        }
    }
}

/// The built-in personalities seeded on first run (the GUI can edit / add / remove them).
/// "Normal" is implicit — it is the off state, so it is not in this list.
fn default_personality_defs() -> Vec<PersonalityDef> {
    let def = |key: &str, label: &str, description: &str| PersonalityDef {
        key: key.to_string(),
        label: label.to_string(),
        description: description.to_string(),
    };
    vec![
        def(
            "enthusiastic",
            "Overly Enthusiastic",
            "Respond with boundless, over-the-top enthusiasm and excitement. Pack in energy, \
             exclamation points, and upbeat, encouraging language, and treat every little thing \
             as thrilling and wonderful.",
        ),
        def(
            "sarcastic",
            "Heavy Sarcasm",
            "Respond with heavy, dry sarcasm and playful mockery — witty, deadpan, and ironic, \
             with a biting edge — while still actually giving the correct, useful answer.",
        ),
        def(
            "depressed",
            "Depressed",
            "Respond in a gloomy, melancholic, world-weary tone. Sigh, be a little deflated and \
             pessimistic, as if everything is a bit much and nothing ever quite works out — but \
             still answer the question.",
        ),
        def(
            "gangster",
            "1940s Gangster",
            "Respond like a tough 1940s mobster with a gravelly, hard-boiled, streetwise attitude. \
             Use period slang (\"see\", \"wise guy\", \"pal\", \"the big cheese\", \"dough\", \
             \"takin' a powder\") — but still answer accurately.",
        ),
        def(
            "trump",
            "Donald Trump",
            "Respond in the distinctive speaking style of Donald Trump: superlatives \
             (\"tremendous\", \"the best\", \"believe me\", \"nobody knows this better than me\"), \
             short punchy sentences, repetition, and self-assured asides. Keep it recognizable but \
             good-natured, and still answer accurately.",
        ),
    ]
}

impl Personality {
    /// The resolved trailing style instruction to append to the system prompt, or `None`
    /// when the assistant should speak normally (disabled, or the active key is
    /// empty/"normal"/unknown, or its description is blank).
    pub fn instruction(&self) -> Option<String> {
        self.active_def()
            .map(|d| d.description.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// The human label of the active personality, when enabled + resolvable.
    pub fn active_label(&self) -> Option<String> {
        self.active_def().map(|d| d.label.clone())
    }

    /// The active [`PersonalityDef`], or `None` when speaking normally.
    fn active_def(&self) -> Option<&PersonalityDef> {
        if !self.enabled {
            return None;
        }
        let key = self.active.trim();
        if key.is_empty() || key.eq_ignore_ascii_case("normal") {
            return None;
        }
        self.definitions
            .iter()
            .find(|d| d.key.eq_ignore_ascii_case(key))
    }

    /// The selectable personality keys, in catalog order (for spoken guidance + the tool).
    pub fn keys(&self) -> Vec<String> {
        self.definitions.iter().map(|d| d.key.clone()).collect()
    }

    /// Clean a GUI-submitted catalog: trim fields, drop entries with a blank key or
    /// label, de-duplicate keys (case-insensitive, first wins), and trim `active`.
    pub fn sanitized(&self) -> Self {
        let mut seen = std::collections::HashSet::new();
        let definitions = self
            .definitions
            .iter()
            .map(|d| PersonalityDef {
                key: d.key.trim().to_string(),
                label: d.label.trim().to_string(),
                description: d.description.trim().to_string(),
            })
            .filter(|d| !d.key.is_empty() && !d.label.is_empty())
            .filter(|d| seen.insert(d.key.to_lowercase()))
            .collect();
        Self {
            enabled: self.enabled,
            active: self.active.trim().to_string(),
            definitions,
        }
    }

    /// Resolve a spoken/typed personality name to a stored key. Accepts
    /// "normal"/"off"/"none"/"" (→ `Ok(None)`, i.e. turn personality off), an exact key
    /// or label (case-insensitive), else a unique substring match on key or label.
    /// Returns `Err` with the valid options when nothing matches.
    pub fn resolve(&self, name: &str) -> std::result::Result<Option<String>, String> {
        let n = name.trim();
        if n.is_empty()
            || n.eq_ignore_ascii_case("normal")
            || n.eq_ignore_ascii_case("off")
            || n.eq_ignore_ascii_case("none")
        {
            return Ok(None);
        }
        if let Some(d) = self
            .definitions
            .iter()
            .find(|d| d.key.eq_ignore_ascii_case(n) || d.label.eq_ignore_ascii_case(n))
        {
            return Ok(Some(d.key.clone()));
        }
        let nl = n.to_lowercase();
        let matches: Vec<&PersonalityDef> = self
            .definitions
            .iter()
            .filter(|d| d.key.to_lowercase().contains(&nl) || d.label.to_lowercase().contains(&nl))
            .collect();
        if matches.len() == 1 {
            return Ok(Some(matches[0].key.clone()));
        }
        let opts = self
            .definitions
            .iter()
            .map(|d| d.key.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!(
            "unknown personality `{n}`; options: normal, {opts}"
        ))
    }
}

/// The mutable settings persisted to disk so page/device changes survive a
/// restart. Contains provider keys in plaintext, so the file is written with
/// `0600` permissions on unix and should stay on a trusted machine.
///
/// Every field tolerates being absent so an older (or hand-trimmed) file still loads.
/// Seeded optional values (provider keys, and the config-seeded Drive / Spotify /
/// Cadora / AppSaid fields) use the three-state encoding of [`overlay_seeded`]:
/// absent or `null` keeps the boot seed, `""` means cleared on the page, anything else
/// is the value. [`RuntimeSettings::overlay_persisted`] reads it back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSettings {
    /// LLM engine label. Empty (absent in an older file) keeps the config seed.
    #[serde(default)]
    pub engine: String,
    /// Web-search toggle. `None` (absent) keeps the config seed.
    #[serde(default)]
    pub web_search: Option<bool>,
    /// Search provider label. Empty (absent) keeps the config seed.
    #[serde(default)]
    pub search_provider: String,
    #[serde(default)]
    pub search_api_key: Option<String>,
    /// LLM backend label. Empty (absent) keeps the config seed.
    #[serde(default)]
    pub llm_backend: String,
    #[serde(default)]
    pub llm_model: Option<String>,
    /// Runtime-set Anthropic API key, so the cloud backend can be selected without
    /// a restart even if `ANTHROPIC_API_KEY` was absent at boot. Plaintext (0600
    /// file); defaulted (absent) for older files, which then fall back to the env key.
    #[serde(default)]
    pub anthropic_api_key: Option<String>,
    /// Runtime-set OpenAI API key (same rationale as `anthropic_api_key`).
    #[serde(default)]
    pub openai_api_key: Option<String>,
    /// Runtime-set Anthropic subscription OAuth token (config page). Defaulted (absent)
    /// for older files, which then fall back to the env token / token command.
    #[serde(default)]
    pub anthropic_oauth_token: Option<String>,
    /// Runtime-set Mapbox token for the directions tool. Defaulted (absent) for older
    /// files, which then fall back to the `MAPBOX_TOKEN` env seed.
    #[serde(default)]
    pub mapbox_token: Option<String>,
    /// Anthropic auth mode (`apikey`/`subscription`). Defaulted for older files.
    #[serde(default = "default_anthropic_auth")]
    pub anthropic_auth: String,
    #[serde(default)]
    pub tts_voice: Option<String>,
    /// End-of-speech trailing silence in ms (VAD). Defaulted for older files.
    #[serde(default = "default_end_silence_ms")]
    pub end_silence_ms: u64,
    /// Speech-vs-noise RMS threshold (VAD). Defaulted for older files.
    #[serde(default = "default_voice_rms_threshold")]
    pub voice_rms_threshold: f64,
    /// Silero speech-probability gate (neural VAD), `0.0..1.0`. `Option` so an older
    /// file (absent) keeps the config-file seed rather than forcing a default.
    #[serde(default)]
    pub silero_threshold: Option<f32>,
    /// VAD engine label (`energy`/`silero`). `Option` so an older file (absent) keeps
    /// the config-file `vad.engine` seed.
    #[serde(default)]
    pub vad_engine: Option<String>,
    /// STT engine label (`wyoming`/`whisper-rs`/`elevenlabs`). `Option` so an older file
    /// (absent) keeps the config-file `stt.engine` seed (overlay only when non-empty).
    #[serde(default)]
    pub stt_engine: Option<String>,
    /// Runtime-set ElevenLabs realtime model id. Defaulted (absent) for older files.
    #[serde(default)]
    pub stt_elevenlabs_model_id: Option<String>,
    /// Runtime-set ElevenLabs WebSocket host base. Defaulted (absent) for older files.
    #[serde(default)]
    pub stt_elevenlabs_base_url: Option<String>,
    /// Runtime-set ElevenLabs API key. Defaulted (absent) for older files, which then
    /// fall back to the `ELEVENLABS_API_KEY` env seed. Plaintext (0600 file).
    #[serde(default)]
    pub stt_elevenlabs_api_key: Option<String>,
    /// Google Drive photo-slideshow credentials + linkage. Defaulted (empty) for
    /// older files.
    #[serde(default)]
    pub drive: DriveConfig,
    /// Canonical household + home information (location, units, people). Defaulted
    /// (empty) for older files; seeded from env at boot when absent.
    #[serde(default)]
    pub household: Household,
    /// Spotify voice-control credentials + linkage. Defaulted (empty) for older
    /// files.
    #[serde(default)]
    pub spotify: SpotifyConfig,
    /// Cadora shopping-list linkage (voice-link token). Defaulted (empty) for older
    /// files.
    #[serde(default)]
    pub cadora: CadoraConfig,
    /// AppSaid phone-push linkage (worker URL + recipients + secret app token).
    /// Defaulted (empty) for older files.
    #[serde(default)]
    pub appsaid: AppSaidConfig,
    /// Home Assistant connection (URL + secret token + default room). Defaulted (empty)
    /// for older files, which then fall back to the config/env seed.
    #[serde(default)]
    pub home_assistant: HomeAssistantSettings,
    /// Runtime-set weather provider label (`visualcrossing`/`openmeteo`). Defaulted
    /// (absent) for older files, which then fall back to the `weather.provider` seed.
    #[serde(default)]
    pub weather_provider: Option<String>,
    /// Runtime-set Visual Crossing API key. Defaulted (absent) for older files, which
    /// then fall back to the `VISUALCROSSING_API_KEY` env seed. Plaintext (0600 file).
    #[serde(default)]
    pub visualcrossing_key: Option<String>,
    /// Runtime-set Google Places API key for the places tool. Defaulted (absent) for
    /// older files, which then fall back to the `GOOGLE_PLACES_API_KEY` env seed.
    /// Plaintext (0600 file).
    #[serde(default)]
    pub google_places_key: Option<String>,
    /// Selected System-1 backend. Defaults to **empty** (not "none") for older files, so
    /// an old persisted file can't clobber a config-file `system1.backend` seed — the
    /// overlay only applies when this is non-empty (a save always writes a real label).
    #[serde(default)]
    pub system1_backend: String,
    #[serde(default = "default_system1_base_url")]
    pub system1_base_url: String,
    #[serde(default = "default_system1_model")]
    pub system1_model: String,
    #[serde(default = "default_system1_min_confidence")]
    pub system1_min_confidence: f64,
    #[serde(default)]
    pub system1_intents: Vec<String>,
    /// Runtime-set OpenRouter API key (config page). Defaulted (absent) for older files,
    /// which then fall back to the `OPENROUTER_API_KEY` env seed.
    #[serde(default)]
    pub openrouter_api_key: Option<String>,
    /// Output personality (on/off, active selection, and the editable catalog). Pure
    /// output tuning. Defaulted (seeded with the built-ins) for older files.
    #[serde(default)]
    pub personality: Personality,
    /// STT pronunciation rewrite table (`plans/stt-vocabulary-biasing.md`). Defaulted
    /// (empty) for older files.
    #[serde(default)]
    pub pronunciations: Vec<crate::stt::PronunciationRule>,
    /// Household languages (`plans/BilingualPlan.md`). Written only once changed on the
    /// config page (i.e. when the live value differs from the `anamanti.json` seed), so
    /// an untouched install keeps following its config file; absent ⇒ the seed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<crate::lang::LanguageSettings>,
    /// Internet-radio station catalog. Defaulted (empty) for older files.
    #[serde(default)]
    pub radio: RadioCatalog,
    /// Per-display pinned music widget (`device_id` → compact bar / full screen); a display
    /// with no entry is automatic. Defaulted (empty) for older files, and read leniently
    /// (garbled entries are dropped). Older files' household-wide `music_display_style`
    /// key is ignored — the automatic per-display rule replaced it.
    #[serde(
        default,
        deserialize_with = "crate::music::display_style::deserialize_pins"
    )]
    pub music_display_pins: MusicDisplayPins,
}

fn default_system1_base_url() -> String {
    DEFAULT_SYSTEM1_BASE_URL.to_string()
}
fn default_system1_model() -> String {
    DEFAULT_SYSTEM1_MODEL.to_string()
}
fn default_system1_min_confidence() -> f64 {
    DEFAULT_SYSTEM1_MIN_CONFIDENCE
}

/// Load persisted settings, or `None` if the file is absent.
///
/// A file that exists but cannot be read or parsed is **never silently dropped**: it
/// is renamed aside to `<name>.corrupt-<unix ts>` (logged at error level) so the next
/// save cannot overwrite the page-entered secrets and roster it may still hold, and
/// the caller boots from the config/env seed.
pub fn load_persisted(path: &Path) -> Option<PersistedSettings> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            quarantine_settings_file(path, &format!("unreadable: {e}"));
            return None;
        }
    };
    match serde_json::from_str(&data) {
        Ok(p) => Some(p),
        Err(e) => {
            quarantine_settings_file(path, &format!("unparseable: {e}"));
            None
        }
    }
}

/// Move a bad settings file aside to `<name>.corrupt-<unix ts>` so it is preserved for
/// manual recovery and not overwritten by the next save.
fn quarantine_settings_file(path: &Path, why: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings".to_string());
    let aside = path.with_file_name(format!("{name}.corrupt-{ts}"));
    match std::fs::rename(path, &aside) {
        Ok(()) => log::error!(
            "settings file {} is {why}; moved it to {} and booting from the config/env \
             seed — recover any page-entered values from that copy",
            path.display(),
            aside.display()
        ),
        Err(e) => log::error!(
            "settings file {} is {why}, and moving it aside to {} failed ({e}); booting \
             from the config/env seed",
            path.display(),
            aside.display()
        ),
    }
}

/// Best-effort **atomic** write of the persisted settings. Failures are logged, never
/// fatal — a settings change still takes effect in memory.
///
/// Writes a sibling temp file (mode `0600` on unix), fsyncs it, renames it over
/// `path`, then best-effort fsyncs the directory, so a crash mid-write leaves either
/// the old or the new file — never a truncated one. The final file is forced to
/// `0600` (it holds plaintext secrets).
fn persist(path: &Path, s: &PersistedSettings) {
    let json = match serde_json::to_string_pretty(s) {
        Ok(j) => j,
        Err(e) => {
            log::warn!("could not serialize settings: {e}");
            return;
        }
    };
    if let Err(e) = write_atomic(path, json.as_bytes()) {
        log::warn!("could not persist settings to {}: {e}", path.display());
    }
}

/// Atomically replace `path` with `bytes` (temp file + fsync + rename + dir fsync).
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings".to_string());
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        // A pre-existing temp file keeps its old mode; force 0600 before writing.
        set_owner_only(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        // Make the rename durable (best-effort: not every platform/fs supports it).
        if let Some(dir) = path.parent() {
            let dir = if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            };
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        set_owner_only(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Restrict a file to owner read/write (`0600`) on unix; a no-op elsewhere.
fn set_owner_only(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Immutable inputs needed to (re)build any LLM backend on demand. Captured once
/// from the environment/config so a runtime swap never re-reads `env`.
#[derive(Clone)]
pub struct LlmFactory {
    /// Local Ollama / llama.cpp base URL.
    pub ollama_url: String,
    /// Cloud Anthropic API base URL.
    pub anthropic_base_url: String,
    /// Anthropic API key seeded from the environment at boot. This is only the
    /// initial default: the live key lives in [`RuntimeSettings::anthropic_api_key`]
    /// and `apply` overrides a factory clone with it, so a key entered at runtime
    /// (config page) takes effect without a restart. Absent here *and* in the
    /// runtime settings means the cloud backend can't be selected (rejected in-band).
    pub anthropic_api_key: Option<String>,
    /// `max_tokens` for Anthropic replies (kept small for spoken output).
    pub anthropic_max_tokens: u32,
    /// Cloud OpenAI API base URL.
    pub openai_base_url: String,
    /// OpenAI API key seeded from the environment at boot (initial default only;
    /// the live key lives in [`RuntimeSettings::openai_api_key`] — see
    /// `anthropic_api_key` above).
    pub openai_api_key: Option<String>,
    /// `max_completion_tokens` for OpenAI replies (kept small for spoken output).
    pub openai_max_tokens: u32,
    /// Subscription (OAuth) token source for Anthropic, used when the auth mode is
    /// `Subscription`. Shared with the model catalog so both authenticate the same.
    pub anthropic_token: Option<Arc<AnthropicTokenProvider>>,
    /// Live home location shared with every rebuilt backend's `directions_lookup`
    /// tool as its default origin. Held here (not per-backend) so a household-location
    /// edit — [`SharedSettings::apply_household`] calls `set` on this same cell —
    /// changes the directions origin without rebuilding the LLM. Seeded from the
    /// [`Household`] record at boot.
    pub home_location: crate::directions::LiveHomeLocation,
    /// The live Spotify controller for the `spotify_control` tool, or `None` when
    /// Spotify isn't linked. Set from the current [`SpotifyConfig`] on every
    /// (re)build so the tool reflects the latest consent without re-reading env.
    pub spotify: Option<Arc<dyn SpotifyController>>,
    /// The live Cadora controller for the `shopping_list_add` tool, or `None` when the
    /// shopping list isn't linked. Set from the current [`CadoraConfig`] on every
    /// (re)build so the tool reflects the latest linkage.
    pub cadora: Option<Arc<dyn CadoraController>>,
    /// The live AppSaid messenger for the `send_phone_message` tool, or `None` when it
    /// isn't fully configured. Set from the current [`AppSaidConfig`] on every
    /// (re)build so the tool reflects the latest linkage.
    pub appsaid: Option<Arc<dyn PhoneMessenger>>,
    /// The web-calendar source for the `calendar_lookup` tool, or `None` when no
    /// subscriptions are configured. Prebuilt once from the config file's `calendar`
    /// block so a rebuild never re-parses config; shared into every rebuilt backend.
    pub calendar: Option<Arc<dyn CalendarSource>>,
    /// The routing provider + imperial-units flag for the `directions_lookup` tool,
    /// or `None` when no provider/token is configured. Built from the live Mapbox token
    /// ([`RuntimeSettings::mapbox_token`]) and refreshed on every (re)build — like
    /// `spotify` — so a token entered on the config page advertises the tool live.
    pub directions: Option<(Arc<dyn DirectionsProvider>, bool)>,
    /// The routing provider label (`directions.provider`, default `mapbox`), kept so
    /// `directions` can be rebuilt from a new runtime token.
    pub directions_provider: String,
    /// Whether directions distances are spoken in imperial units (from the household
    /// `weather_units` at boot). Kept alongside `directions_provider` for rebuilds.
    pub directions_imperial: bool,
    /// The forecast provider for the `weather_lookup` tool, or `None` when weather is
    /// disabled. Built from the live provider label ([`RuntimeSettings::weather_provider`])
    /// and Visual Crossing key ([`RuntimeSettings::visualcrossing_key`]), refreshed on
    /// every (re)build — like `directions` — so a provider/key change on the config page
    /// takes effect live. Shared into every rebuilt backend; the ambient push builds its
    /// own instance from the same live settings.
    pub weather: Option<Arc<dyn crate::weather::WeatherProvider>>,
    /// Whether weather temperatures are reported in imperial units (°F), from the
    /// household `weather_units` at boot. Mirrors `directions_imperial`.
    pub weather_imperial: bool,
    /// Whether the weather feature is enabled (`weather.enabled`). Kept so `weather` can
    /// be rebuilt from a new runtime provider/key without re-reading config; `false`
    /// forces the provider to `None` regardless of the label/key.
    pub weather_enabled: bool,
    /// The Google Places provider for the `places_lookup` tool, or `None` when places is
    /// disabled or no key is configured. Built from the live key
    /// ([`RuntimeSettings::google_places_key`]) and refreshed on every (re)build — like
    /// `weather`/`directions` — so a key entered on the config page advertises the tool
    /// live. Shared into every rebuilt backend.
    pub places: Option<Arc<dyn crate::places::PlacesProvider>>,
    /// Whether the places feature is enabled (`places.enabled`). Kept so `places` can be
    /// rebuilt from a new runtime key without re-reading config; `false` forces the
    /// provider to `None` regardless of the key.
    pub places_enabled: bool,
    /// The places backend label (`places.provider`, default `google`), kept so `places`
    /// can be rebuilt from a new runtime key.
    pub places_provider: String,
    /// The internet-radio player (`plans/RadioPlan.md`), or `None` when music routing or
    /// the mpv IPC is off. Built once at boot and shared by every rebuilt backend, like
    /// `home_location`; its catalog cell is refreshed by
    /// [`SharedSettings::apply_radio`]. Also wraps the Spotify controller
    /// ([`RoutedSpotify`]) so the two sources hand off cleanly.
    pub radio: Option<Arc<RadioPlayer>>,
    /// The Home Assistant hub (`plans/HomeAssistantPlan.md`) for the `home_control` tool
    /// and the pipeline's fast path, or `None` when HA isn't configured. Built once at
    /// boot (like `radio`) so its cached device catalog survives LLM rebuilds.
    pub home_assistant: Option<Arc<crate::home_assistant::HomeAssistant>>,
    /// The NVR camera hub (`plans/CameraPlan.md`) for the `show_camera` tool, the
    /// pipeline's camera fast path and the `anamanti-get-camera-stream` control request,
    /// or `None` when the camera isn't configured. Boot-time only.
    pub camera: Option<Arc<crate::camera::CameraHub>>,
    /// The per-display music-style session state (`plans/MusicDisplayStylePlan.md`),
    /// shared by every rebuilt backend's `spotify_control` / `radio_control` /
    /// `music_screen` tools (which record the requesting display), the display's
    /// on-screen toggle, and the now-playing push. Created once per Core.
    pub music_display: Arc<MusicDisplayRouter>,
}

impl LlmFactory {
    /// A copy of this factory with **every** piece of live, settings-derived tool and
    /// credential state rebuilt from `s`: the provider API keys, the Spotify / Cadora /
    /// AppSaid controllers, and the directions / weather / places providers (honouring
    /// `weather_enabled`, `places_enabled` and `places_provider`). The one place that
    /// maps runtime settings onto the factory, used by boot and every `apply_*`, so a
    /// save of one setting can never drop or mis-build another setting's tool.
    pub fn for_settings(&self, s: &RuntimeSettings) -> LlmFactory {
        let mut f = self.clone();
        f.anthropic_api_key = non_blank(&s.anthropic_api_key);
        f.openai_api_key = non_blank(&s.openai_api_key);
        f.spotify = s.spotify.controller().map(|c| match &f.radio {
            Some(radio) => RoutedSpotify::wrap(c, radio.clone()),
            None => c,
        });
        f.cadora = s.cadora.controller();
        f.appsaid = s.appsaid.messenger();
        // The Home Assistant hub is shared (pipeline fast path + tool), so it is
        // reconfigured in place rather than rebuilt; unchanged settings are a no-op.
        if let Some(hub) = &f.home_assistant {
            let ha = &s.home_assistant;
            hub.configure(
                ha.base_url.as_deref(),
                ha.token.as_deref(),
                ha.default_room.as_deref(),
            );
        }
        f.directions = crate::directions::from_token(
            &f.directions_provider,
            non_blank(&s.mapbox_token).as_deref(),
            f.directions_imperial,
        );
        f.weather = crate::weather::from_config(
            f.weather_enabled,
            &s.weather_provider,
            non_blank(&s.visualcrossing_key).as_deref(),
        );
        f.places = if f.places_enabled {
            crate::places::from_key(
                &f.places_provider,
                non_blank(&s.google_places_key).as_deref(),
            )
        } else {
            None
        };
        f
    }

    /// Build the LLM backend (with its full tool set) that `s` describes: the engine,
    /// web-search config, backend, model and auth from `s`, via a factory derived by
    /// [`Self::for_settings`]. Returns the backend plus its canonical label and model.
    pub fn build_for(
        &self,
        s: &RuntimeSettings,
    ) -> Result<(Arc<dyn LlmBackend>, String, Option<String>)> {
        self.for_settings(s).build(
            s.engine,
            s.web_search,
            &s.search_provider,
            non_blank(&s.search_api_key).as_deref(),
            &s.llm_backend,
            s.llm_model.as_deref(),
            s.anthropic_auth,
        )
    }

    /// The tool dependencies this factory's providers describe, with the given
    /// web-search selection.
    pub fn tool_deps(
        &self,
        web_search: bool,
        search_provider: &str,
        search_api_key: Option<&str>,
    ) -> crate::llm::rig::ToolDeps {
        crate::llm::rig::ToolDeps {
            web_search,
            search_provider: search_provider.to_string(),
            search_api_key: search_api_key.map(str::to_string),
            home_location: self.home_location.clone(),
            calendar: self.calendar.clone(),
            directions: self.directions.clone(),
            spotify: self.spotify.clone(),
            cadora: self.cadora.clone(),
            appsaid: self.appsaid.clone(),
            weather: self.weather.clone(),
            weather_imperial: self.weather_imperial,
            places: self.places.clone(),
            // Advertised only with at least one station configured.
            radio: self.radio.clone().filter(|r| r.has_stations()),
            // Advertised only once a URL + token are set.
            home_assistant: self.home_assistant.clone().filter(|h| h.is_configured()),
            camera: self.camera.clone(),
            music_display: Some(self.music_display.clone()),
        }
    }

    /// Default model for a backend label when the caller pins none. The one place the
    /// default model names live.
    fn default_model(backend: &str) -> Option<&'static str> {
        match backend {
            "ollama" => Some("llama3.2"),
            "anthropic" | "claude" => Some("claude-opus-5"),
            "openai" | "gpt" => Some("gpt-4o-mini"),
            _ => None,
        }
    }

    /// Build a backend from a label (`ollama` / `anthropic` / `openai` / `mock`) and
    /// an optional model. Returns the trait object plus the canonical label and the
    /// resolved model so callers can report exactly what took effect.
    ///
    /// Under the rig engine every provider (Ollama, Anthropic with an API key or a
    /// subscription, OpenAI) runs on rig with the full tool set; the native engine
    /// uses the plain-text HTTP clients. A backend that loses tools or history is
    /// logged loudly here, at build/swap time.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        &self,
        engine: LlmEngine,
        web_search: bool,
        search_provider: &str,
        search_api_key: Option<&str>,
        backend: &str,
        model: Option<&str>,
        anthropic_auth: AnthropicAuth,
    ) -> Result<(Arc<dyn LlmBackend>, String, Option<String>)> {
        // The tool registry, built at most once per build (only the rig engine runs
        // tools) from this factory's live providers.
        let tools = || {
            Arc::new(crate::llm::rig::Tools::build(self.tool_deps(
                web_search,
                search_provider,
                search_api_key,
            )))
        };
        let label = backend.to_lowercase();
        let model = |label: &str| {
            model
                .or(Self::default_model(label))
                .unwrap_or_default()
                .to_string()
        };
        let rig = engine == LlmEngine::Rig;
        let (llm, label, model): (Arc<dyn LlmBackend>, &str, Option<String>) =
            match label.as_str() {
                "mock" => (Arc::new(MockLlm::default()), "mock", None),
                "anthropic" | "claude" => {
                    let model = model("anthropic");
                    let llm: Arc<dyn LlmBackend> = match anthropic_auth {
                        AnthropicAuth::Subscription => {
                            let token = self.anthropic_token.clone().context(
                                "the anthropic subscription backend needs a token — set \
                             ANTHROPIC_OAUTH_TOKEN (run `claude setup-token`) on the orchestrator",
                            )?;
                            if rig {
                                Arc::new(crate::llm::rig::RigBackend::anthropic_subscription(
                                    &self.anthropic_base_url,
                                    token,
                                    &model,
                                    self.anthropic_max_tokens,
                                    tools(),
                                )?)
                            } else {
                                Arc::new(AnthropicBackend::with_subscription(
                                    &self.anthropic_base_url,
                                    token,
                                    &model,
                                    self.anthropic_max_tokens,
                                ))
                            }
                        }
                        AnthropicAuth::ApiKey => {
                            let key = self.anthropic_api_key.clone().context(
                            "the anthropic backend requires ANTHROPIC_API_KEY on the orchestrator \
                             (or switch to subscription auth)",
                        )?;
                            if rig {
                                Arc::new(crate::llm::rig::RigBackend::anthropic(
                                    &self.anthropic_base_url,
                                    &key,
                                    &model,
                                    self.anthropic_max_tokens,
                                    tools(),
                                )?)
                            } else {
                                Arc::new(AnthropicBackend::new(
                                    &self.anthropic_base_url,
                                    key,
                                    &model,
                                    self.anthropic_max_tokens,
                                ))
                            }
                        }
                    };
                    (llm, "anthropic", Some(model))
                }
                "openai" | "gpt" => {
                    let key = self.openai_api_key.clone().context(
                        "the openai backend requires OPENAI_API_KEY on the orchestrator",
                    )?;
                    let model = model("openai");
                    let llm: Arc<dyn LlmBackend> = if rig {
                        Arc::new(crate::llm::rig::RigBackend::openai(
                            &self.openai_base_url,
                            &key,
                            &model,
                            self.openai_max_tokens,
                            tools(),
                        )?)
                    } else {
                        Arc::new(OpenAiBackend::new(
                            &self.openai_base_url,
                            key,
                            &model,
                            self.openai_max_tokens,
                        ))
                    };
                    (llm, "openai", Some(model))
                }
                "ollama" => {
                    let model = model("ollama");
                    let llm: Arc<dyn LlmBackend> = if rig {
                        Arc::new(crate::llm::rig::RigBackend::ollama(
                            &self.ollama_url,
                            &model,
                            tools(),
                        )?)
                    } else {
                        Arc::new(OllamaBackend::new(&self.ollama_url, &model))
                    };
                    (llm, "ollama", Some(model))
                }
                other => {
                    anyhow::bail!(
                        "unknown LLM backend `{other}` (expected ollama/anthropic/openai/mock)"
                    )
                }
            };
        let missing = llm.capabilities().missing();
        if !missing.is_empty() && label != "mock" {
            log::warn!(
                "LLM backend `{label}` on the {} engine runs WITHOUT {}. Set llm.engine=\"rig\" \
                 (config file or config page) to restore them.",
                engine.as_str(),
                missing.join(" or "),
            );
        }
        Ok((llm, label.to_string(), model))
    }
}

/// The live, swappable settings the pipeline reads each turn.
#[derive(Clone)]
pub struct RuntimeSettings {
    /// The currently selected LLM backend.
    pub llm: Arc<dyn LlmBackend>,
    /// Which engine backs ollama/anthropic (native HTTP vs rig-core).
    pub engine: LlmEngine,
    /// Whether the rig web-search tool is enabled (rig engine only).
    pub web_search: bool,
    /// Web-search backend: `duckduckgo` (keyless) or `tavily` (needs a key).
    pub search_provider: String,
    /// API key for the search provider (Tavily). `None` = unset.
    pub search_api_key: Option<String>,
    /// Canonical label of the selected backend (`ollama` / `anthropic` / `mock`).
    pub llm_backend: String,
    /// The resolved model name, if the backend uses one.
    pub llm_model: Option<String>,
    /// Live Anthropic API key. Runtime-settable (config page) so the cloud backend
    /// can be selected without a restart; seeded from `ANTHROPIC_API_KEY` at boot.
    /// `None` = no key (selecting the api-key anthropic backend is rejected).
    pub anthropic_api_key: Option<String>,
    /// Live OpenAI API key. Runtime-settable; seeded from `OPENAI_API_KEY` at boot.
    pub openai_api_key: Option<String>,
    /// Live Anthropic subscription OAuth token. Runtime-settable (config page, when
    /// auth = subscription); seeded from `ANTHROPIC_OAUTH_TOKEN` at boot. `None` = no
    /// override (the provider falls back to env / the token command).
    pub anthropic_oauth_token: Option<String>,
    /// Live Mapbox token for the `directions_lookup` tool. Runtime-settable (config
    /// page Tools tab); seeded from `MAPBOX_TOKEN`/`MAPBOX_ACCESS_TOKEN` at boot.
    /// `None` = the tool isn't advertised.
    pub mapbox_token: Option<String>,
    /// How the Anthropic backend authenticates (API key vs subscription OAuth).
    pub anthropic_auth: AnthropicAuth,
    /// The Piper voice to synthesize with, or `None` for the server default.
    pub tts_voice: Option<String>,
    /// End-of-speech trailing silence (ms) the energy VAD waits for before
    /// finalizing the STT transcript. A/B-tunable from the device.
    pub end_silence_ms: u64,
    /// RMS (i16 units) above which a chunk counts as speech for the VAD.
    pub voice_rms_threshold: f64,
    /// Silero speech-probability gate (`0.0..1.0`) for the neural VAD engine.
    /// A/B-tunable from the device / config page; read from the per-turn snapshot.
    pub silero_threshold: f32,
    /// Which VAD engine decides end-of-speech (energy default, or silero). Live-swappable
    /// from the config page; a swap to silero with no model loaded falls back to energy.
    pub vad_engine: crate::config::VadEngineKind,
    /// The live STT engine selection + ElevenLabs config/key, read per-turn so a
    /// config-page swap takes effect between turns (see [`SttRuntime`]).
    pub stt: SttRuntime,
    /// Google Drive photo-slideshow credentials + linkage (orchestrator-owned;
    /// pulled by the device over Wyoming). Orthogonal to the LLM rebuild path.
    pub drive: DriveConfig,
    /// Canonical household + home information (location, units, people) injected
    /// into the per-turn prompt. Orthogonal to the LLM rebuild path.
    pub household: Household,
    /// Spotify voice-control credentials + linkage. Unlike Drive, a change here
    /// rebuilds the backend (via [`SpotifyConfig::controller`]) so the
    /// `spotify_control` tool is advertised/withdrawn live.
    pub spotify: SpotifyConfig,
    /// Cadora shopping-list linkage. Like Spotify, a change here rebuilds the backend
    /// (via [`CadoraConfig::controller`]) so the `shopping_list_add` tool is
    /// advertised/withdrawn live.
    pub cadora: CadoraConfig,
    /// AppSaid phone-push linkage (worker URL + recipients + secret app token). Like
    /// Cadora, a change here rebuilds the backend (via [`AppSaidConfig::messenger`]) so
    /// the `send_phone_message` tool is advertised/withdrawn live.
    pub appsaid: AppSaidConfig,
    /// Home Assistant connection (URL + secret token + default room). A change
    /// reconfigures the shared hub (fast path + `home_control` tool) live.
    pub home_assistant: HomeAssistantSettings,
    /// The selected weather forecast provider label (`visualcrossing` / `openmeteo`).
    /// Runtime-settable (config page Tools tab); seeded from `weather.provider` at boot.
    /// A change rebuilds the `weather_lookup` tool + retargets the ambient push live.
    pub weather_provider: String,
    /// Live Visual Crossing API key for the weather provider. Runtime-settable (config
    /// page Tools tab); seeded from `VISUALCROSSING_API_KEY` at boot. `None` ⇒ the
    /// Visual Crossing provider falls back to keyless Open-Meteo.
    pub visualcrossing_key: Option<String>,
    /// Live Google Places API key for the `places_lookup` tool. Runtime-settable (config
    /// page Tools tab); seeded from `GOOGLE_PLACES_API_KEY` at boot. `None` ⇒ the tool
    /// isn't advertised (Google Places has no keyless fallback).
    pub google_places_key: Option<String>,
    /// The live System-1 fast-decision selection (plans/system1-fast-decisions.md), read
    /// from the per-turn snapshot so a config-page swap takes effect between turns.
    /// Bundled into one field so the widely-constructed `RuntimeSettings` only gains one.
    pub system1: System1Runtime,
    /// The live output personality (on/off + selection + catalog). Read per-turn from the
    /// snapshot and appended to the prompt as a trailing style instruction. Pure output
    /// tuning — orthogonal to the LLM rebuild path (a change never rebuilds the backend).
    pub personality: Personality,
    /// The live STT pronunciation rewrite table (`plans/stt-vocabulary-biasing.md`). Read
    /// per-turn from the snapshot and applied to the final transcript before it is shown,
    /// stored, or sent to the LLM. Empty ⇒ no rewriting. GUI-editable; orthogonal to the
    /// LLM rebuild path (a change never rebuilds the backend).
    pub pronunciations: Vec<crate::stt::PronunciationRule>,
    /// The household's languages (`plans/BilingualPlan.md`): which ones the assistant
    /// listens/answers in, the default, and a Piper voice per language. Read per turn
    /// from the snapshot. English-only by default (= the pre-bilingual behaviour).
    /// Seeded from `anamanti.json` (`language` + `tts_voices`); editable on the config
    /// page (Voice → Languages). Orthogonal to the LLM rebuild path.
    pub language: crate::lang::LanguageSettings,
    /// The curated internet-radio station list (`plans/RadioPlan.md`). GUI-editable; a
    /// change refreshes the [`RadioPlayer`] catalog and rebuilds the LLM so the
    /// `radio_control` tool's station list stays current.
    pub radio: RadioCatalog,
    /// Per-display pinned music widget (`device_id` → compact bar / full screen), set on
    /// the config page; a display with no pin is automatic (full screen where the play
    /// request came from, compact elsewhere). Read by the now-playing push for each
    /// display; orthogonal to the LLM rebuild path. See [`MusicDisplayRouter`].
    pub music_display_pins: MusicDisplayPins,
}

/// Default System-1 HTTP base URL (local `laya-serve`).
pub const DEFAULT_SYSTEM1_BASE_URL: &str = "http://127.0.0.1:8000";
/// Default System-1 OpenRouter model id (the `jev` backend).
pub const DEFAULT_SYSTEM1_MODEL: &str = "typesafe/jev-1.13";
/// Default System-1 confidence floor.
pub const DEFAULT_SYSTEM1_MIN_CONFIDENCE: f64 = 0.85;

/// The live System-1 selection: the built engine plus the descriptor needed to rebuild
/// it on a config-page swap and report the current choice. `Default` = disabled
/// (`NoDecision`), so a `RuntimeSettings` literal can spell it `System1Runtime::default()`.
#[derive(Clone)]
pub struct System1Runtime {
    /// The live decision engine (`name() == "none"` ⇒ the turn logic skips the stage).
    pub engine: Arc<dyn crate::system1::DecisionEngine>,
    /// Selected backend label (`none`/`laya-serve`/`jev`/…).
    pub backend: String,
    /// HTTP base URL (`laya-serve` host, or the OpenRouter API root for `jev`).
    pub base_url: String,
    /// Model id for the `jev` backend (OpenRouter).
    pub model: String,
    /// Confidence floor below which a decision defers to System-2.
    pub min_confidence: f64,
    /// Allowed intents (empty = the built-in default set).
    pub intents: Vec<String>,
    /// Live OpenRouter API key for `jev`. Runtime-settable (config page); seeded from
    /// `OPENROUTER_API_KEY` at boot. `None` = unset.
    pub openrouter_api_key: Option<String>,
}

impl Default for System1Runtime {
    fn default() -> Self {
        Self {
            engine: crate::system1::none(),
            backend: "none".to_string(),
            base_url: DEFAULT_SYSTEM1_BASE_URL.to_string(),
            model: DEFAULT_SYSTEM1_MODEL.to_string(),
            min_confidence: DEFAULT_SYSTEM1_MIN_CONFIDENCE,
            intents: Vec::new(),
            openrouter_api_key: None,
        }
    }
}

/// A description of the live System-1 selection, for the config page. Never exposes the
/// OpenRouter key (only whether one is set).
#[derive(Debug, Clone, PartialEq)]
pub struct System1View {
    pub backend: String,
    pub base_url: String,
    pub model: String,
    pub min_confidence: f64,
    pub openrouter_key_set: bool,
    pub intents: Vec<String>,
}

/// A requested System-1 change (config page). Absent fields are left unchanged;
/// `openrouter_api_key` is tri-state (`None` = keep, `Some(None)` = clear,
/// `Some(Some(v))` = set).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct System1Update {
    pub backend: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub min_confidence: Option<f64>,
    pub openrouter_api_key: Option<Option<String>>,
    pub intents: Option<Vec<String>>,
}

/// The live STT engine selection + ElevenLabs connection config, read per-turn from the
/// snapshot (`orchestrator::Pipeline::build_stt_transcriber`) so a config-page swap takes
/// effect between turns. Bundled into one field so the widely-constructed
/// [`RuntimeSettings`] only gains one. The in-process whisper model itself is loaded once
/// at boot and held on the pipeline (like Silero); this only picks which engine a turn
/// uses and carries the ElevenLabs settings/key. See `plans/ElevenLabsSttPlan.md`.
#[derive(Debug, Clone)]
pub struct SttRuntime {
    /// Which engine transcribes (`wyoming` / `whisper-rs` / `elevenlabs`).
    pub engine: crate::config::SttEngineKind,
    /// ElevenLabs realtime model id (used only when `engine = elevenlabs`).
    pub elevenlabs_model_id: String,
    /// ElevenLabs WebSocket host base (used only when `engine = elevenlabs`).
    pub elevenlabs_base_url: String,
    /// Live ElevenLabs API key (secret). Runtime-settable (config page); seeded from
    /// `ELEVENLABS_API_KEY` at boot. `None` ⇒ selecting `elevenlabs` falls back to Wyoming.
    pub elevenlabs_api_key: Option<String>,
    /// Decode language (ISO 639) passed to the ElevenLabs engine; `None` ⇒ auto-detect.
    /// Seeded from `stt.language`; not exposed on the config page.
    pub language: Option<String>,
    /// Whether the in-process whisper engine is available (feature compiled + model file
    /// present at boot). Read-only info for the config page; never changed by a swap.
    pub whisper_available: bool,
}

impl Default for SttRuntime {
    fn default() -> Self {
        Self {
            engine: crate::config::SttEngineKind::Wyoming,
            elevenlabs_model_id: crate::stt::elevenlabs::DEFAULT_MODEL_ID.to_string(),
            elevenlabs_base_url: crate::stt::elevenlabs::DEFAULT_BASE_URL.to_string(),
            elevenlabs_api_key: None,
            language: None,
            whisper_available: false,
        }
    }
}

/// Config-page view of the STT selection. Never exposes the ElevenLabs key (only
/// whether one is set).
#[derive(Debug, Clone, PartialEq)]
pub struct SttView {
    /// Current engine label (`wyoming` / `whisper-rs` / `elevenlabs`).
    pub engine: String,
    pub elevenlabs_model_id: String,
    pub elevenlabs_base_url: String,
    /// Whether an ElevenLabs API key is configured (env or runtime).
    pub elevenlabs_key_set: bool,
    /// Whether `whisper-rs` can be selected (feature compiled + model present).
    pub whisper_available: bool,
}

/// A requested STT change (config page). Absent fields are left unchanged; the API key
/// is tri-state (`None` = keep, `Some(None)` = clear, `Some(Some(v))` = set).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SttUpdate {
    pub engine: Option<String>,
    pub elevenlabs_model_id: Option<String>,
    pub elevenlabs_base_url: Option<String>,
    pub elevenlabs_api_key: Option<Option<String>>,
}

/// A description of the settings currently in effect, for reporting back to the
/// device settings screen or the config page.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsView {
    pub llm_backend: String,
    pub llm_model: Option<String>,
    /// Whether an Anthropic API key is configured (env or runtime). The key itself
    /// is never exposed.
    pub anthropic_key_set: bool,
    /// Whether an OpenAI API key is configured. The key itself is never exposed.
    pub openai_key_set: bool,
    /// Whether a runtime Anthropic subscription OAuth token is set. Never exposed.
    pub anthropic_oauth_token_set: bool,
    /// Whether a Mapbox token (directions tool) is configured. Never exposed.
    pub mapbox_token_set: bool,
    /// Whether an AppSaid sender app token is configured. Never exposed.
    pub appsaid_token_set: bool,
    /// Anthropic auth mode (API key vs subscription OAuth).
    pub anthropic_auth: AnthropicAuth,
    pub tts_voice: Option<String>,
    pub engine: LlmEngine,
    pub web_search: bool,
    pub search_provider: String,
    /// Whether a search API key is configured. The key itself is never exposed.
    pub search_key_set: bool,
    /// End-of-speech trailing silence (ms) the VAD waits for.
    pub end_silence_ms: u64,
    /// Speech-vs-noise RMS threshold for the VAD.
    pub voice_rms_threshold: f64,
    /// Silero speech-probability gate (`0.0..1.0`) for the neural VAD engine.
    pub silero_threshold: f32,
    /// The active VAD engine (energy / silero).
    pub vad_engine: crate::config::VadEngineKind,
    /// What the live LLM backend can do (tools / history / streaming), so the config
    /// page can show what an engine choice loses.
    pub llm_capabilities: crate::llm::Capabilities,
}

impl SettingsView {
    /// The view of a settings snapshot (never exposes a secret, only whether it is set).
    pub fn of(s: &RuntimeSettings) -> Self {
        SettingsView {
            llm_backend: s.llm_backend.clone(),
            llm_model: s.llm_model.clone(),
            anthropic_key_set: is_set(&s.anthropic_api_key),
            openai_key_set: is_set(&s.openai_api_key),
            anthropic_oauth_token_set: is_set(&s.anthropic_oauth_token),
            mapbox_token_set: is_set(&s.mapbox_token),
            appsaid_token_set: s.appsaid.token_set(),
            anthropic_auth: s.anthropic_auth,
            tts_voice: s.tts_voice.clone(),
            engine: s.engine,
            web_search: s.web_search,
            search_provider: s.search_provider.clone(),
            search_key_set: is_set(&s.search_api_key),
            end_silence_ms: s.end_silence_ms,
            voice_rms_threshold: s.voice_rms_threshold,
            silero_threshold: s.silero_threshold,
            vad_engine: s.vad_engine,
            llm_capabilities: s.llm.capabilities(),
        }
    }

    /// The JSON settings payload shared by the config page (`GET`/`POST /config`) and
    /// the device `ambient-settings` response, wrapped in an `ok`/`message` envelope.
    /// The field names are the wire contract both front ends consume — do not rename.
    pub fn to_json(&self, ok: bool, message: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "ok": ok,
            "message": message,
            "llm_backend": self.llm_backend,
            "llm_model": self.llm_model,
            "anthropic_key_set": self.anthropic_key_set,
            "openai_key_set": self.openai_key_set,
            "anthropic_oauth_token_set": self.anthropic_oauth_token_set,
            "anthropic_auth": self.anthropic_auth.as_str(),
            "tts_voice": self.tts_voice,
            "engine": self.engine.as_str(),
            "web_search": self.web_search,
            "search_provider": self.search_provider,
            "search_key_set": self.search_key_set,
            "end_silence_ms": self.end_silence_ms,
            "voice_rms_threshold": self.voice_rms_threshold,
            "silero_threshold": self.silero_threshold,
            "vad_engine": self.vad_engine.as_label(),
            "llm_capabilities": self.llm_capabilities,
        })
    }
}

impl System1View {
    /// The view of a System-1 selection (never the OpenRouter key).
    pub fn of(s: &System1Runtime) -> Self {
        System1View {
            backend: s.backend.clone(),
            base_url: s.base_url.clone(),
            model: s.model.clone(),
            min_confidence: s.min_confidence,
            openrouter_key_set: is_set(&s.openrouter_api_key),
            intents: s.intents.clone(),
        }
    }
}

impl SttView {
    /// The view of an STT selection (never the ElevenLabs key).
    pub fn of(s: &SttRuntime) -> Self {
        SttView {
            engine: s.engine.as_label().to_string(),
            elevenlabs_model_id: s.elevenlabs_model_id.clone(),
            elevenlabs_base_url: s.elevenlabs_base_url.clone(),
            elevenlabs_key_set: is_set(&s.elevenlabs_api_key),
            whisper_available: s.whisper_available,
        }
    }
}

/// A requested settings change. Absent fields are left unchanged; a `tts_voice` of
/// `Some(None)` clears the voice.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsUpdate {
    pub llm_backend: Option<String>,
    pub llm_model: Option<String>,
    /// Anthropic API key change: `None` = leave unchanged; `Some(None)` = clear;
    /// `Some(Some(v))` = set to `v`. Lets the cloud backend be enabled at runtime.
    pub anthropic_api_key: Option<Option<String>>,
    /// OpenAI API key change (same tri-state semantics as `anthropic_api_key`).
    pub openai_api_key: Option<Option<String>>,
    /// Anthropic subscription OAuth token change (tri-state). Applied without a
    /// backend rebuild — the shared token provider is updated in place.
    pub anthropic_oauth_token: Option<Option<String>>,
    /// New Anthropic auth mode, or `None` to leave it unchanged.
    pub anthropic_auth: Option<AnthropicAuth>,
    /// `None` = leave unchanged; `Some(None)` = clear; `Some(Some(v))` = set to `v`.
    pub tts_voice: Option<Option<String>>,
    /// Switch the LLM engine (native vs rig).
    pub engine: Option<LlmEngine>,
    /// Toggle the rig web-search tool.
    pub web_search: Option<bool>,
    /// Switch the search backend (`duckduckgo` / `tavily`).
    pub search_provider: Option<String>,
    /// `None` = leave unchanged; `Some(None)` = clear; `Some(Some(v))` = set.
    pub search_api_key: Option<Option<String>>,
    /// New end-of-speech trailing silence (ms) for the VAD, or `None` to leave it.
    pub end_silence_ms: Option<u64>,
    /// New speech-vs-noise RMS threshold for the VAD, or `None` to leave it.
    pub voice_rms_threshold: Option<f64>,
    /// New Silero speech-probability gate (`0.0..1.0`), or `None` to leave it.
    pub silero_threshold: Option<f32>,
    /// Switch the VAD engine (energy / silero), or `None` to leave it unchanged.
    pub vad_engine: Option<crate::config::VadEngineKind>,
}

impl SettingsUpdate {
    /// Parse a change from the JSON object both front ends send (`POST /config` on the
    /// loopback page; the `ambient-set-settings` control frame from the device). One
    /// parser, so the two paths cannot drift. Semantics:
    ///
    /// - An **absent** key leaves that setting unchanged.
    /// - `llm_backend` / `llm_model` / `search_provider`: a blank string is ignored.
    /// - `tts_voice`: `null` or `""` clears the voice (back to the server default).
    /// - `engine`: parsed with [`LlmEngine::from_label`].
    /// - Secrets (`anthropic_api_key`, `openai_api_key`, `anthropic_oauth_token`,
    ///   `search_api_key`): a blank string is ignored (a page reload never wipes a stored
    ///   key) and an explicit `null` clears. They are honoured **only** when
    ///   `allow_secrets` is true (the loopback config page); the device path passes
    ///   `false`, so cloud secrets are never accepted from the shared device screen.
    pub fn from_json(data: &serde_json::Value, allow_secrets: bool) -> Self {
        use serde_json::Value;
        let string_field = |key: &str| {
            data.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let secret_field = |key: &str| -> Option<Option<String>> {
            if !allow_secrets {
                return None;
            }
            match data.get(key) {
                None => None,
                Some(Value::Null) => Some(None),
                Some(v) => v
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| Some(s.to_string())),
            }
        };
        let tts_voice = match data.get("tts_voice") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v) => Some(
                v.as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            ),
        };
        SettingsUpdate {
            llm_backend: string_field("llm_backend"),
            llm_model: string_field("llm_model"),
            anthropic_api_key: secret_field("anthropic_api_key"),
            openai_api_key: secret_field("openai_api_key"),
            anthropic_oauth_token: secret_field("anthropic_oauth_token"),
            anthropic_auth: data
                .get("anthropic_auth")
                .and_then(Value::as_str)
                .map(AnthropicAuth::from_label),
            tts_voice,
            engine: data
                .get("engine")
                .and_then(Value::as_str)
                .map(LlmEngine::from_label),
            web_search: data.get("web_search").and_then(Value::as_bool),
            search_provider: string_field("search_provider"),
            search_api_key: secret_field("search_api_key"),
            end_silence_ms: data.get("end_silence_ms").and_then(Value::as_u64),
            voice_rms_threshold: data.get("voice_rms_threshold").and_then(Value::as_f64),
            silero_threshold: data
                .get("silero_threshold")
                .and_then(Value::as_f64)
                .map(|v| v as f32),
            vad_engine: data
                .get("vad_engine")
                .and_then(Value::as_str)
                .and_then(crate::config::VadEngineKind::from_label),
        }
    }

    /// Whether this change alters which LLM backend object is needed (so it must be
    /// rebuilt). VAD, voice and the OAuth token apply without a rebuild.
    fn needs_rebuild(&self) -> bool {
        self.llm_backend.is_some()
            || self.llm_model.is_some()
            || self.anthropic_auth.is_some()
            || self.engine.is_some()
            || self.web_search.is_some()
            || self.search_provider.is_some()
            || self.search_api_key.is_some()
            || self.anthropic_api_key.is_some()
            || self.openai_api_key.is_some()
    }
}

/// Apply a tri-state change (`None` = keep, `Some(None)` or a blank value = clear,
/// `Some(Some(v))` = set) to `slot`.
fn apply_tristate(slot: &mut Option<String>, change: &Option<Option<String>>) {
    if let Some(v) = change {
        *slot = non_blank(v);
    }
}

impl RuntimeSettings {
    /// Overlay a persisted settings file onto this boot seed (config file + env
    /// secrets), so page/device changes from a previous run win across restarts.
    /// The inverse of [`PersistedSettings::from_runtime`]. Seeded optional values use
    /// [`overlay_seeded`] (absent/`null` keep the seed, `""` = cleared, else the value);
    /// fields an older file lacks keep the seed. Does **not** rebuild the LLM or
    /// System-1 engine — the caller builds those from the result.
    pub fn overlay_persisted(&mut self, p: PersistedSettings) {
        if !p.engine.trim().is_empty() {
            self.engine = LlmEngine::from_label(&p.engine);
        }
        if let Some(ws) = p.web_search {
            self.web_search = ws;
        }
        if !p.search_provider.trim().is_empty() {
            self.search_provider = p.search_provider;
        }
        self.search_api_key = overlay_seeded(self.search_api_key.take(), p.search_api_key);
        if !p.llm_backend.trim().is_empty() {
            self.llm_backend = p.llm_backend;
            self.llm_model = p.llm_model;
        }
        self.anthropic_api_key = overlay_seeded(self.anthropic_api_key.take(), p.anthropic_api_key);
        self.openai_api_key = overlay_seeded(self.openai_api_key.take(), p.openai_api_key);
        self.anthropic_oauth_token =
            overlay_seeded(self.anthropic_oauth_token.take(), p.anthropic_oauth_token);
        self.mapbox_token = overlay_seeded(self.mapbox_token.take(), p.mapbox_token);
        if let Some(prov) = p.weather_provider.filter(|s| !s.trim().is_empty()) {
            self.weather_provider = prov;
        }
        self.visualcrossing_key =
            overlay_seeded(self.visualcrossing_key.take(), p.visualcrossing_key);
        self.google_places_key = overlay_seeded(self.google_places_key.take(), p.google_places_key);
        self.anthropic_auth = AnthropicAuth::from_label(&p.anthropic_auth);
        self.tts_voice = p.tts_voice;
        self.end_silence_ms = p.end_silence_ms;
        self.voice_rms_threshold = p.voice_rms_threshold;
        if let Some(t) = p.silero_threshold {
            self.silero_threshold = t;
        }
        if let Some(e) = p
            .vad_engine
            .as_deref()
            .and_then(crate::config::VadEngineKind::from_label)
        {
            self.vad_engine = e;
        }
        if let Some(e) = p
            .stt_engine
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .and_then(crate::config::SttEngineKind::from_label)
        {
            self.stt.engine = e;
        }
        if let Some(m) = p.stt_elevenlabs_model_id.filter(|s| !s.trim().is_empty()) {
            self.stt.elevenlabs_model_id = m;
        }
        if let Some(b) = p.stt_elevenlabs_base_url.filter(|s| !s.trim().is_empty()) {
            self.stt.elevenlabs_base_url = b;
        }
        self.stt.elevenlabs_api_key =
            overlay_seeded(self.stt.elevenlabs_api_key.take(), p.stt_elevenlabs_api_key);

        // Drive: a persisted value wins (refresh token, page-set creds/folders); the
        // config-file seed is kept for anything the file leaves absent.
        let d = &mut self.drive;
        d.client_id = overlay_seeded(d.client_id.take(), p.drive.client_id);
        d.client_secret = overlay_seeded(d.client_secret.take(), p.drive.client_secret);
        d.refresh_token = overlay_seeded(d.refresh_token.take(), p.drive.refresh_token);
        if !p.drive.folder_ids.is_empty() {
            d.folder_ids = p.drive.folder_ids;
        }
        d.scope = overlay_seeded(d.scope.take(), p.drive.scope);

        // Household: a persisted address / units wins, but the seed is kept for any
        // field the file leaves empty. A file written before the discrete-field split
        // carries only the legacy single-line `location` (migrated to City by
        // `Household::sanitized`). The member roster is dashboard-only, so a persisted
        // list always replaces the empty seed.
        let h = &mut self.household;
        if !p.household.address.is_empty() {
            h.address = p.household.address;
        }
        if p.household.location.is_some() {
            h.location = p.household.location;
        }
        if p.household.weather_units.is_some() {
            h.weather_units = p.household.weather_units;
        }
        if !p.household.members.is_empty() {
            h.members = p.household.members;
        }

        let sp = &mut self.spotify;
        sp.client_id = overlay_seeded(sp.client_id.take(), p.spotify.client_id);
        sp.client_secret = overlay_seeded(sp.client_secret.take(), p.spotify.client_secret);
        sp.refresh_token = overlay_seeded(sp.refresh_token.take(), p.spotify.refresh_token);
        sp.device_name = overlay_seeded(sp.device_name.take(), p.spotify.device_name);
        sp.redirect_url = overlay_seeded(sp.redirect_url.take(), p.spotify.redirect_url);
        sp.scope = overlay_seeded(sp.scope.take(), p.spotify.scope);

        let c = &mut self.cadora;
        c.base_url = overlay_seeded(c.base_url.take(), p.cadora.base_url);
        c.link_token = overlay_seeded(c.link_token.take(), p.cadora.link_token);

        let a = &mut self.appsaid;
        a.worker_url = overlay_seeded(a.worker_url.take(), p.appsaid.worker_url);
        a.app_token = overlay_seeded(a.app_token.take(), p.appsaid.app_token);
        if !p.appsaid.recipients.is_empty() {
            a.recipients = p.appsaid.recipients;
        }
        a.default_recipient =
            overlay_seeded(a.default_recipient.take(), p.appsaid.default_recipient);

        let ha = &mut self.home_assistant;
        ha.base_url = overlay_seeded(ha.base_url.take(), p.home_assistant.base_url);
        ha.token = overlay_seeded(ha.token.take(), p.home_assistant.token);
        ha.default_room = overlay_seeded(ha.default_room.take(), p.home_assistant.default_room);

        // System-1: only when the persisted backend is non-empty (a real save), so an
        // older file — which lacks these fields — can't disable a config-file seed.
        if !p.system1_backend.is_empty() {
            self.system1.backend = p.system1_backend;
            self.system1.base_url = p.system1_base_url;
            self.system1.model = p.system1_model;
            self.system1.min_confidence = p.system1_min_confidence;
            self.system1.intents = p.system1_intents;
        }
        self.system1.openrouter_api_key =
            overlay_seeded(self.system1.openrouter_api_key.take(), p.openrouter_api_key);

        // Pure runtime/GUI state with no config-file seed: the persisted value wins.
        self.personality = p.personality;
        self.pronunciations = p.pronunciations;
        self.radio = p.radio;
        // Languages: only a page-edited value is persisted (see `PersistedSettings`).
        if let Some(language) = p.language {
            self.language = language.sanitized();
        }
        self.music_display_pins = p.music_display_pins;
    }
}

impl PersistedSettings {
    /// Snapshot the mutable settings for persistence. `seed` is the boot seed (config
    /// file + env secrets, before any persisted overlay): a seeded value that is unset
    /// live but present in the seed is written as `""`, the "cleared" marker, so the
    /// clear survives a restart instead of reverting to the seed (see
    /// [`overlay_seeded`]). With no seed, unset values are written as `null`.
    pub fn from_runtime(s: &RuntimeSettings, seed: Option<&PersistedSettings>) -> Self {
        let sd = |pick: fn(&PersistedSettings) -> &Option<String>, live: &Option<String>| {
            persist_seeded(live, seed.map(pick))
        };
        PersistedSettings {
            engine: s.engine.as_str().to_string(),
            web_search: Some(s.web_search),
            search_provider: s.search_provider.clone(),
            search_api_key: sd(|p| &p.search_api_key, &s.search_api_key),
            llm_backend: s.llm_backend.clone(),
            llm_model: s.llm_model.clone(),
            anthropic_api_key: sd(|p| &p.anthropic_api_key, &s.anthropic_api_key),
            openai_api_key: sd(|p| &p.openai_api_key, &s.openai_api_key),
            anthropic_oauth_token: sd(|p| &p.anthropic_oauth_token, &s.anthropic_oauth_token),
            mapbox_token: sd(|p| &p.mapbox_token, &s.mapbox_token),
            anthropic_auth: s.anthropic_auth.as_str().to_string(),
            tts_voice: s.tts_voice.clone(),
            end_silence_ms: s.end_silence_ms,
            voice_rms_threshold: s.voice_rms_threshold,
            silero_threshold: Some(s.silero_threshold),
            vad_engine: Some(s.vad_engine.as_label().to_string()),
            stt_engine: Some(s.stt.engine.as_label().to_string()),
            stt_elevenlabs_model_id: Some(s.stt.elevenlabs_model_id.clone()),
            stt_elevenlabs_base_url: Some(s.stt.elevenlabs_base_url.clone()),
            stt_elevenlabs_api_key: sd(|p| &p.stt_elevenlabs_api_key, &s.stt.elevenlabs_api_key),
            drive: DriveConfig {
                client_id: sd(|p| &p.drive.client_id, &s.drive.client_id),
                client_secret: sd(|p| &p.drive.client_secret, &s.drive.client_secret),
                refresh_token: sd(|p| &p.drive.refresh_token, &s.drive.refresh_token),
                folder_ids: s.drive.folder_ids.clone(),
                scope: sd(|p| &p.drive.scope, &s.drive.scope),
            },
            household: s.household.clone(),
            spotify: SpotifyConfig {
                client_id: sd(|p| &p.spotify.client_id, &s.spotify.client_id),
                client_secret: sd(|p| &p.spotify.client_secret, &s.spotify.client_secret),
                refresh_token: sd(|p| &p.spotify.refresh_token, &s.spotify.refresh_token),
                device_name: sd(|p| &p.spotify.device_name, &s.spotify.device_name),
                redirect_url: sd(|p| &p.spotify.redirect_url, &s.spotify.redirect_url),
                scope: sd(|p| &p.spotify.scope, &s.spotify.scope),
            },
            cadora: CadoraConfig {
                base_url: sd(|p| &p.cadora.base_url, &s.cadora.base_url),
                link_token: sd(|p| &p.cadora.link_token, &s.cadora.link_token),
            },
            appsaid: AppSaidConfig {
                worker_url: sd(|p| &p.appsaid.worker_url, &s.appsaid.worker_url),
                app_token: sd(|p| &p.appsaid.app_token, &s.appsaid.app_token),
                recipients: s.appsaid.recipients.clone(),
                default_recipient: sd(
                    |p| &p.appsaid.default_recipient,
                    &s.appsaid.default_recipient,
                ),
            },
            home_assistant: HomeAssistantSettings {
                base_url: sd(|p| &p.home_assistant.base_url, &s.home_assistant.base_url),
                token: sd(|p| &p.home_assistant.token, &s.home_assistant.token),
                default_room: sd(
                    |p| &p.home_assistant.default_room,
                    &s.home_assistant.default_room,
                ),
            },
            weather_provider: Some(s.weather_provider.clone()),
            visualcrossing_key: sd(|p| &p.visualcrossing_key, &s.visualcrossing_key),
            google_places_key: sd(|p| &p.google_places_key, &s.google_places_key),
            system1_backend: s.system1.backend.clone(),
            system1_base_url: s.system1.base_url.clone(),
            system1_model: s.system1.model.clone(),
            system1_min_confidence: s.system1.min_confidence,
            system1_intents: s.system1.intents.clone(),
            openrouter_api_key: sd(|p| &p.openrouter_api_key, &s.system1.openrouter_api_key),
            personality: s.personality.clone(),
            pronunciations: s.pronunciations.clone(),
            // The boot seed (no `seed`) always records the language so later snapshots
            // can compare; a live snapshot writes it only when it differs from the seed.
            language: match seed {
                Some(sd) if sd.language.as_ref() == Some(&s.language) => None,
                _ => Some(s.language.clone()),
            },
            radio: s.radio.clone(),
            music_display_pins: s.music_display_pins.clone(),
        }
    }
}

/// Thread-safe holder for the runtime settings plus the factory that rebuilds
/// backends on a swap. Shared (via `Arc`) by every connection.
///
/// Reads ([`Self::snapshot`] and the per-field getters) take the `RwLock` briefly.
/// Every **mutation** (`apply*`, `set_*`, the AppSaid recipient helpers) is serialized
/// by a separate mutation lock held through the whole read-modify-build-swap-persist
/// sequence, so concurrent saves can neither lose each other's updates nor persist a
/// stale snapshot. Mutations are synchronous (no `.await` while the lock is held).
pub struct SharedSettings {
    inner: RwLock<RuntimeSettings>,
    /// Serializes mutations (see the type docs). Never held across an `.await`, and
    /// never taken while holding `inner`.
    mutation: Mutex<()>,
    factory: LlmFactory,
    /// Where to persist changes, or `None` to keep settings in-memory only.
    persist_path: Option<PathBuf>,
    /// The boot seed (config file + env secrets, before the persisted overlay), used to
    /// write the "cleared" marker for a seeded value cleared on the page. `None` ⇒
    /// unset values are written as `null`.
    seed: Option<PersistedSettings>,
    /// Test hook: the factory the most recent LLM rebuild built its tool set from.
    #[cfg(test)]
    last_build: Mutex<Option<LlmFactory>>,
}

/// Held for the duration of one settings mutation.
type MutationGuard<'a> = MutexGuard<'a, ()>;

impl SharedSettings {
    /// Create an in-memory-only shared holder (no persistence).
    pub fn new(factory: LlmFactory, initial: RuntimeSettings) -> Arc<Self> {
        Self::new_persistent(factory, initial, None)
    }

    /// Create a shared holder that persists every applied change to `persist_path`
    /// (when `Some`), reloaded at boot by the caller.
    pub fn new_persistent(
        factory: LlmFactory,
        initial: RuntimeSettings,
        persist_path: Option<PathBuf>,
    ) -> Arc<Self> {
        Self::new_seeded(factory, initial, persist_path, None)
    }

    /// Like [`Self::new_persistent`], also recording the boot `seed` (the
    /// [`PersistedSettings::from_runtime`] snapshot of the config/env seed before the
    /// persisted overlay) so a seeded secret cleared on the page stays cleared across a
    /// restart. Used by `Config::shared_settings`.
    pub fn new_seeded(
        factory: LlmFactory,
        initial: RuntimeSettings,
        persist_path: Option<PathBuf>,
        seed: Option<PersistedSettings>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(initial),
            mutation: Mutex::new(()),
            factory,
            persist_path,
            seed,
            #[cfg(test)]
            last_build: Mutex::new(None),
        })
    }

    /// A fixed holder around an already-built backend whose LLM cannot be swapped
    /// (its factory has no credentials). Used by tests and any caller that only
    /// needs the immutable Phase-4 behavior. The TTS voice is still settable.
    pub fn fixed(
        llm: Arc<dyn LlmBackend>,
        llm_backend: impl Into<String>,
        tts_voice: Option<String>,
    ) -> Arc<Self> {
        let factory = LlmFactory {
            ollama_url: "http://127.0.0.1:11434".to_string(),
            anthropic_base_url: "https://api.anthropic.com".to_string(),
            anthropic_api_key: None,
            anthropic_max_tokens: 1024,
            openai_base_url: "https://api.openai.com".to_string(),
            openai_api_key: None,
            openai_max_tokens: 1024,
            anthropic_token: None,
            home_location: crate::directions::LiveHomeLocation::default(),
            spotify: None,
            cadora: None,
            appsaid: None,
            calendar: None,
            directions: None,
            directions_provider: String::new(),
            directions_imperial: false,
            weather: None,
            weather_imperial: false,
            weather_enabled: false,
            places: None,
            places_enabled: true,
            places_provider: "google".to_string(),
            radio: None,
            home_assistant: None,
            camera: None,
            music_display: Default::default(),
        };
        Self::new(
            factory,
            RuntimeSettings {
                llm,
                engine: LlmEngine::Native,
                web_search: false,
                search_provider: "duckduckgo".to_string(),
                search_api_key: None,
                llm_backend: llm_backend.into(),
                llm_model: None,
                anthropic_api_key: None,
                openai_api_key: None,
                anthropic_oauth_token: None,
                mapbox_token: None,
                anthropic_auth: AnthropicAuth::ApiKey,
                tts_voice,
                end_silence_ms: DEFAULT_END_SILENCE_MS,
                voice_rms_threshold: DEFAULT_VOICE_RMS_THRESHOLD,
                silero_threshold: DEFAULT_SILERO_THRESHOLD,
                vad_engine: crate::config::VadEngineKind::Energy,
                stt: SttRuntime::default(),
                drive: DriveConfig::default(),
                household: Household::default(),
                spotify: SpotifyConfig::default(),
                cadora: CadoraConfig::default(),
                appsaid: AppSaidConfig::default(),
                home_assistant: Default::default(),
                weather_provider: "visualcrossing".to_string(),
                visualcrossing_key: None,
                google_places_key: None,
                system1: System1Runtime::default(),
                personality: Personality::default(),
                pronunciations: Vec::new(),
                language: Default::default(),
                music_display_pins: Default::default(),
                radio: Default::default(),
            },
        )
    }

    // ---- mutation plumbing -------------------------------------------------------

    /// Take the mutation lock (poison-tolerant: a panicked mutator left no partial
    /// state, since the candidate is only swapped in on success).
    fn lock_mutation(&self) -> MutationGuard<'_> {
        self.mutation.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Rebuild the LLM backend (with its full tool set) for the candidate `cand`, via
    /// the single settings → factory mapping ([`LlmFactory::build_for`]).
    fn rebuild_llm(&self, cand: &mut RuntimeSettings) -> Result<()> {
        #[cfg(test)]
        {
            *self.last_build.lock().unwrap() = Some(self.factory.for_settings(cand));
        }
        let (llm, label, model) = self.factory.build_for(cand)?;
        cand.llm = llm;
        cand.llm_backend = label;
        cand.llm_model = model;
        Ok(())
    }

    /// Swap `cand` in as the live settings and persist it. The caller must hold the
    /// mutation lock (`_guard`), so the persisted snapshot is exactly the swapped-in
    /// state and no concurrent mutation can interleave.
    fn commit(&self, _guard: &MutationGuard<'_>, cand: RuntimeSettings) {
        let snapshot = self
            .persist_path
            .is_some()
            .then(|| PersistedSettings::from_runtime(&cand, self.seed.as_ref()));
        *self.inner.write().unwrap() = cand;
        if let (Some(path), Some(snap)) = (&self.persist_path, snapshot) {
            persist(path, &snap);
        }
    }

    /// Run one mutation: under the mutation lock, apply `f` to a candidate copy of the
    /// live settings, then — when `rebuild` names the change — rebuild the LLM
    /// **best-effort** (on failure the new config is still stored and the tool activates
    /// on the next successful rebuild or restart, so linkage is never lost to a
    /// transient backend error), then swap in and persist. Returns `f`'s result.
    fn mutate<R>(&self, rebuild: Option<&str>, f: impl FnOnce(&mut RuntimeSettings) -> R) -> R {
        let guard = self.lock_mutation();
        let mut cand = self.snapshot();
        let out = f(&mut cand);
        if let Some(what) = rebuild {
            if let Err(e) = self.rebuild_llm(&mut cand) {
                log::warn!("{what}: applied config but LLM rebuild failed: {e:#}");
            }
        }
        self.commit(&guard, cand);
        out
    }

    // ---- reads -------------------------------------------------------------------

    /// A snapshot of the live settings (cheap: `Arc`/`String` clones). Taken once
    /// per turn so a mid-turn swap never changes the backend under a running reply.
    pub fn snapshot(&self) -> RuntimeSettings {
        self.inner.read().unwrap().clone()
    }

    /// The current settings, for reporting to the device.
    pub fn view(&self) -> SettingsView {
        SettingsView::of(&self.inner.read().unwrap())
    }

    /// The live credentials the model catalog lists with (provider keys + Anthropic
    /// auth mode), so keys entered on the config page reach the model dropdown.
    pub fn catalog_credentials(&self) -> crate::llm::catalog::CatalogCredentials {
        let s = self.inner.read().unwrap();
        crate::llm::catalog::CatalogCredentials {
            anthropic_api_key: non_blank(&s.anthropic_api_key),
            anthropic_auth: s.anthropic_auth,
            openai_api_key: non_blank(&s.openai_api_key),
        }
    }

    /// The shared Anthropic subscription token provider (whose override tracks the
    /// page-set OAuth token), for anything that must authenticate like the LLM does.
    pub fn anthropic_token_provider(&self) -> Option<Arc<AnthropicTokenProvider>> {
        self.factory.anthropic_token.clone()
    }

    // ---- LLM / VAD settings -------------------------------------------------------

    /// Apply a settings change, rebuilding the LLM backend if the backend or model
    /// changed. On success the swap is atomic and the new [`SettingsView`] is
    /// returned; on failure (e.g. anthropic requested with no key) the current
    /// settings are left untouched and the error is returned.
    pub fn apply(&self, update: &SettingsUpdate) -> Result<SettingsView> {
        let guard = self.lock_mutation();
        let mut cand = self.snapshot();
        if let Some(b) = &update.llm_backend {
            cand.llm_backend = b.clone();
        }
        // A backend change with no explicit model resets to that backend's default; a
        // model-only change keeps the current backend.
        match (&update.llm_backend, &update.llm_model) {
            (_, Some(m)) => cand.llm_model = Some(m.clone()),
            (Some(_), None) => cand.llm_model = None,
            (None, None) => {}
        }
        if let Some(e) = update.engine {
            cand.engine = e;
        }
        if let Some(a) = update.anthropic_auth {
            cand.anthropic_auth = a;
        }
        if let Some(ws) = update.web_search {
            cand.web_search = ws;
        }
        if let Some(p) = &update.search_provider {
            cand.search_provider = p.clone();
        }
        apply_tristate(&mut cand.search_api_key, &update.search_api_key);
        apply_tristate(&mut cand.anthropic_api_key, &update.anthropic_api_key);
        apply_tristate(&mut cand.openai_api_key, &update.openai_api_key);
        apply_tristate(
            &mut cand.anthropic_oauth_token,
            &update.anthropic_oauth_token,
        );
        if let Some(voice) = &update.tts_voice {
            cand.tts_voice = voice.clone().filter(|s| !s.is_empty());
        }
        // VAD tuning needs no backend rebuild — it's read from the per-turn snapshot
        // by `stream_to_transcript`. Clamp to sane ranges so a bad request can't wedge
        // end-of-speech detection.
        if let Some(ms) = update.end_silence_ms {
            cand.end_silence_ms = ms.clamp(150, 5000);
        }
        if let Some(thr) = update.voice_rms_threshold {
            cand.voice_rms_threshold = thr.clamp(0.0, 5000.0);
        }
        if let Some(t) = update.silero_threshold {
            cand.silero_threshold = t.clamp(0.0, 1.0);
        }
        if let Some(e) = update.vad_engine {
            cand.vad_engine = e;
        }

        // Build the new LLM *before* swapping so a failed build never leaves the
        // settings half-changed (the candidate is simply dropped).
        if update.needs_rebuild() {
            self.rebuild_llm(&mut cand)?;
        }
        // The Anthropic subscription OAuth token needs no backend rebuild: the shared
        // token provider is consulted per request, so updating its override cell (and
        // the stored value) takes effect immediately.
        if update.anthropic_oauth_token.is_some() {
            if let Some(provider) = &self.factory.anthropic_token {
                provider.set_override(cand.anthropic_oauth_token.clone());
            }
        }
        let view = SettingsView::of(&cand);
        self.commit(&guard, cand);
        Ok(view)
    }

    // ---- Drive / household / personality / pronunciations -----------------------

    /// A snapshot of the live Google Drive config (client creds + refresh token +
    /// folder ids). Read by the `ambient-get-drive-token` control handler and the
    /// consent flow. Cheap clone.
    pub fn drive(&self) -> DriveConfig {
        self.inner.read().unwrap().drive.clone()
    }

    /// A snapshot of the live household + home information (location, units, people).
    /// Read per-turn when assembling the prompt and by the config dashboard. Cheap
    /// clone.
    pub fn household(&self) -> Household {
        self.inner.read().unwrap().household.clone()
    }

    /// The live home-location handle (the factory's shared cell, updated by
    /// [`apply_household`](Self::apply_household)). Handed to the ambient weather push
    /// so it tracks a config-page location edit without a restart.
    pub fn home_location(&self) -> crate::household::LiveHomeLocation {
        self.factory.home_location.clone()
    }

    /// Store `household` (sanitized) on the candidate, point the location-aware tools'
    /// shared home cell at its address, and re-derive the AppSaid recipient roster from
    /// the members. Returns the cleaned household.
    fn set_household(&self, cand: &mut RuntimeSettings, household: &Household) -> Household {
        let cleaned = household.sanitized();
        // The factory's handle is the same cell every rebuilt backend's tool clones, so
        // this takes effect immediately.
        self.factory.home_location.set(cleaned.address.clone());
        cand.appsaid.recipients = cleaned.appsaid_roster();
        cand.household = cleaned.clone();
        cleaned
    }

    /// Replace the whole household record and persist it (best-effort, 0600). The
    /// dashboard household form is a full-record save (edit location/units + the
    /// roster together), so this sets rather than field-diffs; the value is
    /// [`Household::sanitized`] first (trim, drop blanks / nameless members). Returns
    /// the cleaned value that was stored.
    ///
    /// Because the household is the **source of truth for AppSaid recipients** (each
    /// member's `appsaid` address), a save re-derives the roster and rebuilds the LLM so
    /// the `send_phone_message` tool is advertised/withdrawn to match — best-effort, like
    /// [`Self::apply_appsaid`]: if the rebuild fails the config is still stored and takes
    /// effect on the next successful rebuild or restart.
    pub fn apply_household(&self, household: &Household) -> Household {
        self.mutate(Some("household"), |c| self.set_household(c, household))
    }

    /// The live household languages (`plans/BilingualPlan.md`). Read per turn from the
    /// snapshot; this getter is for the config page.
    pub fn language(&self) -> crate::lang::LanguageSettings {
        self.inner.read().unwrap().language.clone()
    }

    /// Replace the household languages (enabled set, default, per-language voices) and
    /// persist them (best-effort, 0600). The value is
    /// [`sanitized`](crate::lang::LanguageSettings::sanitized) first. Takes effect from
    /// the next turn; never rebuilds the LLM.
    pub fn apply_language(
        &self,
        language: &crate::lang::LanguageSettings,
    ) -> crate::lang::LanguageSettings {
        let cleaned = language.clone().sanitized();
        self.mutate(None, |c| c.language = cleaned.clone());
        cleaned
    }

    /// A snapshot of the live output personality (on/off + selection + catalog). Read
    /// per-turn when assembling the prompt and by the config dashboard. Cheap clone.
    pub fn personality(&self) -> Personality {
        self.inner.read().unwrap().personality.clone()
    }

    /// Replace the whole personality record (enabled, active, catalog) and persist it
    /// (best-effort, 0600). The config-page Personality form is a full-record save, so
    /// this sets rather than field-diffs; the value is [`Personality::sanitized`] first
    /// (trim, drop blank entries, de-dupe keys). Never rebuilds the LLM — personality is
    /// pure output tuning injected into the per-turn prompt. Returns the cleaned value.
    pub fn apply_personality(&self, personality: &Personality) -> Personality {
        let cleaned = personality.sanitized();
        self.mutate(None, |c| c.personality = cleaned.clone());
        cleaned
    }

    /// A snapshot of the live STT pronunciation rewrite table. Read by the config
    /// dashboard; the orchestrator reads it from the per-turn settings snapshot. Cheap
    /// clone.
    pub fn pronunciations(&self) -> Vec<crate::stt::PronunciationRule> {
        self.inner.read().unwrap().pronunciations.clone()
    }

    /// Replace the whole STT pronunciation table and persist it (best-effort, 0600).
    /// The config-page form is a full-record save, so this sets rather than field-diffs;
    /// the value is [`crate::stt::sanitize_rules`]d first (trim, drop blank/duplicate/
    /// self-referential variants, drop unusable rules). Never rebuilds the LLM — the
    /// table is applied to the transcript, not the backend. Returns the cleaned value.
    pub fn apply_pronunciations(
        &self,
        rules: &[crate::stt::PronunciationRule],
    ) -> Vec<crate::stt::PronunciationRule> {
        let cleaned = crate::stt::sanitize_rules(rules);
        self.mutate(None, |c| c.pronunciations = cleaned.clone());
        cleaned
    }

    /// A snapshot of the live internet-radio station catalog.
    pub fn radio(&self) -> RadioCatalog {
        self.inner.read().unwrap().radio.clone()
    }

    /// The shared radio player, or `None` when music routing / the mpv IPC is off.
    pub fn radio_player(&self) -> Option<Arc<RadioPlayer>> {
        self.factory.radio.clone()
    }

    /// Replace the whole station catalog (a full-record save from the config page) and
    /// persist it (best-effort, 0600). The value is [`RadioCatalog::sanitized`] first.
    /// The player's live catalog cell is updated *before* the LLM rebuild so the
    /// rebuilt `radio_control` tool advertises the new list (and appears/disappears as
    /// the list becomes non-empty/empty). Returns the cleaned value.
    pub fn apply_radio(&self, catalog: &RadioCatalog) -> RadioCatalog {
        let cleaned = catalog.sanitized();
        self.mutate(Some("radio"), |c| {
            if let Some(player) = &self.factory.radio {
                player.set_catalog(cleaned.stations.clone());
            }
            c.radio = cleaned.clone();
        });
        cleaned
    }

    /// The live per-display pinned music widgets (`device_id` → style; absent = automatic).
    pub fn music_display_pins(&self) -> MusicDisplayPins {
        self.inner.read().unwrap().music_display_pins.clone()
    }

    /// Pin `device_id`'s music widget (`Some`), or return it to automatic (`None`), and
    /// persist it (best-effort, 0600). Never rebuilds the LLM. Also forgets that display's
    /// choice for the current session so the new pin shows at once; the now-playing push
    /// re-sends on the router's change signal. A blank `device_id` is ignored.
    pub fn set_music_display_pin(&self, device_id: &str, style: Option<MusicDisplayStyle>) {
        let id = device_id.trim().to_string();
        if id.is_empty() {
            return;
        }
        self.mutate(None, |c| match style {
            Some(s) => {
                c.music_display_pins.insert(id.clone(), s);
            }
            None => {
                c.music_display_pins.remove(&id);
            }
        });
        self.factory.music_display.forget(&id);
    }

    /// The shared per-display music-style session state (who started the music, and what
    /// each display chose since). One per Core; see [`MusicDisplayRouter`].
    pub fn music_display_router(&self) -> Arc<MusicDisplayRouter> {
        self.factory.music_display.clone()
    }

    /// Set the active personality by spoken/typed name (the `set_personality` voice tool).
    /// `name` is resolved against the live catalog ([`Personality::resolve`]): a match
    /// enables + selects it; "normal"/"off"/"none" disables. Persists. Returns the active
    /// label (`None` for normal) on success, or an error string listing the valid options.
    pub fn set_active_personality(
        &self,
        name: &str,
    ) -> std::result::Result<Option<String>, String> {
        let guard = self.lock_mutation();
        let mut cand = self.snapshot();
        match cand.personality.resolve(name)? {
            Some(key) => {
                cand.personality.enabled = true;
                cand.personality.active = key;
            }
            None => {
                cand.personality.enabled = false;
                cand.personality.active = String::new();
            }
        }
        let label = cand.personality.active_label();
        self.commit(&guard, cand);
        Ok(label)
    }

    // ---- System-1 / STT / Drive --------------------------------------------------

    /// A snapshot of the live System-1 selection for the config page (never the key).
    pub fn system1_view(&self) -> System1View {
        System1View::of(&self.inner.read().unwrap().system1)
    }

    /// Directly install a System-1 engine object (no persist). Used at boot by
    /// `Pipeline::with_system1` and by tests to inject a scripted engine; the config
    /// page uses [`apply_system1`](Self::apply_system1) instead.
    pub fn set_system1(&self, engine: Arc<dyn crate::system1::DecisionEngine>) {
        let _guard = self.lock_mutation();
        let mut w = self.inner.write().unwrap();
        w.system1.backend = engine.name().to_string();
        w.system1.engine = engine;
    }

    /// Apply a System-1 config change: rebuild the engine (before swapping, so a failed
    /// build leaves settings untouched), swap it in, and persist. Orthogonal to the LLM
    /// backend. Returns the resulting [`System1View`].
    pub fn apply_system1(&self, update: &System1Update) -> Result<System1View> {
        let guard = self.lock_mutation();
        let mut cand = self.snapshot();
        let s1 = &mut cand.system1;
        if let Some(b) = &update.backend {
            s1.backend = b.clone();
        }
        if let Some(u) = &update.base_url {
            s1.base_url = u.clone();
        }
        if let Some(m) = &update.model {
            s1.model = m.clone();
        }
        s1.min_confidence = update
            .min_confidence
            .unwrap_or(s1.min_confidence)
            .clamp(0.0, 1.0);
        if let Some(i) = &update.intents {
            s1.intents = i.clone();
        }
        apply_tristate(&mut s1.openrouter_api_key, &update.openrouter_api_key);
        s1.engine = crate::system1::build(
            &s1.backend,
            &s1.base_url,
            &s1.model,
            s1.openrouter_api_key.clone(),
            s1.min_confidence,
            s1.intents.clone(),
        )?;
        let view = System1View::of(&cand.system1);
        self.commit(&guard, cand);
        Ok(view)
    }

    /// The live STT selection, for the config page. Never exposes the ElevenLabs key.
    pub fn stt_view(&self) -> SttView {
        SttView::of(&self.inner.read().unwrap().stt)
    }

    /// Apply an STT selection change and persist it (best-effort, 0600). The engine is
    /// chosen live per turn from the snapshot, so this never rebuilds anything — the next
    /// turn picks up the new engine. An unknown engine label is rejected. The API key is
    /// tri-state (absent = keep, `Some(None)` = clear, `Some(Some(v))` = set).
    pub fn apply_stt(&self, update: &SttUpdate) -> Result<SttView> {
        let engine = match update.engine.as_deref() {
            None => None,
            Some(label) => Some(
                crate::config::SttEngineKind::from_label(label)
                    .ok_or_else(|| anyhow::anyhow!("unknown stt engine `{label}`"))?,
            ),
        };
        Ok(self.mutate(None, |c| {
            if let Some(e) = engine {
                c.stt.engine = e;
            }
            if let Some(m) = update.elevenlabs_model_id.clone().filter(|s| !s.is_empty()) {
                c.stt.elevenlabs_model_id = m;
            }
            if let Some(b) = update.elevenlabs_base_url.clone().filter(|s| !s.is_empty()) {
                c.stt.elevenlabs_base_url = b;
            }
            apply_tristate(&mut c.stt.elevenlabs_api_key, &update.elevenlabs_api_key);
            SttView::of(&c.stt)
        }))
    }

    /// Apply a Drive config change and persist it (best-effort, 0600). Drive is
    /// orthogonal to the LLM backend, so this never rebuilds anything. Empty-string
    /// sets are treated as clears. Returns the resulting [`DriveConfig`].
    pub fn apply_drive(&self, update: &DriveUpdate) -> DriveConfig {
        self.mutate(None, |c| {
            let d = &mut c.drive;
            apply_tristate(&mut d.client_id, &update.client_id);
            apply_tristate(&mut d.client_secret, &update.client_secret);
            apply_tristate(&mut d.refresh_token, &update.refresh_token);
            if let Some(ids) = &update.folder_ids {
                d.folder_ids = ids
                    .iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            apply_tristate(&mut d.scope, &update.scope);
            d.clone()
        })
    }

    // ---- Spotify / Cadora / AppSaid ----------------------------------------------

    /// A snapshot of the live Spotify config (client creds + refresh token + device
    /// name). Read by the config-page status endpoint and the consent flow.
    pub fn spotify(&self) -> SpotifyConfig {
        self.inner.read().unwrap().spotify.clone()
    }

    /// Apply a Spotify config change, rebuild the LLM so the `spotify_control` tool
    /// is advertised/withdrawn to match, and persist (best-effort, 0600). Empty-string
    /// sets are treated as clears. Returns the resulting [`SpotifyConfig`].
    ///
    /// The rebuild is best-effort: if it fails (e.g. the current cloud backend can no
    /// longer build), the new Spotify config is still stored and persisted — the tool
    /// then activates on the next successful rebuild or restart — so consent is never
    /// lost to a transient backend error.
    pub fn apply_spotify(&self, update: &SpotifyUpdate) -> SpotifyConfig {
        self.mutate(Some("spotify"), |c| {
            let sp = &mut c.spotify;
            apply_tristate(&mut sp.client_id, &update.client_id);
            apply_tristate(&mut sp.client_secret, &update.client_secret);
            apply_tristate(&mut sp.refresh_token, &update.refresh_token);
            apply_tristate(&mut sp.device_name, &update.device_name);
            apply_tristate(&mut sp.redirect_url, &update.redirect_url);
            apply_tristate(&mut sp.scope, &update.scope);
            sp.clone()
        })
    }

    /// A snapshot of the live Cadora shopping-list config (base URL + link presence).
    /// Read by the config-page status endpoint and the pairing flow.
    pub fn cadora(&self) -> CadoraConfig {
        self.inner.read().unwrap().cadora.clone()
    }

    /// Apply a Cadora shopping-list config change, rebuild the LLM so the
    /// `shopping_list_add` tool is advertised/withdrawn to match, and persist
    /// (best-effort, 0600). Empty-string sets are treated as clears. Returns the
    /// resulting [`CadoraConfig`].
    ///
    /// Like [`Self::apply_spotify`], the rebuild is best-effort: if it fails the new
    /// config is still stored and persisted, so the tool activates on the next
    /// successful rebuild or restart — linkage is never lost to a transient error.
    pub fn apply_cadora(&self, update: &CadoraUpdate) -> CadoraConfig {
        self.mutate(Some("cadora"), |c| {
            apply_tristate(&mut c.cadora.base_url, &update.base_url);
            apply_tristate(&mut c.cadora.link_token, &update.link_token);
            c.cadora.clone()
        })
    }

    /// A snapshot of the live AppSaid config (worker URL + recipients + default; never
    /// the app token value). Read by the config-page status endpoint.
    pub fn appsaid(&self) -> AppSaidConfig {
        self.inner.read().unwrap().appsaid.clone()
    }

    /// A snapshot of the live AppSaid app-token presence (never the value). Read by the
    /// config-page status endpoint.
    pub fn appsaid_token_set(&self) -> bool {
        self.inner.read().unwrap().appsaid.token_set()
    }

    /// Link (or re-link) a person's phone: set the matching **household member's**
    /// AppSaid address to `user_key`, **auto-adding** them to the household if no member
    /// matches the name (so a paired contact is always in the household). Persists +
    /// rebuilds the tool like [`Self::apply_household`], re-deriving the recipient
    /// roster, as one atomic mutation. The way the device "Link a phone" pairing stores
    /// a claimed `user_key`. Returns the resulting [`AppSaidConfig`] (its `recipients`
    /// reflect the new member).
    pub fn add_appsaid_recipient(&self, name: &str, user_key: &str) -> AppSaidConfig {
        self.mutate(Some("household"), |c| {
            let mut household = c.household.clone();
            household.set_member_appsaid(name, user_key);
            self.set_household(c, &household);
            c.appsaid.clone()
        })
    }

    /// Make sure `name` is in the household roster, **auto-adding** a bare member (no
    /// contact details) if none matches (case-insensitive). Used by guided voice
    /// enrollment's "new person" path so a freshly enrolled voice is a household member
    /// the prompt can reconcile. Returns the canonical stored name (an existing member
    /// keeps their spelling). A no-op — no persist, no LLM rebuild — when already present.
    pub fn ensure_household_member(&self, name: &str) -> String {
        let name = name.trim();
        if let Some(m) = self.household().member_matching(name) {
            return m.name.clone();
        }
        self.mutate(Some("household"), |c| {
            if let Some(m) = c.household.member_matching(name) {
                return m.name.clone();
            }
            let mut household = c.household.clone();
            household.members.push(HouseholdMember {
                name: name.to_string(),
                ..Default::default()
            });
            self.set_household(c, &household);
            name.to_string()
        })
    }

    /// Unlink a person's phone: clear the matching household member's AppSaid address
    /// (case-insensitive) so they drop off the recipient roster — the member record
    /// itself is kept. No-op if no member matches. Returns the resulting config.
    pub fn remove_appsaid_recipient(&self, name: &str) -> AppSaidConfig {
        let needle = name.trim();
        let present = |h: &Household| {
            h.members
                .iter()
                .any(|m| m.name.trim().eq_ignore_ascii_case(needle))
        };
        if !present(&self.household()) {
            return self.appsaid();
        }
        self.mutate(Some("household"), |c| {
            if present(&c.household) {
                let mut household = c.household.clone();
                household.set_member_appsaid(needle, "");
                self.set_household(c, &household);
            }
            c.appsaid.clone()
        })
    }

    /// A snapshot of the Home Assistant settings (the token is inside — callers that
    /// report status must use [`HomeAssistantSettings::token_set`], never the value).
    pub fn home_assistant_settings(&self) -> HomeAssistantSettings {
        self.inner.read().unwrap().home_assistant.clone()
    }

    /// Apply a Home Assistant settings change (URL / token / default room), reconfigure
    /// the shared hub and rebuild the LLM so `home_control` is advertised/withdrawn, and
    /// persist (0600). Best-effort rebuild like [`Self::apply_appsaid`]. Returns the
    /// resulting settings.
    pub fn apply_home_assistant(&self, update: &HomeAssistantUpdate) -> HomeAssistantSettings {
        self.mutate(Some("home_assistant"), |c| {
            let ha = &mut c.home_assistant;
            apply_tristate(&mut ha.base_url, &update.base_url);
            if let Some(url) = ha.base_url.as_mut() {
                *url = url.trim().trim_end_matches('/').to_string();
            }
            apply_tristate(&mut ha.token, &update.token);
            apply_tristate(&mut ha.default_room, &update.default_room);
            ha.clone()
        })
    }

    /// Apply an AppSaid config change (worker URL, app token, default recipient),
    /// rebuild the LLM so the `send_phone_message` tool is advertised/withdrawn to
    /// match, and persist (best-effort, 0600). Empty-string sets are treated as clears.
    /// The recipient **roster is not set here** — it is always re-derived from the
    /// household (each member's `appsaid` address), the single source of truth. Returns
    /// the resulting [`AppSaidConfig`].
    ///
    /// Like [`Self::apply_cadora`], the rebuild is best-effort: if it fails the new
    /// config is still stored and persisted, so the tool activates on the next
    /// successful rebuild or restart — linkage is never lost to a transient error.
    pub fn apply_appsaid(&self, update: &AppSaidUpdate) -> AppSaidConfig {
        self.mutate(Some("appsaid"), |c| {
            apply_tristate(&mut c.appsaid.worker_url, &update.worker_url);
            apply_tristate(&mut c.appsaid.app_token, &update.app_token);
            apply_tristate(&mut c.appsaid.default_recipient, &update.default_recipient);
            // Keep the roster in lockstep with the household (recipients live on members).
            c.appsaid.recipients = c.household.appsaid_roster();
            c.appsaid.clone()
        })
    }

    // ---- directions / weather / places tools -------------------------------------

    /// A snapshot of the live Mapbox token presence (never the value). Read by the
    /// config-page Tools tab status endpoint.
    pub fn mapbox_token_set(&self) -> bool {
        is_set(&self.inner.read().unwrap().mapbox_token)
    }

    /// Apply a directions-tool config change (the Mapbox token), rebuild the LLM so the
    /// `directions_lookup` tool is advertised/withdrawn to match, and persist
    /// (best-effort, 0600). An empty token is treated as a clear. Like
    /// [`Self::apply_spotify`], the rebuild is best-effort: on failure the token is
    /// still stored, so it activates on the next successful rebuild or restart. Returns
    /// whether a token is now set.
    pub fn apply_directions(&self, update: &DirectionsUpdate) -> bool {
        self.mutate(Some("directions"), |c| {
            apply_tristate(&mut c.mapbox_token, &update.mapbox_token);
            is_set(&c.mapbox_token)
        })
    }

    /// The live weather provider label (`visualcrossing`/`openmeteo`). Read by the
    /// config-page Tools tab status endpoint.
    pub fn weather_provider_label(&self) -> String {
        self.inner.read().unwrap().weather_provider.clone()
    }

    /// Whether a Visual Crossing API key is set (never the value). Read by the config-page
    /// Tools tab status endpoint.
    pub fn visualcrossing_key_set(&self) -> bool {
        is_set(&self.inner.read().unwrap().visualcrossing_key)
    }

    /// Whether the weather feature is enabled at all (`weather.enabled`). When `false`
    /// the tool + ambient push are dormant regardless of the provider/key.
    pub fn weather_enabled(&self) -> bool {
        self.factory.weather_enabled
    }

    /// Whether the `calendar_lookup` tool is active — i.e. at least one web-calendar
    /// subscription is configured (the source is prebuilt once at boot). Read-only
    /// status for the device Settings screen's functions snapshot.
    pub fn calendar_active(&self) -> bool {
        self.factory.calendar.is_some()
    }

    /// The shared Home Assistant hub, or `None` when HA isn't configured. The same
    /// instance the `home_control` tool uses, so the fast path shares its catalog cache.
    pub fn home_assistant(&self) -> Option<Arc<crate::home_assistant::HomeAssistant>> {
        self.factory.home_assistant.clone()
    }

    /// The NVR camera hub (`plans/CameraPlan.md`), or `None` when the camera view isn't
    /// configured. Shared by the fast path, the `show_camera` tool and the control
    /// socket's stream request.
    pub fn camera(&self) -> Option<Arc<crate::camera::CameraHub>> {
        self.factory.camera.clone()
    }

    /// Build a weather provider instance from the *current* live provider label + Visual
    /// Crossing key. Used by the ambient push each tick so a provider/key change on the
    /// config page retargets the push without a restart. `None` when weather is disabled.
    pub fn current_weather_provider(&self) -> Option<Arc<dyn crate::weather::WeatherProvider>> {
        let w = self.inner.read().unwrap();
        crate::weather::from_config(
            self.factory.weather_enabled,
            &w.weather_provider,
            w.visualcrossing_key.as_deref(),
        )
    }

    /// The live Anthropic API key (the env seed overlaid by the persisted settings JSON /
    /// config page), or `None` when unset. Used to seed subsystems built outside the LLM
    /// factory — e.g. GraphRAG entity extraction — so they honor a key stored in the
    /// settings file rather than only the environment.
    pub fn current_anthropic_api_key(&self) -> Option<String> {
        non_blank(&self.inner.read().unwrap().anthropic_api_key)
    }

    /// Apply a weather-tool config change (provider label and/or Visual Crossing key),
    /// rebuild the LLM so `weather_lookup` reflects the new provider, and persist
    /// (best-effort, 0600). An empty key is treated as a clear. Like
    /// [`Self::apply_directions`], the rebuild is best-effort: on failure the new
    /// provider/key is still stored, so it takes effect on the next successful rebuild or
    /// restart. The ambient push picks up the change on its next tick via
    /// [`Self::current_weather_provider`]. Returns `(provider_label, visualcrossing_key_set)`.
    pub fn apply_weather_tool(&self, update: &WeatherToolUpdate) -> (String, bool) {
        self.mutate(Some("weather"), |c| {
            if let Some(p) = update
                .provider
                .as_ref()
                .map(|p| p.trim().to_lowercase())
                .filter(|p| !p.is_empty())
            {
                c.weather_provider = p;
            }
            apply_tristate(&mut c.visualcrossing_key, &update.visualcrossing_key);
            (c.weather_provider.clone(), is_set(&c.visualcrossing_key))
        })
    }

    /// Whether a Google Places API key is set (never the value). Read by the config-page
    /// Tools tab status endpoint.
    pub fn google_places_key_set(&self) -> bool {
        is_set(&self.inner.read().unwrap().google_places_key)
    }

    /// Apply a places-tool config change (the Google Places API key), rebuild the LLM so
    /// `places_lookup` is advertised/withdrawn, and persist (best-effort, 0600). An empty
    /// key is treated as a clear (the tool is withdrawn — Google Places has no keyless
    /// fallback). Like [`Self::apply_weather_tool`] the rebuild is best-effort: on failure
    /// the new key is still stored, so it takes effect on the next successful rebuild or
    /// restart. Returns whether a key is now set.
    pub fn apply_places_tool(&self, update: &PlacesToolUpdate) -> bool {
        self.mutate(Some("places"), |c| {
            apply_tristate(&mut c.google_places_key, &update.google_places_key);
            is_set(&c.google_places_key)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn factory_with_key(key: Option<&str>) -> LlmFactory {
        LlmFactory {
            ollama_url: "http://127.0.0.1:11434".to_string(),
            anthropic_base_url: "https://api.anthropic.com".to_string(),
            anthropic_api_key: key.map(String::from),
            anthropic_max_tokens: 256,
            openai_base_url: "https://api.openai.com".to_string(),
            openai_api_key: None,
            openai_max_tokens: 256,
            anthropic_token: None,
            home_location: crate::directions::LiveHomeLocation::default(),
            spotify: None,
            cadora: None,
            appsaid: None,
            calendar: None,
            directions: None,
            directions_provider: String::new(),
            directions_imperial: false,
            weather: None,
            weather_imperial: false,
            weather_enabled: false,
            places: None,
            places_enabled: true,
            places_provider: "google".to_string(),
            radio: None,
            home_assistant: None,
            camera: None,
            music_display: Default::default(),
        }
    }

    fn shared(factory: LlmFactory) -> Arc<SharedSettings> {
        // Seed the live keys from the factory's env keys, exactly as `Config::
        // shared_settings` does at boot, so a later swap to that backend uses them.
        let anthropic_api_key = factory.anthropic_api_key.clone();
        let openai_api_key = factory.openai_api_key.clone();
        let (llm, label, model) = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "ollama",
                Some("llama3.2"),
                AnthropicAuth::ApiKey,
            )
            .unwrap();
        SharedSettings::new(
            factory,
            RuntimeSettings {
                llm,
                engine: LlmEngine::Native,
                web_search: false,
                search_provider: "duckduckgo".to_string(),
                search_api_key: None,
                llm_backend: label,
                llm_model: model,
                anthropic_api_key,
                openai_api_key,
                anthropic_oauth_token: None,
                mapbox_token: None,
                anthropic_auth: AnthropicAuth::ApiKey,
                tts_voice: None,
                end_silence_ms: DEFAULT_END_SILENCE_MS,
                voice_rms_threshold: DEFAULT_VOICE_RMS_THRESHOLD,
                silero_threshold: DEFAULT_SILERO_THRESHOLD,
                vad_engine: crate::config::VadEngineKind::Energy,
                stt: SttRuntime::default(),
                drive: DriveConfig::default(),
                household: Household::default(),
                spotify: SpotifyConfig::default(),
                cadora: CadoraConfig::default(),
                appsaid: AppSaidConfig::default(),
                home_assistant: Default::default(),
                weather_provider: "visualcrossing".to_string(),
                visualcrossing_key: None,
                google_places_key: None,
                system1: System1Runtime::default(),
                personality: Personality::default(),
                pronunciations: Vec::new(),
                language: Default::default(),
                music_display_pins: Default::default(),
                radio: Default::default(),
            },
        )
    }

    #[test]
    fn add_appsaid_recipient_links_household_members_and_remove_unlinks() {
        let s = shared(factory_with_key(None));
        assert!(s.appsaid().recipients.is_empty());
        assert!(s.household().members.is_empty());

        // Linking auto-adds the person to the household (source of truth) and derives
        // the recipient roster from members' AppSaid addresses.
        s.add_appsaid_recipient("Mom", "key_mom_old");
        s.add_appsaid_recipient("Dad", "key_dad");
        // Re-link the first under different casing — it updates the existing member's
        // address in place (keeping the original spelling), not a "mom"/"Mom" duplicate.
        let cfg = s.add_appsaid_recipient("mom", "key_mom_new");
        assert_eq!(cfg.recipients.len(), 2, "no duplicate for Mom/mom");
        assert_eq!(
            cfg.recipients.get("Mom").map(String::as_str),
            Some("key_mom_new")
        );
        assert_eq!(
            cfg.recipients.get("Dad").map(String::as_str),
            Some("key_dad")
        );

        // Both are now household members, each carrying their AppSaid address.
        let h = s.household();
        assert_eq!(h.members.len(), 2, "paired people are in the household");
        let mom = h.member_matching("mom").unwrap();
        assert_eq!(mom.name, "Mom");
        assert_eq!(mom.appsaid.as_deref(), Some("key_mom_new"));

        // Unlink clears the member's address (keeps the person) and drops them off the
        // roster; case-insensitive.
        let cfg = s.remove_appsaid_recipient("MOM");
        assert!(!cfg.recipients.keys().any(|k| k.eq_ignore_ascii_case("mom")));
        assert_eq!(cfg.recipients.len(), 1);
        assert!(cfg.recipients.contains_key("Dad"));
        let h = s.household();
        assert_eq!(h.members.len(), 2, "the person stays in the household");
        assert_eq!(h.member_matching("Mom").unwrap().appsaid, None);
    }

    #[test]
    fn spoken_names_reconcile_to_the_roster_spelling() {
        let member = |name: &str| HouseholdMember {
            name: name.to_string(),
            ..Default::default()
        };
        let h = Household {
            members: vec![
                member("Brendon"),
                member("Kristina"),
                member("Isabelle Adia"),
                member("Mathew"),
            ],
            ..Default::default()
        };
        let canon = |spoken: &str| h.member_matching_spoken(spoken).map(|m| m.name.clone());
        assert_eq!(canon("Christina").as_deref(), Some("Kristina"));
        assert_eq!(canon("Brendan").as_deref(), Some("Brendon"));
        assert_eq!(canon("isabelle").as_deref(), Some("Isabelle Adia"));
        assert_eq!(canon("Matthew").as_deref(), Some("Mathew"));
        assert_eq!(canon("Zoe"), None, "no sound-alike member");
        // Two equally close members → ambiguous → no match.
        let twins = Household {
            members: vec![member("Jon"), member("Jen")],
            ..Default::default()
        };
        assert!(twins.member_matching_spoken("Jan").is_none());
    }

    #[test]
    fn apply_stt_swaps_engine_and_manages_the_elevenlabs_key() {
        let s = shared(factory_with_key(None));
        // Starts on the default Wyoming engine, no key.
        assert_eq!(s.stt_view().engine, "wyoming");
        assert!(!s.stt_view().elevenlabs_key_set);
        assert_eq!(
            s.snapshot().stt.engine,
            crate::config::SttEngineKind::Wyoming
        );

        // Swap to ElevenLabs and set a key — reflected live in the snapshot.
        let view = s
            .apply_stt(&SttUpdate {
                engine: Some("elevenlabs".to_string()),
                elevenlabs_api_key: Some(Some("xi-secret".to_string())),
                elevenlabs_model_id: Some("scribe_v2_realtime".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(view.engine, "elevenlabs");
        assert!(view.elevenlabs_key_set);
        let snap = s.snapshot();
        assert_eq!(snap.stt.engine, crate::config::SttEngineKind::ElevenLabs);
        assert_eq!(snap.stt.elevenlabs_api_key.as_deref(), Some("xi-secret"));

        // A reload-style save (no key field) keeps the stored key.
        s.apply_stt(&SttUpdate {
            engine: Some("elevenlabs".to_string()),
            ..Default::default()
        })
        .unwrap();
        assert!(s.stt_view().elevenlabs_key_set);

        // Clearing wipes it.
        let view = s
            .apply_stt(&SttUpdate {
                elevenlabs_api_key: Some(None),
                ..Default::default()
            })
            .unwrap();
        assert!(!view.elevenlabs_key_set);

        // An unknown engine label is rejected (and nothing changes).
        assert!(s
            .apply_stt(&SttUpdate {
                engine: Some("nope".to_string()),
                ..Default::default()
            })
            .is_err());
        assert_eq!(s.stt_view().engine, "elevenlabs");
    }

    #[test]
    fn apply_system1_swaps_and_reports_the_backend() {
        let s = shared(factory_with_key(None));
        // Starts disabled.
        assert_eq!(s.system1_view().backend, "none");
        assert_eq!(s.snapshot().system1.engine.name(), "none");

        // Swap to the local laya-serve backend (no network needed to construct it).
        let view = s
            .apply_system1(&System1Update {
                backend: Some("laya-serve".to_string()),
                base_url: Some("http://127.0.0.1:9999".to_string()),
                min_confidence: Some(0.9),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(view.backend, "laya-serve");
        assert!((view.min_confidence - 0.9).abs() < 1e-9);
        assert_eq!(s.snapshot().system1.engine.name(), "laya-serve");

        // An unknown backend is rejected and leaves the current engine untouched.
        let err = s.apply_system1(&System1Update {
            backend: Some("bogus".to_string()),
            ..Default::default()
        });
        assert!(err.is_err());
        assert_eq!(s.system1_view().backend, "laya-serve");

        // Back to disabled.
        s.apply_system1(&System1Update {
            backend: Some("none".to_string()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.snapshot().system1.engine.name(), "none");
    }

    #[test]
    fn subscription_backend_builds_with_a_token_provider() {
        use crate::llm::anthropic_auth::AnthropicTokenProvider;
        let mut factory = factory_with_key(None); // no API key…
        factory.anthropic_token = Some(Arc::new(AnthropicTokenProvider::with_fetcher(Arc::new(
            || Ok("tok".into()),
        ))));
        // …but subscription auth builds anyway, because a token provider is wired.
        let (_, label, model) = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "anthropic",
                None,
                AnthropicAuth::Subscription,
            )
            .unwrap();
        assert_eq!(label, "anthropic");
        assert_eq!(model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn subscription_without_token_provider_is_rejected() {
        let factory = factory_with_key(None); // anthropic_token: None
                                              // `Arc<dyn LlmBackend>` isn't Debug, so take the error via `.err()`.
        let err = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "anthropic",
                None,
                AnthropicAuth::Subscription,
            )
            .err()
            .expect("subscription without a token provider must be rejected");
        assert!(format!("{err:#}").contains("ANTHROPIC_OAUTH_TOKEN"));
    }

    #[test]
    fn apply_persists_settings_and_they_reload() {
        let path = std::env::temp_dir().join(format!(
            "ambient_settings_test_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        let factory = factory_with_key(None);
        let (llm, label, model) = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "ollama",
                Some("llama3.2"),
                AnthropicAuth::ApiKey,
            )
            .unwrap();
        let s = SharedSettings::new_persistent(
            factory,
            RuntimeSettings {
                llm,
                engine: LlmEngine::Native,
                web_search: false,
                search_provider: "duckduckgo".into(),
                search_api_key: None,
                llm_backend: label,
                llm_model: model,
                anthropic_api_key: None,
                openai_api_key: None,
                anthropic_oauth_token: None,
                mapbox_token: None,
                anthropic_auth: AnthropicAuth::ApiKey,
                tts_voice: None,
                end_silence_ms: DEFAULT_END_SILENCE_MS,
                voice_rms_threshold: DEFAULT_VOICE_RMS_THRESHOLD,
                silero_threshold: DEFAULT_SILERO_THRESHOLD,
                vad_engine: crate::config::VadEngineKind::Energy,
                stt: SttRuntime::default(),
                drive: DriveConfig::default(),
                household: Household::default(),
                spotify: SpotifyConfig::default(),
                cadora: CadoraConfig::default(),
                appsaid: AppSaidConfig::default(),
                home_assistant: Default::default(),
                weather_provider: "visualcrossing".to_string(),
                visualcrossing_key: None,
                google_places_key: None,
                system1: System1Runtime::default(),
                personality: Personality::default(),
                pronunciations: Vec::new(),
                language: Default::default(),
                music_display_pins: Default::default(),
                radio: Default::default(),
            },
            Some(path.clone()),
        );

        s.apply(&SettingsUpdate {
            web_search: Some(true),
            search_provider: Some("tavily".into()),
            search_api_key: Some(Some("tvly-secret".into())),
            ..Default::default()
        })
        .unwrap();

        let p = load_persisted(&path).expect("settings file should exist");
        assert_eq!(p.web_search, Some(true));
        assert_eq!(p.search_provider, "tavily");
        assert_eq!(p.search_api_key.as_deref(), Some("tvly-secret"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn apply_sets_and_clamps_silero_threshold() {
        let s = shared(factory_with_key(None));
        // In range: applied verbatim, reflected in the view and the per-turn snapshot.
        let view = s
            .apply(&SettingsUpdate {
                silero_threshold: Some(0.35),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(view.silero_threshold, 0.35);
        assert_eq!(s.snapshot().silero_threshold, 0.35);
        // Out-of-range values clamp to [0, 1] so a bad request can't wedge the gate.
        let hi = s
            .apply(&SettingsUpdate {
                silero_threshold: Some(9.0),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hi.silero_threshold, 1.0);
        let lo = s
            .apply(&SettingsUpdate {
                silero_threshold: Some(-1.0),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(lo.silero_threshold, 0.0);
    }

    #[test]
    fn apply_swaps_the_vad_engine() {
        use crate::config::VadEngineKind;
        let s = shared(factory_with_key(None));
        assert_eq!(s.snapshot().vad_engine, VadEngineKind::Energy);
        let view = s
            .apply(&SettingsUpdate {
                vad_engine: Some(VadEngineKind::Silero),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(view.vad_engine, VadEngineKind::Silero);
        assert_eq!(s.snapshot().vad_engine, VadEngineKind::Silero);
        // And back.
        let view = s
            .apply(&SettingsUpdate {
                vad_engine: Some(VadEngineKind::Energy),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(view.vad_engine, VadEngineKind::Energy);
    }

    #[test]
    fn model_only_change_keeps_backend() {
        let s = shared(factory_with_key(None));
        let view = s
            .apply(&SettingsUpdate {
                llm_model: Some("qwen2.5".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(view.llm_backend, "ollama");
        assert_eq!(view.llm_model.as_deref(), Some("qwen2.5"));
    }

    #[test]
    fn backend_change_without_model_uses_default_model() {
        let s = shared(factory_with_key(Some("sk-test")));
        let view = s
            .apply(&SettingsUpdate {
                llm_backend: Some("anthropic".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(view.llm_backend, "anthropic");
        assert_eq!(view.llm_model.as_deref(), Some("claude-opus-5"));
    }

    /// Build `backend` on `engine` with every provider's credentials available.
    fn build_on(
        engine: LlmEngine,
        backend: &str,
        auth: AnthropicAuth,
    ) -> (Arc<dyn LlmBackend>, String, Option<String>) {
        let mut f = factory_with_key(Some("sk-ant"));
        f.openai_api_key = Some("sk-openai".into());
        f.anthropic_token = Some(Arc::new(AnthropicTokenProvider::with_fetcher(Arc::new(
            || Ok("tok".to_string()),
        ))));
        f.build(engine, false, "duckduckgo", None, backend, None, auth)
            .unwrap()
    }

    /// Every backend runs on rig with tools + history under the rig engine —
    /// including Anthropic subscription auth and OpenAI — and the native engine is
    /// still selectable (plain text, reported as such).
    #[test]
    fn engine_decides_capabilities_for_every_backend() {
        let cases = [
            ("ollama", AnthropicAuth::ApiKey, "llama3.2"),
            ("anthropic", AnthropicAuth::ApiKey, "claude-opus-5"),
            ("anthropic", AnthropicAuth::Subscription, "claude-opus-5"),
            ("openai", AnthropicAuth::ApiKey, "gpt-4o-mini"),
        ];
        for (backend, auth, default_model) in cases {
            let (llm, label, model) = build_on(LlmEngine::Rig, backend, auth);
            assert_eq!(label, backend);
            assert_eq!(model.as_deref(), Some(default_model));
            let caps = llm.capabilities();
            assert!(
                caps.tools && caps.history && caps.streaming,
                "{backend} {auth:?}"
            );

            let (llm, _, _) = build_on(LlmEngine::Native, backend, auth);
            assert_eq!(
                llm.capabilities(),
                crate::llm::Capabilities::TEXT_ONLY,
                "{backend} {auth:?}"
            );
        }
    }

    #[test]
    fn settings_view_reports_the_live_backend_capabilities() {
        let s = shared(factory_with_key(Some("sk-test")));
        // `shared` boots on the native engine.
        let view = s.view();
        assert!(!view.llm_capabilities.tools);
        let json = view.to_json(true, None);
        assert_eq!(
            json["llm_capabilities"],
            serde_json::json!({"tools": false, "history": false, "streaming": true})
        );
        let view = s
            .apply(&SettingsUpdate {
                engine: Some(LlmEngine::Rig),
                ..Default::default()
            })
            .unwrap();
        assert!(view.llm_capabilities.tools && view.llm_capabilities.history);
    }

    #[test]
    fn anthropic_without_key_is_rejected_and_leaves_settings_unchanged() {
        let s = shared(factory_with_key(None));
        let before = s.view();
        let err = s
            .apply(&SettingsUpdate {
                llm_backend: Some("anthropic".to_string()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.to_string().contains("ANTHROPIC_API_KEY"));
        assert_eq!(s.view(), before, "a failed swap is atomic");
    }

    #[test]
    fn runtime_anthropic_key_enables_the_backend_without_a_restart() {
        // No env/boot key, so anthropic is rejected up front.
        let s = shared(factory_with_key(None));
        assert!(!s.view().anthropic_key_set);
        assert!(s
            .apply(&SettingsUpdate {
                llm_backend: Some("anthropic".to_string()),
                ..Default::default()
            })
            .is_err());

        // Supply a key at runtime and select anthropic in the same change — it now
        // builds without a restart, and the view reports the key as set (never the
        // value: SettingsView carries only the boolean).
        let view = s
            .apply(&SettingsUpdate {
                llm_backend: Some("anthropic".to_string()),
                anthropic_api_key: Some(Some("sk-runtime".to_string())),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(view.llm_backend, "anthropic");
        assert_eq!(view.llm_model.as_deref(), Some("claude-opus-5"));
        assert!(view.anthropic_key_set);

        // A later unrelated change keeps the runtime key (unchanged tri-state).
        let view = s
            .apply(&SettingsUpdate {
                llm_model: Some("claude-haiku-4-5".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(view.anthropic_key_set);
        assert_eq!(view.llm_model.as_deref(), Some("claude-haiku-4-5"));

        // Clearing the key (Some(None)) while on anthropic is rejected atomically,
        // so the working backend is never torn down by a bad clear.
        let err = s
            .apply(&SettingsUpdate {
                anthropic_api_key: Some(None),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.to_string().contains("ANTHROPIC_API_KEY"));
        assert!(s.view().anthropic_key_set, "rejected clear is atomic");
    }

    #[test]
    fn runtime_key_is_persisted_so_a_restart_is_not_needed_either() {
        let path = std::env::temp_dir().join(format!(
            "ambient_key_persist_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        let factory = factory_with_key(None);
        let (llm, label, model) = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "ollama",
                Some("llama3.2"),
                AnthropicAuth::ApiKey,
            )
            .unwrap();
        let s = SharedSettings::new_persistent(
            factory,
            RuntimeSettings {
                llm,
                engine: LlmEngine::Native,
                web_search: false,
                search_provider: "duckduckgo".into(),
                search_api_key: None,
                llm_backend: label,
                llm_model: model,
                anthropic_api_key: None,
                openai_api_key: None,
                anthropic_oauth_token: None,
                mapbox_token: None,
                anthropic_auth: AnthropicAuth::ApiKey,
                tts_voice: None,
                end_silence_ms: DEFAULT_END_SILENCE_MS,
                voice_rms_threshold: DEFAULT_VOICE_RMS_THRESHOLD,
                silero_threshold: DEFAULT_SILERO_THRESHOLD,
                vad_engine: crate::config::VadEngineKind::Energy,
                stt: SttRuntime::default(),
                drive: DriveConfig::default(),
                household: Household::default(),
                spotify: SpotifyConfig::default(),
                cadora: CadoraConfig::default(),
                appsaid: AppSaidConfig::default(),
                home_assistant: Default::default(),
                weather_provider: "visualcrossing".to_string(),
                visualcrossing_key: None,
                google_places_key: None,
                system1: System1Runtime::default(),
                personality: Personality::default(),
                pronunciations: Vec::new(),
                language: Default::default(),
                music_display_pins: Default::default(),
                radio: Default::default(),
            },
            Some(path.clone()),
        );

        s.apply(&SettingsUpdate {
            llm_backend: Some("anthropic".into()),
            anthropic_api_key: Some(Some("sk-persist".into())),
            ..Default::default()
        })
        .unwrap();

        // The key lands in the persisted file, so the next boot loads it and the
        // cloud backend is selectable with no environment key and no restart.
        let p = load_persisted(&path).expect("settings file should exist");
        assert_eq!(p.llm_backend, "anthropic");
        assert_eq!(p.anthropic_api_key.as_deref(), Some("sk-persist"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tts_voice_can_be_set_and_cleared() {
        let s = shared(factory_with_key(None));
        let set = s
            .apply(&SettingsUpdate {
                tts_voice: Some(Some("en_US-amy-medium".to_string())),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(set.tts_voice.as_deref(), Some("en_US-amy-medium"));

        let cleared = s
            .apply(&SettingsUpdate {
                tts_voice: Some(None),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(cleared.tts_voice, None);
    }

    #[test]
    fn snapshot_is_stable_across_a_later_swap() {
        let s = shared(factory_with_key(None));
        let snap = s.snapshot();
        s.apply(&SettingsUpdate {
            llm_model: Some("other".to_string()),
            ..Default::default()
        })
        .unwrap();
        // The earlier snapshot still points at the original backend name.
        assert_eq!(snap.llm_model.as_deref(), Some("llama3.2"));
        assert_eq!(s.view().llm_model.as_deref(), Some("other"));
    }

    #[test]
    fn household_sanitize_trims_and_drops_blanks_and_nameless() {
        let messy = Household {
            address: crate::household::HomeAddress {
                address1: Some("  123 Main St  ".into()),
                city: Some("  Austin  ".into()),
                state: Some(" Texas ".into()),
                ..Default::default()
            },
            location: None,
            weather_units: Some("   ".into()), // blank → cleared
            members: vec![
                HouseholdMember {
                    name: "  Alice  ".into(),
                    emails: vec![" alice@example.com ".into(), "".into()],
                    phones: vec!["+1 555 0001".into()],
                    appsaid: Some("  user_alice  ".into()),
                    relationship: Some(" parent ".into()),
                    language: Some(" pt_br ".into()),
                },
                // No name → dropped entirely.
                HouseholdMember {
                    name: "   ".into(),
                    emails: vec!["ghost@example.com".into()],
                    ..Default::default()
                },
            ],
        };
        let clean = messy.sanitized();
        assert_eq!(clean.address.address1.as_deref(), Some("123 Main St"));
        assert_eq!(clean.address.city.as_deref(), Some("Austin"));
        assert_eq!(clean.address.state.as_deref(), Some("Texas"));
        assert_eq!(
            clean.address.full_address().as_deref(),
            Some("123 Main St, Austin, Texas")
        );
        assert_eq!(
            clean.address.weather_location().as_deref(),
            Some("Austin, Texas")
        );
        assert_eq!(clean.location, None);
        assert_eq!(clean.weather_units, None);
        assert_eq!(clean.members.len(), 1);
        assert_eq!(clean.members[0].name, "Alice");
        assert_eq!(clean.members[0].emails, vec!["alice@example.com"]);
        assert_eq!(clean.members[0].appsaid.as_deref(), Some("user_alice"));
        assert_eq!(clean.members[0].relationship.as_deref(), Some("parent"));
        // A language preference is normalized to its canonical code.
        assert_eq!(clean.members[0].language.as_deref(), Some("pt-BR"));
        assert!(!clean.is_empty());
        assert!(Household::default().is_empty());
    }

    #[test]
    fn household_sanitize_migrates_legacy_location_into_city() {
        // An old file / config seed carried a single-line `location` and no `address`.
        let legacy = Household {
            location: Some("  Austin, Texas  ".into()),
            ..Default::default()
        };
        let clean = legacy.sanitized();
        // Folds into City (preserving the old weather behavior) and clears the legacy field.
        assert_eq!(clean.address.city.as_deref(), Some("Austin, Texas"));
        assert_eq!(clean.location, None);
        assert_eq!(
            clean.address.weather_location().as_deref(),
            Some("Austin, Texas")
        );
        assert_eq!(
            clean.address.full_address().as_deref(),
            Some("Austin, Texas")
        );

        // When the discrete address is set, the legacy value is ignored (not merged).
        let both = Household {
            address: crate::household::HomeAddress {
                city: Some("Boston".into()),
                state: Some("MA".into()),
                ..Default::default()
            },
            location: Some("Austin, Texas".into()),
            ..Default::default()
        };
        let clean = both.sanitized();
        assert_eq!(clean.address.city.as_deref(), Some("Boston"));
        assert_eq!(clean.location, None);
    }

    #[test]
    fn household_deserializes_legacy_location_json() {
        // A persisted settings file written before the split stored `household.location`.
        let h: Household =
            serde_json::from_str(r#"{ "location": "Austin, Texas", "weather_units": "imperial" }"#)
                .unwrap();
        assert_eq!(h.location.as_deref(), Some("Austin, Texas"));
        assert!(h.address.is_empty());
        // After sanitize it migrates forward.
        assert_eq!(h.sanitized().address.city.as_deref(), Some("Austin, Texas"));
    }

    #[test]
    fn personality_instruction_resolves_only_when_enabled_and_active() {
        let mut p = Personality::default();
        // Off by default (disabled, no active key).
        assert!(p.instruction().is_none());
        assert!(p.active_label().is_none());
        // Enabled but no active key → still normal.
        p.enabled = true;
        assert!(p.instruction().is_none());
        // Select a built-in by key.
        p.active = "gangster".into();
        assert!(p.instruction().is_some_and(|s| !s.is_empty()));
        assert_eq!(p.active_label().as_deref(), Some("1940s Gangster"));
        // "normal" active key = off even when enabled.
        p.active = "normal".into();
        assert!(p.instruction().is_none());
        // Unknown key = off (no panic).
        p.active = "wizard".into();
        assert!(p.instruction().is_none());
    }

    #[test]
    fn personality_resolve_matches_names_and_rejects_unknown() {
        let p = Personality::default();
        assert_eq!(p.resolve("normal").unwrap(), None);
        assert_eq!(p.resolve("off").unwrap(), None);
        assert_eq!(p.resolve("").unwrap(), None);
        // Exact key, case-insensitive.
        assert_eq!(p.resolve("Gangster").unwrap().as_deref(), Some("gangster"));
        // Exact label.
        assert_eq!(p.resolve("Donald Trump").unwrap().as_deref(), Some("trump"));
        // Unique substring.
        assert_eq!(p.resolve("sarcas").unwrap().as_deref(), Some("sarcastic"));
        // Unknown → Err listing the options.
        assert!(p.resolve("wizard").is_err());
    }

    #[test]
    fn personality_sanitize_trims_dedupes_and_drops_blanks() {
        let messy = Personality {
            enabled: true,
            active: "  trump ".into(),
            definitions: vec![
                PersonalityDef {
                    key: " trump ".into(),
                    label: " Trump ".into(),
                    description: "  loud  ".into(),
                },
                // duplicate key (case-insensitive) → dropped
                PersonalityDef {
                    key: "TRUMP".into(),
                    label: "Dup".into(),
                    description: "x".into(),
                },
                // blank key → dropped
                PersonalityDef {
                    key: "  ".into(),
                    label: "Nameless".into(),
                    description: "y".into(),
                },
                // blank label → dropped
                PersonalityDef {
                    key: "k".into(),
                    label: " ".into(),
                    description: "z".into(),
                },
            ],
        };
        let clean = messy.sanitized();
        assert_eq!(clean.active, "trump");
        assert_eq!(clean.definitions.len(), 1);
        assert_eq!(clean.definitions[0].key, "trump");
        assert_eq!(clean.definitions[0].label, "Trump");
        assert_eq!(clean.definitions[0].description, "loud");
    }

    #[test]
    fn set_active_personality_toggles_and_apply_sanitizes() {
        let s = SharedSettings::fixed(
            std::sync::Arc::new(crate::llm::mock::MockLlm::default()),
            "mock",
            None,
        );
        // Default: off.
        assert!(!s.personality().enabled);
        // Switch on by spoken name.
        let label = s.set_active_personality("gangster").unwrap();
        assert_eq!(label.as_deref(), Some("1940s Gangster"));
        let live = s.personality();
        assert!(live.enabled && live.active == "gangster");
        assert!(live.instruction().is_some());
        // Back to normal.
        assert!(s.set_active_personality("normal").unwrap().is_none());
        assert!(!s.personality().enabled);
        // Unknown name is rejected and leaves state unchanged.
        assert!(s.set_active_personality("wizard").is_err());
        assert!(!s.personality().enabled);
        // Full-record save sanitizes (trim key) and takes effect live.
        let saved = s.apply_personality(&Personality {
            enabled: true,
            active: "x".into(),
            definitions: vec![PersonalityDef {
                key: " x ".into(),
                label: "X".into(),
                description: "be x".into(),
            }],
        });
        assert_eq!(saved.definitions.len(), 1);
        assert_eq!(saved.active, "x");
        assert_eq!(s.personality().instruction().as_deref(), Some("be x"));
    }

    #[test]
    fn spotify_config_controller_only_builds_when_linked() {
        let mut c = SpotifyConfig::default();
        assert!(!c.configured() && !c.linked() && c.controller().is_none());
        assert_eq!(c.device_label(), "Ambient"); // default device name
        c.client_id = Some("cid".into());
        c.client_secret = Some("sec".into());
        assert!(c.configured() && !c.linked() && c.controller().is_none());
        c.refresh_token = Some("rt".into());
        assert!(c.linked() && c.controller().is_some());
    }

    #[test]
    fn apply_household_persists_and_reloads() {
        let path = std::env::temp_dir().join(format!(
            "ambient_household_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        let factory = factory_with_key(None);
        let (llm, label, model) = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "ollama",
                Some("llama3.2"),
                AnthropicAuth::ApiKey,
            )
            .unwrap();
        let s = SharedSettings::new_persistent(
            factory,
            RuntimeSettings {
                llm,
                engine: LlmEngine::Native,
                web_search: false,
                search_provider: "duckduckgo".into(),
                search_api_key: None,
                llm_backend: label,
                llm_model: model,
                anthropic_api_key: None,
                openai_api_key: None,
                anthropic_oauth_token: None,
                mapbox_token: None,
                anthropic_auth: AnthropicAuth::ApiKey,
                tts_voice: None,
                end_silence_ms: DEFAULT_END_SILENCE_MS,
                voice_rms_threshold: DEFAULT_VOICE_RMS_THRESHOLD,
                silero_threshold: DEFAULT_SILERO_THRESHOLD,
                vad_engine: crate::config::VadEngineKind::Energy,
                stt: SttRuntime::default(),
                drive: DriveConfig::default(),
                household: Household::default(),
                spotify: SpotifyConfig::default(),
                cadora: CadoraConfig::default(),
                appsaid: AppSaidConfig::default(),
                home_assistant: Default::default(),
                weather_provider: "visualcrossing".to_string(),
                visualcrossing_key: None,
                google_places_key: None,
                system1: System1Runtime::default(),
                personality: Personality::default(),
                pronunciations: Vec::new(),
                language: Default::default(),
                music_display_pins: Default::default(),
                radio: Default::default(),
            },
            Some(path.clone()),
        );

        let stored = s.apply_household(&Household {
            address: crate::household::HomeAddress {
                city: Some("Boston".into()),
                state: Some("MA".into()),
                ..Default::default()
            },
            location: None,
            weather_units: Some("imperial".into()),
            members: vec![HouseholdMember {
                name: "Bob".into(),
                emails: vec!["bob@example.com".into()],
                phones: vec!["+1 555 0002".into()],
                appsaid: None,
                relationship: None,
                language: None,
            }],
        });
        assert_eq!(stored.members.len(), 1);
        assert_eq!(
            s.household().address.weather_location().as_deref(),
            Some("Boston, MA")
        );
        // The live handle reflects the edit for the location-aware tools.
        assert_eq!(s.home_location().weather().as_deref(), Some("Boston, MA"));

        // It round-trips through the 0600 file so a restart keeps it.
        let p = load_persisted(&path).expect("settings file should exist");
        assert_eq!(p.household.address.city.as_deref(), Some("Boston"));
        assert_eq!(p.household.address.state.as_deref(), Some("MA"));
        assert_eq!(p.household.weather_units.as_deref(), Some("imperial"));
        assert_eq!(p.household.members[0].name, "Bob");
        assert_eq!(p.household.members[0].emails, vec!["bob@example.com"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn apply_pronunciations_sanitizes_persists_and_reloads() {
        let path = std::env::temp_dir().join(format!(
            "ambient_pron_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        let factory = factory_with_key(None);
        let (llm, label, model) = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "ollama",
                Some("llama3.2"),
                AnthropicAuth::ApiKey,
            )
            .unwrap();
        let s = SharedSettings::new_persistent(
            factory,
            RuntimeSettings {
                llm,
                engine: LlmEngine::Native,
                web_search: false,
                search_provider: "duckduckgo".into(),
                search_api_key: None,
                llm_backend: label,
                llm_model: model,
                anthropic_api_key: None,
                openai_api_key: None,
                anthropic_oauth_token: None,
                mapbox_token: None,
                anthropic_auth: AnthropicAuth::ApiKey,
                tts_voice: None,
                end_silence_ms: DEFAULT_END_SILENCE_MS,
                voice_rms_threshold: DEFAULT_VOICE_RMS_THRESHOLD,
                silero_threshold: DEFAULT_SILERO_THRESHOLD,
                vad_engine: crate::config::VadEngineKind::Energy,
                drive: DriveConfig::default(),
                household: Household::default(),
                spotify: SpotifyConfig::default(),
                cadora: CadoraConfig::default(),
                appsaid: AppSaidConfig::default(),
                home_assistant: Default::default(),
                weather_provider: "visualcrossing".to_string(),
                visualcrossing_key: None,
                google_places_key: None,
                system1: System1Runtime::default(),
                stt: SttRuntime::default(),
                personality: Personality::default(),
                pronunciations: Vec::new(),
                language: Default::default(),
                music_display_pins: Default::default(),
                radio: Default::default(),
            },
            Some(path.clone()),
        );

        let cleaned = s.apply_pronunciations(&[
            crate::stt::PronunciationRule {
                canonical: "  Zehrs  ".into(),
                variants: vec![
                    "zairs".into(),
                    "  zairs  ".into(),
                    "".into(),
                    "Zehrs".into(),
                ],
            },
            crate::stt::PronunciationRule {
                canonical: "".into(),
                variants: vec!["dropped".into()],
            },
        ]);
        // Sanitized: trimmed canonical, de-duped/blank/self variants dropped, unusable
        // rule removed.
        assert_eq!(cleaned.len(), 1);
        assert_eq!(cleaned[0].canonical, "Zehrs");
        assert_eq!(cleaned[0].variants, vec!["zairs".to_string()]);
        assert_eq!(s.pronunciations(), cleaned);

        // Round-trips through the 0600 file so a restart keeps it.
        let p = load_persisted(&path).expect("settings file should exist");
        assert_eq!(p.pronunciations.len(), 1);
        assert_eq!(p.pronunciations[0].canonical, "Zehrs");
        assert_eq!(p.pronunciations[0].variants, vec!["zairs".to_string()]);
        assert!(p.music_display_pins.is_empty());

        // A display's pinned music widget persists the same way (and leaves the rest
        // alone); clearing it returns that display to automatic.
        s.set_music_display_pin("kitchen", Some(MusicDisplayStyle::Fullscreen));
        s.set_music_display_pin("den", Some(MusicDisplayStyle::Compact));
        s.set_music_display_pin("  ", Some(MusicDisplayStyle::Fullscreen)); // ignored
        let want = MusicDisplayPins::from([
            ("den".to_string(), MusicDisplayStyle::Compact),
            ("kitchen".to_string(), MusicDisplayStyle::Fullscreen),
        ]);
        assert_eq!(s.music_display_pins(), want);
        let p = load_persisted(&path).expect("settings file should exist");
        assert_eq!(p.music_display_pins, want);
        assert_eq!(p.pronunciations.len(), 1);
        let mut reloaded = s.snapshot();
        reloaded.music_display_pins.clear();
        reloaded.overlay_persisted(p);
        assert_eq!(reloaded.music_display_pins, want);
        s.set_music_display_pin("den", None);
        let p = load_persisted(&path).expect("settings file should exist");
        assert_eq!(
            p.music_display_pins.keys().collect::<Vec<_>>(),
            vec!["kitchen"]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn apply_spotify_links_and_persists() {
        let path = std::env::temp_dir().join(format!(
            "ambient_spotify_test_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        let factory = factory_with_key(None);
        let (llm, label, model) = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "ollama",
                Some("llama3.2"),
                AnthropicAuth::ApiKey,
            )
            .unwrap();
        let s = SharedSettings::new_persistent(
            factory,
            RuntimeSettings {
                llm,
                engine: LlmEngine::Native,
                web_search: false,
                search_provider: "duckduckgo".into(),
                search_api_key: None,
                llm_backend: label,
                llm_model: model,
                anthropic_api_key: None,
                openai_api_key: None,
                anthropic_oauth_token: None,
                mapbox_token: None,
                anthropic_auth: AnthropicAuth::ApiKey,
                tts_voice: None,
                end_silence_ms: DEFAULT_END_SILENCE_MS,
                voice_rms_threshold: DEFAULT_VOICE_RMS_THRESHOLD,
                silero_threshold: DEFAULT_SILERO_THRESHOLD,
                vad_engine: crate::config::VadEngineKind::Energy,
                stt: SttRuntime::default(),
                drive: DriveConfig::default(),
                household: Household::default(),
                spotify: SpotifyConfig::default(),
                cadora: CadoraConfig::default(),
                appsaid: AppSaidConfig::default(),
                home_assistant: Default::default(),
                weather_provider: "visualcrossing".to_string(),
                visualcrossing_key: None,
                google_places_key: None,
                system1: System1Runtime::default(),
                personality: Personality::default(),
                pronunciations: Vec::new(),
                language: Default::default(),
                music_display_pins: Default::default(),
                radio: Default::default(),
            },
            Some(path.clone()),
        );

        // Save creds (not linked yet), then link with a refresh token.
        s.apply_spotify(&SpotifyUpdate {
            client_id: Some(Some("cid".into())),
            client_secret: Some(Some("sec".into())),
            device_name: Some(Some("Kitchen".into())),
            ..Default::default()
        });
        assert!(s.spotify().configured() && !s.spotify().linked());
        s.apply_spotify(&SpotifyUpdate {
            refresh_token: Some(Some("rt".into())),
            ..Default::default()
        });
        assert!(s.spotify().linked());
        assert_eq!(s.spotify().device_label(), "Kitchen");

        // The persisted file carries the linkage so a restart needs no re-consent.
        let reloaded = load_persisted(&path).expect("settings file written");
        assert_eq!(reloaded.spotify.refresh_token.as_deref(), Some("rt"));
        assert_eq!(reloaded.spotify.device_name.as_deref(), Some("Kitchen"));

        // A blank secret on a later save must not wipe the stored one.
        s.apply_spotify(&SpotifyUpdate {
            client_secret: Some(None),
            ..Default::default()
        });
        assert!(!s.spotify().configured(), "secret cleared explicitly");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn apply_directions_sets_mapbox_token_and_persists() {
        let path = std::env::temp_dir().join(format!(
            "ambient_directions_test_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        // A factory whose directions provider is mapbox (empty label ⇒ mapbox).
        let factory = factory_with_key(None);
        let (llm, label, model) = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "ollama",
                Some("llama3.2"),
                AnthropicAuth::ApiKey,
            )
            .unwrap();
        let s = SharedSettings::new_persistent(
            factory,
            RuntimeSettings {
                llm,
                engine: LlmEngine::Native,
                web_search: false,
                search_provider: "duckduckgo".into(),
                search_api_key: None,
                llm_backend: label,
                llm_model: model,
                anthropic_api_key: None,
                openai_api_key: None,
                anthropic_oauth_token: None,
                mapbox_token: None,
                anthropic_auth: AnthropicAuth::ApiKey,
                tts_voice: None,
                end_silence_ms: DEFAULT_END_SILENCE_MS,
                voice_rms_threshold: DEFAULT_VOICE_RMS_THRESHOLD,
                silero_threshold: DEFAULT_SILERO_THRESHOLD,
                vad_engine: crate::config::VadEngineKind::Energy,
                stt: SttRuntime::default(),
                drive: DriveConfig::default(),
                household: Household::default(),
                spotify: SpotifyConfig::default(),
                cadora: CadoraConfig::default(),
                appsaid: AppSaidConfig::default(),
                home_assistant: Default::default(),
                weather_provider: "visualcrossing".to_string(),
                visualcrossing_key: None,
                google_places_key: None,
                system1: System1Runtime::default(),
                personality: Personality::default(),
                pronunciations: Vec::new(),
                language: Default::default(),
                music_display_pins: Default::default(),
                radio: Default::default(),
            },
            Some(path.clone()),
        );

        assert!(!s.view().mapbox_token_set);
        assert!(!s.mapbox_token_set());
        let set = s.apply_directions(&DirectionsUpdate {
            mapbox_token: Some(Some("pk.test-token".into())),
        });
        assert!(set);
        assert!(s.view().mapbox_token_set);

        // Persisted so a restart keeps it; a blank re-save leaves it intact.
        let p = load_persisted(&path).expect("settings file written");
        assert_eq!(p.mapbox_token.as_deref(), Some("pk.test-token"));
        s.apply_directions(&DirectionsUpdate { mapbox_token: None });
        assert!(s.mapbox_token_set(), "blank re-save keeps the token");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn apply_anthropic_oauth_token_overrides_provider_and_persists() {
        use crate::llm::anthropic_auth::AnthropicTokenProvider;
        let path = std::env::temp_dir().join(format!(
            "ambient_oauth_test_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        // Keep an Arc to the provider so we can assert the override was set (the factory
        // clone stored in SharedSettings shares the same instance).
        let provider = Arc::new(AnthropicTokenProvider::new(None));
        let mut factory = factory_with_key(None);
        factory.anthropic_token = Some(provider.clone());
        let (llm, label, model) = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "ollama",
                Some("llama3.2"),
                AnthropicAuth::ApiKey,
            )
            .unwrap();
        let s = SharedSettings::new_persistent(
            factory,
            RuntimeSettings {
                llm,
                engine: LlmEngine::Native,
                web_search: false,
                search_provider: "duckduckgo".into(),
                search_api_key: None,
                llm_backend: label,
                llm_model: model,
                anthropic_api_key: None,
                openai_api_key: None,
                anthropic_oauth_token: None,
                mapbox_token: None,
                anthropic_auth: AnthropicAuth::ApiKey,
                tts_voice: None,
                end_silence_ms: DEFAULT_END_SILENCE_MS,
                voice_rms_threshold: DEFAULT_VOICE_RMS_THRESHOLD,
                silero_threshold: DEFAULT_SILERO_THRESHOLD,
                vad_engine: crate::config::VadEngineKind::Energy,
                stt: SttRuntime::default(),
                drive: DriveConfig::default(),
                household: Household::default(),
                spotify: SpotifyConfig::default(),
                cadora: CadoraConfig::default(),
                appsaid: AppSaidConfig::default(),
                home_assistant: Default::default(),
                weather_provider: "visualcrossing".to_string(),
                visualcrossing_key: None,
                google_places_key: None,
                system1: System1Runtime::default(),
                personality: Personality::default(),
                pronunciations: Vec::new(),
                language: Default::default(),
                music_display_pins: Default::default(),
                radio: Default::default(),
            },
            Some(path.clone()),
        );

        assert!(!s.view().anthropic_oauth_token_set);
        assert!(!provider.has_override());
        let view = s
            .apply(&SettingsUpdate {
                anthropic_oauth_token: Some(Some("oauth-xyz".into())),
                ..Default::default()
            })
            .unwrap();
        assert!(view.anthropic_oauth_token_set);
        assert!(
            provider.has_override(),
            "the shared provider got the override"
        );

        // Persisted (plaintext, 0600) so a restart re-applies it; never leaked in view.
        let p = load_persisted(&path).expect("settings file written");
        assert_eq!(p.anthropic_oauth_token.as_deref(), Some("oauth-xyz"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn apply_weather_tool_switches_provider_and_key() {
        let mut factory = factory_with_key(None);
        factory.weather_enabled = true;
        let s = shared(factory);
        // Seed defaults: Visual Crossing selected, no key yet.
        assert_eq!(s.weather_provider_label(), "visualcrossing");
        assert!(!s.visualcrossing_key_set());
        assert!(s.weather_enabled());

        // Set a Visual Crossing key.
        let (prov, key_set) = s.apply_weather_tool(&WeatherToolUpdate {
            provider: None,
            visualcrossing_key: Some(Some("vc-key".into())),
        });
        assert_eq!(prov, "visualcrossing");
        assert!(key_set);
        assert!(s.visualcrossing_key_set());
        assert!(s.current_weather_provider().is_some());

        // Switch to Open-Meteo (case-insensitive, trimmed); the key is retained.
        let (prov, _) = s.apply_weather_tool(&WeatherToolUpdate {
            provider: Some("  OpenMeteo ".into()),
            visualcrossing_key: None,
        });
        assert_eq!(prov, "openmeteo");
        assert!(s.visualcrossing_key_set());

        // Clear the key.
        let (_, key_set) = s.apply_weather_tool(&WeatherToolUpdate {
            provider: None,
            visualcrossing_key: Some(None),
        });
        assert!(!key_set);
        assert!(!s.visualcrossing_key_set());
    }

    #[test]
    fn disabled_weather_never_builds_a_push_provider() {
        let mut factory = factory_with_key(None);
        factory.weather_enabled = false;
        let s = shared(factory);
        // Even with a provider/key set, a disabled feature yields no push provider.
        s.apply_weather_tool(&WeatherToolUpdate {
            provider: Some("visualcrossing".into()),
            visualcrossing_key: Some(Some("k".into())),
        });
        assert!(!s.weather_enabled());
        assert!(s.current_weather_provider().is_none());
    }

    // ---- A1: one settings → tool-state mapping for every rebuild -------------------

    /// A base runtime built from `factory` (ollama/native, live keys seeded from the
    /// factory's env keys, exactly as `Config::shared_settings` does at boot).
    fn runtime_for(factory: &LlmFactory) -> RuntimeSettings {
        shared(factory.clone()).snapshot()
    }

    /// A factory whose directions/weather/places providers can actually be built.
    fn tool_factory() -> LlmFactory {
        let mut f = factory_with_key(None);
        f.directions_provider = "mapbox".to_string();
        f.weather_enabled = true;
        f
    }

    /// The factory the most recent rebuild derived its tool set from.
    fn last_build(s: &SharedSettings) -> LlmFactory {
        s.last_build
            .lock()
            .unwrap()
            .clone()
            .expect("a rebuild should have run")
    }

    fn link_appsaid(s: &SharedSettings) {
        s.apply_appsaid(&AppSaidUpdate {
            worker_url: Some(Some("https://appsaid.example.workers.dev".into())),
            app_token: Some(Some("app-token".into())),
            ..Default::default()
        });
        s.add_appsaid_recipient("Mom", "key_mom");
        assert!(
            last_build(s).appsaid.is_some(),
            "send_phone_message is live"
        );
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "anamanti-settings-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn home_assistant_save_configures_the_shared_hub_live() {
        let mut f = tool_factory();
        let hub = crate::home_assistant::from_config(&Default::default());
        f.home_assistant = Some(hub.clone());
        let s = shared(f);
        let advertised = |s: &SharedSettings| {
            last_build(s)
                .tool_deps(false, "duckduckgo", None)
                .home_assistant
                .is_some()
        };
        assert!(!hub.is_configured());

        let out = s.apply_home_assistant(&HomeAssistantUpdate {
            base_url: Some(Some(" http://ha.local:8123/ ".into())),
            default_room: Some(Some("Kitchen".into())),
            ..Default::default()
        });
        assert_eq!(out.base_url.as_deref(), Some("http://ha.local:8123"));
        assert!(!hub.is_configured(), "no token yet");
        assert!(!advertised(&s), "home_control needs a token");

        s.apply_home_assistant(&HomeAssistantUpdate {
            token: Some(Some("ha-token".into())),
            ..Default::default()
        });
        assert!(hub.is_configured());
        assert_eq!(hub.default_room().as_deref(), Some("Kitchen"));
        assert!(advertised(&s), "home_control is live");

        // An unrelated save keeps it.
        s.apply_places_tool(&PlacesToolUpdate {
            google_places_key: Some(Some("gp-key".into())),
        });
        assert!(hub.is_configured() && advertised(&s));

        // Clearing the token turns the fast path and the tool off.
        s.apply_home_assistant(&HomeAssistantUpdate {
            token: Some(None),
            ..Default::default()
        });
        assert!(!hub.is_configured());
        assert!(!advertised(&s));
    }

    /// Live, **read-only**: apply the URL + token exactly as the config page does, run
    /// "Test connection", and answer a status question through the fast path against
    /// the real Home Assistant. Never switches anything. Ignored by default:
    /// `HOME_ASSISTANT_URL=… HOME_ASSISTANT_TOKEN=… cargo test --lib live_settings_to_home_assistant -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a live Home Assistant (HOME_ASSISTANT_URL + HOME_ASSISTANT_TOKEN)"]
    async fn live_settings_to_home_assistant() {
        let mut f = tool_factory();
        let hub = crate::home_assistant::from_config(&Default::default());
        f.home_assistant = Some(hub.clone());
        let s = shared(f);
        s.apply_home_assistant(&HomeAssistantUpdate {
            base_url: Some(std::env::var("HOME_ASSISTANT_URL").ok()),
            token: Some(std::env::var("HOME_ASSISTANT_TOKEN").ok()),
            default_room: Some(Some("Office".into())),
        });
        assert!(hub.is_configured());
        let (entities, rooms, ms) = hub.test_connection().await.expect("test connection");
        println!("test connection: {entities} devices in {rooms} rooms ({ms} ms)");
        assert!(entities > 0);
        for q in [
            "is the porch light on",
            "are the office lights on",
            "are the lights on",
        ] {
            let t0 = std::time::Instant::now();
            let said = hub.handle_utterance(q, None).await;
            println!("{q:30} → {said:?} ({} ms)", t0.elapsed().as_millis());
            assert!(said.is_some(), "{q}");
        }
    }

    #[test]
    fn home_assistant_settings_persist_and_reload() {
        let mut live = runtime_for(&factory_with_key(None));
        live.home_assistant = HomeAssistantSettings {
            base_url: Some("http://ha.local:8123".into()),
            token: Some("ha-token".into()),
            default_room: Some("Kitchen".into()),
        };
        let json = serde_json::to_value(PersistedSettings::from_runtime(&live, None)).unwrap();
        let mut fresh = runtime_for(&factory_with_key(None));
        fresh.overlay_persisted(serde_json::from_value(json.clone()).unwrap());
        assert_eq!(fresh.home_assistant, live.home_assistant);

        // An older file without the block keeps the boot seed.
        let mut older = json.clone();
        older.as_object_mut().unwrap().remove("home_assistant");
        let mut seeded = runtime_for(&factory_with_key(None));
        seeded.home_assistant.base_url = Some("http://seed:8123".into());
        seeded.overlay_persisted(serde_json::from_value(older).unwrap());
        assert_eq!(
            seeded.home_assistant.base_url.as_deref(),
            Some("http://seed:8123")
        );

        // A token cleared on the page (seeded from env at boot) stays cleared.
        let seed = PersistedSettings::from_runtime(&live, None);
        let mut cleared = live.clone();
        cleared.home_assistant.token = None;
        let snap = PersistedSettings::from_runtime(&cleared, Some(&seed));
        assert_eq!(snap.home_assistant.token.as_deref(), Some(""));
        let mut reboot = live.clone();
        reboot.overlay_persisted(snap);
        assert_eq!(reboot.home_assistant.token, None);
    }

    #[test]
    fn weather_and_places_saves_keep_send_phone_message() {
        let s = shared(tool_factory());
        link_appsaid(&s);

        s.apply_weather_tool(&WeatherToolUpdate {
            provider: Some("openmeteo".into()),
            ..Default::default()
        });
        assert!(
            last_build(&s).appsaid.is_some(),
            "a weather save dropped send_phone_message"
        );

        s.apply_places_tool(&PlacesToolUpdate {
            google_places_key: Some(Some("gp-key".into())),
        });
        assert!(
            last_build(&s).appsaid.is_some(),
            "a places save dropped send_phone_message"
        );
    }

    #[test]
    fn household_and_appsaid_saves_keep_runtime_tool_keys() {
        let s = shared(tool_factory());
        // Keys entered at runtime on the Tools tab (none were in the boot factory).
        s.apply_directions(&DirectionsUpdate {
            mapbox_token: Some(Some("pk.mapbox".into())),
        });
        s.apply_weather_tool(&WeatherToolUpdate {
            provider: Some("visualcrossing".into()),
            visualcrossing_key: Some(Some("vc-key".into())),
        });
        s.apply_places_tool(&PlacesToolUpdate {
            google_places_key: Some(Some("gp-key".into())),
        });
        let check = |what: &str| {
            let f = last_build(&s);
            assert!(f.directions.is_some(), "{what} dropped directions_lookup");
            assert!(f.weather.is_some(), "{what} dropped weather_lookup");
            assert!(f.places.is_some(), "{what} dropped places_lookup");
        };
        check("the tools saves");

        s.apply_household(&Household {
            members: vec![HouseholdMember {
                name: "Dad".into(),
                appsaid: Some("key_dad".into()),
                ..Default::default()
            }],
            ..Default::default()
        });
        check("a household save");
        s.apply_appsaid(&AppSaidUpdate {
            worker_url: Some(Some("https://appsaid.example.workers.dev".into())),
            ..Default::default()
        });
        check("an appsaid save");
        s.add_appsaid_recipient("Mom", "key_mom");
        check("a phone pairing");
    }

    #[test]
    fn places_disabled_survives_every_save() {
        let mut f = tool_factory();
        f.places_enabled = false;
        let s = shared(f);
        let off = |what: &str| {
            assert!(
                last_build(&s).places.is_none(),
                "{what} re-enabled places_lookup despite places.enabled=false"
            );
        };
        s.apply_places_tool(&PlacesToolUpdate {
            google_places_key: Some(Some("gp-key".into())),
        });
        off("a places save");
        s.apply(&SettingsUpdate {
            llm_model: Some("llama3.2".into()),
            ..Default::default()
        })
        .unwrap();
        off("a settings save");
        s.apply_spotify(&SpotifyUpdate {
            device_name: Some(Some("Den".into())),
            ..Default::default()
        });
        off("a spotify save");
        s.apply_cadora(&CadoraUpdate {
            link_token: Some(Some("vl_x".into())),
            ..Default::default()
        });
        off("a cadora save");
        s.apply_appsaid(&AppSaidUpdate::default());
        off("an appsaid save");
        s.apply_household(&Household::default());
        off("a household save");
        s.apply_directions(&DirectionsUpdate {
            mapbox_token: Some(Some("pk.x".into())),
        });
        off("a directions save");
        s.apply_weather_tool(&WeatherToolUpdate::default());
        off("a weather save");
        s.add_appsaid_recipient("Mom", "key_mom");
        off("a phone pairing");
    }

    #[test]
    fn llm_engine_default_matches_the_config_default() {
        assert_eq!(LlmEngine::default(), LlmEngine::Rig);
        assert_eq!(
            crate::config::Config::default().engine,
            LlmEngine::default()
        );
    }

    // ---- A11: atomic persistence, corrupt-file quarantine, serialized mutations ----

    #[cfg(unix)]
    #[test]
    fn persist_is_atomic_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_dir("atomic");
        let path = dir.join("anamanti_settings.json");
        // A pre-existing, too-permissive file is replaced and tightened to 0600.
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let f = factory_with_key(None);
        let s = SharedSettings::new_persistent(f.clone(), runtime_for(&f), Some(path.clone()));
        s.apply(&SettingsUpdate {
            tts_voice: Some(Some("en_US-amy-medium".into())),
            ..Default::default()
        })
        .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "settings file must be owner-only");
        let p = load_persisted(&path).expect("valid file");
        assert_eq!(p.tts_voice.as_deref(), Some("en_US-amy-medium"));
        // No temp file is left behind next to it.
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["anamanti_settings.json".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_settings_file_is_moved_aside_not_overwritten() {
        let dir = unique_dir("corrupt");
        let path = dir.join("anamanti_settings.json");
        std::fs::write(&path, "{ \"engine\": \"rig\", truncated").unwrap();

        assert!(load_persisted(&path).is_none());
        assert!(!path.exists(), "the bad file was moved aside");
        let aside: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("anamanti_settings.json.corrupt-")
            })
            .collect();
        assert_eq!(aside.len(), 1, "one quarantined copy");

        // The next save writes a fresh file and leaves the quarantined copy intact.
        let f = factory_with_key(None);
        let s = SharedSettings::new_persistent(f.clone(), runtime_for(&f), Some(path.clone()));
        s.apply_personality(&Personality::default());
        assert!(load_persisted(&path).is_some());
        assert_eq!(
            std::fs::read_to_string(&aside[0]).unwrap(),
            "{ \"engine\": \"rig\", truncated"
        );
        // A missing file is simply absent — nothing is created or renamed.
        assert!(load_persisted(&dir.join("nope.json")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_saves_do_not_lose_updates() {
        let dir = unique_dir("concurrent");
        let path = dir.join("anamanti_settings.json");
        let f = tool_factory();
        let s = SharedSettings::new_persistent(f.clone(), runtime_for(&f), Some(path.clone()));

        // Each thread read-modify-writes the household (pairing a phone) while others
        // save unrelated settings; every pairing must survive, in memory and on disk.
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let s = s.clone();
                std::thread::spawn(move || {
                    s.add_appsaid_recipient(&format!("Person{i}"), &format!("key{i}"));
                    let provider = if i % 2 == 0 {
                        "openmeteo"
                    } else {
                        "visualcrossing"
                    };
                    s.apply_weather_tool(&WeatherToolUpdate {
                        provider: Some(provider.into()),
                        ..Default::default()
                    });
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(s.household().members.len(), 8, "no pairing lost in memory");
        assert_eq!(s.appsaid().recipients.len(), 8);
        let p = load_persisted(&path).unwrap();
        assert_eq!(p.household.members.len(), 8, "the file is the final state");
        assert_eq!(p.appsaid.recipients.len(), 8);
        assert_eq!(
            p.weather_provider.as_deref(),
            Some(s.weather_provider_label().as_str())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- A12: absent vs cleared, and full persisted round trips --------------------

    #[test]
    fn cleared_seeded_secret_stays_cleared_but_an_absent_one_keeps_the_seed() {
        let dir = unique_dir("cleared");
        let path = dir.join("anamanti_settings.json");
        // Boot seed: ANTHROPIC_API_KEY + MAPBOX_TOKEN came from the environment.
        let f = factory_with_key(Some("env-anthropic"));
        let mut seed_rt = runtime_for(&f);
        seed_rt.mapbox_token = Some("env-mapbox".into());
        let seed = PersistedSettings::from_runtime(&seed_rt, None);
        let s =
            SharedSettings::new_seeded(f.clone(), seed_rt.clone(), Some(path.clone()), Some(seed));

        // Clear both on the page.
        s.apply(&SettingsUpdate {
            anthropic_api_key: Some(None),
            ..Default::default()
        })
        .unwrap();
        s.apply_directions(&DirectionsUpdate {
            mapbox_token: Some(None),
        });

        // "Restart": the same env seed, overlaid with the persisted file.
        let mut boot = seed_rt.clone();
        boot.overlay_persisted(load_persisted(&path).unwrap());
        assert_eq!(boot.anthropic_api_key, None, "cleared key revived from env");
        assert_eq!(boot.mapbox_token, None, "cleared token revived from env");

        // A file where the key is simply absent (or null, as older builds wrote it)
        // keeps the env fallback.
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("anthropic_api_key");
        v["mapbox_token"] = serde_json::Value::Null;
        let mut boot = seed_rt.clone();
        boot.overlay_persisted(serde_json::from_value(v).unwrap());
        assert_eq!(boot.anthropic_api_key.as_deref(), Some("env-anthropic"));
        assert_eq!(boot.mapbox_token.as_deref(), Some("env-mapbox"));

        // With no seed, a never-set secret is written as null (not the cleared marker),
        // so adding the env var later still takes effect.
        let unseeded = PersistedSettings::from_runtime(&runtime_for(&factory_with_key(None)), None);
        assert_eq!(unseeded.anthropic_api_key, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spotify_redirect_url_is_persisted_and_restored() {
        let dir = unique_dir("redirect");
        let path = dir.join("anamanti_settings.json");
        let f = factory_with_key(None);
        let s = SharedSettings::new_persistent(f.clone(), runtime_for(&f), Some(path.clone()));
        s.apply_spotify(&SpotifyUpdate {
            redirect_url: Some(Some("http://127.0.0.1:9999/cb".into())),
            ..Default::default()
        });
        let mut boot = runtime_for(&f);
        boot.overlay_persisted(load_persisted(&path).unwrap());
        assert_eq!(
            boot.spotify.redirect_url_or_default(),
            "http://127.0.0.1:9999/cb"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_minimal_persisted_file_loads_and_keeps_the_seed() {
        // Every field may be absent; `engine` in particular keeps the seed rather than
        // failing the whole file.
        let p: PersistedSettings = serde_json::from_str("{}").unwrap();
        let f = factory_with_key(Some("env-anthropic"));
        let mut seed = runtime_for(&f);
        seed.engine = LlmEngine::Rig;
        seed.web_search = true;
        let mut boot = seed.clone();
        boot.overlay_persisted(p);
        assert_eq!(boot.engine, LlmEngine::Rig);
        assert!(boot.web_search);
        assert_eq!(boot.llm_backend, seed.llm_backend);
        assert_eq!(boot.anthropic_api_key.as_deref(), Some("env-anthropic"));
    }

    // ---- D4: one parser for both front ends ---------------------------------------

    #[test]
    fn settings_update_from_json_gates_secrets_and_has_one_blank_semantics() {
        let data = serde_json::json!({
            "llm_backend": " anthropic ",
            "llm_model": "",
            "engine": "rig-core",
            "tts_voice": "",
            "search_api_key": "tvly-x",
            "anthropic_api_key": "sk-x",
            "openai_api_key": null,
            "anthropic_oauth_token": "",
        });
        // Device path: no secret is ever accepted — search_api_key included.
        let dev = SettingsUpdate::from_json(&data, false);
        assert_eq!(dev.search_api_key, None);
        assert_eq!(dev.anthropic_api_key, None);
        assert_eq!(dev.openai_api_key, None);
        assert_eq!(dev.anthropic_oauth_token, None);
        assert_eq!(dev.llm_backend.as_deref(), Some("anthropic"));
        assert_eq!(dev.llm_model, None, "blank model ignored");
        assert_eq!(dev.engine, Some(LlmEngine::Rig));
        assert_eq!(dev.tts_voice, Some(None), "blank voice clears");

        // Page path: set / clear (null) / ignore (blank) — the same for every secret.
        let web = SettingsUpdate::from_json(&data, true);
        assert_eq!(web.search_api_key, Some(Some("tvly-x".into())));
        assert_eq!(web.anthropic_api_key, Some(Some("sk-x".into())));
        assert_eq!(web.openai_api_key, Some(None));
        assert_eq!(web.anthropic_oauth_token, None);
        let blank = SettingsUpdate::from_json(&serde_json::json!({"search_api_key": ""}), true);
        assert_eq!(
            blank.search_api_key, None,
            "a blank key never wipes the stored one"
        );
    }
    /// Saving the radio catalog sanitizes it, refreshes the shared player's live catalog,
    /// and rebuilds so `radio_control` is advertised exactly while a station exists.
    #[test]
    fn apply_radio_refreshes_the_player_and_the_tool() {
        use crate::music::{RadioPlayer, RadioStation};
        let player = Arc::new(RadioPlayer::new(Arc::new(
            crate::llm::rig::tools::test_support::StaticRadioOutput,
        )));
        let mut f = factory_with_key(None);
        f.radio = Some(player.clone());
        let s = shared(f);

        let saved = s.apply_radio(&RadioCatalog {
            stations: vec![RadioStation {
                id: " cjdv ".into(),
                name: "107.5 Dave Rocks".into(),
                url: "https://corus.leanstream.co/CJDVFM-MP3".into(),
                ..Default::default()
            }],
        });
        assert_eq!(saved.stations[0].id, "CJDV");
        assert_eq!(s.radio(), saved);
        assert_eq!(player.stations(), saved.stations);
        let deps = last_build(&s).tool_deps(false, "duckduckgo", None);
        assert!(
            deps.radio.is_some(),
            "radio_control advertised with a station"
        );

        s.apply_radio(&RadioCatalog::default());
        assert!(!player.has_stations());
        let deps = last_build(&s).tool_deps(false, "duckduckgo", None);
        assert!(deps.radio.is_none(), "no stations ⇒ no radio_control");
    }
}
