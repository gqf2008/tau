//! The full-duplex live loop (docs/realtime-av.md, Phase 2a): one
//! driver task per `/live` session — capture pushes uplink chunks into
//! the `RealtimeSession`, session events translate onto the agent bus
//! (the renderers and the Phase 1 sink consume them unchanged), and
//! the accumulated outcome lands back in the REPL for session-tree
//! recording.
//!
//! Timing is not invented here (red line 3): the REPL wraps the span
//! in a synthetic `RunStart`/`RunEnd` pair on the bus, so the sink's
//! counters and summaries reuse the Phase 1 arms verbatim.

use std::time::Duration;

use futures::StreamExt;
use tau_core::bus::EventBus;
use tau_core::model::{ModelEvent, RealtimeSession};
use tau_core::types::{Content, Media, MediaSource};
use tau_core::{AgentEvent, StopReason};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// REPL → driver commands. Ctrl-C maps to barge-in (`Interrupt`), an
/// orderly session end to `Close`.
pub enum LiveCmd {
    Interrupt,
    Close,
}

/// What one live session produced, for session-tree recording.
pub struct LiveOutcome {
    /// Every uplink byte pushed (the host records its own pushes —
    /// real providers do not echo InputAudioChunk).
    pub uplink: Vec<u8>,
    /// The uplink format (the session's input_media_type).
    pub uplink_media_type: String,
    /// Assembled assistant content (text + audio segments; an
    /// Interrupted splits segments even at the same media type).
    pub assistant: Vec<Content>,
    /// How many barge-ins happened.
    pub interruptions: u64,
}

/// Drive one live session to its end (time elapsed or `Close`),
/// forwarding events to the bus, then deliver the outcome.
pub async fn run(
    mut session: Box<dyn RealtimeSession>,
    seconds: u32,
    sine: bool,
    input_media_type: String,
    bus: EventBus,
    mut cmd: UnboundedReceiver<LiveCmd>,
    done: UnboundedSender<LiveOutcome>,
) {
    let mut events = session.events();
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(capture(seconds, sine, chunk_tx));

    let mut uplink: Vec<u8> = Vec::new();
    let mut text = String::new();
    let mut audio: Vec<(String, Vec<u8>)> = Vec::new();
    // Barge-in boundary, same rule as the agent loop: an Interrupted
    // freezes the segment; the next delta opens a new one.
    let mut frozen = false;
    let mut interruptions = 0u64;
    let mut closing = false;

    loop {
        tokio::select! {
            chunk = chunk_rx.recv(), if !closing => match chunk {
                Some(bytes) => {
                    uplink.extend_from_slice(&bytes);
                    if session.push_audio(bytes).await.is_err() {
                        closing = true; // the door refused; drain events
                        let _ = session.close().await;
                    }
                }
                None => {
                    // Capture ended — the seconds elapsed.
                    closing = true;
                    let _ = session.close().await;
                }
            },
            command = cmd.recv(), if !closing => match command {
                Some(LiveCmd::Interrupt) => {
                    interruptions += 1;
                    let _ = session.interrupt().await;
                }
                Some(LiveCmd::Close) | None => {
                    closing = true;
                    let _ = session.close().await;
                }
            },
            event = events.next() => match event {
                Some(ModelEvent::TextDelta { text: delta }) => {
                    text.push_str(&delta);
                    let _ = bus.send(AgentEvent::TextDelta(delta));
                }
                Some(ModelEvent::AudioDelta { data, media_type }) => {
                    let _ = bus.send(AgentEvent::AudioDelta {
                        data: data.clone(),
                        media_type: media_type.clone(),
                    });
                    match audio.last_mut() {
                        Some((ty, bytes)) if *ty == media_type && !frozen => {
                            bytes.extend_from_slice(&data)
                        }
                        _ => audio.push((media_type, data)),
                    }
                    frozen = false;
                }
                Some(ModelEvent::InputAudioChunk { data, media_type }) => {
                    let _ = bus.send(AgentEvent::InputAudioChunk {
                        bytes: data.len(),
                        media_type,
                    });
                }
                Some(ModelEvent::SpeechStarted) => {
                    let _ = bus.send(AgentEvent::SpeechStarted);
                }
                Some(ModelEvent::SpeechStopped) => {
                    let _ = bus.send(AgentEvent::SpeechStopped);
                }
                Some(ModelEvent::Interrupted) => {
                    frozen = true;
                    let _ = bus.send(AgentEvent::Interrupted);
                }
                // Done/Error mark responses; the session's terminal
                // state is the events stream CLOSING (after close()).
                Some(ModelEvent::Done { .. } | ModelEvent::Error { .. }) => {}
                Some(_) => {}
                None => break,
            },
        }
    }

    let _ = bus.send(AgentEvent::RunEnd {
        stop: StopReason::Stop,
    });

    let mut assistant = Vec::new();
    if !text.is_empty() {
        assistant.push(Content::Text { text });
    }
    for (media_type, bytes) in audio {
        assistant.push(Content::Audio {
            media: Media {
                media_type,
                source: MediaSource::Bytes(bytes),
            },
        });
    }
    let _ = done.send(LiveOutcome {
        uplink,
        uplink_media_type: input_media_type,
        assistant,
        interruptions,
    });
}

/// Uplink capture: `sine` synthesizes a paced 440Hz/16kHz PCM stream
/// (deterministic, hardware-free — the gate path); otherwise the
/// default input device streams for `seconds`. Chunks are 50ms of
/// 16-bit mono at 16kHz (1600 bytes) — the realtime convention, paced
/// in real time (a burst dump is not "live").
async fn capture(seconds: u32, sine: bool, chunk_tx: UnboundedSender<Vec<u8>>) {
    const CHUNK_MS: u64 = 50;
    const RATE: usize = 16_000;
    const CHUNK_SAMPLES: usize = RATE / (1000 / CHUNK_MS as usize); // 800
    let chunks = seconds as u64 * (1000 / CHUNK_MS);

    if sine {
        let mut interval = tokio::time::interval(Duration::from_millis(CHUNK_MS));
        let mut t0 = 0usize;
        for _ in 0..chunks {
            interval.tick().await;
            let mut bytes = Vec::with_capacity(CHUNK_SAMPLES * 2);
            for i in 0..CHUNK_SAMPLES {
                let t = (t0 + i) as f32 / RATE as f32;
                let s = (440.0 * 2.0 * std::f32::consts::PI * t).sin() * 0.5 * 32767.0;
                bytes.extend_from_slice(&(s as i16).to_le_bytes());
            }
            t0 += CHUNK_SAMPLES;
            if chunk_tx.send(bytes).is_err() {
                return;
            }
        }
        return;
    }

    // Real microphone: cpal streams on its own thread; bridge the
    // blocking receiver into this async task.
    match crate::audio::stream_mic(seconds) {
        Ok(mut rx) => {
            while let Some(chunk) = rx.recv().await {
                if chunk_tx.send(chunk).is_err() {
                    return;
                }
            }
        }
        Err(e) => eprintln!("[tau] mic: {e:#}"),
    }
}
