//! `read` — a port of pi's `core/tools/read.ts`.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value as Json;
use tau_core::Media;
use tau_core::tool::{Tool, ToolDef, ToolOutput};
use tau_core::types::Content;

use crate::paths;
use crate::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, format_size,
    truncate_head,
};

/// Read a text file verbatim, or an image as an attachment.
pub struct ReadTool {
    cwd: PathBuf,
    tier: Option<u8>,
}

impl ReadTool {
    /// A tool reading relative to `cwd`, with the `--demo` tier it carries.
    pub fn new(cwd: impl Into<PathBuf>, tier: Option<u8>) -> Self {
        Self {
            cwd: cwd.into(),
            tier,
        }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "read".into(),
            description: format!(
                "Read the contents of a file. Supports text files and images (jpg, png, gif, \
                 webp, bmp). Images are sent as attachments. For text files, output is truncated \
                 to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit \
                 for large files. When you need the full file, continue with offset until \
                 complete.",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read (relative or absolute)"
                    },
                    "offset": {
                        "type": "number",
                        "description": "Line number to start reading from (1-indexed)"
                    },
                    "limit": {
                        "type": "number",
                        "description": "Maximum number of lines to read"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn demo_tier(&self) -> Option<u8> {
        self.tier
    }

    async fn execute(&self, arguments: Json) -> ToolOutput {
        let Some(raw_path) = arguments.get("path").and_then(Json::as_str) else {
            return ToolOutput::err("read requires a path");
        };
        let offset = arguments.get("offset").and_then(Json::as_u64);
        let limit = arguments
            .get("limit")
            .and_then(Json::as_u64)
            .map(|n| n as usize);
        let path = paths::resolve(&self.cwd, raw_path);

        // pi surfaces the raw OS error here (its text is node's); tau prints
        // the path and the OS error together.
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return ToolOutput::err(format!("Cannot read file: {}: {error}", path.display()));
            }
        };

        // Sniff the head: an image becomes a text note plus an image block
        // the provider can actually see.
        let sniffed = &bytes[..bytes.len().min(crate::mime::SNIFF_BYTES)];
        if let Some(mime) = crate::mime::detect(sniffed) {
            return ToolOutput::ok_blocks(vec![
                Content::Text {
                    text: format!("Read image file [{mime}]"),
                },
                Content::Image {
                    media: Media::bytes(mime, bytes),
                },
            ]);
        }

        // Invalid UTF-8 reads as replacement characters, like pi's
        // `buffer.toString("utf-8")`.
        let text = String::from_utf8_lossy(&bytes);
        let all_lines: Vec<&str> = text.split('\n').collect();
        let total_file_lines = all_lines.len();

        // 1-indexed input, 0-indexed array access.
        let start_line = offset.map(|o| o.saturating_sub(1) as usize).unwrap_or(0);
        let start_display = start_line + 1;
        if start_line >= total_file_lines {
            return ToolOutput::err(format!(
                "Offset {} is beyond end of file ({total_file_lines} lines total)",
                offset.unwrap_or(0)
            ));
        }

        // A user limit applies before truncation decides anything.
        let user_limited = limit.map(|l| (start_line + l).min(total_file_lines) - start_line);
        let selected = match limit {
            Some(l) => all_lines[start_line..(start_line + l).min(total_file_lines)].join("\n"),
            None => all_lines[start_line..].join("\n"),
        };
        let truncation = truncate_head(&selected, TruncationOptions::default());

        let output = if truncation.first_line_exceeds_limit {
            // Nothing fits: point the model at a shell instead.
            format!(
                "[Line {start_display} is {}, exceeds {} limit. Use bash: sed -n \
                 '{start_display}p' {raw_path} | head -c {DEFAULT_MAX_BYTES}]",
                format_size(all_lines[start_line].len()),
                format_size(DEFAULT_MAX_BYTES)
            )
        } else if truncation.truncated {
            let end_display = start_display + truncation.output_lines - 1;
            let next_offset = end_display + 1;
            let notice = if truncation.truncated_by == Some(TruncatedBy::Bytes) {
                format!(
                    "[Showing lines {start_display}-{end_display} of {total_file_lines} \
                     ({} limit). Use offset={next_offset} to continue.]",
                    format_size(DEFAULT_MAX_BYTES)
                )
            } else {
                format!(
                    "[Showing lines {start_display}-{end_display} of {total_file_lines}. \
                     Use offset={next_offset} to continue.]"
                )
            };
            format!("{}\n\n{notice}", truncation.content)
        } else if let Some(user_limited) = user_limited
            && start_line + user_limited < total_file_lines
        {
            // The user's own limit stopped early and the file has more.
            let remaining = total_file_lines - (start_line + user_limited);
            let next_offset = start_line + user_limited + 1;
            format!(
                "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                truncation.content
            )
        } else {
            truncation.content
        };
        ToolOutput::ok(output)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    async fn run(dir: &Path, args: Json) -> ToolOutput {
        ReadTool::new(dir, None).execute(args).await
    }

    fn text(out: &ToolOutput) -> String {
        out.text()
    }

    fn write(dir: &Path, name: &str, body: &str) -> String {
        std::fs::write(dir.join(name), body).expect("write fixture");
        name.to_string()
    }

    #[tokio::test]
    async fn text_comes_back_verbatim_and_unnumbered() {
        let tmp = tempfile::tempdir().unwrap();
        let name = write(tmp.path(), "a.txt", "line one\nline two\n");
        let out = run(tmp.path(), serde_json::json!({ "path": name })).await;
        assert!(!out.is_error, "{}", text(&out));
        // Body and trailing newline exactly as on disk: no line numbers, no
        // notice, no re-wrapping.
        assert_eq!(text(&out), "line one\nline two\n");
    }

    #[tokio::test]
    async fn offset_is_one_indexed() {
        let tmp = tempfile::tempdir().unwrap();
        let name = write(tmp.path(), "a.txt", "a\nb\nc");
        let out = run(tmp.path(), serde_json::json!({ "path": name, "offset": 2 })).await;
        assert_eq!(text(&out), "b\nc");
    }

    #[tokio::test]
    async fn a_limit_that_stops_early_notices_the_remaining_lines() {
        let tmp = tempfile::tempdir().unwrap();
        // Three lines plus the trailing newline pi counts as a fourth.
        let name = write(tmp.path(), "a.txt", "a\nb\nc\n");
        let out = run(tmp.path(), serde_json::json!({ "path": name, "limit": 2 })).await;
        assert_eq!(
            text(&out),
            "a\nb\n\n[2 more lines in file. Use offset=3 to continue.]"
        );
    }

    #[tokio::test]
    async fn a_limit_that_reaches_the_end_is_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let name = write(tmp.path(), "a.txt", "a\nb\nc");
        let out = run(tmp.path(), serde_json::json!({ "path": name, "limit": 3 })).await;
        assert_eq!(text(&out), "a\nb\nc");
        let over = run(tmp.path(), serde_json::json!({ "path": name, "limit": 99 })).await;
        assert_eq!(text(&over), "a\nb\nc");
    }

