//! Probes: lifecycle points where extensions observe and influence a run.
//! See docs/probes.md for the full map. Seven points are wired; the two
//! reserved ones (before_compaction, before_navigation) fire when those
//! features land — a probe cannot probe what does not exist.

use async_trait::async_trait;
use serde_json::Value as Json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProbePoint {
    BeforeRun,
    TransformContext,
    /// The final request (messages + system + tools) before it hits the wire.
    BeforeRequest,
    /// One assistant response just assembled, before tool execution.
    AfterResponse,
    BeforeTool,
    AfterTool,
    /// Natural run end, before the produced messages are returned.
    BeforeRunEnd,
}

impl ProbePoint {
    pub fn name(self) -> &'static str {
        match self {
            Self::BeforeRun => "before_run",
            Self::TransformContext => "transform_context",
            Self::BeforeRequest => "before_request",
            Self::AfterResponse => "after_response",
            Self::BeforeTool => "before_tool",
            Self::AfterTool => "after_tool",
            Self::BeforeRunEnd => "before_run_end",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "before_run" => Some(Self::BeforeRun),
            "transform_context" => Some(Self::TransformContext),
            "before_request" => Some(Self::BeforeRequest),
            "after_response" => Some(Self::AfterResponse),
            "before_tool" => Some(Self::BeforeTool),
            "after_tool" => Some(Self::AfterTool),
            "before_run_end" => Some(Self::BeforeRunEnd),
            _ => None,
        }
    }

    pub const ALL: [ProbePoint; 7] = [
        Self::BeforeRun,
        Self::TransformContext,
        Self::BeforeRequest,
        Self::AfterResponse,
        Self::BeforeTool,
        Self::AfterTool,
        Self::BeforeRunEnd,
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

// ---------------------------------------------------------------------------
// Discovery catalog: `tau probes` renders this; extension authors never
// guess payload shapes. Lives next to the enum so the two cannot drift —
// the round-trip test below pins every wired entry to a real variant.
// ---------------------------------------------------------------------------

/// One catalog entry: a wired probe point, or a reserved one that fires
/// when its feature lands.
pub struct PointInfo {
    /// Present when wired; reserved points have no variant yet.
    pub point: Option<ProbePoint>,
    pub name: &'static str,
    pub wired: bool,
    /// JSON shape of the payload handed to the probe.
    pub payload: &'static str,
    /// What each verdict does at this point.
    pub verdicts: &'static str,
}

pub const CATALOG: &[PointInfo] = &[
    PointInfo {
        point: Some(ProbePoint::BeforeRun),
        name: "before_run",
        wired: true,
        payload: r#"{"prompt": Message}"#,
        verdicts: "continue | replace{prompt} | block{reason} — block aborts the run",
    },
    PointInfo {
        point: Some(ProbePoint::TransformContext),
        name: "transform_context",
        wired: true,
        payload: r#"{"messages": [Message], "system": string|null}"#,
        verdicts: "continue | replace{messages?, system?} — block treated as continue",
    },
    PointInfo {
        point: Some(ProbePoint::BeforeRequest),
        name: "before_request",
        wired: true,
        payload: r#"{"system": string|null, "messages": [Message], "tools": [ToolDef]}"#,
        verdicts: "continue | replace{system, messages} (tools are registry-owned) | block{reason}",
    },
    PointInfo {
        point: Some(ProbePoint::AfterResponse),
        name: "after_response",
        wired: true,
        payload: r#"{"message": Message, "stop": StopReason}"#,
        verdicts: "continue | replace{message, stop?} | block{reason}",
    },
    PointInfo {
        point: Some(ProbePoint::BeforeTool),
        name: "before_tool",
        wired: true,
        payload: r#"{"id": string, "name": string, "args": object}"#,
        verdicts: "continue | replace(args) | block{reason} — block becomes a blocked tool result for the model",
    },
    PointInfo {
        point: Some(ProbePoint::AfterTool),
        name: "after_tool",
        wired: true,
        payload: r#"{"id": string, "name": string, "args": object, "content": string, "isError": bool}"#,
        verdicts: "continue | replace{content?, isError?} | block treated as continue",
    },
    PointInfo {
        point: Some(ProbePoint::BeforeRunEnd),
        name: "before_run_end",
        wired: true,
        payload: r#"{"messages": [Message], "stop": StopReason}"#,
        verdicts: "continue | replace{messages} | block{reason}",
    },
    PointInfo {
        point: None,
        name: "before_compaction",
        wired: false,
        payload: r#"{"reason": "manual"|"threshold"|"overflow", "input": [Message]} (reserved — fires when compaction lands)"#,
        verdicts: "continue | replace{summary} | block{reason}",
    },
    PointInfo {
        point: None,
        name: "before_navigation",
        wired: false,
        payload: r#"{"target": entry-id, "summary": string} (reserved — fires when branch navigation lands)"#,
        verdicts: "continue | replace{summary} | block{reason}",
    },
];

#[cfg(test)]
mod catalog_tests {
    use super::*;

    #[test]
    fn every_wired_entry_names_a_real_point() {
        for info in CATALOG {
            assert_eq!(
                info.wired,
                info.point.is_some(),
                "{}: wired flag disagrees with point presence",
                info.name
            );
            if let Some(point) = info.point {
                assert_eq!(point.name(), info.name, "{}: name drift", info.name);
                assert_eq!(ProbePoint::from_name(info.name), Some(point));
            }
        }
        // Every enum variant appears in the catalog exactly once.
        for point in ProbePoint::ALL {
            assert_eq!(
                CATALOG.iter().filter(|i| i.point == Some(point)).count(),
                1,
                "{:?} missing from catalog",
                point
            );
        }
    }
}
