//! `ls` — a port of pi's `core/tools/ls.ts`.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value as Json;
use tau_core::tool::{Tool, ToolDef, ToolOutput};

use crate::paths;
use crate::truncate::{DEFAULT_MAX_BYTES, TruncationOptions, format_size, truncate_head};

/// Entries returned before the limit notice (pi's `DEFAULT_LIMIT`).
pub const DEFAULT_LIMIT: usize = 500;

/// List one directory.
pub struct LsTool {
    cwd: PathBuf,
    tier: Option<u8>,
}

impl LsTool {
    /// A tool listing relative to `cwd`, with the `--demo` tier it carries.
    pub fn new(cwd: impl Into<PathBuf>, tier: Option<u8>) -> Self {
        Self {
            cwd: cwd.into(),
            tier,
        }
    }
}

#[async_trait]
impl Tool for LsTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "ls".into(),
            description: format!(
                "List directory contents. Returns entries sorted alphabetically, with '/' \
                 suffix for directories. Includes dotfiles. Output is truncated to \
                 {DEFAULT_LIMIT} entries or {}KB (whichever is hit first).",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to list (default: current directory)"
                    },
                    "limit": {
                        "type": "number",
                        "description": format!("Maximum number of entries to return (default: {DEFAULT_LIMIT})")
                    }
                }
            }),
        }
    }

    fn demo_tier(&self) -> Option<u8> {
        self.tier
    }

    async fn execute(&self, arguments: Json) -> ToolOutput {
        let raw_path = arguments.get("path").and_then(Json::as_str).unwrap_or(".");
        let limit = arguments
            .get("limit")
            .and_then(Json::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_LIMIT);

        let dir = paths::resolve(&self.cwd, raw_path);
        // pi checks existence, then type, with distinct messages.
        if !dir.exists() {
            return ToolOutput::err(format!("Path not found: {}", dir.display()));
        }
        let Ok(meta) = std::fs::metadata(&dir) else {
            return ToolOutput::err(format!("Cannot read directory: {}", dir.display()));
        };
        if !meta.is_dir() {
            return ToolOutput::err(format!("Not a directory: {}", dir.display()));
        }

        let Ok(read) = std::fs::read_dir(&dir) else {
            return ToolOutput::err(format!("Cannot read directory: {}", dir.display()));
        };
        let mut names: Vec<String> = Vec::new();
        for entry in read.filter_map(Result::ok) {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        // Case-insensitive, then case-sensitive — the order pi sorts in.
        names.sort_by(|a, b| {
            a.to_lowercase()
                .cmp(&b.to_lowercase())
                .then_with(|| a.cmp(b))
        });

        let mut results: Vec<String> = Vec::new();
        let mut entry_limit_reached = false;
        for name in &names {
            if results.len() >= limit {
                entry_limit_reached = true;
                break;
            }
            // Resolves symlinks, so a link to a directory carries the `/`
            // suffix; an entry that cannot be stat-ed is skipped.
            let Ok(meta) = std::fs::metadata(dir.join(name)) else {
                continue;
            };
            results.push(if meta.is_dir() {
                format!("{name}/")
            } else {
                name.clone()
            });
        }

        if results.is_empty() {
            return ToolOutput::ok("(empty directory)");
        }

        let raw_output = results.join("\n");
        // One line per entry, so only the byte limit can bite here.
        let truncation = truncate_head(
            &raw_output,
            TruncationOptions {
                max_lines: usize::MAX,
                max_bytes: DEFAULT_MAX_BYTES,
            },
        );
        let mut output = truncation.content;
        let mut notices: Vec<String> = Vec::new();
        if entry_limit_reached {
            notices.push(format!(
                "{limit} entries limit reached. Use limit={} for more",
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
        LsTool::new(dir, None).execute(args).await
    }

    fn text(out: &ToolOutput) -> String {
        out.text()
    }

    #[tokio::test]
    async fn entries_sort_case_insensitively_and_directories_get_a_slash() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("README.md"), "hi").unwrap();
        std::fs::write(root.join("zeta.txt"), "z").unwrap();
        std::fs::write(root.join(".gitignore"), "target").unwrap();
        std::fs::create_dir(root.join("Beta")).unwrap();

        let out = run(root, serde_json::json!({})).await;
        assert!(!out.is_error, "{}", text(&out));
        assert_eq!(text(&out), ".gitignore\nBeta/\nREADME.md\nsrc/\nzeta.txt");
    }

    #[tokio::test]
    async fn an_empty_directory_says_so() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = run(tmp.path(), serde_json::json!({})).await;
        assert_eq!(text(&out), "(empty directory)");
    }

    #[tokio::test]
    async fn a_path_can_be_relative_and_unicode_spaces_are_folded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sub = tmp.path().join("my dir");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), "a").unwrap();

        // NBSP instead of the space, as a paste out of a document carries.
        let out = run(tmp.path(), serde_json::json!({ "path": "my\u{a0}dir" })).await;
        assert_eq!(text(&out), "a.txt");
    }

    #[tokio::test]
    async fn at_resolves_against_the_cwd_and_tilde_against_home() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let probe = "tau-tools-tilde-probe.txt";
        std::fs::write(tmp.path().join(probe), "a").unwrap();

        let at = run(tmp.path(), serde_json::json!({ "path": "@." })).await;
        assert_eq!(text(&at), probe);

        // `~` is the real home directory, so the cwd's probe must not be in
        // its listing.
        let tilde = run(tmp.path(), serde_json::json!({ "path": "~" })).await;
        assert!(!tilde.is_error, "{}", text(&tilde));
        assert!(
            !text(&tilde)
                .lines()
                .any(|line| line == probe || line == format!("{probe}/")),
            "~ listed the cwd instead of the home directory"
        );
    }

    #[tokio::test]
    async fn missing_and_non_directory_paths_report_pis_messages() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.txt"), "a").unwrap();

        let missing = run(tmp.path(), serde_json::json!({ "path": "nope" })).await;
        assert!(missing.is_error);
        assert!(
            text(&missing).starts_with("Path not found: "),
            "{}",
            text(&missing)
        );

        let not_dir = run(tmp.path(), serde_json::json!({ "path": "a.txt" })).await;
        assert!(not_dir.is_error);
        assert!(
            text(&not_dir).starts_with("Not a directory: "),
            "{}",
            text(&not_dir)
        );
    }

    #[tokio::test]
    async fn the_entry_limit_notices_the_doubled_suggestion() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for i in 0..5 {
            std::fs::write(tmp.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        let out = run(tmp.path(), serde_json::json!({ "limit": 3 })).await;
        assert_eq!(
            text(&out),
            "f0.txt\nf1.txt\nf2.txt\n\n[3 entries limit reached. Use limit=6 for more]"
        );
    }

    #[tokio::test]
    async fn the_byte_limit_appends_a_size_notice() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // 400 entries of 155 bytes each: past 50KB (at ~339 entries) with
        // the 500-entry limit not reached, and each name short enough for
        // the filesystem.
        for i in 0..400 {
            let name = format!("{i:03}-{}.bin", "x".repeat(150));
            std::fs::write(tmp.path().join(name), "x").unwrap();
        }
        let out = run(tmp.path(), serde_json::json!({})).await;
        let body = text(&out);
        assert!(body.contains("\n\n["), "a notice starts a new paragraph");
        assert!(
            body.ends_with("50.0KB limit reached]"),
            "{}",
            &body[body.len().saturating_sub(80)..]
        );
    }

    #[tokio::test]
    async fn a_symlink_to_a_directory_carries_the_suffix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(tmp.path().join("real")).unwrap();
        std::fs::write(tmp.path().join("file.txt"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();
        #[cfg(windows)]
        {
            // Creating a symlink needs a privilege this test may not have.
            if std::os::windows::fs::symlink_dir(tmp.path().join("real"), tmp.path().join("link"))
                .is_err()
            {
                return;
            }
        }
        let out = run(tmp.path(), serde_json::json!({})).await;
        assert!(text(&out).contains("link/"), "{}", text(&out));
    }
}
