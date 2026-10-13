//! Always-on device tools: countdown timers, the output personality, and the
//! on-screen font scale. They need no provider or config — each just emits a
//! [`DeviceAction`] on the per-turn sink and returns a spoken confirmation.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use rig_core::completion::ToolDefinition;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{definition, emit, parse_args, parse_args_or_default, ToolCtx, VoiceTool};
use crate::llm::{DeviceAction, FontDirection};
use crate::speech::DurationStyle;

/// Tool name: start a countdown timer on the device.
pub(super) const SET_TIMER: &str = "set_timer";
/// Tool name: cancel one or all countdown timers on the device.
const CANCEL_TIMER: &str = "cancel_timer";
/// Tool name: switch the assistant's output personality (voice style), or turn it off.
pub(super) const SET_PERSONALITY: &str = "set_personality";
/// Tool name: adjust the global on-screen font scale ("increase/decrease font").
pub(super) const ADJUST_FONT: &str = "adjust_font";

// Hard cap on a timer duration (24h), shared with the System-1 timer fast path.
use crate::timer::MAX_TIMER_SECS;

/// The always-on device tools, in advertised order.
pub(super) fn tools() -> Vec<Arc<dyn VoiceTool>> {
    vec![
        Arc::new(SetTimer),
        Arc::new(CancelTimer),
        Arc::new(SetPersonality),
        Arc::new(AdjustFont),
    ]
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

/// Typed args for `set_timer`.
#[derive(Debug, Deserialize)]
struct SetTimerArgs {
    /// Timer length in whole seconds (the model converts "5 minutes" → 300).
    duration_seconds: u64,
    /// Optional spoken label ("pasta", "laundry").
    #[serde(default)]
    label: Option<String>,
}

/// Typed args for `cancel_timer`.
#[derive(Debug, Deserialize, Default)]
struct CancelTimerArgs {
    /// Which timer to cancel by label; absent/empty cancels *all* timers.
    #[serde(default)]
    label: Option<String>,
}

/// Start a countdown timer on the device.
struct SetTimer;

#[async_trait]
impl VoiceTool for SetTimer {
    fn name(&self) -> &'static str {
        SET_TIMER
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            SET_TIMER,
            "Start a countdown timer on the device. Use for any \"set a timer for …\" / \
             \"remind me in …\" request. Convert the requested time to whole seconds.",
            json!({
                "type": "object",
                "properties": {
                    "duration_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Timer length in whole seconds (e.g. 5 minutes → 300)."
                    },
                    "label": {
                        "type": "string",
                        "description": "Optional short name for the timer, e.g. \"pasta\"."
                    }
                },
                "required": ["duration_seconds"]
            }),
        )
    }

    /// Covers both timer tools (they are always advertised together).
    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You can start and cancel countdown timers on the device with the `set_timer` \
             (convert the requested time to whole seconds) and `cancel_timer` tools. When the user \
             asks to set, start, or cancel a timer, call the tool — never say you are unable to set \
             timers. There can be any number of timers running at once.",
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: SetTimerArgs = parse_args(args, SET_TIMER)?;
        let sink = ctx.require_sink("no device is connected to run a timer on right now")?;
        let secs = args.duration_seconds.clamp(1, MAX_TIMER_SECS);
        let label = args.label.filter(|s| !s.trim().is_empty());
        emit(
            Some(sink),
            DeviceAction::StartTimer {
                label: label.clone(),
                duration_secs: secs,
            },
            "the timer could start",
        )?;
        let spoken = crate::speech::duration(secs as f64, DurationStyle::Exact);
        Ok(match &label {
            Some(l) => format!("Started a {spoken} timer for {l}."),
            None => format!("Started a {spoken} timer."),
        })
    }
}

/// Cancel one or all countdown timers on the device.
struct CancelTimer;

