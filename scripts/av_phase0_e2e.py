#!/usr/bin/env python
"""Realtime-AV Phase 0 smoke (docs/realtime-av.md, validate.sh step 11b):
drives the interactive REPL over a pty through the full voice loop with
the hardware-free synthetic signal:

  /mic 2 sine  →  440Hz WAV synthesized host-side  →  Content::Audio
  user message  →  demo model echoes it as 3 AudioDelta chunks  →
  assembly concatenates them  →  post-run playback path runs.

Asserted: capture bytes, the demo model's echo turn, the assembled
audio/wav block IN the session JSONL, and that the playback path ran
("▶ played" when an output device exists, a playback notice otherwise —
headless machines must not go red, but the path must execute).

Usage: python scripts/av_phase0_e2e.py <path-to-tau-binary> <work-dir>
Requires pywinpty; validate.sh skips step 11b when it is missing.
"""

import json
import os
import sys
import threading
import time

import winpty

TAU = os.path.abspath(sys.argv[1])
WORK = os.path.abspath(sys.argv[2])


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


def main():
    os.makedirs(WORK, exist_ok=True)
    tau = Pty([TAU, "--demo"], cwd=WORK)
    try:
        tau.wait("you> ")
        tau.send("/mic 2 sine\r")
        # Capture (synthesized) must be reported with its byte count…
        tau.wait("[tau] 🎙 captured ", 15)
        # …the demo model echoes the clip (the turn ran on the voice
        # message)…
        tau.wait("echoing your voice clip.", 30)
        # …and the playback path executed — either real playback (an
        # output device exists) or the honest notice (headless).
        deadline = time.time() + 15
        while time.time() < deadline:
            with tau.lock:
                if "[tau] ▶ played " in tau.buf or "[tau] playback: " in tau.buf:
                    break
            time.sleep(0.1)
        else:
            raise AssertionError(f"playback path never ran: {tau.buf[-1500:]}")
        tau.send("/quit\r")
        tau.wait_exit()
    finally:
        tau.close()

    # The assembled audio block must be IN the session tree (base64
    # inline — MediaSource::Bytes serializes as base64; media_type is
    # the assertable anchor).
    session_path = os.path.join(WORK, ".tau", "session.jsonl")
    with open(session_path) as f:
        entries = [json.loads(line) for line in f if line.strip()]
    kinds = []
    audio_found = False
    for entry in entries:
        # Flat shape: {"id","parent","type":"message","message":{...}}.
        msg = entry.get("message", {})
        for content in msg.get("content", []):
            ctype = content.get("type")
            kinds.append(ctype)
            if ctype == "audio" and content.get("media", {}).get("media_type") == "audio/wav":
                audio_found = True
    assert audio_found, f"no audio/wav block in the session: kinds={kinds}"

    print("ok — voice loop closed (sine → Content::Audio uplink → demo echo "
          "→ 3 AudioDelta assembled → playback path → audio/wav in session)")


main()
