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
        rm -f "$HOME/.tau/keys/$THROWAWAY_FP.key"             "$HOME/.tau/trust/$THROWAWAY_FP.pub"             "$HOME/.tau/trust/$THROWAWAY_FP.pub.aside"
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

# --- pre-flight -------------------------------------------------------
step "build release binary + wasm examples"
cargo build --release -p tau-cli --quiet
for ex in upper http-provider; do
    cargo build --manifest-path "examples/${ex}/Cargo.toml" \
        --target wasm32-wasip2 --release --quiet
done
UPPER="$ROOT/examples/upper/target/wasm32-wasip2/release/upper.wasm"
HTTP_PROVIDER="$ROOT/examples/http-provider/target/wasm32-wasip2/release/http_provider.wasm"
[ -f "$UPPER" ] || fail "upper example missing"
[ -f "$HTTP_PROVIDER" ] || fail "http-provider example missing"

rm -rf "$WORK"
mkdir -p "$WORK"
cd "$WORK"

# --- step 1: demo -----------------------------------------------------
step "1/4 demo"
OUT="$("$TAU" --demo -p "hello from validation" 2>&1)" || fail "demo exited $?"
echo "$OUT" | grep -q "tau is alive" || fail "demo answer missing: $OUT"
echo "ok — faux model answered"

# --- step 2: signing + trust chain ------------------------------------
step "2/4 signing and trust chain"
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
step "3/4 built-in provider (loopback SSE mock)"
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
step "4/4 wasm provider consent gate"
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

step "ALL FOUR STEPS PASSED — the release candidate stands"
