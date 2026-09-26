# Probe points

A probe is a lifecycle point where an extension can **observe and influence**
a run. Probes are synchronous request→verdict calls (not fire-and-forget
events): the harness pauses, the extension returns a verdict, the harness
acts on it. This is the surface that makes a typed-decision plugin (jev-style
classify/bool/score) useful: every probe payload is JSON, every verdict is a
small typed answer.

Model (derived from pi's `HookMap`, packages/agent/src/harness/agent-harness.ts):

- `continue` — no opinion, run proceeds unchanged.
- `replace(payload)` — the probe returns a modified payload (messages, args,
  result...) that the harness uses instead.
- `block(reason)` — the probe vetoes the action; semantics per point.

## Run lifecycle

Wired: 1–7 and 9 (verdicts `continue` / `replace` / `block`; block aborts
with the reason as the run error, except `before_tool` where it becomes a
blocked tool result handed back to the model). Reserved: 8 and the
compaction half of 3's payload — they fire when compaction/navigation land;
a probe cannot probe what does not exist.

| # | point | payload (in) | verdicts | jev use case |
|---|-------|--------------|----------|--------------|
| 1 | `before_run` | prompt message | replace prompt / block | classify intent → route to model, inject skill, reject out-of-scope prompts |
| 2 | `transform_context` | assembled messages + system prompt | replace either / continue | trim or re-rank context; inject retrieved memory |
| 3 | `before_request` | final request: messages + system + tools | replace messages/system / block | bool "transient?" → retry with backoff; classify error → fail over to another model |
| 4 | `after_response` | assembled assistant message + stop reason | replace message / block | score quality → accept or regenerate; redact secrets |
| 5 | `before_tool` | tool id + name + args | continue / replace args / block{reason} | bool "destructive?" → block or escalate to user; classify risk tier for policy |
| 6 | `after_tool` | id, name, args, result content, is_error | replace result / continue | truncate or sanitize result; score usefulness → decide run ends early |
| 7 | `before_run_end` | produced messages + stop reason | replace messages / block | chain runs: score "task done?" → enqueue next step |
| 8 | `before_compaction` *(reserved)* | reason (manual/threshold/overflow), prepared summary input | decline / replace with custom summary | custom summarizer; veto compaction during critical phase |
| 9 | `before_navigation` *(reserved)* | branch target, prepared branch summary | decline / replace summary | custom branch summarizer |

## Session lifecycle (observe-only in v0)

| point | payload |
|-------|---------|
| `session_start` / `session_end` | session id, cwd, model |
| `branch` | from entry, to entry |
| `text_delta`, `tool_progress` | streaming progress (high volume, never blocking) |

## Rules

1. Probes run in registration order; `replace` verdicts thread through (each
   probe sees the previous probe's replacement). First `block` wins.
2. A probe that errors is treated as `continue` and reported — a broken
   extension must not wedge the harness.
3. Blocking probes (#5, #9 in the hot path) must be fast; the model is not
   waiting on anything else. jev's ~1s latency is acceptable at `before_tool`,
   not at `text_delta` (which is why streaming points are observe-only).
4. Points are discoverable: `tau probes` lists them with payload schemas, so
   extension authors never guess.
