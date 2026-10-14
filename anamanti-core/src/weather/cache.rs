//! The response-cache decorator every provider built by [`super::from_config`] is
//! wrapped in.

use std::sync::{Arc, OnceLock};

use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;

use super::{ForecastWhen, WeatherProvider, WeatherReport};

/// The logical tool name weather caches under — its key in the `tool_cache` TTL config
/// and in the shared [`ToolCache`](crate::cache::ToolCache) key space.
pub const WEATHER_TOOL: &str = "weather_lookup";

/// The process-wide weather response cache, shared across every provider built by
/// [`super::from_config`]. Lazily created on first use.
pub(super) fn weather_cache() -> Arc<crate::cache::ToolCache> {
    static CACHE: OnceLock<Arc<crate::cache::ToolCache>> = OnceLock::new();
    CACHE
        .get_or_init(|| Arc::new(crate::cache::ToolCache::new()))
        .clone()
}

/// A [`WeatherProvider`] decorator that serves a repeated `(location, imperial)` fetch
/// from a shared [`ToolCache`](crate::cache::ToolCache) within its configured TTL,
/// delegating to the wrapped provider on a miss and caching the result. The TTL comes
/// from `tool_cache.weather_lookup` (default 60 min); `0` turns caching off. Weather is a
/// pure lookup of its arguments for that window, so this is safe; a miss behaves exactly
/// like the inner provider (including its Open-Meteo fallback and error paths — errors
/// are never cached).
pub(super) struct CachingWeatherProvider {
    inner: Arc<dyn WeatherProvider>,
    cache: Arc<crate::cache::ToolCache>,
    /// Which backend `inner` is (`"visualcrossing"` / `"openmeteo"`), part of the cache
    /// key so switching providers never serves the other provider's cached report.
    backend: &'static str,
}

impl CachingWeatherProvider {
    pub(super) fn new(
        inner: Arc<dyn WeatherProvider>,
        cache: Arc<crate::cache::ToolCache>,
        backend: &'static str,
    ) -> Self {
        Self {
            inner,
            cache,
            backend,
        }
    }
}

#[async_trait]
impl WeatherProvider for CachingWeatherProvider {
    async fn fetch(
        &self,
        location: &str,
        imperial: bool,
        when: ForecastWhen,
    ) -> Result<WeatherReport> {
        // Per-tool TTL from the `tool_cache` config (0 ⇒ caching disabled for weather).
        let ttl = crate::cache::config().ttl(WEATHER_TOOL);
        if ttl.is_zero() {
            return self.inner.fetch(location, imperial, when).await;
        }
        // Key on the outgoing call data: the resolved place + units + which window. A
        // right-now request buckets by the current hour so a cached hourly slice never
        // shows a stale start-of-window; a future day buckets by its date.
        let when_key = match when {
            ForecastWhen::Now => format!("now:{}", Local::now().format("%Y-%m-%dT%H")),
            ForecastWhen::Day(d) => format!("day:{}", d.format("%Y-%m-%d")),
        };
        let key = crate::cache::ToolCache::key(
            WEATHER_TOOL,
            &(self.backend, location, imperial, when_key.as_str()),
        );
        if let Some(key) = &key {
            if let Some(hit) = self.cache.get_as::<WeatherReport>(key) {
                log::info!("weather cache hit for {location:?} (imperial={imperial}, {when_key})");
                return Ok(hit);
            }
        }
        let report = self.inner.fetch(location, imperial, when).await?;
        if let Some(key) = key {
            self.cache.put_as(key, &report, ttl);
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    /// A provider that counts how many times the upstream `fetch` actually ran, so the
    /// caching decorator can be verified without any network.
    struct CountingProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl WeatherProvider for CountingProvider {
        async fn fetch(
            &self,
            location: &str,
            imperial: bool,
            _when: ForecastWhen,
        ) -> Result<WeatherReport> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(WeatherReport {
                location_label: location.to_string(),
                units: if imperial { "imperial" } else { "metric" }.to_string(),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn caching_provider_serves_repeat_calls_without_refetching() {
        use std::sync::atomic::Ordering;
        let inner = Arc::new(CountingProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        // A dedicated cache (not the process-global one) so the test is isolated. The TTL
        // comes from the process cache config, which defaults `weather_lookup` to 60 min.
        let cache = Arc::new(crate::cache::ToolCache::new());
        let provider = CachingWeatherProvider::new(inner.clone(), cache, "test");

        // First call misses → upstream runs once.
        let a = provider
            .fetch("Austin", true, ForecastWhen::Now)
            .await
            .unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        // Identical call is served from cache → upstream NOT hit again.
        let b = provider
            .fetch("Austin", true, ForecastWhen::Now)
            .await
            .unwrap();
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            1,
            "second call must be cached"
        );
        assert_eq!(a, b);
        // Different units is a distinct key → upstream runs again.
        provider
            .fetch("Austin", false, ForecastWhen::Now)
            .await
            .unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
        // Different place → another upstream call.
        provider
            .fetch("Dallas", true, ForecastWhen::Now)
            .await
            .unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 3);
        // A future-day request is a distinct window → another upstream call, then cached.
        let day = ForecastWhen::Day(NaiveDate::from_ymd_opt(2999, 1, 2).unwrap());
        provider.fetch("Austin", true, day).await.unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 4);
        provider.fetch("Austin", true, day).await.unwrap();
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            4,
            "future day is cached too"
        );
    }
}
