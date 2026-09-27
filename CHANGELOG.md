# Changelog

## [Unreleased]

### Added — `ingress` capability (webhook IM platforms, docs/im-channels.md)

- WASI has no listen, so the host binds the consented address:
  `--ingress <addr:port>` (repeatable; remembered-consent unions and
  round-trips the list) lets a bridge `ingress.listen(route)`; the host
  (tiny_http) pushes every request on a registered route into the
  component's `ingress-handler.handle-request` export SYNCHRONOUSLY and
  serves its return value as the HTTP response — the push model, no
  idle-pump window (the ws leg's known limitation). Unregistered paths
  get 404, an unloaded/trapped instance 503/502; TLS termination belongs
  to the tunnel in front, signature verification to the component.
- Contract: bridge world gains `import ingress` (`listen`/`close`) and a
  MANDATORY `export ingress-handler`; bridges without a webhook leg
  stub it with 501 (mcp-bridge, ws-echo-bridge, feishu-bridge updated).
- New example `whatsapp-bridge` + `scripts/wa_mock.py` loopback
  platform: inbound POST → steer into the session (honest ack — 403
  when injection is not consented), `after_response` posts the reply to
  the platform's send API. validate.sh step 5d drives the real REPL
  over a pty (pywinpty): delivered webhook ack 200 → idle steer wakes a
  turn → reply POST; refusal leg without `--ingress` never listens.
- The request record carries the RAW `query` string (signature schemes
  like wecom's msg_signature live in it; the host is a pipe and never
  parses it). New example `wecom-bridge` + `scripts/wecom_mock.py`
  (embedded pure-Python AES, NIST-self-tested at startup) prove the
  component-side crypto red line end to end — validate.sh step 5e:
  tampered msg_signature → 403 (nothing decrypted or steered), URL
  verification echostr round-trips as plaintext, encrypted text message
  → steer → idle wake → reply via the send API.
- Fix (tau-cli): an injected steer/follow-up arriving while the
  interactive REPL idles now starts the next turn with the injected
  message as prompt — previously it queued in the agent's control
  channel forever because the REPL's select only woke on user input
  (host-channel control is interposed through the REPL; mid-run
  forwarding is unchanged, print mode forwards straight through).

### Added — dingtalk bridge + ws::send honesty amendment (docs/im-channels.md)

- New example `dingtalk-bridge` + `scripts/dt_mock.py`: the ws family's
  second adapter (stream mode, mechanism shared with feishu-bridge)
  demonstrating the two dingtalk-shaped increments — the in-band ack
  frame on the same connection (the first real `ws::send` user) and
  the double-encoded `data` JSON. validate.sh step 5f (print mode):
  CALLBACK → double-decode → in-band ack → steer → reply POST.
- Contract amendment (ws, 0.3.0 unreleased): **`ws::send`'s Ok now
  means the frame was WRITTEN to the socket** — the connection actor
  confirms after writing and the call blocks until then (bounded by
  the actor's 250ms read tick). The queued-only semantics provably
  lost acks: in print mode the session exits within one tick and the
  ack never reaches the platform (instrumented: zero frames at the
  mock, connection reset at exit). Same doctrine family as the
  webhook honest-ack red line: Ok must mean it happened.

### Added — realtime-av Phase 0: push-to-talk voice loop (docs/realtime-av.md)

- REPL `/mic <sec> [sine]` records the default input (cpal) — or
  synthesizes a 440 Hz sine, the hardware-free deterministic gate path —
  and sends the clip as a `Content::Audio` (audio/wav) user message;
  after the run, assembled assistant audio blocks play through the
  default output (no device = notice, never red). Capture by explicit
  command IS the consent — the red line's consent category governs wasm
  guests, the host CLI acts with the user's keyboard authority.
- `FauxModel::demo` answers a voice message by echoing the clip as
  three `AudioDelta` chunks — the downlink assembly and playback paths
  are exercised offline, for real.
- Acceptance: validate.sh step 11b (pty, pywinpty): sine → uplink →
  echo → assembly → playback path → audio/wav block in the session
  JSONL. Deps: cpal 0.17 + hound 3.5, tau-cli only.

### Added — realtime-av Phase 1: live playback sink (docs/realtime-av.md)

- `audio::PlaybackSink` hangs off both renderers (interactive + print):
  `AudioDelta` chunks play AS THEY ARRIVE — incremental WAV header
  parse (RIFF walk to the data chunk, then raw PCM per chunk), raw
  `audio/pcm` / `audio/L16` with the `rate=` MIME parameter (default
  24000), a 4s ring buffer that drops the OLDEST samples on overflow
  (realtime semantics: a backlog is sound you can no longer catch up
  to), and a null-sink fallback with no output device that decodes and
  counts identically — headless machines assert "it streamed", never
  red. Abort/Ctrl-C and segment switches clear the buffer mid-run.
- Host API break (counts toward 0.3.0): `AgentEvent::AudioDelta` now
  carries `data: Vec<u8>` instead of the `bytes: usize` count — the
  sink cannot play a count. **The WIT contract is untouched**: the
  guest subscribe path's `audio-segment` stays count-only, no audio
  hot path crosses to wasm.
- Post-run replay (Phase 0) now only fires for audio whose deltas
  never streamed, so live-played sound never plays twice.
- Acceptance: validate.sh step 11b asserts the sink's per-sample
  accounting — 32000 samples == the full 2s @ 16kHz sine clip — plus
  unit tests for chunk-split WAV decode, clear-on-abort, segment
  switch, and the pcm rate default.

### Breaking — contract `tau:extension@0.3.0` (tool media results, wit-review F4)

- Tool results are **multi-block**: `tool-result.content` is now
  `list<result-block>` (`text(string)` / `media(media)`) instead of a
  plain string — tools can return images, audio, video, and files, with
  media bytes crossing the ABI raw (never base64). The variant is
  deliberately non-recursive (a tool result never contains tool calls or
  nested results; wasmtime's host bindgen rejects recursive WIT types
  outright). tau-core: `ToolOutput.content` and
  `Content::ToolResult.content` are `Vec<Content>`; `ToolOutput::ok/err`
  stay as text conveniences, `ok_blocks`/`err_blocks` carry media;
  `ToolOutput::text()` gives the text projection. Sessions written
  before 0.3.0 read without migration (a legacy string `content`
  upgrades to one text block; writes always emit the block array).
  Providers degrade at the wire edge: Anthropic maps text/image
  natively; OpenAI chat sends images as a trailing user message of
  `image_url` parts; OpenAI responses uses `input_text`/`input_image`;
  audio/video/file become text placeholders everywhere
  (docs/tool-media.md).
- Components built against 0.2.0 must be rebuilt; load errors name the
  mismatch ("component targets tau:extension@0.2.0; this host requires
  @0.3.0 — rebuild…"). All seven Rust examples and all six language
  examples (C/C++/Python/JS/TS/Go) are rebuilt and live-verified.
- New example `media-tool` (`dot_png` returns a 1x1 PNG image block);
  validate.sh step 1b asserts the image block end to end (guest → host
  → model projection → inline base64 in the session JSONL).

### Added — high-frequency stream subscription (wit-review F2, 0.3.0)

- `host.subscribe(topics)` / `host.poll(handle)` /
  `host.unsubscribe(handle)`: extensions observe the run's
  high-frequency streams (`text-delta`, `audio-delta`) by pulling — a
  bounded per-subscription ring (1024, mirroring the bus) drained inside
  the guest's own invocations, with a `lagged(n)` marker on overrun
  (docs/stream-subscribe.md). Handles are instance-scoped: a trap
  rebuild invalidates them instead of aliasing. Unknown topics and
  handles fail loud; subscribing outside a run (bus not wired) errors.
  `probes.md`'s reserved `text_delta` slot is now covered by
  `subscribe(["text-delta"])`; `tool_progress` stays reserved (no bus
  producer).
- New example `streamer`: subscribes at `session_start`, polls at
  `before_run_end`, notifies the observed delta count; validate.sh
  step 1c asserts the notice end to end.

### Breaking/Added — bridge world gains the host channel + probes (docs/im-channels.md, 0.3.0)

- The bridge world now **imports `host`** (steer/follow-up/notify/emit/
  subscribe/poll/unsubscribe — the IM inbound leg) and **exports
  `probes`** (the outbound leg: `after_response` observes the assembled
  assistant message). Bridges loaded before this change must be rebuilt
  against the 0.3.0 bridge world (a bridge with nothing to observe
  returns an empty `points()` list; the two existing bridge examples
  gained exactly that). Session injection from a bridge rides the same
  consent gate as extensions: `--allow-inject` or a remembered `inject`
  grant (now carried through `BridgeConsent` ↔ remembered-consent
  conversion, sticky-on merge).
- New example `feishu-bridge`: a feishu-shaped IM adapter — ws long
  connection opened at `session_start`, inbound frames drained at probe
  call points (the synchronous guest model: components only run when
  called; the host's ws actor keeps the connection alive between calls),
  IM messages steered into the session, and the answer posted back to
  the platform's reply API from `after_response`. Loopback-verified
  against `scripts/im_mock.py` (stdlib threaded mock platform) in
  validate.sh step 5c, including the unconsented-steer refusal path.

### Fixed — ambient WASI preopens invisible to guests (Windows)

- `preopen_host_fs` formatted the guest path from a `u8` loop variable:
  `format!("/{letter}")` renders a `u8` as its decimal code, so drives
  were preopened as `/99`, `/100`, … instead of `/c`, `/d`, … Every
  host-side check passed (the preopens existed, `preopened_dir` returned
  `Ok`) while guests saw a filesystem that existed but could not be
  spelled — every path lookup failed `no-entry`. Fixed to format the
  letter as a character; regression test
  `allow_all_preopens_are_reachable_under_contract_names` probes the
  boundary from inside a guest (guard example's `fscheck` leg) so a
  host-side-only green can never pass again.

### Added — IM channel mapping config (docs/im-channels.md, 0.3.0)

- `feishu-bridge` reads its session/identity mapping from a JSON config
  file: the host hands the path over via `TAU_IM_CONFIG`, the component
  reads and parses it over ambient WASI fs (the host never learns the IM
  schema). The channel whose `endpoint` equals the consented
  `TAU_MCP_URL` governs — a config can never smuggle in an endpoint the
  user did not consent. Unknown chats are ignored (noted, never
  steered); `users.allow` absent or empty admits nobody (identity is a
  consent question — fail-closed). validate.sh step 5c gained the
  identity leg: a platform message from a user outside the allowlist is
  consumed with a notice, steered nowhere, and answered with silence.

### Added — `ws` capability for bridges (docs/im-channels.md, 0.3.0)

- The bridge world imports `ws`: `connect` / `send` / `recv` / `close`
  over text/binary frames — the host is a frame pipe and never parses
  payloads (IM stream modes: 飞书/钉钉). Origin consent shares the
  `http` allowlist (ws:→http:, wss:→https:; `--mcp-url` accepts ws(s)
  URLs). Keepalive/idle semantics are contractual (wit-review F9): the
  host pings every 30s, closes with a named reason after 60s of inbound
  silence, and `recv` takes a mandatory timeout — a dead connection
  always surfaces as an explicit error, never a hang. Handles carry the
  instance generation like `process`.
- New example `ws-echo-bridge` + `scripts/ws_echo_mock.py` (stdlib
  loopback echo server); validate.sh step 5b drives an echo round trip
  through the consent gate.

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
