//! OpenAI-compatible chat completions with SSE streaming.
//!
//! Works against any endpoint that speaks the chat-completions wire format:
//! OpenAI, OpenRouter, and most proxies. Configuration is by environment:
//! `OPENAI_API_KEY` (required), `OPENAI_BASE_URL` (default
//! `https://api.openai.com/v1`).

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

pub struct OpenAiModel {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    api: Api,
}

impl OpenAiModel {
    pub fn from_env(model: impl Into<String>) -> Result<Self, std::env::VarError> {
        Self::from_env_with(model, Api::ChatCompletions)
    }

    pub fn from_env_with(model: impl Into<String>, api: Api) -> Result<Self, std::env::VarError> {
        let api_key = std::env::var("OPENAI_API_KEY")?;
        let base_url = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());
        Ok(Self::new(base_url, api_key, model).api(api))
    }

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

        let byte_stream = response
            .bytes_stream()
            .map_err(std::io::Error::other);
        let mut events = sse::parse(byte_stream);
        let api = self.api;

        stream! {
            let mut mapper = responses::ChunkMapper::new();
            while let Some(chunk) = events.next().await {
                match chunk {
                    Ok(data) => {
                        let produced = match api {
                            Api::ChatCompletions => wire::chunk_events(&data),
                            Api::Responses => mapper.events(&data),
                        };
                        for event in produced {
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
            // Stream ended without an explicit error; if no finish_reason was
            // seen, chunk_events emitted nothing for it — close out cleanly.
            yield ModelEvent::Done { stop: StopReason::Stop };
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
