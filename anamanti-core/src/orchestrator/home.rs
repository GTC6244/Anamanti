//! The Home Assistant fast path (`plans/HomeAssistantPlan.md` §A1–A2).
//!
//! Runs right after the explicit memory-command check and **before** System-1, memory
//! recall and the LLM. A sentence that isn't shaped like a home command is rejected by
//! pure string code, so ordinary turns pay nothing; a command whose target resolves
//! against the cached device catalog is carried out with one direct REST call and a
//! short fixed confirmation. Anything ambiguous or unknown returns `None` and the turn
//! continues down the normal path untouched.

use tokio::time::Instant;

use crate::lang::Lang;

use super::speech_out::{Delivery, FastReply};
use super::{Pipeline, TurnCtx};

impl Pipeline {
    /// Answer `transcript` as a home command when it is one; `None` to fall through.
    pub(crate) async fn run_home_fast_path(
        &self,
        ctx: &mut TurnCtx<'_>,
        transcript: &str,
    ) -> Option<(String, Delivery)> {
        let hub = self.home_assistant.as_ref()?;
        let t0 = Instant::now();
        // Every enabled language's command grammar (turn language first); the one that
        // matched is the language spoken (plans/BilingualPlan.md §2.6). Single-language
        // mode tries only the default — English, exactly as before.
        let room = ctx.device_ctx.room.clone();
        let mut answered = None;
        for lang in super::language::phrase_order(ctx) {
            let said = match lang {
                Lang::En => hub.handle_utterance(transcript, room.as_deref()).await,
                Lang::PtBr => hub.handle_utterance_pt(transcript, room.as_deref()).await,
            };
            if let Some(text) = said {
                answered = Some((text, lang));
                break;
            }
        }
        let (text, lang) = answered?;
        self.phrase_matched(ctx, lang);
        let elapsed = t0.elapsed().as_millis() as u64;
        log::info!("home fast path: {transcript:?} → {text:?} in {elapsed}ms");
        ctx.timing.path = Some("home".to_string());
        ctx.timing.fetch_ms = Some(elapsed);
        let reply = FastReply::say(text);
        let delivery = self.speak_fast_reply(ctx, &reply).await;
        Some((reply.text, delivery))
    }
}
