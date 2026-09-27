//! Example tau provider component: no network, echoes the last user
//! message back word by word through the push channel (events.emit).
//! Keywords: "probe" reports the received request's length + checksum,
//! "env NAME" probes the WASI env policy, "audio" emits audio deltas.
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
use tau::extension::events::{self, AudioDelta, ModelEvent, StopReason};

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
            // "probe" reports what the guest actually received: the byte
            // length and a FNV-1a checksum of the whole request JSON.
            // The host-side test recomputes both over the exact string it
            // sent, so truncation or corruption at the component boundary
            // (multi-MiB media inflates the JSON far past the usual few
            // KiB) shows up as a mismatch.
            Some(text) if text == "probe" => {
                emit(ModelEvent::TextDelta(format!(
                    "probe bytes={} fnv1a={:016x}",
                    request_json.len(),
                    fnv1a(request_json.as_bytes()),
                )));
                emit(ModelEvent::Done(StopReason::Stop));
            }
            // "env NAME" reports whether the ambient env var is visible —
            // demos the host's WASI policy (allow-all inherits, deny-all
            // sees nothing).
            Some(text) if text.starts_with("env ") => {
                let name = text.trim_start_matches("env ").trim();
                let value = std::env::var(name).unwrap_or_default();
                emit(ModelEvent::TextDelta(format!("{name}={value}")));
                emit(ModelEvent::Done(StopReason::Stop));
            }
            // "audio …" demos the realtime-style channel: two audio
            // chunks then a text note.
            Some(text) if text.starts_with("audio") => {
                // Typed in 0.2.0: raw bytes cross the ABI, no base64.
                for data in [vec![1u8, 2, 3], vec![4u8, 5]] {
                    emit(ModelEvent::AudioDelta(AudioDelta {
                        data,
                        media_type: "audio/pcm;rate=24000".into(),
                    }));
                }
                emit(ModelEvent::TextDelta("(two audio chunks emitted)".into()));
                emit(ModelEvent::Done(StopReason::Stop));
            }
            Some(text) => {
                for word in text.split_inclusive(' ') {
                    emit(ModelEvent::TextDelta(word.to_string()));
                }
                emit(ModelEvent::Done(StopReason::Stop));
            }
            None => {
                emit(ModelEvent::Error(
                    "no user message found in request".into(),
                ));
                emit(ModelEvent::Done(StopReason::Error));
            }
        }
    }
}

/// Push one event. `emit` returns a result in 0.2.0: a host-side
/// rejection is loud on the guest's inherited stderr instead of
/// vanishing into a silent skip.
fn emit(event: ModelEvent) {
    if let Err(e) = events::emit(&event) {
        eprintln!("echo-provider: host rejected event: {e}");
    }
}

/// FNV-1a 64-bit — dependency-free checksum for the "probe" keyword.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

export!(Echo);
