# Event-driven architecture

The harness has exactly one spine: a broadcast event bus
(`tau_core::bus`). Everything that happens in a run is an event on the bus;
everything that wants to know is a subscriber.

## Two channels, one trail

| channel | sync? | can influence execution? | mechanism |
|---------|-------|--------------------------|-----------|
| **events** | no (fire-and-forget) | no | `EventBus` broadcast (`AgentEvent`) |
| **probes** | yes (request→verdict) | yes | `ProbeRegistry` (see `docs/probes.md`) |

Probes are the synchronous points where the harness pauses for a verdict
(`continue` / `replace` / `block`). Every non-trivial probe outcome is *also*
published on the bus as `AgentEvent::Probe`, so observers always see the full
decision trail — the two channels never diverge. The session-lifecycle
points (`session_start` / `branch` / `session_end`) are observe-only: the
harness never pauses, and a misused verdict surfaces as action `ignored`
(`docs/probes.md`).

## Who subscribes

- the CLI renderer (one task, prints text deltas and tool activity);
- wasm extensions publishing through the host channel: `host.notify`
  lands as `AgentEvent::ExtensionNotice`, `host.emit` as
  `AgentEvent::ExtensionFact` (observe-only facts; the schema is the
  extension's own). Extensions observe high-frequency deltas through
  the pull subscription (`host.subscribe`/`poll`,
  `docs/stream-subscribe.md`): the guest drains a bounded ring during
  its own invocations, with a `lagged(n)` marker on overrun;
- telemetry / session recording (future), all without touching the loop.

Subscribers cannot wedge the harness: the bus is bounded
(`BUS_CAPACITY = 1024`), a slow subscriber gets `Lagged` and skips ahead.

## Rules

1. The agent loop emits, never inspects its own subscribers.
2. Anything that changes execution must be a probe, not an event — events
   are facts, probes are decisions. If an extension needs to veto, it
   registers a probe at the matching point.
3. High-volume points (text deltas, audio deltas, tool progress) are events
   only; probes on those paths would put wasm round-trips between the model
   and the user. Realtime-style audio streams in as `audio-delta` model
   events (typed raw bytes on the 0.2.0 contract — base64 exists only
   at the JSON edges); the loop assembles
   same-media-type runs into `Content::Audio` blocks on the assistant
   message, and observers see byte counts, never the payload.
4. Commands (steer, follow-up, abort) arrive on the **control channel**
   (`tau_core::control`) — same event-driven shape, opposite direction:
   an unbounded mpsc the loop drains at checkpoints (stream events, turn
   and run boundaries), never blocking the model path. Steer lands after
   the current turn's tool results (never between tool_use and
   tool_result); follow-ups continue the same run at its natural end;
   abort stops at the next checkpoint with `StopReason::Aborted`. Applied
   commands are published on the bus (`AgentEvent::Steer` / `FollowUp` /
   `Abort`), so the trail never diverges. Extensions reach this same
   channel through `host.steer` / `host.follow-up` (consent-gated,
   enqueue-only) — identical checkpoint semantics, no second path.
