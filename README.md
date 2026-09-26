# tau

Minimal agent harness in Rust. Design follows the pi agent harness
(MIT, earendil-works/pi): session-as-tree, minimal agent loop, everything
extensible — but extensions are **wasm components**, not in-process scripts.

- **Session**: append-only JSONL tree; entries have id + parent; the active
  branch supplies model history; fork = continue from an earlier entry
  (`--continue-from <id>`, `/fork` in the REPL, `tau tree` to see the shape).
- **Agent loop**: prompt → model stream → tool calls → results → repeat.
  Steering/compaction/navigation: see `docs/probes.md` (v0 wires the
  run-critical probes). Compaction condenses the branch into a summary
  entry (`--compact`, `/compact` in the interactive REPL); the originals
  stay in the tree, pi-style.
- **Extensions**: drop a `.wasm` in. Components implement the
  `tau:extension` WIT world (`wit/tau.wit`): `tools` (contribute agent
  tools) and `hooks` (probes that observe and influence the run).
  Sandboxed by default — the world exports no capabilities, so a component
  that imports fs/net/env fails instantiation.
- **Signing**: components must carry an embedded ed25519 signature from a
  trusted key (`tau keygen` / `tau sign` / `tau trust`); `--allow-unsigned`
  is the explicit dev escape. See `docs/signing.md`.
- **Media**: bytes in the model, base64 only at JSON edges; media past
  256KB is externalized to a content-addressed blob store at session
  write and materialized back at the request edge. See `docs/media.md`.
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
  (`--provider anthropic`); plus wasm provider components whose HTTP egress
  is consent-gated per signing fingerprint (`--provider-origin`, remembered
  with `--remember`)
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

# interactive mode (terminal, no -p):
cargo run -p tau-cli -- --demo

# build the example extension, then use it (unsigned -> dev escape, or sign
# it: `tau keygen` once, then `tau sign <file>.wasm`):
cargo build --manifest-path examples/upper/Cargo.toml --target wasm32-wasip2 --release
cargo run -p tau-cli -- --allow-unsigned \
  -e examples/upper/target/wasm32-wasip2/release/upper.wasm \
  -p "shout 'hello tau' using the upper tool"
```

## Releases

`scripts/release.sh` runs the full suite, rebuilds the wasm examples,
builds the release binary (lto + strip), and assembles
`dist/tau-<version>-<target>.zip` with the binary, README, LICENSE, docs/,
and prebuilt (unsigned) example components.

## Layout

| path | role |
|------|------|
| `crates/tau-core` | domain model, session tree, agent loop, probe registry, faux model |
| `crates/tau-openai` | OpenAI chat completions + Responses API providers |
| `crates/tau-anthropic` | Anthropic Messages API provider |
| `crates/tau-ext` | wasmtime component host (sandboxed) |
| `crates/tau-cli` | `tau` binary (print + interactive modes) |
| `wit/tau.wit` | the extension contract, versioned |
| `docs/` | probes, events, bridges, signing, oci, media |
| `examples/upper` | example wasm extension (tool) |
| `examples/echo-provider` | example wasm provider (push-mode streaming) |
| `examples/http-provider` | example wasm provider (consent-gated http) |
| `examples/mcp-bridge` | example MCP bridge (stdio + streamable HTTP) |

## Interactive mode

`tau` with no `-p` on a terminal starts a scrollback REPL (rustyline line
editing, persisted history): streamed answers print inline while you can
keep typing. Mid-run input is the control plane — plain text queues as a
follow-up, `!text` steers after the current turn, Ctrl-C aborts, `/quit`
exits. `tau -p "..."` stays one-shot print mode.

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
