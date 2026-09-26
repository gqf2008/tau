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
        match fetch(&request_json) {
            Ok(text) => {
                emit(&serde_json::json!({ "kind": "text-delta", "text": text }));
                emit(&serde_json::json!({ "kind": "done", "stop": "stop" }));
            }
            Err(message) => {
                emit(&serde_json::json!({ "kind": "error", "message": message }));
                emit(&serde_json::json!({ "kind": "done", "stop": "error" }));
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

    let handle = http::request("GET", &url, &[], &[])?;
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
    Ok(format!("STATUS {status}: {}", body.trim()))
}

fn emit(event: &serde_json::Value) {
    tau::extension::events::emit(&event.to_string());
}

export!(HttpProvider);
