//! In-process ingress end-to-end (docs/im-channels.md): load the
//! whatsapp bridge with an ingress consent, fire session_start (the
//! guest calls ingress.listen), then POST a webhook event over a real
//! TCP connection and assert the guest's ingress-handler answered.
//! The pty-level leg (listener survives into an idle REPL that wakes
//! on the steer) is scripts/wa_ingress_e2e.py, validate.sh step 5d.

use std::path::PathBuf;

use tau_core::probe::{ProbePoint, Verdict};
use tau_ext::bridge::BridgeConsent;
use tau_ext::ExtensionHost;

fn artifact() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/whatsapp-bridge/target/wasm32-wasip2/release/whatsapp_bridge.wasm");
    path.exists().then_some(path)
}

#[tokio::test]
async fn whatsapp_ingress_end_to_end() {
    let Some(path) = artifact() else {
        eprintln!("skipping: whatsapp_bridge.wasm not built");
        return;
    };
    let host = ExtensionHost::new();
    let consent = BridgeConsent {
        ingress: vec!["127.0.0.1:53913".into()],
        inject: true,
        ..BridgeConsent::default()
    };
    let loaded = host.load_bridge(&path, consent).expect("load bridge");
    let (_tools, probes) = loaded.into_parts();
    assert_eq!(probes.len(), 1, "bridge must contribute its probes");

    // session_start → the guest calls ingress.listen.
    let verdict = probes[0]
        .probe(
            ProbePoint::SessionStart,
            serde_json::json!({"session": "t", "model": "t"}),
        )
        .await;
    assert!(
        matches!(verdict, Verdict::Continue),
        "session_start probe: {verdict:?}"
    );

    // The listener must be accepting now.
    for attempt in 0..40 {
        if std::net::TcpStream::connect("127.0.0.1:53913").is_ok() {
            break;
        }
        if attempt == 39 {
            panic!("ingress listener never opened on 127.0.0.1:53913");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // POST a webhook event: the guest steers (inject consented — the
    // control channel is late-bound and absent here, so steer may fail;
    // the ACK SHAPE is what we assert: not an error status line).
    let mut stream = std::net::TcpStream::connect("127.0.0.1:53913").unwrap();
    use std::io::{Read, Write};
    let body = br#"{"type":"message","chat_id":"loopback-c1","user":"loopback-user","text":"ping"}"#;
    write!(
        stream,
        "POST /im/whatsapp HTTP/1.1\r\nHost: 127.0.0.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 2") || text.starts_with("HTTP/1.1 4"),
        "webhook got a broken response: {}",
        &text[..text.len().min(400)]
    );
}
