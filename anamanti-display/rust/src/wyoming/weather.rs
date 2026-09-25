//! Persistent ambient-weather channel — the twin of [`super::notify`].
//!
//! The device dials the orchestrator's Wyoming endpoint, sends an `anamanti-hello`
//! `role=weather` frame to register, then holds the socket open reading
//! `anamanti-weather` push frames (the orchestrator's periodic current-conditions
//! broadcast) — reconnecting with capped exponential backoff whenever the connection
//! drops. Separate from both the per-turn voice socket and the notify channel, so the
//! small icon + temperature beside the idle clock stay fresh without a voice turn.
//!
//! The driver is transport-only: it forwards each pushed report (the raw `weather`
//! JSON object, as a string) to a callback that the FRB layer turns into a stream
//! event for Flutter.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::BufReader;
use tokio::net::TcpStream;

use super::discovery::{resolve, EndpointCache};
use super::protocol::{self, WyomingEvent};

/// Shortest / longest wait between reconnect attempts.
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How long a blocking read waits before it yields so the loop can re-check `running`.
const READ_POLL: Duration = Duration::from_secs(2);

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
/// away. Each pushed report's JSON is handed to `on_report`, which returns `false`
/// when the downstream consumer (the FRB sink) has been dropped — that ends the loop.
/// Reconnects with capped exponential backoff on any drop or failure.
pub async fn run<F>(
    cache: &EndpointCache,
    discovery_timeout: Duration,
    orchestrator_key: Option<String>,
    device_id: String,
    running: Arc<AtomicBool>,
    mut on_report: F,
) where
    F: FnMut(String) -> bool,
{
    let mut backoff = MIN_BACKOFF;
    while running.load(Ordering::SeqCst) {
        match connect_and_listen(
            cache,
            discovery_timeout,
            orchestrator_key.as_deref(),
            &device_id,
            &running,
            &mut on_report,
        )
        .await
        {
            Ok(ListenEnd::ConsumerGone) => return,
            Ok(ListenEnd::Closed) => backoff = MIN_BACKOFF,
            Err(e) => {
                log::debug!("weather channel: {e:#}; retrying in {backoff:?}");
                cache.clear();
            }
        }
        let mut waited = Duration::ZERO;
        while running.load(Ordering::SeqCst) && waited < backoff {
            let step = READ_POLL.min(backoff - waited);
            tokio::time::sleep(step).await;
            waited += step;
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Why a single connection ended.
enum ListenEnd {
    Closed,
    ConsumerGone,
}

/// One connection lifecycle: resolve → dial → register (`role=weather`) → read pushes
/// until the socket closes, we're stopped, or the consumer goes away.
async fn connect_and_listen<F>(
    cache: &EndpointCache,
    discovery_timeout: Duration,
    orchestrator_key: Option<&str>,
    device_id: &str,
    running: &Arc<AtomicBool>,
    on_report: &mut F,
) -> Result<ListenEnd>
where
    F: FnMut(String) -> bool,
{
    let endpoint = resolve(cache, discovery_timeout, orchestrator_key)
        .await
        .context("resolving orchestrator for weather channel")?;
    let stream = TcpStream::connect(endpoint.socket_addr())
        .await
        .with_context(|| format!("dialing {endpoint} for weather channel"))?;
    stream.set_nodelay(true).ok();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    // Register this connection as the device's weather channel (role=weather).
    protocol::write_event(&mut writer, &WyomingEvent::hello_weather(device_id, ""))
        .await
        .context("sending anamanti-hello (weather)")?;
    log::info!("weather channel connected to {endpoint}");

    loop {
        if !running.load(Ordering::SeqCst) {
            return Ok(ListenEnd::Closed);
        }
        match tokio::time::timeout(READ_POLL, protocol::read_event(&mut reader)).await {
            Err(_elapsed) => continue,
            Ok(read) => match read.context("reading weather frame")? {
                None => return Ok(ListenEnd::Closed),
                Some(ev) => {
                    if let Some(json) = report_json(&ev) {
                        if !on_report(json) {
                            return Ok(ListenEnd::ConsumerGone);
                        }
                    }
                }
            },
        }
    }
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
