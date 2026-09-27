# Changelog

## [Unreleased]

### Breaking — contract `tau:extension@0.2.0` (docs/wit-review.md batch)

- **Host channel** (`docs/host-channel.md`, landed): the extension
  world imports `host` — `notify(level, content)` (user-visible notice,
  rendered, never model history), `emit(event-json)` (extension fact on
  the bus), `steer/follow-up(message)` (enqueue into the control
  channel, applied at the loop's existing checkpoints; consent-gated by
  `--allow-inject`, remembered per fingerprint as the `inject` grant).
  Message payloads use the new typed trunk (`types` interface:
  message/content/media/tool-call/tool-result); media crosses the ABI
  as raw bytes, never base64. A typed↔serde conversion layer in tau-ext
  is pinned by round-trip tests.
- `hooks` interface renamed `probes`, matching the code, docs, and CLI.
- `events.emit` is typed (`model-event` variant — the audio-delta
  base64 hot path is gone) and returns `result<_, string>`; the 0.1.0
  malformed-frame silent-skip path is gone with the envelope.
- `process.kill` returns `result<_, string>`; bridge process handles
  carry a generation, so a stale handle after a trap-rebuild errors
  instead of aliasing a new child.
- Load errors name contract-version mismatches ("component targets
  tau:extension@0.1.0; this host requires @0.2.0 — rebuild…").
- A `parameters-json` that does not parse **fails the whole load**,
  naming the tool (extension and bridge paths alike); the 0.1.0 silent
  fallback to an open `{"type": "object"}` schema is gone
  (wit-review F5).
- Components built against 0.1.0 must be rebuilt; no dual-version
  linking (0.x semantics; docs/host-channel.md 兼容性).

### Added

- `AgentEvent::ExtensionNotice` / `AgentEvent::ExtensionFact` on the
  bus; `Agent::bus()` exposes the sending half for composition layers.
- `examples/notifier`: host-channel demo — one `poke` tool exercises
  notify + emit + steer, with the consent refusal visible in the tool
  result. Acceptance: `tau --demo -e notifier.wasm -p "hello"` shows
  "steer refused: session injection not consented"; with
  `--allow-inject` the steer lands (`[tau] steer: hello`).
- CLI `--allow-inject`; `tau consent --list` shows the inject grant.
- Observe-only probe points `session_start` / `branch` / `session_end`
  (probes.md session lifecycle): fired by the CLI via `Agent::observe`;
  verdicts are ignored by contract, and a misused verdict is reported on
  the bus as a `Probe` event with action `ignored`. The `tau probes`
  catalog lists them wired, plus `text_delta` / `tool_progress` as
  reserved slots. The guard example observes `session_start` and reports
  it through `host.notify`; the REPL renderer now prints extension
  notices/facts like print mode.
- The six language examples (C/C++/Python/JS/TS/Go) are rebuilt against
  0.2.0; C++ gained `expected`/`variant` shims (now `-std=c++23`).

### Docs

- `docs/wit-review.md` — full contract review (F1–F10) with the 0.2.0
  action list; F2 feedback leg / F3 (observe leg) / F5 / F6 / F7 / F8
  landed in this batch; F4 evaluated and designed
  (`docs/tool-media.md`, contract 0.3.0), F9 timeout semantics folded
  into `docs/im-channels.md`.
- `docs/im-channels.md`, `docs/realtime-av.md` remain design documents
  (unimplemented; code must not precede them).

### Changed

- `repository`/`homepage` metadata now point at the public repo,
  https://github.com/gqf2008/tau, and member crates actually inherit it
  (rides the next crates.io publish).

## [0.2.0] — 2026-09-27

Examples-and-docs-only release: the five crates are byte-identical
to 0.1.0 in code; the version bump carries the multi-language example
suite into the tagged tree and the dist zip.

### Added

- Minimal wasm extension examples in six more languages — C, C++, Python,
  JavaScript, TypeScript, Go (`examples/<lang>-upper/`) — each implementing
  the same `upper` tool contract as `examples/upper` and passing the
  real-load acceptance (`tool -> upper` / `tool <- upper` in the demo
  transcript). `docs/wasm-languages.md` records the build matrix, exact
  tool versions, and per-language pitfalls (componentize-py class-name
  discovery, jco `--disable http fetch-event`, TinyGo reactor buildmode
  plus four replayed vendored patches). Java is documented honestly as
  no-path-today with authoritative evidence (`examples/java-upper/`).

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
  Ambient WASI by default, `--deny-wasi` restores the sandbox.
- **Signing & consent**: embedded ed25519 signature sections, a trust
  store, per-fingerprint remembered capability grants (bridge argv,
  HTTP origins, credential delivery, WASI-deny). Secrets are delivered,
  never persisted. `tau trust --from-component` onboards keys from
  verified bytes. `--allow-unsigned` excuses only *absent* signatures —
  a signature section that does not verify is refused under every
  policy, so the escape hatch cannot launder tampered bytes. Consent
  files validate their key: a corrupt file reads as absent (the gate
  closes), and a caller-supplied "fingerprint" is accepted only as
  16 lowercase hex — never as a path out of the store; the same shape
  check guards tau sign --key and the trust/key stores. Origin
  checks parse the way the HTTP client does (authority ends at
  `/ ? # \`), so userinfo hidden in a query, fragment, or backslash
  cannot smuggle a request past a consented origin.
- **Distribution**: push/pull components through any OCI registry;
  digest-addressed cache that verifies hits and re-pulls a corrupted
  entry instead of handing bad bytes to the load path;
  signature/trust/consent apply to pulled bytes unchanged.
  Registry calls carry connect and per-request total timeouts (tight
  for manifest/token calls, generous for blob transfers) — a mute or
  blackholed registry errors instead of hanging the CLI forever.
  The dist zip ships the example components unsigned (`release.sh`
  strips any local dev signature), so a first user meets the
  documented sign-and-trust onboarding, not a foreign key.
- **Models**: built-in OpenAI chat completions, OpenAI Responses, and
  Anthropic Messages providers; wasm provider components with push-mode
  streaming and consent-gated HTTP egress — a trapped provider fails
  its run and the instance is rebuilt, so one crash never fails the
  rest of the session. Loading a provider enforces its advertised
  model list: a `--model` the component does not list is refused at
  load, naming the available ids, instead of silently running whatever
  the guest does with an unknown model. Multimodal messages (text,
  image, audio, video, file), media >256KB externalized to a
  content-addressed blob store with `tau gc`. Blob writes are atomic
  and reads verify the hash: a corrupt blob degrades to a placeholder,
  wrong bytes are never served to the model. gc fails closed: a session
  path that does not exist is an error, never an empty mark set — a
  mistyped `--session` cannot orphan every live blob, even under
  `--yes`. Multi-MiB media crosses
  the wasm boundary byte-for-byte — the guest reports the length and
  FNV-1a of the request it received and the host pins both against the
  string it sent, and a normal turn right after still works on the
  reused instance.
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
in eleven steps: demo, the signing/trust chain (trusted load; untrusted,
byte-flipped, signature-stripped, and corrupted-signature rejection; a
garbage pubkey refused with a key-shaped message),
all three built-in providers
against a loopback mock, wasm-provider consent gate (a consent-escaping 302 is shown to the guest, never followed; userinfo/backslash URLs stay on the consented host while delimiter tricks and normalized twins are refused), multi-MiB media crossing the session→guest boundary whole, an unadvertised provider --model refused at load with the available ids named, MCP bridge spawn
gate, remembered-consent lifecycle, credential delivery (the token
reaches the origin through the guest; consent and session files never
persist the secret; TAU_PROVIDER_AUTH flows only with the grant),
OCI push/pull/trust onboarding,
blob GC (the mark covers the whole session tree — active, compacted, and abandoned-branch references all keep their blobs; a missing session file is refused even under --yes), compaction (summary entry; originals stay; follow-ups run on
the compacted branch), torn-tail recovery, concurrent access, and
probe verdicts (block, continue, and a trapped probe degrading without
going dead), and the WASI sandbox boundary (ambient env visible by
default, empty under --deny-wasi), and the interactive REPL over a
real pty (banner, a full turn, /help, idle Ctrl-C, /quit, recall
history) — restoring
the environment exactly afterwards, even when a step fails mid-run
(seeded blobs are registered with the exit trap, never left squatting
in the real store). 121 tests, clippy-clean across all
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
