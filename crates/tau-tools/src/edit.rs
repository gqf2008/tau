//! `edit` — a port of pi's `core/tools/edit.ts`.
//!
//! The matching itself lives in [`crate::edit_match`]; this module is the
//! tool: schema, argument tolerance, BOM and line-ending preservation, and
//! the file access check.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value as Json;
use tau_core::tool::{Tool, ToolDef, ToolOutput};

use crate::edit_match::Edit;
use crate::paths;

/// The BOM this tool strips before matching and puts back on write.
const BOM: char = '\u{feff}';

/// Edit one file with exact-text replacements.
pub struct EditTool {
    cwd: PathBuf,
    tier: Option<u8>,
}

impl EditTool {
    /// A tool editing relative to `cwd`, with the `--demo` tier it carries.
    pub fn new(cwd: impl Into<PathBuf>, tier: Option<u8>) -> Self {
        Self {
            cwd: cwd.into(),
            tier,
        }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "edit".into(),
            description: "Edit a single file using exact text replacement. Every edits[].oldText \
                          must match a unique, non-overlapping region of the original file. If \
                          two changes affect the same block or nearby lines, merge them into one \
                          edit instead of emitting overlapping edits. Do not include large \
                          unchanged regions just to connect distant changes."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to edit (relative or absolute)"
                    },
                    "edits": {
                        "type": "array",
                        "description": "One or more targeted replacements. Each edit is matched \
                                        against the original file, not incrementally. Do not \
                                        include overlapping or nested edits. If two changes touch \
                                        the same block or nearby lines, merge them into one edit \
                                        instead.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": {
                                    "type": "string",
                                    "description": "Exact text for one targeted replacement. It \
                                                    must be unique in the original file and must \
                                                    not overlap with any other edits[].oldText \
                                                    in the same call."
                                },
                                "newText": {
                                    "type": "string",
                                    "description": "Replacement text for this targeted edit."
                                }
                            },
                            "required": ["oldText", "newText"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
        }
    }

    fn demo_tier(&self) -> Option<u8> {
        self.tier
    }

    async fn execute(&self, arguments: Json) -> ToolOutput {
        let Some(raw_path) = arguments.get("path").and_then(Json::as_str) else {
            return ToolOutput::err("edit requires a path");
        };
        let Some(edits) = prepare_edits(&arguments) else {
            return ToolOutput::err(
                "Edit tool input is invalid. edits must contain at least one replacement.",
            );
        };
        let path = paths::resolve(&self.cwd, raw_path);

        // Readable *and* writable, like pi's access(R_OK | W_OK) — reported
        // with node's error code so the message reads the same as pi's.
        if let Err(error) = std::fs::OpenOptions::new().write(true).open(&path) {
            return ToolOutput::err(format!(
                "Could not edit file: {raw_path}. Error code: {}.",
                error_code(&error)
            ));
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return ToolOutput::err(format!("Cannot read file: {}: {error}", path.display()));
            }
        };

        // The model never sends the invisible BOM, so match without it and
        // put it back on write.
        let text = String::from_utf8_lossy(&bytes);
        let (bom, content) = match text.strip_prefix(BOM) {
            Some(rest) => (BOM.to_string(), rest),
            None => (String::new(), text.as_ref()),
        };
        let ending = crate::edit_match::detect_line_ending(content);
        let normalized = crate::edit_match::normalize_to_lf(content);

        let applied = match crate::edit_match::apply_edits_to_normalized_content(
            &normalized,
            &edits,
            raw_path,
        ) {
            Ok(applied) => applied,
            Err(message) => return ToolOutput::err(message),
        };

        let final_content = format!(
            "{bom}{}",
            crate::edit_match::restore_line_endings(&applied.new_content, ending)
        );
        if let Err(error) = std::fs::write(&path, final_content.as_bytes()) {
            return ToolOutput::err(format!("Cannot write file: {}: {error}", path.display()));
        }
        ToolOutput::ok(format!(
            "Successfully replaced {} block(s) in {raw_path}.",
            edits.len()
        ))
    }
}

/// The error code pi's message carries: node's for the two cases a model
/// actually hits, and the OS error otherwise.
fn error_code(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "ENOENT".to_string(),
        std::io::ErrorKind::PermissionDenied => "EACCES".to_string(),
        _ => error.to_string(),
    }
}

