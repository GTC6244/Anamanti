//! Bilingual support: English + Brazilian Portuguese (`plans/BilingualPlan.md`).
//!
//! Every turn resolves one [`TurnLang`] right after STT ([`resolve`]); everything
//! downstream reads it — the prompt's language block, the Piper voice, the localized
//! canned replies ([`phrases`]) and the fast-path phrase tables. **Replies follow the
//! language spoken on that turn** (owner decision): signals that measure what was said
//! (the STT engine's detected language, then text language ID) decide; the previous
//! turn in a follow-up chain, the speaker's preferred language and the household default
//! only break a tie when neither can tell.
//!
//! With a single enabled language (the default — and what an install with no `language`
//! block in `anamanti.json` gets) every turn is [`LangSource::Fixed`] and nothing in
//! the pipeline changes: no prompt block, no voice override, English-only fast paths.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub mod detect;
pub mod phrases;

/// A language the assistant can listen and answer in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lang {
    /// English (the historical, and default, language).
    En,
    /// Brazilian Portuguese.
    PtBr,
}

impl Lang {
    /// Every supported language, in display order.
    pub const ALL: [Lang; 2] = [Lang::En, Lang::PtBr];

    /// The canonical BCP-47 code used in config and the UI (`en`, `pt-BR`).
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::PtBr => "pt-BR",
        }
    }

    /// ISO 639-1 (`en`, `pt`) — what STT engines take as a decode language.
    pub fn iso639_1(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::PtBr => "pt",
        }
    }

    /// The English name, for the prompt and the admin console.
    pub fn english_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::PtBr => "Brazilian Portuguese",
        }
    }

    /// The language's own name (shown in the Languages picker).
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::PtBr => "Português (Brasil)",
        }
    }

    /// The Piper voice-name locale prefix (`en_US-amy-medium` → `en_US`).
    pub fn piper_locale(self) -> &'static str {
        match self {
            Lang::En => "en_US",
            Lang::PtBr => "pt_BR",
        }
    }

    /// Parse a language label leniently: BCP-47 / ISO 639-1 / ISO 639-3 / Piper-style
    /// locales and English names, case-insensitive (`en`, `en-US`, `eng`, `english`,
    /// `pt`, `pt-BR`, `pt_BR`, `por`, `portuguese`). `None` for anything else.
    pub fn parse(s: &str) -> Option<Lang> {
        let l = s.trim().to_lowercase().replace('_', "-");
        let base = l.split('-').next().unwrap_or_default();
        match base {
            "en" | "eng" | "english" => Some(Lang::En),
            "pt" | "por" | "portuguese" | "português" | "portugues" => Some(Lang::PtBr),
            _ => None,
        }
    }

    /// Whether a Piper voice name belongs to this language (`pt_BR-faber-medium`).
    pub fn owns_voice(self, voice: &str) -> bool {
        let prefix = voice.split('-').next().unwrap_or_default();
        Lang::parse(prefix) == Some(self)
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl Serialize for Lang {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.code())
    }
}

impl<'de> Deserialize<'de> for Lang {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Lang::parse(&s).ok_or_else(|| {
            serde::de::Error::custom(format!("unknown language `{s}` (expected `en` or `pt-BR`)"))
        })
    }
}

/// The household's language configuration: which languages the assistant listens and
/// answers in, the default, and a Piper voice per language.
///
/// Seeded from the `language` + `tts_voices` blocks of `anamanti.json`, editable on the
/// config page (Voice → Languages), persisted to the settings overlay. The default value
/// is English-only — exactly the pre-bilingual behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageSettings {
    /// Enabled languages, in order. Never empty after [`Self::sanitized`].
    pub enabled: Vec<Lang>,
    /// The household default: the last-resort tie-break and the STT engine's primary
    /// language. Always one of `enabled` after [`Self::sanitized`].
    pub default: Lang,
    /// A Piper voice per language. A language with no entry speaks with the global
    /// `tts_voice` (the Voice page's main picker) — so an English-only install, which
    /// has no entries, is unchanged.
    #[serde(default)]
    pub voices: BTreeMap<Lang, String>,
}

