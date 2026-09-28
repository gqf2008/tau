//! `write` — a port of pi's `core/tools/write.ts`.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value as Json;
use tau_core::tool::{Tool, ToolDef, ToolOutput};

use crate::paths;

/// Create or overwrite one file.
pub struct WriteTool {
    cwd: PathBuf,
    tier: Option<u8>,
}

impl WriteTool {
    /// A tool writing relative to `cwd`, with the `--demo` tier it carries.
    pub fn new(cwd: impl Into<PathBuf>, tier: Option<u8>) -> Self {
        Self {
            cwd: cwd.into(),
            tier,
        }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "write".into(),
            description: "Write content to a file. Creates the file if it doesn't exist, \
                          overwrites if it does. Automatically creates parent directories."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write (relative or absolute)"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn demo_tier(&self) -> Option<u8> {
        self.tier
    }

    async fn execute(&self, arguments: Json) -> ToolOutput {
        let Some(raw_path) = arguments.get("path").and_then(Json::as_str) else {
            return ToolOutput::err("write requires a path");
        };
        let Some(content) = arguments.get("content").and_then(Json::as_str) else {
            return ToolOutput::err("write requires content");
        };
        let path = paths::resolve(&self.cwd, raw_path);

        // Parent directories come first, as in pi.
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return ToolOutput::err(format!(
                "Cannot create directory: {}: {error}",
                parent.display()
            ));
        }
        // UTF-8, no BOM, no newline translation (pi writes "utf-8").
        if let Err(error) = std::fs::write(&path, content.as_bytes()) {
            return ToolOutput::err(format!("Cannot write file: {}: {error}", path.display()));
        }
        // The message names the path as the model wrote it, like pi.
        ToolOutput::ok(format!("Successfully wrote to {raw_path}"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    async fn run(dir: &Path, args: Json) -> ToolOutput {
        WriteTool::new(dir, None).execute(args).await
    }

    #[tokio::test]
    async fn a_new_file_is_written_with_its_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run(
            tmp.path(),
            serde_json::json!({ "path": "a/b/c.txt", "content": "hello\n" }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text());
        assert_eq!(out.text(), "Successfully wrote to a/b/c.txt");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a/b/c.txt")).unwrap(),
            "hello\n"
        );
    }

    #[tokio::test]
    async fn an_existing_file_is_overwritten_whole() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "old\nlonger\n").unwrap();
        let out = run(
            tmp.path(),
            serde_json::json!({ "path": "a.txt", "content": "new" }),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn content_is_written_as_utf8_without_a_bom() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run(
            tmp.path(),
            serde_json::json!({ "path": "cn.txt", "content": "你好\n" }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text());
        let bytes = std::fs::read(tmp.path().join("cn.txt")).unwrap();
        assert_eq!(bytes, "你好\n".as_bytes());
        assert_ne!(bytes[..3], [0xef, 0xbb, 0xbf]);
    }

    #[tokio::test]
    async fn the_path_resolves_against_the_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run(
            tmp.path(),
            serde_json::json!({ "path": "@nested/./x.txt", "content": "x" }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text());
        assert!(tmp.path().join("nested/x.txt").exists());
    }

    #[tokio::test]
    async fn a_missing_argument_is_an_error_result() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run(tmp.path(), serde_json::json!({ "path": "a.txt" })).await;
        assert!(out.is_error);
        assert_eq!(out.text(), "write requires content");
    }

    #[tokio::test]
    async fn an_unwritable_target_is_an_error_result() {
        let tmp = tempfile::tempdir().unwrap();
        // The parent path is a file, so neither the mkdir nor the write can
        // succeed — the model gets a result it can read and retry from.
        std::fs::write(tmp.path().join("blocker"), "x").unwrap();
        let out = run(
            tmp.path(),
            serde_json::json!({ "path": "blocker/child.txt", "content": "x" }),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text().starts_with("Cannot "), "{}", out.text());
    }
}
