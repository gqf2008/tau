//! Integration test: the mcp-bridge component exposes a mock MCP stdio
//! server as tau tools. Skipped unless the wasm artifact has been built and
//! a Python interpreter is available:
//!   cargo build --manifest-path examples/mcp-bridge/Cargo.toml \
//!       --target wasm32-wasip2 --release

use std::path::PathBuf;

use tau_ext::ExtensionHost;

fn artifact() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/mcp-bridge/target/wasm32-wasip2/release/mcp_bridge.wasm");
    path.exists().then_some(path)
}

fn python() -> Option<String> {
    for candidate in ["python", "python3", "py"] {
        if std::process::Command::new(candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Some(candidate.to_string());
        }
    }
    None
}

#[tokio::test]
async fn bridge_exposes_mcp_tools() {
    let (Some(path), Some(python)) = (artifact(), python()) else {
        eprintln!("skipping: mcp_bridge.wasm not built or no python");
        return;
    };
    let server = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/mcp-bridge/mock_server.py")
        .canonicalize()
        .expect("mock server exists");
    let command = vec![python, server.to_string_lossy().into_owned()];

    let host = ExtensionHost::new();
    let tools = host.load_bridge(&path, &command).expect("load bridge");
    let names: Vec<String> = tools.iter().map(|t| t.def().name).collect();
    assert_eq!(names, ["echo", "fail"]);

    let echo = &tools[0];
    let out = echo.execute(serde_json::json!({ "text": "hello over stdio" })).await;
    assert!(!out.is_error);
    assert_eq!(out.content, "hello over stdio");

    // MCP isError surfaces as a tau tool error, not a trap.
    let fail = &tools[1];
    let out = fail.execute(serde_json::json!({})).await;
    assert!(out.is_error);
    assert_eq!(out.content, "tool failed on purpose");

    // echo with a non-string argument still yields a result, not a trap.
    let out = echo.execute(serde_json::json!({ "text": 42 })).await;
    assert!(!out.is_error);
}

#[tokio::test]
async fn bridge_rejects_missing_command() {
    let Some(path) = artifact() else {
        eprintln!("skipping: mcp_bridge.wasm not built");
        return;
    };
    let host = ExtensionHost::new();
    // A command that does not exist must fail at handshake, not silently.
    let result = host.load_bridge(&path, &["definitely-not-a-real-program-xyz".into()]);
    assert!(result.is_err());
}
