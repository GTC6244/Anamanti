//! The always-on ambient weather push: a registry of connected weather channels plus
//! a periodic task that fetches current conditions for the household location and
//! fans them out as `anamanti-weather` `{"action":"current"}` frames — so the small
//! icon + temperature beside the idle clock stay fresh without a voice turn.
//!
//! Structurally a twin of [`crate::notify::NotificationService`] — both wrap a
//! [`ChannelRegistry`]: the socket is owned by the per-connection task in
//! [`crate::server`]; the registry only holds an mpsc *sender* into that task. A
//! newly-registered channel is immediately sent the last-known report so the indicator
//! appears at once rather than after the next tick.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use serde_json::{to_value as json_to_value, Value};

use crate::notify::ChannelRegistry;
use crate::settings::SharedSettings;
use crate::weather::{WeatherProvider, WeatherReport};
use crate::wyoming::protocol::WyomingEvent;

/// Serialize a report to the JSON `weather` payload carried in the frame; an empty
/// object on the impossible serialize failure keeps the push infallible.
fn to_value(report: &WeatherReport) -> Value {
    json_to_value(report).unwrap_or_default()
}

/// Registry of connected weather channels + the last-known report. Cheap to share
/// behind an `Arc`; construct once at boot and hand a clone to the device-facing
/// server (which registers live channels) and the periodic push task.
#[derive(Default)]
pub struct WeatherService {
    channels: ChannelRegistry<()>,
    /// The most recent report, replayed to a channel the moment it connects.
    last: Mutex<Option<WeatherReport>>,
}

impl WeatherService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a newly-opened weather channel. Returns a connection handle (used to
    /// [`deregister`](Self::deregister) on close) and the receiver the connection task
    /// drains to write pushes out. If a report is already cached it is queued
    /// immediately so the device shows conditions without waiting for the next tick.
    pub fn register(&self) -> (u64, mpsc::UnboundedReceiver<WyomingEvent>) {
        let initial = self
            .last
            .lock()
            .unwrap()
            .as_ref()
            .map(|report| WyomingEvent::weather_current(to_value(report)));
        self.channels.register((), initial)
    }

    /// Drop a channel from the registry (its task is ending / the device closed).
    pub fn deregister(&self, conn_id: u64) {
        self.channels.deregister(conn_id);
    }

    /// How many weather channels are currently connected.
    pub fn connected(&self) -> usize {
        self.channels.len()
    }

    /// Cache `report` and push it to every connected device as a `current` frame.
    /// Dead channels (receiver dropped) are pruned. Returns the delivery count.
    pub fn broadcast(&self, report: &WeatherReport) -> usize {
        *self.last.lock().unwrap() = Some(report.clone());
        self.channels
            .broadcast(&WyomingEvent::weather_current(to_value(report)))
    }
}

/// Spawn the periodic ambient-weather push: every `interval`, one [`push_once`] with the
/// *currently selected* forecast provider built from `settings` (so a config-page
/// provider/key change retargets the push without a restart). Returns immediately; the
/// task runs for the process lifetime.
pub fn spawn_periodic(
    service: Arc<WeatherService>,
    settings: Arc<SharedSettings>,
    interval: Duration,
) {
    tokio::spawn(async move {
        // A tiny initial delay lets the device dial its channel before the first push.
        tokio::time::sleep(Duration::from_secs(2)).await;
        loop {
            push_once(&service, &settings, settings.current_weather_provider()).await;
            tokio::time::sleep(interval).await;
        }
    });
}

