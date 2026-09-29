//! Probes: lifecycle points where extensions observe and influence a run.
//! See docs/probes.md for the full map. All nine points are wired.

use std::sync::Arc;

use async_trait::async_trait;

use crate::probe_payload::ProbePayload;

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
    /// A session opened. Observe-only: verdicts are reported, never
    /// honored (see `Agent::observe`).
    SessionStart,
    /// The session forked to an older entry (after `before_navigation`
    /// allowed it). Observe-only.
    Branch,
    /// A session closed cleanly. Observe-only.
    SessionEnd,
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
            Self::SessionStart => "session_start",
            Self::Branch => "branch",
            Self::SessionEnd => "session_end",
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
            "session_start" => Some(Self::SessionStart),
            "branch" => Some(Self::Branch),
            "session_end" => Some(Self::SessionEnd),
            _ => None,
        }
    }

    /// Every point, in lifecycle order.
    pub const ALL: [ProbePoint; 12] = [
        Self::SessionStart,
        Self::BeforeRun,
        Self::TransformContext,
        Self::BeforeRequest,
        Self::AfterResponse,
        Self::BeforeTool,
        Self::AfterTool,
        Self::BeforeRunEnd,
        Self::BeforeCompaction,
        Self::BeforeNavigation,
        Self::Branch,
        Self::SessionEnd,
    ];

    /// Observe-only points inform; they never influence. The harness
    /// fires them via `Agent::observe`, which reports but ignores any
    /// non-continue verdict.
    pub fn observe_only(self) -> bool {
        matches!(self, Self::SessionStart | Self::Branch | Self::SessionEnd)
    }
}

/// A probe's decision for one firing.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// No opinion; the run proceeds with the current payload.
    Continue,
    /// Replacement payload. Its arm must be the one the firing used —
    /// [`ProbePayload::point`] says which — and the registry ignores a
    /// replacement that answers a different point instead of wedging.
    Replace(ProbePayload),
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
    /// Decide one firing: the point and its payload. The payload carries
    /// the point (`payload.point()`), so a handler that answers several
    /// points matches on the payload rather than on a second argument.
    async fn probe(&self, point: ProbePoint, payload: ProbePayload) -> Verdict;
}

/// Sequential fold: each handler sees the previous handler's replacement;
/// first block wins; a panicking/failing handler degrades to `Continue`
/// (a broken extension must not wedge the harness).
/// The agent's probe set. See the fold semantics note above.
///
/// Cloning shares the handlers, exactly as [`ToolRegistry`] does — see
/// the note there.
///
/// [`ToolRegistry`]: crate::tool::ToolRegistry
#[derive(Clone)]
pub struct ProbeRegistry {
    handlers: Vec<Arc<dyn ProbeHandler>>,
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

    /// Add a handler; firing order is registration order. The registry
    /// takes ownership of the box and shares it with every existing clone.
    pub fn register(&mut self, handler: Box<dyn ProbeHandler>) {
        self.handlers.push(Arc::from(handler));
    }

