//! Integration test: loads the example component and exercises both the tool
//! and the sandbox. Skipped unless the wasm artifact has been built:
//!   cargo build --manifest-path examples/upper/Cargo.toml \
//!       --target wasm32-wasip2 --release

use std::path::PathBuf;

use tau_ext::ExtensionHost;

fn artifact() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/upper/target/wasm32-wasip2/release/upper.wasm");
    path.exists().then_some(path)
}

#[tokio::test]
async fn loads_tool_and_executes() {
    let Some(path) = artifact() else {
        eprintln!("skipping: upper.wasm not built");
        return;
    };
    let host = ExtensionHost::new();
    let extension = host.load(&path).expect("load extension");
    let (tools, _probes) = extension.into_parts();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].def().name, "upper");

    let out = tools[0]
        .execute(serde_json::json!({ "text": "hello tau" }))
        .await;
    assert!(!out.is_error);
    assert_eq!(out.content, "HELLO TAU");
}

fn provider_artifact() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/echo-provider/target/wasm32-wasip2/release/echo_provider.wasm");
    path.exists().then_some(path)
}

/// The load contract is "select one of the component's models by id":
/// an unknown id must be refused at load, naming the available ones —
/// never silently run whatever the guest does with a model it does not
/// list. (A typo'd --model used to sail through and the run "worked".)
#[tokio::test]
async fn provider_load_refuses_an_unknown_model_id() {
    let Some(path) = provider_artifact() else {
        eprintln!("skipping: echo_provider.wasm not built");
        return;
    };
    let host = ExtensionHost::new();
    let err = host
        .load_provider(&path, "nosuch", Default::default(), None)
        .err()
        .expect("unknown model id must not load");
    let message = err.to_string();
    assert!(message.contains("nosuch"), "names the bad id: {message}");
    assert!(
        message.contains("echo"),
        "lists the available ids: {message}"
    );

    // The listed id still loads.
    host.load_provider(&path, "echo", Default::default(), None)
        .expect("the advertised model id loads");
}
