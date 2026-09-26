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
  condenses the branch into a summary entry; originals stay. A crash
  mid-append leaves a torn tail that is discarded with a warning —
  never a bricked session — while real corruption refuses with a named
  line.
- **Agent loop**: prompt → model stream → tool calls → results →
  repeat. Mid-run steering (`!text`) and queued follow-ups; Ctrl-C
  aborts through the control channel.
- **Wasm component extensions** (`wit/tau.wit`): tools and probe hooks.
  Nine wired probe points (`before_run` … `before_navigation`) with
  continue/replace/block verdicts; a trapped probe degrades to continue
  and the guest instance is rebuilt, so later probes still decide —
  a broken probe never wedges the run and never goes silently dead.
  Ambient WASI by default, `--deny-wasi` restores the sandbox.- **Signing & consent**: embedded ed25519 signature sections, a trust
  store, per-fingerprint remembered capability grants (bridge argv,
  HTTP origins, credential delivery, WASI-deny). Secrets are delivered,
  never persisted. `tau trust --from-component` onboards keys from
  verified bytes. `--allow-unsigned` excuses only *absent* signatures —
  a signature section that does not verify is refused under every
  policy, so the escape hatch cannot launder tampered bytes.
- **Distribution**: push/pull components through any OCI registry;
  digest-addressed cache that verifies hits and re-pulls a corrupted
  entry instead of handing bad bytes to the load path;
  signature/trust/consent apply to pulled bytes unchanged.
  The dist zip ships the example components unsigned (`release.sh`
  strips any local dev signature), so a first user meets the
  documented sign-and-trust onboarding, not a foreign key.
- **Models**: built-in OpenAI chat completions, OpenAI Responses, and
  Anthropic Messages providers; wasm provider components with push-mode
  streaming and consent-gated HTTP egress — a trapped provider fails
  its run and the instance is rebuilt, so one crash never fails the
  rest of the session. Multimodal messages (text,
  image, audio, video, file), media >256KB externalized to a
  content-addressed blob store with `tau gc`. Blob writes are atomic
  and reads verify the hash: a corrupt blob degrades to a placeholder,
  wrong bytes are never served to the model.
- **MCP without MCP in core**: external protocols are translated by
  bridge components over consent-gated spawn/http capabilities; the
  reference bridge speaks stdio + streamable HTTP with protocol-version
  negotiation, and reconnects (respawn + re-handshake) after a
  mid-session server death instead of erroring on the dead pipe
  forever.

### Install & try

```bash
cargo install tau-cli --locked   # provides `tau`
tau --demo -p "hello"            # no API key needed
```

Then: `docs/extensions.md` to write an extension, `README.md` for the
full tour.

### Validation

`scripts/validate.sh` proves the release the way a first user meets it,
in ten steps: demo, the signing/trust chain (trusted load; untrusted,
byte-flipped, signature-stripped, and corrupted-signature rejection),
all three built-in providers
against a loopback mock, wasm-provider consent gate, MCP bridge spawn
gate, remembered-consent lifecycle, credential delivery (the token
reaches the origin through the guest; consent and session files never
persist the secret; TAU_PROVIDER_AUTH flows only with the grant),
OCI push/pull/trust onboarding,
blob GC, compaction (summary entry; originals stay; follow-ups run on
the compacted branch), torn-tail recovery, concurrent access, and
probe verdicts (block, continue, and a trapped probe degrading without
going dead), and the WASI sandbox boundary (ambient env visible by
default, empty under --deny-wasi) — restoring
the environment exactly afterwards. 108 tests, clippy-clean across all
workspaces.

Performance baseline (docs/perf.md): extension load 202ms cold → 9ms
warm (wasmtime compile cache); 10k-entry session opens in 66ms.

### Known limitations

- Publishing and the git remote are pending; `repository` metadata
  arrives with the public repo.
- wasip3-style stream ABI for large payloads is not in this release.
- The interactive REPL is deliberately simple (scrollback + rustyline),
  no alternate screen.
- Concurrent tau processes on one session file are structurally safe —
  the append-only tree turns them into implicit branches, never
  corruption — but each process sees only its own writes until reopen.
  Run `tau gc` only on idle sessions: a blob written a moment before
  its referencing entry could look orphaned to a racing sweep.
