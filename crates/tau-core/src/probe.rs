//! Probes: lifecycle points where extensions observe and influence a run.
//! See docs/probes.md for the full map. All nine points are wired.

use async_trait::async_trait;
use serde_json::Value as Json;

/// A lifecycle point where a probe may fire. Payload shapes and verdict
/// semantics per point: [`CATALOG`] (rendered by `tau probes --json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProbePoint {
    /// A run is starting; payload is the prompt.
    BeforeRun,
    /// The assembled context may be rewritten before the request is built.
    TransformContext,
    /// The final request (messages + system + tools) before it hits the wire.
    BeforeRequest,
    /// One assistant response just assembled, before tool execution.
    AfterResponse,
    /// A tool call is about to execute; block vetoes, replace rewrites args.
    BeforeTool,
    /// A tool result came back; replace rewrites the recorded output.
    AfterTool,
    /// Natural run end, before the produced messages are returned.
    BeforeRunEnd,
    /// About to compact: the message set to be summarized, before the
    /// summarization request.
    BeforeCompaction,
    /// About to navigate: fork the session at an older entry.
    BeforeNavigation,
}

impl ProbePoint {
    /// The wire name (`before_tool`, ...), as used in WIT and the catalog.
    pub fn name(self) -> &'static str {
        match self {
            Self::BeforeRun => "before_run",
            Self::TransformContext => "transform_context",
            Self::BeforeRequest => "before_request",
            Self::AfterResponse => "after_response",
            Self::BeforeTool => "before_tool",
            Self::AfterTool => "after_tool",
            Self::BeforeRunEnd => "before_run_end",
            Self::BeforeCompaction => "before_compaction",
            Self::BeforeNavigation => "before_navigation",
        }
    }

    /// Parse a wire name back into a point.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "before_run" => Some(Self::BeforeRun),
            "transform_context" => Some(Self::TransformContext),
            "before_request" => Some(Self::BeforeRequest),
            "after_response" => Some(Self::AfterResponse),
            "before_tool" => Some(Self::BeforeTool),
            "after_tool" => Some(Self::AfterTool),
            "before_run_end" => Some(Self::BeforeRunEnd),
            "before_compaction" => Some(Self::BeforeCompaction),
            "before_navigation" => Some(Self::BeforeNavigation),
            _ => None,
        }
    }

    /// Every point, in lifecycle order.
    pub const ALL: [ProbePoint; 9] = [
        Self::BeforeRun,
        Self::TransformContext,
        Self::BeforeRequest,
        Self::AfterResponse,
        Self::BeforeTool,
        Self::AfterTool,
        Self::BeforeRunEnd,
        Self::BeforeCompaction,
        Self::BeforeNavigation,
    ];
}

/// A probe's decision for one firing.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// No opinion; the run proceeds with the current payload.
    Continue,
    /// Replacement payload, point-specific.
    Replace(Json),
    /// Veto the action; reason goes back to the model (tool points) or user.
    Block {
        /// Human-readable justification for the veto.
        reason: String,
    },
}

/// A probe implementation (native or wasm-backed).
#[async_trait]
pub trait ProbeHandler: Send + Sync {
    /// Which points this handler answers; others are never routed to it.
    fn points(&self) -> &[ProbePoint];
    /// Decide one firing: the point and its payload.
    async fn probe(&self, point: ProbePoint, payload: Json) -> Verdict;
}

/// Sequential fold: each handler sees the previous handler's replacement;
/// first block wins; a panicking/failing handler degrades to `Continue`
/// (a broken extension must not wedge the harness).
/// The agent's probe set. See the fold semantics note above.
pub struct ProbeRegistry {
    handlers: Vec<Box<dyn ProbeHandler>>,
}

impl Default for ProbeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProbeRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    /// Add a handler; firing order is registration order.
    pub fn register(&mut self, handler: Box<dyn ProbeHandler>) {
        self.handlers.push(handler);
    }

    /// True when no handlers are registered.
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
    /// Stable wire name.
    pub name: &'static str,
    /// Whether the point fires today (vs. reserved for a future feature).
    pub wired: bool,
    /// JSON shape of the payload handed to the probe.
    pub payload: &'static str,
    /// What each verdict does at this point.
    pub verdicts: &'static str,
}

/// Every probe point the harness knows: wired ones plus reserved slots
/// whose features have not landed yet.
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
        point: Some(ProbePoint::BeforeCompaction),
        name: "before_compaction",
        wired: true,
        payload: r#"{"reason": "manual", "messages": [Message]}"#,
        verdicts: "continue | replace{messages} | block{reason} — block vetoes the compaction",
    },
    PointInfo {
        point: Some(ProbePoint::BeforeNavigation),
        name: "before_navigation",
        wired: true,
        payload: r#"{"target": entry-id, "summary": string — first line of the target's message}"#,
        verdicts: "continue | replace{target} — navigate to a different entry instead | block{reason} — vetoes the navigation",
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
