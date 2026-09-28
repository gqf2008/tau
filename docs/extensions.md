# Writing a tau extension

End-to-end: from an empty crate to a signed, distributed component. The
contract is `wit/tau.wit` (versioned, `tau:extension@0.1.0`); this guide
walks the three worlds — `extension` (tools + probes), `provider`
(models), `bridge` (external protocols) — using the shipped examples as
reference implementations.

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
  `tool-call` / `tool-result`), verdicts, definitions, handles, errors.
- JSON strings only at schema-less leaves: `arguments-json` (arbitrary
  model-produced JSON), `parameters-json` (JSON Schema is itself a
  schema language), probe `payload-json` (per-point, fast-evolving,
  discoverable via `tau probes`).

WIT types are frozen within a package version; evolution rides package
minor bumps (0.x semantics). The host links exactly one contract
version; a component built against an older one gets a load error
naming the mismatch (docs/host-channel.md 兼容性). Typed↔serde conversion lives ONCE in the host and
is pinned by round-trip property tests (message → ABI → JSON ==
original). A bonus the envelope never had: media crosses the ABI as
raw `list<u8>`, never base64 — which is what the data model
(`tau_core::types`) always required of binary boundaries.

`process`/`http` handles stay plain `u64`, not resources — resources
drag in wasi:io and strain C/TinyGo toolchains.

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

- `definitions()` — called **once at load time**. Each definition is a
  name, a description the model reads when deciding to call, and a JSON
  Schema (serialized) for the arguments. A `parameters-json` that does
  not parse **fails the whole load**, naming the tool — a broken schema
  is never silently widened to an open one (wit-review F5).
- `execute(name, arguments-json)` — called per tool call. Return
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
`before_navigation`. `tau probes` prints the catalog with payload
shapes; `docs/probes.md` has the full table and verdict semantics.

- `points()` — called once at load; return the wire names you handle
  (`["before_tool"]`). Empty = observe nothing.
- `probe(point, payload-json)` — synchronous; the harness **pauses**
  until the verdict returns. Keep hot-path handlers fast; a slow probe
  slows every run.
- Verdicts: `continue` (no opinion), `replace` (+ `payload-json`,
  point-specific), `block` (+ `reason`; vetoes the action — at
  `before_tool` the reason goes back to the model as the tool result).

Handlers fold in load order: each sees the previous handler's
replacement; first `block` wins. A trapping handler degrades to
`continue` — a broken extension must not wedge the harness.

## 4. The host channel (world `extension`, import `host`)

Since `tau:extension@0.2.0` the extension world imports `host` — the
guest→host active channel (design: `docs/host-channel.md`). Facts are
always allowed; decisions are consent-gated:

- `notify(level, content)` — a user-visible notice ("info" / "warn" /
  "error"); the renderer draws text blocks and media placeholders.
  Published on the event bus as `AgentEvent::ExtensionNotice`; never
  enters model history.
- `emit(event-json)` — an extension-defined fact on the bus
  (`AgentEvent::ExtensionFact`). The schema is yours, so this stays a
  JSON leaf — but it must be well-formed JSON, or the result says so.
- `steer(message)` / `follow-up(message)` — inject a user message into
  the run. **Consent-gated**: pass `--allow-inject` (or persist the
  grant with `--remember`); without it the call fails with a named
  refusal. Messages must have `role: user`; the host validates and caps
  size (4 MiB). Enqueue-only: delivery follows the control channel's
  checkpoints (steer after the current turn's tool results, follow-up
  when the run finishes) — a probe mid-call never re-enters the loop.
- `subscribe(topics)` / `poll(handle)` / `unsubscribe(handle)` —
  observe the run's high-frequency streams (since 0.3.0, design:
  `docs/stream-subscribe.md`). Topics: `text-delta`, `audio-delta`.
  The guest **pulls**: the host hangs a bounded ring (1024 events) on
  the bus per subscription and the guest drains it with `poll` inside
  its own invocations — the host never calls into a component
  asynchronously, so granularity is the guest's own call frequency and
  an overrun surfaces as a `lagged(n)` marker. Handles are
  instance-scoped: a trap rebuild invalidates them. Unknown topics and
  handles fail loud.

