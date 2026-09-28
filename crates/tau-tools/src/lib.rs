//! Built-in tools — `read`, `write`, `edit`, `ls`, `grep`, `find`, `bash`,
//! `powershell` — so tau can work on a repository without a wasm extension
//! installed first.
//!
//! They are ordinary [`tau_core::Tool`]s registered into the same
//! [`ToolRegistry`] that `-e` components fill; registration is last-wins, so
//! a component shipping its own `read` shadows the built-in (pi's behaviour
//! too).
//!
//! The built-ins are **host code** — they read, write, and spawn processes
//! with the permissions of the tau process. `--deny-wasi` does not apply to
//! them (docs/builtin-tools.md, docs/extensions.md §7).
//!
//! ```
//! use tau_core::ToolRegistry;
//! use tau_tools::BuiltinTools;
//!
//! let mut registry = ToolRegistry::new();
//! let registered = tau_tools::register(&mut registry, &BuiltinTools::all("."));
//! assert!(registered.contains(&"read".to_string()));
//! assert_eq!(registry.defs().len(), registered.len());
//! ```

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use tau_core::ToolRegistry;
use tau_core::tool::{DEMO_USER_NAMED, Tool};

pub mod truncate;

mod accumulate;
mod edit;
mod edit_match;
mod find;
mod grep;
mod ls;
mod mime;
mod paths;
mod read;
mod shell;
mod walk;
mod write;

pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use shell::{
    BASH_PATH_VAR, POWERSHELL_PATH_VAR, Shell, ShellKind, ShellTool, find_bash, find_powershell,
};
pub use write::WriteTool;

/// Every built-in this build implements, on this platform. The single source
/// of truth for [`names`] and [`register`]; a name here without a match arm
/// in [`build`] fails the tests.
const IMPLEMENTED: [&str; 8] = [
    "bash",
    "edit",
    "find",
    "grep",
    "ls",
    "powershell",
    "read",
    "write",
];

/// Built-ins whose output cannot change the user's machine: the only ones
/// `--demo` may script, and only when `--tools` named them.
const READ_ONLY: [&str; 4] = ["find", "grep", "ls", "read"];

/// Every built-in name this platform can register, sorted — the names
/// `--tools` accepts.
pub fn names() -> Vec<&'static str> {
    let mut all: Vec<&'static str> = IMPLEMENTED
        .iter()
        .copied()
        .filter(|name| cfg!(windows) || *name != "powershell")
        .collect();
    all.sort_unstable();
    all
}

/// Which built-ins to put in a run, and where they resolve paths from.
#[derive(Debug)]
pub struct BuiltinTools {
    cwd: PathBuf,
    /// `None` = every built-in; `Some(set)` = exactly those names.
    only: Option<BTreeSet<String>>,
}

impl BuiltinTools {
    /// Every built-in this platform has.
    pub fn all(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            only: None,
        }
    }

    /// No built-ins at all (`--no-builtin-tools`).
    pub fn none(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            only: Some(BTreeSet::new()),
        }
    }

    /// Exactly `names` (`--tools`), replacing the default set. An unknown
    /// name is an error rather than a silent skip — a typo must not leave
    /// the model with fewer tools than the user asked for.
    pub fn only(cwd: impl Into<PathBuf>, names: &[String]) -> Result<Self, UnknownTool> {
        let known = crate::names();
        for name in names {
            if !known.contains(&name.as_str()) {
                return Err(UnknownTool {
                    name: name.clone(),
                    known: known.iter().map(|n| (*n).to_string()).collect(),
                });
            }
        }
        Ok(Self::selecting(cwd, names))
    }

    /// Exactly `names`, unchecked. The CLI uses this because `--tools` names
    /// component tools as well (pi's flag picks from both), and those are
    /// only known once the components have loaded — so the caller checks the
    /// union itself, and a name that is no built-in registers nothing here.
    pub fn selecting(cwd: impl Into<PathBuf>, names: &[String]) -> Self {
        Self {
            cwd: cwd.into(),
            only: Some(names.iter().cloned().collect()),
        }
    }

    /// The directory relative paths resolve against (the session's working
    /// directory, captured once at startup).
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// True when `name` is part of this selection.
    pub fn selects(&self, name: &str) -> bool {
        match &self.only {
            Some(only) => only.contains(name),
            None => true,
        }
    }
}

/// A `--tools` name no built-in answers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownTool {
    /// The name as the user typed it.
    pub name: String,
    /// The names that would have worked, sorted.
    pub known: Vec<String>,
}

impl fmt::Display for UnknownTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown tool: {} (known: {})",
            self.name,
            self.known.join(", ")
        )
    }
}

impl std::error::Error for UnknownTool {}

/// The `--demo` tier a built-in carries (see [`Tool::demo_tier`]).
///
/// A default-on built-in is never scripted, so `tau --demo -p …` stays the
/// plain-text transcript it always was; a read-only built-in the user named
/// with `--tools` may be scripted, and the mutating four never are — even by
/// name, because `--tools bash --demo` must not run a shell (validate 1e).
fn tier(name: &str, named: bool) -> Option<u8> {
    (named && READ_ONLY.contains(&name)).then_some(DEMO_USER_NAMED)
}