/// The edit shapes models actually send (pi's `prepareEditArguments`):
/// `edits` as an array, as a JSON *string*, as a single edit object, or the
/// legacy top-level `oldText`/`newText` pair — which pi appends to whatever
/// `edits` held. `None` when the arguments cannot be read as edits at all.
fn prepare_edits(arguments: &Json) -> Option<Vec<Edit>> {
    let mut edits_value = arguments.get("edits").cloned();
    if let Some(Json::String(raw)) = edits_value.clone()
        && let Ok(parsed) = serde_json::from_str::<Json>(&raw)
    {
        edits_value = Some(parsed);
    }

    let edits: Option<Vec<Edit>> = match edits_value {
        Some(Json::Array(items)) => items.iter().map(as_edit).collect(),
        Some(value) => as_edit(&value).map(|edit| vec![edit]),
        None => Some(Vec::new()),
    };
    let mut edits = edits?;

    if let (Some(old_text), Some(new_text)) = (
        arguments.get("oldText").and_then(Json::as_str),
        arguments.get("newText").and_then(Json::as_str),
    ) {
        edits.push(Edit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        });
    }

    (!edits.is_empty()).then_some(edits)
}

/// One `{oldText, newText}` object, or `None` if it is anything else.
fn as_edit(value: &Json) -> Option<Edit> {
    let object = value.as_object()?;
    Some(Edit {
        old_text: object.get("oldText")?.as_str()?.to_string(),
        new_text: object.get("newText")?.as_str()?.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    async fn run(dir: &Path, args: Json) -> ToolOutput {
        EditTool::new(dir, None).execute(args).await
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn read(dir: &Path, name: &str) -> Vec<u8> {
        std::fs::read(dir.join(name)).unwrap()
    }

    fn read_text(dir: &Path, name: &str) -> String {
        String::from_utf8(read(dir, name)).unwrap()
    }

    #[tokio::test]
    async fn one_edit_reports_what_it_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "one\ntwo\n");
        let out = run(
            tmp.path(),
            serde_json::json!({
                "path": "a.txt",
                "edits": [{ "oldText": "two", "newText": "2" }],
            }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text());
        assert_eq!(out.text(), "Successfully replaced 1 block(s) in a.txt.");
        assert_eq!(read_text(tmp.path(), "a.txt"), "one\n2\n");
    }

    #[tokio::test]
    async fn several_edits_count_as_several_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "one\ntwo\nthree\n");
        let out = run(
            tmp.path(),
            serde_json::json!({
                "path": "a.txt",
                "edits": [
                    { "oldText": "one", "newText": "1" },
                    { "oldText": "three", "newText": "3" },
                ],
            }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text());
        assert_eq!(out.text(), "Successfully replaced 2 block(s) in a.txt.");
        assert_eq!(read_text(tmp.path(), "a.txt"), "1\ntwo\n3\n");
    }

    #[tokio::test]
    async fn a_crlf_file_stays_crlf() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "one\r\ntwo\r\n");
        let out = run(
            tmp.path(),
            serde_json::json!({
                "path": "a.txt",
                "edits": [{ "oldText": "two", "newText": "2\nand a half" }],
            }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text());
        // The new line the model wrote is CRLF too: the file keeps one shape.
        assert_eq!(
            read(tmp.path(), "a.txt"),
            b"one\r\n2\r\nand a half\r\n".to_vec()
        );
    }

    #[tokio::test]
    async fn a_bom_survives_the_edit() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "\u{feff}one\ntwo\n");
        let out = run(
            tmp.path(),
            serde_json::json!({
                "path": "a.txt",
                "edits": [{ "oldText": "one", "newText": "1" }],
            }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text());
        let bytes = read(tmp.path(), "a.txt");
        assert_eq!(bytes, "\u{feff}1\ntwo\n".as_bytes());
    }

    #[tokio::test]
    async fn a_missing_file_reports_pis_message_with_a_node_code() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run(
            tmp.path(),
            serde_json::json!({
                "path": "nope.txt",
                "edits": [{ "oldText": "a", "newText": "b" }],
            }),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(
            out.text(),
            "Could not edit file: nope.txt. Error code: ENOENT."
        );
    }

    #[tokio::test]
    async fn a_directory_target_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("adir")).unwrap();
        let out = run(
            tmp.path(),
            serde_json::json!({
                "path": "adir",
                "edits": [{ "oldText": "a", "newText": "b" }],
            }),
        )
        .await;
        assert!(out.is_error);
        assert!(
            out.text().starts_with("Could not edit file: adir."),
            "{}",
            out.text()
        );
    }

    #[tokio::test]
    async fn matching_errors_come_back_as_results_not_panics() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "x\ny\nx\n");
        let out = run(
            tmp.path(),
            serde_json::json!({
                "path": "a.txt",
                "edits": [{ "oldText": "x", "newText": "z" }],
            }),
        )
        .await;
        assert!(out.is_error);
        assert!(
            out.text().starts_with("Found 2 occurrences"),
            "{}",
            out.text()
        );
        // Nothing was written.
        assert_eq!(read_text(tmp.path(), "a.txt"), "x\ny\nx\n");
    }

    #[tokio::test]
    async fn an_empty_edit_list_is_pis_invalid_input_message() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "x\n");
        let out = run(
            tmp.path(),
            serde_json::json!({ "path": "a.txt", "edits": [] }),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(
            out.text(),
            "Edit tool input is invalid. edits must contain at least one replacement."
        );
    }

    #[tokio::test]
    async fn a_malformed_edit_entry_is_rejected_rather_than_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "x\n");
        let out = run(
            tmp.path(),
            serde_json::json!({
                "path": "a.txt",
                "edits": [{ "oldText": "x", "newText": "y" }, "junk"],
            }),
        )
        .await;
        assert!(out.is_error);
        assert!(
            out.text().starts_with("Edit tool input is invalid."),
            "{}",
            out.text()
        );
        assert_eq!(read_text(tmp.path(), "a.txt"), "x\n");
    }

    #[tokio::test]
    async fn edits_sent_as_a_json_string_still_work() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "x\n");
        let out = run(
            tmp.path(),
            serde_json::json!({
                "path": "a.txt",
                "edits": r#"[{"oldText":"x","newText":"y"}]"#,
            }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text());
        assert_eq!(read_text(tmp.path(), "a.txt"), "y\n");
    }

    #[tokio::test]
    async fn a_single_edit_object_is_accepted_in_place_of_an_array() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "x\n");
        let out = run(
            tmp.path(),
            serde_json::json!({
                "path": "a.txt",
                "edits": { "oldText": "x", "newText": "y" },
            }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text());
        assert_eq!(read_text(tmp.path(), "a.txt"), "y\n");
    }

    #[tokio::test]
    async fn the_legacy_top_level_pair_is_appended_to_edits() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "x\ny\n");
        let out = run(
            tmp.path(),
            serde_json::json!({
                "path": "a.txt",
                "edits": [{ "oldText": "x", "newText": "1" }],
                "oldText": "y",
                "newText": "2",
            }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text());
        assert_eq!(out.text(), "Successfully replaced 2 block(s) in a.txt.");
        assert_eq!(read_text(tmp.path(), "a.txt"), "1\n2\n");
    }

    #[tokio::test]
    async fn prepare_edits_reads_every_shape() {
        assert_eq!(
            prepare_edits(&serde_json::json!({
                "edits": [{ "oldText": "a", "newText": "b" }]
            })),
            Some(vec![Edit {
                old_text: "a".into(),
                new_text: "b".into()
            }])
        );
        assert_eq!(
            prepare_edits(&serde_json::json!({ "edits": "not json" })),
            None
        );
        assert_eq!(prepare_edits(&serde_json::json!({})), None);
        assert_eq!(
            prepare_edits(&serde_json::json!({ "oldText": "a", "newText": "b" })),
            Some(vec![Edit {
                old_text: "a".into(),
                new_text: "b".into()
            }])
        );
    }

    #[tokio::test]
    async fn a_fuzzy_edit_leaves_the_rest_of_the_file_alone() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "keep   \n“curly” text   \ntail\n");
        let out = run(
            tmp.path(),
            serde_json::json!({
                "path": "a.txt",
                "edits": [{ "oldText": "\"curly\" text", "newText": "straight" }],
            }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text());
        assert_eq!(read_text(tmp.path(), "a.txt"), "keep   \nstraight\ntail\n");
    }

    #[test]
    fn error_codes_cover_the_two_common_kinds() {
        assert_eq!(
            error_code(&std::io::Error::from(std::io::ErrorKind::NotFound)),
            "ENOENT"
        );
        assert_eq!(
            error_code(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            "EACCES"
        );
        assert!(
            error_code(&std::io::Error::from(std::io::ErrorKind::TimedOut)).contains("timed out")
        );
    }

    #[test]
    fn the_tool_never_wants_the_demo_slot() {
        // The tier table in lib.rs never hands edit a tier; this pins the
        // tool's own answer for the shape the demo would have to call.
        assert_eq!(EditTool::new(".", None).demo_tier(), None);
    }
}
