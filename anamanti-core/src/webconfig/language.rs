//! The Voice page's **Languages** card (`plans/BilingualPlan.md` §2.11): which languages
//! the assistant listens and answers in, the household default, and a Piper voice per
//! language. Applied live from the next turn and persisted to the settings overlay.

use serde_json::{json, Value};

use crate::lang::{Lang, LanguageSettings};
use crate::settings::SharedSettings;

/// `GET /language/status.json`.
pub(super) fn language_status_json(settings: &SharedSettings) -> String {
    let lang = settings.language();
    let snap = settings.snapshot();
    let voices: serde_json::Map<String, Value> = lang
        .voices
        .iter()
        .map(|(l, v)| (l.code().to_string(), json!(v)))
        .collect();
    let supported: Vec<Value> = Lang::ALL
        .iter()
        .map(|l| {
            json!({
                "code": l.code(),
                "name": l.english_name(),
                "native": l.native_name(),
                "piper_locale": l.piper_locale(),
            })
        })
        .collect();
    json!({
        "ok": true,
        "enabled": lang.enabled.iter().map(|l| l.code()).collect::<Vec<_>>(),
        "default": lang.default.code(),
        "voices": voices,
        "bilingual": lang.is_bilingual(),
        "supported": supported,
        "warnings": crate::lang::setup_warnings(&lang, snap.tts_voice.as_deref(), snap.stt.engine),
    })
    .to_string()
}

/// `POST /language/save` — body `{ "enabled": ["en","pt-BR"], "default": "en",
/// "voices": { "pt-BR": "pt_BR-faber-medium" } }`. An unknown code is rejected.
pub(super) fn language_save_json(settings: &SharedSettings, data: &Value) -> String {
    match parse(data) {
        Ok(lang) => {
            settings.apply_language(&lang);
            language_status_json(settings)
        }
        Err(message) => json!({ "ok": false, "message": message }).to_string(),
    }
}

fn parse(data: &Value) -> Result<LanguageSettings, String> {
    let code = |v: &Value| -> Result<Lang, String> {
        let s = v.as_str().unwrap_or_default();
        Lang::parse(s).ok_or_else(|| format!("unknown language `{s}` (supported: en, pt-BR)"))
    };
    let enabled = data
        .get("enabled")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(code).collect::<Result<Vec<_>, _>>())
        .transpose()?
        .unwrap_or_default();
    let default = match data.get("default") {
        Some(v) if v.as_str().is_some_and(|s| !s.trim().is_empty()) => code(v)?,
        _ => enabled.first().copied().unwrap_or(Lang::En),
    };
    let mut voices = std::collections::BTreeMap::new();
    if let Some(map) = data.get("voices").and_then(Value::as_object) {
        for (k, v) in map {
            let lang = Lang::parse(k).ok_or_else(|| format!("unknown language `{k}`"))?;
            if let Some(name) = v.as_str().filter(|s| !s.trim().is_empty()) {
                voices.insert(lang, name.trim().to_string());
            }
        }
    }
    Ok(LanguageSettings {
        enabled,
        default,
        voices,
    }
    .sanitized())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_codes_and_rejects_unknown_ones() {
        let l = parse(&json!({
            "enabled": ["en", "pt-BR"], "default": "en",
            "voices": { "pt-BR": "pt_BR-faber-medium", "en": "" }
        }))
        .unwrap();
        assert!(l.is_bilingual());
        assert_eq!(l.voices.len(), 1);
        assert!(parse(&json!({ "enabled": ["xx"] })).is_err());
        // An empty body is English-only (the default).
        assert_eq!(parse(&json!({})).unwrap(), LanguageSettings::default());
    }

    use crate::config::SttEngineKind;
    use crate::lang::setup_warnings as warnings;

    #[test]
    fn warns_about_missing_voices_and_engine_setup() {
        let l = parse(&json!({ "enabled": ["en", "pt-BR"] })).unwrap();
        let w = warnings(&l, Some("en_US-amy-medium"), SttEngineKind::ElevenLabs);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("Brazilian Portuguese"));
        let l = parse(&json!({
            "enabled": ["en", "pt-BR"], "voices": { "pt-BR": "pt_BR-faber-medium" }
        }))
        .unwrap();
        assert!(warnings(&l, Some("en_US-amy-medium"), SttEngineKind::ElevenLabs).is_empty());
        assert_eq!(
            warnings(&l, Some("en_US-amy-medium"), SttEngineKind::Wyoming).len(),
            1
        );
        assert!(warnings(&LanguageSettings::default(), None, SttEngineKind::Wyoming).is_empty());
    }
}
