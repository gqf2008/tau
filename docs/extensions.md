# Writing a tau extension

End-to-end: from an empty crate to a signed, distributed component. The
contract is `wit/tau.wit` (versioned — the `package` line there is the
authority, currently `tau:extension@0.8.0`); this guide
walks the two worlds — `extension` (tools + probes) and `bridge`
(external protocols) — using the shipped examples as
reference implementations. For the same path walked hands-on, with every
command and its output, see `docs/tutorial.md`.

## Contract conventions

(amended 2026-09-27 after the `docs/wit-review.md` discussion — the
earlier "JSON envelopes everywhere" rule was wrong; pi compatibility
constrains the session-file and HTTP edges, NOT the component ABI)

Calibrated after pi (`packages/agent`): the minimalism lives in the
MECHANISM (a hook is a function, a tool is a function, payloads are
plain data) — not in elaborate data structures. So the typed surface
stays small: a handful of records/variants, no type cathedral.

The rule: **payloads whose schema tau owns use rigorous WIT types;
JSON envelopes only where the schema is external or genuinely
arbitrary.** Concretely:

- Typed: the message trunk (`message` / `content` / `media` /
  `tool-call` / `tool-result`), verdicts, definitions, probe points
  and payloads (since 0.7.0 — the point set and every payload shape
  are tau's own, docs/probes.md), errors.
- JSON strings only at schema-less leaves: `arguments-json` (arbitrary
  model-produced JSON), `parameters-json` (JSON Schema is itself a
  schema language) and `host.emit`'s `event-json` (the extension owns
  that schema, so no type here can describe it).

WIT types are frozen within a package version; evolution rides package
minor bumps (0.x semantics). The host links exactly one contract
version; a component built against an older one gets a load error
naming the mismatch (docs/host-channel.md 兼容性). Typed↔serde conversion lives ONCE in the host and
is pinned by round-trip property tests (message → ABI → JSON ==
original). A bonus the envelope never had: media crosses the ABI as
raw `list<u8>`, never base64 — which is what the data model
(`tau_core::types`) always required of binary boundaries.

Since 0.7.0 the stateful ends are **resources** (`process.child`,
`http.response`, `ws.connection`, `ingress.registration`,
`host.subscription`) and the waits are **streams and
futures** — a guest's own await is where a deadline belongs, so every
`timeout-ms` parameter left the contract (the budgets are host knobs,
`TAU_*_TIMEOUT_MS`, each refusal naming its budget). The toolchain cost
the old `u64` posture avoided is now measured rather than assumed:
`docs/wasm-languages.md` records which generators carry the async ABI
today (C and Python pass; C++, JS/TS and Go are toolchain-blocked).
Since 0.8.0 those ends live only in the `bridge` world — with `world
provider` and `world realtime` deleted, the component-model async ABI
is a property of talking to the outside world, not of writing a tau
extension.

Prerequisites: a Rust toolchain with the component target —

```bash
rustup target add wasm32-wasip2
```

Rust is the reference toolchain; minimal examples in C, C++,
Python, JavaScript, TypeScript, and Go — each load-tested against this
exact contract — live in `examples/<lang>-upper/`, with the build
matrix and per-language pitfalls in `docs/wasm-languages.md`.

## 1. Scaffold

A component is a `cdylib` crate in its own workspace (components build
standalone; the examples use an empty `[workspace]` table to opt out of
the host workspace):

```toml
# Cargo.toml
[package]
name = "my-ext"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.46"
serde_json = "1"

[workspace]
```

```rust
// src/lib.rs
wit_bindgen::generate!({
    path: "path/to/tau/wit/tau.wit",   // vendored copy recommended
    world: "extension",
});
export!(MyExt);
```

**Vendor the WIT file into your repo.** The path is resolved at compile
time against your crate; tracking a specific `tau:extension` version
keeps your build reproducible when the contract evolves.

## 2. Tools (world `extension`)

Implement `exports::tau::extension::tools::Guest`:

- `definitions()` — called **once at load time** (`async` since 0.7.0:
  a bridge whose tool list lives on a remote server performs its
  handshake here; a literal list awaits nothing and reads as it did).
  Each definition is a
  name, a description the model reads when deciding to call, and a JSON
  Schema (serialized) for the arguments. A `parameters-json` that does
  not parse **fails the whole load**, naming the tool — a broken schema
  is never silently widened to an open one (wit-review F5).
