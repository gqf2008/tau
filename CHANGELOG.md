# Changelog

## [0.1.0] — 2026-09-27

First public release. tau is a minimal agent harness in Rust, designed
after the pi agent harness (MIT, earendil-works/pi): session-as-tree,
a minimal agent loop, everything extensible — with wasm components as
the extension unit instead of in-process scripts.

0.x semantics: any minor bump may break.

### Highlights

- **Session as a tree**: append-only JSONL, entries with id + parent;
  the active branch supplies model history; fork by continuing from any
  earlier entry (`/fork`, `--continue-from`, `tau tree`). Compaction
  condenses the branch into a summary entry; originals stay.
- **Agent loop**: prompt → model stream → tool calls → results →
  repeat. Mid-run steering (`!text`) and queued follow-ups; Ctrl-C
  aborts through the control channel.
- **Wasm component extensions** (`wit/tau.wit`): tools and probe hooks.
  Nine wired probe points (`before_run` … `before_navigation`) with
  continue/replace/block verdicts; a broken probe degrades to continue
  instead of wedging the run. Ambient WASI by default, `--deny-wasi`
  restores the sandbox.
- **Signing & consent**: embedded ed25519 signature sections, a trust
  store, per-fingerprint remembered capability grants (bridge argv,
  HTTP origins, credential delivery, WASI-deny). Secrets are delivered,
  never persisted. `tau trust --from-component` onboards keys from
  verified bytes.
- **Distribution**: push/pull components through any OCI registry;
  digest-addressed cache; signature/trust/consent apply to pulled bytes
  unchanged.
- **Models**: built-in OpenAI chat completions, OpenAI Responses, and
  Anthropic Messages providers; wasm provider components with push-mode
  streaming and consent-gated HTTP egress. Multimodal messages (text,
  image, audio, video, file), media >256KB externalized to a
  content-addressed blob store with `tau gc`.
- **MCP without MCP in core**: external protocols are translated by
  bridge components over consent-gated spawn/http capabilities; the
  reference bridge speaks stdio + streamable HTTP with protocol-version
  negotiation.

### Install & try

```bash
cargo install tau-cli --locked   # provides `tau`
tau --demo -p "hello"            # no API key needed
```

Then: `docs/extensions.md` to write an extension, `README.md` for the
full tour.

### Validation

`scripts/validate.sh` proves the release the way a first user meets it,
in nine steps: demo, the signing/trust chain (trusted load; untrusted,
byte-flipped, signature-stripped, and corrupted-signature rejection),
all three built-in providers
against a loopback mock, wasm-provider consent gate, MCP bridge spawn
gate, remembered-consent lifecycle, OCI push/pull/trust onboarding,
blob GC, and probe verdicts — restoring the environment exactly
afterwards. 97 tests, clippy-clean across all workspaces.

Performance baseline (docs/perf.md): extension load 202ms cold → 9ms
warm (wasmtime compile cache); 10k-entry session opens in 66ms.

### Known limitations

- Publishing and the git remote are pending; `repository` metadata
  arrives with the public repo.
- wasip3-style stream ABI for large payloads is not in this release.
- The interactive REPL is deliberately simple (scrollback + rustyline),
  no alternate screen.
