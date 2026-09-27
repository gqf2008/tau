"""Loopback IM platform for validate.sh (stdlib only).

Speaks the feishu-shaped loopback protocol from docs/im-channels.md:
- GET /im upgrades to WebSocket and immediately pushes one inbound
  message event (a text frame: {"type":"message","chat_id","user",
  "text"}), then keeps the socket open until the peer goes away.
- POST /reply records the adapter's outbound reply and prints
  "IM REPLY: <body>" on stdout for the validate assertion.

Frame codec shared with ws_echo_mock.py (hand-rolled, no deps).
Prints "im mock ready PORT" once listening.
"""

import base64
import hashlib
import json
import os
import struct
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

# The sender is parameterizable so validate.sh can exercise the identity
# allowlist's refusal leg (IM_MOCK_USER=intruder).
INBOUND = json.dumps(
    {
        "type": "message",
        "chat_id": "loopback-c1",
        "user": os.environ.get("IM_MOCK_USER", "loopback-user"),
        "text": "ping from the IM platform",
    }
)


def encode_frame(payload):
    data = payload.encode()
    header = bytearray([0x81])  # FIN + text
    n = len(data)
    if n < 126:
        header.append(n)
    elif n < 65536:
        header.append(126)
        header += struct.pack(">H", n)
    else:
        header.append(127)
        header += struct.pack(">Q", n)
    return bytes(header) + data


def drain_until_close(conn):
    """Read and ignore frames (pongs, the close handshake) until EOF."""
    try:
        while conn.recv(4096):
            pass
    except (ConnectionError, OSError):
        pass


class Handler(BaseHTTPRequestHandler):
    # tungstenite refuses HTTP/1.0 responses ("HTTP version must be 1.1").
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        if not self.path.startswith("/im"):
            self.send_error(404)
            return
        key = self.headers.get("Sec-WebSocket-Key")
        if not key:
            self.send_error(400)
            return
        accept = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()
        self.send_response(101, "Switching Protocols")
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.end_headers()
        self.close_connection = True  # we own the socket from here
        # A real platform pushes the event when it happens; the loopback
        # pushes on connect. The host-side ws actor holds the frame until
        # the component's next call point drains it.
        self.connection.sendall(encode_frame(INBOUND))
        print("im mock: inbound message pushed", flush=True)
        drain_until_close(self.connection)

    def do_POST(self):
        if not self.path.startswith("/reply"):
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length).decode(errors="replace")
        print(f"IM REPLY: {body}", flush=True)
        self.send_response(200)
        self.send_header("Content-Length", "0")
        # Close after the reply: without it the keep-alive read for a next
        # request aborts (the client leaves) and the 200 can be lost.
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True

    def log_message(self, *args):
        pass


def main():
    # Threaded: the ws long connection's handler blocks for the session's
    # lifetime; the reply POST must be served concurrently.
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    print(f"im mock ready {server.server_address[1]}", flush=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    threading.Event().wait()


if __name__ == "__main__":
    main()
