//! `home_control` — the LLM fallback for Home Assistant (`plans/HomeAssistantPlan.md`
//! §A4). Most home commands never reach the model: the pipeline's fast path
//! (`orchestrator::home`) handles them before System-1 / recall / the LLM. This tool
//! covers the rest — unusual phrasings, multi-step requests, "what's on in the
//! office?" — using the **same** resolver and the same direct REST calls, so the model
//! can only ever act on a device the catalog actually contains.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use rig_core::completion::ToolDefinition;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{definition, parse_args, ToolCtx, VoiceTool};
use crate::home_assistant::{HomeAssistant, HomeCommand, Verb, UNREACHABLE};

/// Tool name: control or query Home Assistant devices.
pub(super) const HOME_CONTROL: &str = "home_control";

#[derive(Debug, Deserialize)]
struct HomeArgs {
    action: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    brightness_pct: Option<u8>,
}

struct HomeControl {
    hub: Arc<HomeAssistant>,
}

/// The `home_control` tool over the shared hub.
pub(super) fn home_control(hub: Arc<HomeAssistant>) -> Arc<dyn VoiceTool> {
    Arc::new(HomeControl { hub })
}

#[async_trait]
impl VoiceTool for HomeControl {
    fn name(&self) -> &'static str {
        HOME_CONTROL
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            HOME_CONTROL,
            "Control or query the household's smart-home devices through Home Assistant: \
             lights, switches, scenes, scripts and TV/media-player power. `target` is a \
             device name (\"porch lights\"), a room (\"office lights\", \"lights in the \
             kitchen\"), \"all lights\", or a scene/script name. Use action \"list\" to see \
             every room and device with its current state.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["turn_on", "turn_off", "activate", "set_brightness", "status", "list"],
                        "description": "turn_on/turn_off a device or room; activate a scene or script; set_brightness of lights; status = is it on; list = all devices and states."
                    },
                    "target": {
                        "type": "string",
                        "description": "Device, room, 'all lights', or scene/script name. Not needed for list."
                    },
                    "brightness_pct": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 100,
                        "description": "Brightness percent, for set_brightness."
                    }
                },
                "required": ["action"]
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "For smart-home requests (lights, switches, scenes, TV power, \"is the porch \
             light on?\"), call home_control. If it can't find a device, call it with \
             action \"list\" and retry with the exact name, or ask the user which one they \
             meant. Never say a device changed unless home_control confirmed it.",
        )
    }

    async fn invoke(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<String> {
        let args: HomeArgs = parse_args(args, HOME_CONTROL)?;
        // Every action needs the catalog; distinguish "HA is down" from "no such device".
        let catalog = self
            .hub
            .catalog()
            .await
            .context("home control failed: Home Assistant isn't reachable right now")?;

        let verb = match args.action.as_str() {
            "list" => {
                return self
                    .hub
                    .describe()
                    .await
                    .context("home control failed: Home Assistant isn't reachable right now");
            }
            "turn_on" => Verb::On,
            "turn_off" => Verb::Off,
            "activate" => Verb::Activate,
            "status" => Verb::Status,
            "set_brightness" => Verb::Brightness(
                args.brightness_pct
                    .context("home control failed: set_brightness needs brightness_pct")?
                    .min(100),
            ),
            other => bail!("home control failed: unknown action {other:?}"),
        };
        let target = args
            .target
            .filter(|t| !t.trim().is_empty())
            .context("home control failed: a target is required")?;

        let cmd = HomeCommand {
            verb,
            target: target.clone(),
        };
        match self.hub.run(&cmd, None).await {
            Some(text) if text == UNREACHABLE => {
                bail!("home control failed: Home Assistant didn't respond")
            }
            Some(text) => Ok(text),
            None => bail!(
                "home control failed: no single device, room or scene matches {target:?} for \
                 {action}. Rooms: {rooms}. Call home_control with action \"list\" for exact \
                 device names, or ask the user which one they meant.",
                action = args.action,
                rooms = catalog.areas().join(", "),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home_assistant::catalog::tests::house;
    use crate::home_assistant::tests::{hub_with, FakeApi};

    fn tool() -> (Arc<FakeApi>, Arc<dyn VoiceTool>) {
        let api = Arc::new(FakeApi {
            catalog: house(),
            ..Default::default()
        });
        (api.clone(), home_control(hub_with(api, None)))
    }

    async fn call(tool: &Arc<dyn VoiceTool>, args: Value) -> Result<String> {
        tool.invoke(&args, &ToolCtx { actions: None }).await
    }

    #[tokio::test]
    async fn turns_a_room_off_through_the_shared_resolver() {
        let (api, tool) = tool();
        let said = call(
            &tool,
            json!({"action": "turn_off", "target": "office lights"}),
        )
        .await
        .unwrap();
        assert_eq!(said, "Office lights off.");
        assert_eq!(api.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_targets_explain_how_to_recover() {
        let (api, tool) = tool();
        let err = call(&tool, json!({"action": "turn_on", "target": "garage"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no single device"), "{err}");
        assert!(err.contains("Office"), "{err}");
        assert!(api.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_and_brightness() {
        let (_, tool) = tool();
        let list = call(&tool, json!({"action": "list"})).await.unwrap();
        assert!(list.contains("Office: "), "{list}");
        assert!(list.contains("Porch lights [switch] (off)"), "{list}");
        let said = call(
            &tool,
            json!({"action": "set_brightness", "target": "dining lights", "brightness_pct": 40}),
        )
        .await
        .unwrap();
        assert_eq!(said, "Dining lights at 40 percent.");
        assert!(call(
            &tool,
            json!({"action": "set_brightness", "target": "dining lights"})
        )
        .await
        .is_err());
    }
}
