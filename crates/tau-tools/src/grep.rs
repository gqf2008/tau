//! `grep` — pi's `core/tools/grep.ts`, natively.
//!
//! pi shells out to ripgrep (`rg --json`); tau must not require an external
//! binary for a tool that is on by default, and ripgrep's engine *is* the
//! `regex` crate, so the two are semantically the same family. What is
//! mirrored from pi is everything the model sees: the schema, the description,
//! the `path:line: text` output shape with `-` separators for context lines,
//! the match/byte/line notices, and the truncation limits.

use std::path::PathBuf;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value as Json;
use tau_core::tool::{Tool, ToolDef, ToolOutput};

use crate::paths;
use crate::truncate::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, TruncationOptions, format_size, truncate_head,
    truncate_line,
};
use crate::walk::FileFilter;

/// Matches returned before the limit notice (pi's `DEFAULT_LIMIT`).
pub const DEFAULT_LIMIT: usize = 100;

/// How far into a file to look for a NUL byte before calling it binary.
/// ripgrep skips binary files by default; so does this.
const BINARY_SNIFF_BYTES: usize = 8192;

/// Search file contents for a regex or literal pattern.
pub struct GrepTool {
    cwd: PathBuf,
    tier: Option<u8>,
}

impl GrepTool {
    /// A tool searching relative to `cwd`, with the `--demo` tier it carries.
    pub fn new(cwd: impl Into<PathBuf>, tier: Option<u8>) -> Self {
        Self {
            cwd: cwd.into(),
            tier,
        }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "grep".into(),
            description: format!(
                "Search file contents for a pattern. Returns matching lines with file paths and \
                 line numbers. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} matches \
                 or {}KB (whichever is hit first). Long lines are truncated to \
                 {GREP_MAX_LINE_LENGTH} chars.",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Search pattern (regex or literal string)"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory or file to search (default: current directory)"
                    },
                    "glob": {
                        "type": "string",
                        "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"
                    },
                    "ignoreCase": {
                        "type": "boolean",
                        "description": "Case-insensitive search (default: false)"
                    },
                    "literal": {
                        "type": "boolean",
                        "description": "Treat pattern as literal string instead of regex (default: false)"
                    },
                    "context": {
                        "type": "number",
                        "description": "Number of lines to show before and after each match (default: 0)"
                    },
                    "limit": {
                        "type": "number",
                        "description": format!("Maximum number of matches to return (default: {DEFAULT_LIMIT})")
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
            return ToolOutput::err("grep requires a pattern");
        };
        let search_path = paths::resolve(
            &self.cwd,
            arguments.get("path").and_then(Json::as_str).unwrap_or("."),
        );
        if !search_path.exists() {
            return ToolOutput::err(format!("Path not found: {}", search_path.display()));
        }
        let is_directory = search_path.is_dir();

        let ignore_case = arguments
            .get("ignoreCase")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let literal = arguments
            .get("literal")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let context = arguments
            .get("context")
            .and_then(Json::as_u64)
            .filter(|n| *n > 0)
            .unwrap_or(0) as usize;
        let limit = arguments
            .get("limit")
            .and_then(Json::as_u64)
            .map(|n| n.max(1) as usize)
            .unwrap_or(DEFAULT_LIMIT);

        // ripgrep reports a bad pattern on stderr and exits 2; the model sees
        // the message either way.
        let source = if literal {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };
        let source = if ignore_case {
            format!("(?i){source}")
        } else {
            source
        };
        let regex = match Regex::new(&source) {
            Ok(regex) => regex,
            Err(error) => return ToolOutput::err(format!("Invalid pattern: {error}")),
        };

        let glob = match arguments.get("glob").and_then(Json::as_str) {
            Some(pattern) => match FileFilter::new(pattern) {
                Ok(filter) => Some(filter),
                Err(error) => return ToolOutput::err(format!("Invalid glob pattern: {error}")),
            },
            None => None,
        };

        // One file, or a whole tree walked the way ripgrep walks it:
        // .gitignore respected, hidden files included, `.git` skipped. The
        // walk stays lazy — the loop below stops at the match limit.
        let candidates: Box<dyn Iterator<Item = PathBuf> + '_> = if is_directory {
            Box::new(crate::walk::files(&search_path))
        } else {
            Box::new(std::iter::once(search_path.clone()))
        };

        let mut lines_truncated = false;
        let mut match_limit_reached = false;
        let mut matches = 0usize;
        let mut output_lines: Vec<String> = Vec::new();

