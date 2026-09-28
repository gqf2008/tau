//! `find` — pi's `core/tools/find.ts`, natively.
//!
//! pi shells out to fd; tau walks with the `ignore` crate instead (see
//! `walk.rs`), so the tool works with nothing installed. The schema, the
//! description, the relative-path output and the notices are pi's.
//!
//! One deliberate difference from pi: fd matches a path-shaped pattern
//! against the *absolute* candidate, which is why pi prepends `**/`. Here the
//! candidate is already relative to the search root, so `src/**/*.spec.ts`
//! matches without rewriting the pattern.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value as Json;
use tau_core::tool::{Tool, ToolDef, ToolOutput};

use crate::paths;
use crate::truncate::{DEFAULT_MAX_BYTES, TruncationOptions, format_size, truncate_head};
use crate::walk::FileFilter;

/// Results returned before the limit notice (pi's `DEFAULT_LIMIT`).
pub const DEFAULT_LIMIT: usize = 1000;

/// Search for files by glob pattern.
pub struct FindTool {
    cwd: PathBuf,
    tier: Option<u8>,
}

impl FindTool {
    /// A tool searching relative to `cwd`, with the `--demo` tier it carries.
    pub fn new(cwd: impl Into<PathBuf>, tier: Option<u8>) -> Self {
        Self {
            cwd: cwd.into(),
            tier,
        }
    }
}