impl Default for LanguageSettings {
    fn default() -> Self {
        Self::single(Lang::En)
    }
}

impl LanguageSettings {
    /// One fixed language (no detection, no prompt block, no voice switching).
    pub fn single(lang: Lang) -> Self {
        Self {
            enabled: vec![lang],
            default: lang,
            voices: BTreeMap::new(),
        }
    }

    /// Whether more than one language is enabled (bilingual mode).
    pub fn is_bilingual(&self) -> bool {
        self.enabled.len() > 1
    }

    /// Whether `lang` is enabled.
    pub fn allows(&self, lang: Lang) -> bool {
        self.enabled.contains(&lang)
    }

    /// Normalize: dedupe `enabled`, make sure it is non-empty and contains `default`
    /// (prepending it if missing), and drop blank voice entries.
    pub fn sanitized(mut self) -> Self {
        let mut seen = Vec::new();
        for l in self.enabled {
            if !seen.contains(&l) {
                seen.push(l);
            }
        }
        if !seen.contains(&self.default) {
            seen.insert(0, self.default);
        }
        self.enabled = seen;
        self.voices.retain(|_, v| !v.trim().is_empty());
        for v in self.voices.values_mut() {
            *v = v.trim().to_string();
        }
        self
    }

    /// The enabled languages other than the default, in order.
    pub fn secondary(&self) -> impl Iterator<Item = Lang> + '_ {
        self.enabled.iter().copied().filter(|l| *l != self.default)
    }

    /// The Piper voice to speak `lang` with: its own entry, else the global voice.
    pub fn voice_for(&self, lang: Lang, global: Option<&str>) -> Option<String> {
        self.voices
            .get(&lang)
            .cloned()
            .or_else(|| global.map(str::to_string))
    }

    /// Map an STT-reported language code onto an enabled language. Lookalikes the
    /// engines confuse with Portuguese on short utterances (Spanish, Galician) count as
    /// Portuguese when Portuguese is enabled. `None` when it isn't an enabled language.
    pub fn from_stt_code(&self, code: &str) -> Option<Lang> {
        let l = code.trim().to_lowercase().replace('_', "-");
        let base = l.split('-').next().unwrap_or_default();
        let lang = match base {
            "es" | "spa" | "gl" | "glg" if self.allows(Lang::PtBr) => Some(Lang::PtBr),
            _ => Lang::parse(base),
        }?;
        self.allows(lang).then_some(lang)
    }
}

/// How a turn's language was decided (logged + shown on the Conversations page).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LangSource {
    /// Single-language mode: no detection ran.
    Fixed,
    /// The STT engine reported the language it heard.
    Stt,
    /// Text language identification on the transcript.
    Text,
    /// A fast-path phrase table matched in this language (it is what was said).
    Phrase,
    /// Tie-break: the previous turn of this follow-up chain.
    Sticky,
    /// Tie-break: the identified speaker's preferred language.
    Speaker,
    /// Tie-break: the household default.
    Default,
}

impl LangSource {
    pub fn as_str(self) -> &'static str {
        match self {
            LangSource::Fixed => "fixed",
            LangSource::Stt => "stt",
            LangSource::Text => "text",
            LangSource::Phrase => "phrase",
            LangSource::Sticky => "sticky",
            LangSource::Speaker => "speaker",
            LangSource::Default => "default",
        }
    }
}

/// The language a turn was spoken in, and how that was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnLang {
    pub lang: Lang,
    pub source: LangSource,
}

impl TurnLang {
    /// A single-language turn.
    pub fn fixed(lang: Lang) -> Self {
        Self {
            lang,
            source: LangSource::Fixed,
        }
    }
}

impl Default for TurnLang {
    fn default() -> Self {
        Self::fixed(Lang::En)
    }
}

