# Prebuilt example components

These `.wasm` files are the built form of the `examples/*` sources in the
tau repository. They are **unsigned** — under the default `RequireTrusted`
policy, load them with `--allow-unsigned` (or sign them with your own key:
`tau keygen` once, then `tau sign <file>.wasm`).

| file | kind | try it |
|------|------|--------|
| `upper.wasm` | extension (tool) | `tau --allow-unsigned -e examples/upper.wasm --demo -p "shout hello using the upper tool"` |
| `echo_provider.wasm` | provider | `tau --allow-unsigned --provider-wasm examples/echo_provider.wasm --model echo -p "hello"` |
| `http_provider.wasm` | provider (http capability) | `tau --allow-unsigned --provider-wasm examples/http_provider.wasm --model http --provider-origin http://example.com -p "http://example.com/"` (add `--provider-auth <token>` to hand it a bearer credential) |
| `mcp_bridge.wasm` | bridge (MCP) | `tau --allow-unsigned --mcp-bridge examples/mcp_bridge.wasm --mcp-command '["python","server.py"]' -p "hi"` |
| `guard.wasm` | extension (probe) | `tau --allow-unsigned -e examples/upper.wasm -e examples/guard.wasm --demo -p "shout forbidden"` → the `before_tool` probe blocks the call and the model sees the reason |
| `notifier.wasm` | extension (host channel) | `tau --allow-unsigned -e examples/notifier.wasm --demo -p "hello"` → notify/emit reach the renderer, steer is refused; add `--allow-inject` and the steer lands |
| `media_tool.wasm` | extension (media tool result) | `tau --allow-unsigned -e examples/media_tool.wasm --demo -p "show me a dot"` → the `dot_png` tool returns a 1x1 PNG image block; the model sees `a 1x1 transparent dot. [image: image/png]` and the session JSONL keeps the bytes |

See `docs/bridges.md` for the bridge/MCP contract and `docs/signing.md` for
signing, trust, and remembered consent.
