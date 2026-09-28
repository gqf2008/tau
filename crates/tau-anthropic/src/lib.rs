//! Anthropic Messages API (`POST /v1/messages`) with SSE streaming.
//!
//! Configuration by environment: `ANTHROPIC_API_KEY` (sent as `x-api-key`),
//! or `ANTHROPIC_AUTH_TOKEN` (sent as `Authorization: Bearer`, the proxy
//! convention); `ANTHROPIC_BASE_URL` (default `https://api.anthropic.com`).
//!
//! ```no_run
//! use tau_anthropic::AnthropicModel;
//! use tau_core::{Message, Model, Request};
//!
//! # async fn demo() {
//! let model = AnthropicModel::from_env("claude-sonnet-4-5").expect("ANTHROPIC_API_KEY");
//! let request = Request {
//!     messages: vec![Message::user("hello")],
//!     ..Request::default()
//! };
//! let _stream = model.stream(&request).await;
//! # }
//! ```

mod wire;

use async_stream::stream;
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use tau_core::model::{Model, ModelEvent, Request, StopReason};
use tau_core::sse;

/// How the request authenticates: the official API key header, or a
/// bearer token (Claude Code style proxies).
enum Auth {
    ApiKey(String),
    Bearer(String),
}

/// The Anthropic Messages API as a tau [`Model`](tau_core::Model).
pub struct AnthropicModel {
    client: reqwest::Client,
    base_url: String,
    auth: Auth,
    model: String,
    /// Required by the API; bounds one response.
    max_tokens: u32,
}

impl AnthropicModel {
    /// Configure from `ANTHROPIC_API_KEY` (falling back to
    /// `ANTHROPIC_AUTH_TOKEN` for bearer auth) and optional
    /// `ANTHROPIC_BASE_URL`.
    pub fn from_env(model: impl Into<String>) -> Result<Self, std::env::VarError> {
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".into());
        match std::env::var("ANTHROPIC_API_KEY") {
            Ok(key) => Ok(Self::new(base_url, key, model)),
            Err(_) => {
                let token = std::env::var("ANTHROPIC_AUTH_TOKEN")?;
                Ok(Self::bearer(base_url, token, model))
            }
        }
    }

    /// API-key auth against `base_url` (trailing slashes trimmed).
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth: Auth::ApiKey(api_key.into()),
            model: model.into(),
            max_tokens: 8192,
        }
    }

    /// Authenticate with `Authorization: Bearer` instead of `x-api-key`.
    pub fn bearer(
        base_url: impl Into<String>,
        token: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let mut this = Self::new(base_url, "", model);
        this.auth = Auth::Bearer(token.into());
        this
    }

    /// Override the response cap (default 8192; required by the API).
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

#[async_trait]
impl Model for AnthropicModel {
    async fn stream(&self, req: &Request) -> BoxStream<'static, ModelEvent> {
        let body = wire::request_body(&self.model, self.max_tokens, req);
        let request = self.client.post(format!("{}/v1/messages", self.base_url));
        let request = match &self.auth {
            Auth::ApiKey(key) => request.header("x-api-key", key),
            Auth::Bearer(token) => request.bearer_auth(token),
        };
        let response = request
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await;

        let response = match response {
            Ok(r) => r,
            Err(e) => return error_stream(format!("request failed: {e}")),
        };
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return error_stream(format!("HTTP {status}: {}", &body[..body.len().min(500)]));
        }

        let byte_stream = response.bytes_stream().map_err(std::io::Error::other);
        let mut events = sse::parse(byte_stream);

        stream! {
            let mut closing = tau_core::sse::Closing::default();
            while let Some(chunk) = events.next().await {
                match chunk {
                    Ok(data) => {
                        for event in closing.observe(wire::chunk_events(&data)) {
                            yield event;
                        }
                    }
                    Err(e) => {
                        yield ModelEvent::Error { message: e };
                        yield ModelEvent::Done { stop: StopReason::Error };
                        return;
                    }
                }
            }
            // A `message_delta` naming the stop_reason is the provider's
            // word and survives the stream's end; `end_turn` vs `tool_use`
            // is exactly what the loop dispatches on (`tau_core::sse::Closing`).
            if let Some(event) = closing.fallback() {
                yield event;
            }
        }
        .boxed()
    }
}

fn error_stream(message: String) -> BoxStream<'static, ModelEvent> {
    futures::stream::iter([
        ModelEvent::Error { message },
        ModelEvent::Done {
            stop: StopReason::Error,
        },
    ])
    .boxed()
}