- `execute(name, arguments-json)` — called per tool call (`async`
  since 0.7.0: a tool that talks to the host awaits those calls in
  place). Return
  `ToolResult { content, is_error }`; the content blocks go back to the
  model as the tool result. Since tau:extension@0.3.0 `content` is a
  **list of result-blocks** (docs/tool-media.md): `text(string)` and/or
  `media(media)` — a tool can return images/audio/files, with raw bytes
  crossing the ABI (never base64). Providers that only accept text/image
  tool results get media degraded to a text placeholder at the provider
  edge; the bytes are still persisted in the session. Never panic: a
  trap kills the load, but an `is_error` result is just a tool failure
  the model can react to.

Reference: `examples/upper/src/lib.rs` (an `upper` tool, ~60 lines
including a no-op probes impl). Build and load:

```bash
cargo build --target wasm32-wasip2 --release
tau --allow-unsigned -e target/wasm32-wasip2/release/my_ext.wasm \
  --demo -p "try the tool"
```

The transcript shows the loop closing: `tool → my_tool`, then
`tool ← my_tool: <output>`, then the answer. Iterate with `--demo`
(no API key needed); switch to a real provider when the tool behaves.

## 3. Probes (same world, interface `probes`; `hooks` before 0.2.0)

Probes observe and influence the run at nine wired points —
`before_run`, `transform_context`, `before_request`, `after_response`,
`before_tool`, `after_tool`, `before_run_end`, `before_compaction`,
`before_navigation` — plus three observe-only session points
(`session_start`, `branch`, `session_end`). `tau probes` prints the
catalog with payload shapes; `docs/probes.md` has the full table and
verdict semantics.

- `points()` — called once at load; return the points you handle
  (`[Point::BeforeTool]`). Empty = observe nothing.
- `probe(point, payload)` — typed since 0.7.0: `point` is a 12-arm
  enum, `payload` a variant with one arm per point, so a
  context-trimming probe no longer parses the message trunk to do it.
  Still **synchronous** on purpose: the harness **pauses** until the
  verdict returns — a probe is a decision point, not an I/O
  opportunity. Keep hot-path handlers fast; a slow probe slows every
  run.
- Verdicts: `continue` (no opinion), `replace(payload)` (the same
  variant, point-specific arm), `block(reason)` — vetoes the action; at
  `before_tool` the reason goes back to the model as the tool result.

Handlers fold in load order: each sees the previous handler's
replacement; first `block` wins. A trapping handler degrades to
`continue`, and so does a `replace` whose payload does not fit the point
it answers (the host names the offending field on stderr) — a broken
extension must not wedge the harness.

## 4. The host channel (world `extension`, import `host`)

Since `tau:extension@0.2.0` the extension world imports `host` — the
guest→host active channel (design: `docs/host-channel.md`). Facts are
always allowed; the run-affecting calls are validated, but since 0.8.0
there is no call-time gate left to pass:

- `notify(level, content)` — a user-visible notice ("info" / "warn" /
  "error"); the renderer draws text blocks and media placeholders.
  Published on the event bus as `AgentEvent::ExtensionNotice`; never
  enters model history.
- `emit(event-json)` — an extension-defined fact on the bus
  (`AgentEvent::ExtensionFact`). The schema is yours, so this stays a
  JSON leaf — but it must be well-formed JSON, or the result says so.
- `steer(message)` / `follow-up(message)` — inject a user message into
  the run. **Installing the component is the authorization** (0.8.0:
  `--allow-inject` and its call-time refusal are gone; the authority is
  part of the install record, and it is the one capability ambient WASI
  cannot imitate). Messages must have `role: user`; the host validates
  and caps size (4 MiB). Enqueue-only: delivery follows the control
  channel's
  checkpoints (steer after the current turn's tool results, follow-up
  when the run finishes) — a probe mid-call never re-enters the loop.
- `subscribe(topics)` — observe the run's high-frequency streams
  (since 0.3.0, design: `docs/stream-subscribe.md`), returning a
  `subscription` **resource** (since 0.7.0 the handle table and the
  explicit unsubscribe collapsed into ownership: dropping it
  unsubscribes, and a trap rebuild drops it for you). Topics are an
  enum (`text-delta`, `audio-delta`), so an unknown one is
  unrepresentable. The guest **pulls**: the host hangs a bounded ring
  (1024 events) on the bus per subscription and the guest drains it
  with `subscription.poll()` inside its own invocations — the host
  never calls into a component asynchronously on this path, so
  granularity is the guest's own call frequency and an overrun surfaces
  as a `lagged(n)` marker. **0.9.0 pending**: this pull shape is the
  0.7.0 answer; the call-convention unification (resource handlers plus
  `dispatch`) that replaces it is 0.9.0 work, not 0.8.0.

