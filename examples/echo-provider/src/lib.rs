//! Example tau provider component: no network, echoes the last user
//! message back word by word through the push channel (events.emit).
//!
//! Build:
//!   cargo build --manifest-path examples/echo-provider/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use:
//!   tau --provider-wasm examples/echo-provider/target/wasm32-wasip2/release/echo_provider.wasm \
//!       --model echo -p "hello push mode"

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "provider",
});

use exports::tau::extension::models::{Guest, Info};

struct Echo;

impl Guest for Echo {
    fn list_models() -> Vec<Info> {
        vec![Info {
            id: "echo".into(),
            name: "Echo (demo provider)".into(),
            context_window: 8192,
            max_output: 1024,
        }]
    }

    fn run(request_json: String) {
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&request_json);
        let last_user_text = parsed
            .ok()
            .and_then(|req| {
                req["messages"].as_array().and_then(|messages| {
                    messages.iter().rev().find_map(|m| {
                        (m["role"].as_str() == Some("user")).then(|| {
                            m["content"]
                                .as_array()
                                .and_then(|c| c.first())
                                .and_then(|b| b["text"].as_str().map(str::to_string))
                        })
                    })
                })
            })
            .flatten();

        match last_user_text {
            // "audio …" demos the realtime-style channel: two audio
            // chunks then a text note.
            Some(text) if text.starts_with("audio") => {
                for data in ["AQID", "BAU="] {
                    emit(&serde_json::json!({
                        "kind": "audio-delta",
                        "data": data,
                        "media_type": "audio/pcm;rate=24000",
                    }));
                }
                emit(&serde_json::json!({
                    "kind": "text-delta",
                    "text": "(two audio chunks emitted)",
                }));
                emit(&serde_json::json!({ "kind": "done", "stop": "stop" }));
            }
            Some(text) => {
                for word in text.split_inclusive(' ') {
                    emit(&serde_json::json!({
                        "kind": "text-delta",
                        "text": word,
                    }));
                }
                emit(&serde_json::json!({ "kind": "done", "stop": "stop" }));
            }
            None => {
                emit(&serde_json::json!({
                    "kind": "error",
                    "message": "no user message found in request",
                }));
                emit(&serde_json::json!({ "kind": "done", "stop": "error" }));
            }
        }
    }
}

fn emit(event: &serde_json::Value) {
    tau::extension::events::emit(&event.to_string());
}

export!(Echo);
