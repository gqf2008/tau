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
    /// Textual content; the model reads it as the tool result block.
    pub content: String,
    /// True when the tool itself reported failure (still a *result*, not
    /// an exception — the model gets to see and recover from it).
    pub is_error: bool,
}

impl ToolOutput {
    /// A successful result.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    /// A failed result (tool-side error the model should see).
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// An executable tool: native (built-in), wasm (tau-ext), or bridged
/// (e.g. an MCP server behind a bridge component).
#[async_trait]
pub trait Tool: Send + Sync {
    /// The definition advertised to the model.
    fn def(&self) -> ToolDef;
    /// Run one call with the model's arguments.
    async fn execute(&self, arguments: Json) -> ToolOutput;
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

    /// True when no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