#[async_trait]
impl VoiceTool for CancelTimer {
    fn name(&self) -> &'static str {
        CANCEL_TIMER
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            CANCEL_TIMER,
            "Cancel a running countdown timer. Pass the timer's label to cancel just \
             that one, or omit it to cancel all timers.",
            json!({
                "type": "object",
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "Label of the timer to cancel; omit to cancel every timer."
                    }
                }
            }),
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: CancelTimerArgs = parse_args_or_default(args, CANCEL_TIMER)?;
        let sink = ctx.require_sink("no device is connected to manage timers on right now")?;
        let label = args.label.filter(|s| !s.trim().is_empty());
        emit(
            Some(sink),
            DeviceAction::CancelTimer {
                label: label.clone(),
            },
            "the timer could be cancelled",
        )?;
        Ok(match &label {
            Some(l) => format!("Cancelled the {l} timer."),
            None => "Cancelled all timers.".to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Personality (Core-side output tuning)
// ---------------------------------------------------------------------------

/// Typed args for `set_personality`.
#[derive(Debug, Deserialize)]
struct SetPersonalityArgs {
    /// The personality to switch to (one of the names listed in the turn instructions),
    /// or "normal"/"off" to turn personality off.
    name: String,
}

/// Switch the output personality. Emits a [`DeviceAction::SetPersonality`]; the
/// orchestrator resolves it against the live catalog and applies it to the settings
/// (it takes effect on the next turn).
struct SetPersonality;

#[async_trait]
impl VoiceTool for SetPersonality {
    fn name(&self) -> &'static str {
        SET_PERSONALITY
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            SET_PERSONALITY,
            "Change the personality / voice style you speak in. This is PURE OUTPUT TUNING — it \
             changes only how your replies sound, never what you know, remember, or do. Call it \
             when the user asks you to talk like one of the available personalities, switch \
             personality, or go back to normal. The current turn's instructions list the \
             available personality names and which one is active; pass \"normal\" to turn it off.",
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The personality name to switch to (one of the names in the \
                                        turn instructions), or \"normal\" to turn personality off."
                    }
                },
                "required": ["name"]
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You can change the personality / voice style you speak in with the `set_personality` \
             tool (pure output tuning — it changes only how you sound, not what you know or do). \
             Call it whenever the user asks you to talk like one of the available personalities, \
             switch personality, or go back to normal. The turn's instructions list the available \
             personality names and which one is active; pass \"normal\" to turn it off.",
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: SetPersonalityArgs = parse_args(args, SET_PERSONALITY)?;
        let name = args.name.trim().to_string();
        let off = name.is_empty()
            || name.eq_ignore_ascii_case("normal")
            || name.eq_ignore_ascii_case("off")
            || name.eq_ignore_ascii_case("none");
        emit(
            ctx.actions,
            DeviceAction::SetPersonality { name: name.clone() },
            "the personality could change",
        )?;
        Ok(if off {
            "Okay, going back to my normal voice.".to_string()
        } else {
            format!("Okay — switching to the {name} personality.")
        })
    }
}

// ---------------------------------------------------------------------------
// Font scale
// ---------------------------------------------------------------------------

/// Typed args for `adjust_font`.
#[derive(Debug, Deserialize)]
struct AdjustFontArgs {
    /// `"increase"` (bigger) or `"decrease"` (smaller).
    action: String,
}

/// Step the global on-screen font scale up or down.
struct AdjustFont;

