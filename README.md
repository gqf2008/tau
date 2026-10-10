# tau

Minimal agent harness in Rust, following the pi agent harness (MIT, earendil-works/pi) with three decisions that shape everything else:

- **Session-as-tree**: append-only JSONL; the active branch is the model history; continuing from an old entry is a fork.
- **Minimal loop, maximal extension**: prompt → model stream → tool calls → repeat; tools, probes and bridges live in extensions.
- **Extensions are wasm components**, not in-process scripts: fault isolation, language neutrality, one signable OCI-pushable `.wasm`. A component runs with the permissions of the tau process — the real boundary is an OS-level one around tau (`docs/extensions.md` §7).

Development lives in **walgit, the source of truth** — branches and the collaboration ledger (`refs/collab/*`) are there; github.com/gqf2008/tau is a public mirror.

## Where to read

| doc | covers |
|-----|--------|
| `docs/architecture.md` | the design doc: tree, loop, REPL (Chinese) |
| `docs/tutorial.md` + `docs/extensions.md` | hands-on walkthrough; extension author guide |
| `docs/builtin-tools.md` + `docs/skills.md` | the eight native tools; skills + `AGENTS.md` |
| `docs/probes.md` + `docs/events.md` | the nine probe points; the event bus |
| `docs/signing.md` + `docs/oci.md` + `docs/bridges.md` | signing and trust; OCI distribution; MCP bridges |
| `docs/media.md` + `docs/acp.md` + `docs/realtime-av.md` | media and blob store; editor mode (ACP); realtime voice |
| `CHANGELOG.md` + `docs/release.md` | per-release notes; the release process |

## Try it

```bash
cargo install tau-cli --locked
cargo install --path crates/tau-cli --locked   # from a checkout; provides `tau`
tau --demo -p "hello"
cargo run -p tau-cli -- --demo -p "hello"      # without installing
cargo run -p tau-cli -- --demo                 # interactive REPL
cargo run -p tau-cli -- --acp                  # editor's agent (Agent Client Protocol)
cargo build --manifest-path examples/upper/Cargo.toml --target wasm32-wasip2 --release
# example extension, unsigned -> dev escape:
cargo run -p tau-cli -- --allow-unsigned \
  -e examples/upper/target/wasm32-wasip2/release/upper.wasm \
  --demo -p "shout hello tau"
```

With a real endpoint: `export OPENAI_API_KEY=... OPENAI_BASE_URL=... TAU_MODEL=...`, then
`cargo run -p tau-cli -- -p "hello"`.

## Testing and releases

`cargo test --workspace` is fully offline; real-provider smoke tests are opt-in
(`TAU_SMOKE=1 cargo test -p tau-anthropic --test smoke` with `ANTHROPIC_API_KEY` or
`ANTHROPIC_AUTH_TOKEN`, likewise `-p tau-openai` with `OPENAI_API_KEY`). `scripts/validate.sh`
proves a candidate the way a first user meets it; `scripts/release.sh` builds the release
binary and assembles `dist/tau-<version>-<target>.zip` — process in `docs/release.md`.

## Layout

`crates/tau-core` (session tree, agent loop), `tau-openai` / `tau-anthropic` (the built-in
providers), `tau-ext` (wasmtime host, two worlds), `tau-tools` (native tools), `tau-cli`
(the `tau` binary). `wit/tau.wit` is the versioned contract. Contracts worth reading first:
`tau-core/src/model.rs` (never panic — errors are terminal stream events),
`tau-core/src/session.rs` (the tree model), `docs/probes.md`, `docs/events.md`.
