#!/usr/bin/env bash
# First-user validation: prove the release candidate works for someone
# who just installed tau, in eleven steps — demo, the signing/trust chain
# (incl. tamper rejection), all three built-in providers against a
# loopback mock, the wasm provider consent gate, the MCP bridge spawn
# gate, the remembered-consent lifecycle, OCI distribution, blob GC,
# compaction, probe verdicts, and the interactive REPL over a real pty
# (skipped with a note when pywinpty is not installed).
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
REG_PID=""
OCI_BLOB=""
GC_BLOB=""
THROWAWAY_FP=""

step() { echo; echo "== $1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }

cleanup() {
    [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
    [ -n "$REG_PID" ] && kill "$REG_PID" 2>/dev/null || true
    [ -n "$OCI_BLOB" ] && rm -f "$HOME/.tau/oci/blobs/$OCI_BLOB"
    [ -n "$GC_BLOB" ] && rm -f "$HOME/.tau/blobs/$GC_BLOB"
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
step "build release binary + wasm examples"
cargo build --release -p tau-cli --quiet
for ex in upper http-provider mcp-bridge guard echo-provider; do
    cargo build --manifest-path "examples/${ex}/Cargo.toml" \
        --target wasm32-wasip2 --release --quiet
done
UPPER="$ROOT/examples/upper/target/wasm32-wasip2/release/upper.wasm"
HTTP_PROVIDER="$ROOT/examples/http-provider/target/wasm32-wasip2/release/http_provider.wasm"
MCP_BRIDGE="$ROOT/examples/mcp-bridge/target/wasm32-wasip2/release/mcp_bridge.wasm"
GUARD="$ROOT/examples/guard/target/wasm32-wasip2/release/guard.wasm"
ECHO_PROVIDER="$ROOT/examples/echo-provider/target/wasm32-wasip2/release/echo_provider.wasm"
[ -f "$UPPER" ] || fail "upper example missing"
[ -f "$HTTP_PROVIDER" ] || fail "http-provider example missing"
[ -f "$MCP_BRIDGE" ] || fail "mcp-bridge example missing"
[ -f "$GUARD" ] || fail "guard example missing"
[ -f "$ECHO_PROVIDER" ] || fail "echo-provider example missing"

rm -rf "$WORK"
mkdir -p "$WORK"
cd "$WORK"

# --- step 1: demo -----------------------------------------------------
step "1/11 demo"
OUT="$("$TAU" --demo -p "hello from validation" 2>&1)" || fail "demo exited $?"
echo "$OUT" | grep -q "tau is alive" || fail "demo answer missing: $OUT"
echo "ok — faux model answered"

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
echo "ok — tau sign --key rejects non-fingerprint input"

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
        # Side channel: record the Authorization header of every request
        # so the credential-delivery step can prove what the origin saw.
        with open("auth_capture.log", "a") as f:
            f.write(str(self.headers.get("authorization")) + "\n")
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

# --- step 4: wasm provider consent gate --------------------------------
step "4/11 wasm provider consent gate"
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

# A redirect would escape consent: the consented origin answers 302 to
# an unconsented host, and the guest must see the 302, not the target.
OUT="$("$TAU" --allow-unsigned \
    --provider-wasm "$HTTP_PROVIDER" --model http-echo \
    --provider-origin http://127.0.0.1:8402 \
    -p "http://127.0.0.1:8402/redirect" 2>&1)" || fail "redirect run: $OUT"
echo "$OUT" | grep -q "STATUS 302" \
    || fail "redirect was followed — consent escaped: $OUT"
echo "ok — consented origin's 302 is shown, never followed"

# Origin matching sees the same host the client dials: userinfo inside
# the authority is stripped (flows), a backslash after the authority is
# path material (stays on the consented host), and the delimiter tricks
# plus the trailing-dot twin are refused as unconsented.
OUT="$("$TAU" --allow-unsigned \
    --provider-wasm "$HTTP_PROVIDER" --model http-echo \
    --provider-origin http://127.0.0.1:8402 \
    -p "http://user:pw@127.0.0.1:8402/" 2>&1)" || fail "userinfo run: $OUT"
echo "$OUT" | grep -q "STATUS 200: hello" \
    || fail "userinfo-inside-authority fetch did not land: $OUT"
OUT="$("$TAU" --allow-unsigned \
    --provider-wasm "$HTTP_PROVIDER" --model http-echo \
    --provider-origin http://127.0.0.1:8402 \
    -p 'http://127.0.0.1:8402\@evil.invalid/' 2>&1)" || fail "backslash run: $OUT"
echo "$OUT" | grep -q "STATUS 200: hello" \
    || fail "backslash-after-authority left the consented host: $OUT"
for evil in 'http://evil.invalid\@127.0.0.1:8402/' \
            'http://evil.invalid?@127.0.0.1:8402/' \
            'http://127.0.0.1:8402./'; do
    OUT="$("$TAU" --allow-unsigned \
        --provider-wasm "$HTTP_PROVIDER" --model http-echo \
        --provider-origin http://127.0.0.1:8402 \
        -p "$evil" 2>&1 || true)"
    echo "$OUT" | grep -q "not in consent allowlist" \
        || fail "consent bypass reached the network: $evil → $OUT"
    if echo "$OUT" | grep -q "STATUS 200"; then
        fail "consent bypass was served: $evil"
    fi
done
echo "ok — origin gate: userinfo/backslash stay home, tricks and twins refused"

# --- step 4b: large payload over the component boundary ----------------
step "4b/11 large media crosses the component boundary intact"
# A 3 MiB image in the session history inflates the request JSON past
# 4 MiB of base64 — far beyond the few KiB every other step sends. The
# echo provider's "probe" keyword reports the byte length of the
# request the GUEST received; the session → materialize → wasm boundary
# path must deliver it whole, not choke, truncate, or refuse. (The
# exact length + FNV-1a contract against the sent string is pinned by
# tau-ext's large_payload_tests; here the real CLI drives it.)
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
PYEOF
OUT="$("$TAU" --allow-unsigned \
    --provider-wasm "$ECHO_PROVIDER" --model echo \
    --session big-session.jsonl --continue \
    -p "probe" 2>&1)" || fail "large-payload run: $OUT"
BYTES=$(echo "$OUT" | grep -o 'bytes=[0-9]*' | head -1 | cut -d= -f2)
[ -n "$BYTES" ] || fail "guest never reported the payload size: $OUT"
[ "$BYTES" -gt 4000000 ] \
    || fail "payload truncated at the boundary: guest saw only $BYTES bytes"
echo "ok — 3 MiB of media crossed session→guest whole ($BYTES bytes of request JSON)"

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

# --- step 6: remembered consent lifecycle ------------------------------
step "6/11 remembered consent (--remember / --list / --revoke)"
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

# Credential delivery: passing --provider-auth IS the consent, the token
# reaches the origin through the guest, and the secret is never
# persisted — the grant stores only the auth_delivery boolean.
OUT="$("$TAU" --provider-wasm prov.wasm --model http-echo \
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
OUT="$(TAU_PROVIDER_AUTH=sekrit-456 "$TAU" --provider-wasm prov.wasm --model http-echo \
    -p "http://127.0.0.1:8402/" 2>&1)" || fail "env recall run: $OUT"
echo "$OUT" | grep -q "STATUS 200 \[auth\]" \
    || fail "recalled grant did not deliver the env token: $OUT"
grep -q "Bearer sekrit-456" auth_capture.log \
    || fail "env token did not reach the origin"
echo "ok — recalled grant delivers TAU_PROVIDER_AUTH"

# The same env var without any grant: delivered nowhere, noted loudly.
"$TAU" consent --revoke "$THROWAWAY_FP" > /dev/null || fail "second revoke"
: > auth_capture.log
OUT="$(TAU_PROVIDER_AUTH=sekrit-789 "$TAU" --provider-wasm prov.wasm --model http-echo \
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
OUT="$("$TAU" --provider-wasm prov.wasm --model http-echo \
    --provider-origin http://127.0.0.1:8402 --remember \
    -p "http://127.0.0.1:8402/" 2>&1)" || fail "re-remember run: $OUT"
echo "{ not json" > "$CONSENT_FILE"
if "$TAU" --provider-wasm prov.wasm --model http-echo \
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
GC_BLOB=""

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

# --- step 11: interactive REPL over a real pty -------------------------
step "11/11 interactive REPL (pty: banner, turn, /help, Ctrl-C, /quit, history)"
if python -c "import winpty" 2> /dev/null; then
    python "$ROOT/scripts/repl_e2e.py" "$TAU" || fail "repl pty e2e failed"
else
    echo "skip — pywinpty not installed; REPL pty e2e not run"
fi

step "ALL ELEVEN STEPS PASSED — the release candidate stands"
