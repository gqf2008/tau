#!/usr/bin/env python
"""Agent Client Protocol end to end (docs/acp.md): a scripted client
speaks to `tau --acp` over the pipes an editor would use, and every line
tau writes to stdout is parsed as JSON-RPC — a diagnostic that leaked
into the protocol stream would fail this leg, which is the point of it.

What is proven here, in one process each:

- The handshake: protocol version 1, `loadSession` false, images true,
  no auth methods, and the client-info block.
- `session/new` lands a session file at the DEFAULT location —
  `<cwd>/.tau/sessions/<id>.jsonl`, with the client's cwd — and that
  file is an ordinary tau session: `tau tree --session` reads it.
- A prompt streams chunks and answers `end_turn`, and the file holds
  the turn.
- `session/cancel` with no turn in flight is dropped, not queued: the
  turn that runs afterwards is not cancelled (a queued abort would be
  taken by the next run and stop it before its first token).
- A tool round trip with a wasm component (`-e upper.wasm`): the call
  is announced and then completed, the answer quotes it — and NO
  permission request is sent for it. The gate covers the mutating
  built-ins (write/edit/bash/powershell), not an extension's tool; the
  Rust leg `a_gated_call_asks_the_client_and_the_answer_decides` covers
  the gate itself with a real provider.
- `--acp -p hi` is refused: two ways to run a prompt do not combine.

Usage: python scripts/acp_e2e.py <path-to-tau-binary> <upper.wasm> <work-dir>
"""

import json
import os
import queue
import shutil
import subprocess
import sys
import threading
import time

# Absolute: every process below runs in a scratch cwd of its own, so a
# path relative to the caller would not resolve there.
TAU = os.path.abspath(sys.argv[1])
UPPER = os.path.abspath(sys.argv[2])
WORK = os.path.abspath(sys.argv[3])

PATIENCE = 30.0
_next_id = [0]


def an_id():
    _next_id[0] += 1
    return _next_id[0]


