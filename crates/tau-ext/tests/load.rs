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
