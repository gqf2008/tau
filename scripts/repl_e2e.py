#!/usr/bin/env python
"""Interactive-REPL end-to-end over a real pty (Windows: pywinpty).

Drives the actual `tau --demo` process the way a user meets it:
banner, prompt, one full turn, /help, idle Ctrl-C (hint, REPL alive),
/quit (clean exit), and the line-editor recall history — which must
survive a /quit exit (it used to persist only on Ctrl-D).

Usage: python scripts/repl_e2e.py <path-to-tau-binary>
Requires pywinpty; validate.sh skips this step when it is missing.
"""

import os
import shutil
import sys
import tempfile
import threading
import time

import winpty

TAU = sys.argv[1]

BANNER = "tau interactive — /help for commands"
PROMPT = "you> "


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
            if not self.alive and not self.proc.isalive():
                return
            time.sleep(0.05)
        self.proc.terminate()
        raise AssertionError(f"/quit did not exit: {self.buf[-800:]}")


def main():
    history = os.path.expanduser("~/.tau/repl_history.txt")
    backup = None
    if os.path.exists(history):
        backup = history + ".e2e-backup"
        shutil.copy2(history, backup)
        os.remove(history)

    scratch = tempfile.mkdtemp(prefix="tau-repl-e2e-")
    try:
        session = os.path.join(scratch, "session.jsonl")
        pt = Pty([TAU, "--demo", "--session", session], cwd=scratch)

        pt.wait(BANNER)
        print("ok — banner and mid-run cheat sheet printed")

        pt.send("hello from the pty\r")
        pt.wait("tau is alive")
        pt.wait("[tau] ready")
        print("ok — a full turn streamed and completed")

        pt.send("/help\r")
        pt.wait("commands: /help /compact /fork")
        print("ok — /help lists the commands")

        # Idle Ctrl-C: a hint, and the REPL stays alive afterwards.
        pt.send("\x03")
        pt.wait("Ctrl-C aborts a running turn")
        print("ok — idle Ctrl-C hints instead of killing the REPL")

        pt.send("/quit\r")
        pt.wait_exit()
        print("ok — /quit exits cleanly")

        # The recall history must survive a /quit exit (per-line append).
        with open(history, encoding="utf-8", errors="replace") as f:
            saved = f.read()
        assert "hello from the pty" in saved, (
            f"/quit lost the recall history: {saved!r}"
        )
        print("ok — recall history persisted across /quit")

        # And the session file holds the exchange.
        with open(session, encoding="utf-8") as f:
            entries = f.read()
        assert "hello from the pty" in entries and "tau is alive" in entries
        print("ok — the exchange landed in the session tree")
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
        if os.path.exists(history):
            os.remove(history)
        if backup:
            shutil.move(backup, history)


if __name__ == "__main__":
    main()
