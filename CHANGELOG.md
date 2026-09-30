# Changelog

## [Unreleased]

### Fixed

- `examples/{js,ts}-upper/build.sh`: invoke jco as `@bytecodealliance/jco`
  explicitly. The unscoped `jco` on npm is a dependency-confusion placeholder
  (1.0.0, published 2026-01, ships a stub `jco` bin). The scripts were safe
  only implicitly — `npm install` pins the scoped package locally and npx's
  local-first resolution masked the name collision; an ad-hoc `npx jco`
  outside the project would fetch the placeholder. Say the real name.

### Docs

- `docs/wasm-languages.md`: dated 2026-09-30 toolchain recheck of the three
  blocked cells — all blockers stand (wit-bindgen v0.62.0 is still the latest
  upstream release and its `main` still `todo!()`s the async paths the C++
  cell needs; jco still 1.35.0; TinyGo still v0.42.0). Matrix unchanged: C
  and Python pass, nothing rebuilt this round.
- `docs/release.md`: the post-publish acceptance loop now lists only the
  components the zip actually ships (`upper`, `c_upper`); cpp/go rejoin when
  their toolchains unblock.

## [0.7.0] — 2026-09-30

### Breaking — contract `tau:extension@0.7.0`: exports that wait are async, probes carry types, resources replace handles

`wit/tau.wit` (identically vendored at `crates/tau-ext/wit/tau.wit`) is
`tau:extension@0.7.0`. This is the release where the guest side of the
async ABI starts to bind, so it is the largest contract change so far. By
theme:

- **Exports that wait are `async func`.** A synchronously lowered export
  cannot wait, and both ways of faking it are dead ends: a task spawned
  from one is never polled, and `block_on` inside one traps the instance
  (`wasm trap: cannot block a synchronous task before returning`). Both
  measured, with positive controls — docs/wit-redesign.md section 5. So
  `tools.definitions` / `tools.execute` (enumerating a remote tool list is
  connect + initialize + tools/list), `models.run`, the `session`
  resource, `http.request`, `ws.connection.connect` / `.send`,
  `ingress-handler.handle-request` and the new `bridge-io.turn` are async.
  `probes.probe` stays **synchronous on purpose**: it is the decision
  point on the run's hot path, and letting it await would let one
  component hold a whole run on one network call.
- **Probes carry types, not JSON strings.** `point` is an enum (12 arms),
  `payload` a variant (one arm per point), the answer a `verdict`
  (`continue` / `replace(payload)` / `block(string)`), and a host call's
  failure is `types.error` (`refused` / `failed` / `invalid`). The host
  still validates the pairing: a `before-tool` probe cannot answer with a
  `navigation` payload.
- **Resources replace the `u64` handles**: `process.child`,
  `http.response`, `ws.connection`, `ingress.registration`,
  `host.subscription`, `session.session`. Dropping one releases it, which
  made `ingress.registration` turn "close a route that was never
  registered" — an error 0.6.0 had to define — into something
  inexpressible.
