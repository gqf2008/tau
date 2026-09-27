#!/usr/bin/env python
"""Realtime-AV Phase 2a full-duplex gate (docs/realtime-av.md,
validate.sh step 11c). Two legs over a pty:

Leg 1 — happy path: `/live 2 sine` streams paced 50ms PCM chunks
(16kHz s16le) into the demo model's RealtimeSession; the double VADs
(SpeechStarted), echoes every byte down as AudioDelta (same media
type), and at close flushes SpeechStopped + Done. Asserted, in order
of the pipeline: the live banner, server VAD, the sink's announce
line (the PCM raw-stream path, not WAV), the RunEnd sink accounting
(32000 samples == 2s @ 16kHz echoed whole — byte-exact), the live
summary line, and BOTH session blocks in the JSONL (user uplink +
assistant assembly).

Leg 2 — barge-in: `/live 30 sine`, then Ctrl-C mid-stream → the
driver calls interrupt(), the double emits Interrupted, the renderer
clears the sink and prints the line; the REPL stays alive and /quit
exits cleanly (orderly live shutdown + recording).

Usage: python scripts/av_live_e2e.py <path-to-tau-binary> <work-dir>
Requires pywinpty; validate.sh skips step 11c when it is missing.
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


def leg_happy(work):
    os.makedirs(work, exist_ok=True)
    tau = Pty([TAU, "--demo"], cwd=work)
    try:
        tau.wait("you> ")
        tau.send("/live 2 sine\r")
        tau.wait("[tau] 🎤 live 2s (sine)", 10)
        # Server VAD (the double's deterministic script)…
        tau.wait("[tau] 🎤 speech", 15)
        # …the sink announced the PCM raw-stream segment (Phase 1's
        # pcm path, rate from the MIME parameter)…
        tau.wait("[tau] ▶ streaming (audio/pcm;rate=16000 @ 16kHz)", 15)
        # …the echo note came down as text…
        tau.wait("live echo active.", 15)
        # …close flushed VAD-off, the sink accounted for EVERY echoed
        # sample (2s @ 16kHz = 32000 — byte-exact full-duplex
        # accounting, null sink or not)…
        tau.wait("[tau] 🎤 speech stopped", 20)
        tau.wait("[tau] ▶ streamed 32000 samples (audio/pcm;rate=16000 @ 16kHz)", 15)
        # …and the REPL recorded the session.
        tau.wait("[tau] 🎤 live ended — uplink 64000 bytes", 15)
        tau.send("/quit\r")
        tau.wait_exit()
    finally:
        tau.close()

    session_path = os.path.join(work, ".tau", "session.jsonl")
    with open(session_path) as f:
        entries = [json.loads(line) for line in f if line.strip()]
    roles_media = []
    for entry in entries:
        msg = entry.get("message", {})
        for content in msg.get("content", []):
            if content.get("type") == "audio":
                roles_media.append((msg.get("role"), content.get("media", {}).get("media_type")))
    assert ("user", "audio/pcm;rate=16000") in roles_media, roles_media
    assert ("assistant", "audio/pcm;rate=16000") in roles_media, roles_media


def leg_barge_in(work):
    os.makedirs(work, exist_ok=True)
    tau = Pty([TAU, "--demo"], cwd=work)
    try:
        tau.wait("you> ")
        tau.send("/live 30 sine\r")
        tau.wait("[tau] ▶ streaming (audio/pcm;rate=16000 @ 16kHz)", 20)
        # Barge-in: Ctrl-C mid-stream = interrupt(), not death.
        tau.send("\x03")
        tau.wait("[tau] ⚡ interrupted — buffer cleared", 15)
        # The session survives the interruption and the REPL is alive:
        # /quit must exit cleanly (orderly close + recording).
        tau.send("/quit\r")
        tau.wait_exit()
    finally:
        tau.close()

    session_path = os.path.join(work, ".tau", "session.jsonl")
    with open(session_path) as f:
        entries = [json.loads(line) for line in f if line.strip()]
    # The interrupted session still recorded its uplink.
    assert any(
        content.get("type") == "audio"
        for entry in entries
        for content in entry.get("message", {}).get("content", [])
    ), "interrupted live session left no audio in the tree"


def main():
    leg_happy(os.path.join(WORK, "happy"))
    print("ok — live happy path (VAD → pcm echo → sink accounting 32000 exact → tree)")
    leg_barge_in(os.path.join(WORK, "barge"))
    print("ok — barge-in (Ctrl-C → Interrupted → buffer cleared → REPL alive → clean /quit)")


os.makedirs(WORK, exist_ok=True)
main()
