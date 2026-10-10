# ACP: tau attached to an editor

`tau --acp` speaks the **Agent Client Protocol** — the JSON-RPC dialect
Zed (and any other ACP host) uses to drive an agent — over stdin and
stdout. The editor spawns tau, asks what it can do, opens a session, and
sends prompts; tau streams the answer back and asks before it touches
the machine.

```
Zed  ──stdin/stdout, JSON-RPC──►  tau --acp  ──►  the same agent loop
                                                  the REPL runs
```

The mode is a *transport*, not a second agent: same tools, same probes,
same session files, same `--demo`. What changes is who is on the other
end of the conversation.

## Why native

pi's ACP support comes from adapters (`pi-acp` and friends) that spawn
`pi --mode rpc` and translate between pi's RPC JSONL and ACP. tau has no
such protocol surface to adapt, and building one to then translate it
would add a second format nobody asked for. `agent-client-protocol`
(the official Rust SDK) is a dependency of `tau-cli`, and the agent side
of the protocol is a handful of mappings (`crates/tau-cli/src/acp/`).
One process, no adapter, no extra binary.

## Running it

```bash
tau --acp                  # speaks ACP on stdin/stdout until stdin closes
```

`--acp` is a flag, not a subcommand: every other flag keeps its meaning.
It conflicts with `-p`/`--print`, `--compact`, `--continue`, and
`--continue-from` — those are other ways to run a prompt — and clap
refuses the pair with exit code 2.

In Zed, `settings.json`:

```json
{
  "agent_servers": {
    "tau": {
      "type": "custom",
      "command": "tau",
      "args": ["--acp"],
      "env": {}
    }
  }
}
```

**stdout carries the protocol and nothing else.** Every diagnostic tau
prints was already on stderr (the `[tau] …` lines), and the client shows
that stream to the user. A client that logs stdout to a file gets
parsable JSON-RPC, one message per line; framing is the SDK's
(newline-delimited, LF written, CRLF tolerated on read).

**Credentials come from the spawn environment.** There are no auth
methods in the handshake: `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` (or
`ANTHROPIC_AUTH_TOKEN`), `TAU_MODEL`, and the `--model` / `--provider`
args the host passes work exactly as they do in print mode.
To offer two models, add a second `agent_servers` entry with different
`args` or `env` — switching models mid-session is not supported.

Everything process-scoped is built once, before the handshake is
answered: the tools the components registered, the probes, and the
model. A component given as an `oci://` reference is pulled at that
point, so a slow registry makes the handshake slow.

## One process, N sessions

A client may open any number of sessions in one tau process. Each gets
its own JSONL file and its own agent; the tools, probes, host, and model
are shared.

- `--session <dir>` names the **directory** in this mode (it names a
  file in print and REPL modes). The default is `.tau/sessions`,
  relative to the process's working directory.
- Each session lands at `<dir>/<session-id>.jsonl`, created at
  `session/new` — so the file exists by the time the client is told the
  id, and an unwritable directory fails there instead of mid-turn.
- The file is an ordinary tau session. `tau tree --session <file>`
  reads it, `--continue --session <file>` resumes it, and a torn tail
  from an earlier crash is reported at `session/new`.
- `session/new`'s `cwd` is compared with the process's: tau has one
  working directory per process and the built-in tools resolve against
  it, so a client asking for a different one gets a session rooted where
  tau actually is, plus one line on stderr saying so.
- `mcpServers` in `session/new` is ignored in v1. tau's MCP servers come
  from `-e`/`--mcp-bridge` with their own consent gate; a host that wants
  a server injected spawns tau with the bridge component.
- The first session owns the process-wide host channel (extension
  notifications and injection); this is a known limitation of the
  one-sink-per-process wiring, and it is why an extension that notifies
  per session should be given one session per process.

## The handshake

`initialize` answers exactly what is true:

