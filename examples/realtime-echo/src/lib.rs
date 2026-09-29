//! Example tau realtime provider (world `realtime`, docs/realtime-av.md):
//! the SAME deterministic script as tau-core's FauxRealtime — one script,
//! two carriers (native test double and wasm component), so the gate's two
//! paths cross-prove each other:
//!
//!   first uplink chunk of a burst → speech-started (+ one text note);
//!   every chunk → input-audio-chunk fact + audio-delta echo of the same
//!   bytes and media type; interrupt() → interrupted; the uplink's
//!   writable end going away (which is what the host's close does) →
//!   speech-stopped (if mid-burst) + done(stop).
//!
//! 0.7.0 made the whole session resources-and-streams: the guest produces
//! the downlink (a stream the host drains), consumes the uplinks
//! (host-produced byte streams) and answers interrupt() as an ordinary
//! async call. There is no `close` call to answer any more — the uplink
//! ending IS the close signal, and the flush rides the downlink the host
//! is still reading.
//!
//! The component doubles as an ordinary provider: `run` answers plain
//! text (print mode keeps working with this component loaded).
//!
//! One guest-side detail worth knowing before copying this shape: the
//! downlink pump waits on a purely Rust event (a local channel), and a
//! guest task that sleeps on Rust-originating events only is not woken by
//! the component-model executor unless `wit-bindgen` is built with
//! `inter-task-wakeup` — without it the first such await panics the task.
//! Tasks that await only component-model operations (streams, futures)
//! need nothing extra; this one does, so the feature is on.
//!
//! Build:
//!   cargo build --manifest-path examples/realtime-echo/Cargo.toml --target wasm32-wasip2 --release
//! Use:
//!   tau --provider-wasm .../realtime_echo.wasm --model echo-realtime
//!   # then /live 2 sine in the REPL

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "realtime",
});

use std::cell::RefCell;
use std::rc::Rc;

use exports::tau::extension::models::{AudioDelta, Event, Guest as ModelsGuest, Info, Request};
use exports::tau::extension::session::{Config, Guest as SessionGuest, GuestSession, Session};
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use tau::extension::types::{Error, StopReason};
use wit_bindgen::rt::async_support::{FutureReader, StreamReader, StreamResult, spawn_local};

struct EchoRealtime;

/// Everything one live session carries. A plain `Rc<RefCell<..>>` is
/// honest here: a guest component is single-threaded, and every task below
/// is a `spawn_local` on that one thread.
struct Live {
    input_media_type: String,
    speech_active: bool,
    noted: bool,
    /// The downlink pump's inbox. Every producer holds only the sender
    /// (it is `Clone`), which is what lets `interrupt` emit without ever
    /// touching the writer the pump owns.
    tx: UnboundedSender<Event>,
    /// Taken by `downlink`, which happens once per session.
    rx: Option<UnboundedReceiver<Event>>,
}

pub struct EchoSession {
    live: Rc<RefCell<Live>>,
}

impl ModelsGuest for EchoRealtime {
    fn list_models() -> Vec<Info> {
        vec![Info {
            id: "echo-realtime".into(),
            name: "Echo Realtime (demo provider)".into(),
            context_window: 8192,
            max_output: 1024,
        }]
    }

    async fn run(request: Request) -> (StreamReader<Event>, FutureReader<Result<(), Error>>) {
        let _ = request;
        let (mut events, events_rx) = wit_stream::new::<Event>();
        let (verdict, verdict_rx) = wit_future::new::<Result<(), Error>>(|| Ok(()));
        spawn_local(async move {
            // The writer must live under an async export for `spawn_local`
            // to be scheduled at all (docs/wit-redesign.md §5): `run` is
            // `async func`, so this task is driven.
            for event in [
                Event::TextDelta("I am a realtime provider — use /live in the REPL. ".into()),
                Event::Done(StopReason::Stop),
            ] {
                if events.write_one(event).await.is_some() {
                    break; // the host hung up: nothing is left to deliver
                }
            }
            drop(events);
            let _ = verdict.write(Ok(())).await;
        });
        (events_rx, verdict_rx)
    }
}

impl SessionGuest for EchoRealtime {
    type Session = EchoSession;
}

impl GuestSession for EchoSession {
    async fn create(config: Config) -> Result<Session, Error> {
        let (tx, rx) = unbounded();
        Ok(Session::new(EchoSession {
            live: Rc::new(RefCell::new(Live {
                input_media_type: config.input_media_type,
                speech_active: false,
                noted: false,
                tx,
                rx: Some(rx),
            })),
        }))
    }