/// The evidence [`resolve`] weighs for one turn.
#[derive(Debug, Clone, Copy, Default)]
pub struct LangSignals<'a> {
    /// The language the STT engine reported, if any (raw engine code).
    pub stt: Option<&'a str>,
    /// The final transcript.
    pub transcript: &'a str,
    /// The previous turn's language, when this is a follow-up within the history window.
    pub previous: Option<Lang>,
    /// The identified speaker's preferred language, if they set one.
    pub speaker: Option<Lang>,
}

/// Decide the turn's language. Single-language settings short-circuit to
/// [`LangSource::Fixed`]. Otherwise the first signal that clears its bar wins: an exact
/// short phrase ([`detect::short_phrase`]), the STT engine's report (overridden by a
/// decisive [`detect::function_word_language`] verdict that disagrees) (an audio-based measurement, so it beats text ID when both exist),
/// then text language ID ([`detect::detect_text`], ≥ [`detect::MIN_WORDS`] words), then
/// the tie-breaks — previous turn, speaker preference, household default.
pub fn resolve(cfg: &LanguageSettings, s: &LangSignals<'_>) -> TurnLang {
    if !cfg.is_bilingual() {
        return TurnLang::fixed(cfg.default);
    }
    let pick = |lang, source| TurnLang { lang, source };
    // A whole-utterance short phrase ("para", "stop", "obrigado") is unmistakable, and
    // exactly where engines mislabel the language — it decides first.
    if let Some(lang) = detect::short_phrase(s.transcript, &cfg.enabled) {
        return pick(lang, LangSource::Phrase);
    }
    // The transcript's function words, when decisive, are the words actually spoken —
    // they override an engine label that disagrees (measured: ElevenLabs labelled the
    // English "How do you say good morning in Portuguese?" as Portuguese).
    let words = detect::function_word_language(s.transcript, &cfg.enabled);
    if let Some(lang) = s.stt.and_then(|c| cfg.from_stt_code(c)) {
        return match words {
            Some(w) if w != lang => pick(w, LangSource::Text),
            _ => pick(lang, LangSource::Stt),
        };
    }
    if let Some(lang) = words {
        return pick(lang, LangSource::Text);
    }
    if let Some((lang, _)) = detect::detect_text(s.transcript, &cfg.enabled, cfg.default) {
        return pick(lang, LangSource::Text);
    }
    if let Some(lang) = s.previous.filter(|l| cfg.allows(*l)) {
        return pick(lang, LangSource::Sticky);
    }
    if let Some(lang) = s.speaker.filter(|l| cfg.allows(*l)) {
        return pick(lang, LangSource::Speaker);
    }
    pick(cfg.default, LangSource::Default)
}

/// The per-turn system-prompt block naming the reply language. Empty in single-language
/// mode (the prompt is then byte-identical to the pre-bilingual one).
pub fn prompt_block(cfg: &LanguageSettings, turn: &TurnLang) -> String {
    if !cfg.is_bilingual() {
        return String::new();
    }
    match turn.lang {
        Lang::En => "\n\n# Language\nThe person spoke English. Reply in English unless they \
                     explicitly ask for another language."
            .to_string(),
        Lang::PtBr => "\n\n# Language\nThe person spoke Brazilian Portuguese. Reply in \
                       Brazilian Portuguese (pt-BR) — Brazilian vocabulary and spelling, not \
                       European Portuguese — unless they explicitly ask for another language. \
                       Tool results and memories may be in English; still answer in Brazilian \
                       Portuguese."
            .to_string(),
    }
}

