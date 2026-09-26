//! The event bus.
//!
//! tau is event-driven: every lifecycle occurrence — run start/end, text
//! deltas, tool calls, probe verdicts — is published on one broadcast
//! channel. Renderers, observers, telemetry, and (via tau-ext) wasm
//! extensions all subscribe to the same stream; none of them can slow down
//! or wedge the harness (lagging subscribers simply miss events).
//!
//! Probes (`crate::probe`) are the *synchronous* complement: a probe is a
//! point where the harness pauses and a handler returns a verdict that can
//! change execution. Every probe outcome is also published here as an
//! [`AgentEvent::Probe`] so observers see the full decision trail.

use tokio::sync::broadcast;

use crate::agent::AgentEvent;

/// Capacity bounds queued events per subscriber; a slow subscriber that
/// falls behind gets `RecvError::Lagged` and skips ahead.
pub const BUS_CAPACITY: usize = 1024;

pub type EventBus = broadcast::Sender<AgentEvent>;
pub type EventStream = broadcast::Receiver<AgentEvent>;

pub fn new_bus() -> EventBus {
    broadcast::channel(BUS_CAPACITY).0
}
