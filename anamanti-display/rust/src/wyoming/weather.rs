//! Persistent ambient-weather channel (`role=weather`): reads the orchestrator's
//! periodic `anamanti-weather` current-conditions broadcast so the small icon +
//! temperature beside the idle clock stay fresh without a voice turn. The connection,
//! registration and reconnect logic is the shared [`super::push_channel`] driver; this
//! module only picks the weather report out of each frame (the raw `weather` JSON object,
//! as a string).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use super::discovery::EndpointCache;
use super::protocol::{self, WyomingEvent};
use super::push_channel::{self, Channel};

/// Extract the serialized `weather` report object from an `anamanti-weather` frame,
/// or `None` if `ev` is a different frame or carries no report. Both `show` and
/// `current` actions carry a report; the ambient channel normally sees `current`.
pub fn report_json(ev: &WyomingEvent) -> Option<String> {
    match ev.weather_command()? {
        protocol::WeatherCommand::Show(v) | protocol::WeatherCommand::Current(v) => {
            Some(v.to_string())
        }
        protocol::WeatherCommand::Dismiss => None,
    }
}

/// Run the persistent weather channel until `running` is cleared or the consumer goes
/// away (see [`push_channel::run`]). Each pushed report's JSON is handed to `on_report`.
pub async fn run<F>(
    cache: &EndpointCache,
    discovery_timeout: Duration,
    orchestrator_key: Option<String>,
    device_id: String,
    device_name: String,
    running: Arc<AtomicBool>,
    on_report: F,
) where
    F: FnMut(String) -> bool,
{
    let channel = Channel {
        label: "weather",
        hello: WyomingEvent::hello_weather(device_id, "", device_name),
    };
    push_channel::run(
        cache,
        discovery_timeout,
        orchestrator_key,
        channel,
        running,
        report_json,
        on_report,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_report_from_current_and_show() {
        let report = json!({ "location_label": "Austin", "units": "imperial" });
        let cur = WyomingEvent::weather_current(report.clone());
        assert!(report_json(&cur).unwrap().contains("Austin"));
        let show = WyomingEvent::weather_show(report);
        assert!(report_json(&show).unwrap().contains("imperial"));
    }

    #[test]
    fn dismiss_and_other_frames_yield_no_report() {
        assert_eq!(report_json(&WyomingEvent::weather_dismiss()), None);
        assert_eq!(report_json(&WyomingEvent::interrupt()), None);
    }
}
