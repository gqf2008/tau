//! Integration test: pull a component from a mock OCI registry and load it
//! through the normal path (signature/trust policy unchanged). Skipped
//! unless the upper.wasm artifact has been built and python is available.

use std::path::PathBuf;

use tau_ext::ExtensionHost;
use tau_ext::oci;

fn artifact() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/upper/target/wasm32-wasip2/release/upper.wasm");
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

struct MockRegistry {
    child: std::process::Child,
    port: u16,
}

impl MockRegistry {
    fn start(python: &str, blob: &PathBuf, auth: bool) -> Self {
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/mock_oci_registry.py")
            .canonicalize()
            .expect("mock registry exists");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind ephemeral port")
            .local_addr()
            .expect("local addr")
            .port();
        let mut command = std::process::Command::new(python);
        command
            .arg(&script)
            .arg(port.to_string())
            .arg(blob)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if auth {
            command.arg("--auth");
        }
        let child = command.spawn().expect("spawn mock registry");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            if std::time::Instant::now() > deadline {
                panic!("mock registry did not start");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Self { child, port }
    }
}

impl Drop for MockRegistry {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// reqwest blocking must not run on a tokio runtime thread — pull on a
/// blocking thread and join it.
async fn pull(reference: &str, cache: &std::path::Path) -> Result<oci::Pulled, oci::OciError> {
    let reference = reference.to_string();
    let cache = cache.to_path_buf();
    tokio::task::spawn_blocking(move || oci::pull_into(&reference, &cache))
        .await
        .expect("pull task")
}

fn cache() -> PathBuf {
    // Unique per call, by construction: these tests run in parallel and
    // `SystemTime::now()` is not a source of uniqueness on Windows (it
    // advances in 100 ns steps, so two tests starting together get the
    // same stamp, share a cache dir, and one removes it under the other
    // -- PermissionDenied / NotFound out of `pull_into`).
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("tau-test-oci-{}-{seq}", std::process::id()))
}

#[tokio::test]
async fn pulls_loads_and_caches_from_registry() {
    let (Some(wasm), Some(python)) = (artifact(), python()) else {
        eprintln!("skipping: upper.wasm not built or no python");
        return;
    };
    let registry = MockRegistry::start(&python, &wasm, false);
    let cache = cache();
    let reference = format!("oci://127.0.0.1:{}/test/component:latest", registry.port);

    let pulled = pull(&reference, &cache).await.expect("pull");
    assert!(pulled.mutable_tag);
    assert!(pulled.digest.starts_with("sha256:"));
    // The pulled bytes are exactly the artifact's bytes.
    assert_eq!(
        std::fs::read(&pulled.path).unwrap(),
        std::fs::read(&wasm).unwrap()
    );

    // Second pull is a cache hit (manifest is re-fetched, blob is not).
    let pulled_again = pull(&reference, &cache).await.expect("pull cached");
    assert_eq!(pulled_again.path, pulled.path);

    // The cached file loads and runs through the normal path.
    let host = ExtensionHost::new();
    let extension = host.load(&pulled.path).expect("load pulled component");
    let (tools, _) = extension.into_parts();
    let out = tools[0]
        .execute(serde_json::json!({ "text": "from oci" }))
        .await;
    assert_eq!(out.text(), "FROM OCI");

    let _ = std::fs::remove_dir_all(&cache);
}

#[tokio::test]
async fn pulls_behind_the_bearer_token_dance() {
    let (Some(wasm), Some(python)) = (artifact(), python()) else {
        eprintln!("skipping: upper.wasm not built or no python");
        return;
    };
    let registry = MockRegistry::start(&python, &wasm, true);
    let cache = cache();
    let reference = format!("oci://127.0.0.1:{}/test/component:latest", registry.port);
    let pulled = pull(&reference, &cache).await.expect("pull with token");
    assert_eq!(
        std::fs::read(&pulled.path).unwrap(),
        std::fs::read(&wasm).unwrap()
    );
    let _ = std::fs::remove_dir_all(&cache);
}

/// Push a component, then pull it back through the normal path: the
/// round trip must return byte-identical content, with and without the
/// bearer-token dance.
fn push_pull_round_trip(auth: bool) {
    let (Some(wasm), Some(python)) = (artifact(), python()) else {
        eprintln!("skipping: upper.wasm not built or no python");
        return;
    };
    let registry = MockRegistry::start(&python, &wasm, auth);
    let reference = format!("oci://127.0.0.1:{}/test/component:pushed", registry.port);

    let pushed = oci::push(&reference, &wasm).expect("push");
    assert!(pushed.digest.starts_with("sha256:"));
    assert_eq!(pushed.reference, reference);

    let cache = cache();
    let pulled = oci::pull_into(&reference, &cache).expect("pull pushed component");
    assert_eq!(pulled.digest, pushed.digest);
    assert_eq!(
        std::fs::read(&pulled.path).unwrap(),
        std::fs::read(&wasm).unwrap()
    );
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn push_pull_round_trip_anonymous() {
    push_pull_round_trip(false);
}

#[test]
fn push_pull_round_trip_behind_the_token_dance() {
    push_pull_round_trip(true);
}

#[test]
fn push_to_a_digest_reference_is_an_error() {
    let wasm = std::env::temp_dir().join("tau-oci-push-digest.wasm");
    std::fs::write(&wasm, b"wasm").unwrap();
    let result = oci::push(
        &format!(
            "oci://127.0.0.1:1/test/component@sha256:{}",
            "00".repeat(32)
        ),
        &wasm,
    );
    assert!(matches!(result, Err(oci::OciError::BadReference(_))));
}

#[test]
fn unknown_digest_is_an_error() {
    let Some(python) = python() else {
        eprintln!("skipping: no python");
        return;
    };
    // Serve a blob whose digest in the manifest will not match: the mock
    // computes the digest over the actual bytes, so corrupt in transit is
    // not possible — instead verify the check fires when the reference
    // digest and blob disagree by pulling a digest-pinned reference whose
    // digest does not exist.
    let wasm = artifact().unwrap_or_else(|| {
        // Any bytes will do for a 404 check.
        let path = std::env::temp_dir().join("tau-oci-corrupt.bin");
        std::fs::write(&path, b"not wasm").unwrap();
        path
    });
    let registry = MockRegistry::start(&python, &wasm, false);
    let bogus = format!(
        "oci://127.0.0.1:{}/test/component@sha256:{}",
        registry.port,
        "00".repeat(32)
    );
    let cache = cache();
    // Plain sync test: no tokio runtime thread, so reqwest blocking is
    // fine right here.
    let result = oci::pull_into(&bogus, &cache);
    assert!(result.is_err());
    let _ = std::fs::remove_dir_all(&cache);
}

#[tokio::test]
async fn corrupt_cache_entry_is_verified_and_re_pulled() {
    // The cache is content-addressed: a hit must still verify. A
    // corrupted entry (torn write, disk rot, tampering) self-heals by
    // re-pulling instead of handing bad bytes to the load path.
    let (Some(wasm), Some(python)) = (artifact(), python()) else {
        eprintln!("skipping: upper.wasm not built or no python");
        return;
    };
    let registry = MockRegistry::start(&python, &wasm, false);
    let cache = cache();
    let reference = format!("oci://127.0.0.1:{}/test/component:latest", registry.port);

    let pulled = pull(&reference, &cache).await.expect("pull");
    let good = std::fs::read(&pulled.path).unwrap();

    // Corrupt the cache entry; the next pull must notice and re-pull.
    std::fs::write(&pulled.path, b"torn").unwrap();
    let healed = pull(&reference, &cache).await.expect("re-pull");
    assert_eq!(healed.path, pulled.path);
    assert_eq!(std::fs::read(&healed.path).unwrap(), good);

    // And the healed entry loads through the normal path again.
    let host = ExtensionHost::new();
    let extension = host.load(&healed.path).expect("load healed component");
    let (tools, _) = extension.into_parts();
    let out = tools[0]
        .execute(serde_json::json!({ "text": "healed" }))
        .await;
    assert_eq!(out.text(), "HEALED");

    // Atomic writes leave no temp-file litter in the cache.
    let litter: Vec<_> = std::fs::read_dir(&cache)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".tmp-"))
        .collect();
    assert!(litter.is_empty(), "temp litter: {litter:?}");

    let _ = std::fs::remove_dir_all(&cache);
}
