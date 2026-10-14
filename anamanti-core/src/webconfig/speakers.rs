//! Speaker tagging for the admin console (SpeakerScopedMemoryPlan.md §8).
//!
//! Every memory and every conversation turn the console shows is tagged with **who it
//! belongs to and how sure we were**, using the person's *current* name rather than a
//! raw `spk-…` id. [`SpeakerDirectory`] is built once per request from the voiceprint
//! registry, the persisted speaker aliases (merged / forgotten ids), the household
//! roster (canonical spelling + relationship) and the conversations-page turn
//! corrections, so renaming, merging or forgetting a voice updates every past turn and
//! memory on the next page load without rewriting the append-only chat log.
//!
//! It also owns the console's review/correction endpoints:
//!
//! - `GET /speakers.json` — the directory itself (filter menus + chip colours);
//! - `POST /memory/attribute` — `{ "id": 7, "to": "spk-…" | "household" | "unknown" }`
//!   or, for the review queue's bulk "Confirm all for …", `{ "ids": [7, 8], "to": … }`;
//! - `POST /memory/delete` — `{ "id": 7 }` or `{ "ids": [7, 8] }`;
//! - `POST /conversations/attribute` — `{ "turn_id": "t-…", "to": "spk-…" | "unknown" }`
//!   ("This was → person"): records a turn-attribution override and re-scopes the
//!   person/unknown memories that turn wrote. Voiceprints are **never** trained from a
//!   correction (decision D6).
//!
//! The mutations update the SQLite store of record. The live HelixDB graph is not
//! reachable from here (the GraphRAG controller only exposes the recall / graph-view
//! proxies, neither of which carries `reattribute_turn` / `retag_memory`), so every
//! reply carries `graph_updated` (true once the live GraphRAG store took the change); the ingester also honours turn overrides for turns
//! it has not yet ingested.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::{json, Value};

use super::fail;
use crate::app::AppServices;
use crate::memory::{
    ChatLogRecord, Memory, MemoryScope, MemoryStore, PromptLogRecord, ScopeKind, WrittenMemory,
};
use crate::settings::Household;
use crate::speaker::{AliasMap, SpeakerProfile, HOUSEHOLD_SPEAKER, UNKNOWN_SPEAKER};

/// Resolution label the console uses for a turn whose speaker was corrected by hand.
pub(super) const CORRECTED: &str = "corrected";

/// One resolved speaker, ready to render as a chip (`ui.speakerChip`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct SpeakerTag {
    /// The surviving `spk-…` after alias resolution; `None` for household / unknown.
    pub id: Option<String>,
    /// `"person" | "household" | "unknown"`.
    pub kind: &'static str,
    /// Live registry name, reconciled to the household roster's spelling; `None` while
    /// the voice is unnamed (and for household / unknown).
    pub name: Option<String>,
    /// What to print: "Brendon", "Speaker 3 (unnamed)", "Household", "Unknown speaker".
    pub label: String,
    /// The roster relationship ("son", "parent", …), when the person is on the roster.
    pub relationship: Option<String>,
    /// `identified | tentative | new | unknown | corrected` (turns, memory provenance).
    pub resolution: Option<String>,
    /// The voice-match cosine behind `resolution`.
    pub confidence: Option<f32>,
    /// Unknown rows: the best-guess person ("Unknown · maybe Brendon?").
    pub candidate: Option<Box<SpeakerTag>>,
    /// Whether the voice has stored clips (the display's People screen can play them).
    pub has_clips: bool,
    /// Where the name came from: `enrolled | admin | spoken`.
    pub name_source: Option<&'static str>,
    /// A stable CSS colour per person (hashed from the id), shared by every page.
    pub color: Option<String>,
    /// Turns: identity carried over from the follow-up chain (too short on its own).
    pub inherited: bool,
    /// Turns: identified by the read-only short-clip path.
    pub short_clip: bool,
}

impl SpeakerTag {
    fn bare(kind: &'static str, label: &str) -> Self {
        Self {
            id: None,
            kind,
            name: None,
            label: label.to_string(),
            relationship: None,
            resolution: None,
            confidence: None,
            candidate: None,
            has_clips: false,
            name_source: None,
            color: None,
            inherited: false,
            short_clip: false,
        }
    }

    /// The "Unknown speaker" tag.
    pub fn unknown() -> Self {
        Self::bare("unknown", "Unknown speaker")
    }