- **Streams and futures replace the read/write-with-budget calls.**
  `process.child.stdin(data: stream<u8>) -> future<…>` (the host pumps;
  backpressure is the stream's), `.stdout()` / `.stderr()` and
  `http.response.body()` return streams, `ws.connection.receive()` returns
  `tuple<stream<frame>, future<…>>`. 0.6.0's "taken count" made a bounded
  write honest; a stream makes the bound unnecessary, so the parameter and
  the count are gone with it.
- **`bridge-io.turn(point, payload)` is new**, exported by `world bridge`:
  the host calls it right after `probe` has answered for the same point,
  in a context where the guest may await. It returns nothing — decisions
  stay `probe`'s — so a bridge keeps its decision synchronous and does its
  waiting (open the socket, post the reply) here. Both bridges that need
  no such hook export an explicit no-op.
- **`ws.connection.poll()` is new**: a synchronous, never-waiting drain of
  the frames that have arrived, because a pump must not wait and a
  sync-lowered probe cannot await a stream read. One consumer per
  connection — `receive` and `poll` split nothing, the second caller gets
  `invalid` — and frames that did arrive are delivered before a terminal
  reason, which surfaces on the next call.
- **Every `timeout-ms` parameter is gone.** They were 0.5.0's and 0.6.0's
  answer, and 0.7.0 has no guest-side counterpart for them: a wasm guest
  has no clock it can await. The budgets are host knobs now —
  `TAU_HTTP_REQUEST_TIMEOUT_MS` (30s), `TAU_HTTP_IDLE_TIMEOUT_MS` (120s),
  `TAU_WS_CONNECT_TIMEOUT_MS` (30s), and the new
  `TAU_PROCESS_STDIN_IDLE_TIMEOUT_MS` (30s) /
  `TAU_PROCESS_STDOUT_IDLE_TIMEOUT_MS` (120s). Each message names its
  budget and its duration, so the gate shortens one and greps for it.

An old component is rejected by name: `component targets
tau:extension@0.6.0; this host requires @0.7.0; rebuild with the 0.7.0
bindings (wit/tau.wit), see CHANGELOG.md`. All fifteen Rust examples are
rebuilt on 0.7.0, and so is the language matrix — C and Python build and
pass the acceptance run, while C++, JavaScript/TypeScript and Go are
blocked on their toolchains' async-export support, recorded verbatim in
docs/wasm-languages.md.

### Added

- **Built-in tools** (`crates/tau-tools`): `read`, `write`, `edit`, `ls`,
  `grep`, `find`, `bash` and `powershell` are registered by default, so a
  run works on a repository with no extension installed. Schemas,
  descriptions, output shapes, error texts and limits are pi's; `grep`
  and `find` search natively (`ignore` + `regex` + `globset`) instead of
  requiring ripgrep or fd, and a `timeout:` kills the whole process tree.
  Two new flags: `--tools <list>` replaces the selection — built-in and
  component tools alike, an unknown name fails the run — and
  `--no-builtin-tools` registers none. Every agent run now prints
  `[tau] built-in tools: …`. The tools are host code, so `--deny-wasi`
  does not apply to them; `docs/builtin-tools.md` is the reference.
- **`ToolRegistry` and `ProbeRegistry` are `Clone`** (tau-core), and a
  clone shares its tools and handlers rather than copying them: one
  instantiated component can serve more than one session, while
  registering into a clone still affects only that clone. `register`
  still takes a `Box`, so no call site changed.
- **ACP mode** (`--acp`): tau speaks the Agent Client Protocol over
  stdin/stdout, natively (`agent-client-protocol` 2.2.0) — no adapter
  and no second binary — so an editor can spawn it as an agent. stdout
  carries the protocol and nothing else; diagnostics stay on stderr. One
  process serves any number of sessions, and in this mode `--session`
  names a **directory** (default `.tau/sessions`): each session is a
  JSONL at `<dir>/<session-id>.jsonl`, which is an ordinary tau session
  (`tau tree --session` reads it, `--continue --session` resumes it).
  The handshake claims only what is true — v1, `loadSession: false`,
  `promptCapabilities.image` and `.audio` (both blocks reach the model as
  `Content::Image`/`Content::Audio`), no auth methods — and every method
  tau does not implement (`session/load`, `authenticate`, `fs/*`,
  `terminal/*`) is answered method-not-found. A prompt's blocks fold, in
  order, into one message (text, image and audio blocks all carry their
  bytes to the model); the loop's events become `session/update` (text
  deltas; a tool call announced before its update, which carries a
  flattened preview of the output); a realtime provider's audio, VAD and
  barge-in have no stable-v1 update, so they are dropped with one line on
  stderr per turn — the assembled audio stays in the session file;
  tool-call ids on the wire are
  `{turn}:{provider id}`, because a scripted model reuses its own ids
  across turns and a client would otherwise draw one call that never
  ends. `session/cancel` is honored only while a turn is in flight — at
  rest it is dropped, not queued, because a queued abort would stop the
  *next* run before its first token — and it cannot interrupt a tool that
  is already running (give the tool a `timeout:`). `--acp` conflicts with
  `-p`/`--print`, `--compact`, `--continue` and `--continue-from`.
  `docs/acp.md` is the reference.
