#!/usr/bin/env python3
"""WhatsApp-shaped loopback platform for the ingress leg of
docs/im-channels.md (validate.sh step 5d).

Two halves, one process:

1. A webhook SENDER: retries POSTing one inbound message event to the
   component's ingress listener (--webhook URL) until accepted or the
   retry budget (--retry-seconds) runs out. The listener only exists
   after the bridge's session_start probe runs, so delivery begins
   connection-refused; the first success prints
   "wa mock: webhook delivered, ack: <status> <body>". Budget exhausted
   prints "wa mock: webhook undelivered".

2. The platform's send API: POST /send records the reply
   ("WA SEND: <body>"). ThreadingHTTPServer: a threaded server so the
   webhook sender thread never blocks the API.

Prints "wa mock ready PORT" once listening.
"""

import argparse
import http.client
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

INBOUND = {
    "type": "message",
    "chat_id": "loopback-c1",
    "user": "loopback-user",
    "text": "ping from the IM platform",
}


def deliver(webhook_url: str, retry_seconds: float) -> None:
    body = json.dumps(INBOUND).encode()
    deadline = time.monotonic() + retry_seconds
    while time.monotonic() < deadline:
        try:
            conn = http.client.HTTPConnection(
                webhook_url.split("://", 1)[1].split("/", 1)[0], timeout=5
            )
            path = "/" + webhook_url.split("://", 1)[1].split("/", 1)[1]
            conn.request(
                "POST", path, body, {"content-type": "application/json"}
            )
            resp = conn.getresponse()
            ack = resp.read().decode(errors="replace")
            print(
                f"wa mock: webhook delivered, ack: {resp.status} {ack}",
                flush=True,
            )
            conn.close()
            return
        except OSError:
            # Listener not up yet (or already gone).
            time.sleep(0.05)
    print("wa mock: webhook undelivered", flush=True)


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        body = self.rfile.read(length)
        if self.path == "/send":
            print(f"WA SEND: {body.decode(errors='replace')}", flush=True)
            payload = b'{"ok":true}'
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(payload)))
            # No keep-alive: the reply stream is one-shot per run.
            self.send_header("connection", "close")
            self.end_headers()
            self.wfile.write(payload)
        else:
            self.send_response(404)
            self.send_header("content-length", "0")
            self.send_header("connection", "close")
            self.end_headers()

    def log_message(self, *args):
        pass


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--webhook", required=True, help="component ingress URL")
    parser.add_argument(
        "--retry-seconds",
        type=float,
        default=15.0,
        help="webhook delivery budget (refusal leg passes ~3)",
    )
    args = parser.parse_args()

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    port = server.server_address[1]
    threading.Thread(
        target=deliver, args=(args.webhook, args.retry_seconds), daemon=True
    ).start()
    print(f"wa mock ready {port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
