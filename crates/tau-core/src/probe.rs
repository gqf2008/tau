//! Probes: lifecycle points where extensions observe and influence a run.
//! See docs/probes.md for the full map; v0 wires the run-critical four.

use async_trait::async_trait;
use serde_json::Value as Json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProbePoint {
    BeforeRun,
    TransformContext,
    BeforeTool,
    AfterTool,
}

impl ProbePoint {
    pub fn name(self) -> &'static str {
        match self {
            Self::BeforeRun => "before_run",
            Self::TransformContext => "transform_context",
            Self::BeforeTool => "before_tool",
            Self::AfterTool => "after_tool",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "before_run" => Some(Self::BeforeRun),
            "transform_context" => Some(Self::TransformContext),
            "before_tool" => Some(Self::BeforeTool),
            "after_tool" => Some(Self::AfterTool),
            _ => None,
        }
    }

    pub const ALL: [ProbePoint; 4] = [
        Self::BeforeRun,
        Self::TransformContext,
        Self::BeforeTool,
        Self::AfterTool,
    ];
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Continue,
    /// Replacement payload, point-specific.
    Replace(Json),
    /// Veto the action; reason goes back to the model (tool points) or user.
    Block { reason: String },
}

#[async_trait]
pub trait ProbeHandler: Send + Sync {
    /// Which points this handler answers; others are never routed to it.
    fn points(&self) -> &[ProbePoint];
    async fn probe(&self, point: ProbePoint, payload: Json) -> Verdict;
}

/// Sequential fold: each handler sees the previous handler's replacement;
/// first block wins; a panicking/failing handler degrades to `Continue`
/// (a broken extension must not wedge the harness).
pub struct ProbeRegistry {
    handlers: Vec<Box<dyn ProbeHandler>>,
}

impl Default for ProbeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProbeRegistry {
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    pub fn register(&mut self, handler: Box<dyn ProbeHandler>) {
        self.handlers.push(handler);
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// Folds all registered handlers; `Replace` if any handler replaced,
    /// `Block` on first block, else `Continue`.
    pub async fn probe(&self, point: ProbePoint, mut payload: Json) -> Verdict {
        let mut replaced = false;
        for handler in &self.handlers {
            if !handler.points().contains(&point) {
                continue;
            }
            match handler.probe(point, payload.clone()).await {
                Verdict::Continue => {}
                Verdict::Replace(replacement) => {
                    payload = replacement;
                    replaced = true;
                }
                Verdict::Block { reason } => return Verdict::Block { reason },
            }
        }
        if replaced {
            Verdict::Replace(payload)
        } else {
            Verdict::Continue
        }
    }
}
