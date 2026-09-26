//! Integration test: a trapped provider fails its run — and the host
//! rebuilds the instance, so the NEXT run reaches a working guest. The
//! CLI reuses one provider instance across every run of a REPL session,
//! so without the rebuild one crash would fail every later prompt until
//! restart. Uses the http-provider example, whose `run` panics when the
//! request mentions "crash". Skipped unless the artifact has been built:
//!   cargo build --manifest-path examples/http-provider/Cargo.toml \
//!       --target wasm32-wasip2 --release

use std::path::PathBuf;

use futures::StreamExt;
use tau_core::{Message, Model, ModelEvent, Request};
use tau_ext::ExtensionHost;

fn artifact() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/http-provider/target/wasm32-wasip2/release/http_provider.wasm");
    path.exists().then_some(path)
}

fn request(text: &str) -> Request {
    Request {
        system: None,
        messages: vec![Message::user(text)],
        tools: vec![],
    }
}

async fn collect(model: &tau_ext::WasmModel, text: &str) -> Vec<ModelEvent> {
    model.stream(&request(text)).await.collect().await
}

fn error_message(events: &[ModelEvent]) -> Option<&str> {
    events.iter().find_map(|e| match e {
        ModelEvent::Error { message } => Some(message.as_str()),
        _ => None,
    })
}

#[tokio::test]
async fn trapped_provider_fails_the_run_and_the_instance_is_rebuilt() {
    let Some(path) = artifact() else {
        eprintln!("skipping: http_provider.wasm not built");
        return;
    };
    let host = ExtensionHost::new();
    // No consented origins: a working guest reports the allowlist denial
    // as its OWN error event — distinct from the host's "provider
    // trapped", so it proves the guest actually ran.
    let model = host
        .load_provider(&path, "http", Default::default(), None)
        .expect("load provider");

    let events = collect(&model, "crash now").await;
    let message = error_message(&events).expect("trap surfaces an error event");
    assert!(
        message.contains("provider trapped"),
        "the trap must surface as a host error, got: {message}"
    );

    // The rebuilt instance runs again: this error comes from the guest
    // (allowlist denial), not from a poisoned instance trapping.
    let events = collect(&model, "http://127.0.0.1:9/").await;
    let message = error_message(&events).expect("working guest reports its own error");
    assert!(
        message.contains("not in consent allowlist"),
        "the next run must reach a working guest, got: {message}"
    );
    assert!(
        !message.contains("provider trapped"),
        "the instance must not stay poisoned: {message}"
    );
}
