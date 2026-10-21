//! The per-turn language (`plans/BilingualPlan.md` §2.2): resolve it once after STT,
//! keep it on the [`TurnCtx`], and point the turn's Piper voice at it.
//!
//! In single-language mode (the default) every helper here is inert: the turn language
//! is the fixed default, the voice is untouched, nothing is recorded, and the fast-path
//! order is just that one language — so the turn is byte-identical to the pre-bilingual
//! pipeline.

use crate::lang::{self, Lang, LangSignals, LangSource, TurnLang};
use crate::memory::chatlog::now_secs;
use crate::speaker::SpeakerContext;

use super::{Pipeline, TurnCtx};

impl Pipeline {
    /// Decide the turn's language from the STT report, the transcript, the previous turn
    /// of this follow-up chain and the speaker's preferred language, then apply it
    /// ([`Self::set_turn_lang`]).
    pub(crate) fn resolve_turn_lang(
        &self,
        ctx: &mut TurnCtx<'_>,
        stt_lang: Option<&str>,
        transcript: &str,
        speaker: &SpeakerContext,
    ) {
        if !ctx.runtime.language.is_bilingual() {
            ctx.lang = TurnLang::fixed(ctx.runtime.language.default);
            return;
        }
        let previous = if ctx.followup_depth > 0 {
            (*self.last_lang.lock().unwrap())
                .filter(|(_, at)| now_secs() - at <= self.follow_up.history_window_secs)
                .map(|(l, _)| l)
        } else {
            None
        };
        let speaker_pref = speaker
            .reader()
            .and(speaker.name.as_deref())
            .and_then(|n| ctx.runtime.household.member_matching_spoken(n))
            .and_then(|m| m.language.as_deref())
            .and_then(Lang::parse);
        let turn = lang::resolve(
            &ctx.runtime.language,
            &LangSignals {
                stt: stt_lang,
                transcript,
                previous,
                speaker: speaker_pref,
            },
        );
        log::info!(
            "turn language: {} (via {}{})",
            turn.lang,
            turn.source.as_str(),
            stt_lang
                .map(|l| format!(", stt reported {l}"))
                .unwrap_or_default()
        );
        self.set_turn_lang(ctx, turn);
    }

    /// Make `turn` the turn's language: record it, remember it for the follow-up chain,
    /// and speak with that language's Piper voice. A no-op in single-language mode.
    pub(crate) fn set_turn_lang(&self, ctx: &mut TurnCtx<'_>, turn: TurnLang) {
        if !ctx.runtime.language.is_bilingual() {
            return;
        }
        if ctx.lang.lang != turn.lang && turn.source == LangSource::Phrase {
            log::info!(
                "turn language: {} → {} (a {} phrase matched)",
                ctx.lang.lang,
                turn.lang,
                turn.lang.english_name()
            );
        }
        ctx.lang = turn;
        ctx.runtime.tts_voice = ctx
            .runtime
            .language
            .voice_for(turn.lang, ctx.base_voice.as_deref());
        ctx.timing.lang = Some(turn.lang.code().to_string());
        ctx.timing.lang_source = Some(turn.source.as_str().to_string());
        *self.last_lang.lock().unwrap() = Some((turn.lang, now_secs()));
    }

    /// A fast-path phrase table matched in `lang`: that is what the user spoke, so it
    /// wins over the detected language.
    pub(crate) fn phrase_matched(&self, ctx: &mut TurnCtx<'_>, lang: Lang) {
        if ctx.lang.lang != lang {
            self.set_turn_lang(
                ctx,
                TurnLang {
                    lang,
                    source: LangSource::Phrase,
                },
            );
        }
    }
}

/// The order to try fast-path phrase tables in: the turn's language first, then the
/// other enabled languages. Single-language mode → just that language.
pub(crate) fn phrase_order(ctx: &TurnCtx<'_>) -> Vec<Lang> {
    let mut order = vec![ctx.lang.lang];
    order.extend(
        ctx.runtime
            .language
            .enabled
            .iter()
            .copied()
            .filter(|l| *l != ctx.lang.lang),
    );
    order
}

/// [`reply_language_switch`] for the first reply sentence, returning the language and a
/// copy of the turn settings re-pointed at its voice (`None` = keep the turn's voice).
pub(crate) fn first_sentence_voice(
    turn_lang: Lang,
    runtime: &crate::settings::RuntimeSettings,
    base_voice: Option<&str>,
    sentence: &str,
) -> Option<(Lang, crate::settings::RuntimeSettings)> {
    let lang = reply_language_switch(turn_lang, &runtime.language, sentence)?;
    let mut switched = runtime.clone();
    switched.tts_voice = runtime.language.voice_for(lang, base_voice);
    Some((lang, switched))
}

/// Bilingual mode, first reply sentence: if the LLM answered in a different enabled
/// language than the turn (e.g. the user said "answer in English"), return it so the
/// voice can follow. Biased toward the turn's language — only a clear lead switches.
pub(crate) fn reply_language_switch(
    ctx_lang: Lang,
    cfg: &lang::LanguageSettings,
    sentence: &str,
) -> Option<Lang> {
    if !cfg.is_bilingual() {
        return None;
    }
    // Function words first (decisive on a normal sentence — trigram ID gives English a
    // weak lead and would keep the turn's voice); trigram ID as the fallback.
    lang::detect::function_word_language(sentence, &cfg.enabled)
        .or_else(|| lang::detect::detect_text(sentence, &cfg.enabled, ctx_lang).map(|(l, _)| l))
        .filter(|l| *l != ctx_lang)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::LanguageSettings;

    #[test]
    fn reply_switch_catches_an_english_answer_to_a_portuguese_turn() {
        let cfg = LanguageSettings {
            enabled: vec![Lang::En, Lang::PtBr],
            default: Lang::En,
            voices: Default::default(),
        };
        // The live case: "responde em inglês…" → an English first sentence.
        assert_eq!(
            reply_language_switch(Lang::PtBr, &cfg, "The capital of Brazil is Brasília."),
            Some(Lang::En)
        );
        // A Portuguese reply to a Portuguese turn, or one quoting English words, stays.
        assert_eq!(
            reply_language_switch(
                Lang::PtBr,
                &cfg,
                "\"Bom dia\" em inglês é \"good morning\"."
            ),
            None
        );
        assert_eq!(
            reply_language_switch(
                Lang::En,
                &cfg,
                "In Portuguese, good morning is \"bom dia\"."
            ),
            None
        );
        // Never in single-language mode.
        assert_eq!(
            reply_language_switch(Lang::En, &LanguageSettings::default(), "Que dia é hoje?"),
            None
        );
    }
}
