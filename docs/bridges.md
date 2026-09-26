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

Remote MCP servers (streamable HTTP) need network, not `process`. The bridge
world also imports `tau:extension/http`: a plain-data, handle-based interface
(`request` / `status` / `header` / `read-body` / `close`), deliberately the
same shape as `process` rather than `wasi:http` — the guest never touches
pollables, and SSE responses are consumed incrementally and closed early once
the JSON-RPC response arrives (the server may legally hold the stream open).

The host enforces the consent: an origin allowlist (`scheme://host[:port]`).
Every request's origin is checked before sending; **redirects are never
followed** — a redirect would silently move the request to an origin the user
did not consent to. Both capabilities are always linked but granted empty by
default, so an unconsented bridge loads fine and fails at call time, not
instantiation time. (This is about the custom `process`/`http` capabilities;
ambient WASI — fs/env/stdio/network — is granted by default and can be
withdrawn with `--deny-wasi`.)

## Provider credential delivery

A wasm provider calling a real LLM gateway needs an API credential. The
host does not keep secrets for the guest — the user hands a bearer token
to the host explicitly (`--provider-auth <token>`), and the host injects
it into every `run`'s request-json as `"auth": {"bearer": "<token>"}`.
**Giving it IS the consent** to place the token in guest memory. The
token is never written to the consent file; the *delivery grant* can be
(`--provider-auth ... --remember` once). A remembered grant lets the
`TAU_PROVIDER_AUTH` environment variable flow on later runs without the
flag — the secret is re-given every run, only the grant is remembered.
Without flag or remembered grant, `TAU_PROVIDER_AUTH` alone does **not**
reach the component (a note on stderr says so).

Guest side: read `parsed["auth"]["bearer"]`; no field means none was
given. See `examples/http-provider` (forwards it as the Authorization
header and marks the output `[auth]`).

## Provider network egress

The **provider world imports the same `http` interface**: a wasm provider
reaches its model API over exactly this consent-gated channel (same origin
allowlist, same no-redirects rule, same shared host implementation in
`tau-ext/src/http.rs`). Grant origins with `--provider-origin
https://api.openai.com` (repeatable); like bridge consent, grants are
remembered per signing fingerprint with `--remember` and recalled on later
runs. Without consent the provider loads but every http call fails at call
time — a well-behaved provider reports that as an error event, never a trap
(see `examples/http-provider`).

CLI:

```
tau --mcp-bridge mcp_bridge.wasm \
    --mcp-url https://api.example.com/mcp \
    -p "use the echo tool"
```

The URL's origin becomes the allowlist; the bridge learns the endpoint via
`TAU_MCP_URL`. Stdio and HTTP consents are independent — pass either or both;
the bridge prefers `TAU_MCP_URL` when both are present.

## Worked example: walgit

`walgit mcp` (the walgit git server's client-side adapter) is a stdio MCP server whose tools are
the walgit CLI itself (read-only by default; the one writing tool needs
`--allow-write`). tau spawns it through the bridge like any other stdio
server — the `--mcp-command` flag is the consent:

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

Two demo notes: the registry hands tools to the model sorted by name, so
the faux demo model's "first tool" is `ci_status`; and `--repo` defaults
to the cwd — run from a git checkout (or pass one) for the `collab_*` /
`ci_status` tools, while `repo_list` works anywhere.