- **A permission gate for the mutating built-ins, in ACP mode only.**
  `write`, `edit`, `bash` and `powershell` ask the client through
  `session/request_permission` before they run, with the protocol's four
  options (allow or reject, once or for the session); `always` is
  remembered per tool in that session and never written to disk. The gate
  is a `before_tool` probe registered *after* the components', so an
  extension's rewritten arguments are what the user approves, and a call
  an extension already blocked is never asked about. The question has no
  timeout: `session/cancel` is what unblocks it, arriving as `cancelled`
  and read as a refusal of that one call — as is every other non-allow
  outcome, because a gate that fails open is not a gate. A refusal
  becomes the tool result `blocked: …` with `is_error`, the client sees
  the call go `failed`, and the model reads why. Outside this mode
  nothing changed: the built-ins are host code with no gate
  (`docs/builtin-tools.md`). The handshake also reports once, on stderr,
  the `fs`/`terminal` delegation a client offers and tau does not take
  up.
- **`HostError`** (tau-core): the three-way host-call error that the
  0.7.0 contract's `types.error` projects — `Refused` (nobody granted
  this), `Failed` (granted, and it broke), `Invalid` (never a valid call
  here). The 0.7.0 contract carries the arm (`types.error`), so a guest
  branches on the variant instead of matching English prose; the point of
  the type is exactly that — the same detail string under two different
  arms is two different values — and `From<HostError> for String` is what
  the paths that predate the arm still use (docs/wit-redesign.md 投影规则).
- **Typed probe payloads** (tau-core): `ProbeHandler::probe` takes a
  `ProbePayload` — one arm per point, with records named after the 0.7.0
  contract's (`BeforeRun`, `AssembledContext`, `FinalRequest`,
  `AssembledResponse`, `ToolOutcome`, `RunEnd`, `Compaction`,
  `Navigation`, `SessionFacts`, `Branch`) — instead of a
  `serde_json::Value` that every handler indexed by string
  (`payload["messages"]`). The point is fixed when a handler is called, so
  `ProbePayload::point()` is the only way to ask which one it is, and a
  renamed field is now a compile error rather than a runtime no-op.
  `Agent::observe` takes the same type (`session_start` / `branch` /
  `session_end` carry `SessionFacts` / `Branch`). The contract carries the
  same arms since 0.7.0 and the projection is field by field
  (`crates/tau-ext/src/convert.rs`); `ProbePayload::to_json` /
  `merge_json` keep the JSON shapes and 0.6.0's replacement semantics for
  everything that still speaks JSON.
  One behaviour change: a replacement that does not fit the point it
  answers (a missing `prompt`, a `messages` that is not a list) used to
  fail the run; it now degrades to `continue` with a line on stderr, like a
  trapping component, because a broken extension must not wedge the
  harness. `Verdict` is no longer `PartialEq` (its payload is not a
  value); match it. Migration stage 1's two tau-core types — the error
  enum above and these payloads — are both in.

- **Skills and project instructions.** The working directory is read
  before the first request. `.agents/skills`, `.claude/skills` and
  `.goose/skills` are searched, in that order, for skill directories (a
  `SKILL.md` with `name` and `description` frontmatter in it), and each one
  becomes a single manifest line — `- hello: greets the reader in a set
  way` — in the system prompt: the bodies are large and are not inlined,
  which is what the new `load_skill` built-in is for. It returns a body, or
  a supporting file named relative to the skill directory, and refuses
  anything that climbs out of it. `AGENTS.md` from the working directory up
  to the repository root goes into the system prompt root-first, and
  `--system` precedes both. Every agent run now prints `[tau] skills: …`
  and one `[tau] project instructions: …` line per file, both `none`-able.
  `load_skill` is a built-in like the rest — `--tools`/`--no-builtin-tools`
  decide whether it is in the run, and the manifest is advertised only when
  it is — and a directory with no skills registers no such tool, so
  `--demo` transcripts and the default tool set are unchanged. Discovery
  never fails a run: a broken or duplicated skill is a line on stderr and a
  skip. `docs/skills.md` is the reference; `scripts/validate.sh` 3c asserts
  the manifest and the instructions on the provider wire, and that the body
  is not there.

### Changed

- `--demo` scripts **the tool the run picked**, not the alphabetically
  first one. The pick comes from `Tool::demo_tier` +
  `ToolRegistry::demo_pick` (lowest tier, then first name) and is resolved
  by name against the request: a pick the request does not advertise
  scripts nothing (the old `first()` fallback is gone), and a tool can opt
  out of the demo entirely with a `None` tier. `docs/tutorial.md` and
  `docs/bridges.md` carry the new rule. The eight built-ins use it: a
  read-only one the user named with `--tools` may be scripted, and the
  mutating four never are — `tau --tools bash --demo` registers bash and
  runs nothing (`scripts/validate.sh` 1e).