    /// Uplink audio: the host writes, the guest reads. One read is one
    /// burst of the script (a byte stream carries no framing of its own,
    /// so a burst boundary is whatever one read returns).
    async fn uplink_audio(&self, audio: StreamReader<u8>) -> FutureReader<Result<(), Error>> {
        let (verdict, verdict_rx) = wit_future::new::<Result<(), Error>>(|| Ok(()));
        let live = Rc::clone(&self.live);
        spawn_local(async move {
            let mut audio = audio;
            let mut buf: Vec<u8> = Vec::new();
            loop {
                let before = buf.len();
                if buf.len() == buf.capacity() {
                    buf.reserve(4096);
                }
                let (status, filled) = audio.read(buf).await;
                buf = filled;
                if buf.len() > before {
                    burst(&live, &buf[before..]);
                }
                if matches!(status, StreamResult::Dropped | StreamResult::Cancelled) {
                    break;
                }
            }
            // The uplink's writable end is gone — in this contract that IS
            // the close signal, so the flush belongs here.
            let tail = {
                let mut live = live.borrow_mut();
                let mut tail = Vec::new();
                if live.speech_active {
                    live.speech_active = false;
                    tail.push(Event::SpeechStopped);
                }
                tail.push(Event::Done(StopReason::Stop));
                tail
            };
            {
                let live = live.borrow();
                for event in tail {
                    let _ = live.tx.unbounded_send(event);
                }
            }
            let _ = verdict.write(Ok(())).await;
        });
        verdict_rx
    }

    /// Uplink images: accepted, unanswered — this double has no eyes. The
    /// task still has to drain the stream, or the host's writes would pile
    /// up against a reader nobody drives.
    async fn uplink_image(&self, jpeg: StreamReader<Vec<u8>>) -> FutureReader<Result<(), Error>> {
        let (verdict, verdict_rx) = wit_future::new::<Result<(), Error>>(|| Ok(()));
        spawn_local(async move {
            let mut jpeg = jpeg;
            while jpeg.next().await.is_some() {}
            let _ = verdict.write(Ok(())).await;
        });
        verdict_rx
    }

    /// Downlink: the guest's event stream, pumped from the session's inbox
    /// by one task that owns the writer. It ends after the terminal event,
    /// which is the contract's normal end — and what lets the host's close
    /// wait for the flush instead of racing it.
    async fn downlink(&self) -> (StreamReader<Event>, FutureReader<Result<(), Error>>) {
        let (mut events, events_rx) = wit_stream::new::<Event>();
        let (verdict, verdict_rx) = wit_future::new::<Result<(), Error>>(|| Ok(()));
        let rx = self.live.borrow_mut().rx.take();
        spawn_local(async move {
            if let Some(mut rx) = rx {
                while let Some(event) = rx.next().await {
                    let terminal = matches!(event, Event::Done(_) | Event::Error(_));
                    let hung_up = events.write_one(event).await.is_some();
                    if terminal || hung_up {
                        break;
                    }
                }
            }
            // Dropping the writer ends the downlink.
            drop(events);
            let _ = verdict.write(Ok(())).await;
        });
        (events_rx, verdict_rx)
    }

    async fn interrupt(&self) -> Result<(), Error> {
        let live = self.live.borrow();
        let _ = live.tx.unbounded_send(Event::Interrupted);
        Ok(())
    }
}

/// One uplink burst, exactly as the native double scripts it: VAD on the
/// first one, the note once, then the fact and the echo.
fn burst(live: &Rc<RefCell<Live>>, bytes: &[u8]) {
    let media_type = {
        let mut live = live.borrow_mut();
        if !live.speech_active {
            live.speech_active = true;
            let _ = live.tx.unbounded_send(Event::SpeechStarted);
        }
        if !live.noted {
            live.noted = true;
            let _ = live
                .tx
                .unbounded_send(Event::TextDelta("live echo active. ".into()));
        }
        live.input_media_type.clone()
    };
    let live = live.borrow();
    let _ = live.tx.unbounded_send(Event::InputAudioChunk(AudioDelta {
        data: bytes.to_vec(),
        media_type: media_type.clone(),
    }));
    // The echo: every uplink byte comes back down, same media type —
    // duplex, VAD, assembly and the playback sink all exercised by one
    // deterministic rule.
    let _ = live.tx.unbounded_send(Event::AudioDelta(AudioDelta {
        data: bytes.to_vec(),
        media_type,
    }));
}

export!(EchoRealtime);
