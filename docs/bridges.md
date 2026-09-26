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

## The consent model

Bridge components need a capability ordinary extensions never get: spawning a
process. tau grants it exactly one way:

- The WIT `bridge` world imports `tau:extension/process`: `spawn`, `write-stdin`,
  `read-stdout`, `kill`. Plain data only — no `wasi:io` types, no MCP-shaped
  types. The interface is a generic spawn-with-pipes; it does not know MCP
  exists.
- The host links this interface **only** for components loaded via
  `load_bridge`, and only when the caller passes the allowed command argv.
  Passing the argv **is** the consent. The bridge receives it through the
  single env var `TAU_MCP_COMMAND` (a JSON argv array) — the only env it gets.
- The wasm sandbox contains the bridge itself. The spawned server process is
  the user's own choice of risk, exactly as with a native MCP client.

CLI:

```
tau --mcp-bridge mcp_bridge.wasm \
    --mcp-command '["python", "server.py"]' \
    -p "use the echo tool"
```

## The reference bridge

`examples/mcp-bridge` speaks newline-delimited JSON-RPC (MCP stdio transport):

- `definitions()`: lazily spawns the server, runs `initialize` →
  `notifications/initialized` → `tools/list`, mapping each MCP tool's
  `inputSchema` to a tau `parameters-json`. A handshake failure traps, which
  the host reports as a bridge load error — a broken server is loud, never
  silently tool-less.
- `execute()`: `tools/call` with the model's arguments; text content blocks
  are joined into the tool result, `isError` maps to tau's error flag, and
  non-text blocks degrade to a placeholder rather than vanishing.

Because the bridge is a wasm component, it can be written in any wasm
language, distributed as a single `.wasm` file, and signed — the same
delivery story as every other tau extension.

## HTTP transports

Remote MCP servers (streamable HTTP) need network, not `process`. The plan is
scoped `wasi:http`: the host links the HTTP interfaces for a bridge only with
a per-origin allowlist, so a bridge asking for `https://api.example.com` can
reach exactly that origin and nothing else. Stdio proved the pattern; HTTP
adds origin-scoped consent on the same host-side gate.
