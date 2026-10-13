//! The Household tab (home address, units, roster, plus the Cadora shopping list and
//! AppSaid phone-push links), the Personality tab and the Pronunciation tab. The
//! household, personality and pronunciation saves replace the whole record.

use serde_json::{json, Value};

use super::{fail, keep_or_set, parse_json_body, set_or_clear, str_field};
use crate::settings::{
    AppSaidUpdate, CadoraUpdate, Household, HouseholdMember, Personality, PersonalityDef,
    SharedSettings,
};

/// `GET /household/status.json` — the canonical household record (home address +
/// units + roster). Contact details are shown so they can be edited on the page;
/// this surface is loopback + unauthenticated by design (same as the rest).
pub(super) fn household_status_json(settings: &SharedSettings) -> String {
    let h = settings.household();
    let members: Vec<Value> = h
        .members
        .iter()
        .map(|m| {
            json!({
                "name": m.name,
                "emails": m.emails,
                "phones": m.phones,
                "appsaid": m.appsaid,
                "relationship": m.relationship,
            })
        })
        .collect();
    json!({
        "ok": true,
        "address": {
            "address1": h.address.address1,
            "address2": h.address.address2,
            "city": h.address.city,
            "state": h.address.state,
            "postal": h.address.postal,
            "country": h.address.country,
        },
        "weather_units": h.weather_units,
        "members": members,
    })
    .to_string()
}

