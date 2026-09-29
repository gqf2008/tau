//! Integration test: a probe that traps degrades to continue instead of
//! wedging the run — and the same instance keeps probing afterwards.
//! Uses the guard example, whose probe panics when the tool arguments
//! carry "crash". Skipped unless the wasm artifact has been built:
//!   cargo build --manifest-path examples/guard/Cargo.toml \
//!       --target wasm32-wasip2 --release

use std::path::PathBuf;

use tau_core::probe::{ProbePoint, Verdict};
use tau_core::probe_payload::ProbePayload;
use tau_core::types::ToolCall;
use tau_ext::ExtensionHost;

fn artifact() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/guard/target/wasm32-wasip2/release/guard.wasm");
    path.exists().then_some(path)
}

/// A `before_tool` firing for the guard example's probe.
fn tool_payload(id: &str, text: &str) -> ProbePayload {
    ProbePayload::BeforeTool(ToolCall {
        id: id.into(),
        name: "upper".into(),
        arguments: serde_json::json!({ "text": text }),
    })
}

#[tokio::test]
async fn trapped_probe_degrades_to_continue_and_the_instance_survives() {
    let Some(path) = artifact() else {
        eprintln!("skipping: guard.wasm not built");
        return;
    };
    let host = ExtensionHost::new();
    let extension = host.load(&path).expect("load extension");
    let (_tools, probes) = extension.into_parts();
    assert_eq!(probes.len(), 1);
    let probe = &probes[0];

    // The trap must not surface: the verdict degrades to continue.
    let verdict = probe
        .probe(ProbePoint::BeforeTool, tool_payload("t1", "crash me"))
        .await;
    assert!(
        matches!(verdict, Verdict::Continue),
        "trapped probe must degrade to continue, got {verdict:?}"
    );

    // "Never wedges the run" means later probes on the same instance
    // still decide: a forbidden call must still block after the crash.
    let verdict = probe
        .probe(ProbePoint::BeforeTool, tool_payload("t2", "forbidden"))
        .await;
    assert!(
        matches!(verdict, Verdict::Block { .. }),
        "probe must keep deciding after a trap, got {verdict:?}"
    );
}
