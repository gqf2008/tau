# tau

Minimal agent harness in Rust. Design follows the pi agent harness
(MIT, earendil-works/pi): session-as-tree, minimal agent loop, everything
extensible — but extensions are **wasm components**, not in-process scripts.

- **Session**: append-only JSONL tree; entries have id + parent; the active
  branch supplies model history; fork = continue from an earlier entry.
- **Agent loop**: prompt → model stream → tool calls → results → repeat.
  Steering/compaction/navigation: see `docs/probes.md` (v0 wires the
  run-critical probes).
- **Extensions**: drop a `.wasm` in. Components implement the
  `tau:extension` WIT world (`wit/tau.wit`): `tools` (contribute agent
  tools) and `hooks` (probes that observe and influence the run).
  Sandboxed by default — the world exports no capabilities, so a component
  that imports fs/net/env fails instantiation.
- **Signing**: components must carry an embedded ed25519 signature from a
  trusted key (`tau keygen` / `tau sign` / `tau trust`); `--allow-unsigned`
  is the explicit dev escape. See `docs/signing.md`.
- **Distribution**: components pull from any OCI registry
  (`oci://ghcr.io/org/ext:tag`), content-addressed cache, digest-verified;
  signature/trust/consent apply to pulled bytes unchanged. Pull-only —
  publishing is `oras`/`crane`'s job. See `docs/oci.md`.
- **Bridges**: no MCP in core. External tool protocols (MCP) are translated
  by bridge components over a consent-gated spawn-with-pipes capability
  (`--mcp-bridge b.wasm --mcp-command '["python","server.py"]'`).
  See `docs/bridges.md`.
- **Models**: three built-in APIs — OpenAI chat completions (`--provider openai`),
  OpenAI Responses (`--provider responses`), Anthropic Messages
  (`--provider anthropic`); plus wasm provider components
  (`--provider-wasm x.wasm --model id`) pushing stream events through the
  `events.emit` host channel. Messages are multimodal: text, image, audio,
  video, and file blocks, mapped per API (or degraded to placeholders where
  the API has no equivalent block).

## Try it

```bash
cargo run -p tau-cli -- --demo -p "hello"

# with a real endpoint:
export OPENAI_API_KEY=... OPENAI_BASE_URL=... TAU_MODEL=...
cargo run -p tau-cli -- -p "hello"

# build the example extension, then use it:
cargo build --manifest-path examples/upper/Cargo.toml --target wasm32-wasip2 --release
cargo run -p tau-cli -- -e examples/upper/target/wasm32-wasip2/release/upper.wasm \
  -p "shout 'hello tau' using the upper tool"
```

## Layout

| path | role |
|------|------|
| `crates/tau-core` | domain model, session tree, agent loop, probe registry, faux model |
| `crates/tau-openai` | OpenAI chat completions + Responses API providers |
| `crates/tau-anthropic` | Anthropic Messages API provider |
| `crates/tau-ext` | wasmtime component host (sandboxed) |
| `crates/tau-cli` | `tau` binary (print mode) |
| `wit/tau.wit` | the extension contract, versioned |
| `docs/probes.md` | lifecycle probe points and verdict semantics |
| `examples/upper` | example wasm extension (tool) |
| `examples/echo-provider` | example wasm provider (push-mode streaming) |

## Testing

`cargo test --workspace` is fully offline. Real-provider smoke tests are
opt-in: `TAU_SMOKE=1 cargo test -p tau-anthropic --test smoke` (needs
`ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`; `TAU_MODEL` to pick the
model) and likewise `-p tau-openai` with `OPENAI_API_KEY`.

## Contracts worth reading first

- `crates/tau-core/src/model.rs` — the `Model` trait: never panic, errors
  travel as terminal stream events (pi's `StreamFn` contract).
- `crates/tau-core/src/session.rs` — the tree model.
- `docs/probes.md` — where extensions can influence a run.
- `docs/events.md` — the event-bus spine.
