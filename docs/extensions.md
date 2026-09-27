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

All four return `result<_, string>`: validation failures reach the
guest; nothing is silently swallowed. Demo: `examples/notifier` — its
`poke` tool does all three kinds of call and reports each outcome in
the tool result, so the consent gate is visible in the transcript.

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

## 6. Bridges (world `bridge`)

Bridges translate an external tool protocol into tau tools — the host
stays protocol-agnostic and only grants capabilities: `process`
(spawn-with-pipes) and `http` (origin allowlist). Both are always
linked, granted empty, checked at call time; the user's consent UX
shows the exact argv / origin.

Reference: `examples/mcp-bridge` (MCP stdio + streamable HTTP, with
protocol-version negotiation). `docs/bridges.md` has the capability
model and the walgit worked example.

## 7. WASI: ambient by default

Components run with ambient WASI — fs/env/stdio/args/network — unless
the user passes `--deny-wasi` (or remembered it for your fingerprint).
Design for both: read env vars defensively, treat filesystem access as
a bonus not a requirement. `examples/echo-provider`'s `env NAME`
prompt demos the difference live.

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
