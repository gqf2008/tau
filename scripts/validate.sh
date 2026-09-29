#!/usr/bin/env bash
# First-user validation: prove the release candidate works for someone
# who just installed tau, in eleven steps — demo, the built-in tools
# (default set, off switch, no shell through --demo), skills discovery
# (the manifest on the provider wire, the body on demand), the ACP mode
# over the pipes an editor uses, the signing/trust chain (incl. tamper
# rejection), all three built-in providers against a loopback mock (and
# a real tool-call round trip on that wire), the wasm provider consent
# gate, the MCP bridge spawn gate, the remembered-consent lifecycle, OCI
# distribution, blob GC, compaction, probe verdicts, and the interactive
# REPL over a real pty (skipped with a note when pywinpty is not
# installed).
#
# Usage: scripts/validate.sh
#
# Environment contract (Windows: dirs::home_dir ignores HOME/USERPROFILE,
# so ~/.tau is the REAL one): the script keygens one throwaway key,
# records its fingerprint, and removes exactly that key + pub and the
# blobs it seeded on exit (even on FAIL).
# Nothing else in ~/.tau is touched; the consent store is not written.
set -euo pipefail
cd "$(dirname "$0")/.."

ROOT="$(pwd)"
TAU="$ROOT/target/release/tau"
[ -f "$ROOT/target/release/tau.exe" ] && TAU="$ROOT/target/release/tau.exe"
WORK="$ROOT/target/validate"
MOCK_PID=""
REG_PID=""
OCI_BLOB=""
GC_SEEDS=""
THROWAWAY_FP=""

