//! Integration test: a wasm provider's audio-delta events cross the
//! events.emit channel and arrive as ModelEvent::AudioDelta with honest
//! bytes (base64 only on the wire). Skipped unless the echo_provider
//! artifact has been built.

use std::path::PathBuf;

use futures::StreamExt;
use tau_core::model::{Model, ModelEvent};
use tau_ext::ExtensionHost;

fn artifact() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/echo-provider/target/wasm32-wasip2/release/echo_provider.wasm");
    path.exists().then_some(path)
}

#[tokio::test]
async fn audio_deltas_cross_the_channel_as_bytes() {
    let Some(wasm) = artifact() else {
        eprintln!("skipping: echo_provider.wasm not built");
        return;
    };
    let host = ExtensionHost::new();
    let model = host
        .load_provider(&wasm, "echo", Default::default(), None)
        .expect("load provider");

    let request = tau_core::Request {
        system: None,
        messages: vec![tau_core::Message::user("audio please")],
        tools: vec![],
    };
    let events: Vec<ModelEvent> = model.stream(&request).await.collect().await;
    let chunks: Vec<Vec<u8>> = events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::AudioDelta { data, media_type } => {
                assert_eq!(media_type, "audio/pcm;rate=24000");
                Some(data.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(chunks, vec![vec![1, 2, 3], vec![4, 5]]);
    assert!(events.iter().any(|e| matches!(
        e,
        ModelEvent::TextDelta { text } if text.contains("audio chunks")
    )));
    assert!(matches!(
        events.last(),
        Some(ModelEvent::Done {
            stop: tau_core::StopReason::Stop
        })
    ));
}

/// The host's WasiPolicy reaches the guest's ambient env: allow-all
/// inherits the process environment, deny-all hides it.
#[tokio::test]
async fn wasi_policy_controls_ambient_env() {
    let Some(wasm) = artifact() else {
        eprintln!("skipping: echo_provider.wasm not built");
        return;
    };
    unsafe { std::env::set_var("TAU_EXT_TEST_ENV", "visible") };

    let run = |policy| {
        let wasm = wasm.clone();
        async move {
            let host = ExtensionHost::new().with_wasi(policy);
            let model = host
                .load_provider(&wasm, "echo", Default::default(), None)
                .expect("load provider");
            let request = tau_core::Request {
                system: None,
                messages: vec![tau_core::Message::user("env TAU_EXT_TEST_ENV")],
                tools: vec![],
            };
            let mut text = String::new();
            let mut stream = model.stream(&request).await;
            while let Some(event) = stream.next().await {
                if let ModelEvent::TextDelta { text: delta } = event {
                    text.push_str(&delta);
                }
            }
            text
        }
    };

    let allow = run(tau_ext::WasiPolicy::AllowAll).await;
    assert_eq!(allow, "TAU_EXT_TEST_ENV=visible");
    let deny = run(tau_ext::WasiPolicy::DenyAll).await;
    assert_eq!(deny, "TAU_EXT_TEST_ENV=");
}