/// `POST /household/save` — replace the whole household record (discrete address fields,
/// units, roster). The value is sanitized on apply (trim, drop blanks / nameless members).
pub(super) fn household_save_json(settings: &SharedSettings, data: &Value) -> String {
    let str_list = |v: Option<&Value>| {
        v.and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let members = data
        .get("members")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|m| HouseholdMember {
                    name: str_field(m, "name").unwrap_or_default(),
                    emails: str_list(m.get("emails")),
                    phones: str_list(m.get("phones")),
                    appsaid: str_field(m, "appsaid"),
                    relationship: str_field(m, "relationship"),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let addr = data.get("address");
    let addr_field = |k: &str| addr.and_then(|a| str_field(a, k));
    let household = Household {
        address: crate::household::HomeAddress {
            address1: addr_field("address1"),
            address2: addr_field("address2"),
            city: addr_field("city"),
            state: addr_field("state"),
            postal: addr_field("postal"),
            country: addr_field("country"),
        },
        location: None,
        weather_units: str_field(data, "weather_units"),
        members,
    };
    settings.apply_household(&household);
    household_status_json(settings)
}

/// `GET /personality/status.json` — the live personality state + editable catalog.
pub(super) fn personality_status_json(settings: &SharedSettings) -> String {
    let p = settings.personality();
    let definitions: Vec<Value> = p
        .definitions
        .iter()
        .map(|d| {
            json!({
                "key": d.key,
                "label": d.label,
                "description": d.description,
            })
        })
        .collect();
    json!({
        "ok": true,
        "enabled": p.enabled,
        "active": p.active,
        "definitions": definitions,
    })
    .to_string()
}

/// `POST /personality/save` — replace the whole personality record (on/off, active
/// selection, catalog). Sanitized on apply (trim, drop blank entries, de-dupe keys).
pub(super) fn personality_save_json(settings: &SharedSettings, data: &Value) -> String {
    // Trimmed, absent = "" (the catalog keeps blank fields; apply drops blank entries).
    let text = |v: &Value, key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let definitions = data
        .get("definitions")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|d| PersonalityDef {
                    key: text(d, "key"),
                    label: text(d, "label"),
                    description: text(d, "description"),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let personality = Personality {
        enabled: data
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        active: text(data, "active"),
        definitions,
    };
    settings.apply_personality(&personality);
    personality_status_json(settings)
}

/// `GET /pronunciations/status.json` — the live STT pronunciation rewrite table.
pub(super) fn pronunciations_status_json(settings: &SharedSettings) -> String {
    let rules: Vec<Value> = settings
        .pronunciations()
        .iter()
        .map(|r| json!({ "canonical": r.canonical, "variants": r.variants }))
        .collect();
    json!({ "ok": true, "rules": rules }).to_string()
}

/// `POST /pronunciations/save` — replace the whole table from a JSON body
/// `{ "rules": [ { "canonical": "Zehrs", "variants": ["zairs", "zay rz"] }, … ] }`.
/// Sanitized on apply (trim, drop blank/duplicate/self variants, drop unusable rules).
pub(super) fn pronunciations_save_json(settings: &SharedSettings, data: &Value) -> String {
    let rules = data
        .get("rules")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|r| crate::stt::PronunciationRule {
                    canonical: r
                        .get("canonical")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    variants: r
                        .get("variants")
                        .and_then(Value::as_array)
                        .map(|vs| {
                            vs.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    settings.apply_pronunciations(&rules);
    pronunciations_status_json(settings)
}

/// `GET /cadora/status.json` — the Cadora shopping-list linkage state for the
/// `/household` page. Reports only booleans + the base URL; the voice-link token is
/// never included.
pub(super) fn cadora_status_json(settings: &SharedSettings) -> String {
    let c = settings.cadora();
    json!({
        "ok": true,
        "linked": c.linked(),
        "base_url": c.base_url_or_default(),
    })
    .to_string()
}

/// `POST /cadora/save` — set the Cadora app-server base URL (a blank value resets it
/// to the default). Does not link. Applying rebuilds the LLM so the tool tracks state.
pub(super) fn cadora_save_json(settings: &SharedSettings, data: &Value) -> String {
    settings.apply_cadora(&CadoraUpdate {
        base_url: set_or_clear(data, "base_url"),
        ..Default::default()
    });
    cadora_status_json(settings)
}

/// `POST /cadora/link` — link the shopping list. Two inputs are accepted: a **6-digit
/// pairing `code`** (the primary path — NextHaul → Settings → Voice & Integrations
/// mints one via `/voice/links/pair`; we redeem it here via `/voice/links/redeem` for
/// a durable token, exactly as the "speaker" is meant to), or a directly-pasted
/// **`vl_…` token** as a fallback. On success the `shopping_list_add` tool activates
/// immediately.
pub(super) async fn cadora_link_json(settings: &SharedSettings, body: &[u8]) -> String {
    let data = match parse_json_body(body) {
        Ok(v) => v,
        Err(invalid) => return invalid,
    };
    let token = str_field(&data, "token").unwrap_or_default();
    let code = str_field(&data, "code").unwrap_or_default();

    // An optional base_url in the same request lets the user point at a non-default
    // server before linking; persist it first so a redeem is minted against it.
    if let Some(base) = str_field(&data, "base_url") {
        settings.apply_cadora(&CadoraUpdate {
            base_url: Some(Some(base)),
            ..Default::default()
        });
    }

    // Resolve the durable token: redeem a 6-digit code (the NextHaul flow), else
    // accept a directly-pasted `vl_…` token (fallback / older app builds).
    let resolved = if !code.is_empty() {
        let base_url = settings.cadora().base_url_or_default();
        match crate::cadora::redeem_pairing_code(&base_url, &code).await {
            Ok(t) => t,
            Err(e) => return fail(format!("{e:#}")),
        }
    } else if !token.is_empty() {
        token
    } else {
        return fail(
            "Enter the 6-digit code from NextHaul (Settings → Voice & Integrations), or paste a vl_… token.",
        );
    };

    settings.apply_cadora(&CadoraUpdate {
        link_token: Some(Some(resolved)),
        ..Default::default()
    });
    let now = settings.cadora();
    json!({
        "ok": true,
        "message": "Shopping list linked — try \"add milk to the shopping list\".",
        "linked": now.linked(),
        "base_url": now.base_url_or_default(),
    })
    .to_string()
}

/// `GET /appsaid/status.json` — the AppSaid phone-push state for the `/household`
/// page. Reports the worker URL, the configured recipient names, and whether the app
/// token is set + the `send_phone_message` tool is therefore active. The token value
/// is never included.
pub(super) fn appsaid_status_json(settings: &SharedSettings) -> String {
    let a = settings.appsaid();
    json!({
        "ok": true,
        "worker_url": a.worker_url.clone().unwrap_or_default(),
        "recipients": a.recipient_names(),
        "default_recipient": a.default_recipient.clone().unwrap_or_default(),
        "token_set": a.token_set(),
        "tool_active": a.active(),
    })
    .to_string()
}

/// `POST /appsaid/save` — set the AppSaid worker URL and/or default recipient. A blank
/// value clears the field; an absent field is left unchanged. Applying rebuilds the LLM
/// so the tool tracks state. Recipients themselves come from the household roster.
pub(super) fn appsaid_save_json(settings: &SharedSettings, data: &Value) -> String {
    let worker_url = set_or_clear(data, "worker_url")
        .map(|url| url.map(|u| u.trim_end_matches('/').to_string()));
    settings.apply_appsaid(&AppSaidUpdate {
        worker_url,
        default_recipient: set_or_clear(data, "default_recipient"),
        ..Default::default()
    });
    appsaid_status_json(settings)
}

/// `POST /appsaid/token` — set the secret AppSaid sender app token. A blank/absent
/// token is left unchanged (a page reload never wipes the stored token); a non-empty
/// value sets it and rebuilds the backend so `send_phone_message` activates live.
pub(super) fn appsaid_token_json(settings: &SharedSettings, data: &Value) -> String {
    settings.apply_appsaid(&AppSaidUpdate {
        app_token: keep_or_set(data, "app_token"),
        ..Default::default()
    });
    appsaid_status_json(settings)
}
