//! Example tau provider component exercising the consent-gated `http`
//! capability: treats the last user message as a URL, GETs it, and
//! streams back "STATUS <code>: <body>". Without a consented origin the
//! request fails at call time and the component reports an error event
//! (the Model contract: never trap).
//!
//! 0.7.0 moved every wait out of the guest: `request` is an async import
//! awaited under the export, and the body is a stream the guest pulls
//! until the host ends it. The 0.6.0 `idle=<ms>` URL token went with the
//! guest-supplied `timeout-ms` parameter it fed — waiting is host policy
//! now. The test knobs are `TAU_HTTP_REQUEST_TIMEOUT_MS` (headers) and
//! `TAU_HTTP_IDLE_TIMEOUT_MS` (between body bytes); production values are
//! the host's own `REQUEST_TIMEOUT` / `IDLE_TIMEOUT`.
//!
//! Build:
//!   cargo build --manifest-path examples/http-provider/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use:
//!   tau --provider-wasm .../http_provider.wasm --model http \
//!       --provider-origin http://127.0.0.1:8080 -p "http://127.0.0.1:8080/"

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "provider",
});

use exports::tau::extension::models::{Auth, Event, Guest, Info, Request};
use tau::extension::http;
use tau::extension::types::{Content, Error, Role, StopReason};
use wit_bindgen::rt::async_support::{FutureReader, StreamReader, spawn_local};

struct HttpProvider;

impl Guest for HttpProvider {
    fn list_models() -> Vec<Info> {
        vec![Info {
            id: "http".into(),
            name: "HTTP fetch (demo provider)".into(),
            context_window: 8192,
            max_output: 4096,
        }]
    }

    async fn run(request: Request) -> (StreamReader<Event>, FutureReader<Result<(), Error>>) {
        // Test hook, mirroring the guard example's "crash": a trapped
        // provider must fail this run — and the host must rebuild the
        // instance so the NEXT run still reaches a working guest. Checked
        // before anything is spawned, so the trap lands on the call itself
        // exactly as the old synchronous `run` did.
        if last_user_text(&request).is_some_and(|text| text.contains("crash")) {
            panic!("the provider blew up");
        }
        let (mut events, events_rx) = wit_stream::new::<Event>();
        let (verdict, verdict_rx) = wit_future::new::<Result<(), Error>>(|| Ok(()));
        spawn_local(async move {
            // The writer must live under an async export for `spawn_local`
            // to be scheduled at all (docs/wit-redesign.md §5): `run` is
            // `async func`, so this task is driven.
            let mut hung_up = None;
            for event in fetch(&request).await {
                if events.write_one(event).await.is_some() {
                    // The host dropped the read end: a refusal or a
                    // cancellation. Nothing is left to deliver.
                    hung_up = Some(Error::Failed(
                        "the host stopped reading the event stream".into(),
                    ));
                    break;
                }
            }
            // Dropping the writer ends the stream — the normal terminator
            // after `done`/`error`, and harmless after a hang-up.
            drop(events);
            let value = match hung_up {
                Some(error) => Err(error),
                None => Ok(()),
            };
            let _ = verdict.write(value).await;
        });
        (events_rx, verdict_rx)
    }
}

/// The whole answer as an ordered event list: the fetch's text plus the
/// terminal event, or the error pair (never a trap).
async fn fetch(request: &Request) -> Vec<Event> {
    match get(request).await {
        Ok(text) => vec![Event::TextDelta(text), Event::Done(StopReason::Stop)],
        Err(message) => vec![Event::Error(message), Event::Done(StopReason::Error)],
    }
}

/// GET the URL in the last user message and return "STATUS <code>: <body>".
async fn get(request: &Request) -> Result<String, String> {
    let url = last_user_text(request)
        .ok_or("no user message found in request")?
        .trim()
        .to_string();

    // The host injects a consented bearer token as `auth`; forward it as
    // the Authorization header.
    let headers: Vec<(String, String)> = match &request.auth {
        Some(Auth::Bearer(token)) => vec![("authorization".into(), format!("Bearer {token}"))],
        None => Vec::new(),
    };
    let marker = if headers.is_empty() { "" } else { " [auth]" };

    let response = http::request("GET".into(), url, headers, Vec::new())
        .await
        .map_err(describe)?;
    let status = response.status();
    // Dropping the read end abandons the rest of the body and closes the
    // connection; `collect` reads to the end of it, which for this
    // example's finite bodies is the same thing.
    let body = response.body().collect().await;
    Ok(format!(
        "STATUS {status}{marker}: {}",
        String::from_utf8_lossy(&body).trim()
    ))
}

/// The contract's three-way error, spelled the way the host spells it.
fn describe(error: http::Error) -> String {
    match error {
        http::Error::Refused(detail) => format!("refused: {detail}"),
        http::Error::Failed(detail) => format!("failed: {detail}"),
        http::Error::Invalid(detail) => format!("invalid: {detail}"),
    }
}

/// The last user message's first text block.
fn last_user_text(request: &Request) -> Option<String> {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .and_then(|message| {
            message.content.iter().find_map(|block| match block {
                Content::Text(text) => Some(text.clone()),
                _ => None,
            })
        })
}

export!(HttpProvider);
