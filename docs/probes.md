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

| # | point | payload (in) | verdicts | jev use case |
|---|-------|--------------|----------|--------------|
| 1 | `before_run` | prompt messages, resources | replace messages / continue | classify intent → route to model, inject skill, reject out-of-scope prompts |
| 2 | `transform_context` | assembled messages + system prompt | replace either / continue | trim or re-rank context; inject retrieved memory |
| 3 | `before_request` | model, step (assistant/compaction/...), attempt, stream options | patch options / continue | bool "transient?" → retry with backoff; classify error → fail over to another model |
| 4 | `after_response` | settled assistant message (+status/headers) | replace message / continue | score quality → accept or regenerate; redact secrets |
| 5 | `before_tool` | tool name + args (validated) | continue / replace args / block{reason} | bool "destructive?" → block or escalate to user; classify risk tier for policy |
| 6 | `after_tool` | name, args, result content, is_error | replace result / terminate run | truncate or sanitize result; score usefulness → decide run ends early |
| 7 | `before_compaction` | reason (manual/threshold/overflow), prepared summary input | decline / replace with custom summary | custom summarizer; veto compaction during critical phase |
| 8 | `before_navigation` | branch target, prepared branch summary | decline / replace summary | custom branch summarizer |
| 9 | `before_run_end` | run id, all messages | follow-up prompt / none | chain runs: score "task done?" → enqueue next step |

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