/// Build one built-in, or `None` when this build has no such tool (or the
/// platform has no such tool).
fn build(name: &str, cwd: &Path, tier: Option<u8>) -> Option<Box<dyn Tool>> {
    match name {
        "bash" => Some(Box::new(shell::ShellTool::bash(cwd, tier))),
        "edit" => Some(Box::new(edit::EditTool::new(cwd, tier))),
        "find" => Some(Box::new(find::FindTool::new(cwd, tier))),
        "grep" => Some(Box::new(grep::GrepTool::new(cwd, tier))),
        "ls" => Some(Box::new(ls::LsTool::new(cwd, tier))),
        "powershell" => Some(Box::new(shell::ShellTool::powershell(cwd, tier))),
        "read" => Some(Box::new(read::ReadTool::new(cwd, tier))),
        "write" => Some(Box::new(write::WriteTool::new(cwd, tier))),
        _ => None,
    }
}

/// Register the selected built-ins into `registry`, returning the names
/// registered, sorted — the startup line prints them, and validate.sh
/// asserts on the line.
pub fn register(registry: &mut ToolRegistry, tools: &BuiltinTools) -> Vec<String> {
    let mut registered = Vec::new();
    for name in names() {
        if !tools.selects(name) {
            continue;
        }
        let Some(tool) = build(name, &tools.cwd, tier(name, tools.only.is_some())) else {
            continue;
        };
        registered.push(tool.def().name.clone());
        registry.register(tool);
    }
    registered.sort();
    registered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sorted_and_platform_shaped() {
        let names = names();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert!(names.contains(&"read"));
        assert!(names.contains(&"ls"));
        #[cfg(not(windows))]
        assert!(!names.contains(&"powershell"));
    }

    #[test]
    fn every_advertised_name_builds() {
        // The drift guard: `names()` is what `--tools` accepts and what the
        // startup line prints, so every name must have an implementation.
        for name in names() {
            let tool = build(name, Path::new("."), None)
                .unwrap_or_else(|| panic!("{name} is advertised but not built"));
            assert_eq!(tool.def().name, name);
        }
    }

    #[test]
    fn the_default_registers_every_built_in() {
        let mut registry = ToolRegistry::new();
        let registered = register(&mut registry, &BuiltinTools::all("."));
        assert_eq!(registered, names());
        let defs: Vec<String> = registry.defs().into_iter().map(|d| d.name).collect();
        assert_eq!(defs, names());
    }

    #[test]
    fn none_registers_nothing() {
        let mut registry = ToolRegistry::new();
        let registered = register(&mut registry, &BuiltinTools::none("."));
        assert!(registered.is_empty());
        assert!(registry.is_empty());
    }

    #[test]
    fn only_registers_exactly_the_named_tools() {
        let mut registry = ToolRegistry::new();
        let only = BuiltinTools::only(".", &["read".to_string()]).expect("known name");
        assert_eq!(register(&mut registry, &only), vec!["read".to_string()]);
        assert!(registry.get("ls").is_none());
        assert!(registry.get("read").is_some());
    }

    #[test]
    fn only_rejects_an_unknown_name_and_lists_the_known_ones() {
        let error = BuiltinTools::only(".", &["read".to_string(), "nope".to_string()])
            .expect_err("unknown name");
        assert_eq!(error.name, "nope");
        assert_eq!(
            error.known,
            names().iter().map(|n| (*n).to_string()).collect::<Vec<_>>()
        );
        assert_eq!(
            error.to_string(),
            format!("unknown tool: nope (known: {})", names().join(", "))
        );
    }

    #[test]
    fn only_reports_the_first_unknown_name() {
        let error = BuiltinTools::only(".", &["a".to_string(), "b".to_string()])
            .expect_err("unknown names");
        assert_eq!(error.name, "a");
    }

    #[test]
    fn the_default_set_is_never_scripted_by_the_demo() {
        // The `--demo` transcript of a plain run must not change just
        // because built-ins are now on by default.
        let mut registry = ToolRegistry::new();
        register(&mut registry, &BuiltinTools::all("."));
        assert_eq!(registry.demo_pick(), None);
    }

    #[test]
    fn a_named_read_only_builtin_is_scriptable() {
        let mut registry = ToolRegistry::new();
        let only = BuiltinTools::only(".", &["read".to_string()]).expect("known name");
        register(&mut registry, &only);
        assert_eq!(registry.demo_pick().as_deref(), Some("read"));
    }

    #[test]
    fn the_mutating_four_are_never_scripted_even_when_named() {
        // `--tools bash --demo` must not run a shell, however the user asks
        // (validate.sh 1e).
        for name in ["bash", "edit", "powershell", "write"] {
            assert_eq!(tier(name, true), None, "{name} must not be scriptable");
            assert_eq!(tier(name, false), None, "{name} must not be scriptable");
        }
        for name in READ_ONLY {
            assert_eq!(
                tier(name, false),
                None,
                "a default-on {name} is not scripted"
            );
            assert_eq!(tier(name, true), Some(DEMO_USER_NAMED));
        }
    }

    #[test]
    fn the_cwd_is_where_paths_resolve() {
        let tools = BuiltinTools::all("/work/project");
        assert_eq!(tools.cwd(), Path::new("/work/project"));
    }
}