step() { echo; echo "== $1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }

cleanup() {
    [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
    [ -n "$REG_PID" ] && kill "$REG_PID" 2>/dev/null || true
    [ -n "$OCI_BLOB" ] && rm -f "$HOME/.tau/oci/blobs/$OCI_BLOB"
    for b in $GC_SEEDS; do rm -f "$HOME/.tau/blobs/$b"; done
    # Step 8 may have created the blob store; leave it only if it was
    # already there or something legitimately lives in it.
    rmdir --ignore-fail-on-non-empty "$HOME/.tau/blobs" 2> /dev/null || true
    cd "$ROOT" # cannot remove the workdir while standing in it (Windows)
    if [ -n "$THROWAWAY_FP" ]; then
        rm -f "$HOME/.tau/keys/$THROWAWAY_FP.key"             "$HOME/.tau/trust/$THROWAWAY_FP.pub"             "$HOME/.tau/trust/$THROWAWAY_FP.pub.aside"             "$HOME/.tau/consent/$THROWAWAY_FP.json"
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

# --- pre-flight -------------------------------------------------------
# The release.sh example lists must enumerate the SAME set as the
# EXAMPLES list below — two hand-maintained copies drift silently
# (LESSON_两处本该一致的逻辑分开维护漂移不报错). Assert equality
# before building.
EXAMPLES="upper http-provider mcp-bridge guard echo-provider notifier media-tool streamer ws-echo-bridge feishu-bridge whatsapp-bridge wecom-bridge dingtalk-bridge realtime-echo"
rel_build=$(sed -n 's/^for ex in \(.*\); do$/\1/p' "$ROOT/scripts/release.sh" | head -1)
rel_zip=$(sed -n 's/^for ex in \(.*\); do$/\1/p' "$ROOT/scripts/release.sh" | sed -n '2p' | tr '_' '-')
[ "$(echo $EXAMPLES | tr ' ' '\n' | sort)" = "$(echo $rel_build | tr ' ' '\n' | sort)" ] \
    || fail "release.sh build list drifted from validate.sh EXAMPLES"
[ "$(echo $EXAMPLES | tr ' ' '\n' | sort)" = "$(echo $rel_zip | tr ' ' '\n' | sort)" ] \
    || fail "release.sh zip list drifted from validate.sh EXAMPLES"
# examples/README.md ships in the dist zip — it is a THIRD copy of the
# example enumeration; every shipped component must have a row.
for ex in $EXAMPLES; do
    wasm_name="$(echo "$ex" | tr '-' '_').wasm"
    grep -q "\`$wasm_name\`" "$ROOT/examples/README.md" \
        || fail "examples/README.md missing a row for $wasm_name"
done

step "build release binary + wasm examples"
cargo build --release -p tau-cli --quiet
for ex in $EXAMPLES; do
    cargo build --manifest-path "examples/${ex}/Cargo.toml" \
        --target wasm32-wasip2 --release --quiet
done
UPPER="$ROOT/examples/upper/target/wasm32-wasip2/release/upper.wasm"
HTTP_PROVIDER="$ROOT/examples/http-provider/target/wasm32-wasip2/release/http_provider.wasm"
MCP_BRIDGE="$ROOT/examples/mcp-bridge/target/wasm32-wasip2/release/mcp_bridge.wasm"
GUARD="$ROOT/examples/guard/target/wasm32-wasip2/release/guard.wasm"
ECHO_PROVIDER="$ROOT/examples/echo-provider/target/wasm32-wasip2/release/echo_provider.wasm"
NOTIFIER="$ROOT/examples/notifier/target/wasm32-wasip2/release/notifier.wasm"
MEDIA_TOOL="$ROOT/examples/media-tool/target/wasm32-wasip2/release/media_tool.wasm"
STREAMER="$ROOT/examples/streamer/target/wasm32-wasip2/release/streamer.wasm"
WS_ECHO="$ROOT/examples/ws-echo-bridge/target/wasm32-wasip2/release/ws_echo_bridge.wasm"
FEISHU="$ROOT/examples/feishu-bridge/target/wasm32-wasip2/release/feishu_bridge.wasm"
WHATSAPP="$ROOT/examples/whatsapp-bridge/target/wasm32-wasip2/release/whatsapp_bridge.wasm"
WECOM="$ROOT/examples/wecom-bridge/target/wasm32-wasip2/release/wecom_bridge.wasm"
DINGTALK="$ROOT/examples/dingtalk-bridge/target/wasm32-wasip2/release/dingtalk_bridge.wasm"
[ -f "$UPPER" ] || fail "upper example missing"
[ -f "$HTTP_PROVIDER" ] || fail "http-provider example missing"
[ -f "$MCP_BRIDGE" ] || fail "mcp-bridge example missing"
[ -f "$GUARD" ] || fail "guard example missing"
[ -f "$ECHO_PROVIDER" ] || fail "echo-provider example missing"
REALTIME_ECHO="$ROOT/examples/realtime-echo/target/wasm32-wasip2/release/realtime_echo.wasm"
[ -f "$REALTIME_ECHO" ] || fail "realtime-echo example missing"
[ -f "$NOTIFIER" ] || fail "notifier example missing"
[ -f "$MEDIA_TOOL" ] || fail "media-tool example missing"
[ -f "$STREAMER" ] || fail "streamer example missing"
[ -f "$WS_ECHO" ] || fail "ws-echo-bridge example missing"
[ -f "$FEISHU" ] || fail "feishu-bridge example missing"
[ -f "$WHATSAPP" ] || fail "whatsapp-bridge example missing"
[ -f "$WECOM" ] || fail "wecom-bridge example missing"
[ -f "$DINGTALK" ] || fail "dingtalk-bridge example missing"

rm -rf "$WORK"
mkdir -p "$WORK"
cd "$WORK"

# --- step 1: demo -----------------------------------------------------
step "1/11 demo"
OUT="$("$TAU" --demo -p "hello from validation" 2>&1)" || fail "demo exited $?"
echo "$OUT" | grep -q "tau is alive" || fail "demo answer missing: $OUT"
echo "ok — faux model answered"

# --- step 1b: tool media results (F4, docs/tool-media.md) -------------
step "1b/11 tool media result (image block end to end)"
# The media-tool example's dot_png returns a text block plus a real
# 1x1 PNG image block (tau:extension@0.3.0). Assert the whole path:
# guest → host conversion → faux model's text projection shows the
# [image: …] marker, and the session JSONL persisted the image inline
# (68 bytes is under the blob threshold).
OUT="$("$TAU" --allow-unsigned -e "$MEDIA_TOOL" --demo --session media.jsonl -p "show me a dot" 2>&1)" || fail "media-tool run: $OUT"
echo "$OUT" | grep -q "tool ← dot_png" || fail "dot_png never executed: $OUT"
echo "$OUT" | grep -q "\[image: image/png\]" || fail "image block missing from the text projection: $OUT"
grep -q '"media_type":"image/png"' media.jsonl || fail "session JSONL lacks the image block: $(cat media.jsonl)"
grep -q "iVBORw0KGgo" media.jsonl || fail "PNG bytes not inline-base64 in the session: $(cat media.jsonl)"
echo "ok — image block crossed guest→host→model and persisted inline"

# --- step 1c: stream subscription (F2, docs/stream-subscribe.md) ------
step "1c/11 stream subscription (guest polls text deltas)"
# The streamer example subscribes to "text-delta" at session_start and
# polls at before_run_end, then notifies the count. The notice proves
# the full path: model delta → bus → per-subscription ring → guest poll
# → host.notify → renderer.
OUT="$("$TAU" --allow-unsigned -e "$STREAMER" --demo -p "hello" 2>&1)" || fail "streamer run: $OUT"
echo "$OUT" | grep -qE "ext info: stream observed: [1-9][0-9]* text deltas" || fail "stream observation notice missing or empty: $OUT"
echo "ok — guest polled the run's text deltas and reported them"

# --- step 1d: built-in tools (docs/builtin-tools.md) ------------------
step "1d/11 built-in tools (default set; a named one runs for real)"
# The default set is every built-in this platform has, and the run says
# which ones it registered — that line is the contract, and it is printed
# even when the answer is "none" (1e).
EXPECTED_TOOLS="bash, edit, find, grep, ls, read, write"
case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) EXPECTED_TOOLS="bash, edit, find, grep, ls, powershell, read, write" ;;
esac
OUT="$("$TAU" --demo -p "hello" 2>&1)" || fail "default run: $OUT"
echo "$OUT" | grep -qxF "[tau] built-in tools: $EXPECTED_TOOLS" || fail "default built-in line: $OUT"
# A named read-only built-in may be scripted by the demo, and the real
# tool runs: what comes back is a directory listing, not a canned answer.
OUT="$("$TAU" --tools ls --demo -p "." 2>&1)" || fail "tools ls run: $OUT"
echo "$OUT" | grep -qxF "[tau] built-in tools: ls" || fail "selection line: $OUT"
echo "$OUT" | grep -qF "tool ← ls: " || fail "ls never executed: $OUT"
echo "ok — built-ins are on by default, and a named one really ran"

# --- step 1e: built-in tools (off switch; --demo cannot run a shell) --
step "1e/11 built-in tools (off switch; the demo cannot run a shell)"
# --no-builtin-tools leaves the toolset to the components, and says so.
OUT="$("$TAU" --no-builtin-tools --demo -p "hello" 2>&1)" || fail "no-builtin-tools run: $OUT"
echo "$OUT" | grep -qxF "[tau] built-in tools: none" || fail "off switch line: $OUT"
# --demo may never script a mutating built-in, even when named: `--tools
# bash` puts bash in the registry, and the run still cannot write a file.
rm -f marker.txt
OUT="$("$TAU" --tools bash --demo -p "echo pwned > marker.txt" 2>&1)" || fail "tools bash run: $OUT"
echo "$OUT" | grep -q "tau is alive" || fail "tools bash run lost its answer: $OUT"
[ ! -f marker.txt ] || fail "--demo scripted a mutating built-in: marker.txt exists"
# A name nothing answers to stops the run rather than shrinking the
# toolset silently.
if "$TAU" --tools nope --demo -p "hello" > /dev/null 2>&1; then
    fail "--tools nope exited 0"
fi
echo "ok — the off switch works, the demo cannot run a shell, typos are fatal"

# --- step 1f: ACP mode over the pipes an editor uses (docs/acp.md) ----
step "1f/11 acp (a scripted client speaks the editor protocol)"
# A real client in python: handshake, session/new, a streamed prompt,
# a component's tool round trip, a cancel at rest, and --acp -p refused.
# It parses EVERY line tau writes to stdout, so a diagnostic that leaked
# into the protocol stream fails here.
python "$ROOT/scripts/acp_e2e.py" "$TAU" "$UPPER" "$WORK/acp-e2e" || fail "acp e2e failed"

# --- step 2: signing + trust chain ------------------------------------
step "2/11 signing and trust chain"
GEN="$("$TAU" keygen)" || fail "keygen: $GEN"
THROWAWAY_FP=$(echo "$GEN" | sed -n 's/^key generated and trusted: //p')
[ -n "$THROWAWAY_FP" ] || fail "no fingerprint in keygen output: $GEN"

cp "$UPPER" ext.wasm
"$TAU" sign ext.wasm --key "$THROWAWAY_FP" > /dev/null || fail "sign"
OUT="$("$TAU" -e ext.wasm --demo -p "shout validation" 2>&1)" || fail "signed load: $OUT"
echo "$OUT" | grep -q "tool ← upper: SHOUT VALIDATION" \
    || fail "signed tool loop did not close: $OUT"
echo "ok — signed component loads under the default RequireTrusted policy"

# The gate must reject what it cannot trust: same bytes, key dropped
# from the trust store. (A truly unsigned artifact is not available
# here — the repo's example .wasm files are signed — but an untrusted
# signature exercises the same RequireTrusted check.)
mv "$HOME/.tau/trust/$THROWAWAY_FP.pub" "$HOME/.tau/trust/$THROWAWAY_FP.pub.aside"
if "$TAU" -e ext.wasm --demo -p hi > /dev/null 2>&1; then
    mv "$HOME/.tau/trust/$THROWAWAY_FP.pub.aside" "$HOME/.tau/trust/$THROWAWAY_FP.pub"
    fail "untrusted component loaded — trust gate is open"
fi
mv "$HOME/.tau/trust/$THROWAWAY_FP.pub.aside" "$HOME/.tau/trust/$THROWAWAY_FP.pub"
echo "ok — untrusted component rejected"

# A caller-supplied "fingerprint" must never become a path: --key
# accepts only the canonical 16-hex shape, never ../-style input.
if "$TAU" sign ext.wasm --key ../escape > sign-key.out 2>&1; then
    fail "path-shaped --key accepted by tau sign"
fi
grep -q "not a signing fingerprint" sign-key.out \
    || fail "unexpected sign rejection: $(cat sign-key.out)"
# ...and as a bad KEY: key problems used to share the module parser's
# Malformed variant and render as "not a wasm binary".
if grep -q "not a wasm binary" sign-key.out; then
    fail "signing-key error misreported as a wasm parse error: $(cat sign-key.out)"
fi
echo "ok — tau sign --key rejects non-fingerprint input"

# A garbage pubkey must be refused as a bad KEY too — same history: the
# shared Malformed variant used to misreport it as "not a wasm binary".
OUT="$("$TAU" trust 'not-base64!!!' 2>&1 || true)"
echo "$OUT" | grep -q "invalid public key" \
    || fail "garbage pubkey error misleads: $OUT"
echo "ok — garbage pubkey refused with a key-shaped message"

# Tamper attacks on the signed component: however the bytes were corrupted
# after signing, the default gate must refuse them.
step "2b/11 signature tamper rejection"

# Flip one byte mid-module (far outside the trailing signature section):
# the digest no longer matches the signature.
python - << 'PYEOF'
with open("ext.wasm", "rb") as f:
    data = bytearray(f.read())
data[len(data) // 2] ^= 0xFF
with open("tampered-code.wasm", "wb") as f:
    f.write(bytes(data))
PYEOF
if "$TAU" -e tampered-code.wasm --demo -p hi 2> tampered-code.err; then
    fail "byte-flipped component loaded — verification is not checking the bytes"
fi
grep -q "signature does not verify" tampered-code.err \
    || fail "unexpected rejection: $(cat tampered-code.err)"
echo "ok — flipped byte in the module: signature does not verify"

# Strip the signature section outright: downgrades to unsigned, which the
# default policy refuses just the same.
python - << 'PYEOF'
with open("ext.wasm", "rb") as f:
    data = f.read()
out = bytearray(data[:8])
pos = 8
while pos < len(data):
    sec_id = data[pos]
    leb_start = pos
    pos += 1
    size = 0
    shift = 0
    while True:
        b = data[pos]
        pos += 1
        size |= (b & 0x7F) << shift
        shift += 7
        if not b & 0x80:
            break
    payload = data[pos:pos + size]
    # Custom-section name length is one LEB byte here ("tau-signature" < 128).
    is_sig = sec_id == 0 and payload[1:1 + payload[0]] == b"tau-signature"
    if not is_sig:
        out.extend(data[leb_start:pos + size])
    pos += size
with open("stripped.wasm", "wb") as f:
    f.write(bytes(out))
PYEOF
if "$TAU" -e stripped.wasm --demo -p hi 2> stripped.err; then
    fail "signature-stripped component loaded — unsigned downgrade passed the gate"
fi
grep -q "component is unsigned" stripped.err \
    || fail "unexpected rejection: $(cat stripped.err)"
echo "ok — stripped signature section: component is unsigned"

# Corrupt the signature payload itself: refused, not a crash or a bypass.
python - << 'PYEOF'
with open("ext.wasm", "rb") as f:
    data = bytearray(f.read())
data[-20] ^= 0xFF
with open("tampered-sig.wasm", "wb") as f:
    f.write(bytes(data))
PYEOF
if "$TAU" -e tampered-sig.wasm --demo -p hi 2> tampered-sig.err; then
    fail "corrupted signature section loaded — the gate trusts broken signatures"
fi
grep -q "signature" tampered-sig.err \
    || fail "unexpected rejection: $(cat tampered-sig.err)"
echo "ok — corrupted signature payload refused"

# --allow-unsigned is no laundering path: a component that carries a
# signature section which does not verify is tampered, not unsigned.
if "$TAU" --allow-unsigned -e tampered-sig.wasm --demo -p hi 2> tampered-au.err; then
    fail "--allow-unsigned loaded a corrupted signature — the escape hatch launders tampering"
fi
grep -q "signature" tampered-au.err \
    || fail "unexpected rejection: $(cat tampered-au.err)"
echo "ok — --allow-unsigned still refuses a corrupted signature"

# --- step 3: built-in provider against a loopback SSE mock -------------
step "3/11 built-in providers (loopback SSE mock)"
cat > mock.py << 'PYEOF'
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

SSE_BODY = (
    'data: {"choices":[{"delta":{"content":"mock "}}]}\n\n'
    'data: {"choices":[{"delta":{"content":"provider ok"}}]}\n\n'
    'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n\n'
    "data: [DONE]\n\n"
)


RESPONSES_SSE_BODY = (
    'data: {"type":"response.output_text.delta","delta":"mock "}\n\n'
    'data: {"type":"response.output_text.delta","delta":"responses ok"}\n\n'
    'data: {"type":"response.completed"}\n\n'
    "data: [DONE]\n\n"
)


ANTHROPIC_SSE_BODY = (
    'data: {"type":"message_start","message":{"id":"m1","role":"assistant"}}\n\n'
    'data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}\n\n'
    'data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"mock "}}\n\n'
    'data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"anthropic ok"}}\n\n'
    'data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}\n\n'
)


# Step 3b: the request that does not carry a tool result yet is answered
# with an `ls` call split across two deltas (so the partial-JSON
# assembler is exercised); the request that does carry one is answered
# with plain text. json.dumps builds the payloads, so the nested
# escaping is the encoder's problem rather than the reader's.
def _chunk(obj):
    return "data: " + json.dumps(obj) + "\n\n"


TOOLCALL_SSE_BODY = (
    _chunk({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_ls_1", "function": {"name": "ls", "arguments": ""}}]}}]})
    + _chunk({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": '{"path":"."}'}}]}}]})
    + _chunk({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
    + "data: [DONE]\n\n"
)


TOOLCALL_FINAL_SSE_BODY = (
    _chunk({"choices": [{"delta": {"content": "listed "}}]})
    + _chunk({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]})
    + "data: [DONE]\n\n"
)


# The same round trip on the Anthropic wire, whose tool results ride back
# as a `tool_result` block in a user message rather than a `tool` role.
TOOLCALL_ANTHROPIC_SSE_BODY = (
    _chunk({"type": "message_start", "message": {"id": "m1", "role": "assistant"}})
    + _chunk({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_ls_1", "name": "ls"}})
    + _chunk({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": '{"path":"."}'}})
    + _chunk({"type": "content_block_stop", "index": 0})
    + _chunk({"type": "message_delta", "delta": {"stop_reason": "tool_use"}})
    + "data: [DONE]\n\n"
)


TOOLCALL_ANTHROPIC_FINAL_SSE_BODY = (
    _chunk({"type": "message_start", "message": {"id": "m2", "role": "assistant"}})
    + _chunk({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
    + _chunk({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "listed ok"}})
    + _chunk({"type": "message_delta", "delta": {"stop_reason": "end_turn"}})
    + "data: [DONE]\n\n"
)


class ChatHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length", 0)))
        if "/toolcall" in self.path:
            # Side channel for step 3b: every provider request body, one
            # JSON document per line — the leg asserts on what the model
            # was advertised and what came back to it.
            with open("toolcall_requests.jsonl", "ab") as f:
                f.write(raw + b"\n")
            if "/toolcall-anthropic" in self.path:
                body = (
                    TOOLCALL_ANTHROPIC_FINAL_SSE_BODY
                    if b'"type":"tool_result"' in raw
                    else TOOLCALL_ANTHROPIC_SSE_BODY
                ).encode()
            else:
                body = (
                    TOOLCALL_FINAL_SSE_BODY if b'"role":"tool"' in raw else TOOLCALL_SSE_BODY
                ).encode()
        elif "/skills" in self.path:
            # Side channel for step 3c: what the model was told about the
            # working directory — project instructions, the skills
            # manifest, and (asserted absent) the skill bodies.
            with open("skills_requests.jsonl", "ab") as f:
                f.write(raw + b"\n")
            body = SSE_BODY.encode()
        elif self.path.endswith("/responses"):
            body = RESPONSES_SSE_BODY.encode()
        elif self.path.endswith("/messages"):
            body = ANTHROPIC_SSE_BODY.encode()
        else:
            body = SSE_BODY.encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class HelloHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        # Side channel: record the Authorization header of every request
        # so the credential-delivery step can prove what the origin saw.
        with open("auth_capture.log", "a") as f:
            f.write(str(self.headers.get("authorization")) + "\n")
        if self.path.startswith("/quiet"):
            # Headers, then silence, connection held open: the half-open
            # peer the idle budget exists for (wit-review F9).
            self.send_response(200)
            self.send_header("content-type", "text/plain")
            self.send_header("content-length", "100")
            self.end_headers()
            time.sleep(3)
            return
        if self.path.startswith("/mute"):
            # Accepted, then silence before any header at all — the peer the
            # request-headers budget exists for (wit-review F11).
            time.sleep(3)
            return
        if self.path.startswith("/redirect"):
            # A consent-escaping redirect: the client must NOT follow it.
            self.send_response(302)
            self.send_header("location", "http://evil.invalid/loot")
            self.send_header("content-length", "0")
            self.end_headers()
            return
        body = b"hello from mock origin"
        self.send_response(200)
        self.send_header("content-type", "text/plain")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


threading.Thread(
    target=HTTPServer(("127.0.0.1", 8401), ChatHandler).serve_forever, daemon=True
).start()
threading.Thread(
    target=HTTPServer(("127.0.0.1", 8402), HelloHandler).serve_forever, daemon=True
).start()
print("mocks ready", flush=True)
threading.Event().wait()
PYEOF
python mock.py > mock.log 2>&1 &
MOCK_PID=$!
for _ in $(seq 1 20); do
    grep -q "mocks ready" mock.log 2> /dev/null && break
    sleep 0.5
done
grep -q "mocks ready" mock.log || fail "mock servers did not start: $(cat mock.log)"

OUT=$(OPENAI_API_KEY=dummy OPENAI_BASE_URL=http://127.0.0.1:8401/v1 TAU_MODEL=mock-model \
    "$TAU" -p "say something" 2>&1) || fail "built-in provider: $OUT"
echo "$OUT" | grep -q "mock provider ok" || fail "SSE stream did not land: $OUT"
echo "ok — chat-completions SSE streamed end to end"

OUT=$(OPENAI_API_KEY=dummy OPENAI_BASE_URL=http://127.0.0.1:8401/v1 TAU_MODEL=mock-model \
    "$TAU" --provider responses -p "say something" 2>&1) || fail "responses provider: $OUT"
echo "$OUT" | grep -q "mock responses ok" || fail "Responses SSE stream did not land: $OUT"
echo "ok — Responses API SSE streamed end to end"

OUT=$(ANTHROPIC_API_KEY=dummy ANTHROPIC_BASE_URL=http://127.0.0.1:8401 TAU_MODEL=mock-model \
    "$TAU" --provider anthropic -p "say something" 2>&1) || fail "anthropic provider: $OUT"
echo "$OUT" | grep -q "mock anthropic ok" || fail "Anthropic SSE stream did not land: $OUT"
echo "ok — Anthropic Messages SSE streamed end to end"

# --- step 3b: built-in tools reach the provider wire ------------------
step "3b/11 built-in tools on the provider wire (tool call round trip)"
# 1d proves the registry; this proves the wire. The request the agent
# builds advertises every built-in it registered, and a tool call the
# model asks for really runs and really comes back as a tool message —
# the one leg where the model is not the faux one. The expected set is
# 1d's EXPECTED_TOOLS: the startup line and the wire must agree.
TOOLCALL_DIR="$WORK/toolcall"
rm -rf "$TOOLCALL_DIR" && mkdir -p "$TOOLCALL_DIR"
touch "$TOOLCALL_DIR/tau-3b-marker.txt"
rm -f toolcall_requests.jsonl
# `--provider` is explicit on both legs: the assertions below are
# wire-shaped (chat-completions advertises `tools`; Messages shapes it
# differently), so the leg must not inherit an ambient key's inference.
OUT="$(cd "$TOOLCALL_DIR" && OPENAI_API_KEY=dummy \
    OPENAI_BASE_URL=http://127.0.0.1:8401/v1/toolcall TAU_MODEL=mock-model \
    "$TAU" --provider openai -p "list the files here" 2>&1)" || fail "tool call round trip: $OUT"
echo "$OUT" | grep -q "listed ok" || fail "the final answer never landed: $OUT"
[ -f toolcall_requests.jsonl ] || fail "the mock captured no provider request"
REQS="$(grep -c . toolcall_requests.jsonl || true)"
[ "$REQS" -eq 2 ] || fail "expected two provider requests (the call, then its result), saw $REQS"
FIRST="$(sed -n '1p' toolcall_requests.jsonl)"
SECOND="$(sed -n '2p' toolcall_requests.jsonl)"
echo "$FIRST" | grep -qF '"tools":[' || fail "the request advertises no tools: $FIRST"
for name in $(echo "$EXPECTED_TOOLS" | tr -d ','); do
    echo "$FIRST" | grep -qF '"name":"'"$name"'"' \
        || fail "$name is registered but was never advertised to the provider"
done
echo "$SECOND" | grep -qF '"role":"tool"' || fail "the tool result never reached the provider: $SECOND"
echo "$SECOND" | grep -qF 'tau-3b-marker.txt' || fail "the ls output is missing from the tool result: $SECOND"
echo "$SECOND" | grep -qF '"id":"call_ls_1"' || fail "the tool call id did not round-trip: $SECOND"
echo "ok — every built-in is advertised, and a model-asked ls call closed the loop"

# The Anthropic wire closes the same loop through a different shape: the
# result rides back as a `tool_result` block in a user message, and the
# stop that ends the first turn is `tool_use` rather than a finish_reason.
# Both wires had the same defect — a trailing fallback `stop` overwrote
# the provider's own word, so the call was assembled, persisted and never
# executed — and this leg is what caught it.
rm -f toolcall_requests.jsonl
OUT="$(cd "$TOOLCALL_DIR" && ANTHROPIC_API_KEY=dummy \
    ANTHROPIC_BASE_URL=http://127.0.0.1:8401/v1/toolcall-anthropic TAU_MODEL=mock-model \
    "$TAU" --provider anthropic -p "list the files here" 2>&1)" \
    || fail "anthropic tool call round trip: $OUT"
echo "$OUT" | grep -q "listed ok" || fail "the anthropic final answer never landed: $OUT"
REQS="$(grep -c . toolcall_requests.jsonl || true)"
[ "$REQS" -eq 2 ] || fail "expected two anthropic requests, saw $REQS"
FIRST="$(sed -n '1p' toolcall_requests.jsonl)"
SECOND="$(sed -n '2p' toolcall_requests.jsonl)"
for name in $(echo "$EXPECTED_TOOLS" | tr -d ','); do
    echo "$FIRST" | grep -qF '"name":"'"$name"'"' \
        || fail "$name was never advertised to the anthropic provider"
done
echo "$SECOND" | grep -qF '"type":"tool_result"' \
    || fail "the anthropic tool result never reached the provider: $SECOND"
echo "$SECOND" | grep -qF '"tool_use_id":"toolu_ls_1"' || fail "the anthropic tool_use id did not round-trip: $SECOND"
echo "$SECOND" | grep -qF 'tau-3b-marker.txt' \
    || fail "the ls output is missing from the anthropic tool result: $SECOND"
echo "ok — the Anthropic wire closes the same loop"

# --- step 3c: skills and project instructions (docs/skills.md) --------
step "3c/11 skills discovery (manifest and AGENTS.md on the wire; body on demand)"
# A directory shaped like a project: a skill under the `.agents`
# convention, instructions in it and one level down, and a `.git` marker
# so the walk up stops here — the tree lives inside the checkout, whose
# own files must not leak into the assertions.
SKILLS_ROOT="$WORK/skills"
rm -rf "$SKILLS_ROOT"
mkdir -p "$SKILLS_ROOT/.git" "$SKILLS_ROOT/sub" "$SKILLS_ROOT/.agents/skills/hello"
cat > "$SKILLS_ROOT/.agents/skills/hello/SKILL.md" << 'SKILLEOF'
---
name: hello
description: greets the reader in a set way
---

# Hello

BODY-MARKER-8431
SKILLEOF
echo 'root guidance' > "$SKILLS_ROOT/AGENTS.md"
echo 'inner guidance' > "$SKILLS_ROOT/sub/AGENTS.md"

# The startup log says what the directory offered, and the instructions
# are listed root-first — the repository's guidance, then the nearer
# file. From a subdirectory with no skills of its own, the skills line
# reads `none`: the skill roots are the working directory's, not the
# tree's (docs/skills.md, "Deliberate limits").
OUT="$(cd "$SKILLS_ROOT/sub" && "$TAU" --demo -p "hello" 2>&1)" || fail "subdirectory run: $OUT"
echo "$OUT" | grep -qxF "[tau] skills: none" \
    || fail "a subdirectory offers no skills and must say so: $OUT"
ROOT_LINE="$(echo "$OUT" | grep -nE 'skills.AGENTS[.]md' | sed -n '1p' | cut -d: -f1 || true)"
INNER_LINE="$(echo "$OUT" | grep -nE 'sub.AGENTS[.]md' | sed -n '1p' | cut -d: -f1 || true)"
[ -n "$ROOT_LINE" ] && [ -n "$INNER_LINE" ] \
    || fail "both AGENTS.md files must be reported: $OUT"
[ "$ROOT_LINE" -lt "$INNER_LINE" ] \
    || fail "the parent's instructions must be listed first: $OUT"

# What the model is actually told: the manifest and the instructions are
# in the system prompt, the skill's body is not (it is what load_skill
# is for), and the tool that serves it is advertised. The mock captures
# the request; this is the only leg where the system prompt is read.
rm -f skills_requests.jsonl
OUT="$(cd "$SKILLS_ROOT" && OPENAI_API_KEY=dummy \
    OPENAI_BASE_URL=http://127.0.0.1:8401/v1/skills TAU_MODEL=mock-model \
    "$TAU" --provider openai -p "hello" 2>&1)" || fail "skills wire run: $OUT"
echo "$OUT" | grep -qxF "[tau] skills: hello" \
    || fail "the skill was not discovered in the working directory: $OUT"
[ -f skills_requests.jsonl ] || fail "the mock captured no provider request"
REQ="$(sed -n '1p' skills_requests.jsonl)"
[ -n "$REQ" ] || fail "the captured provider request is empty"
echo "$REQ" | grep -qF 'greets the reader in a set way' \
    || fail "the skill is not named in the model's context: $REQ"
echo "$REQ" | grep -qF 'root guidance' \
    || fail "the project instructions are not in the model's context: $REQ"
echo "$REQ" | grep -qF '"name":"load_skill"' \
    || fail "load_skill was not advertised to the provider: $REQ"
if echo "$REQ" | grep -qF 'BODY-MARKER-8431'; then
    fail "the skill body was inlined instead of left for load_skill: $REQ"
fi

# And the load path closes: the demo may script a named read-only
# built-in, and load_skill reads the skill the prompt named.
OUT="$(cd "$SKILLS_ROOT" && "$TAU" --tools load_skill --demo -p "hello" 2>&1)" \
    || fail "load_skill run: $OUT"
echo "$OUT" | grep -qxF "[tau] built-in tools: load_skill" \
    || fail "the selection line is wrong: $OUT"
echo "$OUT" | grep -qF "tool ← load_skill: " || fail "load_skill never executed: $OUT"
echo "$OUT" | grep -qF 'BODY-MARKER-8431' || fail "the body never came back: $OUT"

# A body the model cannot load is worse than no body: with the tool
# selected away, the manifest goes with it.
rm -f skills_requests.jsonl
OUT="$(cd "$SKILLS_ROOT" && OPENAI_API_KEY=dummy \
    OPENAI_BASE_URL=http://127.0.0.1:8401/v1/skills TAU_MODEL=mock-model \
    "$TAU" --provider openai --no-builtin-tools -p "hello" 2>&1)" \
    || fail "no-builtin-tools wire run: $OUT"
[ -f skills_requests.jsonl ] || fail "the mock captured no provider request"
REQ="$(sed -n '1p' skills_requests.jsonl)"
echo "$REQ" | grep -qF 'root guidance' \
    || fail "the instructions must go in either way: $REQ"
if echo "$REQ" | grep -qF 'greets the reader in a set way'; then
    fail "the manifest was advertised without the tool that serves it: $REQ"
fi
echo "ok — the manifest and AGENTS.md reach the model, the body waits for load_skill"

# --- step 4: wasm provider consent gate --------------------------------
step "4/11 wasm provider consent gate"
if "$TAU" --allow-unsigned \
    --provider-wasm "$HTTP_PROVIDER" --model http \
    -p "http://127.0.0.1:8402/" 2>&1 | grep -q "STATUS 200"; then
    fail "http fetch succeeded with no consent — consent gate is open"
fi
echo "ok — no consent: fetch denied"

OUT="$("$TAU" --allow-unsigned \
    --provider-wasm "$HTTP_PROVIDER" --model http \
    --provider-origin http://127.0.0.1:8402 \
    -p "http://127.0.0.1:8402/" 2>&1)" || fail "consented fetch: $OUT"
echo "$OUT" | grep -q "STATUS 200: hello from mock origin" \
    || fail "consented fetch did not land: $OUT"
echo "ok — with --provider-origin the fetch flows"

# A redirect would escape consent: the consented origin answers 302 to
# an unconsented host, and the guest must see the 302, not the target.
OUT="$("$TAU" --allow-unsigned \
    --provider-wasm "$HTTP_PROVIDER" --model http \
    --provider-origin http://127.0.0.1:8402 \
    -p "http://127.0.0.1:8402/redirect" 2>&1)" || fail "redirect run: $OUT"
echo "$OUT" | grep -q "STATUS 302" \
    || fail "redirect was followed — consent escaped: $OUT"
echo "ok — consented origin's 302 is shown, never followed"

# A peer that sends headers and then goes quiet must surface as an error,
# not a hang: the body's idle budget is the HOST's since 0.7.0 (the guest
# no longer passes a timeout), so the knob shortens it for this gate. The
# reason goes to stderr and the guest's stream simply ends (wit-review F9).
OUT="$(TAU_HTTP_IDLE_TIMEOUT_MS=300 "$TAU" --allow-unsigned \
    --provider-wasm "$HTTP_PROVIDER" --model http \
    --provider-origin http://127.0.0.1:8402 \
    -p "http://127.0.0.1:8402/quiet" 2>&1 || true)"
echo "$OUT" | grep -q "no bytes within 300ms" \
    || fail "idle budget never fired — the body stream blocked or swallowed it: $OUT"
echo "ok — a quiet peer ends the body stream with an explicit timeout"

# A peer that accepts the connection and then sends NOTHING — no headers,
# no error, no FIN — must surface as an error too: the wait for response
# headers carries its own bound, separate from the body's, and it is the
# HOST's bound as well since 0.7.0 (wit-review F11). This failure reaches
# the guest as the import's error, so the model output is what names it.
OUT="$(TAU_HTTP_REQUEST_TIMEOUT_MS=300 "$TAU" --allow-unsigned \
    --provider-wasm "$HTTP_PROVIDER" --model http \
    --provider-origin http://127.0.0.1:8402 \
    -p "http://127.0.0.1:8402/mute" 2>&1 || true)"
echo "$OUT" | grep -q "no response headers within 300ms" \
    || fail "headers budget never fired — request blocked or swallowed it: $OUT"
echo "ok — a peer that never sends headers returns an explicit timeout"

# Origin matching sees the same host the client dials: userinfo inside
# the authority is stripped (flows), a backslash after the authority is
# path material (stays on the consented host), and the delimiter tricks
# plus the trailing-dot twin are refused as unconsented.
OUT="$("$TAU" --allow-unsigned \
    --provider-wasm "$HTTP_PROVIDER" --model http \
    --provider-origin http://127.0.0.1:8402 \
    -p "http://user:pw@127.0.0.1:8402/" 2>&1)" || fail "userinfo run: $OUT"
echo "$OUT" | grep -q "STATUS 200: hello" \
    || fail "userinfo-inside-authority fetch did not land: $OUT"
OUT="$("$TAU" --allow-unsigned \
    --provider-wasm "$HTTP_PROVIDER" --model http \
    --provider-origin http://127.0.0.1:8402 \
    -p 'http://127.0.0.1:8402\@evil.invalid/' 2>&1)" || fail "backslash run: $OUT"
echo "$OUT" | grep -q "STATUS 200: hello" \
    || fail "backslash-after-authority left the consented host: $OUT"
for evil in 'http://evil.invalid\@127.0.0.1:8402/' \
            'http://evil.invalid?@127.0.0.1:8402/' \
            'http://127.0.0.1:8402./'; do
    OUT="$("$TAU" --allow-unsigned \
        --provider-wasm "$HTTP_PROVIDER" --model http \
        --provider-origin http://127.0.0.1:8402 \
        -p "$evil" 2>&1 || true)"
    echo "$OUT" | grep -q "not in consent allowlist" \
        || fail "consent bypass reached the network: $evil → $OUT"
    if echo "$OUT" | grep -q "STATUS 200"; then
        fail "consent bypass was served: $evil"
    fi
done
echo "ok — origin gate: userinfo/backslash stay home, tricks and twins refused"

# The load contract is enforced: an unadvertised model id is refused at
# load, naming the ids the component actually lists — never silently
# running whatever the guest does with a model it does not advertise.
OUT="$("$TAU" --allow-unsigned \
    --provider-wasm "$HTTP_PROVIDER" --model nosuch \
    --provider-origin http://127.0.0.1:8402 \
    -p "http://127.0.0.1:8402/" 2>&1 || true)"
echo "$OUT" | grep -q "model 'nosuch' not provided" \
    || fail "unknown model id was not refused: $OUT"
echo "$OUT" | grep -q "available: http" \
    || fail "refusal does not name the available ids: $OUT"
echo "ok — unknown model id refused at load, available ids named"

# --- step 4b: large payload over the component boundary ----------------
step "4b/11 large media crosses the component boundary intact"
# A 3 MiB image in the session history must cross the session →
# materialize → wasm boundary whole — not choked, truncated, or refused.
# Since 0.7.0 the echo provider's "probe" keyword reports the byte length
# and the FNV-1a of the RAW media bytes it received (the serialized
# request JSON is a host-side wire detail now; the guest sees typed
# records). A length alone is a weak leg, so the script recomputes the
# checksum over the very bytes it sent: corruption is a mismatch,
# truncation a short count. (tau-ext's large_payload_tests pin the same
# length+checksum contract against a hand-built component; here the real
# CLI drives it.)
python - << 'PYEOF'
import base64, json, random
random.seed()
data = random.randbytes(3 * 1024 * 1024)
msg = {
    "id": "big", "parent": None, "type": "message",
    "message": {"role": "user", "content": [
        {"type": "text", "text": "big media attached"},
        {"type": "image", "media": {"media_type": "image/png",
            "source": "base64", "data": base64.b64encode(data).decode()}},
    ]},
}
with open("big-session.jsonl", "w") as f:
    f.write(json.dumps(msg) + "\n")
h = 0xcbf29ce484222325
for b in data:
    h ^= b
    h = (h * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
with open("big-fnv.txt", "w") as f:
    f.write("%016x" % h)
PYEOF
OUT="$("$TAU" --allow-unsigned \
    --provider-wasm "$ECHO_PROVIDER" --model echo \
    --session big-session.jsonl --continue \
    -p "probe" 2>&1)" || fail "large-payload run: $OUT"
BYTES=$(echo "$OUT" | grep -o 'bytes=[0-9]*' | head -1 | cut -d= -f2)
[ -n "$BYTES" ] || fail "guest never reported the payload size: $OUT"
[ "$BYTES" -eq 3145728 ] \
    || fail "payload truncated at the boundary: guest saw only $BYTES bytes"
echo "$OUT" | grep -q "fnv1a=$(cat big-fnv.txt)" \
    || fail "payload corrupted at the boundary (checksums differ): $OUT"
echo "ok — 3 MiB of media crossed session→guest whole ($BYTES bytes, FNV-1a matches)"

# --- step 5: MCP bridge (consent-gated spawn) --------------------------
step "5/11 MCP bridge (consent-gated spawn)"
# Without --mcp-command the bridge has nothing it may spawn: the load
# must fail, not silently degrade.
if "$TAU" --allow-unsigned --mcp-bridge "$MCP_BRIDGE" --demo -p hi > /dev/null 2>&1; then
    fail "bridge ran with no command consent — spawn gate is open"
fi
echo "ok — no command consent: bridge load refused"

# With the command granted (passing it IS the consent), the mock MCP
# server comes up, its echo tool registers, and the demo drives it.
# Windows python needs a Windows-form path with forward slashes.
ROOT_WIN=$(cygpath -m "$ROOT" 2> /dev/null || echo "$ROOT")
OUT="$("$TAU" --allow-unsigned --mcp-bridge "$MCP_BRIDGE" \
    --mcp-command "[\"python\",\"$ROOT_WIN/examples/mcp-bridge/mock_server.py\"]" \
    --demo -p "bridge validation ok" 2>&1)" || fail "bridge run: $OUT"
echo "$OUT" | grep -q "tool ← echo: bridge validation ok" \
    || fail "bridged echo tool did not close the loop: $OUT"
echo "ok — consent-gated spawn served the echo tool through the MCP bridge"

# A server that never reads its stdin must not wedge the bridge (wit-review
# F12). The wait is the HOST budget since 0.7.0 — a wasm guest has no clock
# it can await — so the gate shortens it with the knob: the host stdin sink
# gives up on a queue nobody drains, the guest write comes back with the
# unwritten remainder, and the bridge names the stall in the tool result.
# TAU_MCP_PAD inflates the request past the host buffer — no argv can carry
# that many bytes.
OUT="$(TAU_MCP_PAD=200000 TAU_PROCESS_STDIN_IDLE_TIMEOUT_MS=300 "$TAU" --allow-unsigned \
    --mcp-bridge "$MCP_BRIDGE" \
    --mcp-command "[\"python\",\"$ROOT_WIN/examples/mcp-bridge/mock_server.py\",\"--mute-stdin\"]" \
    --demo -p "write stall validation" 2>&1 || true)"
echo "$OUT" | grep -q "tau process.stdin: the child took nothing for 300ms" \
    || fail "the host stdin budget never fired: $OUT"
echo "$OUT" | grep -q "the server stopped taking stdin" \
    || fail "a server that never reads stdin was not reported: $OUT"
echo "ok — a server that never reads stdin surfaces as a named stall, not a hang"

# --- step 5b: ws capability (frame pipe, docs/im-channels.md) ---------
step "5b/11 ws capability (frame pipe over a loopback echo server)"
python "$ROOT/scripts/ws_echo_mock.py" > ws_mock.log 2>&1 &
WS_MOCK_PID=$!
for _ in $(seq 1 20); do
    grep -q "ws echo ready" ws_mock.log 2> /dev/null && break
    sleep 0.5
done
WS_PORT=$(sed -n 's/^ws echo ready //p' ws_mock.log)
[ -n "$WS_PORT" ] || fail "ws echo mock did not start: $(cat ws_mock.log)"
OUT="$("$TAU" --allow-unsigned --mcp-bridge "$WS_ECHO"     --mcp-url "ws://127.0.0.1:$WS_PORT/echo" --demo -p "echo frame-pipe-ok via ws_echo" 2>&1)" || fail "ws run: $OUT"
kill "$WS_MOCK_PID" 2> /dev/null || true
# The faux model decides the exact payload wording; assert the round
# trip (echo: prefix = frame came back) and the payload's survival.
echo "$OUT" | grep -qE "tool ← ws_echo: echo: .*frame-pipe-ok" || fail "ws echo did not close the loop: $OUT"
echo "ok — frame crossed guest→host→ws→echo→back (consent-gated origin)"

# A listener that accepts the TCP connection and never upgrades it: the
# handshake wait is the HOST's budget since 0.7.0 (the guest no longer
# passes one), so the gate shortens it with the knob instead. Failure
# must still be loud rather than park the run (wit-review F11 —
# tungstenite's connect does transport + upgrade in one unbounded
# blocking call).
python - <<'MUTE' > ws_mute.log 2>&1 &
import socket, time
srv = socket.socket()
srv.bind(("127.0.0.1", 0))
srv.listen(4)
print("ws mute ready", srv.getsockname()[1], flush=True)
while True:
    conn, _ = srv.accept()
    time.sleep(60)
MUTE
WS_MUTE_PID=$!
for _ in $(seq 1 20); do
    grep -q "ws mute ready" ws_mute.log 2> /dev/null && break
    sleep 0.5
done
WS_MUTE_PORT=$(sed -n 's/^ws mute ready //p' ws_mute.log)
[ -n "$WS_MUTE_PORT" ] || fail "ws mute listener did not start: $(cat ws_mute.log)"
OUT="$(TAU_WS_CONNECT_TIMEOUT_MS=5000 "$TAU" --allow-unsigned --mcp-bridge "$WS_ECHO" \
    --mcp-url "ws://127.0.0.1:$WS_MUTE_PORT/echo" --demo \
    -p "never answers via ws_echo" 2>&1 || true)"
kill "$WS_MUTE_PID" 2> /dev/null || true
echo "$OUT" | grep -q "no handshake within 5000ms" \
    || fail "ws handshake budget never fired: $OUT"
echo "ok — a listener that never upgrades returns an explicit timeout"

# --- step 5c: IM loopback (feishu-shaped adapter, docs/im-channels.md) -
step "5c/11 IM loopback (ws inbound steer + after_response reply POST)"
python "$ROOT/scripts/im_mock.py" > im_mock.log 2>&1 &
IM_MOCK_PID=$!
for _ in $(seq 1 20); do
    grep -q "im mock ready" im_mock.log 2> /dev/null && break
    sleep 0.5
done
IM_PORT=$(sed -n 's/^im mock ready //p' im_mock.log)
[ -n "$IM_PORT" ] || fail "im mock did not start: $(cat im_mock.log)"
# Consented: ws origin covers the connect AND the reply POST (same
# origin), --allow-inject consents the steer. Semantic anchors only —
# the faux model owns the reply wording. The mapping config file
# (docs/im-channels.md) is written once the mock's port is known — the
# channel endpoint must equal the consented TAU_MCP_URL.
cat > im-config.json <<CONFIG
{"version": 1, "channels": [{"id": "feishu-loopback", "platform": "feishu",
  "endpoint": "ws://127.0.0.1:$IM_PORT/im",
  "chats": {"loopback-c1": {"session": ".tau/sessions/feishu-loopback-c1.jsonl", "threads": "branch"}},
  "users": {"allow": ["loopback-user"]}}]}
CONFIG
OUT="$(TAU_IM_CONFIG="$WORK/im-config.json" "$TAU" --allow-unsigned --mcp-bridge "$FEISHU"     --mcp-url "ws://127.0.0.1:$IM_PORT/im" --allow-inject --demo -p "hi" 2>&1)" || fail "im run: $OUT"
echo "$OUT" | grep -q "steer: .IM chat loopback-c1"     || fail "IM message was not steered into the session: $OUT"
echo "$OUT" | grep -q "chat loopback-c1 → .tau/sessions/feishu-loopback-c1.jsonl"     || fail "session mapping did not come from the config file: $OUT"
echo "$OUT" | grep -q "feishu: reply posted to loopback-c1"     || fail "reply was not posted back: $OUT"
grep -q "IM REPLY: " im_mock.log     || fail "mock never received the reply POST: $(cat im_mock.log)"
grep -q "loopback-c1" im_mock.log     || fail "reply lost the chat mapping: $(cat im_mock.log)"
kill "$IM_MOCK_PID" 2> /dev/null || true
# Identity leg: the same config but the platform message comes from a
# user outside users.allow — consumed, noted, never steered, no reply.
IM_MOCK_USER=intruder-user python "$ROOT/scripts/im_mock.py" > im_mock_intruder.log 2>&1 &
IM_MOCK_PID=$!
for _ in $(seq 1 20); do
    grep -q "im mock ready" im_mock_intruder.log 2> /dev/null && break
    sleep 0.5
done
IM_PORT=$(sed -n 's/^im mock ready //p' im_mock_intruder.log)
[ -n "$IM_PORT" ] || fail "im mock (identity leg) did not start: $(cat im_mock_intruder.log)"
sed "s/endpoint\": \"ws:\/\/127.0.0.1:[0-9]*/endpoint\": \"ws:\/\/127.0.0.1:$IM_PORT/" im-config.json > im-config-intruder.json
OUT="$(TAU_IM_CONFIG="$WORK/im-config-intruder.json" "$TAU" --allow-unsigned --mcp-bridge "$FEISHU"     --mcp-url "ws://127.0.0.1:$IM_PORT/im" --allow-inject --demo -p "hi" 2>&1)" || fail "im identity run: $OUT"
kill "$IM_MOCK_PID" 2> /dev/null || true
echo "$OUT" | grep -q "feishu: ignored message (chat loopback-c1 configured: true, user intruder-user allowed: false)"     || fail "unauthorized user was not ignored: $OUT"
echo "$OUT" | grep -q "steer: .IM chat"     && fail "unauthorized user was steered into the session: $OUT"
grep -q "IM REPLY: " im_mock_intruder.log     && fail "unauthorized user got a reply: $(cat im_mock_intruder.log)"
# Refusal path: without --allow-inject the steer must fail loud and no
# reply may leave. Fresh mock + fresh log (truncating a live mock's log
# file fights its open fd).
python "$ROOT/scripts/im_mock.py" > im_mock2.log 2>&1 &
IM_MOCK_PID=$!
for _ in $(seq 1 20); do
    grep -q "im mock ready" im_mock2.log 2> /dev/null && break
    sleep 0.5
done
IM_PORT=$(sed -n 's/^im mock ready //p' im_mock2.log)
[ -n "$IM_PORT" ] || fail "im mock (refusal leg) did not start: $(cat im_mock2.log)"
# The refusal leg carries an authorized config too — without it the
# fail-closed identity gate eats the message before steer is even
# attempted, and this leg would stop measuring injection consent.
sed "s/endpoint\": \"ws:\/\/127.0.0.1:[0-9]*/endpoint\": \"ws:\/\/127.0.0.1:$IM_PORT/" im-config.json > im-config-refusal.json
OUT="$(TAU_IM_CONFIG="$WORK/im-config-refusal.json" "$TAU" --allow-unsigned --mcp-bridge "$FEISHU"     --mcp-url "ws://127.0.0.1:$IM_PORT/im" --demo -p "hi" 2>&1)" || fail "im refusal run: $OUT"
kill "$IM_MOCK_PID" 2> /dev/null || true
echo "$OUT" | grep -q "feishu: steer refused: session injection not consented"     || fail "unconsented steer was not refused: $OUT"
grep -q "IM REPLY: " im_mock2.log     && fail "reply left without inject consent: $(cat im_mock2.log)"
echo "ok — IM loop closed (ws inbound → steer → turn → reply POST); config-gated identity: unauthorized user ignored; unconsented steer refused"

# --- step 5d: webhook ingress (whatsapp-shaped adapter, docs/im-channels.md)
# The dual of 5c's ws leg: WASI has no listen, so the host binds the
# consented --ingress address and pushes each webhook request into the
# component's ingress-handler export. Delivery lands while the session
# IDLES — the acceptance must be the interactive REPL over a pty (print
# mode's sub-second run would be a race), so it needs pywinpty like 11.
step "5d/11 webhook ingress (host listener → ingress-handler → idle REPL wakes → reply POST)"
if python -c "import winpty" 2> /dev/null; then
    mkdir -p "$WORK/wa-e2e"
    python "$ROOT/scripts/wa_ingress_e2e.py" "$TAU" "$WHATSAPP" "$WORK/wa-e2e"         || fail "whatsapp ingress e2e failed"
else
    echo "skip — pywinpty not installed; webhook ingress e2e not run"
fi

# --- step 5e: wecom ingress with the crypto gate (docs/im-channels.md)
# 5d proved the pipe; 5e proves the component-side red line: signature
# verification and AES decryption live in the guest (the mock signs and
# encrypts for real, NIST-self-tested). The no-consent refusal leg is
# 5d's (same consent gate); 5e's assertions are the crypto legs.
step "5e/11 wecom webhook (bad-signature 403 → echostr round-trip → encrypted message → reply)"
if python -c "import winpty" 2> /dev/null; then
    python "$ROOT/scripts/wecom_ingress_e2e.py" "$TAU" "$WECOM" "$WORK/wecom-e2e"         || fail "wecom ingress e2e failed"
else
    echo "skip — pywinpty not installed; wecom ingress e2e not run"
fi

# --- step 5f: dingtalk stream loopback (docs/im-channels.md) ----------
# The ws family's second adapter: mechanism copied from 5c (feishu),
# dingtalk-shaped increments asserted: the in-band ack frame on the
# same connection (ws::send — synchronous since the send-semantics
# amendment, so print mode cannot outrun the write) and the
# double-encoded data JSON.
step "5f/11 dingtalk stream (CALLBACK double-decode → in-band ack → steer → reply)"
python "$ROOT/scripts/dt_mock.py" > dt_mock.log 2>&1 &
DT_MOCK_PID=$!
for _ in $(seq 1 20); do
    grep -q "dt mock ready" dt_mock.log 2> /dev/null && break
    sleep 0.5
done
DT_PORT=$(sed -n 's/^dt mock ready //p' dt_mock.log)
[ -n "$DT_PORT" ] || fail "dt mock did not start: $(cat dt_mock.log)"
OUT="$("$TAU" --allow-unsigned --mcp-bridge "$DINGTALK"     --mcp-url "ws://127.0.0.1:$DT_PORT/dt" --allow-inject --demo -p "hi" 2>&1)" || fail "dingtalk run: $OUT"
kill "$DT_MOCK_PID" 2> /dev/null || true
echo "$OUT" | grep -q "steer: .IM dingtalk"     || fail "dingtalk message was not steered into the session: $OUT"
echo "$OUT" | grep -q "dingtalk: reply posted to loopback-user"     || fail "reply was not posted back: $OUT"
grep -q "DT ACK: " dt_mock.log     || fail "the ack frame never reached the platform: $(cat dt_mock.log)"
grep -q "DT REPLY: " dt_mock.log     || fail "mock never received the reply POST: $(cat dt_mock.log)"
echo "ok — dingtalk loop closed (CALLBACK → double-decode → in-band ack → steer → reply POST)"

# --- step 6: remembered consent lifecycle ------------------------------
step "6/11 remembered consent (--remember / --list / --revoke)"
# Consent is keyed by signing fingerprint, so the provider copy is
# signed with the throwaway key — the real trust store and any real
# consent records stay untouched.
cp "$HTTP_PROVIDER" prov.wasm
"$TAU" sign prov.wasm --key "$THROWAWAY_FP" > /dev/null || fail "sign provider"
CONSENT_FILE="$HOME/.tau/consent/$THROWAWAY_FP.json"

OUT="$("$TAU" --provider-wasm prov.wasm --model http \
    --provider-origin http://127.0.0.1:8402 --remember \
    -p "http://127.0.0.1:8402/" 2>&1)" || fail "remembered run: $OUT"
echo "$OUT" | grep -q "STATUS 200" || fail "remembered run fetch: $OUT"
[ -f "$CONSENT_FILE" ] || fail "--remember persisted nothing"
LIST="$("$TAU" consent --list)" || fail "consent --list: $LIST"
echo "$LIST" | grep -q "$THROWAWAY_FP" || fail "--list hides the grant: $LIST"
echo "$LIST" | grep -q "origin: http://127.0.0.1:8402" \
    || fail "--list hides the origin: $LIST"
echo "ok — grant persisted and listed"

# The remembered grant flows without the flag.
OUT="$("$TAU" --provider-wasm prov.wasm --model http \
    -p "http://127.0.0.1:8402/" 2>&1)" || fail "recalled run: $OUT"
echo "$OUT" | grep -q "STATUS 200" || fail "recalled grant did not flow: $OUT"
echo "ok — remembered origin flows without --provider-origin"

OUT="$("$TAU" consent --revoke "$THROWAWAY_FP")" || fail "consent --revoke: $OUT"
echo "$OUT" | grep -q "revoked: $THROWAWAY_FP" || fail "revoke output: $OUT"
[ ! -f "$CONSENT_FILE" ] || fail "revoke left the consent file"
if "$TAU" --provider-wasm prov.wasm --model http \
    -p "http://127.0.0.1:8402/" 2>&1 | grep -q "STATUS 200"; then
    fail "fetch flowed after revoke — the gate is open"
fi
echo "ok — revoked grant is gone and the gate closes again"

# Credential delivery: passing --provider-auth IS the consent, the token
# reaches the origin through the guest, and the secret is never
# persisted — the grant stores only the auth_delivery boolean.
OUT="$("$TAU" --provider-wasm prov.wasm --model http \
    --provider-origin http://127.0.0.1:8402 --provider-auth sekrit-123 --remember \
    -p "http://127.0.0.1:8402/" 2>&1)" || fail "auth run: $OUT"
echo "$OUT" | grep -q "STATUS 200 \[auth\]" || fail "guest did not get the token: $OUT"
grep -q "Bearer sekrit-123" auth_capture.log \
    || fail "token did not reach the origin: $(cat auth_capture.log)"
grep -q "sekrit-123" "$CONSENT_FILE" \
    && fail "consent file persisted the secret: $(cat "$CONSENT_FILE")"
grep -q "sekrit-123" .tau/session.jsonl \
    && fail "session file persisted the secret"
echo "ok — token delivered to the origin; consent + session carry no secret"

# With the recalled grant, TAU_PROVIDER_AUTH flows without the flag.
: > auth_capture.log
OUT="$(TAU_PROVIDER_AUTH=sekrit-456 "$TAU" --provider-wasm prov.wasm --model http \
    -p "http://127.0.0.1:8402/" 2>&1)" || fail "env recall run: $OUT"
echo "$OUT" | grep -q "STATUS 200 \[auth\]" \
    || fail "recalled grant did not deliver the env token: $OUT"
grep -q "Bearer sekrit-456" auth_capture.log \
    || fail "env token did not reach the origin"
echo "ok — recalled grant delivers TAU_PROVIDER_AUTH"

# The same env var without any grant: delivered nowhere, noted loudly.
"$TAU" consent --revoke "$THROWAWAY_FP" > /dev/null || fail "second revoke"
: > auth_capture.log
OUT="$(TAU_PROVIDER_AUTH=sekrit-789 "$TAU" --provider-wasm prov.wasm --model http \
    --provider-origin http://127.0.0.1:8402 \
    -p "http://127.0.0.1:8402/" 2>&1)" || fail "no-grant run: $OUT"
echo "$OUT" | grep -q "no auth-delivery grant" \
    || fail "missing the no-grant note: $OUT"
echo "$OUT" | grep -q "STATUS 200: hello" || fail "fetch broke without auth: $OUT"
if echo "$OUT" | grep -q "STATUS 200 \[auth\]"; then
    fail "token flowed without a grant — the auth gate is open"
fi
if grep -q "sekrit-789" auth_capture.log; then
    fail "origin saw a token it never consented to"
fi
echo "ok — env token refused without the grant"

# A corrupt consent file reads as absent: the remembered origin is
# gone with it and the gate closes (never an error, never lenient).
OUT="$("$TAU" --provider-wasm prov.wasm --model http \
    --provider-origin http://127.0.0.1:8402 --remember \
    -p "http://127.0.0.1:8402/" 2>&1)" || fail "re-remember run: $OUT"
echo "{ not json" > "$CONSENT_FILE"
if "$TAU" --provider-wasm prov.wasm --model http \
    -p "http://127.0.0.1:8402/" 2>&1 | grep -q "STATUS 200"; then
    fail "corrupt consent file still granted the origin"
fi
rm -f "$CONSENT_FILE"
echo "ok — corrupt consent file reads as absent, the gate closes"

# A caller-supplied "fingerprint" must never become a path out of the
# store: consent --revoke rejects anything not 16 lowercase hex.
if "$TAU" consent --revoke "../escape" > revoke.out 2>&1; then
    fail "path-shaped fingerprint accepted by consent --revoke"
fi
grep -q "not a signing fingerprint" revoke.out \
    || fail "unexpected revoke rejection: $(cat revoke.out)"
echo "ok — consent --revoke rejects non-fingerprint input"

# --- step 7: OCI distribution ------------------------------------------
step "7/11 OCI distribution (push / pull / trust onboarding)"
# ext.wasm from step 2 is signed with the throwaway key: signature and
# trust must apply to pulled bytes unchanged.
OCI_REF="oci://127.0.0.1:8403/test/component:v1"
python "$ROOT/crates/tau-ext/tests/mock_oci_registry.py" 8403 ext.wasm > reg.log 2>&1 &
REG_PID=$!
for _ in $(seq 1 20); do
    curl -sf http://127.0.0.1:8403/v2/test/component/manifests/latest \
        -H "accept: application/vnd.oci.image.manifest.v1+json" > /dev/null 2>&1 && break
    sleep 0.5
done
curl -sf http://127.0.0.1:8403/v2/test/component/manifests/latest \
    -H "accept: application/vnd.oci.image.manifest.v1+json" > /dev/null \
    || fail "mock OCI registry did not start: $(cat reg.log)"

OUT="$("$TAU" push ext.wasm "$OCI_REF" 2>&1)" || fail "push: $OUT"
DIGEST=$(echo "$OUT" | sed -n 's/^pushed .*(\(sha256:[0-9a-f]*\)).*/\1/p')
[ -n "$DIGEST" ] || fail "no digest in push output: $OUT"
OCI_BLOB=$(echo "$DIGEST" | tr ':' '_')
echo "ok — pushed ($DIGEST)"

OUT="$("$TAU" -e "$OCI_REF" --demo -p "shout oci" 2>&1)" || fail "pull+load: $OUT"
echo "$OUT" | grep -q "tool ← upper: SHOUT OCI" \
    || fail "pulled component did not close the loop: $OUT"
[ -f "$HOME/.tau/oci/blobs/$OCI_BLOB" ] || fail "pull did not populate the cache"
echo "ok — pulled, digest-cached, and the trusted signature loads"

# Trust onboarding from the component itself: set the verified key
# aside, onboard it back from the pulled bytes.
mv "$HOME/.tau/trust/$THROWAWAY_FP.pub" "$HOME/.tau/trust/$THROWAWAY_FP.pub.aside"
OUT="$("$TAU" trust --from-component "$OCI_REF" 2>&1)" \
    || { mv "$HOME/.tau/trust/$THROWAWAY_FP.pub.aside" "$HOME/.tau/trust/$THROWAWAY_FP.pub"; fail "trust --from-component: $OUT"; }
echo "$OUT" | grep -q "$THROWAWAY_FP" \
    || fail "onboarding did not print the fingerprint: $OUT"
[ -f "$HOME/.tau/trust/$THROWAWAY_FP.pub" ] || fail "onboarding did not trust the key"
rm -f "$HOME/.tau/trust/$THROWAWAY_FP.pub.aside"
echo "ok — trust --from-component onboards the verified key from oci://"

# --- step 8: blob GC ----------------------------------------------------
step "8/11 blob GC (dry-run reports, --yes deletes, live blobs kept)"
# Seed the real blob store with four blobs only this run could own
# (random content, unique digests): one referenced on the active
# branch, one inside a compaction summary, one on an abandoned branch,
# one orphan. A gc bug that eats live blobs would eat real user data,
# so this checks the live-set math against the real store — and the
# mark must cover the WHOLE session tree, compaction entries included.
mkdir -p "$HOME/.tau/blobs"
HASHES=$(python - << 'PYEOF'
import hashlib, os, random

random.seed()
blobs = os.path.expanduser("~/.tau/blobs")
out = []
for _ in range(4):
    data = random.randbytes(300 * 1024)
    digest = "sha256:" + hashlib.sha256(data).hexdigest()
    with open(os.path.join(blobs, digest.replace(":", "_")), "wb") as f:
        f.write(data)
    out.append(digest)
print(" ".join(out))
PYEOF
)
LIVE_HASH=$(echo "$HASHES" | cut -d' ' -f1)
COMPACT_HASH=$(echo "$HASHES" | cut -d' ' -f2)
BRANCH_HASH=$(echo "$HASHES" | cut -d' ' -f3)
ORPHAN_HASH=$(echo "$HASHES" | cut -d' ' -f4)
ORPHAN_FILE="$HOME/.tau/blobs/$(echo "$ORPHAN_HASH" | tr ':' '_')"
# Register all seeds with the exit trap: a FAIL mid-step must not
# leave them squatting in the real store (they break the next run).
GC_SEEDS=$(echo "$HASHES" | tr ':' '_')

# a (active message, LIVE) → b (compaction summary, COMPACT) → d (head);
# c hangs off a as an abandoned branch (BRANCH). The mark walks the
# whole tree, so all three survive and only the orphan goes.
cat > gc-session.jsonl << EOF
{"id":"a","parent":null,"type":"message","message":{"role":"user","content":[{"type":"text","text":"look"},{"type":"image","media":{"media_type":"image/png","source":"blob","hash":"$LIVE_HASH"}}]}}
{"id":"b","parent":"a","type":"compaction","summary":{"role":"user","content":[{"type":"text","text":"[summary] still referencing"},{"type":"image","media":{"media_type":"image/png","source":"blob","hash":"$COMPACT_HASH"}}]}}
{"id":"c","parent":"a","type":"message","message":{"role":"user","content":[{"type":"text","text":"abandoned branch"},{"type":"image","media":{"media_type":"image/png","source":"blob","hash":"$BRANCH_HASH"}}]}}
{"id":"d","parent":"b","type":"message","message":{"role":"user","content":[{"type":"text","text":"after the compaction"}]}}
EOF

OUT="$("$TAU" gc --session gc-session.jsonl 2>&1)" || fail "gc dry-run: $OUT"
echo "$OUT" | grep -q "would free 1 blob(s)" || fail "dry-run report: $OUT"
echo "$OUT" | grep -q "$ORPHAN_HASH" || fail "dry-run hides the orphan: $OUT"
for live in "$LIVE_HASH" "$COMPACT_HASH" "$BRANCH_HASH"; do
    if echo "$OUT" | grep -q "$live"; then
        fail "dry-run marks a live blob for removal: $live"
    fi
done
[ -f "$ORPHAN_FILE" ] || fail "dry-run deleted the orphan"
echo "ok — dry-run reports the orphan, keeps every tree-referenced blob, deletes nothing"

OUT="$("$TAU" gc --session gc-session.jsonl --yes 2>&1)" || fail "gc --yes: $OUT"
echo "$OUT" | grep -q "freed 1 blob(s)" || fail "--yes report: $OUT"
[ ! -f "$ORPHAN_FILE" ] || fail "--yes left the orphan"
for hash in "$LIVE_HASH" "$COMPACT_HASH" "$BRANCH_HASH"; do
    f="$HOME/.tau/blobs/$(echo "$hash" | tr ':' '_')"
    [ -f "$f" ] || fail "--yes deleted a LIVE blob: $hash"
    rm -f "$f"
done
echo "ok — --yes frees exactly the orphan; active, compacted, and abandoned-branch blobs survive"

# A mistyped --session must not orphan live data: gc refuses missing
# session files, even under --yes (it used to treat them as empty and
# would have deleted every referenced blob).
TYPO_HASH=$(python - << 'PYEOF'
import hashlib, os, random
random.seed()
data = random.randbytes(1000)
digest = "sha256:" + hashlib.sha256(data).hexdigest()
with open(os.path.expanduser("~/.tau/blobs/" + digest.replace(":", "_")), "wb") as f:
    f.write(data)
print(digest)
PYEOF
)
TYPO_FILE="$HOME/.tau/blobs/$(echo "$TYPO_HASH" | tr ':' '_')"
GC_SEEDS="$GC_SEEDS $(echo "$TYPO_HASH" | tr ':' '_')"
OUT="$("$TAU" gc --session does-not-exist.jsonl --yes 2>&1 || true)"
echo "$OUT" | grep -q "session file not found" \
    || fail "gc --yes accepted a missing session: $OUT"
[ -f "$TYPO_FILE" ] || fail "gc --yes with a typo session deleted a blob"
rm -f "$TYPO_FILE"
echo "ok — gc refuses a missing session file, even under --yes"
GC_SEEDS=""

# --- step 9: compaction --------------------------------------------------
step "9/11 compaction (summary entry; originals stay in the tree)"
# Seed a small session: two demo exchanges on disk.
"$TAU" --session session.jsonl --demo -p "first exchange" > /dev/null 2>&1 \
    || fail "seed run 1"
"$TAU" --session session.jsonl --demo -p "second exchange" > /dev/null 2>&1 \
    || fail "seed run 2"
BEFORE=$(grep -c "" session.jsonl)

OUT="$("$TAU" --session session.jsonl --demo --compact 2>&1)" || fail "compact: $OUT"
echo "$OUT" | grep -q "compacted session" || fail "compact output: $OUT"

# Exactly one entry appended — the summary — and the originals stay.
AFTER=$(grep -c "" session.jsonl)
[ "$AFTER" -eq "$((BEFORE + 1))" ] \
    || fail "entry count $BEFORE -> $AFTER, expected +1"
TREE="$("$TAU" tree --session session.jsonl 2>&1)" || fail "tree: $TREE"
echo "$TREE" | grep -q "\[compaction\]" \
    || fail "tree hides the compaction entry: $TREE"
echo "$TREE" | grep -q "first exchange" \
    || fail "originals vanished from the tree: $TREE"
echo "ok — summary entry appended, originals intact"

# The compacted branch is the context for what follows: a follow-up run
# appends under the compaction entry and still answers.
OUT="$("$TAU" --session session.jsonl --demo -p "after compact" 2>&1)" \
    || fail "post-compact run: $OUT"
echo "$OUT" | grep -q "tau is alive" || fail "post-compact answer: $OUT"
TREE="$("$TAU" tree --session session.jsonl 2>&1)" || fail "tree: $TREE"
echo "$TREE" | grep -q "after compact" \
    || fail "follow-up did not land on the compacted branch: $TREE"
echo "ok — follow-up runs on the compacted branch"

# --- step 9b: torn-tail recovery ------------------------------------------
step "9b/11 torn-tail recovery (a crash mid-append must not brick the session)"
# session.jsonl from step 9 is intact; tear its tail the way a crash
# mid-append leaves it: a partial JSON line, no trailing newline.
printf '{"id":"torn","parent":null,"kind":{"me' >> session.jsonl
OUT="$("$TAU" tree --session session.jsonl 2>&1)" || fail "torn tail bricked the session: $OUT"
echo "$OUT" | grep -q "discarded a torn tail at line" \
    || fail "no recovery warning: $OUT"
echo "$OUT" | grep -q "first exchange" \
    || fail "intact entries lost to the tear: $OUT"
if grep -q '"id":"torn"' session.jsonl; then
    fail "torn bytes survived recovery — appends would stay corrupt"
fi
echo "ok — torn tail discarded with a warning, file truncated clean"

# A bad line with GOOD lines after it is real corruption: refuse loudly,
# never truncate.
cp session.jsonl middle.jsonl
sed -i '2i not json at all' middle.jsonl
if "$TAU" tree --session middle.jsonl > /dev/null 2>&1; then
    fail "middle-corrupt session loaded"
fi
OUT="$("$TAU" tree --session middle.jsonl 2>&1 || true)"
echo "$OUT" | grep -q "corrupt session file .* line 2" \
    || fail "corruption error does not name the line: $OUT"
grep -q "not json at all" middle.jsonl \
    || fail "refused file was truncated — forensics destroyed"
echo "ok — middle corruption refuses with a named line, file untouched"

# --- step 9c: concurrent access -------------------------------------------
step "9c/11 concurrent access (shared session file stays a valid tree)"
# Four tau processes append to ONE session file at once. The tree model
# makes this structurally safe: each process's chain lands in order, so
# parents always precede children — implicit branching, never corruption.
PIDS=""
for i in 1 2 3 4; do
    "$TAU" --session shared.jsonl --demo -p "concurrent $i" > /dev/null 2>&1 &
    PIDS="$PIDS $!"
done
# Bare `wait` would also wait for the mock daemons started in earlier
# steps (killed only by the exit trap) — wait on exactly these writers.
wait $PIDS
python - << 'EOF_CONCURRENT'
import json, sys
entries = []
with open("shared.jsonl") as f:
    for n, line in enumerate(f, 1):
        line = line.strip()
        if not line:
            continue
        try:
            entries.append(json.loads(line))
        except Exception as ex:
            sys.exit(f"line {n} corrupt after concurrent appends: {ex}")
ids = set()
for e in entries:
    parent = e.get("parent")
    if parent and parent not in ids:
        sys.exit(f"parent {parent} appears after its child -- tree broken")
    ids.add(e["id"])
if len(entries) != 8:
    sys.exit(f"expected 8 entries from 4 runs, got {len(entries)}")
EOF_CONCURRENT
echo "ok — 4 writers, 8 entries, every parent precedes its child"

# --- step 10: probe verdicts ---------------------------------------------
step "10/11 probe verdicts (before_tool block reaches the model)"
OUT="$("$TAU" --allow-unsigned -e "$UPPER" -e "$GUARD" \
    --demo -p "shout forbidden" 2>&1)" || fail "guard run: $OUT"
echo "$OUT" | grep -q "probe before_tool: block" \
    || fail "block verdict not on the decision trail: $OUT"
echo "$OUT" | grep -q "tool ← upper (error): blocked: the guard said no" \
    || fail "block reason did not become the tool result: $OUT"
echo "$OUT" | grep -q "The tool failed: blocked: the guard said no" \
    || fail "block reason did not reach the model: $OUT"
echo "ok — block: probe → tool result → the model sees the reason"

# Observe leg (probes.md): the guard registers session_start; the host
# fires it, the guest reports through host.notify, the renderer prints.
echo "$OUT" | grep -q "ext info: session_start: .tau/session.jsonl" \
    || fail "session_start observation did not reach the guest/renderer: $OUT"
echo "ok — observe-only session_start fires, guest sees it, verdict ignored"

# Discoverability: the catalog lists the wired lifecycle points and the
# reserved streaming slots.
OUT="$("$TAU" probes)" || fail "tau probes: $OUT"
echo "$OUT" | grep -q "session_start \[wired\]" \
    || fail "catalog missing session_start: $OUT"
echo "$OUT" | grep -q "session_end \[wired\]" \
    || fail "catalog missing session_end: $OUT"
echo "$OUT" | grep -q "branch \[wired\]" \
    || fail "catalog missing branch: $OUT"
echo "$OUT" | grep -q "text_delta \[reserved\]" \
    || fail "catalog missing reserved text_delta: $OUT"
echo "ok — tau probes catalog: lifecycle wired, streaming reserved"

OUT="$("$TAU" --allow-unsigned -e "$UPPER" -e "$GUARD" \
    --demo -p "shout allowed" 2>&1)" || fail "guard pass run: $OUT"
echo "$OUT" | grep -q "tool ← upper: SHOUT ALLOWED" \
    || fail "allowed call did not pass through: $OUT"
echo "ok — continue: clean calls pass untouched"

# The guard's probe panics on "crash": the trap must degrade to continue
# (the call goes through) and the guest's panic text must reach stderr.
OUT="$("$TAU" --allow-unsigned -e "$UPPER" -e "$GUARD" \
    --demo -p "shout crash" 2>&1)" || fail "broken-probe run: $OUT"
echo "$OUT" | grep -q "tool ← upper: SHOUT CRASH" \
    || fail "trapped probe did not degrade to continue: $OUT"
echo "$OUT" | grep -q "the guard blew up" \
    || fail "guest panic did not reach stderr: $OUT"
echo "ok — degrade: trapped probe continues, the call goes through"

# The WASI sandbox boundary, observable from inside the guest: the
# guard's "wasicheck" probe reads the ambient TAU_AMBIENT env var.
OUT="$(TAU_AMBIENT=hunter2 "$TAU" --allow-unsigned -e "$UPPER" -e "$GUARD" \
    --demo -p "shout wasicheck" 2>&1)" || fail "wasicheck run: $OUT"
echo "$OUT" | grep -q "ambient env leaked" \
    || fail "ambient env was not visible under the default WASI policy: $OUT"
echo "ok — ambient WASI: the guest inherits the host env by default"

OUT="$(TAU_AMBIENT=hunter2 "$TAU" --allow-unsigned --deny-wasi -e "$UPPER" -e "$GUARD" \
    --demo -p "shout wasicheck" 2>&1)" || fail "deny-wasi run: $OUT"
echo "$OUT" | grep -q "tool ← upper: SHOUT WASICHECK" \
    || fail "--deny-wasi did not hide the ambient env from the guest: $OUT"
echo "ok — --deny-wasi: the guest env is empty, the sandbox holds"

# --- step 10b: host channel (facts flow; injection consent-gated) ------
step "10b/11 host channel (notify/emit facts; steer consent gate)"

# Without --allow-inject: notify/emit reach the renderer and the bus;
# steer fails closed with a named refusal inside the tool result.
OUT="$("$TAU" --allow-unsigned -e "$NOTIFIER" --demo -p "hello host" 2>&1)" \
    || fail "notifier run: $OUT"
echo "$OUT" | grep -q "ext info: poke: hello host" \
    || fail "host.notify did not reach the renderer: $OUT"
echo "$OUT" | grep -q 'ext fact: {"poke":"hello host"}' \
    || fail "host.emit did not reach the event bus: $OUT"
echo "$OUT" | grep -q "steer refused: session injection not consented" \
    || fail "steer without consent was not refused: $OUT"
echo "$OUT" | grep -q "\[tau\] steer:" && fail "refused steer still landed: $OUT"
echo "ok — facts flow; injection fails closed without consent"

# With --allow-inject: the steer queues, applies at the turn checkpoint,
# and shows on the decision trail.
OUT="$("$TAU" --allow-unsigned --allow-inject -e "$NOTIFIER" --demo -p "hello host" 2>&1)" \
    || fail "notifier inject run: $OUT"
echo "$OUT" | grep -q "consent: may inject messages into the session" \
    || fail "consent UX line missing: $OUT"
echo "$OUT" | grep -q "steer queued" \
    || fail "consented steer was not accepted: $OUT"
echo "$OUT" | grep -q "\[tau\] steer: hello host" \
    || fail "consented steer did not land at the checkpoint: $OUT"
echo "ok — consent granted: steer queues and lands on the trail"

# A 0.1.0-contract component is refused with the version mismatch named.
OLD_UPPER="$ROOT/dist/tau-0.2.0-x86_64-pc-windows-gnu/examples/c_upper.wasm"
if [ -f "$OLD_UPPER" ]; then
    OUT="$("$TAU" --allow-unsigned -e "$OLD_UPPER" --demo -p "hi" 2>&1)" && \
        fail "0.1.0 component loaded against the 0.2.0 host: $OUT"
    echo "$OUT" | grep -q "targets tau:extension@0.1.0" \
        || fail "version mismatch not named in the load error: $OUT"
    echo "ok — 0.1.0 component refused, version mismatch named"
else
    echo "skip — dist copy of a 0.1.0-contract component not found"
fi

# A component declaring an invalid parameters-json is refused at load,
# naming the broken tool (wit-review F5 — never degrade to an open schema).
BAD_SCHEMA="$ROOT/examples/bad-schema/target/wasm32-wasip2/release/bad_schema.wasm"
# bad-schema is deliberately NOT in EXAMPLES (a broken component must never
# ship), so the pre-flight does not rebuild it — and an existence check here
# would silently reuse the previous contract's artifact, failing this leg
# with a version mismatch instead of the schema error it asserts
# (LESSON_契约版本升级后示例夹具须先重建再跑测试). Always build: it is tiny,
# and the unit test reads the same file.
cargo build --manifest-path "$ROOT/examples/bad-schema/Cargo.toml"         --target wasm32-wasip2 --release --quiet
OUT="$("$TAU" --allow-unsigned -e "$BAD_SCHEMA" --demo -p "hi" 2>&1)" &&     fail "bad-schema component loaded: $OUT"
echo "$OUT" | grep -q "tool 'bad_schema'" || fail "broken tool not named: $OUT"
echo "$OUT" | grep -q "invalid parameters-json" || fail "reason not named: $OUT"
echo "ok — invalid parameters-json refused at load, tool named"

# --- step 11: interactive REPL over a real pty -------------------------
step "11/11 interactive REPL (pty: banner, turn, /help, Ctrl-C, /quit, history)"
if python -c "import winpty" 2> /dev/null; then
    python "$ROOT/scripts/repl_e2e.py" "$TAU" || fail "repl pty e2e failed"
else
    echo "skip — pywinpty not installed; REPL pty e2e not run"
fi

# --- step 11b: realtime-av voice loop (docs/realtime-av.md) ---
# The hardware-free path: /mic N sine synthesizes the clip, the demo
# model echoes it as AudioDelta chunks, the Phase 1 live sink streams
# them (null sink on headless machines — it counts identically, so the
# gate asserts the EXACT sample count, not "some sound happened").
step "11b/11 voice loop (sine → Content::Audio uplink → audio echo → live sink → session)"
if python -c "import winpty" 2> /dev/null; then
    python "$ROOT/scripts/av_phase0_e2e.py" "$TAU" "$WORK/av-e2e"         || fail "av phase0 e2e failed"
else
    echo "skip — pywinpty not installed; av phase0 e2e not run"
fi

# --- step 11c: realtime-av Phase 2a full duplex (docs/realtime-av.md) ---
# The demo model's RealtimeSession double: paced sine uplink → server
# VAD → per-chunk echo → live sink plays the pcm raw stream; then a
# barge-in leg (Ctrl-C → Interrupted → buffer cleared, REPL alive).
step "11c/11 full duplex (sine uplink → VAD → echo → live sink → tree; barge-in leg)"
if python -c "import winpty" 2> /dev/null; then
    python "$ROOT/scripts/av_live_e2e.py" "$TAU" "$WORK/av-live-e2e"         || fail "av live e2e failed"
else
    echo "skip — pywinpty not installed; av live e2e not run"
fi

# --- step 11d: realtime-av Phase 2b (docs/realtime-av.md) ---
# The same full-duplex loop behind the wasm boundary (world realtime,
# examples/realtime-echo), plus the microphone consent category: the
# grant guards the DEVICE (real-mic path refuses without it; the sine
# path never touches a device and flows regardless).
step "11d/11 realtime over wasm (consent gate + duplex through world realtime)"
if python -c "import winpty" 2> /dev/null; then
    python "$ROOT/scripts/av_wasm_live_e2e.py" "$TAU" "$REALTIME_ECHO" "$WORK/av-wasm-live-e2e" || fail "av wasm live e2e failed"
else
    echo "skip — pywinpty not installed; av wasm live e2e not run"
fi

step "ALL ELEVEN STEPS PASSED — the release candidate stands"