class Client:
    """One `tau --acp` process, spoken to over its pipes.

    Its cwd is a scratch directory of its own, which is what makes the
    session-file assertion mean something: the default location is
    relative to the cwd the client asked for.
    """

    def __init__(self, name, extra, env=None):
        self.cwd = os.path.join(WORK, name, "cwd")
        os.makedirs(self.cwd, exist_ok=True)
        environment = dict(os.environ)
        environment.update(env or {})
        self.child = subprocess.Popen(
            [TAU, "--acp"] + extra,
            cwd=self.cwd,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.lines = queue.Queue()
        self.not_json = []
        self.json_lines = 0
        self.notes = []
        threading.Thread(target=self._read_stdout, daemon=True).start()
        self.err = []
        threading.Thread(target=self._read_stderr, daemon=True).start()

    def _read_stdout(self):
        # Binary, one line at a time: on Windows a text-mode wrapper
        # would rewrite the payload's newlines, and the framing is the
        # one part of this protocol tau cannot be lenient about.
        for raw in self.child.stdout:
            line = raw.decode("utf-8").rstrip("\r\n")
            if not line:
                continue
            try:
                json.loads(line)
                self.json_lines += 1
            except ValueError:
                self.not_json.append(line)
            self.lines.put(line)
        self.lines.put(None)

    def _read_stderr(self):
        # Drained on its own thread: nobody reads it while a leg waits on
        # stdout, and an undrained pipe would wedge tau mid-turn.
        for raw in self.child.stderr:
            self.err.append(raw.decode("utf-8", "replace"))

    def diagnostics(self):
        return "".join(self.err)

    def next_line(self):
        try:
            line = self.lines.get(timeout=PATIENCE)
        except queue.Empty:
            raise AssertionError(
                f"tau said nothing for {PATIENCE}s\n--- stderr ---\n{self.diagnostics()}"
            )
        assert line is not None, (
            f"tau closed stdout mid-conversation\n--- stderr ---\n{self.diagnostics()}"
        )
        assert not self.not_json, (
            f"a line on stdout is not JSON: {self.not_json!r}\n--- stderr ---\n"
            f"{self.diagnostics()}"
        )
        line = json.loads(line)
        # A request from tau to this client (an id and a method; a
        # notification has a method and no id). The only request tau sends is
        # the permission gate's, and it covers the mutating built-ins
        # (write/edit/bash/powershell) - never a component's tool, which
        # is what this leg runs. Nothing here answers requests, so one
        # would also leave tau waiting on a prompt no user can see: fail
        # on the method rather than on the timeout.
        assert not ("method" in line and "id" in line), (
            f"tau asked this client to answer {line['method']!r}; the gate is for "
            f"the mutating built-ins, and this leg runs a component's tool: {line}"
        )
        return line

    def send(self, message):
        self.child.stdin.write((json.dumps(message) + "\n").encode("utf-8"))
        self.child.stdin.flush()

    def call(self, method, params):
        """One request, answered: notifications that arrive first are
        kept in `self.notes`."""
        ident = an_id()
        self.send({"jsonrpc": "2.0", "id": ident, "method": method, "params": params})
        while True:
            line = self.next_line()
            if "id" in line:
                assert line["id"] == ident, f"the answer to {method} carries its own id: {line}"
                return line
            self.notes.append(line)

    def initialize(self):
        return self.call(
            "initialize",
            {
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": True, "writeTextFile": True}},
                "clientInfo": {"name": "tau-acp-e2e", "version": "0"},
            },
        )

    def new_session(self):
        answer = self.call("session/new", {"cwd": self.cwd, "mcpServers": []})
        return answer["result"]["sessionId"]

    def prompt(self, session, text):
        """Send a prompt and wait for its answer, returning the
        notifications that arrived while it ran."""
        ident = an_id()
        marker = len(self.notes)
        self.send(
            {
                "jsonrpc": "2.0",
                "id": ident,
                "method": "session/prompt",
                "params": {"sessionId": session, "prompt": [{"type": "text", "text": text}]},
            }
        )
        while True:
            line = self.next_line()
            if "id" in line:
                assert line["id"] == ident, f"the answer to the prompt: {line}"
                return line, self.notes[marker:]
            self.notes.append(line)

    def cancel(self, session):
        self.send({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}})

    def shut_down(self):
        self.child.stdin.close()
        try:
            code = self.child.wait(timeout=PATIENCE)
        except subprocess.TimeoutExpired:
            self.child.kill()
            raise AssertionError(
                f"tau did not exit after its stdin closed\n--- stderr ---\n{self.diagnostics()}"
            )
        return code


def updates(notes, session):
    """The `update` object of every `session/update` notification, in
    arrival order — and the assertion that nothing else arrived, and that
    it all belonged to this session."""
    out = []
    for note in notes:
        assert note.get("method") == "session/update", f"an unexpected notification: {note}"
        assert note["params"]["sessionId"] == session, f"another session's update: {note}"
        out.append(note["params"]["update"])
    return out


def kinds(streamed):
    return [u["sessionUpdate"] for u in streamed]


def chunk_text(update):
    return update["content"]["text"]


def tree_of(file):
    tree = subprocess.run(
        [TAU, "tree", "--session", file],
        cwd=WORK,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=PATIENCE,
    )
    assert tree.returncode == 0, f"tau tree refused {file}: {tree.stderr.decode('utf-8')}"
    return tree.stdout.decode("utf-8")


def wait_for_stderr(agent, fragment):
    """Stderr is drained on its own thread, so a line written during the
    leg may not have been collected yet when it ends."""
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if fragment in agent.diagnostics():
            return True
        time.sleep(0.05)
    return False


