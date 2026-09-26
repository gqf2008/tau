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
decision trail — the two channels never diverge.

## Who subscribes

- the CLI renderer (one task, prints text deltas and tool activity);
- wasm extensions that observe (via tau-ext; observe-only, never blocking);
- telemetry / session recording (future), all without touching the loop.

Subscribers cannot wedge the harness: the bus is bounded
(`BUS_CAPACITY = 1024`), a slow subscriber gets `Lagged` and skips ahead.

## Rules

1. The agent loop emits, never inspects its own subscribers.
2. Anything that changes execution must be a probe, not an event — events
   are facts, probes are decisions. If an extension needs to veto, it
   registers a probe at the matching point.
3. High-volume points (text deltas, tool progress) are events only; probes
   on those paths would put wasm round-trips between the model and the user.
4. Commands (steering, abort, follow-up injection) will arrive as a control
   channel into the loop — same event-driven shape, opposite direction.
   Not wired in v0.