| field | value | why |
|-------|-------|-----|
| `protocolVersion` | `1` | only the stable v1 protocol is implemented; a client offering another version is answered `1` and told on stderr |
| `agentCapabilities.loadSession` | `false` | the file that would back it is on disk, but replaying a branch as `session/update` is not written — claiming it would make an editor offer a history it cannot show |
| `agentCapabilities.promptCapabilities.image` | `true` | an image block becomes `Content::Image`, which is how the built-in `read` already returns a picture |
| `agentCapabilities.promptCapabilities.audio` | `true` | an audio block becomes `Content::Audio`, the shape the REPL's own `/mic` sends; a realtime provider's *output* audio is the other direction and is not carried (see the event table) |
| `authMethods` | `[]` | credentials come from the spawn environment; `authenticate` is answered method-not-found |
| `agentInfo` | `tau <version>` | |

Every method tau does not implement — `session/load`, `session/set_mode`,
`authenticate`, the `fs/*` and `terminal/*` families — gets a JSON-RPC
method-not-found from the SDK. Nothing is silently accepted.

## A prompt's life

`session/prompt` carries content blocks. They are folded, in order, into
the one user message tau's loop takes:

| block | becomes |
|-------|---------|
| `text` | `Content::Text` |
| `image` | `Content::Image` (base64 decoded to bytes) |
| `audio` | `Content::Audio` (base64 decoded to bytes) |
| `resource_link` | the text `[resource: <uri>]` |
| anything else | `[unsupported prompt block: <name>]`, plus a line on stderr |

The turn appends the user message, runs the agent loop on the session's
active branch **as read from the file** (a turn runs on what is on disk,
including a turn another client wrote), streams updates, persists what
the loop produced, and answers with a stop reason.

`AgentEvent` becomes `session/update` like this:

