//! Integration test: a wasm realtime component (world `realtime`,
//! examples/realtime-echo) drives a full session across the boundary —
//! open → VAD + echo per chunk → interrupt → close flush — with the
//! events arriving as typed tau_core::ModelEvent. Skipped unless the
//! realtime_echo artifact has been built.

use std::path::PathBuf;

use futures::StreamExt;
use tau_core::model::{Model, ModelEvent, RealtimeConfig, StopReason};
use tau_ext::ExtensionHost;

fn artifact() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../../examples/realtime-echo/target/wasm32-wasip2/release/realtime_echo.wasm",
    );
    path.exists().then_some(path)
}

fn config() -> RealtimeConfig {
    RealtimeConfig {
        input_media_type: "audio/pcm;rate=16000".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn realtime_session_crosses_the_wasm_boundary() {
    let Some(wasm) = artifact() else {
        eprintln!("skipping: realtime_echo.wasm not built");
        return;
    };
    let host = ExtensionHost::new();
    // The capability probe reads the component's exports.
    let bytes = std::fs::read(&wasm).unwrap();
    assert!(host.is_realtime_component(&bytes));
    let model = host
        .load_realtime(&wasm, "echo-realtime", Default::default(), None)
        .expect("load realtime provider");

    // Request/response still works (the component doubles as a plain
    // provider)…
    let request = tau_core::Request {
        system: None,
        messages: vec![tau_core::Message::user("hello")],
        tools: vec![],
    };
    let events: Vec<ModelEvent> = model.stream(&request).await.collect().await;
    assert!(events.iter().any(|e| matches!(e, ModelEvent::TextDelta { text } if text.contains("realtime provider"))));

    // …and the session runs the deterministic script.
    let mut session = model.realtime(config()).expect("realtime capability");
    let mut events = session.events();
    session.push_audio(vec![1, 2, 3, 4]).await.unwrap();
    session.push_audio(vec![5, 6]).await.unwrap();
    session.interrupt().await.unwrap();
    session.close().await.unwrap();

    let mut seen = Vec::new();
    while let Some(event) = events.next().await {
        seen.push(event);
    }
    assert_eq!(
        seen,
        vec![
            ModelEvent::SpeechStarted,
            ModelEvent::TextDelta { text: "live echo active. ".into() },
            ModelEvent::InputAudioChunk { data: vec![1, 2, 3, 4], media_type: "audio/pcm;rate=16000".into() },
            ModelEvent::AudioDelta { data: vec![1, 2, 3, 4], media_type: "audio/pcm;rate=16000".into() },
            ModelEvent::InputAudioChunk { data: vec![5, 6], media_type: "audio/pcm;rate=16000".into() },
            ModelEvent::AudioDelta { data: vec![5, 6], media_type: "audio/pcm;rate=16000".into() },
            ModelEvent::Interrupted,
            ModelEvent::SpeechStopped,
            ModelEvent::Done { stop: StopReason::Stop },
        ]
    );

    // The door refuses after close.
    assert!(session.push_audio(vec![7]).await.is_err());
    assert!(session.interrupt().await.is_err());
    assert!(session.close().await.is_ok()); // twice is a no-op
}

#[tokio::test]
async fn plain_provider_is_not_realtime_and_reports_so() {
    let provider = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/echo-provider/target/wasm32-wasip2/release/echo_provider.wasm");
    if !provider.exists() {
        eprintln!("skipping: echo_provider.wasm not built");
        return;
    }
    let host = ExtensionHost::new();
    let bytes = std::fs::read(&provider).unwrap();
    // The probe says no…
    assert!(!host.is_realtime_component(&bytes));
    // …and the plain provider's realtime() is the default None.
    let model = host
        .load_provider(&provider, "echo", Default::default(), None)
        .expect("load provider");
    assert!(model.realtime(config()).is_none());
}
