# Releasing tau to crates.io

Five publishable crates, one workspace version (`[workspace.package]
version` in the root `Cargo.toml`). Bump it once, then also bump the
`version = "x.y.z"` pins on the inter-crate path deps in
`crates/*/Cargo.toml` (tau-core/tau-openai/tau-anthropic/tau-ext edges) —
cargo refuses to resolve otherwise. `cargo check --workspace` then
refreshes the lockfile.
0.x semantics: any minor bump may break.

Publish order (each depends on the previous being live on crates.io):

```
tau-core → tau-openai, tau-anthropic → tau-ext → tau-cli
```

## Pre-flight

```bash
cargo clippy --all-targets                # 0 warnings (see the replay note)
cargo clippy --manifest-path examples/mcp-bridge/Cargo.toml --target wasm32-wasip2
cargo test                                # all suites green
cargo build --manifest-path examples/<each>/Cargo.toml --target wasm32-wasip2 --release
git status                                # clean tree
scripts/validate.sh                       # first-user validation, 11 steps
SKIP_TESTS=1 scripts/release.sh           # dist zip assembles; smoke the
                                          # packaged binary + examples once
```

**A clippy run after `cargo test` can be a cache replay.** `cargo clippy`
shares the check-profile fingerprints with `cargo check` / `cargo test`, so
when the suites ran first a clippy pass may print only `Finished` — no
`Checking <crate>` lines — and report no lints for code it never re-linted.
That green proves nothing. The pre-flight list above therefore runs clippy *before* the suites; whenever you run it later anyway, force the rebuild and watch for the lines:

```bash
touch crates/*/src/lib.rs            # or use a separate CARGO_TARGET_DIR
cargo clippy --workspace --all-targets   # expect Checking <crate> per member
```

The examples are not workspace members: the ones with code changes need their
own run (`cargo clippy --manifest-path examples/<ex>/Cargo.toml
--target wasm32-wasip2`), and they cache the same way. Hit on the 0.4.0 cut:
two consecutive clippy runs finished in ~0.5s with zero `Checking` lines.

**Contract bumps: build the fixtures before the suites.** The tau-ext unit
tests load example artifacts (`echo-provider`, `upper`, `guard`, …) straight
from `examples/*/target/wasm32-wasip2/release`. After a WIT version bump a
stale artifact fails the test as a version mismatch ("this host requires
@x.y.z") — which reads like a code defect but is just an old fixture.
`scripts/validate.sh` builds them all — its pre-flight covers the shipped
examples, and `bad-schema` (deliberately not shippable) is rebuilt
unconditionally at its own step, because an existence guard there used to
reuse the previous contract's artifact. So on a contract bump run
validate.sh first and `cargo test` second; the 0.4.0 bump hit exactly this
with `echo-provider` and `bad-schema`.

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
- `repository`/`homepage` metadata is set (github.com/gqf2008/tau,
  public since 2026-09).