def handshake_and_a_turn():
    agent = Client("prompt", ["--demo"])
    try:
        result = agent.initialize()["result"]
        # Exactly what an editor reads before it offers anything, and
        # each of these is a promise: v1 only, no history it cannot
        # replay, images it really does carry, credentials from the
        # environment it spawned us with.
        assert result["protocolVersion"] == 1, result
        capabilities = result["agentCapabilities"]
        assert capabilities["loadSession"] is False, result
        assert capabilities["promptCapabilities"]["image"] is True, result
        assert result["authMethods"] == [], result
        assert result["agentInfo"]["name"] == "tau", result
        print("ok — handshake: v1, no session/load, images yes, no auth methods")

        session = agent.new_session()
        assert session, "session/new answered without an id"
        # The default location, under the cwd the client asked for: the
        # flag defaults to `.tau/sessions`, and nothing else was passed.
        file = os.path.join(agent.cwd, ".tau", "sessions", f"{session}.jsonl")
        assert os.path.exists(file), f"session/new promised {file} and did not write it"
        print("ok — session/new landed <cwd>/.tau/sessions/<id>.jsonl")

        answer, notes = agent.prompt(session, "hello")
        assert answer["result"]["stopReason"] == "end_turn", answer
        streamed = updates(notes, session)
        assert kinds(streamed) == ["agent_message_chunk"] * 3, streamed
        text = "".join(chunk_text(u) for u in streamed)
        assert text.startswith("tau is alive."), text
        print("ok — a prompt streamed its chunks and answered end_turn")

        # The file is the session, not a log of it: tau's own reader
        # opens what the protocol just wrote.
        tree = tree_of(file)
        assert "hello" in tree and "tau is alive." in tree, tree
        assert len(tree.strip().splitlines()) == 2, f"one prompt, one answer: {tree}"
        print("ok — tau tree reads the turn the protocol wrote")

        assert agent.shut_down() == 0, "clean EOF on stdin is a normal end"
        assert agent.json_lines > 0 and not agent.not_json, "stdout carried no JSON-RPC"
    finally:
        agent.child.kill()
    print("ok — every line on stdout was JSON, and stdin closing ended tau cleanly")


def a_tool_round_trip_over_the_wire():
    agent = Client("tool", ["--demo", "--allow-unsigned", "-e", UPPER])
    try:
        agent.initialize()
        session = agent.new_session()

        # Nothing is running. The abort would sit in the control channel
        # and be taken by the *next* run, which would then stop before
        # its first token on behalf of a client that cancelled a turn
        # that had already finished — so it has to be dropped here, and
        # the turn below is the proof.
        agent.cancel(session)

        answer, notes = agent.prompt(session, "shout e2e")
        assert answer["result"]["stopReason"] == "end_turn", (
            f"a cancel sent at rest reached the next turn: {answer}"
        )
        assert wait_for_stderr(agent, "arrived with no turn in flight"), (
            f"the cancel at rest was not reported as ignored\n--- stderr ---\n"
            f"{agent.diagnostics()}"
        )
        print("ok — a cancel with no turn in flight is dropped, not queued")

        streamed = updates(notes, session)
        assert kinds(streamed) == [
            "tool_call",
            "tool_call_update",
            "agent_message_chunk",
            "agent_message_chunk",
            "agent_message_chunk",
        ], streamed

        # Announced before its result, so an editor has something to draw
        # the update on. The id is namespaced to the turn: the loop's own
        # ids come from the provider and repeat across turns.
        call = streamed[0]
        assert call["toolCallId"] == "1:demo-call-1", call
        assert call["title"] == "upper", call
        # `kind` is left off the wire when it is the protocol's own default
        # (`other`), which is what an extension's tool is: a client reads
        # the absence as exactly that.
        assert call.get("kind", "other") == "other", call
        assert call["status"] == "in_progress", call

        done = streamed[1]
        assert done["status"] == "completed", done
        preview = done["content"][0]["content"]["text"]
        assert "SHOUT E2E" in preview, done

        text = "".join(chunk_text(u) for u in streamed[2:])
        assert "The tool answered: SHOUT E2E" in text, text
        print("ok — a component's tool ran, was announced, and the model saw it")

        assert agent.shut_down() == 0
    finally:
        agent.child.kill()


def acp_refuses_to_share_the_run_with_a_prompt():
    out = subprocess.run(
        [TAU, "--acp", "-p", "hi"],
        cwd=WORK,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=PATIENCE,
    )
    assert out.returncode != 0, (
        "a prompt and ACP are two ways to run; the pair must be refused: "
        f"{out.stdout.decode('utf-8')}"
    )
    print(f"ok — --acp -p is refused (exit {out.returncode})")


handshake_and_a_turn()
a_tool_round_trip_over_the_wire()
acp_refuses_to_share_the_run_with_a_prompt()
print("acp e2e: all legs passed")
