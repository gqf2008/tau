//! Component load-time baseline for docs/perf.md. The gate half runs by
//! default (cache populates, loads stay correct); the timing half is
//! ignored and run on demand:
//!
//!   cargo test -p tau-ext --test perf_load -- --ignored --nocapture

use std::path::PathBuf;
use std::time::Instant;

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
        .join("target/wasm32-wasip2/release")
        .join(format!("{}.wasm", name.replace('-', "_")))
}

#[test]
fn compile_cache_populates_and_loads_stay_correct() {
    let upper = example("upper");
    if !upper.is_file() {
        eprintln!("skipping: build examples/upper first (wasm32-wasip2)");
        return;
    }
    let host = tau_ext::ExtensionHost::new();
    let ext = host.load(&upper).expect("load upper");
    let (tools, _) = ext.into_parts();
    assert_eq!(tools.len(), 1);
    // The whole point: the cache is actually active, not silently
    // disabled — a module entry must appear under the cache dir.
    let dir = tau_ext::compile_cache_dir();
    let populated = std::fs::read_dir(&dir)
        .map(|rd| rd.flatten().count() > 0)
        .unwrap_or(false);
    assert!(
        populated,
        "compile cache did not populate {}",
        dir.display()
    );
}

#[test]
#[ignore = "perf baseline, run on demand"]
fn component_load_times() {
    let upper = example("upper");
    if !upper.is_file() {
        eprintln!("skipping: build examples/upper first (wasm32-wasip2)");
        return;
    }
    // A fresh ExtensionHost per round = a fresh Engine; only the on-disk
    // compile cache carries over. Round 0 is cold, the rest are warm.
    for round in 0..5 {
        let host = tau_ext::ExtensionHost::new();
        let start = Instant::now();
        let ext = host.load(&upper).expect("load upper");
        println!(
            "round {round}: load upper in {:?} (extension: {})",
            start.elapsed(),
            ext.name
        );
    }
}