- **`serde_json` keeps insertion order now.** `agent-client-protocol`
  depends on it with `preserve_order` and `raw_value`, and Cargo unifies
  features per crate across a workspace build, so the effect is global:
  `serde_json::Map`, and with it every `serde_json::Value` tau holds,
  preserves the order keys were written in instead of sorting them.
  Nothing becomes non-deterministic — a `json!` literal has one order —
  but two outputs change visibly: `tau probes --json` lists its keys in
  the order the source writes them (`name`, `wired`, `payload`,
  `verdicts`), and tool-call arguments stored in a session keep the
  provider's order instead of an alphabetical one.

### Fixed

- **A tool call from a real provider was assembled and never run.** Both
  the chat-completions and the Messages streams ended by yielding a
  `ModelEvent::Done { stop: Stop }` of their own, on top of whatever the
  provider had already said. The agent loop keeps the last stop it sees
  and executes tools only on `StopReason::ToolUse`, so a provider's
  `finish_reason: "tool_calls"` (or `stop_reason: "tool_use"`) was
  overwritten on the way out: the call was parsed, persisted to the
  session, and dropped. Ending a stream now goes through
  `tau_core::sse::Closing`, which emits that fallback only when the
  provider never named a stop — a stream that spoke keeps its word.
  `scripts/validate.sh` 3b is the leg that pins it down — and the
  one that found it.
- `tau sign` reported signing-key failures as `not a wasm binary`: a
  malformed `--key` fingerprint, an ambiguous keyring, and an unreadable key
  file all shared the module parser's error variant, so the message named a
  file that was never opened. Key failures now carry their own key-shaped
  message; `tau trust` had already been fixed the same way.
- `scripts/validate.sh` asserts the wasm-parse wording cannot come back on
  the `tau sign --key` path, not just on `tau trust`.

### Docs

- `docs/tutorial.md`: the hands-on walkthrough of the path the author guide
  describes — scaffold → sign → OCI with every command and its real output,
  including the offline loopback-registry recipe `scripts/validate.sh` step 7
  uses and the consumer-side `tau trust --from-component` onboarding.
- `docs/release.md`: how to force a fresh clippy run for the non-workspace
  example crates after a version-only cut — a cut does not move their
  fingerprints, so the leg can be empty on a brand-new commit.

## [0.6.0] — 2026-09-28

### Breaking — contract `tau:extension@0.6.0`: `write-stdin` takes a budget and says how much it took (wit-review F12)

- `process.write-stdin(handle, data, timeout-ms) -> result<u32, string>`:
  the last host call that could block forever has a bound now, and it is not
  the shape the reads took. A write that times out may already have delivered
  part of its buffer, so "return an error and let the guest retry" would
  silently replay half a JSON-RPC message — worse than a hang. The count
  returned is what the host **took**: those bytes are handed to the child in
  order and belong to the host from that moment, so the guest resumes at
  `data[taken..]` and never resends. A short count is normal, `0` means the
  child took nothing within the budget, and a hard error means no more can
  ever be taken (stdin closed, or the child is gone). `timeout-ms` is
  refused at 0 like every other budget here.
- Host side: each child gets a writer thread that owns its end of the pipe
  plus a bounded buffer (64 KiB, queued in 8 KiB chunks), so a child that
  stops reading costs one buffer, never a parked host thread. The
  taken/delivered counters live in the host; the guest holds the offset.
- Guest side: `examples/mcp-bridge` sends in offsets and fails by name —
  "server took nothing from stdin in 3 budgets of 3000ms — is it reading?"
  — instead of waiting forever, and bytes the server has not read yet are
  still delivered in order afterwards.