        'files: for file in candidates {
            if let Some(filter) = &glob
                && !filter.matches(&search_path, &file)
            {
                continue;
            }
            let Ok(bytes) = std::fs::read(&file) else {
                continue;
            };
            let window = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
            if window.contains(&0) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);

            // The same split pi makes: `\n`-separated, so a file ending in a
            // newline has a trailing empty entry to draw context from.
            let lines: Vec<&str> = text.split('\n').collect();
            for (index, line) in lines.iter().enumerate() {
                let line_text = line.strip_suffix('\r').unwrap_or(line);
                if !regex.is_match(line_text) {
                    continue;
                }
                matches += 1;

                // The path as the model should see it: relative to the search
                // root when searching a directory, the bare name for one file.
                let name = if is_directory {
                    crate::walk::relative(&search_path, &file)
                } else {
                    file.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| file.to_string_lossy().into_owned())
                };

                if context == 0 {
                    let (text, was_truncated) = truncate_line(line_text, GREP_MAX_LINE_LENGTH);
                    lines_truncated |= was_truncated;
                    output_lines.push(format!("{name}:{}: {text}", index + 1));
                } else {
                    let first = (index + 1).saturating_sub(context).max(1);
                    let last = (index + 1 + context).min(lines.len());
                    for current in first..=last {
                        let raw = lines.get(current - 1).copied().unwrap_or("");
                        let raw = raw.strip_suffix('\r').unwrap_or(raw);
                        let (text, was_truncated) = truncate_line(raw, GREP_MAX_LINE_LENGTH);
                        lines_truncated |= was_truncated;
                        if current == index + 1 {
                            output_lines.push(format!("{name}:{current}: {text}"));
                        } else {
                            output_lines.push(format!("{name}-{current}- {text}"));
                        }
                    }
                }

                if matches >= limit {
                    match_limit_reached = true;
                    break 'files;
                }
            }
        }

        if matches == 0 {
            return ToolOutput::ok("No matches found");
        }

        let raw_output = output_lines.join("\n");
        // One line per match, so only the byte limit can bite.
        let truncation = truncate_head(
            &raw_output,
            TruncationOptions {
                max_lines: usize::MAX,
                max_bytes: DEFAULT_MAX_BYTES,
            },
        );
        let mut output = truncation.content;
        let mut notices: Vec<String> = Vec::new();
        if match_limit_reached {
            notices.push(format!(
                "{limit} matches limit reached. Use limit={} for more, or refine pattern",
                limit * 2
            ));
        }
        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        }
        if lines_truncated {
            notices.push(format!(
                "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
            ));
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
        GrepTool::new(dir, None).execute(args).await
    }

    fn tree(dir: &Path) {
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(
            dir.join("src/nested/lib.rs"),
            "pub fn helper() {\n    // nothing here\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("README.md"), "the main idea\n").unwrap();
        std::fs::write(dir.join("ignored.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.join(".hidden.rs"), "fn main() {}\n").unwrap();
    }

    #[tokio::test]
    async fn matches_print_path_line_and_text() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let out = run(tmp.path(), serde_json::json!({ "pattern": "fn main" })).await;
        let body = out.text();
        let mut lines: Vec<&str> = body.lines().collect();
        lines.sort_unstable();
        // Relative to the search root, posix separators, line numbers:
        // .hidden.rs is searched (pi passes --hidden), ignored.rs is not.
        assert_eq!(
            lines,
            vec![".hidden.rs:1: fn main() {}", "src/main.rs:1: fn main() {}"]
        );
    }

    #[tokio::test]
    async fn a_file_search_prints_the_bare_name() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let out = run(
            tmp.path(),
            serde_json::json!({ "pattern": "helper", "path": "src/nested/lib.rs" }),
        )
        .await;
        assert_eq!(out.text(), "lib.rs:1: pub fn helper() {");
    }

    #[tokio::test]
    async fn no_matches_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let out = run(
            tmp.path(),
            serde_json::json!({ "pattern": "nothing-matches" }),
        )
        .await;
        assert_eq!(out.text(), "No matches found");
    }

    #[tokio::test]
    async fn context_lines_use_dash_separators() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let out = run(
            tmp.path(),
            serde_json::json!({ "pattern": "three", "context": 1 }),
        )
        .await;
        assert_eq!(out.text(), "a.txt-2- two\na.txt:3: three\na.txt-4- four");
    }

    #[tokio::test]
    async fn context_at_the_end_of_a_file_reaches_the_trailing_empty_line() {
        // pi splits on '\n', so the phantom line after a trailing newline is
        // context like any other. Mirrored on purpose.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "one\ntwo\n").unwrap();
        let out = run(
            tmp.path(),
            serde_json::json!({ "pattern": "two", "context": 1 }),
        )
        .await;
        assert_eq!(out.text(), "a.txt-1- one\na.txt:2: two\na.txt-3- ");
    }

    #[tokio::test]
    async fn literal_mode_escapes_regex_metacharacters() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "a.b()\naxb\n").unwrap();
        let literal = run(
            tmp.path(),
            serde_json::json!({ "pattern": "a.b()", "literal": true }),
        )
        .await;
        assert_eq!(literal.text(), "a.txt:1: a.b()");

        let regex = run(tmp.path(), serde_json::json!({ "pattern": "a.b" })).await;
        assert_eq!(regex.text(), "a.txt:1: a.b()\na.txt:2: axb");
    }

    #[tokio::test]
    async fn ignore_case_widens_the_search() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "Main\n").unwrap();
        let sensitive = run(tmp.path(), serde_json::json!({ "pattern": "main" })).await;
        assert_eq!(sensitive.text(), "No matches found");
        let insensitive = run(
            tmp.path(),
            serde_json::json!({ "pattern": "main", "ignoreCase": true }),
        )
        .await;
        assert_eq!(insensitive.text(), "a.txt:1: Main");
    }

    #[tokio::test]
    async fn a_glob_filters_the_files_searched() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());

        let markdown = run(
            tmp.path(),
            serde_json::json!({ "pattern": "fn main", "glob": "**/*.md" }),
        )
        .await;
        assert_eq!(markdown.text(), "No matches found");

        // A bare glob matches file names, hidden ones included (--hidden).
        let bare = run(
            tmp.path(),
            serde_json::json!({ "pattern": "fn main", "glob": "*.rs" }),
        )
        .await;
        let rendered = bare.text();
        let mut lines: Vec<&str> = rendered.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            vec![".hidden.rs:1: fn main() {}", "src/main.rs:1: fn main() {}"]
        );

        // A glob with a separator matches the path relative to the search
        // root, and `*` does not cross a `/`.
        let scoped = run(
            tmp.path(),
            serde_json::json!({ "pattern": "fn main", "glob": "src/*.rs" }),
        )
        .await;
        assert_eq!(scoped.text(), "src/main.rs:1: fn main() {}");
    }

    #[tokio::test]
    async fn the_match_limit_notices_the_doubled_suggestion() {
        let tmp = tempfile::tempdir().unwrap();
        let body = (0..10)
            .map(|i| format!("hit {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("a.txt"), body).unwrap();
        let out = run(
            tmp.path(),
            serde_json::json!({ "pattern": "hit", "limit": 3 }),
        )
        .await;
        assert!(
            out.text()
                .ends_with("[3 matches limit reached. Use limit=6 for more, or refine pattern]"),
            "{}",
            out.text()
        );
        assert_eq!(out.text().lines().count(), 3 + 2); // three hits, blank, notice
    }

    #[tokio::test]
    async fn long_lines_are_cut_with_a_marker_and_noticed() {
        let tmp = tempfile::tempdir().unwrap();
        let long = format!("start {} end", "x".repeat(600));
        std::fs::write(tmp.path().join("a.txt"), format!("{long}\n")).unwrap();
        let out = run(tmp.path(), serde_json::json!({ "pattern": "start" })).await;
        let body = out.text();
        assert!(body.contains("... [truncated]"), "{body}");
        assert!(
            body.ends_with("[Some lines truncated to 500 chars. Use read tool to see full lines]"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_missing_path_reports_pis_message() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run(
            tmp.path(),
            serde_json::json!({ "pattern": "x", "path": "nope" }),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text().starts_with("Path not found: "), "{}", out.text());
    }

    #[tokio::test]
    async fn a_bad_pattern_is_an_error_result() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let out = run(tmp.path(), serde_json::json!({ "pattern": "a(" })).await;
        assert!(out.is_error);
        assert!(
            out.text().starts_with("Invalid pattern: "),
            "{}",
            out.text()
        );
    }

    #[tokio::test]
    async fn a_bad_glob_is_an_error_result() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let out = run(
            tmp.path(),
            serde_json::json!({ "pattern": "fn", "glob": "[" }),
        )
        .await;
        assert!(out.is_error);
        assert!(
            out.text().starts_with("Invalid glob pattern: "),
            "{}",
            out.text()
        );
    }

    #[tokio::test]
    async fn binary_files_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("bin.dat"), b"hit\x00hit\n").unwrap();
        let out = run(tmp.path(), serde_json::json!({ "pattern": "hit" })).await;
        assert_eq!(out.text(), "No matches found");
    }

    #[tokio::test]
    async fn crlf_files_match_without_the_carriage_return() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "one\r\ntwo\r\n").unwrap();
        let out = run(tmp.path(), serde_json::json!({ "pattern": "^two$" })).await;
        assert_eq!(out.text(), "a.txt:2: two");
    }

    #[tokio::test]
    async fn the_git_directory_is_not_searched() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".git/config"), "hit\n").unwrap();
        let out = run(tmp.path(), serde_json::json!({ "pattern": "hit" })).await;
        assert_eq!(out.text(), "No matches found");
    }

    #[test]
    fn the_limits_are_pis() {
        assert_eq!(DEFAULT_LIMIT, 100);
        assert_eq!(GREP_MAX_LINE_LENGTH, 500);
    }
}