    /// The "Household" tag (a fact deliberately true for everyone).
    pub fn household() -> Self {
        Self::bare("household", "Household")
    }

    /// Attach the resolution + confidence that produced this tag.
    fn with_resolution(mut self, resolution: Option<&str>, confidence: Option<f32>) -> Self {
        self.resolution = resolution.map(str::to_string);
        self.confidence = confidence;
        self
    }

    fn as_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// One voiceprint as the directory knows it.
#[derive(Debug, Clone)]
struct Person {
    id: String,
    /// Canonical (roster-reconciled) name.
    name: Option<String>,
    relationship: Option<String>,
    /// 1-based position in registry creation order ("Speaker N").
    ordinal: usize,
    name_source: Option<&'static str>,
    has_clips: bool,
    samples: u64,
    created_at: i64,
}

/// The per-request speaker directory (plan §8.1).
pub(super) struct SpeakerDirectory {
    people: Vec<Person>,
    by_id: HashMap<String, usize>,
    aliases: AliasMap,
    /// `turn_id → speaker_id` corrections from the conversations page.
    overrides: HashMap<String, String>,
    /// Whether a voiceprint registry backs this directory. Without one (speaker
    /// identification off) a logged `spk-…` id cannot be resolved, so it is shown by
    /// its logged name instead of being called unknown.
    registry: bool,
    /// The roster, for reconciling logged names when there is no registry.
    household: Household,
}

/// A stable hue per person, so a person's chip has the same colour on every page.
fn color_for(id: &str) -> String {
    // FNV-1a: tiny, deterministic, no dependency.
    let mut h: u32 = 0x811c_9dc5;
    for b in id.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("hsl({} 62% 46%)", h % 360)
}

/// Legacy chat-log / prompt-log speaker ids that mean an **unattributed** turn: the
/// pre-scope `"household"` sentinel and an empty id (plan §3.2).
fn is_unattributed(raw: &str) -> bool {
    let raw = raw.trim();
    raw.is_empty() || raw == HOUSEHOLD_SPEAKER || raw == UNKNOWN_SPEAKER
}

impl SpeakerDirectory {
    /// Build the directory from its parts (the request path uses [`Self::build`]).
    /// `has_clips` reports whether a voice has stored clips.
    pub fn from_parts(
        profiles: &[SpeakerProfile],
        aliases: AliasMap,
        household: &Household,
        overrides: HashMap<String, String>,
        registry: bool,
        has_clips: impl Fn(&str) -> bool,
    ) -> Self {
        let people: Vec<Person> = profiles
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let raw = p.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
                let member = raw.and_then(|n| household.member_matching_spoken(n));
                Person {
                    id: p.id.clone(),
                    name: member
                        .map(|m| m.name.trim().to_string())
                        .or_else(|| raw.map(str::to_string)),
                    relationship: member
                        .and_then(|m| m.relationship.clone())
                        .filter(|r| !r.trim().is_empty()),
                    ordinal: i + 1,
                    name_source: p.name_source.map(|s| s.as_str()),
                    has_clips: has_clips(&p.id),
                    samples: p.samples,
                    created_at: p.created_at,
                }
            })
            .collect();
        let by_id = people
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id.clone(), i))
            .collect();
        Self {
            people,
            by_id,
            aliases,
            overrides,
            registry,
            household: household.clone(),
        }
    }

    /// Build the directory for one request from the live services. Every source is
    /// best-effort: a read failure just leaves that part empty.
    pub fn build(s: &AppServices) -> Self {
        let household = s.settings().household();
        let overrides = s.memory().turn_overrides().unwrap_or_default();
        match s.speaker() {
            Some(svc) => {
                let reg = svc.registry();
                let profiles = reg.list().unwrap_or_default();
                let aliases = reg.aliases().unwrap_or_default();
                Self::from_parts(&profiles, aliases, &household, overrides, true, |id| {
                    svc.list_clips(id).map(|c| !c.is_empty()).unwrap_or(false)
                })
            }
            None => Self::from_parts(
                &[],
                AliasMap::default(),
                &household,
                overrides,
                false,
                |_| false,
            ),
        }
    }

    fn person_tag(&self, p: &Person) -> SpeakerTag {
        let label = match &p.name {
            Some(n) => n.clone(),
            None => format!("Speaker {} (unnamed)", p.ordinal),
        };
        SpeakerTag {
            id: Some(p.id.clone()),
            kind: "person",
            name: p.name.clone(),
            label,
            relationship: p.relationship.clone(),
            has_clips: p.has_clips,
            name_source: p.name_source,
            color: Some(color_for(&p.id)),
            ..SpeakerTag::bare("person", "")
        }
    }

    /// Tag a raw speaker id: alias-resolved first; legacy `"household"` / `""` and a
    /// forgotten id read as Unknown.
    pub fn tag(&self, raw: &str) -> SpeakerTag {
        self.tag_hinted(raw, None)
    }

    /// [`tag`](Self::tag), with the name that was logged alongside the id — used only
    /// when there is no registry to resolve the id against.
    pub fn tag_hinted(&self, raw: &str, logged_name: Option<&str>) -> SpeakerTag {
        if is_unattributed(raw) {
            return SpeakerTag::unknown();
        }
        let id = self.aliases.resolve(raw.trim());
        if is_unattributed(id) {
            return SpeakerTag::unknown();
        }
        if let Some(&i) = self.by_id.get(id) {
            return self.person_tag(&self.people[i]);
        }
        if self.registry {
            // A voiceprint that no longer exists (forgotten before aliases were kept).
            return SpeakerTag::unknown();
        }
        let name = logged_name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(|n| {
                self.household
                    .member_matching_spoken(n)
                    .map(|m| m.name.trim().to_string())
                    .unwrap_or_else(|| n.to_string())
            });
        SpeakerTag {
            id: Some(id.to_string()),
            kind: "person",
            label: name.clone().unwrap_or_else(|| "Unnamed voice".to_string()),
            name,
            color: Some(color_for(id)),
            ..SpeakerTag::bare("person", "")
        }
    }

    /// An Unknown tag carrying `candidate` as its best guess (when that is a person).
    fn unknown_with_candidate(&self, candidate: Option<&str>) -> SpeakerTag {
        let mut t = SpeakerTag::unknown();
        if let Some(c) = candidate {
            let ct = self.tag(c);
            if ct.kind == "person" {
                t.label = format!("Unknown · maybe {}?", ct.label);
                t.candidate = Some(Box::new(ct));
            }
        }
        t
    }

    /// Who a memory is **for** (its scope).
    pub fn memory_for(&self, m: &Memory) -> SpeakerTag {
        match m.scope {
            ScopeKind::Person => match m.speaker_id.as_deref() {
                Some(id) => self.tag(id),
                None => SpeakerTag::unknown(),
            },
            ScopeKind::Household => SpeakerTag::household(),
            ScopeKind::Unknown => self.unknown_with_candidate(m.candidate_speaker_id.as_deref()),
        }
    }

    /// Who **said** a memory (provenance) with the resolution at write time; `None`
    /// when not recorded (legacy rows).
    pub fn memory_said_by(&self, m: &Memory) -> Option<SpeakerTag> {
        if m.said_by_speaker_id.is_none() && m.speaker_resolution.is_none() {
            return None;
        }
        let t = match m.said_by_speaker_id.as_deref() {
            Some(id) => self.tag(id),
            None => self.unknown_with_candidate(m.candidate_speaker_id.as_deref()),
        };
        Some(t.with_resolution(m.speaker_resolution.as_deref(), m.speaker_confidence))
    }

    /// The correction recorded for `turn_id`, if any.
    pub fn override_for(&self, turn_id: Option<&str>) -> Option<&str> {
        turn_id
            .and_then(|t| self.overrides.get(t))
            .map(String::as_str)
    }

    /// The speaker of a chat-log turn, honouring a conversations-page correction.
    pub fn turn_tag(&self, r: &ChatLogRecord) -> SpeakerTag {
        if let Some(to) = self.override_for(r.turn_id.as_deref()) {
            return self.tag(to).with_resolution(Some(CORRECTED), None);
        }
        let mut t = self
            .tag_hinted(&r.speaker_id, r.speaker_name.as_deref())
            .with_resolution(r.speaker_resolution.as_deref(), r.speaker_confidence);
        t.inherited = r.speaker_inherited;
        t.short_clip = r.speaker_short_clip;
        t
    }

    /// Who a memory written by a turn is for, from its logged scope — re-scoped by a
    /// correction like `MemoryStore::reattribute_turn` does (household stays put).
    fn written_for(&self, w: &WrittenMemory, corrected: Option<&str>) -> SpeakerTag {
        let scope = w.scope.as_str();
        if let Some(to) = corrected.filter(|_| scope == "person" || scope == "unknown") {
            return if is_unattributed(to) {
                SpeakerTag::unknown()
            } else {
                self.tag(to)
            };
        }
        match scope {
            "person" => w
                .speaker_id
                .as_deref()
                .map(|id| self.tag(id))
                .unwrap_or_else(SpeakerTag::unknown),
            "household" => SpeakerTag::household(),
            _ => self.unknown_with_candidate(w.speaker_id.as_deref()),
        }
    }

    /// A chat-log record as JSON, with the resolved `speaker` tag, the raw
    /// `speaker_override` (if corrected) and `memories_tagged` — each written memory
    /// with its **For** tag (`scope`/`for` are `null` for records that predate scoped
    /// memories and only carry `memories_written`). Every raw field is kept.
    pub fn chatlog_record_json(&self, r: &ChatLogRecord) -> Value {
        let mut v = serde_json::to_value(r).unwrap_or_else(|_| json!({}));
        let corrected = self.override_for(r.turn_id.as_deref());
        let tagged: Vec<Value> = if r.memories.is_empty() {
            r.memories_written
                .iter()
                .map(|c| json!({ "content": c, "scope": null, "speaker_id": null, "for": null }))
                .collect()
        } else {
            r.memories
                .iter()
                .map(|w| {
                    json!({
                        "content": w.content,
                        "scope": w.scope,
                        "speaker_id": w.speaker_id,
                        "for": self.written_for(w, corrected).as_json(),
                    })
                })
                .collect()
        };
        if let Some(obj) = v.as_object_mut() {
            obj.insert("speaker".into(), self.turn_tag(r).as_json());
            obj.insert("speaker_override".into(), json!(corrected));
            obj.insert("memories_tagged".into(), Value::Array(tagged));
        }
        v
    }

    /// A prompt-log record as JSON with its resolved `speaker` tag.
    pub fn prompt_record_json(&self, r: &PromptLogRecord) -> Value {
        let mut v = serde_json::to_value(r).unwrap_or_else(|_| json!({}));
        let corrected = self.override_for(r.turn_id.as_deref());
        let tag = match corrected {
            Some(to) => self.tag(to).with_resolution(Some(CORRECTED), None),
            None => self
                .tag_hinted(&r.speaker_id, r.speaker_name.as_deref())
                .with_resolution(r.speaker_resolution.as_deref(), None),
        };
        if let Some(obj) = v.as_object_mut() {
            obj.insert("speaker".into(), tag.as_json());
        }
        v
    }

    /// The resolved fields `/sqlite.json` adds to one memory row.
    pub fn memory_fields(&self, m: &Memory) -> Value {
        let scope_tag = self.memory_for(m);
        json!({
            "scope": m.scope.as_str(),
            "speaker": scope_tag.as_json(),
            "candidate": scope_tag.candidate.as_ref().map(|c| c.as_json()),
            "said_by": self.memory_said_by(m).map(|t| t.as_json()),
            "candidate_speaker_id": m.candidate_speaker_id,
            "said_by_speaker_id": m.said_by_speaker_id,
            "speaker_resolution": m.speaker_resolution,
            "speaker_confidence": m.speaker_confidence,
            "turn_id": m.turn_id,
        })
    }

    /// `GET /speakers.json` — every voiceprint as a tag (registry order) plus the
    /// household / unknown tags and the roster members with no voiceprint yet.
    pub fn to_json(&self, memory: Option<&MemoryStore>) -> Value {
        let speakers: Vec<Value> = self
            .people
            .iter()
            .map(|p| {
                let mut v = self.person_tag(p).as_json();
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("ordinal".into(), json!(p.ordinal));
                    obj.insert("samples".into(), json!(p.samples));
                    obj.insert("created_at".into(), json!(p.created_at));
                    let pending = memory.and_then(|m| m.unknown_count_for_candidate(&p.id).ok());
                    obj.insert(
                        "unknown_candidate_count".into(),
                        json!(pending.unwrap_or(0)),
                    );
                }
                v
            })
            .collect();
        let voiced: Vec<&str> = self
            .people
            .iter()
            .filter_map(|p| p.name.as_deref())
            .collect();
        let roster: Vec<Value> = self
            .household
            .members
            .iter()
            .filter(|m| !m.name.trim().is_empty())
            .map(|m| {
                json!({
                    "name": m.name.trim(),
                    "relationship": m.relationship,
                    "has_voice": voiced.iter().any(|n| n.eq_ignore_ascii_case(m.name.trim())),
                })
            })
            .collect();
        json!({
            "ok": true,
            "enabled": self.registry,
            "speakers": speakers,
            "household": SpeakerTag::household().as_json(),
            "unknown": SpeakerTag::unknown().as_json(),
            "roster": roster,
        })
    }

    /// Resolve an attribution target posted by the page: `"household"`, `"unknown"`,
    /// or a `spk-…` id (alias-resolved; it must name an existing voiceprint).
    fn target(&self, to: &str) -> Result<Target, String> {
        match to.trim() {
            "" => Err("`to` is required".into()),
            HOUSEHOLD_SPEAKER => Ok(Target::Household),
            UNKNOWN_SPEAKER => Ok(Target::Unknown),
            raw => {
                let id = self.aliases.resolve(raw);
                if id == UNKNOWN_SPEAKER {
                    return Err(format!("{raw} was forgotten; pick another person"));
                }
                if self.by_id.contains_key(id) {
                    Ok(Target::Person(id.to_string()))
                } else {
                    Err(format!("no such person: {raw}"))
                }
            }
        }
    }
}

