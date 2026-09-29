//! Example tau provider component: no network, echoes the last user
//! message back word by word through the request's event stream.
//! Keywords: "probe" reports the media bytes it received (length +
//! checksum), "env NAME" probes the WASI env policy, "audio" emits audio
//! deltas.
//!
//! 0.7.0 turned the push channel into a pull stream: `run` returns
//! `(stream<event>, future<result<(), error>>)` instead of the component
//! calling `events.emit`. The guest keeps the writer, hands the host the
//! reader, and the write awaits when the host is slow — backpressure
//! instead of a dropped-buffer compromise. When the host hangs up early
//! (a cancellation), the next write returns the unwritten value.
//!
//! Build:
//!   cargo build --manifest-path examples/echo-provider/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use:
//!   tau --provider-wasm examples/echo-provider/target/wasm32-wasip2/release/echo_provider.wasm \
//!       --model echo -p "hello pull mode"

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "provider",
});

use exports::tau::extension::models::{AudioDelta, Event, Guest, Info, Request};
use tau::extension::types::{Content, Error, MediaSource, Role, StopReason};
use wit_bindgen::rt::async_support::{FutureReader, StreamReader, spawn_local};

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

    /// The typed request replaces 0.6.0's JSON parse: the last user
    /// message's first text block is the prompt.
    async fn run(request: Request) -> (StreamReader<Event>, FutureReader<Result<(), Error>>) {
        let (mut events, events_rx) = wit_stream::new::<Event>();
        let (verdict, verdict_rx) = wit_future::new::<Result<(), Error>>(|| Ok(()));
        spawn_local(async move {
            // The writer must live under an async export for `spawn_local`
            // to be scheduled at all (docs/wit-redesign.md §5, leg 1b/1d):
            // `run` is `async func`, so this task is driven.
            let mut hung_up = None;
            for event in respond(&request) {
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

/// The whole response as an ordered event list. Small by construction:
/// this provider has nothing to wait for.
fn respond(request: &Request) -> Vec<Event> {
    let Some(text) = last_user_text(request) else {
        return vec![
            Event::Error("no user message found in request".into()),
            Event::Done(StopReason::Error),
        ];
    };

    // "probe" reports what the guest actually received: the byte length
    // and FNV-1a checksum of every inline media byte, in order. The
    // host-side test recomputes both over the very bytes it sent, so
    // truncation or corruption at the component boundary (multi-MiB
    // media in one message) shows up as a mismatch.
    if text == "probe" {
        let bytes = media_bytes(request);
        return vec![
            Event::TextDelta(format!("probe bytes={} fnv1a={:016x}", bytes.len(), fnv1a(&bytes))),
            Event::Done(StopReason::Stop),
        ];
    }

    // "env NAME" reports whether the ambient env var is visible — demos
    // the host's WASI policy (allow-all inherits, deny-all sees nothing).
    if let Some(name) = text.strip_prefix("env ") {
        let name = name.trim();
        let value = std::env::var(name).unwrap_or_default();
        return vec![
            Event::TextDelta(format!("{name}={value}")),
            Event::Done(StopReason::Stop),
        ];
    }

    // "audio …" demos the realtime-style channel: two audio chunks then a
    // text note.
    if text.starts_with("audio") {
        let mut events = Vec::new();
        for data in [vec![1u8, 2, 3], vec![4u8, 5]] {
            events.push(Event::AudioDelta(AudioDelta {
                data,
                media_type: "audio/pcm;rate=24000".into(),
            }));
        }
        events.push(Event::TextDelta("(two audio chunks emitted)".into()));
        events.push(Event::Done(StopReason::Stop));
        return events;
    }

    let mut events: Vec<Event> = text
        .split_inclusive(' ')
        .map(|word| Event::TextDelta(word.to_string()))
        .collect();
    events.push(Event::Done(StopReason::Stop));
    events
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

/// Every inline media byte in the request, in message/block order.
/// Blob and URL references are not bytes and contribute nothing.
fn media_bytes(request: &Request) -> Vec<u8> {
    let mut out = Vec::new();
    for message in &request.messages {
        for block in &message.content {
            let media = match block {
                Content::Image(media) | Content::Audio(media) | Content::Video(media) => media,
                Content::File(file) => &file.media,
                _ => continue,
            };
            if let MediaSource::Bytes(bytes) = &media.source {
                out.extend_from_slice(bytes);
            }
        }
    }
    out
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
