//! The camera fast path (`plans/CameraPlan.md`).
//!
//! Runs right after the Home Assistant fast path and **before** System-1, memory recall
//! and the LLM. "Show the front door" / "who's at the door" opens the live view on
//! *this* display (the frame goes down this turn's socket), and while the view is open a
//! bare "stop" / "close it" closes it. Non-camera turns are rejected by pure string code
//! in [`crate::camera::CameraHub::match_utterance`], so ordinary turns pay nothing.

use super::speech_out::{Delivery, FastReply};
use super::{Pipeline, TurnCtx};
use crate::camera::CameraIntent;
use crate::llm::DeviceAction;
use crate::wyoming::protocol::DisplayContext;

impl Pipeline {
    /// Answer `transcript` as a camera show/close when it is one; `None` to fall through.
    pub(crate) async fn run_camera_fast_path(
        &self,
        ctx: &mut TurnCtx<'_>,
        transcript: &str,
    ) -> Option<(String, Delivery)> {
        let hub = self.settings.camera()?;
        let camera_open = matches!(ctx.device_ctx.widget, Some(DisplayContext::Camera(_)));
        let timer_running = ctx.device_ctx.timers.any_running();
        let reply = match hub.match_utterance(transcript, camera_open, timer_running)? {
            CameraIntent::Show(name) => FastReply::say(format!("Here's the {name}."))
                .with_action(DeviceAction::ShowCamera { name }),
            CameraIntent::Dismiss => {
                FastReply::say("Okay.").with_action(DeviceAction::DismissCamera)
            }
        }
        .ending_session();
        log::info!("camera fast path: {transcript:?} → {:?}", reply.action);
        ctx.timing.path = Some("camera".to_string());
        let delivery = self.speak_fast_reply(ctx, &reply).await;
        Some((reply.text, delivery))
    }
}