/// One ambient-weather tick. Reads the **live** household location and units from the
/// settings (so a config-page edit of either applies on the next tick, matching the
/// spoken weather path), fetches current conditions from `provider`, and broadcasts
/// them to every connected weather channel. Returns the delivery count, or `None` when
/// the tick was skipped: no home location, weather disabled (`provider` is `None`), or
/// the fetch failed (the previous report stays on screen).
pub async fn push_once(
    service: &WeatherService,
    settings: &SharedSettings,
    provider: Option<Arc<dyn WeatherProvider>>,
) -> Option<usize> {
    let household = settings.household();
    let Some(provider) = provider else {
        log::debug!("weather push: weather disabled; skipping tick");
        return None;
    };
    let Some(location) = household.address.weather_location() else {
        log::debug!("weather push: no home location set; skipping tick");
        return None;
    };
    let imperial = crate::household::units_are_imperial(household.weather_units.as_deref());
    match provider
        .fetch(&location, imperial, crate::weather::ForecastWhen::Now)
        .await
    {
        Ok(report) => {
            let n = service.broadcast(&report);
            log::debug!(
                "weather push: {} in {} → {n} device(s)",
                report.current.temp,
                report.location_label
            );
            Some(n)
        }
        Err(e) => {
            log::warn!("weather push: fetch failed: {e:#}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::weather::{CurrentConditions, WeatherReport};
    use crate::wyoming::protocol::types;

    fn report() -> WeatherReport {
        WeatherReport {
            location_label: "Austin, Texas".into(),
            units: "imperial".into(),
            when_label: String::new(),
            current: CurrentConditions {
                temp: 72,
                description: "partly cloudy".into(),
                ..Default::default()
            },
            hourly: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    fn broadcasts_and_prunes_dead_channels() {
        let svc = WeatherService::new();
        assert_eq!(svc.connected(), 0);

        let (id_a, mut rx_a) = svc.register();
        let (_id_b, rx_b) = svc.register();
        assert_eq!(svc.connected(), 2);

        drop(rx_b);
        assert_eq!(svc.broadcast(&report()), 1);
        assert_eq!(svc.connected(), 1);

        let got = rx_a.try_recv().expect("live channel received the push");
        assert_eq!(got.event_type, types::WEATHER);
        assert_eq!(got.data["action"], serde_json::json!("current"));

        svc.deregister(id_a);
        assert_eq!(svc.connected(), 0);
    }

    #[test]
    fn a_new_channel_gets_the_last_report_immediately() {
        let svc = WeatherService::new();
        svc.broadcast(&report()); // caches, nobody connected yet
        let (_id, mut rx) = svc.register();
        let got = rx
            .try_recv()
            .expect("late joiner replayed the cached report");
        assert_eq!(got.event_type, types::WEATHER);
    }

    /// Records the `imperial` flag of every fetch.
    struct UnitsProbe(std::sync::Mutex<Vec<bool>>);

    #[async_trait::async_trait]
    impl WeatherProvider for UnitsProbe {
        async fn fetch(
            &self,
            _location: &str,
            imperial: bool,
            _when: crate::weather::ForecastWhen,
        ) -> anyhow::Result<WeatherReport> {
            self.0.lock().unwrap().push(imperial);
            Ok(report())
        }
    }

    fn household(units: &str) -> crate::settings::Household {
        crate::settings::Household {
            address: crate::household::HomeAddress {
                city: Some("Austin".into()),
                state: Some("Texas".into()),
                ..Default::default()
            },
            weather_units: Some(units.into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn each_tick_reads_the_live_household_units() {
        // Regression guard: the push used to capture `imperial` once at boot, so the
        // idle-screen weather disagreed with the spoken weather after a units change.
        let settings = crate::settings::SharedSettings::fixed(
            std::sync::Arc::new(crate::llm::mock::MockLlm::new("")),
            "mock",
            None,
        );
        let svc = WeatherService::new();
        let probe = Arc::new(UnitsProbe(Default::default()));

        settings.apply_household(&household("metric"));
        assert_eq!(
            push_once(&svc, &settings, Some(probe.clone())).await,
            Some(0)
        );
        settings.apply_household(&household("imperial"));
        assert_eq!(
            push_once(&svc, &settings, Some(probe.clone())).await,
            Some(0)
        );

        assert_eq!(*probe.0.lock().unwrap(), vec![false, true]);
    }

    #[tokio::test]
    async fn a_tick_without_a_home_location_or_provider_is_skipped() {
        let settings = crate::settings::SharedSettings::fixed(
            std::sync::Arc::new(crate::llm::mock::MockLlm::new("")),
            "mock",
            None,
        );
        let svc = WeatherService::new();
        let probe = Arc::new(UnitsProbe(Default::default()));
        assert_eq!(push_once(&svc, &settings, Some(probe.clone())).await, None);
        settings.apply_household(&household("metric"));
        assert_eq!(push_once(&svc, &settings, None).await, None);
        assert!(probe.0.lock().unwrap().is_empty());
    }
}
