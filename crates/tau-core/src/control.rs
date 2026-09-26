//! The control channel: commands into the running agent loop — the same
//! event-driven shape as the bus, opposite direction (events.md rule 4).
//!
//! Delivery is an unbounded mpsc; the loop drains it at checkpoints and
//! never blocks the model path on it:
//!
//! - **Abort**: honored at the next stream event or tool boundary; the run
//!   ends with `StopReason::Aborted` and whatever was produced so far.
//! - **Steer**: injected after the current assistant turn's tool results
//!   are recorded — never between a tool_use and its tool_result, so
//!   provider invariants hold by construction.
//! - **FollowUp**: waits until the run would naturally end, then continues
//!   the same run with the message as the next prompt: one RunStart, one
//!   RunEnd, a single trail.
//!
//! Every applied command is also published on the bus
//! (`AgentEvent::Steer` / `FollowUp`), so the decision trail never
//! diverges from what observers see.

use crate::types::Message;

#[derive(Debug, Clone)]
pub enum Control {
    Steer(Message),
    FollowUp(Message),
    Abort,
}

pub type ControlTx = tokio::sync::mpsc::UnboundedSender<Control>;
pub type ControlRx = tokio::sync::mpsc::UnboundedReceiver<Control>;

pub fn channel() -> (ControlTx, ControlRx) {
    tokio::sync::mpsc::unbounded_channel()
}