/// Plain-language problems with a bilingual setup — a language with no voice of its own,
/// an STT engine that needs extra setup — shown on the Voice page's Languages card and
/// logged at boot. Empty in single-language mode.
pub fn setup_warnings(
    lang: &LanguageSettings,
    global_voice: Option<&str>,
    stt: crate::config::SttEngineKind,
) -> Vec<String> {
    let mut out = Vec::new();
    if !lang.is_bilingual() {
        return out;
    }
    for l in &lang.enabled {
        match lang.voice_for(*l, global_voice) {
            Some(v) if l.owns_voice(&v) => {}
            Some(v) => out.push(format!(
                "{} has no {} Piper voice of its own, so its replies would be read by `{v}`. \
                 Pick one below (install a `{}-…` voice in Piper first).",
                l.english_name(),
                l.piper_locale(),
                l.piper_locale()
            )),
            None => out.push(format!(
                "{} has no Piper voice set, so its replies use the Piper server's default \
                 voice. Pick a `{}-…` voice below.",
                l.english_name(),
                l.piper_locale()
            )),
        }
    }
    match stt {
        crate::config::SttEngineKind::ElevenLabs => {}
        crate::config::SttEngineKind::WhisperLocal => out.push(
            "whisper-rs: bilingual mode needs a multilingual model (`stt.multilingual: true`, \
             e.g. `small` or `large-v3-turbo`); an `.en` model only transcribes English."
                .to_string(),
        ),
        crate::config::SttEngineKind::Wyoming => out.push(
            "Wyoming Whisper: run the faster-whisper server with a multilingual model and \
             without `--language`, or Portuguese will be transcribed as English."
                .to_string(),
        ),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bilingual() -> LanguageSettings {
        LanguageSettings {
            enabled: vec![Lang::En, Lang::PtBr],
            default: Lang::En,
            voices: BTreeMap::new(),
        }
    }

    #[test]
    fn parse_accepts_common_spellings() {
        for s in ["en", "EN", "en-US", "en_US", "eng", "English"] {
            assert_eq!(Lang::parse(s), Some(Lang::En), "{s}");
        }
        for s in ["pt", "pt-BR", "pt_BR", "por", "Portuguese", "português"] {
            assert_eq!(Lang::parse(s), Some(Lang::PtBr), "{s}");
        }
        assert_eq!(Lang::parse("de"), None);
        assert_eq!(Lang::parse(""), None);
    }

    #[test]
    fn serde_round_trips_as_codes() {
        let s = LanguageSettings {
            voices: [(Lang::PtBr, "pt_BR-faber-medium".to_string())].into(),
            ..bilingual()
        };
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(
            j,
            serde_json::json!({
                "enabled": ["en", "pt-BR"],
                "default": "en",
                "voices": { "pt-BR": "pt_BR-faber-medium" }
            })
        );
        let back: LanguageSettings = serde_json::from_value(j).unwrap();
        assert_eq!(back, s);
        assert!(serde_json::from_value::<LanguageSettings>(
            serde_json::json!({ "enabled": ["xx"], "default": "en" })
        )
        .is_err());
    }

    #[test]
    fn sanitized_keeps_the_default_enabled_and_dedupes() {
        let s = LanguageSettings {
            enabled: vec![Lang::PtBr, Lang::PtBr],
            default: Lang::En,
            voices: [(Lang::PtBr, "  ".to_string())].into(),
        }
        .sanitized();
        assert_eq!(s.enabled, vec![Lang::En, Lang::PtBr]);
        assert!(s.voices.is_empty());
    }

    #[test]
    fn voices_fall_back_to_the_global_voice() {
        let s = LanguageSettings {
            voices: [(Lang::PtBr, "pt_BR-faber-medium".to_string())].into(),
            ..bilingual()
        };
        assert_eq!(
            s.voice_for(Lang::PtBr, Some("en_US-amy-medium")).as_deref(),
            Some("pt_BR-faber-medium")
        );
        assert_eq!(
            s.voice_for(Lang::En, Some("en_US-amy-medium")).as_deref(),
            Some("en_US-amy-medium")
        );
        assert_eq!(s.voice_for(Lang::En, None), None);
        assert!(Lang::PtBr.owns_voice("pt_BR-faber-medium"));
        assert!(!Lang::PtBr.owns_voice("en_US-amy-medium"));
    }

    #[test]
    fn single_language_is_always_fixed() {
        let cfg = LanguageSettings::default();
        let t = resolve(
            &cfg,
            &LangSignals {
                stt: Some("pt"),
                transcript: "que horas são agora por favor",
                ..Default::default()
            },
        );
        assert_eq!(t, TurnLang::fixed(Lang::En));
        assert!(prompt_block(&cfg, &t).is_empty());
    }

    #[test]
    fn stt_report_wins_and_maps_lookalikes() {
        let cfg = bilingual();
        let r = |stt| {
            resolve(
                &cfg,
                &LangSignals {
                    stt: Some(stt),
                    transcript: "Clara Silva.",
                    ..Default::default()
                },
            )
        };
        assert_eq!(r("pt").lang, Lang::PtBr);
        assert_eq!(r("pt").source, LangSource::Stt);
        assert_eq!(r("por").lang, Lang::PtBr);
        assert_eq!(r("es").lang, Lang::PtBr);
        assert_eq!(r("eng").lang, Lang::En);
        // A language that isn't enabled is no evidence at all → falls to the default.
        assert_eq!(r("de").source, LangSource::Default);
    }

    #[test]
    fn decisive_function_words_overrule_a_mislabelled_engine_report() {
        let cfg = bilingual();
        let t = resolve(
            &cfg,
            &LangSignals {
                stt: Some("pt"),
                transcript: "How do you say \"good morning\" in Portuguese?",
                ..Default::default()
            },
        );
        assert_eq!((t.lang, t.source), (Lang::En, LangSource::Text));
        // Agreement (or no decisive verdict) keeps the engine's label.
        let t = resolve(
            &cfg,
            &LangSignals {
                stt: Some("pt"),
                transcript: "Que dia é hoje?",
                ..Default::default()
            },
        );
        assert_eq!((t.lang, t.source), (Lang::PtBr, LangSource::Stt));
        let t = resolve(
            &cfg,
            &LangSignals {
                stt: Some("en"),
                transcript: "play Garota de Ipanema by Tom Jobim",
                ..Default::default()
            },
        );
        assert_eq!((t.lang, t.source), (Lang::En, LangSource::Stt));
    }

    #[test]
    fn a_short_phrase_beats_a_mislabelled_engine_report() {
        let cfg = bilingual();
        let t = resolve(
            &cfg,
            &LangSignals {
                stt: Some("en"),
                transcript: "para.",
                ..Default::default()
            },
        );
        assert_eq!((t.lang, t.source), (Lang::PtBr, LangSource::Phrase));
        let t = resolve(
            &cfg,
            &LangSignals {
                stt: Some("pt"),
                transcript: "Stop.",
                ..Default::default()
            },
        );
        assert_eq!(t.lang, Lang::En);
    }

    #[test]
    fn text_id_then_tie_breaks() {
        let cfg = bilingual();
        let t = resolve(
            &cfg,
            &LangSignals {
                transcript: "me conta uma piada curta sobre gatos",
                ..Default::default()
            },
        );
        assert_eq!((t.lang, t.source), (Lang::PtBr, LangSource::Text));
        // Too short for text ID → previous turn, then speaker, then default.
        let short = |previous, speaker| {
            resolve(
                &cfg,
                &LangSignals {
                    transcript: "ok",
                    previous,
                    speaker,
                    ..Default::default()
                },
            )
        };
        assert_eq!(
            short(Some(Lang::PtBr), Some(Lang::En)).source,
            LangSource::Sticky
        );
        let s = short(None, Some(Lang::PtBr));
        assert_eq!((s.lang, s.source), (Lang::PtBr, LangSource::Speaker));
        let d = short(None, None);
        assert_eq!((d.lang, d.source), (Lang::En, LangSource::Default));
    }

    #[test]
    fn prompt_block_names_the_turn_language_only_when_bilingual() {
        let cfg = bilingual();
        let pt = prompt_block(&cfg, &TurnLang::fixed(Lang::PtBr));
        assert!(pt.contains("Brazilian Portuguese (pt-BR)"));
        let en = prompt_block(&cfg, &TurnLang::fixed(Lang::En));
        assert!(en.contains("Reply in English"));
    }
}