The calls return `result<_, error>` — a typed `failed` / `invalid`
arm (`refused` was 0.7.0's third arm; 0.8.0 deleted it, because with
the gates gone nothing refuses at call time and a variant with no
producer is not a contract), so a guest branches on the arm instead of
matching English prose (the detail string still says what happened);
nothing is silently swallowed. Demos: `examples/notifier` — its
`poke` tool does notify/emit/steer and reports each outcome in the tool
result, so every outcome is visible in the transcript;
`examples/streamer` — subscribes at `session_start` and polls at
`before_run_end`, reporting the observed delta count via `notify`.

## 5. Providers — removed in 0.8.0

No component is a model. `world provider`, `world realtime`,
`interface models` and `interface session` were deleted, and with them
`--provider-wasm`, `--provider-origin`, `--provider-auth` and the
credential-delivery story (token injected as `auth.bearer`, grant
remembered with `--remember`). The model is the harness's own brain:
the built-in providers are host code — OpenAI chat completions, OpenAI
Responses, Anthropic Messages, each pointed at a base URL through the
environment — and the media plane is host-internal end to end
(`docs/realtime-av.md`, red line 1).

The cost is stated rather than discovered: model coverage is release
cadence now, and a vendor tau does not ship has no component path. What
remains for a component author is §2–§4 (`extension`) and §6
(`bridge`); the 0.7.0 text of this section is in the repository
history and summarized in `CHANGELOG.md`.

## 6. Bridges (world `bridge`)

Since 0.3.0 the bridge world also imports `ws` — a WebSocket frame pipe
for stream-mode protocols (IM long connections, docs/im-channels.md).
Resource-based since 0.7.0: `connect(url)` is `async` and returns a
`connection` (`--mcp-url` accepts ws(s)
URLs); `send(frame)` awaits the socket write — Ok still means
*written*, not queued (the dingtalk honest-ack amendment); and receiving
has two legs for the two kinds of guest: `receive()` returns
`tuple<stream<frame>, future<…>>` for a consumer that can await, while
`poll()` drains what has arrived **synchronously, never waiting** — the
shape a pump inside a sync probe needs (one consumer per connection; the
second caller gets `invalid`). Close is dropping the resource. The F9
liveness rule is unchanged — the host pings every 30s and closes with a
named reason after 60s of inbound silence — but the `timeout-ms`
parameters are gone (a guest has no clock to await): the connect budget
is the host knob `TAU_WS_CONNECT_TIMEOUT_MS` (30s), and a silent peer's
stream simply ends with the reason named. Demo:
`examples/ws-echo-bridge`. **0.9.0 pending**: `poll()` is the 0.7.0
pull shape; the call-convention unification (resource handlers plus
`dispatch`) that replaces it is 0.9.0 work, not 0.8.0.

Also since 0.3.0 (the docs/im-channels.md contract amendment): bridges
import the **host channel** and export **probes** — the IM adapter
three-leg set. `host.steer`/`follow-up` inject inbound messages into the
session on the same terms as extensions (installing the component is the
authorization; 0.8.0 deleted the `--allow-inject` gate);
`host.notify`/`emit`/`subscribe`/`poll` behave exactly as for
extensions. Probe points are opt-in
via `points()` (empty = observe nothing); `after_response` is the IM
outbound leg. Demo: `examples/feishu-bridge` (ws long connection in,
reply POST out; loopback mock `scripts/im_mock.py`, validate.sh 5c).

Webhook platforms need the mirror capability: `ingress.listen(route)`,
serving on the host's listen address (`--ingress 127.0.0.1:8080` — host
configuration, not a per-component grant since 0.8.0), returns
a `registration` resource — dropping it stops serving the route — and
the host pushes each request into the mandatory
`ingress-handler.handle-request` export (`async` since 0.7.0; the host
still serializes per instance, so a webhook arriving mid-tool-call
queues — platforms retry, that is a fact of the platform, not a loss).
The return value *is* the HTTP response, so a 200 ack and a synchronous
callback reply both fall out naturally; a bridge with no webhook leg
exports a stub returning 501. Signature verification stays the
component's job — it holds the platform secret. Demos:
`examples/whatsapp-bridge`, `examples/wecom-bridge` (validate.sh 5d/5e).

0.7.0 also adds `bridge-io.turn(point, payload)`, an export the host
calls right after `probe` answered for the same point, in a context
where the guest **may await**. It returns nothing — decisions stay
`probe`'s — so a bridge keeps its verdict synchronous and does its
waiting (open the socket, drain the frames, post the reply) here; a
bridge that needs no such hook exports an explicit no-op.