/// Where an admin moves a memory / turn.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    Person(String),
    Household,
    Unknown,
}

/// The graph owner string a memory row maps to (`spk-…` | `household` | `unknown`).
fn owner_of(m: &Memory) -> String {
    match m.scope {
        ScopeKind::Person => m
            .speaker_id
            .clone()
            .unwrap_or_else(|| UNKNOWN_SPEAKER.into()),
        ScopeKind::Household => HOUSEHOLD_SPEAKER.into(),
        ScopeKind::Unknown => UNKNOWN_SPEAKER.into(),
    }
}

/// `id` (a number or numeric string) or `ids` (an array of them) from a request body.
fn memory_ids(data: &Value) -> Result<Vec<i64>, String> {
    let one = |v: &Value| -> Option<i64> {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    };
    if let Some(list) = data.get("ids").and_then(Value::as_array) {
        let ids: Option<Vec<i64>> = list.iter().map(one).collect();
        return match ids {
            Some(ids) if !ids.is_empty() => Ok(ids),
            Some(_) => Err("`ids` is empty".into()),
            None => Err("`ids` must be memory ids".into()),
        };
    }
    data.get("id")
        .and_then(one)
        .map(|id| vec![id])
        .ok_or_else(|| "`id` (or `ids`) is required".into())
}