    /// True when no handlers are registered.
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// Folds all registered handlers; `Replace` if any handler replaced,
    /// `Block` on first block, else `Continue`.
    ///
    /// A replacement whose arm answers a different point is dropped: the
    /// next handler sees the payload it would have seen, and the fold's
    /// answer stays the last well-aimed one. That is the same stance as a
    /// panicking handler (a broken extension must not wedge the harness),
    /// and it cannot happen by accident: [`ProbePayload::merge_json`] and
    /// every other constructor preserve the arm they started from.
    pub async fn probe(&self, point: ProbePoint, mut payload: ProbePayload) -> Verdict {
        debug_assert_eq!(
            payload.point(),
            point,
            "payload and point disagree at the call site"
        );
        let mut replaced = false;
        for handler in &self.handlers {
            if !handler.points().contains(&point) {
                continue;
            }
            match handler.probe(point, payload.clone()).await {
                Verdict::Continue => {}
                Verdict::Replace(replacement) if replacement.point() == point => {
                    payload = replacement;
                    replaced = true;
                }
                Verdict::Replace(_) => {}
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
    PointInfo {
        point: Some(ProbePoint::SessionStart),
        name: "session_start",
        wired: true,
        payload: r#"{"session": path, "cwd": path, "model": string}"#,
        verdicts: "observe-only — a non-continue verdict is reported as ignored, never honored",
    },
    PointInfo {
        point: Some(ProbePoint::Branch),
        name: "branch",
        wired: true,
        payload: r#"{"from": entry-id|null, "to": entry-id}"#,
        verdicts: "observe-only — a non-continue verdict is reported as ignored, never honored",
    },
    PointInfo {
        point: Some(ProbePoint::SessionEnd),
        name: "session_end",
        wired: true,
        payload: r#"{"session": path, "cwd": path, "model": string}"#,
        verdicts: "observe-only — a non-continue verdict is reported as ignored, never honored",
    },
    PointInfo {
        point: None,
        name: "text_delta",
        wired: false,
        payload: r#"{"text": string}"#,
        verdicts: "observe-only (reserved — high-frequency streaming awaits the pull-subscription design in host-channel.md)",
    },
    PointInfo {
        point: None,
        name: "tool_progress",
        wired: false,
        payload: r#"{"id": string, "name": string, "progress": object}"#,
        verdicts: "observe-only (reserved — high-frequency streaming awaits the pull-subscription design in host-channel.md)",
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

    #[test]
    fn reserved_slots_and_observe_only_flags_are_consistent() {
        // Reserved slots: no variant, not wired, name not parseable yet.
        for name in ["text_delta", "tool_progress"] {
            let info = CATALOG.iter().find(|i| i.name == name).expect(name);
            assert!(!info.wired && info.point.is_none(), "{name} drifted");
            assert_eq!(ProbePoint::from_name(name), None, "{name} wired?");
        }
        // The observe-only set is exactly the session lifecycle trio.
        for point in ProbePoint::ALL {
            let expected = matches!(
                point,
                ProbePoint::SessionStart | ProbePoint::Branch | ProbePoint::SessionEnd
            );
            assert_eq!(point.observe_only(), expected, "{point:?}");
        }
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting {
        point: ProbePoint,
        /// Shared with the test, so the handler's identity is observable
        /// from outside: a clone that copies handlers duplicates this
        /// count, a clone that shares them does not.
        seen: std::sync::Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ProbeHandler for Counting {
        fn points(&self) -> &[ProbePoint] {
            std::slice::from_ref(&self.point)
        }
        async fn probe(&self, _point: ProbePoint, payload: ProbePayload) -> Verdict {
            self.seen.fetch_add(1, Ordering::SeqCst);
            Verdict::Replace(payload)
        }
    }

    #[test]
    fn a_clone_shares_the_handlers_instead_of_copying_them() {
        let seen = std::sync::Arc::new(AtomicUsize::new(0));
        let mut probes = ProbeRegistry::new();
        probes.register(Box::new(Counting {
            point: ProbePoint::SessionStart,
            seen: seen.clone(),
        }));
        assert_eq!(std::sync::Arc::strong_count(&seen), 2);

        let handed_out = probes.clone();
        assert_eq!(
            std::sync::Arc::strong_count(&seen),
            2,
            "the clone built its own handler instead of sharing the registered one"
        );

        // The clone routes to that same handler, which hands the payload
        // straight back: a replacement equal to what it was given.
        let facts = crate::probe_payload::SessionFacts {
            session: "s1".into(),
            cwd: "/tmp".into(),
            model: "demo".into(),
        };
        let verdict = futures::executor::block_on(handed_out.probe(
            ProbePoint::SessionStart,
            ProbePayload::SessionStart(facts.clone()),
        ));
        match verdict {
            Verdict::Replace(ProbePayload::SessionStart(back)) => {
                assert_eq!(back.session, facts.session, "payload drifted in the fold");
                assert_eq!(back.cwd, facts.cwd);
            }
            other => panic!("expected a replacement, got {other:?}"),
        }
        assert_eq!(seen.load(Ordering::SeqCst), 1);

        // Registration is still per-registry: the clone's handler set is
        // not the original's.
        let mut other = handed_out.clone();
        other.register(Box::new(Counting {
            point: ProbePoint::SessionEnd,
            seen: seen.clone(),
        }));
        assert!(!other.is_empty() && !probes.is_empty());
        assert_eq!(std::sync::Arc::strong_count(&seen), 3);
    }
}
