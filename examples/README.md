# Prebuilt example components

These `.wasm` files are the built form of the `examples/*` sources in the
tau repository. They are **unsigned** — under the default `RequireTrusted`
policy, load them with `--allow-unsigned` (or sign them with your own key:
`tau keygen` once, then `tau sign <file>.wasm`).

| file | kind | try it |
|------|------|--------|
| `upper.wasm` | extension (tool) | `tau --allow-unsigned -e examples/upper.wasm --demo -p "shout hello using the upper tool"` |
| `mcp_bridge.wasm` | bridge (MCP) | `tau --allow-unsigned --mcp-bridge examples/mcp_bridge.wasm --mcp-command '["python","server.py"]' -p "hi"` |
| `guard.wasm` | extension (probe) | `tau --allow-unsigned -e examples/upper.wasm -e examples/guard.wasm --demo -p "shout forbidden"` → the `before_tool` probe blocks the call and the model sees the reason |
| `notifier.wasm` | extension (host channel) | `tau --allow-unsigned -e examples/notifier.wasm --demo -p "hello"` → notify/emit reach the renderer, steer is refused; add `--allow-inject` and the steer lands |
| `feishu_bridge.wasm` | bridge (IM adapter: ws inbound steer + after_response reply) | `tau --allow-unsigned --mcp-bridge examples/feishu_bridge.wasm --mcp-url ws://<host>/im --allow-inject --demo -p "hi"` → IM message is steered in, the answer is posted back (loopback: `scripts/im_mock.py`) |
| `ws_echo_bridge.wasm` | bridge (ws capability) | `tau --allow-unsigned --mcp-bridge examples/ws_echo_bridge.wasm --mcp-url ws://<host>/echo --demo -p "echo hi via ws_echo"` → the frame crosses the consent-gated host pipe and back |
| `streamer.wasm` | extension (stream subscription) | `tau --allow-unsigned -e examples/streamer.wasm --demo -p "hello"` → subscribes to `text-delta` at session start and reports the observed deltas at run end (`ext info: stream observed: …`) |
| `media_tool.wasm` | extension (media tool result) | `tau --allow-unsigned -e examples/media_tool.wasm --demo -p "show me a dot"` → the `dot_png` tool returns a 1x1 PNG image block; the model sees `a 1x1 transparent dot. [image: image/png]` and the session JSONL keeps the bytes |
| `whatsapp_bridge.wasm` | bridge (IM adapter: webhook ingress + reply POST) | `tau --allow-unsigned --mcp-bridge examples/whatsapp_bridge.wasm --mcp-url http://<platform-send-api> --ingress 127.0.0.1:9001 --allow-inject --demo` → the host binds the consented ingress address, the platform's webhook POST steers the session, the answer is posted back (loopback: `scripts/wa_mock.py`, full pty loop: `scripts/wa_ingress_e2e.py`) |
| `wecom_bridge.wasm` | bridge (IM adapter: webhook ingress with component-side AES/msg_signature) | same shape as whatsapp plus the crypto gate: tampered signatures get 403 before anything is decrypted (loopback: `scripts/wecom_mock.py`, full pty loop: `scripts/wecom_ingress_e2e.py`) |
| `dingtalk_bridge.wasm` | bridge (IM adapter: stream mode, CALLBACK double-decode + in-band ack) | `tau --allow-unsigned --mcp-bridge examples/dingtalk_bridge.wasm --mcp-url ws://<host>/dt --allow-inject --demo -p "hi"` → ack frame and reply POST both reach the platform (loopback: `scripts/dt_mock.py`) |

Three more components are built outside cargo — they need their own
toolchains — and are shipped prebuilt so you can load-test without
installing any of them:

| file | language | try it |
|------|----------|--------|
| `c_upper.wasm` | C | `tau --allow-unsigned -e examples/c_upper.wasm --demo -p "shout hello using the upper tool"` |
| `cpp_upper.wasm` | C++ | same, with `examples/cpp_upper.wasm` |
| `go_upper.wasm` | Go | same, with `examples/go_upper.wasm` |

(The Python/JS/TS ones are 12–18 MB and are not shipped; build them with
`bash examples/<lang>/build.sh`.) Toolchains, the per-language breakpoints,
and how to rebuild and re-accept all six: `docs/wasm-languages.md`.

See `docs/bridges.md` for the bridge/MCP contract and `docs/signing.md` for
signing, trust, and remembered consent.
