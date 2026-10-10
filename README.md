# tau

Minimal agent harness in Rust. Design follows the pi agent harness
(MIT, earendil-works/pi): session-as-tree, minimal agent loop, everything
extensible — but extensions are **wasm components**, not in-process scripts.

- **Session**: append-only JSONL tree; entries have id + parent; the active
  branch supplies model history; fork = continue from an earlier entry
  (`--continue-from <id>`, `/fork` in the REPL, `tau tree` to see the shape).
- **Agent loop**: prompt → model stream → tool calls → results → repeat.
  Steering/compaction/navigation: see `docs/probes.md` (all nine probe
  points are wired). Compaction condenses the branch into a summary
  entry (`--compact`, `/compact` in the interactive REPL); the originals
  stay in the tree, pi-style.
- **Tools**: eight built-in native tools — `read`, `write`, `edit`, `ls`,
  `grep`, `find`, `bash`, `powershell` — so a run can work on a repository
  with nothing installed first. They are host code (the wasm sandbox does
  not apply to them): `--tools read,grep` narrows the set,
  `--no-builtin-tools` hands the toolset to the components, and a
  component tool with the same name shadows the built-in. See
  `docs/builtin-tools.md`.
- **Skills and project instructions**: `.agents/skills`, `.claude/skills` and
  `.goose/skills` under the working directory are discovered, put one
  `name: description` line each into the system prompt, and loaded only when
  a task calls for one — bodies never ride along — through the `load_skill`
  tool, which also reads a skill's supporting files and refuses any path that
  leaves the skill directory. `AGENTS.md`, from the working directory up to
  the repository root, goes into the system prompt root-first. See
  `docs/skills.md`.
- **Extensions**: drop a `.wasm` in. Components implement the
  `tau:extension` WIT world (`wit/tau.wit`): `tools` (contribute agent
  tools), `probes` (observe and influence the run), and the `host`
  channel back into the harness (notifications and facts always; session
  injection needs no flag — installing the component is the
  authorization).
  Ambient WASI (fs/env/stdio/args/network) is granted, always, and a
  component runs with the permissions of the tau process: 0.8.0 deleted
  `--deny-wasi` and every per-capability gate. Signing answers *which*
  component this is, never *what it may do* — a real boundary is an
  OS-level one around the tau process. The `bridge` world is the only
  world that imports `process`/`http`/`ws`/`ingress`.
  **Writing one? `docs/extensions.md` is the author guide (the API surface,
  the two worlds); `docs/tutorial.md` walks the same path hands-on —
  scaffold → sign → OCI, every command with its output.**
- **Signing**: components must carry an embedded ed25519 signature from a
  trusted key (`tau keygen` / `tau sign` / `tau trust`); `--allow-unsigned`
  is the explicit dev escape. See `docs/signing.md`.
- **Media**: bytes in the model, base64 only at JSON edges; media past
  256KB is externalized to a content-addressed blob store at session
  write and materialized back at the request edge; `tau gc` sweeps
  unreferenced blobs (dry-run by default). See `docs/media.md`.
- **Distribution**: components push to and pull from any OCI registry
  (`tau push ext.wasm oci://ghcr.io/org/ext:tag`, then `-e oci://…` to
  load), content-addressed cache, digest-verified; signature/trust
  apply to pulled bytes unchanged. See `docs/oci.md`.
- **Bridges**: no MCP in core. External tool protocols (MCP) are translated
  by bridge components — the only world that imports spawn-with-pipes,
  http/ws and ingress. The host configures those
  (`--mcp-bridge b.wasm --mcp-command '["python","server.py"]'`); since
  0.8.0 it does not gate them at call time. See `docs/bridges.md`.
- **Models**: the model set is closed and host-owned — three built-in
  APIs, OpenAI chat completions (`--provider openai`), OpenAI Responses
  (`--provider responses`) and Anthropic Messages (`--provider anthropic`),
  each pointed at a configurable base URL through the environment
  (`OPENAI_BASE_URL` takes any OpenAI-compatible gateway;
  `ANTHROPIC_BASE_URL` for Messages). Since 0.8.0 no
  component is a model: `world provider` and `world realtime` are gone,
  and a vendor tau does not ship has no component path — coverage is
  release cadence. Messages are
  multimodal: text, image, audio, video, and file blocks, mapped per API
  (or degraded to placeholders where the API has no equivalent block).