/// `GET /speakers.json`.
pub(super) fn speakers_json(s: &AppServices) -> String {
    SpeakerDirectory::build(s)
        .to_json(Some(s.memory()))
        .to_string()
}

/// `POST /memory/attribute` — move memories into a scope: `{ id | ids, to }` with `to`
/// = `spk-…` (that person), `"household"` or `"unknown"` (keeps the best guess).
/// Replies `{ ok, moved, ids, to, graph_updated: false, message }`.
fn memory_attribute(s: &AppServices, data: &Value, ops: &mut Vec<GraphOp>) -> String {
    let ids = match memory_ids(data) {
        Ok(ids) => ids,
        Err(e) => return fail(e),
    };
    let to = data.get("to").and_then(Value::as_str).unwrap_or("");
    let dir = SpeakerDirectory::build(s);
    let target = match dir.target(to) {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    let memory = s.memory();
    let mut moved = Vec::new();
    for id in ids {
        let row = match memory.get(id) {
            Ok(Some(m)) => m,
            Ok(None) => continue,
            Err(e) => return fail(format!("{e:#}")),
        };
        let scope = match &target {
            Target::Person(p) => MemoryScope::Person(p.clone()),
            Target::Household => MemoryScope::Household,
            Target::Unknown => MemoryScope::Unknown {
                candidate: row.candidate_speaker_id.clone(),
            },
        };
        let from = owner_of(&row);
        match memory.attribute(id, &scope) {
            Ok(true) => {
                let to_owner = scope
                    .speaker_id()
                    .map(str::to_string)
                    .unwrap_or_else(|| scope.kind().as_str().to_string());
                ops.push(GraphOp::Retag {
                    content: row.content.clone(),
                    from: from.clone(),
                    to: to_owner,
                });
                log::info!(
                    "admin console: memory {id} {from} → {}",
                    scope.speaker_id().unwrap_or(scope.kind().as_str())
                );
                moved.push(id);
            }
            Ok(false) => {}
            Err(e) => return fail(format!("{e:#}")),
        }
    }
    if moved.is_empty() {
        return fail("no such memory");
    }
    let to_label = match &target {
        Target::Person(p) => dir.tag(p).label,
        Target::Household => "Household".into(),
        Target::Unknown => "Unknown speaker".into(),
    };
    json!({
        "ok": true,
        "moved": moved.len(),
        "ids": moved,
        "to": to.trim(),
        "graph_updated": false,
        "message": format!(
            "{} memor{} moved to {to_label}.",
            moved.len(),
            if moved.len() == 1 { "y" } else { "ies" }
        ),
    })
    .to_string()
}

/// `POST /memory/delete` — `{ id | ids }`. Replies `{ ok, deleted, graph_updated: false }`.
fn memory_delete(s: &AppServices, data: &Value, ops: &mut Vec<GraphOp>) -> String {
    let ids = match memory_ids(data) {
        Ok(ids) => ids,
        Err(e) => return fail(e),
    };
    let mut deleted = 0usize;
    for id in ids {
        if let Ok(Some(row)) = s.memory().get(id) {
            ops.push(GraphOp::Forget {
                content: row.content.clone(),
                owner: owner_of(&row),
            });
        }
        match s.memory().delete(id) {
            Ok(true) => deleted += 1,
            Ok(false) => {}
            Err(e) => return fail(format!("{e:#}")),
        }
    }
    if deleted == 0 {
        return fail("no such memory");
    }
    json!({
        "ok": true,
        "deleted": deleted,
        "graph_updated": false,
        "message": format!("Deleted {deleted} memor{}.", if deleted == 1 { "y" } else { "ies" }),
    })
    .to_string()
}

/// `POST /conversations/attribute` — `{ turn_id, to }` with `to` = `spk-…` or
/// `"unknown"`: record the turn-attribution override and re-scope the person/unknown
/// memories that turn wrote (household facts stay household). Never touches a
/// voiceprint (D6). Replies `{ ok, turn_id, to, memories, graph_updated: false }`.
fn conversations_attribute(s: &AppServices, data: &Value, ops: &mut Vec<GraphOp>) -> String {
    let Some(turn_id) = super::str_field(data, "turn_id") else {
        return fail("`turn_id` is required");
    };
    let to = data.get("to").and_then(Value::as_str).unwrap_or("");
    let dir = SpeakerDirectory::build(s);
    let (speaker_id, scope) = match dir.target(to) {
        Ok(Target::Person(p)) => (p.clone(), MemoryScope::Person(p)),
        Ok(Target::Unknown) => (
            UNKNOWN_SPEAKER.to_string(),
            MemoryScope::Unknown { candidate: None },
        ),
        Ok(Target::Household) => {
            return fail("a turn is said by a person or an unknown voice, not the household")
        }
        Err(e) => return fail(e),
    };
    let memory = s.memory();
    if let Err(e) = memory.set_turn_override(&turn_id, &speaker_id) {
        return fail(format!("{e:#}"));
    }
    let n = match memory.reattribute_turn(&turn_id, &scope) {
        Ok(n) => n,
        Err(e) => return fail(format!("{e:#}")),
    };
    log::info!("admin console: turn {turn_id} re-attributed to {speaker_id} ({n} memories)");
    // The graph keys a turn by its chat-log record id, not the turn id.
    let record_id = crate::memory::chatlog::read_tail(&s.debug.chatlog_path, 5000)
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.turn_id.as_deref() == Some(turn_id.as_str()))
        .map(|r| r.id);
    if let Some(ext_id) = record_id {
        ops.push(GraphOp::Turn {
            ext_id,
            to: speaker_id.clone(),
        });
    }
    let label = dir.tag(&speaker_id).label;
    json!({
        "ok": true,
        "turn_id": turn_id,
        "to": speaker_id,
        "speaker": dir.tag(&speaker_id).with_resolution(Some(CORRECTED), None).as_json(),
        "memories": n,
        "graph_updated": false,
        "message": format!(
            "Turn re-attributed to {label}; {n} memor{} moved.",
            if n == 1 { "y" } else { "ies" }
        ),
    })
    .to_string()
}

