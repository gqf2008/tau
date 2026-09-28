//! The tool boundary: definitions the model sees, outputs the loop records.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value as Json;

/// A tool the model may call, described with a JSON Schema parameter object.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolDef {
    /// Unique name the model calls (`snake_case` by convention).
    pub name: String,
    /// What it does — the model reads this to decide when to call.
    pub description: String,
    /// JSON Schema object describing the arguments.
    pub parameters: Json,
}

/// The result of one tool execution.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// Result blocks; the model reads them as the tool result content.
    /// Multi-block (text and/or media) since 0.3.0 — docs/tool-media.md.
    pub content: Vec<crate::types::Content>,
    /// True when the tool itself reported failure (still a *result*, not
    /// an exception — the model gets to see and recover from it).
    pub is_error: bool,
}

impl ToolOutput {
    /// A successful text result.
    pub fn ok(content: impl Into<String>) -> Self {
        Self::ok_blocks(vec![crate::types::Content::Text {
            text: content.into(),
        }])
    }

    /// A failed text result (tool-side error the model should see).
    pub fn err(content: impl Into<String>) -> Self {
        Self::err_blocks(vec![crate::types::Content::Text {
            text: content.into(),
        }])
    }

    /// A successful multi-block result (text and/or media).
    pub fn ok_blocks(content: Vec<crate::types::Content>) -> Self {
        Self {
            content,
            is_error: false,
        }
    }

    /// A failed multi-block result.
    pub fn err_blocks(content: Vec<crate::types::Content>) -> Self {
        Self {
            content,
            is_error: true,
        }
    }

    /// Text projection (media as placeholders) for events and probes.
    pub fn text(&self) -> String {
        crate::types::tool_result_text(&self.content)
    }
}

/// Tier of a tool the user explicitly put in the run: an `-e` component's
/// tool, or one named with `--tools`. The `--demo` script picks from the
/// lowest tier present.
pub const DEMO_USER_LOADED: u8 = 0;

/// Tier of a built-in the user explicitly named with `--tools`. Below
/// [`DEMO_USER_LOADED`] so an extension tool still wins the demo slot, but
/// only reachable by asking for the built-in by name — a default-on
/// built-in is never scripted (docs/builtin-tools.md).
pub const DEMO_USER_NAMED: u8 = 1;

/// An executable tool: native (built-in), wasm (tau-ext), or bridged
/// (e.g. an MCP server behind a bridge component).
#[async_trait]
pub trait Tool: Send + Sync {
    /// The definition advertised to the model.
    fn def(&self) -> ToolDef;
    /// Run one call with the model's arguments.
    async fn execute(&self, arguments: Json) -> ToolOutput;

    /// Where this tool sits in the `--demo` script's pick order, or `None`
    /// when the scripted model must never call it at all. The demo scripts
    /// exactly one call per run: the lowest tier present, and inside a tier
    /// the alphabetically first name.
    ///
    /// Default: [`DEMO_USER_LOADED`] — a tool the user put in the run by
    /// hand is fair game. Built-ins override this: read-only ones return
    /// [`DEMO_USER_NAMED`] only when `--tools` named them, and mutating
    /// ones (write/edit/bash/powershell) always return `None`, so the demo
    /// can never execute a shell or write a file (validate.sh 1e).
    fn demo_tier(&self) -> Option<u8> {
        Some(DEMO_USER_LOADED)
    }
}

/// The agent's tool set, keyed by name.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool; a same-named tool is replaced.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.def().name.clone(), tool);
    }

    /// Look a tool up by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// All definitions, sorted by name (deterministic model input).
    pub fn defs(&self) -> Vec<ToolDef> {
        let mut defs: Vec<_> = self.tools.values().map(|t| t.def()).collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// The tool `--demo` should script: the lowest [`Tool::demo_tier`],
    /// then the alphabetically first name — the same order [`Self::defs`]
    /// uses, so the pick is stable. `None` when no tool wants the slot
    /// (with built-ins on by default, that is the no-flag case: the demo
    /// answers plain text, exactly as before built-ins existed).
    pub fn demo_pick(&self) -> Option<String> {
        self.tools
            .values()
            .filter_map(|t| t.demo_tier().map(|tier| (tier, t.def().name)))
            .min()
            .map(|(_, name)| name)
    }

    /// True when no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub {
        name: &'static str,
        tier: Option<u8>,
    }

    #[async_trait]
    impl Tool for Stub {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: self.name.into(),
                description: "stub".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }
        }
        async fn execute(&self, _arguments: Json) -> ToolOutput {
            ToolOutput::ok("stub")
        }
        fn demo_tier(&self) -> Option<u8> {
            self.tier
        }
    }

    fn registry(tools: Vec<Stub>) -> ToolRegistry {
        let mut r = ToolRegistry::new();
        for t in tools {
            r.register(Box::new(t));
        }
        r
    }

    #[test]
    fn demo_pick_prefers_the_lowest_tier_over_the_first_name() {
        // "aaa" sorts first but is user-named (tier 1); "zzz" is a plain
        // user-loaded tool (tier 0) and must win the demo slot.
        let r = registry(vec![
            Stub { name: "aaa", tier: Some(DEMO_USER_NAMED) },
            Stub { name: "zzz", tier: Some(DEMO_USER_LOADED) },
        ]);
        assert_eq!(r.demo_pick().as_deref(), Some("zzz"));
    }

    #[test]
    fn demo_pick_breaks_ties_by_name() {
        let r = registry(vec![
            Stub { name: "bbb", tier: Some(DEMO_USER_LOADED) },
            Stub { name: "aaa", tier: Some(DEMO_USER_LOADED) },
        ]);
        assert_eq!(r.demo_pick().as_deref(), Some("aaa"));
    }

    #[test]
    fn a_none_tier_tool_is_invisible_to_the_demo() {
        // A bash-shaped built-in must never be scripted, even as the only
        // tool in the registry.
        let r = registry(vec![
            Stub { name: "bash", tier: None },
            Stub { name: "read", tier: Some(DEMO_USER_NAMED) },
        ]);
        assert_eq!(r.demo_pick().as_deref(), Some("read"));

        let only_mutating = registry(vec![Stub { name: "bash", tier: None }]);
        assert_eq!(only_mutating.demo_pick(), None);
    }

    #[test]
    fn an_empty_registry_scripts_nothing() {
        assert_eq!(ToolRegistry::new().demo_pick(), None);
    }
}
