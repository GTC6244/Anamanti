//! Claude **subscription (OAuth)** auth for rig's Anthropic provider.
//!
//! rig's Anthropic client authenticates with a fixed `x-api-key` default header, but
//! a subscription token is a short-lived Bearer token that must be minted (and
//! refreshed) per request. rig lets the HTTP backend be swapped
//! ([`HttpClientExt`]), so [`SubscriptionHttp`] wraps rig's reqwest client and, on
//! every request, does exactly what the native backend
//! ([`crate::llm::anthropic::AnthropicBackend`]) does in subscription mode:
//!
//! - drops `x-api-key`;
//! - sends `Authorization: Bearer <token>` from the shared
//!   [`AnthropicTokenProvider`] (runtime override → `ANTHROPIC_OAUTH_TOKEN` → the
//!   token command, cached);
//! - adds `anthropic-beta: oauth-2025-04-20` (merged into any betas rig set);
//! - on a `401`, invalidates the cached token, mints a fresh one and retries once.

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use rig_core::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};
use rig_core::wasm_compat::WasmCompatSend;

use crate::llm::anthropic_auth::{AnthropicTokenProvider, OAUTH_BETA};

/// rig's reqwest client plus the subscription token source. `Default` (required by
/// rig's Anthropic model bound) has no token source, so every request fails with a
/// clear error instead of going out unauthenticated.
#[derive(Clone, Default)]
pub(crate) struct SubscriptionHttp {
    inner: http_client::ReqwestClient,
    token: Option<Arc<AnthropicTokenProvider>>,
}

impl std::fmt::Debug for SubscriptionHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubscriptionHttp").finish_non_exhaustive()
    }
}

impl SubscriptionHttp {
    pub(crate) fn new(
        inner: http_client::ReqwestClient,
        token: Arc<AnthropicTokenProvider>,
    ) -> Self {
        Self {
            inner,
            token: Some(token),
        }
    }

    async fn bearer(&self) -> http_client::Result<String> {
        let token = self.token.as_ref().ok_or_else(|| {
            http_client::Error::Instance(
                "subscription auth selected but no token provider is wired".into(),
            )
        })?;
        token
            .bearer()
            .await
            .map_err(|e| http_client::Error::Instance(e.into()))
    }

    fn invalidate(&self) {
        if let Some(token) = &self.token {
            token.invalidate();
        }
    }
}

/// The method, URI, version and headers of a request, kept so it can be rebuilt for
/// the 401 retry (the body is cheap `Bytes`).
struct Head {
    method: http_client::Method,
    uri: http_client::Uri,
    version: reqwest::Version,
    headers: http_client::HeaderMap,
}

impl Head {
    fn of<T>(req: Request<T>) -> (Self, T) {
        let (parts, body) = req.into_parts();
        (
            Self {
                method: parts.method,
                uri: parts.uri,
                version: parts.version,
                headers: parts.headers,
            },
            body,
        )
    }

    /// The request with subscription auth applied.
    fn authorized<B>(&self, body: B, bearer: &str) -> http_client::Result<Request<B>> {
        let mut headers = self.headers.clone();
        headers.remove("x-api-key");
        headers.insert(
            reqwest::header::AUTHORIZATION,
            http_client::HeaderValue::from_str(&format!("Bearer {bearer}"))?,
        );
        let betas = match headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
            Some(existing) if existing.split(',').any(|b| b.trim() == OAUTH_BETA) => {
                existing.to_string()
            }
            Some(existing) if !existing.trim().is_empty() => format!("{existing},{OAUTH_BETA}"),
            _ => OAUTH_BETA.to_string(),
        };
        headers.insert(
            "anthropic-beta",
            http_client::HeaderValue::from_str(&betas)?,
        );
        let mut builder = Request::builder()
            .method(self.method.clone())
            .uri(self.uri.clone())
            .version(self.version);
        if let Some(h) = builder.headers_mut() {
            *h = headers;
        }
        Ok(builder.body(body)?)
    }
}

/// Whether `err` is the provider rejecting the token.
fn is_unauthorized(err: &http_client::Error) -> bool {
    use http_client::Error::*;
    let status = match err {
        InvalidStatusCode(s) | InvalidStatusCodeWithMessage(s, _) => *s,
        InvalidStatusCodeWithDetails { status, .. } => *status,
        _ => return false,
    };
    status == reqwest::StatusCode::UNAUTHORIZED
}

impl HttpClientExt for SubscriptionHttp {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes>,
        T: WasmCompatSend,
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        let (head, body) = Head::of(req);
        let body: Bytes = body.into();
        let this = self.clone();
        async move {
            let bearer = this.bearer().await?;
            match this
                .inner
                .send(head.authorized(body.clone(), &bearer)?)
                .await
            {
                Err(e) if is_unauthorized(&e) => {
                    this.invalidate();
                    let bearer = this.bearer().await?;
                    this.inner.send(head.authorized(body, &bearer)?).await
                }
                other => other,
            }
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        // Not used by the Messages API; authorized once, no retry (a multipart body
        // cannot be replayed).
        let (head, body) = Head::of(req);
        let this = self.clone();
        async move {
            let bearer = this.bearer().await?;
            this.inner
                .send_multipart(head.authorized(body, &bearer)?)
                .await
        }
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let (head, body) = Head::of(req);
        let body: Bytes = body.into();
        let this = self.clone();
        async move {
            let bearer = this.bearer().await?;
            match this
                .inner
                .send_streaming(head.authorized(body.clone(), &bearer)?)
                .await
            {
                Err(e) if is_unauthorized(&e) => {
                    this.invalidate();
                    let bearer = this.bearer().await?;
                    this.inner
                        .send_streaming(head.authorized(body, &bearer)?)
                        .await
                }
                other => other,
            }
        }
    }
}
