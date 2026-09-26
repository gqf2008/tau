# Releasing tau to crates.io

Five publishable crates, one workspace version (`[workspace.package]
version` in the root `Cargo.toml` — bump it once, every crate follows).
0.x semantics: any minor bump may break.

Publish order (each depends on the previous being live on crates.io):

```
tau-core → tau-openai, tau-anthropic → tau-ext → tau-cli
```

## Pre-flight

```bash
cargo test                                # all suites green
cargo clippy --all-targets                # 0 warnings
cargo clippy --manifest-path examples/mcp-bridge/Cargo.toml --target wasm32-wasip2
cargo build --manifest-path examples/<each>/Cargo.toml --target wasm32-wasip2 --release
git status                                # clean tree
scripts/validate.sh                       # first-user validation, 4 steps
```

## Package checks

`cargo package -p tau-core` must pass with the verify build. **Before the
first publish of the whole chain this is the only crate that can pass**:
`cargo package` rewrites path deps to registry deps, and the predecessor
crate is not on crates.io yet, so `tau-openai`/`tau-anthropic`/`tau-ext`/
`tau-cli` fail resolution with "no matching package named `tau-core`" —
expected, not a defect. They validate at real publish time, in order.

`cargo package -p <crate> --list --allow-dirty` shows the tarball
contents; noteworthy inclusions:

- `tau-ext` vendors `wit/tau.wit` (bindgen paths are relative to the
  crate manifest; the canonical copy at the workspace root serves the
  examples). The `wit_vendored` unit test fails the build if the two
  drift — sync the vendored copy when editing the canonical WIT.

## Publish

```bash
cargo login                               # crates.io token, once
cargo publish -p tau-core
cargo publish -p tau-openai
cargo publish -p tau-anthropic
cargo publish -p tau-ext
cargo publish -p tau-cli
git tag v$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
```

Post-publish, verify as a stranger would (README's install line must
work verbatim) and push the tag:

```bash
cargo install tau-cli --locked
scripts/validate.sh
git push origin main --tags
```

If the machine replaces the crates-io source with a mirror (e.g.
`rsproxy-sparse` in `~/.cargo/config.toml`), `cargo publish` refuses
with "crates-io is replaced with non-remote-registry source" — publish
uploads must go to the real registry, so append `--registry crates-io`
to every publish command (and to the `--dry-run` checks). Installing
through a mirror is fine; only publishing is.

Name availability was checked 2026-09-26: all five names were free on
crates.io. If one is taken by publish time, rename is a workspace-wide
change (crate names appear in path deps and doc links) — do not publish
a partial chain.

## Not published

- The wasm example components (`examples/*`, separate workspaces) are
  built from source; their distribution channel is OCI (`tau push`,
  see `docs/oci.md`), not crates.io.
- `repository`/`homepage` metadata is intentionally unset until the repo
  gets a public remote — add both to `[workspace.package]` then.