- Landed with: five host unit tests (`0` refused; a small message handed
  over without waiting; a child that never reads bounded, remainder left to
  the caller; what was taken really does reach a child that reads late; a
  dead child's stdin is an error, not a `0`) and one validate.sh leg — a
  request inflated past the host's buffer (`TAU_MCP_PAD`) against a
  `--mute-stdin` server must surface as the named stall.
- Docs: `docs/bridges.md` states the write path's shape, `docs/wit-review.md`
  closes F12 — its last open finding — and the contract-version references in
  `docs/extensions.md`, `docs/architecture.md`, `docs/host-channel.md` and
  `docs/wasm-languages.md` move to 0.6.0. The six-language matrix was
  rebuilt and re-accepted (all six cells: two-line transcript each, sizes
  unchanged); the round touches only the `bridge` world, so that rebuild is
  the version gate's doing — every import/export name carries the contract
  version — not a semantic change for those cells.

### Docs — the post-publish check is a recipe now, not a sentence

- `docs/release.md`'s post-publish block spells out the stranger test the
  0.4.0 and 0.5.0 rounds actually ran: the crates.io binary against the dist
  zip's components (the two-line acceptance of docs/wasm-languages.md), the
  side-effect-ledger check that the install really replaced the previous
  version, and a previous-contract component refused with both versions
  named. It also says plainly that `scripts/validate.sh` builds from the
  checkout — it proves the tree, not the upload — and warns that a native
  `tau.exe` cannot read an MSYS `/tmp/...` argument (`os error 3` under
  "reading <path>", which reads like a component defect and is not one).

## [0.5.0] — 2026-09-28

### Breaking — contract `tau:extension@0.5.0`: every wait on a peer gets a budget (wit-review F11)

- Three host calls could block forever, each on a different peer shape, and
  each now takes a `timeout-ms` like `http.read-body` and `ws.recv`:
  - `http.request(method, url, headers, body, timeout-ms)` — the wait for
    **response headers**. reqwest's blocking `timeout` runs until the
    response body has finished, so it cannot bound this phase without also
    cutting long SSE streams short; the host runs the send on a helper
    thread and bounds the wait here instead. A peer that accepts the TCP
    connection and then says nothing is indistinguishable from a dead one:
    `err("http.request: no response headers within Nms")`.
  - `ws.connect(url, timeout-ms)` — the **handshake**. `tungstenite::connect`
    does TCP + TLS + the upgrade in one unbounded blocking call, and the
    read tick is only set after it returns, so nothing else could have
    bounded it. `err("ws.connect: no handshake within Nms")`.
  - `process.read-stdout(handle, max, timeout-ms)` — the wait for **bytes**.
    A child that is alive but silent used to park the host thread until it
    exited; the timeout is neither EOF nor a dead handle
    (`err("process.read-stdout: no bytes within Nms")`), so the guest can
    retry or kill it.
- 0 is refused in all three, naming the missing budget: "block forever is
  not a contract". The budget belongs to the guest: the IM bridges pass 30s
  (`NET_MS`), the MCP bridge 60s (shared with its body reads), the ws-echo
  example 5s, and the demo HTTP provider reuses its `idle=<ms>` prompt token
  so the gate can exercise a short budget instead of waiting out the
  production one.
- Landed with: seven host unit tests (0 refused at each of the three; a
  silent loopback peer for the headers; a listener that never upgrades for
  the handshake; a quiet child for the stdout read, plus one that speaks
  late — the late bytes are still delivered after the timeout) and two
  validate.sh legs (`/mute` route for the headers bound, a TCP listener that
  never upgrades for the handshake bound).
- Single source for the contract version: `tau-ext`'s `CONTRACT_VERSION`
  now backs both load-error messages that name it (the extension version
  hint and the bridge instantiation failure) and the comparison that
  decides whether to add the hint, and a test asserts it equals the
  `package` line in `wit/tau.wit` — the hint can no longer drift from the
  contract it names.
- Recorded, deliberately not in scope: `process.write-stdin` is the one
  blocking host call left unbounded (wit-review F12), and it cannot take the
  same shape — a write that times out may already have delivered part of the
  data, so "retry the whole buffer" silently replays half a message. Fixing
  it means a partial-write return (bytes written, guest-held offset), an
  interface-shape change for a later round.

### Docs — the current contract version is stated once per doc, at 0.5.0

- `docs/architecture.md` §4.1, `docs/extensions.md`, `docs/host-channel.md`
  and `docs/wasm-languages.md` (contract-history line) now name
  `tau:extension@0.5.0`; `docs/bridges.md` and `docs/im-channels.md` state
  the new bounds where they describe the capability; `docs/wit-review.md`
  records F11 as landed and opens F12. The `media-tool` and `notifier`
  example headers follow the contract they build against.
- Re-verified this round: the six-language matrix in
  `docs/wasm-languages.md` (C / C++ / Python / JS / TS / Go — Java has no
  path) was rebuilt against 0.5.0 and all six pass the two-line acceptance.
  A contract bump is exactly what invalidates that table, because the
  version is part of every export name; `docs/release.md` now says so in
  the pre-flight.

## [0.4.0] — 2026-09-28

### Breaking — contract `tau:extension@0.4.0`: `http.read-body` gains an idle budget (wit-review F9)

- `http.read-body(handle, max)` → `read-body(handle, max, timeout-ms)`, the
  same shape as `ws.recv` (whose half of F9 landed in 0.3.0). A peer that
  sends headers and then goes quiet — a silently hung SSE, a half-open TCP
  connection — used to park the host thread until process exit; the read
  now returns `err("http.read-body: no bytes within Nms")`, and 0 is
  refused outright, because "block forever" is not a contract. The handle
  survives a timeout: retry with a longer budget, or close it.
- The guest picks the budget, because only it knows its protocol:
  `examples/http-provider` passes 30s, `examples/mcp-bridge` 60s (the
  spec's SSE keepalives keep real long-running calls under it). The demo
  provider also accepts a trailing `idle=<ms>` prompt token so the gate can
  exercise a short budget instead of waiting out the production one.
- Landed with: host unit tests (zero refused; a quiet loopback peer returns
  at its budget instead of blocking, and the byte that arrives late is
  still readable afterwards) and a validate.sh leg against a silent route.
  Stale components are refused at load with the version named — rebuild
  against `wit/tau.wit`.
- Gate hygiene: `scripts/validate.sh` rebuilds the `bad-schema` fixture
  unconditionally — an existence guard was reusing the previous contract's
  artifact, so that leg failed as a version mismatch instead of naming the
  broken tool. `docs/release.md` records the ordering rule the bump
  exposed: build the fixtures (validate.sh) before the suites (cargo test).
  Its pre-flight list now also runs clippy *before* the suites, because a
  clippy pass after `cargo test` shares the check-profile fingerprints and
  can be a pure cache replay — green, ~0.5s, and no `Checking <crate>` line.
- Recorded, deliberately not in scope: `process.read-stdout` still has no
  idle bound (wit-review F11).

### Docs — the capability gates are documented as intent, not a sandbox (wit-review F1, 裁定 A)

- F1 decision (owner, 2026-09-28): keep ambient WASI open by default.
  `docs/extensions.md` §7 now states exactly what the default hands
  every component — stdio, the whole host env and argv, network + DNS,
  and the entire host filesystem preopened read-write — and that the
  `http`/`process` consent gates can be routed around by importing
  `wasi:sockets`/`wasi:filesystem` directly. The two real closers
  (`--deny-wasi`, sticky per fingerprint with `--remember`, lifted by
  `tau consent --revoke`) and the deny semantics are spelled out there
  as the single authoritative copy; `docs/wit-review.md` F1 records the
  decision, `docs/architecture.md` and `docs/bridges.md` point at it.
  No behavior change.
- Status honesty: `docs/realtime-av.md`'s banner no longer says "design
  draft, not landed" — Phases 0/1/2a/2b are landed (its own inventory
  table already said so); only Phase 3 (wasip3 stream ABI) is pending.
- The six-language matrix (docs/wasm-languages.md) was rebuilt end to end
  and re-accepted at 0.4.0. C++ needed the value-form `std::expected`, which
  the generated bindings now instantiate. That gap is a property of the
  current C++ generator, not of this contract change: rebuilding the v0.3.0
  tree with the installed wit-bindgen 0.62 fails identically, and the 0.3.0
  ✅ was backed by a real artifact (stamped @0.3.0, still loads and runs) that
  had merely stopped being rebuildable. The Go rebuild needs `TINYGO` /
  `WASMOPT` / `ADAPTER` given explicitly (neither is on PATH). Sizes
  re-measured.

## [0.3.0] — 2026-09-28

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

### Added — realtime-av Phase 2a: full-duplex core (docs/realtime-av.md)

- `RealtimeSession` — the persistent bidirectional session abstraction
  (the OpenAI Realtime / Gemini Live shape) as an OPTIONAL capability
  of `Model`: `push_audio` / `push_image` / `interrupt` / `close` +
  `events()`. `Model::realtime(config)` defaults to `None`, so
  capability discovery IS the call and request/response providers
  carry zero burden. `Agent::realtime` forwards it untouched.
- New event kinds, mirrored on `ModelEvent` and `AgentEvent`:
  `InputAudioChunk` (uplink fact; count-only on the agent bus — the
  bytes are already local), `SpeechStarted`/`SpeechStopped` (server
  VAD), `Interrupted` (barge-in: the loop freezes the current audio
  segment — the next same-media_type delta opens a NEW block — and
  playback sinks clear their buffers).
- `FauxRealtime` (demo variant only — discovery keeps a negative
  case): deterministic VAD on the first chunk of a burst, every
  uplink byte echoed down as an `AudioDelta` of the same media type,
  `Interrupted` answered to `interrupt()`, VAD-off + `Done` at close.
- CLI `/live <sec> [sine]`: paced 50ms `audio/pcm;rate=16000` chunks
  (synthesized sine for the gate, real mic via `audio::stream_mic`),
  Ctrl-C = barge-in (the REPL survives), /quit and EOF close the
  session orderly and record both blocks into the session tree. The
  live span is wrapped in a synthetic `RunStart`/`RunEnd` on the bus
  so the Phase 1 sink arms (announce, per-sample summary, clear) are
  reused verbatim — timing is not invented (red line 3).
- The WIT contract is untouched (Phase 2b seals the verified shape:
  world `realtime`, microphone/camera consent categories, wasm
  realtime provider example).
- Acceptance: validate.sh step 11c (pty, two legs) — happy path with
  byte-exact duplex accounting (32000 samples == 2s @ 16kHz echoed
  whole; uplink 64000 bytes recorded) and the barge-in leg; unit
  tests pin the deterministic script, door-refusal after close, the
  Interrupted assembly split, and uplink facts never entering
  assistant content.

### Added — realtime-av Phase 2b: world `realtime` + device consent categories (docs/realtime-av.md)

- WIT (still `tau:extension@0.3.0`): `model-event` gains the four
  realtime kinds (`input-audio-chunk`, `speech-started`,
  `speech-stopped`, `interrupted`); new interface `session`
  (`open`/`push-audio`/`push-image`/`interrupt`/`close`, all
  door-refusing `result<_, string>`) and new world `realtime` —
  `import events + http`, `export models + session`, so a realtime
  component doubles as an ordinary provider and downlink events reuse
  `events.emit`. One session per instance.
- tau-ext: `ExtensionHost::load_realtime` → `WasmRealtimeModel`
  (`stream()` via `models.run` like any provider; `realtime()` builds
  a FRESH instance per session, opens it, wires the session-long event
  channel; `close()` consumes the instance so the event stream ends
  after terminal events flush — a trapped session poisons only its own
  instance). `ExtensionHost::is_realtime_component` probes the
  component's exports — the CLI picks the world by reading the type,
  never by error-driven fallback.
- Consent taxonomy gains sticky per-fingerprint `microphone`/`camera`
  categories. The category guards the DEVICE, not the session:
  `/live N` (real mic) with a wasm realtime provider requires
  `--microphone` (remembered with `--remember`); `/live N sine`
  synthesizes and needs no grant. `camera` is registered but admits no
  capture path yet — always absent.
- `examples/realtime-echo` (world `realtime`): the SAME deterministic
  script as `FauxRealtime` — one script, two carriers, the gate's
  native and wasm paths cross-prove each other.
- Acceptance: validate.sh step 11d (pty, two legs — refusal names
  `--microphone` while sine flows, then consented duplex with exact
  32000-sample accounting across the wasm boundary); tau-ext
  `realtime_echo` integration tests pin the per-event sequence, the
  door refusal after close, and the probe's negative case.

### Breaking — contract `tau:extension@0.3.0` (tool media results, wit-review F4)

- `model-event` grows four realtime cases (`input-audio-chunk`,
  `speech-started`, `speech-stopped`, `interrupted`) and the package
  gains interface `session` + world `realtime` — additive at the WIT
  level, but the interface version hash changes, so 0.2.0-built
  components must rebuild against the new tau.wit (the load error
  names the missing/mismatched instance).
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

### Fixed

- `examples/README.md` (shipped in the dist zip) listed only 10 of the
  14 components; rows for whatsapp/wecom/dingtalk bridges and
  realtime-echo added, and validate.sh pre-flight now fails if the
  README, release.sh's two loops, and the EXAMPLES list ever disagree
  again (three-way drift guard).

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
