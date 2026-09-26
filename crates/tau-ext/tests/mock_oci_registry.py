"""Mock OCI registry for tau tests: registry v2 pull (manifest + blob
GETs) and push (blob upload session + manifest PUT), optionally gated
behind the bearer-token dance.

Usage: mock_oci_registry.py <port> <blob-file> [--auth]
Seeds repo "test/component" with tag "latest" and the blob file as the
single wasm layer; pushed tags/blobs accumulate in the same store, so a
push can be pulled back. With --auth, every non-token route 401s with a
WWW-Authenticate challenge and requires the token issued by /token.
"""

import hashlib
import json
import sys
import uuid
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1])
SEED = open(sys.argv[2], "rb").read()
AUTH = "--auth" in sys.argv
TOKEN = "mock-token-123"
REPO = "test/component"

# Content stores: blobs by digest, manifests by tag-or-digest, in-flight
# upload sessions by uuid.
BLOBS = {}
MANIFESTS = {}
UPLOADS = {}


def store_manifest(tag, manifest):
    MANIFESTS[tag] = manifest
    for layer in manifest.get("layers", []):
        MANIFESTS[layer["digest"]] = manifest


def seed():
    digest = "sha256:" + hashlib.sha256(SEED).hexdigest()
    BLOBS[digest] = SEED
    manifest = {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.wasm.config.v1+json",
            "digest": "sha256:" + hashlib.sha256(b"{}").hexdigest(),
            "size": 2,
        },
        "layers": [
            {
                "mediaType": "application/vnd.wasm.content.layer.v1+wasm",
                "digest": digest,
                "size": len(SEED),
            }
        ],
    }
    store_manifest("latest", manifest)


seed()


class Handler(BaseHTTPRequestHandler):
    def authed(self):
        if not AUTH:
            return True
        if self.headers.get("authorization") == f"Bearer {TOKEN}":
            return True
        self.send_response(401)
        self.send_header(
            "www-authenticate",
            f'Bearer realm="http://127.0.0.1:{PORT}/token",service="mock",'
            f'scope="repository:{REPO}:pull,push"',
        )
        self.end_headers()
        return False

    def reply_json(self, status, value, extra=None):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        for name, val in (extra or {}).items():
            self.send_header(name, val)
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        # self.path includes the query string; route on the path part only.
        path = self.path.split("?", 1)[0]
        if path == "/token":
            self.reply_json(200, {"token": TOKEN})
            return
        if not self.authed():
            return

        if path.startswith(f"/v2/{REPO}/manifests/"):
            ref = path.rsplit("/", 1)[1]
            manifest = MANIFESTS.get(ref)
            if manifest is None:
                self.send_response(404)
                self.end_headers()
                return
            body = json.dumps(manifest).encode()
            self.send_response(200)
            self.send_header("content-type", "application/vnd.oci.image.manifest.v1+json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif path.startswith(f"/v2/{REPO}/blobs/"):
            digest = path.rsplit("/", 1)[1]
            blob = BLOBS.get(digest)
            if blob is None:
                self.send_response(404)
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("content-length", str(len(blob)))
            self.end_headers()
            self.wfile.write(blob)
        else:
            self.send_response(404)
            self.end_headers()

    def do_POST(self):
        path = self.path.split("?", 1)[0]
        if not self.authed():
            return
        if path == f"/v2/{REPO}/blobs/uploads/":
            upload_id = uuid.uuid4().hex
            UPLOADS[upload_id] = b""
            self.send_response(202)
            self.send_header("location", f"/v2/{REPO}/blobs/uploads/{upload_id}")
            self.send_header("content-length", "0")
            self.end_headers()
        else:
            self.send_response(404)
            self.end_headers()

    def do_PUT(self):
        path, _, query = self.path.partition("?")
        if not self.authed():
            return
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length)

        if path.startswith(f"/v2/{REPO}/blobs/uploads/"):
            upload_id = path.rsplit("/", 1)[1]
            if upload_id not in UPLOADS:
                self.send_response(404)
                self.end_headers()
                return
            blob = UPLOADS.pop(upload_id) + body
            expected = query.split("=", 1)[1] if "=" in query else ""
            actual = "sha256:" + hashlib.sha256(blob).hexdigest()
            if expected != actual:
                self.send_response(400)
                self.end_headers()
                return
            BLOBS[actual] = blob
            self.send_response(201)
            self.send_header("content-length", "0")
            self.end_headers()
        elif path.startswith(f"/v2/{REPO}/manifests/"):
            ref = path.rsplit("/", 1)[1]
            try:
                manifest = json.loads(body)
            except ValueError:
                self.send_response(400)
                self.end_headers()
                return
            missing = [d for d in [manifest.get("config", {}).get("digest")]
                       + [l["digest"] for l in manifest.get("layers", [])]
                       if d and d not in BLOBS]
            if missing:
                self.send_response(400)
                self.end_headers()
                return
            store_manifest(ref, manifest)
            self.send_response(201)
            self.send_header("content-length", "0")
            self.end_headers()
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
