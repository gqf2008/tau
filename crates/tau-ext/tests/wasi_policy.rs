//! Integration test: --deny-wasi restores the sandbox. The guard
//! example's probe reads the ambient env var TAU_AMBIENT when the tool
//! arguments carry "wasicheck": under the default allow-all WASI the
//! guest inherits the host env and the probe blocks (the leak is
//! visible); under DenyAll the guest env is empty and the call passes.
//! Skipped unless the wasm artifact has been built:
//!   cargo build --manifest-path examples/guard/Cargo.toml \
//!       --target wasm32-wasip2 --release

use std::path::PathBuf;

use tau_core::probe::{ProbePoint, Verdict};
use tau_core::probe_payload::ProbePayload;
use tau_core::types::ToolCall;
use tau_ext::{ExtensionHost, WasiPolicy};

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
async fn deny_wasi_hides_the_ambient_env_that_allow_all_exposes() {
    let Some(path) = artifact() else {
        eprintln!("skipping: guard.wasm not built");
        return;
    };
    // SAFETY: no other test in this process reads or writes TAU_AMBIENT.
    unsafe { std::env::set_var("TAU_AMBIENT", "hunter2") };

    // Default ambient WASI: the guest inherits the host env, sees the
    // var, and the probe blocks — the leak is observable.
    let host = ExtensionHost::new();
    let extension = host.load(&path).expect("load extension");
    let (_tools, probes) = extension.into_parts();
    let verdict = probes[0]
        .probe(ProbePoint::BeforeTool, tool_payload("w1", "wasicheck"))
        .await;
    assert!(
        matches!(verdict, Verdict::Block { .. }),
        "ambient env must be visible under the default policy, got {verdict:?}"
    );

    // DenyAll: the guest env is empty, the var is gone, the call passes.
    let host = ExtensionHost::new().with_wasi_policy(WasiPolicy::DenyAll);
    let extension = host.load(&path).expect("load extension");
    let (_tools, probes) = extension.into_parts();
    let verdict = probes[0]
        .probe(ProbePoint::BeforeTool, tool_payload("w2", "wasicheck"))
        .await;
    assert!(
        matches!(verdict, Verdict::Continue),
        "--deny-wasi must hide the ambient env, got {verdict:?}"
    );

    // SAFETY: same as above.
    unsafe { std::env::remove_var("TAU_AMBIENT") };
}

/// Regression: allow-all preopens must be reachable UNDER THE NAMES the
/// contract promises (`/` on unix, `/<letter>` per drive on Windows).
/// A host that pushed drive preopens under mangled guest names (a u8
/// formatted as its decimal code — "/99" instead of "/c") passed every
/// host-side check while guests saw a filesystem that existed but could
/// not be spelled; this test fails on that host.
#[tokio::test]
async fn allow_all_preopens_are_reachable_under_contract_names() {
    let Some(path) = artifact() else {
        eprintln!("skipping: guard.wasm not built");
        return;
    };
    let host = ExtensionHost::new();
    let extension = host.load(&path).expect("load extension");
    let (_tools, probes) = extension.into_parts();
    let verdict = probes[0]
        .probe(ProbePoint::BeforeTool, tool_payload("f1", "fscheck"))
        .await;
    assert!(
        matches!(verdict, Verdict::Block { .. }),
        "allow-all must expose at least one preopen at a contract name, got {verdict:?}"
    );

    let host = ExtensionHost::new().with_wasi_policy(WasiPolicy::DenyAll);
    let extension = host.load(&path).expect("load extension");
    let (_tools, probes) = extension.into_parts();
    let verdict = probes[0]
        .probe(ProbePoint::BeforeTool, tool_payload("f2", "fscheck"))
        .await;
    assert!(
        matches!(verdict, Verdict::Continue),
        "--deny-wasi must expose zero preopens, got {verdict:?}"
    );
}
