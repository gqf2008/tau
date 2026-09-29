# Probe points

A probe is a lifecycle point where an extension can **observe and influence**
a run. Probes are synchronous request→verdict calls (not fire-and-forget
events): the harness pauses, the extension returns a verdict, the harness
acts on it. This is the surface that makes a typed-decision plugin (jev-style
classify/bool/score) useful: every probe payload is JSON, every verdict is a
small typed answer.

Naming: the WIT interface was `hooks` in `tau:extension@0.1.0`;
0.2.0 renamed it `probes`, matching the code, this doc, and the
`tau probes` CLI.

The JSON below is the wire: what a component receives as `payload-json` and
answers as `replace-json`, and what `tau probes --json` prints. Host-side
handlers (`ProbeHandler`) do not index that JSON by name — they get a
`tau_core::probe_payload::ProbePayload`, one arm per point
(`ProbePayload::BeforeTool(ToolCall { .. })`, …), so a renamed field is a
compile error instead of a silent no-op; `ProbePayload::point()` says which
point a payload belongs to. `to_json` / `merge_json` are the edge that keeps
the two in step: the shapes are exactly the ones in this doc, and a
`replace` that does not fit its point degrades to `continue` (with a line on
stderr) rather than failing the run. The 0.7.0 contract types the same
payloads (docs/wit-redesign.md §3).

Model (derived from pi's `HookMap`, packages/agent/src/harness/agent-harness.ts):

- `continue` — no opinion, run proceeds unchanged.
- `replace(payload)` — the probe returns a modified payload (messages, args,
  result...) that the harness uses instead.
- `block(reason)` — the probe vetoes the action; semantics per point.

## Run lifecycle

All nine points are wired (verdicts `continue` / `replace` / `block`;
block aborts with the reason as the run error, except `before_tool` where
it becomes a blocked tool result handed back to the model, and
`before_compaction`/`before_navigation` where it vetoes the action).

| # | point | payload (in) | verdicts | jev use case |
|---|-------|--------------|----------|--------------|
| 1 | `before_run` | prompt message | replace prompt / block | classify intent → route to model, inject skill, reject out-of-scope prompts |
| 2 | `transform_context` | assembled messages + system prompt | replace either / continue | trim or re-rank context; inject retrieved memory |
| 3 | `before_request` | final request: messages + system + tools | replace messages/system / block | bool "transient?" → retry with backoff; classify error → fail over to another model |
| 4 | `after_response` | assembled assistant message + stop reason | replace message / block | score quality → accept or regenerate; redact secrets |
| 5 | `before_tool` | tool id + name + args | continue / replace args / block{reason} | bool "destructive?" → block or escalate to user; classify risk tier for policy |
| 6 | `after_tool` | id, name, args, result content, is_error | replace result / continue | truncate or sanitize result; score usefulness → decide run ends early |
| 7 | `before_run_end` | produced messages + stop reason | replace messages / block | chain runs: score "task done?" → enqueue next step |
| 8 | `before_compaction` | `{"reason": "manual", "messages": [Message]}` | continue / replace{messages} / block{reason} | custom summarizer (replace the message set the summarizer sees); veto compaction during a critical phase |
| 9 | `before_navigation` | `{"target": entry-id, "summary": string — first line of the target's message}` | continue / replace{target} / block{reason} | policy: veto rewinding past a critical point; redirect navigation to a sanctioned entry |

## Session lifecycle — observe-only (wired since 0.2.0)

These three points **observe, never influence**: the harness fires them
via `Agent::observe`, every registered handler sees the payload, and any
non-`continue` verdict is reported on the bus as a `Probe` event with
action `ignored` — visible on the decision trail, never honored. Fire
sites live in the CLI (session open/close, `--continue-from` and REPL
`/fork`), not in the run loop.

| point | payload | fires |
|-------|---------|-------|
| `session_start` | `{"session": path, "cwd": path, "model": string}` | once the agent exists, after the renderer subscribes |
| `branch` | `{"from": entry-id|null, "to": entry-id}` | after a navigation the `before_navigation` probe allowed |
| `session_end` | same as `session_start` | on clean exit (print-mode end, compact-only return, REPL quit); error exits skip it — a crash is not a session end |

Two renderer caveats, both honest consequences of the bus: in print
mode the CLI renderer detaches at run end, so a `session_end` notice is
for bus subscribers (embedders) — the guest observes the point
regardless; and a buffered notice can drain a line late relative to
direct `eprintln` status lines.

### Reserved (design commitment, not shipping code)

`text_delta` / `tool_progress` — streaming progress, high volume, never
blocking. `text_delta` is covered by the pull subscription
(`host.subscribe(["text-delta"])` + `host.poll`,
`docs/stream-subscribe.md`, landed in 0.3.0): the probe slot stays
reserved — probes on high-frequency paths remain forbidden — and
`ProbePoint::from_name` still rejects both names. `tool_progress` stays
reserved outright: the bus has no tool-progress event producer, and the
subscription does not invent one.

## Rules

1. Probes run in registration order; `replace` verdicts thread through (each
   probe sees the previous probe's replacement). First `block` wins.
2. A probe that errors is treated as `continue` and reported — a broken
   extension must not wedge the harness.
3. Blocking probes (#5, #9 in the hot path) must be fast; the model is not
   waiting on anything else. jev's ~1s latency is acceptable at `before_tool`,
   not at `text_delta` (which is why streaming points stay observe-only and
   reserved).
4. Points are discoverable: `tau probes` lists them with payload schemas, so
   extension authors never guess.