#[async_trait]
impl VoiceTool for AdjustFont {
    fn name(&self) -> &'static str {
        ADJUST_FONT
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            ADJUST_FONT,
            "Make the text on the display bigger or smaller. Use ONLY when the turn \
             context says a resizable text surface is on screen; if the context says \
             the text is already at the maximum (for increase) or minimum (for \
             decrease) size, tell the user that instead of calling this tool.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["increase", "decrease"],
                        "description": "increase = make the text bigger; decrease = make it smaller."
                    }
                },
                "required": ["action"]
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You can make the on-screen text larger or smaller with the `adjust_font` tool \
             (action `increase` / `decrease`), but ONLY when the turn context says a resizable \
             text surface is on screen. When the user asks to increase/decrease the font, make \
             the text bigger/smaller, or says it's too small/too big to read, call `adjust_font` \
             and relay its short spoken confirmation. If the context says there is nothing to \
             resize, tell the user there's nothing on screen to resize; if it says the text is \
             already at the maximum (for increase) or minimum (for decrease), tell them that \
             instead of calling the tool.",
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: AdjustFontArgs = parse_args(args, ADJUST_FONT)?;
        let (dir, confirmation) = match args.action.as_str() {
            "increase" => (FontDirection::Increase, "Making the text bigger."),
            "decrease" => (FontDirection::Decrease, "Making the text smaller."),
            other => anyhow::bail!("unknown adjust_font action `{other}`"),
        };
        emit(
            ctx.actions,
            DeviceAction::AdjustFont(dir),
            "the font could change",
        )?;
        Ok(confirmation.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn call(
        tool: &dyn VoiceTool,
        args: Value,
        tx: Option<&crate::llm::ActionSink>,
    ) -> Result<String> {
        tool.invoke(&args, &ToolCtx { actions: tx }).await
    }

    #[tokio::test]
    async fn set_timer_emits_start_action_and_confirms() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out = call(
            &SetTimer,
            json!({ "duration_seconds": 300, "label": "pasta" }),
            Some(&tx),
        )
        .await
        .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::StartTimer {
                label: Some("pasta".to_string()),
                duration_secs: 300
            }
        );
        assert_eq!(out, "Started a 5 minutes timer for pasta.");
        let out = call(&SetTimer, json!({ "duration_seconds": 3661 }), Some(&tx))
            .await
            .unwrap();
        assert_eq!(out, "Started a 1 hour 1 minute 1 second timer.");
    }

    #[tokio::test]
    async fn set_timer_clamps_and_without_device_errors() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        call(
            &SetTimer,
            json!({ "duration_seconds": 999999999u64 }),
            Some(&tx),
        )
        .await
        .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::StartTimer {
                label: None,
                duration_secs: MAX_TIMER_SECS
            }
        );
        // No device attached → the tool reports an error the model can relay.
        let err = call(&SetTimer, json!({ "duration_seconds": 60 }), None)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "no device is connected to run a timer on right now"
        );
        // A closed sink → the device disconnected mid-turn.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let err = call(&SetTimer, json!({ "duration_seconds": 60 }), Some(&tx))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "the device disconnected before the timer could start"
        );
    }

    #[tokio::test]
    async fn cancel_timer_null_args_cancels_all() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out = call(&CancelTimer, Value::Null, Some(&tx)).await.unwrap();
        assert_eq!(out, "Cancelled all timers.");
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::CancelTimer { label: None }
        );
        call(&CancelTimer, json!({ "label": "pasta" }), Some(&tx))
            .await
            .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::CancelTimer {
                label: Some("pasta".to_string())
            }
        );
    }

    #[tokio::test]
    async fn set_personality_emits_action_and_confirms() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // A named personality → emit the raw name for the orchestrator to resolve.
        let out = call(&SetPersonality, json!({ "name": "gangster" }), Some(&tx))
            .await
            .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::SetPersonality {
                name: "gangster".to_string()
            }
        );
        assert_eq!(out, "Okay — switching to the gangster personality.");

        // "normal" turns it off with the normal-voice confirmation.
        let off = call(&SetPersonality, json!({ "name": "normal" }), Some(&tx))
            .await
            .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::SetPersonality {
                name: "normal".to_string()
            }
        );
        assert_eq!(off, "Okay, going back to my normal voice.");

        // No device attached → the tool reports an error the model can relay.
        let err = call(&SetPersonality, json!({ "name": "trump" }), None)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "no device is connected right now");
    }

    #[tokio::test]
    async fn adjust_font_emits_direction_action_and_confirms() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out = call(&AdjustFont, json!({ "action": "increase" }), Some(&tx))
            .await
            .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::AdjustFont(FontDirection::Increase)
        );
        assert_eq!(out, "Making the text bigger.");

        call(&AdjustFont, json!({ "action": "decrease" }), Some(&tx))
            .await
            .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::AdjustFont(FontDirection::Decrease)
        );
    }

    #[tokio::test]
    async fn adjust_font_rejects_unknown_action_and_missing_device() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = call(&AdjustFont, json!({ "action": "zoom" }), Some(&tx))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "unknown adjust_font action `zoom`");
        // No device attached → the tool reports an error the model can relay.
        let err = call(&AdjustFont, json!({ "action": "increase" }), None)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "no display is connected right now");
    }
}
