# Bridges: external tool protocols behind wasm

tau core has **no MCP support** — deliberately. MCP is one tool protocol among
many; baking it into the core would couple every release to its evolution.
Instead, external protocols are translated into tau tools by **bridge
components**, following wassette's insight inverted: wassette exposes wasm
components *as* MCP tools; tau's bridge consumes an MCP server *and exposes*
its tools to the agent through `tau:extension/tools`.

```
MCP server process  ← JSON-RPC over stdio →  mcp-bridge.wasm  ← tools iface →  tau core
(any language)                               (sandboxed)                     (zero MCP knowledge)
```

## The capability model

Bridge components need a capability ordinary extensions never get: spawning a
process. The contract gives it exactly one way:

- The WIT `bridge` world imports `tau:extension/process`: `spawn` returns a
  `child` **resource** (since 0.7.0) — `stdin(stream<u8>)` with a future
  that resolves when the host has passed everything on, `stdout()` /
  `stderr()` as streams, `wait()` as a future, `kill()`. The interface is
  a generic spawn-with-pipes; it does not know MCP exists, and nothing in
  it is protocol-shaped. The `extension` world does not import it at all.
- **Since 0.8.0 there is no per-command gate behind it.** The world a
  component was installed as IS the declaration, and the argv the host is
  configured with is host configuration, not a runtime consent. The bridge
  receives it through the single tau-supplied env var `TAU_MCP_COMMAND`
  (a JSON argv array).
- The wasm sandbox contains the bridge itself. The spawned server process is
  the user's own choice of risk, exactly as with a native MCP client.

CLI:

```
tau --mcp-bridge mcp_bridge.wasm \
    --mcp-command '["python", "server.py"]' \
    -p "use the echo tool"
```

## The reference bridge

`examples/mcp-bridge` speaks newline-delimited JSON-RPC (MCP stdio transport).
Protocol version negotiation follows the spec: the bridge asks for its
newest (`2025-06-18`), the server picks, and a pick the bridge does not
speak (supported: `2024-11-05`, `2025-03-26`, `2025-06-18`) fails the
handshake with a clear message — muddling through divergent semantics is
worse than a loud refusal. Servers that omit the field are tolerated.

- `definitions()`: lazily spawns the server, runs `initialize` →
  `notifications/initialized` → `tools/list`, mapping each MCP tool's
  `inputSchema` to a tau `parameters-json`. A handshake failure traps, which
  the host reports as a bridge load error — a broken server is loud, never
  silently tool-less. The guest's panic message reaches its inherited
  stderr; the host error itself is compacted to a one-line summary plus
  root cause, not a wasm backtrace dump.
- `execute()`: `tools/call` with the model's arguments; text content blocks
  are joined into the tool result, `isError` maps to tau's error flag, and
  non-text blocks degrade to a placeholder rather than vanishing.
- Robustness: one JSON-RPC message (and one HTTP body) is capped at
  16 MiB — a server flooding bytes without a newline gets an error, not
  an unbounded linear-memory grow. Host-side, a guest trap during a call
  degrades to a tool error result (never wedges the run), and a poisoned
  instance lock is recovered rather than killing the tool permanently.

Because the bridge is a wasm component, it can be written in any wasm
language, distributed as a single `.wasm` file, and signed — the same
delivery story as every other tau extension.

## HTTP transports

Remote MCP servers (streamable HTTP) need network, not `process`. The bridge
world also imports `tau:extension/http`: `request` (`async` since 0.7.0)
returns a `response` **resource** once the response headers are in —
`status()`, `header(name)`, and `body()`, a `stream<u8>` the guest drains
incrementally and drops early once the JSON-RPC response arrives (the
server may legally hold an SSE stream open; dropping the stream abandons
the rest and closes the connection, which is the only cancellation an SSE
consumer needs). The peer-wait semantics the F9/F11 amendments pinned are
unchanged — a peer that goes quiet or never answers gets an explicit
error, never a permanent park — but the carrier changed: a wasm guest has
no clock it can await, so every `timeout-ms` parameter left the contract
and the budgets are host knobs now — `TAU_HTTP_REQUEST_TIMEOUT_MS` (the
response-headers wait, 30s) and `TAU_HTTP_IDLE_TIMEOUT_MS` (a silent peer
mid-body, 120s) — each refusal naming its budget and duration, so the
gate can shorten one and grep for it.

The write path is where the 0.7.0 stream pays for the knot F12 could not
untie: a blocking write that timed out could never say how much it had
delivered, which is why 0.6.0's `write-stdin` returned a taken-count.
`stdin(data: stream<u8>) -> future<result<_, error>>` makes the whole
question the guest's own await — the host pumps, backpressure is the
stream's, and the future resolves when everything was passed on or the
pipe broke (its error says which). The host still holds a bounded buffer
per child (64 KiB) and writes it from a thread of its own, so a server
that stops reading costs the host that buffer and nothing more; a child
that takes nothing for `TAU_PROCESS_STDIN_IDLE_TIMEOUT_MS` (30s) gets its
write dropped with a line that names the budget and asks whether the
child is reading at all.

Requests go where the component says: since 0.8.0 there is no origin
allowlist, no consent to record and no call-time gate — the world a
component was installed as is its whole declaration (`docs/extensions.md`
§7). **Redirects are never followed**: a redirect is a different endpoint
than the one the component named, and the protocols here name their
endpoint explicitly, so following one would silently move the request
somewhere nobody asked for.

CLI:

```
tau --mcp-bridge mcp_bridge.wasm \
    --mcp-url https://api.example.com/mcp \
    -p "use the echo tool"
```

The bridge learns the endpoint via `TAU_MCP_URL`. Stdio and HTTP are
independent — pass either or both; the bridge prefers `TAU_MCP_URL` when
both are present.

The 0.7.0 version of this file had two more sections here — provider
credential delivery and provider network egress. Both described a wasm
provider, and 0.8.0 deleted that world: `world provider`, `world
realtime`, `--provider-wasm`, `--provider-origin`, `--provider-auth`
and the delivery grant are all gone, and models are host code
(`docs/extensions.md` §5).

## Worked example: walgit

`walgit mcp` (the walgit git server's client-side adapter) is a stdio MCP server whose tools are
the walgit CLI itself (read-only by default; the one writing tool needs
`--allow-write`). tau spawns it through the bridge like any other stdio
server — the `--mcp-command` flag is the host configuration (0.8.0: no
call-time gate behind it):

```
tau --allow-unsigned \
    --mcp-bridge examples/mcp-bridge/target/wasm32-wasip2/release/mcp_bridge.wasm \
    --mcp-command '["walgit", "mcp"]' \
    --demo -p "ci status"
```

The bridge handshakes (`initialize`), registers the 15 read-only tools
(`repo_list`, `repo_refs`, `repo_tree`, `repo_blob`, `repo_commits`,
`repo_diff`, `collab_*`, `ci_status`, `wal_ls`, …) as tau tools, and every
call round-trips: tau → wasm bridge → spawned `walgit mcp` → the walgit
CLI subcommand → result back into the session. `walgit mcp` reads its
usual config (`~/.walgit/walgit.toml`), so `repo_*` tools operate on the
configured bucket directly.

Two demo notes: the demo scripts exactly one tool per run — the lowest tier
present, then the alphabetically first name — and a bridge's tools are
user-loaded, so the pick here is `ci_status` (with no `-e` there would be
nothing to script); and `--repo` defaults
to the cwd — run from a git checkout (or pass one) for the `collab_*` /
`ci_status` tools, while `repo_list` works anywhere.