/// Add a resolved `$speaker` tag to every sampled graph node that carries a
/// `speaker_id` (User / Turn / Memory), so the graph view shows names, not ids.
pub(super) fn tag_graph_nodes(dir: &SpeakerDirectory, nodes: &mut Value) {
    let Some(labels) = nodes.as_object_mut() else {
        return;
    };
    for rows in labels.values_mut() {
        let Some(rows) = rows.as_array_mut() else {
            continue;
        };
        for row in rows {
            let Some(obj) = row.as_object_mut() else {
                continue;
            };
            let Some(sid) = obj.get("speaker_id").and_then(Value::as_str) else {
                continue;
            };
            let tag = if sid == HOUSEHOLD_SPEAKER {
                // In the graph `household` is the shared-facts owner (legacy household
                // turns were migrated to `unknown`), unlike the chat log's legacy id.
                SpeakerTag::household()
            } else {
                dir.tag_hinted(sid, obj.get("name").and_then(Value::as_str))
            };
            obj.insert("$speaker".into(), tag.as_json());
        }
    }
}

/// A graph-store change an admin action implies, applied after the SQLite change by
/// [`apply_graph_ops`] so recall from the live GraphRAG store matches.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum GraphOp {
    Retag {
        content: String,
        from: String,
        to: String,
    },
    Forget {
        content: String,
        owner: String,
    },
    Turn {
        ext_id: String,
        to: String,
    },
}

