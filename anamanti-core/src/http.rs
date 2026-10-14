//! Shared outbound HTTP client tuning, plus the request helpers every integration uses.
//!
//! Every subsystem that hits an external API — System-1 (laya-serve/jev),
//! System-2 LLM backends (anthropic/openai/ollama + rig), the web-search tools,
//! and the lookup tools (weather/places/directions/calendar/recipe/cadora/appsaid/
//! spotify) — should use [`shared_client`] so they all share **one connection pool**
//! with the same keep-alive policy.
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
//! Note: we deliberately set **no global request timeout** on the shared client — a
//! single client serves both the 4 s System-1 budget and long-lived streaming LLM
//! turns. Integrations go through [`send`] / [`send_ok`] / [`send_json`] /
//! [`send_text`], which make a per-request timeout **mandatory**, and start their
//! requests with [`get`] / [`post`] / [`request`], which set the per-component
//! `User-Agent` header.
//!
//! **Secrets never reach error text.** A `reqwest::Error` carries the request URL,
//! and some providers only accept their key as a query parameter (Visual Crossing
//! `key=`, Mapbox `access_token=`). Tool errors are formatted into LLM history and
//! logs, so every helper strips the URL from reqwest errors
//! ([`reqwest::Error::without_url`]) and additionally runs [`redact_secrets`] over the
//! final message as a defensive second layer.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use reqwest::{IntoUrl, Method, RequestBuilder, Response};
use serde::de::DeserializeOwned;

/// Pooled-connection idle lifetime shared by every outbound client.
pub(crate) const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// TCP keepalive probe interval shared by every outbound client.
pub(crate) const TCP_KEEPALIVE: Duration = Duration::from_secs(60);
/// HTTP/2 keepalive ping interval shared by every outbound client.
pub(crate) const HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// The product token every integration's `User-Agent` starts with.
pub const USER_AGENT_PRODUCT: &str = "anamanti-core/0.1";

/// A `reqwest::ClientBuilder` pre-populated with the shared keep-alive policy.
///
/// Prefer [`shared_client`]: a separately built client has its own pool, so its
/// keep-alive never benefits anyone else. Use this only where a component genuinely
/// needs client-level settings the shared client can't take per request.
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
/// per-request (the `send*` helpers here require one); streaming LLM turns leave it
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

/// The `User-Agent` value for one integration, e.g. `anamanti-core/0.1 (weather)`.
pub fn user_agent(component: &str) -> String {
    format!("{USER_AGENT_PRODUCT} ({component})")
}

/// Start a `method` request to `url` on the [`shared_client`], tagged with the
/// `component`'s `User-Agent`. Finish it with one of the `send*` helpers.
pub fn request(method: Method, url: impl IntoUrl, component: &str) -> RequestBuilder {
    shared_client()
        .request(method, url)
        .header(reqwest::header::USER_AGENT, user_agent(component))
}

/// [`request`] with `GET`.
pub fn get(url: impl IntoUrl, component: &str) -> RequestBuilder {
    request(Method::GET, url, component)
}

/// [`request`] with `POST`.
pub fn post(url: impl IntoUrl, component: &str) -> RequestBuilder {
    request(Method::POST, url, component)
}

/// Send `req` with a mandatory `timeout`. Any status is returned as-is (use
/// [`send_ok`] to reject non-2xx). A transport failure (connect refused, timeout, TLS)
/// becomes `"{what}: request failed: …"` with the URL and any secrets removed.
pub async fn send(req: RequestBuilder, timeout: Duration, what: &str) -> Result<Response> {
    req.timeout(timeout)
        .send()
        .await
        .map_err(|e| redacted(format!("{what}: request failed"), e))
}

/// [`send`], then reject a non-2xx status as `"{what} returned an error status (…)"`
/// (URL and secrets removed).
pub async fn send_ok(req: RequestBuilder, timeout: Duration, what: &str) -> Result<Response> {
    send(req, timeout, what)
        .await?
        .error_for_status()
        .map_err(|e| redacted(format!("{what} returned an error status"), e))
}

/// [`send_ok`], then decode the body as JSON into `T` — a typed response struct, or
/// [`serde_json::Value`] for free-form bodies. A decode failure becomes
/// `"parsing the {what} response: …"`.
pub async fn send_json<T: DeserializeOwned>(
    req: RequestBuilder,
    timeout: Duration,
    what: &str,
) -> Result<T> {
    send_ok(req, timeout, what)
        .await?
        .json::<T>()
        .await
        .map_err(|e| redacted(format!("parsing the {what} response"), e))
}

/// [`send_ok`], then read the body as text.
pub async fn send_text(req: RequestBuilder, timeout: Duration, what: &str) -> Result<String> {
    send_ok(req, timeout, what)
        .await?
        .text()
        .await
        .map_err(|e| redacted(format!("reading the {what} response"), e))
}

