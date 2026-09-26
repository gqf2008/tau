"""Mock OCI registry for tau tests: registry v2 manifest + blob GETs,
optionally gated behind the anonymous bearer-token dance.

Usage: mock_oci_registry.py <port> <blob-file> [--auth]
Serves repo "test/component" with tag "latest" and the blob file as the
single wasm layer. With --auth, manifest/blob GETs 401 with a
WWW-Authenticate challenge and require the token issued by /token.
"""

import hashlib
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1])
BLOB = open(sys.argv[2], "rb").read()
AUTH = "--auth" in sys.argv
DIGEST = "sha256:" + hashlib.sha256(BLOB).hexdigest()
TOKEN = "mock-token-123"
REPO = "test/component"

MANIFEST = json.dumps(
    {
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
                "digest": DIGEST,
                "size": len(BLOB),
            }
        ],
    }
).encode()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        # self.path includes the query string; route on the path part only.
        path = self.path.split("?", 1)[0]
        if path == "/token":
            body = json.dumps({"token": TOKEN}).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        if AUTH and self.headers.get("authorization") != f"Bearer {TOKEN}":
            self.send_response(401)
            self.send_header(
                "www-authenticate",
                f'Bearer realm="http://127.0.0.1:{PORT}/token",service="mock",'
                f'scope="repository:{REPO}:pull"',
            )
            self.end_headers()
            return

        if path in (f"/v2/{REPO}/manifests/latest", f"/v2/{REPO}/manifests/{DIGEST}"):
            self.send_response(200)
            self.send_header("content-type", "application/vnd.oci.image.manifest.v1+json")
            self.send_header("content-length", str(len(MANIFEST)))
            self.end_headers()
            self.wfile.write(MANIFEST)
        elif path == f"/v2/{REPO}/blobs/{DIGEST}":
            self.send_response(200)
            self.send_header("content-length", str(len(BLOB)))
            self.end_headers()
            self.wfile.write(BLOB)
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