/// Apply `ops` to the live recall backend (a no-op on SQLite-only recall). Returns
/// whether a graph store took them (`false` without GraphRAG or on an error, which is
/// logged — the SQLite change already stands).
async fn apply_graph_ops(s: &AppServices, ops: &[GraphOp]) -> bool {
    let Some(graphrag) = s.graphrag.as_ref() else {
        return false;
    };
    let recall = graphrag.recall();
    if recall.backend() != "helix" {
        return false;
    }
    for op in ops {
        let res = match op {
            GraphOp::Retag { content, from, to } => recall.retag_memory(content, from, to).await,
            GraphOp::Forget { content, owner } => recall.forget_memory(content, owner).await,
            GraphOp::Turn { ext_id, to } => recall.reattribute_turn(ext_id, to).await,
        };
        if let Err(e) = res {
            log::warn!("admin console: graph update {op:?} failed: {e:#}");
            return false;
        }
    }
    true
}

/// Run a SQLite-side admin action, then apply the graph ops it implies and report
/// `graph_updated` truthfully.
async fn with_graph(
    s: &AppServices,
    data: &Value,
    action: fn(&AppServices, &Value, &mut Vec<GraphOp>) -> String,
) -> String {
    let mut ops = Vec::new();
    let reply = action(s, data, &mut ops);
    let mut v: Value = match serde_json::from_str(&reply) {
        Ok(v) => v,
        Err(_) => return reply,
    };
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        v["graph_updated"] = json!(apply_graph_ops(s, &ops).await);
    }
    v.to_string()
}

