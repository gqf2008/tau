#!/usr/bin/env python
"""Webhook ingress end-to-end (docs/im-channels.md): drives the real
`tau` interactive REPL over a pty so the session stays alive when the
platform's webhook arrives — the push model means delivery any time the
listener is up lands; print mode's sub-second run would be a race, not
a test.

Consented leg: wa_mock.py POSTs one inbound message event to the
bridge's ingress route; the guest's ingress-handler steers it (inject
consented), the REPL runs the turn, and after_response posts the reply
to the mock's /send API. Asserted end to end: ack 200, steer line in
the REPL, reply notice, WA SEND at the mock.

Refusal leg: without --ingress the listen call fails closed — the REPL
shows the notice naming the missing consent, the mock never delivers,
no reply leaves.

Usage: python scripts/wa_ingress_e2e.py <path-to-tau-binary> <whatsapp_bridge.wasm> <work-dir>
Requires pywinpty; validate.sh skips step 5d when it is missing.
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

ROUTE = "/im/whatsapp"


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class Pty:
    def __init__(self, argv, cwd):
        self.proc = winpty.PtyProcess.spawn(argv, cwd=cwd, dimensions=(24, 100))
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
    mock_script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "wa_mock.py")
    log = open(log_path, "w")
    proc = subprocess.Popen(
        [
            sys.executable,
            mock_script,
            "--webhook",
            webhook_url,
            "--retry-seconds",
            str(retry_seconds),
        ],
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    # The mock prints "wa mock ready PORT" on startup.
    deadline = time.time() + 10
    while time.time() < deadline:
        log.flush()
        with open(log_path) as f:
            for line in f:
                if line.startswith("wa mock ready "):
                    return proc, int(line.split()[-1])
        time.sleep(0.1)
    raise AssertionError("wa mock did not start")


def read_log(path):
    with open(path) as f:
        return f.read()


def consented_leg():
    ingress_port = free_port()
    webhook = f"http://127.0.0.1:{ingress_port}{ROUTE}"
    mock, api_port = start_mock(webhook, 15, f"{WORK}/wa_mock_e2e.log")
    try:
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
                "--allow-inject",
                "--demo",
            ],
            cwd=WORK,
        )
        try:
            tau.wait("you> ")
            # The webhook arrives on its own schedule (the platform
            # pushes); the REPL stays alive and the turn happens.
            tau.wait("steer: [IM chat loopback-c1 from loopback-user]", 30)
            tau.wait("wa: reply posted to loopback-c1", 30)
            tau.send("/quit\r")
            tau.wait_exit()
        finally:
            tau.close()
        log = read_log(f"{WORK}/wa_mock_e2e.log")
        assert "webhook delivered, ack: 200" in log, f"webhook not acked 200: {log}"
        assert "WA SEND: " in log and "loopback-c1" in log, f"reply never reached the platform: {log}"
    finally:
        mock.terminate()
    print("ok — webhook pushed into the live session (ack 200 → steer → turn → reply POST)")


def refusal_leg():
    ingress_port = free_port()
    webhook = f"http://127.0.0.1:{ingress_port}{ROUTE}"
    mock, api_port = start_mock(webhook, 4, f"{WORK}/wa_mock_e2e_refusal.log")
    try:
        tau = Pty(
            [
                TAU,
                "--allow-unsigned",
                "--mcp-bridge",
                BRIDGE,
                "--mcp-url",
                f"http://127.0.0.1:{api_port}",
                "--allow-inject",
                "--demo",
            ],
            cwd=WORK,
        )
        try:
            tau.wait("you> ")
            # Fail-closed: the listen call errors, the notice names the
            # missing consent, and no listener ever exists.
            tau.wait("wa: ingress refused: ingress not consented", 15)
            tau.send("/quit\r")
            # No turn ever ran (nothing steered), so there is no
            # "ready (session: ...)" line to wait on — /quit just exits.
            tau.wait_exit()
        finally:
            tau.close()
        log = read_log(f"{WORK}/wa_mock_e2e_refusal.log")
        assert "webhook undelivered" in log, f"refusal leg delivered?!: {log}"
        assert "WA SEND: " not in log, f"reply left without ingress consent: {log}"
    finally:
        mock.terminate()
    print("ok — no --ingress: listen refused (named consent), webhook undelivered, zero reply")


consented_leg()
refusal_leg()
print("wa ingress e2e: both legs passed")
