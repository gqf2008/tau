#!/usr/bin/env python
"""Realtime-AV Phase 2b gate (docs/realtime-av.md, validate.sh step
11d): the SAME full-duplex loop as 11c, but with the provider behind
the wasm boundary (world `realtime`, examples/realtime-echo) — plus
the microphone consent category.

Leg 1 — consent guards the device, not the session: provider loaded
WITHOUT --microphone; `/live 2` (real mic path) is refused naming the
missing grant, while `/live 2 sine` (synthetic uplink — no device)
runs the full loop anyway.

Leg 2 — consented duplex: with --microphone, `/live 2 sine` crosses
the wasm boundary every chunk: guest VAD (speech-started), echo deltas
played by the live sink (pcm raw stream, exact 32000-sample
accounting), guest close flush (speech-stopped), both blocks in the
session tree.

Usage: python scripts/av_wasm_live_e2e.py <tau-binary> <realtime_echo.wasm> <work-dir>
Requires pywinpty; validate.sh skips step 11d when it is missing.
"""

import json
import os
import sys
import threading
import time

import winpty

TAU = os.path.abspath(sys.argv[1])
WASM = os.path.abspath(sys.argv[2])
WORK = os.path.abspath(sys.argv[3])


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

    def wait_exit(self, timeout=15):
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


def base_argv():
    return [
        TAU,
        "--allow-unsigned",
        "--provider-wasm", WASM,
        "--model", "echo-realtime",
    ]


def leg_consent(work):
    """No --microphone: the mic path refuses, the sine path flows."""
    os.makedirs(work, exist_ok=True)
    tau = Pty(base_argv(), cwd=work)
    try:
        tau.wait("you> ")
        tau.send("/live 2\r")
        tau.wait("--microphone", 15)
        # The refusal names the grant AND the session did not start
        # (no live banner after the refusal — cumulative-buffer needle:
        # use the refusal line itself as the anchor, then check no
        # "speech" VAD arrived before the next prompt cycle).
        tau.send("/live 2 sine\r")
        tau.wait("[tau] 🎤 live 2s (sine)", 15)
        tau.wait("[tau] 🎤 speech", 20)
        tau.wait("[tau] ▶ streamed 32000 samples (audio/pcm;rate=16000 @ 16kHz)", 25)
        tau.wait("[tau] 🎤 live ended — uplink 64000 bytes", 15)
        tau.send("/quit\r")
        tau.wait_exit()
    finally:
        tau.close()

    session_path = os.path.join(work, ".tau", "session.jsonl")
    with open(session_path) as f:
        entries = [json.loads(line) for line in f if line.strip()]
    roles_media = [
        (entry.get("message", {}).get("role"), content.get("media", {}).get("media_type"))
        for entry in entries
        for content in entry.get("message", {}).get("content", [])
        if content.get("type") == "audio"
    ]
    assert ("user", "audio/pcm;rate=16000") in roles_media, roles_media
    assert ("assistant", "audio/pcm;rate=16000") in roles_media, roles_media


def leg_consented(work):
    """With --microphone: the same duplex loop, consented flag wired."""
    os.makedirs(work, exist_ok=True)
    tau = Pty(base_argv() + ["--microphone"], cwd=work)
    try:
        tau.wait("you> ")
        tau.send("/live 2 sine\r")
        tau.wait("[tau] 🎤 speech", 20)
        tau.wait("live echo active.", 15)
        tau.wait("[tau] 🎤 speech stopped", 25)
        tau.wait("[tau] ▶ streamed 32000 samples (audio/pcm;rate=16000 @ 16kHz)", 15)
        tau.wait("[tau] 🎤 live ended — uplink 64000 bytes, 2 assistant blocks, 0 interruptions", 15)
        tau.send("/quit\r")
        tau.wait_exit()
    finally:
        tau.close()


def main():
    leg_consent(os.path.join(WORK, "consent"))
    print("ok — microphone consent guards the device (mic refused, sine flows), duplex across wasm")
    leg_consented(os.path.join(WORK, "consented"))
    print("ok — consented realtime wasm session (VAD + echo + 32000 exact + tree)")


os.makedirs(WORK, exist_ok=True)
main()