| event | update |
|-------|--------|
| `TextDelta` | `agent_message_chunk` |
| `ToolCallStart` | `tool_call` (`in_progress`, with the tool's kind) |
| `ToolCallEnd` | `tool_call_update` (`completed`/`failed`, with a flattened preview of the output — the whole output is in the session file) |
| `RunEnd { stop }` | the response to `session/prompt` |
| `RunError { message }` | a JSON-RPC error (`internal_error`, message in `data` and `message`) |
| probe, steer, follow-up, extension notice/fact | stderr only — stable v1 has no update for them, and neither is a log |
| a realtime provider's audio, VAD, barge-in | dropped, with one line on stderr per turn to say so — this mode maps text deltas only, and a line per chunk is not a log. ACP v1 does define an audio content block, so carrying them is possible; not carrying them is this version's scope. The audio itself is kept: the loop assembles the chunks into `Content::Audio` in the assistant message, which is in the session file |

Tool-call ids on the wire are `{turn}:{provider id}` — the loop's ids
come from the provider and a scripted model reuses them across turns, so
a client drawing two turns would otherwise see one call that never ends.
The session file keeps the provider's own ids.

| tau's stop | the wire's |
|------------|------------|
| `Stop` | `end_turn` |
| `Length` | `max_tokens` |
| `Aborted` | `cancelled` |

A second `session/prompt` for a session that is already running a turn
is answered `invalid_params` rather than queued; a prompt for a session
this process never created is answered the same way, naming the session.

## Cancelling

`session/cancel` sends `Control::Abort` to the running turn, and it is
honored at **model-event checkpoints**: between deltas, and between
batches of tool calls. What that means in practice:

- A streaming answer stops within a token or two; the text produced so
  far is kept and persisted, the stop reason is `cancelled`.
- **A running tool is not interrupted.** Abort is checked after the
  batch, so a long command runs to completion — give the tool its own
  `timeout:` (`bash`, `powershell`) rather than relying on cancel.
- A cancel that arrives with **no turn in flight is dropped**, not
  queued. Queued, it would be taken by the *next* run and stop it before
  its first token, on behalf of a client that cancelled a turn that had
  already finished.
- `session/cancel` on a session tau is not serving is reported on stderr
  and otherwise ignored.

## Permissions

The four built-in tools that change the machine — `write`, `edit`,
`bash`, `powershell` — ask the client before they run:

```
tau  ──session/request_permission {toolCall, options}──►  Zed
     ◄── {outcome: "selected", optionId: "allow_once"} ──
```

| option | meaning |
|--------|---------|
| `allow_once` | run it this time |
| `allow_always` | run it, and stop asking for this tool in this session |
| `reject_once` | do not run it |
| `reject_always` | do not run it, and stop asking for this tool in this session |

`always` is remembered **in that session only** — a new session asks
again, and nothing is written to disk. The question names the call the
client was already told about (same id, title, kind, and `rawInput`).

The gate is a `before_tool` probe registered **after** the components'
probes, which is what makes the extension contract hold: an extension
that rewrites a call's arguments has done so before the user is asked, so
what they approve is what will run, and an extension that blocks a call
stops it before anyone is asked about something that will not happen.

There is **no timeout** on the question — it is the user's to answer. The
one thing that unblocks it is `session/cancel`, whose abort arrives as
`cancelled` and is read as a refusal of that call. Every other outcome is
a refusal too: `cancelled`, an option id that was never offered, a
request that did not reach the client, no answer at all. A refusal does
not fail the turn — the loop records a tool result `blocked: <why>` with
`is_error`, the model reads it and continues, and the client sees the
call go `failed`.

**Outside this mode there is no gate at all.** In print and REPL modes
nothing asks: the built-in tools are host code running with the
permissions of the tau process (`docs/builtin-tools.md`,
`docs/extensions.md` §7). The gate is a
property of the editor-attached mode, where there is a human on the
other end of the connection to ask.

The wasm components' posture is separate and simpler since 0.8.0: there
are no per-fingerprint grants left to make — a component runs with the
tau process's permissions, and installing/trusting its signature is the
authorization.

## What v1 does not do

- `session/load` / `session/resume` / `session/fork` — and the
  handshake says so rather than offering a history it cannot replay.
- `session/set_mode`, ACP auth methods, the `fs/*` and `terminal/*`
  delegations: tau uses its own built-in tools in its own working
  directory. Only the client's capabilities are logged.
- Client-provided MCP servers (`mcpServers` is ignored; see above).
- Plan/thought/usage updates and compaction notices: tau produces no
  reasoning stream, and those updates are unstable-v1 and need client
  capabilities tau does not request.
- Realtime voice in this mode: a realtime provider's audio, VAD, and
  barge-in are dropped — with one line on stderr per turn to say so — and
  no microphone or playback is opened. Audio *prompts* are carried (see
  the prompt table); what v1 does not carry is the model's audio coming
  back out.
- Per-session working directories, and per-session isolation of a
  component's state.

## Troubleshooting

- **The client reports a protocol error.** Check stdout: it must be
  JSON-RPC only. Something that prints there (`println!` in an
  extension, a library banner) breaks framing. Everything tau itself
  prints goes to stderr.
- **`tau --acp -p "hi"` exits 2** — by design; the pair is refused.
- **The handshake is slow.** Components are loaded before it is
  answered: start-up includes signing checks and, for `oci://`
  references, the pull.
- **A turn hangs on a tool.** Cancel cannot interrupt a running tool
  (see above); use the tool's `timeout:`.
- **`TAU_ACP_STALL_MS=<n>`** makes a turn wait `n` milliseconds before it
  starts. It exists so the gate can test `session/cancel` against a turn
  that is in flight but has not begun; it is not a product knob.

## Testing

`crates/tau-cli/tests/acp_stdio.rs` drives the binary over real pipes:
the handshake, the session file, streaming, the built-in tool round trip,
cancel in flight and at rest, concurrent prompts, `session/load` and
`authenticate` as method-not-found, CRLF, and — with a loopback HTTP
provider speaking SSE, because `--demo` can never script a gated tool —
the permission gate's allow and reject legs.

`scripts/acp_e2e.py` is the same protocol from a client written in
python: it parses every line tau writes, and `scripts/validate.sh` 1f
runs it against the release binary.
