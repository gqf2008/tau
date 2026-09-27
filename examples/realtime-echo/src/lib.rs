//! Example tau realtime provider (world `realtime`, docs/realtime-av.md
//! Phase 2b): the SAME deterministic script as tau-core's FauxRealtime —
//! one script, two carriers (native test double and wasm component), so
//! the gate's two paths cross-prove each other:
//!
//!   first push-audio of a burst → speech-started (+ one text note);
//!   every chunk → input-audio-chunk fact + audio-delta echo of the
//!   same bytes and media type; interrupt() → interrupted; close() →
//!   speech-stopped (if mid-burst) + done(stop).
//!
//! The component doubles as an ordinary provider: `run` answers plain
//! text (print mode keeps working with this component loaded).
//!
//! Build:
//!   cargo build --manifest-path examples/realtime-echo/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use:
//!   tau --provider-wasm examples/realtime-echo/target/wasm32-wasip2/release/realtime_echo.wasm \
//!       --model echo-realtime   # then /live 2 sine in the REPL

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "realtime",
});

use std::cell::Cell;

use exports::tau::extension::models::{Guest as ModelsGuest, Info};
use exports::tau::extension::session::Guest as SessionGuest;
use tau::extension::events::{self, AudioDelta, ModelEvent, StopReason};

struct EchoRealtime;

// One session per instance (the host instantiates per open), so plain
// Cell state is honest — there is no second caller.
thread_local! {
    static SPEECH_ACTIVE: Cell<bool> = const { Cell::new(false) };
    static NOTED: Cell<bool> = const { Cell::new(false) };
    static INPUT_MEDIA_TYPE: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

fn emit(event: ModelEvent) {
    // Semantic violations (empty media-type) are refused by the host;
    // this script never produces one, so a refusal IS a bug worth
    // trapping on — but the contract prefers reporting, so ignore the
    // result the way the host ignores a dropped receiver.
    let _ = events::emit(&event);
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

    fn run(request_json: String) {
        let _ = request_json;
        emit(ModelEvent::TextDelta(
            "I am a realtime provider — use /live in the REPL. ".into(),
        ));
        emit(ModelEvent::Done(StopReason::Stop));
    }
}

impl SessionGuest for EchoRealtime {
    fn open(config_json: String) -> Result<(), String> {
        let parsed: serde_json::Value =
            serde_json::from_str(&config_json).map_err(|e| format!("config json: {e}"))?;
        let input = parsed["input-media-type"]
            .as_str()
            .ok_or("config missing input-media-type")?
            .to_string();
        INPUT_MEDIA_TYPE.with(|m| *m.borrow_mut() = input);
        Ok(())
    }

    fn push_audio(data: Vec<u8>) -> Result<(), String> {
        let media_type = INPUT_MEDIA_TYPE.with(|m| m.borrow().clone());
        SPEECH_ACTIVE.with(|active| {
            if !active.get() {
                active.set(true);
                emit(ModelEvent::SpeechStarted);
            }
        });
        NOTED.with(|noted| {
            if !noted.get() {
                noted.set(true);
                emit(ModelEvent::TextDelta("live echo active. ".into()));
            }
        });
        emit(ModelEvent::InputAudioChunk(AudioDelta {
            data: data.clone(),
            media_type: media_type.clone(),
        }));
        // The echo: every uplink byte comes back down, same media type.
        emit(ModelEvent::AudioDelta(AudioDelta { data, media_type }));
        Ok(())
    }

    fn push_image(_jpeg: Vec<u8>) -> Result<(), String> {
        Ok(()) // accepted, unanswered — this double has no eyes
    }

    fn interrupt() -> Result<(), String> {
        emit(ModelEvent::Interrupted);
        Ok(())
    }

    fn close() -> Result<(), String> {
        SPEECH_ACTIVE.with(|active| {
            if active.get() {
                active.set(false);
                emit(ModelEvent::SpeechStopped);
            }
        });
        emit(ModelEvent::Done(StopReason::Stop));
        Ok(())
    }
}

export!(EchoRealtime);