/// `POST /memory/attribute`, applied to SQLite and the live graph.
pub(super) async fn memory_attribute_async(s: &AppServices, data: &Value) -> String {
    with_graph(s, data, memory_attribute).await
}

/// `POST /memory/delete`, applied to SQLite and the live graph.
pub(super) async fn memory_delete_async(s: &AppServices, data: &Value) -> String {
    with_graph(s, data, memory_delete).await
}

/// `POST /conversations/attribute`, applied to SQLite and the live graph.
pub(super) async fn conversations_attribute_async(s: &AppServices, data: &Value) -> String {
    with_graph(s, data, conversations_attribute).await
}

/// The SQLite side of `POST /memory/attribute`, without the graph (tests).
#[cfg(test)]
pub(super) fn memory_attribute_json(s: &AppServices, data: &Value) -> String {
    memory_attribute(s, data, &mut Vec::new())
}

/// The SQLite side of `POST /memory/delete`, without the graph (tests).
#[cfg(test)]
pub(super) fn memory_delete_json(s: &AppServices, data: &Value) -> String {
    memory_delete(s, data, &mut Vec::new())
}

/// The SQLite side of `POST /conversations/attribute`, without the graph (tests).
#[cfg(test)]
pub(super) fn conversations_attribute_json(s: &AppServices, data: &Value) -> String {
    conversations_attribute(s, data, &mut Vec::new())
}
