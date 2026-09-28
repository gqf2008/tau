//! OpenAI-compatible chat completions with SSE streaming.
//!
//! Works against any endpoint that speaks the chat-completions wire format:
//! OpenAI, OpenRouter, and most proxies. Configuration is by environment:
//! `OPENAI_API_KEY` (required), `OPENAI_BASE_URL` (default
//! `https://api.openai.com/v1`).
//!
//! ```no_run
//! use tau_core::{Message, Model, ModelEvent, Request};
//! use tau_openai::OpenAiModel;
//! # use futures::StreamExt;
//!
//! # async fn demo() {
//! let model = OpenAiModel::from_env("gpt-4o-mini").expect("OPENAI_API_KEY");
//! let request = Request {
//!     messages: vec![Message::user("hello")],
//!     ..Request::default()
//! };
//! let mut stream = model.stream(&request).await;
//! while let Some(event) = stream.next().await {
//!     if let ModelEvent::TextDelta { text } = event {
//!         print!("{text}");
//!     }
//! }
//! # }
//! ```

mod responses;
mod wire;

use async_stream::stream;
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use tau_core::model::{Model, ModelEvent, Request, StopReason};
use tau_core::sse;

/// Which OpenAI API surface to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    /// `/chat/completions` — the older, most widely proxied format.
    ChatCompletions,
    /// `/responses` — the current OpenAI format.
    Responses,
}

/// An OpenAI-compatible API as a tau [`Model`](tau_core::Model) —
/// works with any provider speaking one of the [`Api`] wire formats.
pub struct OpenAiModel {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    api: Api,
}

impl OpenAiModel {
    /// Configure from `OPENAI_API_KEY` and optional `OPENAI_BASE_URL`,
    /// using the chat-completions wire format.
    pub fn from_env(model: impl Into<String>) -> Result<Self, std::env::VarError> {
        Self::from_env_with(model, Api::ChatCompletions)
    }

    /// [`from_env`](Self::from_env) with an explicit wire format.
    pub fn from_env_with(model: impl Into<String>, api: Api) -> Result<Self, std::env::VarError> {
        let api_key = std::env::var("OPENAI_API_KEY")?;
        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
        Ok(Self::new(base_url, api_key, model).api(api))
    }

    /// Bearer-key auth against `base_url` (trailing slashes trimmed).
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            api: Api::ChatCompletions,
        }
    }

    /// Select the wire format (default [`Api::ChatCompletions`]).
    pub fn api(mut self, api: Api) -> Self {
        self.api = api;
        self
    }
}

#[async_trait]
impl Model for OpenAiModel {
    async fn stream(&self, req: &Request) -> BoxStream<'static, ModelEvent> {
        let (body, path) = match self.api {
            Api::ChatCompletions => (wire::request_body(&self.model, req), "/chat/completions"),
            Api::Responses => (responses::request_body(&self.model, req), responses::PATH),
        };
        let response = self
            .client
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.api_key)
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
            return error_stream(format!("HTTP {status}: {}", truncate(&body, 500)));
        }

        let byte_stream = response.bytes_stream().map_err(std::io::Error::other);
        let mut events = sse::parse(byte_stream);
        let api = self.api;

        stream! {
            let mut mapper = responses::ChunkMapper::new();
            let mut closing = sse::Closing::default();
            while let Some(chunk) = events.next().await {
                match chunk {
                    Ok(data) => {
                        let produced = match api {
                            Api::ChatCompletions => wire::chunk_events(&data),
                            Api::Responses => mapper.events(&data),
                        };
                        for event in closing.observe(produced) {
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
            // The stream ended without an error. A provider that named its
            // stop keeps it — `finish_reason: "tool_calls"` must reach the
            // loop as `ToolUse` — so only a stream that said nothing gets
            // the fallback (`sse::Closing`).
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

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
