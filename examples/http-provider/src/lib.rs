//! Example tau provider component exercising the consent-gated `http`
//! capability: treats the last user message as a URL, GETs it, and
//! streams back "STATUS <code>: <body>". Without a consented origin the
//! request fails at call time and the component reports an error event
//! (the Model contract: never trap).
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

use exports::tau::extension::models::{Guest, Info};
use tau::extension::events::{self, ModelEvent, StopReason};
use tau::extension::http;

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

    fn run(request_json: String) {
        // Test hook, mirroring the guard example's "crash": a trapped
        // provider must fail this run — and the host must rebuild the
        // instance so the NEXT run still reaches a working guest.
        if request_json.contains("crash") {
            panic!("the provider blew up");
        }
        match fetch(&request_json) {
            Ok(text) => {
                emit(ModelEvent::TextDelta(text));
                emit(ModelEvent::Done(StopReason::Stop));
            }
            Err(message) => {
                emit(ModelEvent::Error(message));
                emit(ModelEvent::Done(StopReason::Error));
            }
        }
    }
}

fn fetch(request_json: &str) -> Result<String, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(request_json).map_err(|e| format!("bad request json: {e}"))?;
    let url = parsed["messages"]
        .as_array()
        .and_then(|messages| {
            messages.iter().rev().find_map(|m| {
                (m["role"].as_str() == Some("user"))
                    .then(|| m["content"].as_array()?.first()?["text"].as_str())
                    .flatten()
            })
        })
        .ok_or("no user message found in request")?
        .trim()
        .to_string();

    // The host injects a consented bearer token as {"auth": {"bearer": …}};
    // forward it as the Authorization header.
    let headers: Vec<(String, String)> = match parsed["auth"]["bearer"].as_str() {
        Some(token) => vec![("authorization".into(), format!("Bearer {token}"))],
        None => vec![],
    };
    let authed = !headers.is_empty();

    let handle = http::request("GET", &url, &headers, &[])?;
    let status = http::status(handle)?;
    let mut body = String::new();
    loop {
        let (chunk, eof) = http::read_body(handle, 8192)?;
        body.push_str(&String::from_utf8_lossy(&chunk));
        if eof {
            break;
        }
    }
    http::close(handle);
    let marker = if authed { " [auth]" } else { "" };
    Ok(format!("STATUS {status}{marker}: {}", body.trim()))
}

/// Push one event; host-side rejections are loud on inherited stderr.
fn emit(event: ModelEvent) {
    if let Err(e) = events::emit(&event) {
        eprintln!("http-provider: host rejected event: {e}");
    }
}

export!(HttpProvider);