/// Turn a reqwest error into an `anyhow` error whose text carries no URL and no
/// secret: the URL is stripped at the source, the full cause chain is flattened into
/// one message, and [`redact_secrets`] runs over it. The result has no `source()`
/// chain, so neither `{e}`, `{e:#}` nor `{e:?}` can reintroduce the URL.
pub fn redacted(context: String, err: reqwest::Error) -> anyhow::Error {
    let chain = format!("{:#}", anyhow::Error::new(err.without_url()));
    anyhow::anyhow!(redact_secrets(&format!("{context}: {chain}")))
}

/// Query-parameter names whose values are secrets. Matched case-insensitively at a
/// parameter boundary (so `access_token=` is not double-matched as `token=`).
const SECRET_PARAMS: [&str; 4] = ["access_token=", "api_key=", "token=", "key="];

/// Replace the value of every `key=` / `access_token=` / `token=` / `api_key=` query
/// parameter in `text` with `REDACTED`. A defensive second layer behind
/// [`reqwest::Error::without_url`] for any text that might embed a request URL.
pub fn redact_secrets(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'scan: while i < text.len() {
        let at_boundary = i == 0 || {
            let prev = bytes[i - 1];
            !(prev.is_ascii_alphanumeric() || prev == b'_')
        };
        if at_boundary {
            for name in SECRET_PARAMS {
                let matches = text
                    .get(i..i + name.len())
                    .is_some_and(|s| s.eq_ignore_ascii_case(name));
                if matches {
                    out.push_str(&text[i..i + name.len()]);
                    out.push_str("REDACTED");
                    i += name.len();
                    while i < bytes.len() && !is_value_end(bytes[i]) {
                        i += 1;
                    }
                    continue 'scan;
                }
            }
        }
        // Copy one (possibly multi-byte) char.
        let ch = text[i..].chars().next().expect("i is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Whether `b` ends a query-parameter value inside free text.
fn is_value_end(b: u8) -> bool {
    b.is_ascii_whitespace()
        || matches!(
            b,
            b'&' | b')' | b'(' | b'"' | b'\'' | b'#' | b',' | b';' | b'>' | b'<'
        )
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Tiny loopback HTTP servers for integration tests (no real network).

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve one raw response per inbound connection, in order, then stop. Each entry is
    /// `(status line, body)`, e.g. `("401 Unauthorized", "{}")`.
    pub fn serve_statuses(
        responses: Vec<(&'static str, String)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let listener = TcpListener::from_std(std_listener).unwrap();
            for (status, body) in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{addr}"), handle)
    }

    /// Serve a `200 OK` JSON body per inbound connection, in order.
    pub fn serve_sequence(bodies: Vec<String>) -> (String, tokio::task::JoinHandle<()>) {
        serve_statuses(bodies.into_iter().map(|b| ("200 OK", b)).collect())
    }

    /// A loopback base URL nothing listens on, so a request is refused at connect time.
    pub fn refused_base() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        format!("http://{addr}")
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn redacts_secret_query_values() {
        let s = "error sending request for url (https://x.test/a?q=b&key=SECRET1&access_token=SECRET2&api_key=S3&token=S4)";
        let r = redact_secrets(s);
        for secret in ["SECRET1", "SECRET2", "S3", "S4"] {
            assert!(!r.contains(secret), "{r}");
        }
        assert!(r.contains("q=b"), "non-secret params survive: {r}");
        assert!(r.contains("key=REDACTED"), "{r}");
        assert!(r.contains("access_token=REDACTED"), "{r}");
        // A word merely ending in "key" is not a parameter boundary.
        assert_eq!(redact_secrets("monkey=1"), "monkey=1");
        // Case-insensitive and multi-byte safe.
        assert_eq!(redact_secrets("KEY=abc — é"), "KEY=REDACTED — é");
    }

    #[test]
    fn user_agent_names_the_component() {
        assert_eq!(user_agent("weather"), "anamanti-core/0.1 (weather)");
    }

    #[tokio::test]
    async fn status_error_text_has_no_url_or_secret() {
        let (base, server) = serve_statuses(vec![("401 Unauthorized", "{}".into())]);
        let err = send_json::<serde_json::Value>(
            get(format!("{base}/x?key=TOPSECRET"), "test"),
            Duration::from_secs(5),
            "Test API",
        )
        .await
        .unwrap_err();
        for text in [format!("{err}"), format!("{err:#}"), format!("{err:?}")] {
            assert!(!text.contains("TOPSECRET"), "{text}");
            assert!(!text.contains(&base), "{text}");
        }
        assert!(format!("{err}").contains("401"), "{err}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn transport_error_text_has_no_url_or_secret() {
        let base = refused_base();
        let err = send(
            get(format!("{base}/x?access_token=TOPSECRET"), "test"),
            Duration::from_secs(5),
            "Test API",
        )
        .await
        .unwrap_err();
        let text = format!("{err:#} {err:?}");
        assert!(!text.contains("TOPSECRET"), "{text}");
        assert!(text.starts_with("Test API: request failed"), "{text}");
    }
}