- **Realtime voice**: the media plane is host-internal end to end (0.8.0):
  device I/O, the clock, the jitter buffer and playback belong to the
  host, and no component sits on the audio path — the `realtime` world
  and the `--microphone` consent went with it. Try the REPL's
  `/live N [sine]` against the built-in demo; downlink audio plays live
  through a lazy playback sink (text-only sessions never touch the audio
  device), a real microphone needs only the user typing the command, and
  the synthetic `sine` uplink needs no grant. See `docs/realtime-av.md`.
- **Editor-attached mode**: `tau --acp` speaks the Agent Client Protocol
  (JSON-RPC over stdin/stdout) to Zed and anything else that hosts ACP
  agents — native, no adapter and no second binary. One process serves
  any number of sessions, each with its own session file; stdout carries
  the protocol and nothing else. The four built-ins that change the
  machine (`write`, `edit`, `bash`, `powershell`) ask the client first
  through `session/request_permission` — the only place tau has a
  permission gate, because it is the only place with a human on the
  other end (outside it, the built-ins are host code; see
  `docs/builtin-tools.md`). See `docs/acp.md`.

## Try it

```bash
cargo install tau-cli --locked
# from a checkout:
cargo install --path crates/tau-cli --locked   # provides `tau`
tau --demo -p "hello"
# or without installing:
cargo run -p tau-cli -- --demo -p "hello"

# with a real endpoint:
export OPENAI_API_KEY=... OPENAI_BASE_URL=... TAU_MODEL=...
cargo run -p tau-cli -- -p "hello"

# interactive mode (terminal, no -p):
cargo run -p tau-cli -- --demo

# as an editor's agent (Agent Client Protocol on stdin/stdout):
cargo run -p tau-cli -- --acp

# build the example extension, then use it (unsigned -> dev escape, or sign
# it: `tau keygen` once, then `tau sign <file>.wasm`):
cargo build --manifest-path examples/upper/Cargo.toml --target wasm32-wasip2 --release
cargo run -p tau-cli -- --allow-unsigned \
  -e examples/upper/target/wasm32-wasip2/release/upper.wasm \
  --demo -p "shout hello tau"
```

The demo with the extension loaded shows the whole agent loop — call,
result, answer citing the result:

```
[tau] built-in tools: bash, edit, find, grep, ls, powershell, read, write
[tau] loaded extension: upper
[tau]   tool: upper
[tau] tool → upper
[tau] tool ← upper: SHOUT HELLO TAU
tau is alive. The tool answered: SHOUT HELLO TAU. (faux model — …)
```

## Releases

See `CHANGELOG.md` for what's in each version. `scripts/validate.sh`
proves the release candidate the way a first user meets it (demo,
built-in tools, skills discovery, an ACP client over real pipes,
signing/trust chain, the built-in providers against a loopback mock,
the wasm host channel)
and restores the environment afterwards. `scripts/release.sh` runs the full suite, rebuilds the wasm
examples, builds the release binary (lto + strip), and assembles
`dist/tau-<version>-<target>.zip` with the binary, README, LICENSE, docs/,
and prebuilt (unsigned) example components.

## Layout

| path | role |
|------|------|
| `crates/tau-core` | domain model, session tree, agent loop, probe registry, faux model |
| `crates/tau-openai` | OpenAI chat completions + Responses API providers |
| `crates/tau-anthropic` | Anthropic Messages API provider |
| `crates/tau-ext` | wasmtime component host (ambient WASI, two worlds) |
| `crates/tau-tools` | built-in native tools (read/write/edit/ls/grep/find/bash/powershell) |
| `crates/tau-cli` | `tau` binary (print, interactive, and `--acp` modes) |
| `wit/tau.wit` | the extension contract, versioned |
| `docs/` | architecture (the design doc), extensions (author guide), tutorial (hands-on walkthrough), builtin-tools, skills, acp, probes, events, bridges, signing, oci, media, wasip3-streams, release, perf |
| `examples/upper` | example wasm extension (tool) |
| `examples/mcp-bridge` | example MCP bridge (stdio + streamable HTTP) |
| `examples/guard` | example probe extension (`before_tool` block verdicts) |

## Interactive mode

`tau` with no `-p` on a terminal starts a scrollback REPL (rustyline line
editing, persisted history): streamed answers print inline while you can
keep typing. Mid-run input is the control plane — plain text queues as a
follow-up, `!text` steers after the current turn, Ctrl-C aborts, `/quit`
exits. Idle commands: `/compact` (summarize history into a compaction
entry), `/fork [id|#index]` (rewind to an earlier entry and branch from
there), `/live N [sine]` (N-second full-duplex voice session against a
realtime-capable model — the built-in demo today; Ctrl-C during it is a
barge-in interrupt, not
an abort), `/help`. `tau -p "..."` stays one-shot print mode.

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