All seven return `result<_, string>`: validation failures reach the
guest; nothing is silently swallowed. Demos: `examples/notifier` — its
`poke` tool does notify/emit/steer and reports each outcome in the tool
result, so the consent gate is visible in the transcript;
`examples/streamer` — subscribes at `session_start` and polls at
`before_run_end`, reporting the observed delta count via `notify`.

## 5. Providers (world `provider`)

A provider component serves models. Streaming is **push-mode**: you
call `events.emit(json)` per chunk and return from `run` when done.

- `list-models()` — ids the user can select with `--model`. Load fails
  for any other id, naming the available ones, so keep this list honest.
- `run(request-json)` — the request uses tau's wire shape
  (`{"model", "system", "messages", "tools", "auth"?}`). Emit
  `text-delta` / `audio-delta` / `tool-call-delta` events, then exactly
  one `done` with a stop reason. **Contract:** never trap on
  request/transport failures — emit an `error` event followed by
  `done {"stop":"error"}`.

Network access is consent-gated: the `http` import is always linked
but granted empty, so calls fail at call time until the user allows
your origins (`--provider-origin https://api.example.com`, remembered
per fingerprint with `--remember`). When the user hands the host a
bearer token, it arrives inside the request as `"auth": {"bearer": …}` —
never persisted by the host.

References: `examples/echo-provider` (no network, word-by-word echo —
start here), `examples/http-provider` (real consent-gated HTTPS + SSE).

Load with:

```bash
tau --provider-wasm target/wasm32-wasip2/release/my_provider.wasm \
  --model my-model --provider-origin https://api.example.com -p "hi"
```

## 5.5 Realtime providers (world `realtime`)

A provider that also exports `session` becomes a realtime provider: the
world is `import events + http`, `export models + session` — so the
component still doubles as an ordinary provider (`models.run` serves
print mode), while `/live N` in the REPL opens a full-duplex session.
Capability discovery is the export itself: the host probes the component
type for `tau:extension/session@…`, there is no flag to set.

- `open(config-json)` — the config carries `input-media-type` (e.g.
  `audio/pcm;rate=16000`) plus optional output media type and
  instructions. Refuse a config you cannot serve with `err`.
- `push-audio(bytes)` / `push-image(jpeg)` — uplink. Every call is a
  `result<_, string>`: once you consider the session dead, refuse at
  the door instead of trapping.
- `interrupt()` — the user barged in (Ctrl-C during `/live`). What you
  already emitted is what the user heard; freeze the current output
  segment and emit an `interrupted` model-event.
- `close()` — flush terminal events (`speech-stopped`, then exactly one
  `done`); the host ends your event stream right after. One session per
  instance — the host instantiates fresh per session, and a trapped
  session poisons only its own instance.

Downlink events ride the same `events.emit` as plain providers, with
four extra model-event kinds: `input-audio-chunk` (uplink fact),
`speech-started` / `speech-stopped` (VAD), `interrupted`. The audio
bytes cross to the host in `audio-delta` payloads; the guest-side
subscription channel stays count-only by design (no audio hot path
through `host.poll`).

Device consent is the host's job, not yours: a real microphone uplink
requires the user's `--microphone` grant (the category guards the
device, not the session — the synthetic `sine` uplink needs none), and
the `camera` category is registered but admits no capture path yet.

Reference: `examples/realtime-echo` — a deterministic VAD + echo double
(the same script as the native demo provider), exercised end to end by
validate.sh step 11d.

## 6. Bridges (world `bridge`)

Since 0.3.0 the bridge world also imports `ws` — a WebSocket frame pipe
for stream-mode protocols (IM long connections, docs/im-channels.md):
`connect(url)` (origin allowlist shared with `http`; `--mcp-url` accepts
ws(s) URLs), `send(handle, frame)`, `recv(handle, timeout-ms)` (the
timeout is mandatory — a recv that can block forever hides a dead
connection; the host pings every 30s and closes with a named reason
after 60s of inbound silence), `close(handle)`. Handles are
generation-fenced like `process`. Demo: `examples/ws-echo-bridge`.