Bridges translate an external tool protocol into tau tools — the host
stays protocol-agnostic and only provides capabilities: `process`
(spawn-with-pipes), `http`/`ws`, `ingress` and `host` (session
injection). Since 0.8.0 there is no call-time gate behind any of them:
the world a component was installed as is the declaration, and the
capability interfaces the host serves are exactly the ones that world
imports.

Reference: `examples/mcp-bridge` (MCP stdio + streamable HTTP, with
protocol-version negotiation). `docs/bridges.md` has the capability
model and the walgit worked example.

## 7. WASI: ambient, always — and why that is the whole posture

Components run with ambient WASI — fs/env/stdio/args/network — always.
0.8.0 deleted `--deny-wasi` and the `WasiPolicy` type with it: there
is one posture, not a default plus a tightening knob, and no
`--remember` to make the tightening sticky for a fingerprint. Design
accordingly: read env vars defensively if you like, but filesystem and
network access are simply there.

Be precise about what "ambient" hands over, because it is more than the
word suggests. Every component — extension and bridge alike — gets:

- stdio, the **whole host environment**, and the host's argv;
- network access and DNS resolution;
- the **entire host filesystem preopened read-write** (`/` on unix; on
  Windows every existing drive, as `/c`, `/d`, …).

The consequence that 0.7.0 stated as a warning is now the design:

> **Nothing here is a boundary.** 0.7.0 had per-capability gates on
> `http` and `process` and they were never a wall — a component that
> imports `wasi:sockets` or `wasi:filesystem` goes around them
> (`docs/wit-review.md`, F1). A gate that only stops honest mistakes
> costs every user the vocabulary and buys nothing, so 0.8.0 keeps only
> the world boundary — a component that needs `process` has to be a
> bridge — and drops the call-time checks.

What is left as the authorization act is **installing/trusting signed
bytes**, and the declaration shown at that moment is read from the
component type (its imports and exports), not hand-written. The signing
chain — fingerprint → trust — is what makes "this component, and not
another one" answerable at all. It answers *which* component this is,
never *what it may do*; it is not a sandbox and does not pretend to be
one.

Which leaves the honest answer for a real boundary: that is an OS-level
question about the process tau runs in (a separate account, a container,
a VM).

The same honesty applies to the tools tau ships itself. The eight
built-in tools (`docs/builtin-tools.md`) are **host code, not
components**: they read, write, and spawn with the permissions of the
tau process, and no wasm-side policy ever applied to them. They are not
gated either — turning them off is `--no-builtin-tools`, which
is about what the model may call, not about what the sandbox allows.
The one place a built-in asks before it acts is ACP mode, where the four
mutating tools go through the editor's `session/request_permission`
first (`docs/acp.md`); that is a prompt shown to a person, which is not
the same thing as a boundary, and outside that mode there is none.

## 8. Sign and distribute

Unsigned components need `--allow-unsigned` on every load — fine for
development, wrong for distribution. Signing embeds an ed25519
signature section (`tau-signature`) carrying the pubkey:

```bash
# once per author machine:
tau keygen                       # → fingerprint, key in ~/.tau/keys

# after every build:
tau sign target/wasm32-wasip2/release/my_ext.wasm

# distribute via any OCI registry:
tau push target/wasm32-wasip2/release/my_ext.wasm \
  oci://ghcr.io/you/my-ext:0.1.0
```

The receiver onboards your key **from the signed bytes** — the
signature section embeds the pubkey, and only keys whose signature
verifies are trusted:

```bash
tau trust --from-component oci://ghcr.io/you/my-ext:0.1.0
# prints your fingerprint — the receiver MUST verify it out-of-band
# (your README, a signed git tag, a tweet) before trusting
tau -e oci://ghcr.io/you/my-ext:0.1.0 -p "..."
```

Publish the fingerprint next to the download link. Re-sign after every
rebuild (the signature covers the exact bytes); tags are mutable —
receivers who want immutability pull by digest
(`oci://ghcr.io/you/my-ext@sha256:…`).

0.8.0 deleted the consent store along with the gates: there is no
`~/.tau/consent/*`, no `--remember`, no `tau consent --list` /
`--revoke`. Signing still travels per **signing fingerprint**, not per
file — but it now answers only "which component is this", and the
authorization is the act of installing/trusting those bytes.

## 9. Checklist

- [ ] `definitions()`/`list-models()` return fast — they run at load
- [ ] no panics on bad input; `is_error` / `error` events instead
- [ ] probes are fast (the harness waits) and degrade gracefully
- [ ] declares its reach by its world and imports — there is no runtime
      gate to fall back on (0.8.0: ambient WASI always, and the component
      runs with the tau process's permissions)
- [ ] signed after the final build; fingerprint published out-of-band
- [ ] pushed by tag for convenience, by digest for the cautious