#[async_trait]
impl Tool for FindTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "find".into(),
            description: format!(
                "Search for files by glob pattern. Returns matching file paths relative to the \
                 search directory. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} \
                 results or {}KB (whichever is hit first).",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in (default: current directory)"
                    },
                    "limit": {
                        "type": "number",
                        "description": format!("Maximum number of results (default: {DEFAULT_LIMIT})")
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn demo_tier(&self) -> Option<u8> {
        self.tier
    }

    async fn execute(&self, arguments: Json) -> ToolOutput {
        let Some(pattern) = arguments.get("pattern").and_then(Json::as_str) else {
            return ToolOutput::err("find requires a pattern");
        };
        let search_path = paths::resolve(
            &self.cwd,
            arguments.get("path").and_then(Json::as_str).unwrap_or("."),
        );
        if !search_path.exists() {
            return ToolOutput::err(format!("Path not found: {}", search_path.display()));
        }
        let limit = arguments
            .get("limit")
            .and_then(Json::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_LIMIT);
        let filter = match FileFilter::new(pattern) {
            Ok(filter) => filter,
            Err(error) => return ToolOutput::err(format!("Invalid glob pattern: {error}")),
        };

        let mut results: Vec<String> = Vec::new();
        let mut result_limit_reached = false;
        let mut consider = |path: &Path, is_dir: bool| -> bool {
            if !filter.matches(&search_path, path) {
                return false;
            }
            let mut name = if path == search_path {
                // The search path was a single file: report its own name
                // (pi's fd path prints nothing at all here — an empty line).
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                crate::walk::relative(&search_path, path)
            };
            if is_dir {
                name.push('/');
            }
            results.push(name);
            if results.len() >= limit {
                result_limit_reached = true;
                return true;
            }
            false
        };

        if search_path.is_dir() {
            // Skip the root itself: fd does not report the search directory.
            for (path, is_dir) in crate::walk::all_entries(&search_path).skip(1) {
                if consider(&path, is_dir) {
                    break;
                }
            }
        } else {
            consider(&search_path, false);
        }

        if results.is_empty() {
            return ToolOutput::ok("No files found matching pattern");
        }

        let raw_output = results.join("\n");
        // One line per result, so only the byte limit can bite.
        let truncation = truncate_head(
            &raw_output,
            TruncationOptions {
                max_lines: usize::MAX,
                max_bytes: DEFAULT_MAX_BYTES,
            },
        );
        let mut output = truncation.content;
        let mut notices: Vec<String> = Vec::new();
        if result_limit_reached {
            notices.push(format!(
                "{limit} results limit reached. Use limit={} for more, or refine pattern",
                limit * 2
            ));
        }
        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }
        ToolOutput::ok(output)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    async fn run(dir: &Path, args: Json) -> ToolOutput {
        FindTool::new(dir, None).execute(args).await
    }

    fn tree(dir: &Path) {
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join("src/nested/spec.rs"), "").unwrap();
        std::fs::write(dir.join("README.md"), "").unwrap();
        std::fs::write(dir.join("skipped.rs"), "").unwrap();
        std::fs::write(dir.join(".gitignore"), "skipped.rs\n").unwrap();
        std::fs::write(dir.join(".hidden.rs"), "").unwrap();
    }

    fn sorted(body: &str) -> Vec<String> {
        let mut lines: Vec<String> = body
            .lines()
            .take_while(|line| !line.starts_with('['))
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        lines.sort();
        lines
    }

    #[tokio::test]
    async fn a_bare_pattern_matches_names_anywhere_in_the_tree() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let out = run(tmp.path(), serde_json::json!({ "pattern": "*.rs" })).await;
        assert!(!out.is_error, "{}", out.text());
        assert_eq!(
            sorted(&out.text()),
            vec![".hidden.rs", "src/main.rs", "src/nested/spec.rs"]
        );
    }

    #[tokio::test]
    async fn a_path_pattern_matches_relative_to_the_search_root() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let out = run(
            tmp.path(),
            serde_json::json!({ "pattern": "src/nested/*.rs" }),
        )
        .await;
        assert_eq!(out.text(), "src/nested/spec.rs");

        let glob_star = run(tmp.path(), serde_json::json!({ "pattern": "src/**/*.rs" })).await;
        assert_eq!(
            sorted(&glob_star.text()),
            vec!["src/main.rs", "src/nested/spec.rs"]
        );
    }

    #[tokio::test]
    async fn the_path_argument_narrows_the_search_and_the_output() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let out = run(
            tmp.path(),
            serde_json::json!({ "pattern": "*.rs", "path": "src" }),
        )
        .await;
        // Relative to the search root, not to the cwd.
        assert_eq!(sorted(&out.text()), vec!["main.rs", "nested/spec.rs"]);
    }

    #[tokio::test]
    async fn directories_match_and_carry_a_trailing_slash() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let out = run(tmp.path(), serde_json::json!({ "pattern": "nested" })).await;
        assert_eq!(out.text(), "src/nested/");
    }

    #[tokio::test]
    async fn no_results_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let out = run(tmp.path(), serde_json::json!({ "pattern": "*.zzz" })).await;
        assert_eq!(out.text(), "No files found matching pattern");
    }

    #[tokio::test]
    async fn gitignore_is_respected_and_hidden_files_are_not() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let out = run(tmp.path(), serde_json::json!({ "pattern": "*" })).await;
        let body = out.text();
        // Hidden files are searched (pi passes --hidden), which includes the
        // ignore file itself...
        assert!(body.contains(".hidden.rs"), "{body}");
        assert!(body.contains(".gitignore"), "{body}");
        // ...while a gitignored file is not.
        assert!(!body.contains("skipped.rs"), "{body}");
    }

    #[tokio::test]
    async fn the_result_limit_notices_the_doubled_suggestion() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(tmp.path().join(format!("f{i}.txt")), "").unwrap();
        }
        let out = run(
            tmp.path(),
            serde_json::json!({ "pattern": "*.txt", "limit": 3 }),
        )
        .await;
        assert!(
            out.text()
                .ends_with("[3 results limit reached. Use limit=6 for more, or refine pattern]"),
            "{}",
            out.text()
        );
        assert_eq!(sorted(&out.text()).len(), 3);
    }

    #[tokio::test]
    async fn a_single_file_search_reports_that_file() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let out = run(
            tmp.path(),
            serde_json::json!({ "pattern": "main.rs", "path": "src/main.rs" }),
        )
        .await;
        assert_eq!(out.text(), "main.rs");
    }

    #[tokio::test]
    async fn a_missing_path_reports_the_message() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run(
            tmp.path(),
            serde_json::json!({ "pattern": "*", "path": "nope" }),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text().starts_with("Path not found: "), "{}", out.text());
    }

    #[tokio::test]
    async fn an_invalid_glob_is_an_error_result() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let out = run(tmp.path(), serde_json::json!({ "pattern": "[" })).await;
        assert!(out.is_error);
        assert!(
            out.text().starts_with("Invalid glob pattern: "),
            "{}",
            out.text()
        );
    }

    #[tokio::test]
    async fn a_missing_pattern_is_an_error_result() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run(tmp.path(), serde_json::json!({})).await;
        assert!(out.is_error);
        assert_eq!(out.text(), "find requires a pattern");
    }
}
