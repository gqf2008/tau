#!/usr/bin/env python3
"""Loopback dingtalk-shaped IM platform for validate.sh step 5f
(docs/im-channels.md §钉钉回环协议, stdlib only).

The two dingtalk-shaped increments over the feishu loopback
(im_mock.py), both asserted:

1. the stream frame is DOUBLE-ENCODED JSON — `data` is a string
   holding escaped JSON (the inner message body);
2. the client must ACK on the same ws connection — the mock reads
   frames after pushing and prints "DT ACK: <frame>" when the
   adapter's {"code":200,...} arrives.

- GET /dt upgrades to WebSocket and pushes one CALLBACK frame, then
  reads until EOF (acks, pongs, the close handshake).
- POST /reply records the robot send API reply: "DT REPLY: <body>".

The real gateway handshake (POST /v1.0/gateway/connections exchanging
credentials for a wss endpoint+ticket) only OBTAINS the connection
address — the loopback connects directly, and the doc records the
difference honestly.

Prints "dt mock ready PORT" once listening.
"""

import base64
import hashlib
import json
import struct
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

# The inner message body, then the CALLBACK frame whose `data` carries
# it as an escaped JSON STRING — the double encoding is the shape
# under test, so build it with json.dumps for real, not by hand.
INNER = json.dumps(
    {
        "msgtype": "text",
        "text": {"content": "ping from the dingtalk platform"},
        "senderStaffId": "loopback-user",
        "conversationId": "loopback-c1",
    }
)
INBOUND = json.dumps(
    {
        "specVersion": "1.0",
        "type": "CALLBACK",
        "headers": {
            "contentType": "application/json",
            "messageId": "loopback-m1",
            "time": "1777000002000",
            "topic": "/v1.0/im/bot/messages/get",
        },
        "data": INNER,
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


def read_exact(conn, n):
    buf = b""
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf


def read_frames(conn):
    """Read client frames until EOF; print the ack when it arrives.
    Client frames are masked (RFC 6455) — unmask them."""
    try:
        while True:
            hdr = read_exact(conn, 2)
            opcode = hdr[0] & 0x0F
            masked = hdr[1] & 0x80
            length = hdr[1] & 0x7F
            if length == 126:
                length = struct.unpack(">H", read_exact(conn, 2))[0]
            elif length == 127:
                length = struct.unpack(">Q", read_exact(conn, 8))[0]
            mask = read_exact(conn, 4) if masked else b"\x00" * 4
            payload = bytearray(read_exact(conn, length))
            for i in range(length):
                payload[i] ^= mask[i % 4]
            if opcode == 0x8:  # close
                return
            if opcode == 0x1:  # text
                text = payload.decode(errors="replace")
                if '"code":200' in text.replace(" ", ""):
                    print(f"DT ACK: {text}", flush=True)
    except (EOFError, ConnectionError, OSError):
        pass


class Handler(BaseHTTPRequestHandler):
    # tungstenite refuses HTTP/1.0 responses ("HTTP version must be 1.1").
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        if not self.path.startswith("/dt"):
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
        print("dt mock: inbound CALLBACK frame pushed", flush=True)
        read_frames(self.connection)

    def do_POST(self):
        if not self.path.startswith("/reply"):
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length).decode(errors="replace")
        print(f"DT REPLY: {body}", flush=True)
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
    print(f"dt mock ready {server.server_address[1]}", flush=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    threading.Event().wait()


if __name__ == "__main__":
    main()
