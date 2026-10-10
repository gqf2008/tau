//! Example tau extension: a probe that actually decides. Contributes no
//! tools; its `before_tool` probe blocks any call whose arguments carry
//! the word "forbidden" — the block reason goes back to the model as
//! the tool result, so the run continues instead of dying. Arguments
//! carrying "crash" make the probe panic instead: the host degrades a
//! broken probe to continue, so the call still goes through. Arguments
//! carrying "wasicheck" make the probe read the ambient environment:
//! since 0.8.0 there is no WASI gate at all — every component inherits
//! the host's env and filesystem (installing/trusting signed bytes is the
//! one authorization act), so a probe that can read the host's
//! TAU_AMBIENT blocks: the ambient reach is unconditional, by design.
//! Arguments carrying "fscheck" do the same for ambient filesystem
//! preopens: `/` (unix) or each drive as `/<letter>` (Windows) is
//! always preopened, so a probe that can list any of them blocks. It also
//! registers the observe-only `session_start` point and reports the
//! session via `host.notify` — observe leg (probes.md), verdict ignored
//! by contract.
//!
//! Build:
//!   cargo build --manifest-path examples/guard/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use (with any tool extension, e.g. upper):
//!   tau --allow-unsigned \
//!     -e examples/upper/target/wasm32-wasip2/release/upper.wasm \
//!     -e examples/guard/target/wasm32-wasip2/release/guard.wasm \
//!     --demo -p "shout forbidden"
//!   # → [tau] tool ← upper (error): blocked: the guard said no

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "extension",
});

use exports::tau::extension::probes::{Guest as Probes, Payload, Point, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::host::{self, Level};
use tau::extension::types::{Content, Error as HostError, ResultBlock, ToolCall};

/// A host error as one line: the typed kind picks the wording (the contract
/// says the variant is what a guest branches on), the detail is the host's
/// own sentence handed through verbatim. `{e}` would print the Debug form
/// of the variant, which is not a sentence.
fn host_error(verb: &str, error: HostError) -> String {
    match error {
        HostError::Failed(detail) => format!("{verb} failed: {detail}"),
        HostError::Invalid(detail) => format!("{verb} invalid: {detail}"),
    }
}

struct Guard;

impl Tools for Guard {
    async fn definitions() -> Vec<Definition> {
        // This extension only probes; it provides no tools.
        Vec::new()
    }

    async fn execute(name: String, _arguments_json: String) -> ToolResult {
        ToolResult {
            content: vec![ResultBlock::Text(format!(
                "guard provides no tools (called: {name})"
            ))],
            is_error: true,
        }
    }
}

/// The model's arguments object, as the probe sees it. Since 0.7.0 the
/// tool call arrives typed (`types.tool-call`); the JSON leaf is the
/// arguments object itself.
fn argument_text(call: &ToolCall) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(&call.arguments_json).ok()?;
    parsed["text"].as_str().map(str::to_string)
}

impl Probes for Guard {
    fn points() -> Vec<Point> {
        vec![Point::BeforeTool, Point::SessionStart]
    }

    /// The typed payload names its own point (the host guarantees the
    /// pairing), so the old string dispatch on the point collapses into a
    /// match on the payload arm.
    fn probe(point: Point, payload: Payload) -> Verdict {
        match payload {
            Payload::SessionStart(facts) => {
                // Observe-only: report the session through the host
                // channel. The verdict is ignored by contract.
                if let Err(e) = host::notify(
                    Level::Info,
                    &[Content::Text(format!(
                        "session_start: {} (model {})",
                        facts.session, facts.model
                    ))],
                ) {
                    eprintln!("guard: notify failed: {}", host_error("notify", e));
                }
                Verdict::Continue
            }
            Payload::BeforeTool(call) if point == Point::BeforeTool => {
                decide(argument_text(&call).as_deref())
            }
            _ => Verdict::Continue,
        }
    }
}

/// The verdict for one before_tool probe, spelled out so the fixture
/// reads the same as it did on 0.6.0's JSON payload.
fn decide(text: Option<&str>) -> Verdict {
    // A broken probe must degrade to continue, never wedge the run: this
    // panic is the fixture that proves it end to end.
    if text.is_some_and(|t| t.contains("crash")) {
        panic!("the guard blew up");
    }
    // Ambient WASI, observable from inside: read an env var. Every
    // component inherits the host env since 0.8.0 (0.8.0 deleted the WASI
    // gate), so a probe that can read it blocks.
    if text.is_some_and(|t| t.contains("wasicheck")) {
        if std::env::var("TAU_AMBIENT").is_ok() {
            return Verdict::Block("ambient env leaked into the guest".into());
        }
        return Verdict::Continue;
    }
    // Same for the ambient filesystem: `/` (unix) / every drive as
    // `/<letter>` (Windows) is preopened, always. Count how many
    // candidate roots actually list — at least one must. (Regression: a
    // host that preopened drives under mangled guest names failed this
    // check — the preopens existed but at paths the guest never guesses.)
    if text.is_some_and(|t| t.contains("fscheck")) {
        let mut reachable = std::fs::read_dir("/").is_ok() as usize;
        for letter in b'a'..=b'z' {
            if std::fs::read_dir(format!("/{}", letter as char)).is_ok() {
                reachable += 1;
            }
        }
        if reachable > 0 {
            return Verdict::Block(format!(
                "ambient fs leaked into the guest ({reachable} preopens reachable)"
            ));
        }
        return Verdict::Continue;
    }
    if text.is_some_and(|text| text.contains("forbidden")) {
        Verdict::Block("the guard said no".into())
    } else {
        Verdict::Continue
    }
}

export!(Guard);
