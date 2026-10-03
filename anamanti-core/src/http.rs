//! Shared outbound HTTP client tuning.
//!
//! Every subsystem that hits an external API — System-1 (laya-serve/jev),
//! System-2 LLM backends (anthropic/openai/ollama + rig), the web-search tools,
//! and the lookup tools (weather/places/directions/calendar/recipe/cadora) —
//! should build its `reqwest::Client` through here so they all share the same
//! connection keep-alive policy.
//!
//! The goal is **handshake avoidance**: keep a pooled TLS/TCP connection warm
//! between turns so the next request to a provider skips the DNS + TCP + TLS
//! handshake (~hundreds of ms on a fresh HTTPS connection). reqwest pools idle
//! connections by default, but its default idle timeout (~90 s) is shorter than
//! the gap between turns on an ambient assistant, so idle connections get reaped
//! and the next turn cold-handshakes. We raise the idle timeout, add TCP
//! keepalive (so a NAT/firewall does not silently drop the pooled socket), and
//! turn on HTTP/2 keepalive pings (for ALPN-negotiated h2 providers). This is
//! the same pattern already proven in `memory::embed`.
//!
//! Note: we deliberately set **no global request timeout** here — a single
//! shared client serves both the 4 s System-1 budget and long-lived streaming
//! LLM turns, so timeouts are applied per-request at the call site via
//! `RequestBuilder::timeout(..)`.

use std::sync::OnceLock;
use std::time::Duration;

/// Connection-level keep-alive tuning shared by every outbound client.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);
const HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// A `reqwest::ClientBuilder` pre-populated with the shared keep-alive policy.
///
/// Use this when a component needs its own client-level settings on top of the
/// shared pooling — e.g. a per-component request timeout or `user_agent` — then
/// chain `.timeout(..)` / `.user_agent(..)` / `.build()` as usual.
pub fn tuned_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .http2_keep_alive_interval(HTTP2_KEEPALIVE_INTERVAL)
        .http2_keep_alive_while_idle(true)
}

/// A process-wide, keep-alive-tuned `reqwest::Client` with **no** global
/// timeout. Cheap to clone (reqwest wraps its internals in an `Arc`), so every
/// caller shares one connection pool. Callers that need a deadline apply it
/// per-request with `RequestBuilder::timeout(..)`; streaming LLM turns leave it
/// off so a long reply is never cut short.
pub fn shared_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            tuned_builder().build().unwrap_or_else(|e| {
                log::warn!("shared HTTP client build failed ({e}); using default");
                reqwest::Client::new()
            })
        })
        .clone()
}
