//! Integration test: the mcp-bridge component exposes a mock MCP stdio
//! server as tau tools. Skipped unless the wasm artifact has been built and
//! a Python interpreter is available:
//!   cargo build --manifest-path examples/mcp-bridge/Cargo.toml \
//!       --target wasm32-wasip2 --release

use std::path::PathBuf;

use tau_ext::ExtensionHost;
use tau_ext::bridge::BridgeConfig;

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
async fn bridge_rejects_unspeakable_protocol_version() {
    // A server that chooses a protocol version the bridge does not speak
    // must fail the handshake (spec: the client disconnects) rather than
    // register tools against divergent semantics.
    let (Some(path), Some(python)) = (artifact(), python()) else {
        eprintln!("skipping: mcp_bridge.wasm not built or no python");
        return;
    };
    let server = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/mcp-bridge/mock_server.py")
        .canonicalize()
        .expect("mock server exists");
    let command = vec![
        python,
        server.to_string_lossy().into_owned(),
        "--protocol-version".into(),
        "1999-01-01".into(),
    ];

    let host = ExtensionHost::new();
    let result = host.load_bridge(
        &path,
        BridgeConfig {
            command: Some(command),
            ..BridgeConfig::default()
        },
    );
    let error = match result {
        Ok(_) => panic!("an unspeakable version must fail the handshake"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("bridge handshake failed"),
        "expected a handshake failure, got: {error}"
    );
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
    let tools = host
        .load_bridge(
            &path,
            BridgeConfig {
                command: Some(command),
                ..BridgeConfig::default()
            },
        )
        .expect("load bridge")
        .into_parts()
        .0;
    let names: Vec<String> = tools.iter().map(|t| t.def().name).collect();
    assert_eq!(names, ["echo", "fail"]);

    let echo = &tools[0];
    let out = echo
        .execute(serde_json::json!({ "text": "hello over stdio" }))
        .await;
    assert!(!out.is_error);
    assert_eq!(out.text(), "hello over stdio");

    // MCP isError surfaces as a tau tool error, not a trap.
    let fail = &tools[1];
    let out = fail.execute(serde_json::json!({})).await;
    assert!(out.is_error);
    assert_eq!(out.text(), "tool failed on purpose");

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
    let result = host.load_bridge(
        &path,
        BridgeConfig {
            command: Some(vec!["definitely-not-a-real-program-xyz".into()]),
            ..BridgeConfig::default()
        },
    );
    assert!(result.is_err());
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

#[tokio::test]
async fn bridge_exposes_mcp_tools_over_http() {
    let (Some(path), Some(python)) = (artifact(), python()) else {
        eprintln!("skipping: mcp_bridge.wasm not built or no python");
        return;
    };
    let server = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/mcp-bridge/mock_http_server.py")
        .canonicalize()
        .expect("mock http server exists");
    let port = free_port();
    let mut child = std::process::Command::new(&python)
        .arg(&server)
        .arg(port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn mock http server");
    // Wait for the server to accept connections (it binds synchronously but
    // python startup takes a moment).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("mock http server did not start");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let url = format!("http://127.0.0.1:{port}/mcp");
    let _origin = tau_ext::bridge::origin_of(&url).expect("origin");
    let host = ExtensionHost::new();
    let tools = host.load_bridge(
        &path,
        BridgeConfig {
            mcp_url: Some(url),
            ..BridgeConfig::default()
        },
    );
    let tools = match tools {
        Ok(loaded) => loaded.into_parts().0,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("load bridge over http: {e}");
        }
    };
    let names: Vec<String> = tools.iter().map(|t| t.def().name).collect();
    assert_eq!(names, ["echo", "fail"]);

    let out = tools[0]
        .execute(serde_json::json!({ "text": "hello over http" }))
        .await;
    assert!(!out.is_error);
    assert_eq!(out.text(), "hello over http");

    let out = tools[1].execute(serde_json::json!({})).await;
    assert!(out.is_error);

    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test]
async fn http_origin_outside_allowlist_is_denied() {
    let Some(path) = artifact() else {
        eprintln!("skipping: mcp_bridge.wasm not built");
        return;
    };
    let host = ExtensionHost::new();
    // mcp_url is granted (so the bridge knows where to POST) but the origins
    // allowlist is EMPTY: every http request must fail, so the handshake
    // fails and the load errors out. A bridge can never reach an origin the
    // user did not consent to.
    let result = host.load_bridge(
        &path,
        BridgeConfig {
            mcp_url: Some("http://127.0.0.1:9/mcp".into()),
            ..BridgeConfig::default()
        },
    );
    assert!(result.is_err());
}

#[test]
fn origin_of_parses_schemes_hosts_ports() {
    use tau_ext::bridge::origin_of;
    assert_eq!(
        origin_of("https://api.example.com/mcp"),
        Some("https://api.example.com".into())
    );
    assert_eq!(
        origin_of("http://127.0.0.1:8080/mcp?x=1"),
        Some("http://127.0.0.1:8080".into())
    );
    assert_eq!(
        origin_of("http://user:pw@example.com/mcp"),
        Some("http://example.com".into())
    );
    assert_eq!(origin_of("ftp://example.com/x"), None);
    assert_eq!(origin_of("not a url"), None);
}

#[tokio::test]
async fn bridge_reconnects_after_server_death() {
    // A server that dies mid-session must not wedge the bridge for the
    // rest of the process: the call that notices the death errors, and
    // the next call respawns + re-handshakes instead of writing to a
    // dead pipe forever.
    let (Some(path), Some(python)) = (artifact(), python()) else {
        eprintln!("skipping: mcp_bridge.wasm not built or no python");
        return;
    };
    let server = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/mcp-bridge/mock_server.py")
        .canonicalize()
        .expect("mock server exists");
    let command = vec![
        python,
        server.to_string_lossy().into_owned(),
        "--die-after-call".into(),
    ];

    let host = ExtensionHost::new();
    let tools = host
        .load_bridge(
            &path,
            BridgeConfig {
                command: Some(command),
                ..BridgeConfig::default()
            },
        )
        .expect("load bridge")
        .into_parts()
        .0;
    let echo = tools
        .iter()
        .find(|tool| tool.def().name == "echo")
        .expect("echo tool registered");

    // The mock exits right after replying to this first call.
    let out = echo.execute(serde_json::json!({ "text": "one" })).await;
    assert!(
        !out.is_error && out.text() == "one",
        "first call must work: {out:?}"
    );

    // This call meets the dead pipe and must report the error (no
    // silent retry of a possibly non-idempotent tool).
    let out = echo.execute(serde_json::json!({ "text": "two" })).await;
    assert!(
        out.is_error,
        "the call that meets the dead server must error, got: {out:?}"
    );

    // The connection was dropped: a fresh server is spawned and
    // re-handshaked, and the bridge works again.
    let out = echo.execute(serde_json::json!({ "text": "three" })).await;
    assert!(
        !out.is_error && out.text() == "three",
        "the bridge must reconnect after server death: {out:?}"
    );
}