Also since 0.3.0 (the docs/im-channels.md contract amendment): bridges
import the **host channel** and export **probes** — the IM adapter
three-leg set. `host.steer`/`follow-up` inject inbound messages into the
session behind the same `inject` consent as extensions (`--allow-inject`
or a remembered grant); `host.notify`/`emit`/`subscribe`/`poll`/
`unsubscribe` behave exactly as for extensions. Probe points are opt-in
via `points()` (empty = observe nothing); `after_response` is the IM
outbound leg. Demo: `examples/feishu-bridge` (ws long connection in,
reply POST out; loopback mock `scripts/im_mock.py`, validate.sh 5c).

Bridges translate an external tool protocol into tau tools — the host
stays protocol-agnostic and only grants capabilities: `process`
(spawn-with-pipes), `http`/`ws` (origin allowlist) and `host`
(session injection, consent-gated). All are always linked, granted
empty, checked at call time; the user's consent UX shows the exact
argv / origin / grant.

Reference: `examples/mcp-bridge` (MCP stdio + streamable HTTP, with
protocol-version negotiation). `docs/bridges.md` has the capability
model and the walgit worked example.

## 7. WASI: ambient by default — and why the gates are not a wall

Components run with ambient WASI — fs/env/stdio/args/network — unless
the user passes `--deny-wasi` (or remembered it for your fingerprint).
Design for both: read env vars defensively, treat filesystem access as
a bonus not a requirement. `examples/echo-provider`'s `env NAME`
prompt demos the difference live.

Be precise about what "ambient" hands over, because it is more than the
word suggests. Under the default policy (`WasiPolicy::AllowAll`) every
component — extension, bridge, provider, realtime alike — gets:

- stdio, the **whole host environment**, and the host's argv;
- network access and DNS resolution;
- the **entire host filesystem preopened read-write** (`/` on unix; on
  Windows every existing drive, as `/c`, `/d`, …).

One consequence deserves to be stated outright rather than discovered:

> **The consent gates on `http` and `process` are not a security
> boundary.** A component that imports `wasi:sockets` or
> `wasi:filesystem` directly goes around them: what the user never
> granted is refused on the gated interface and simply not enforced on
> the ambient one. The gates are a declaration of intent — they make a
> component's reach auditable in one place, and they stop honest
> mistakes — not a wall against code that means to get out.

What actually closes ambient WASI is host-side and all-or-nothing per
component (there is no per-capability ambient subset):

- `--deny-wasi` — this run, every component;
- `--deny-wasi --remember` — sticky for that signing fingerprint; only
  `tau consent --revoke` lifts it.

Under deny the WASI interfaces still link, but nothing is granted: fs
and network calls fail permission-denied, env and args come back empty,
stdio goes nowhere.

So do not mistake the gates for a sandbox. If a real boundary is what
you need, that is an OS-level question about the process tau runs in.
What does hold is the signing chain — fingerprint → trust → consent is
what makes "this component, and not another one" answerable at all. The
gates record what the user agreed to; they do not enforce it against a
component that routes around them.

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

Consent (bridge argv, HTTP origins, credential delivery, WASI-deny) is
remembered per **signing fingerprint**, not per file — your users keep
their grants across your releases as long as you sign with the same
key. `tau consent --list` / `tau consent --revoke <fingerprint>` is
their escape hatch.

## 9. Checklist

- [ ] `definitions()`/`list-models()` return fast — they run at load
- [ ] no panics on bad input; `is_error` / `error` events instead
- [ ] probes are fast (the harness waits) and degrade gracefully
- [ ] works under `--deny-wasi` or documents why it cannot
- [ ] signed after the final build; fingerprint published out-of-band
- [ ] pushed by tag for convenience, by digest for the cautious
