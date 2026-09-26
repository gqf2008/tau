#!/usr/bin/env bash
# First-user validation: prove the release candidate works for someone
# who just installed tau — demo, the signing/trust chain, a built-in
# provider against a loopback SSE mock, and the wasm provider consent
# gate in both directions.
#
# Usage: scripts/validate.sh
#
# Environment contract (Windows: dirs::home_dir ignores HOME/USERPROFILE,
# so ~/.tau is the REAL one): the script keygens one throwaway key,
# records its fingerprint, and removes exactly that key + pub on exit.
# Nothing else in ~/.tau is touched; the consent store is not written.
set -euo pipefail
cd "$(dirname "$0")/.."

ROOT="$(pwd)"
TAU="$ROOT/target/release/tau"
[ -f "$ROOT/target/release/tau.exe" ] && TAU="$ROOT/target/release/tau.exe"
WORK="$ROOT/target/validate"
MOCK_PID=""
THROWAWAY_FP=""

step() { echo; echo "== $1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }

cleanup() {
    [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
    cd "$ROOT" # cannot remove the workdir while standing in it (Windows)
    if [ -n "$THROWAWAY_FP" ]; then
        rm -f "$HOME/.tau/keys/$THROWAWAY_FP.key"             "$HOME/.tau/trust/$THROWAWAY_FP.pub"             "$HOME/.tau/trust/$THROWAWAY_FP.pub.aside"             "$HOME/.tau/consent/$THROWAWAY_FP.json"
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

# --- pre-flight -------------------------------------------------------
step "build release binary + wasm examples"
cargo build --release -p tau-cli --quiet
for ex in upper http-provider mcp-bridge; do
    cargo build --manifest-path "examples/${ex}/Cargo.toml" \
        --target wasm32-wasip2 --release --quiet
done
UPPER="$ROOT/examples/upper/target/wasm32-wasip2/release/upper.wasm"
HTTP_PROVIDER="$ROOT/examples/http-provider/target/wasm32-wasip2/release/http_provider.wasm"
MCP_BRIDGE="$ROOT/examples/mcp-bridge/target/wasm32-wasip2/release/mcp_bridge.wasm"
[ -f "$UPPER" ] || fail "upper example missing"
[ -f "$HTTP_PROVIDER" ] || fail "http-provider example missing"
[ -f "$MCP_BRIDGE" ] || fail "mcp-bridge example missing"

rm -rf "$WORK"
mkdir -p "$WORK"
cd "$WORK"

# --- step 1: demo -----------------------------------------------------
step "1/6 demo"
OUT="$("$TAU" --demo -p "hello from validation" 2>&1)" || fail "demo exited $?"
echo "$OUT" | grep -q "tau is alive" || fail "demo answer missing: $OUT"
echo "ok — faux model answered"

# --- step 2: signing + trust chain ------------------------------------
step "2/6 signing and trust chain"
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

# --- step 3: built-in provider against a loopback SSE mock -------------
step "3/6 built-in providers (loopback SSE mock)"
cat > mock.py << 'PYEOF'
import threading
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


class ChatHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length", 0)))
        if self.path.endswith("/responses"):
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

# --- step 4: wasm provider consent gate --------------------------------
step "4/6 wasm provider consent gate"
if "$TAU" --allow-unsigned \
    --provider-wasm "$HTTP_PROVIDER" --model http-echo \
    -p "http://127.0.0.1:8402/" 2>&1 | grep -q "STATUS 200"; then
    fail "http fetch succeeded with no consent — consent gate is open"
fi
echo "ok — no consent: fetch denied"

OUT="$("$TAU" --allow-unsigned \
    --provider-wasm "$HTTP_PROVIDER" --model http-echo \
    --provider-origin http://127.0.0.1:8402 \
    -p "http://127.0.0.1:8402/" 2>&1)" || fail "consented fetch: $OUT"
echo "$OUT" | grep -q "STATUS 200: hello from mock origin" \
    || fail "consented fetch did not land: $OUT"
echo "ok — with --provider-origin the fetch flows"

# --- step 5: MCP bridge (consent-gated spawn) --------------------------
step "5/6 MCP bridge (consent-gated spawn)"
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

# --- step 6: remembered consent lifecycle ------------------------------
step "6/6 remembered consent (--remember / --list / --revoke)"
# Consent is keyed by signing fingerprint, so the provider copy is
# signed with the throwaway key — the real trust store and any real
# consent records stay untouched.
cp "$HTTP_PROVIDER" prov.wasm
"$TAU" sign prov.wasm --key "$THROWAWAY_FP" > /dev/null || fail "sign provider"
CONSENT_FILE="$HOME/.tau/consent/$THROWAWAY_FP.json"

OUT="$("$TAU" --provider-wasm prov.wasm --model http-echo \
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
OUT="$("$TAU" --provider-wasm prov.wasm --model http-echo \
    -p "http://127.0.0.1:8402/" 2>&1)" || fail "recalled run: $OUT"
echo "$OUT" | grep -q "STATUS 200" || fail "recalled grant did not flow: $OUT"
echo "ok — remembered origin flows without --provider-origin"

OUT="$("$TAU" consent --revoke "$THROWAWAY_FP")" || fail "consent --revoke: $OUT"
echo "$OUT" | grep -q "revoked: $THROWAWAY_FP" || fail "revoke output: $OUT"
[ ! -f "$CONSENT_FILE" ] || fail "revoke left the consent file"
if "$TAU" --provider-wasm prov.wasm --model http-echo \
    -p "http://127.0.0.1:8402/" 2>&1 | grep -q "STATUS 200"; then
    fail "fetch flowed after revoke — the gate is open"
fi
echo "ok — revoked grant is gone and the gate closes again"

step "ALL SIX STEPS PASSED — the release candidate stands"