    #[tokio::test]
    async fn an_offset_past_the_end_is_pis_error() {
        let tmp = tempfile::tempdir().unwrap();
        let name = write(tmp.path(), "a.txt", "a\nb\nc");
        let out = run(tmp.path(), serde_json::json!({ "path": name, "offset": 9 })).await;
        assert!(out.is_error);
        assert_eq!(text(&out), "Offset 9 is beyond end of file (3 lines total)");
    }

    #[tokio::test]
    async fn reading_a_missing_file_is_an_error_result() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run(tmp.path(), serde_json::json!({ "path": "nope.txt" })).await;
        assert!(out.is_error);
        let body = text(&out);
        assert!(body.starts_with("Cannot read file: "), "{body}");
        assert!(body.contains("nope.txt"), "{body}");
    }

    #[tokio::test]
    async fn the_line_limit_notices_the_next_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let body = (1..=2001)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let name = write(tmp.path(), "big.txt", &body);
        let out = run(tmp.path(), serde_json::json!({ "path": name })).await;
        let body = text(&out);
        assert!(
            body.ends_with("[Showing lines 1-2000 of 2001. Use offset=2001 to continue.]"),
            "{}",
            &body[body.len().saturating_sub(90)..]
        );
        assert!(body.starts_with("1\n2\n3\n"));
    }

    #[tokio::test]
    async fn the_byte_limit_notice_carries_the_limit_size() {
        let tmp = tempfile::tempdir().unwrap();
        let body = (1..=600)
            .map(|i| format!("{i:03} {}", "x".repeat(96)))
            .collect::<Vec<_>>()
            .join("\n");
        let name = write(tmp.path(), "wide.txt", &body);
        let out = run(tmp.path(), serde_json::json!({ "path": name })).await;
        let body = text(&out);
        assert!(
            body.contains("(50.0KB limit). Use offset="),
            "{}",
            &body[body.len().saturating_sub(90)..]
        );
        assert!(
            body.contains(" of 600 "),
            "{}",
            &body[body.len().saturating_sub(90)..]
        );
    }

    #[tokio::test]
    async fn a_first_line_over_the_byte_limit_points_at_sed() {
        let tmp = tempfile::tempdir().unwrap();
        let long = "y".repeat(60 * 1024);
        let name = write(tmp.path(), "one-line.txt", &long);
        let out = run(tmp.path(), serde_json::json!({ "path": name })).await;
        assert_eq!(
            text(&out),
            format!(
                "[Line 1 is 60.0KB, exceeds 50.0KB limit. Use bash: sed -n '1p' {name} | head -c 51200]"
            )
        );
    }

    #[tokio::test]
    async fn an_image_is_attached_with_a_text_note() {
        let tmp = tempfile::tempdir().unwrap();
        let mut png = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&[0u8; 21]);
        std::fs::write(tmp.path().join("shot.png"), &png).unwrap();

        let out = run(tmp.path(), serde_json::json!({ "path": "shot.png" })).await;
        assert!(!out.is_error);
        assert_eq!(out.content.len(), 2, "{:?}", out.content);
        match &out.content[0] {
            Content::Text { text } => assert_eq!(text, "Read image file [image/png]"),
            other => panic!("expected the text note, got {other:?}"),
        }
        match &out.content[1] {
            Content::Image { media } => {
                assert_eq!(media.media_type, "image/png");
                assert!(matches!(media.source, tau_core::MediaSource::Bytes(_)));
            }
            other => panic!("expected the image block, got {other:?}"),
        }
        // And the media survives the JSON edge as base64.
        assert!(media_base64(&out).is_some());
    }

    #[tokio::test]
    async fn a_missing_path_argument_is_an_error_result() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run(tmp.path(), serde_json::json!({})).await;
        assert!(out.is_error);
        assert_eq!(text(&out), "read requires a path");
    }

    #[tokio::test]
    async fn invalid_utf8_reads_as_replacement_characters() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("bin.txt"), [0x61, 0xff, 0x62]).unwrap();
        let out = run(tmp.path(), serde_json::json!({ "path": "bin.txt" })).await;
        assert!(!out.is_error);
        assert_eq!(text(&out), "a\u{fffd}b");
    }

    fn media_base64(out: &ToolOutput) -> Option<String> {
        out.content.iter().find_map(|c| match c {
            Content::Image { media } => media.source.encode_base64(),
            _ => None,
        })
    }
}
