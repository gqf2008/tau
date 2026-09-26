//! Integration test: a wasm provider's HTTP egress is consent-gated.
//! With the origin granted the component fetches successfully; without
//! consent the http call fails at call time and the provider reports an
//! error event (never a trap). Skipped unless the http_provider.wasm
//! artifact has been built and python is available.

use std::path::PathBuf;

use futures::StreamExt;
use tau_core::{Message, Model, ModelEvent, Request};
use tau_ext::ExtensionHost;

fn artifact() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/http-provider/target/wasm32-wasip2/release/http_provider.wasm");
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

/// Serve a directory with one file over http on an ephemeral port.
struct StaticServer {
    child: std::process::Child,
    port: u16,
    _dir: PathBuf,
}

impl StaticServer {
    fn start(python: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("tau-test-http-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hello.txt"), "hello from server").unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = std::process::Command::new(python)
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
            ])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn static server");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            if std::time::Instant::now() > deadline {
                panic!("static server did not start");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Self {
            child,
            port,
            _dir: dir,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/hello.txt", self.port)
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for StaticServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

async fn run_provider(model: &dyn Model, url: &str) -> (String, Option<String>) {
    let request = Request {
        messages: vec![Message::user(url)],
        ..Request::default()
    };
    let mut text = String::new();
    let mut error = None;
    let mut stream = model.stream(&request).await;
    while let Some(event) = stream.next().await {
        match event {
            ModelEvent::TextDelta { text: delta } => text.push_str(&delta),
            ModelEvent::Error { message } => error = Some(message),
            _ => {}
        }
    }
    (text, error)
}

#[tokio::test]
async fn granted_origin_fetches() {
    let (Some(wasm), Some(python)) = (artifact(), python()) else {
        eprintln!("skipping: http_provider.wasm not built or no python");
        return;
    };
    let server = StaticServer::start(&python);
    let host = ExtensionHost::new();
    let model = host
        .load_provider(&wasm, "http", [server.origin()].into_iter().collect(), None)
        .expect("load provider");
    let (text, error) = run_provider(&model, &server.url()).await;
    assert!(error.is_none(), "unexpected error: {error:?}");
    assert_eq!(text, "STATUS 200: hello from server");
}

#[tokio::test]
async fn consented_auth_token_reaches_the_guest() {
    let (Some(wasm), Some(python)) = (artifact(), python()) else {
        eprintln!("skipping: http_provider.wasm not built or no python");
        return;
    };
    let server = StaticServer::start(&python);
    let host = ExtensionHost::new();
    let model = host
        .load_provider(
            &wasm,
            "http",
            [server.origin()].into_iter().collect(),
            Some("tok-secret".into()),
        )
        .expect("load provider");
    let (text, error) = run_provider(&model, &server.url()).await;
    assert!(error.is_none(), "unexpected error: {error:?}");
    // The guest saw {"auth": {"bearer": …}} and marked its answer; the
    // token itself never appears in the output.
    assert_eq!(text, "STATUS 200 [auth]: hello from server");
    assert!(!text.contains("tok-secret"));
}

#[tokio::test]
async fn ungranted_origin_is_denied_at_call_time() {
    let (Some(wasm), Some(python)) = (artifact(), python()) else {
        eprintln!("skipping: http_provider.wasm not built or no python");
        return;
    };
    let server = StaticServer::start(&python);
    let host = ExtensionHost::new();
    // No consent: empty origin set.
    let model = host
        .load_provider(&wasm, "http", Default::default(), None)
        .expect("load provider");
    let (text, error) = run_provider(&model, &server.url()).await;
    assert!(text.is_empty(), "no fetch without consent, got: {text:?}");
    let error = error.expect("an error event, not a trap");
    assert!(
        error.contains("allowlist"),
        "error should name the consent allowlist, got: {error}"
    );
}
