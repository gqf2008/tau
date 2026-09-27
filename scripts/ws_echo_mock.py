"""Loopback WebSocket echo server for validate.sh (stdlib only).

Handshake + frame codec, no dependencies: answers the upgrade, echoes
text/binary frames verbatim, replies to pings, honors close. Prints
"ws echo ready PORT" on stdout once listening.
"""

import base64
import hashlib
import socket
import struct
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


def read_exact(conn, n):
    buf = b""
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf


def encode_len(n):
    if n < 126:
        return bytes([n])
    if n < 65536:
        return b"\x7e" + struct.pack(">H", n)
    return b"\x7f" + struct.pack(">Q", n)


def echo_loop(conn):
    while True:
        try:
            hdr = read_exact(conn, 2)
        except (EOFError, ConnectionError, OSError):
            return
        opcode = hdr[0] & 0x0F
        masked = hdr[1] & 0x80
        length = hdr[1] & 0x7F
        try:
            if length == 126:
                length = struct.unpack(">H", read_exact(conn, 2))[0]
            elif length == 127:
                length = struct.unpack(">Q", read_exact(conn, 8))[0]
            mask = read_exact(conn, 4) if masked else b"\x00" * 4
            payload = bytearray(read_exact(conn, length))
        except (EOFError, ConnectionError, OSError):
            return
        for i in range(length):
            payload[i] ^= mask[i % 4]
        if opcode == 0x8:  # close
            conn.sendall(b"\x88\x00")
            return
        if opcode == 0x9:  # ping -> pong
            conn.sendall(b"\x8a" + encode_len(length) + bytes(payload))
            continue
        if opcode in (0x1, 0x2):  # echo text/binary
            conn.sendall(bytes([0x80 | opcode]) + encode_len(length) + bytes(payload))


class Handler(BaseHTTPRequestHandler):
    # tungstenite refuses HTTP/1.0 responses ("HTTP version must be 1.1").
    protocol_version = "HTTP/1.1"

    def do_GET(self):
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
        try:
            echo_loop(self.connection)
        except (ConnectionError, OSError):
            pass

    def log_message(self, *args):
        pass


def main():
    server = HTTPServer(("127.0.0.1", 0), Handler)
    print(f"ws echo ready {server.server_address[1]}", flush=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    threading.Event().wait()


if __name__ == "__main__":
    main()
