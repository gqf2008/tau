#!/usr/bin/env python
"""WeCom webhook ingress end-to-end (docs/im-channels.md §企微回环协议,
validate.sh step 5e): drives the real `tau` interactive REPL over a pty
so the session stays alive when the platform's callback arrives.

The new ground vs wa_ingress_e2e.py is the CRYPTO GATE: wecom_mock.py
signs and encrypts for real (NIST-self-tested AES), so these assertions
prove the component — not the host — verifies msg_signature and
decrypts AES-256-CBC:

  leg 1 (negative control): tampered signature -> 403, never steered;
  leg 2: URL verify — encrypted echostr round-trips as plaintext;
  leg 3: encrypted text message -> ack 200 "success" -> idle REPL
         wakes on the steer -> after_response posts to the send API.

The no-consent refusal leg is 5d's (same consent gate), not repeated.

Usage: python scripts/wecom_ingress_e2e.py <tau-binary> <wecom_bridge.wasm> <work-dir>
Requires pywinpty; validate.sh skips step 5e when it is missing.
"""

import os
import socket
import subprocess
import sys
import threading
import time

import winpty

TAU = os.path.abspath(sys.argv[1])
BRIDGE = os.path.abspath(sys.argv[2])
WORK = os.path.abspath(sys.argv[3])

ROUTE = "/wecom/callback"
TOKEN = "loopback-wecom-token"
CORPID = "loopback-corp"
# The mock's default loopback key (base64 of bytes(range(16, 48))).
AES_KEY = "EBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8"


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class Pty:
    def __init__(self, argv, cwd, env=None):
        self.proc = winpty.PtyProcess.spawn(argv, cwd=cwd, dimensions=(24, 100), env=env)
        self.buf = ""
        self.lock = threading.Lock()
        self.alive = True
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        while True:
            try:
                data = self.proc.read(4096)
            except EOFError:
                self.alive = False
                return
            if not data:
                self.alive = False
                return
            with self.lock:
                self.buf += data

    def wait(self, needle, timeout=30):
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self.lock:
                if needle in self.buf:
                    return
            if not self.alive:
                raise AssertionError(f"process died waiting for {needle!r}: {self.buf[-800:]}")
            time.sleep(0.05)
        raise AssertionError(f"timeout waiting for {needle!r}: {self.buf[-1500:]}")

    def send(self, text):
        self.proc.write(text)

    def wait_exit(self, timeout=10):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if not self.alive:
                return
            time.sleep(0.05)
        raise AssertionError("process still alive after /quit: " + self.buf[-400:])

    def close(self):
        try:
            self.proc.terminate()
        except Exception:
            pass


def start_mock(webhook_url, retry_seconds, log_path):
    mock_script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "wecom_mock.py")
    log = open(log_path, "w")
    proc = subprocess.Popen(
        [
            sys.executable,
            mock_script,
            "--webhook",
            webhook_url,
            "--retry-seconds",
            str(retry_seconds),
            "--token",
            TOKEN,
            "--aes-key",
            AES_KEY,
            "--corpid",
            CORPID,
        ],
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    deadline = time.time() + 10
    while time.time() < deadline:
        log.flush()
        with open(log_path) as f:
            for line in f:
                if line.startswith("wecom mock ready "):
                    return proc, int(line.split()[-1])
        time.sleep(0.1)
    raise AssertionError("wecom mock did not start")


def main():
    os.makedirs(WORK, exist_ok=True)
    ingress_port = free_port()
    webhook = f"http://127.0.0.1:{ingress_port}{ROUTE}"
    log_path = os.path.join(WORK, "wecom_mock_e2e.log")
    mock, api_port = start_mock(webhook, 20, log_path)
    try:
        env = dict(os.environ)
        env.update(
            {
                "WECOM_TOKEN": TOKEN,
                "WECOM_ENCODING_AES_KEY": AES_KEY,
                "WECOM_CORP_ID": CORPID,
            }
        )
        tau = Pty(
            [
                TAU,
                "--allow-unsigned",
                "--mcp-bridge",
                BRIDGE,
                "--mcp-url",
                f"http://127.0.0.1:{api_port}",
                "--ingress",
                f"127.0.0.1:{ingress_port}",
                "--demo",
            ],
            cwd=WORK,
            env=env,
        )
        try:
            tau.wait("you> ")
            # Legs 1+2 (bad signature refused, url verify) run as the
            # listener comes up; leg 3's message wakes the idle REPL.
            tau.wait("steer: [IM wecom loopback-user]", 30)
            tau.wait("wecom: reply posted to loopback-user", 30)
            tau.send("/quit\r")
            tau.wait_exit()
        finally:
            tau.close()
        with open(log_path) as f:
            log = f.read()
        assert "bad signature refused (403)" in log, f"negative control missing: {log}"
        assert "BAD SIGNATURE NOT REFUSED" not in log, f"crypto gate open: {log}"
        assert "url verify ok" in log, f"echostr round-trip failed: {log}"
        assert "message delivered, ack: 200 success" in log, f"message not acked: {log}"
        assert "WECOM SEND: " in log and "loopback-user" in log, \
            f"reply never reached the send API: {log}"
    finally:
        mock.terminate()
    print(
        "ok — wecom crypto gate verified end to end "
        "(bad sig 403 → echostr round-trip → encrypted message → steer → idle wake → send API)"
    )


main()
