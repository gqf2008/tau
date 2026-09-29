//! `load_skill` — the on-demand half of skills discovery (issue #4).
//!
//! The system prompt carries the *manifest* (one line per skill); this
//! tool reads a body when a task actually calls for one, because skill
//! bodies are large and inlining them all would crowd out the
//! conversation. `path` reads a supporting file inside the same skill
//! directory; anything that climbs out of it is refused by
//! [`tau_core::SkillIndex::load`].

use async_trait::async_trait;
use serde_json::Value as Json;
use tau_core::SkillIndex;
use tau_core::tool::{Tool, ToolDef, ToolOutput};

use crate::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationOptions, format_size, truncate_head,
};

/// Load a skill body (or one of its supporting files) by name.
pub struct LoadSkillTool {
    skills: SkillIndex,
    tier: Option<u8>,
}

impl LoadSkillTool {
    /// A tool over the skills discovered for this session, with the
    /// `--demo` tier it carries.
    pub fn new(skills: SkillIndex, tier: Option<u8>) -> Self {
        Self { skills, tier }
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn def(&self) -> ToolDef {
        let available: Vec<&str> = self
            .skills
            .skills()
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        ToolDef {
            name: "load_skill".into(),
            description: format!(
                "Load a skill's instructions, or one of the files that ship with it (`path`, \
                 relative to the skill directory). Skill bodies are large, which is why they \
                 are not already in the conversation. Available: {}.",
                if available.is_empty() {
                    "none discovered in this working directory".to_string()
                } else {
                    available.join(", ")
                }
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The skill to load, as named in the manifest."
                    },
                    "path": {
                        "type": "string",
                        "description": "A supporting file inside the skill directory; omit to \
                                        load the skill's SKILL.md."
                    }
                },
                "required": ["name"]
            }),
        }
    }

    fn demo_tier(&self) -> Option<u8> {
        self.tier
    }

    async fn execute(&self, arguments: Json) -> ToolOutput {
        let Some(name) = arguments.get("name").and_then(Json::as_str) else {
            return ToolOutput::err("load_skill needs a `name` (a skill from the manifest)");
        };
        let path = match arguments.get("path") {
            None | Some(Json::Null) => None,
            Some(Json::String(path)) => Some(path.as_str()),
            Some(_) => {
                return ToolOutput::err(
                    "`path` must be a string naming a file inside the skill directory",
                );
            }
        };
        match self.skills.load(name, path) {
            Ok(text) => {
                let truncation = truncate_head(&text, TruncationOptions::default());
                if !truncation.truncated {
                    return ToolOutput::ok(text);
                }
                // Same posture as `read`: the head is shown, and the notice
                // says where the rest is and how big it is.
                let shown = match path {
                    Some(path) => format!("{name}/{path}"),
                    None => format!("{name}/SKILL.md"),
                };
                let limit = if truncation.truncated_by == Some(crate::truncate::TruncatedBy::Bytes) {
                    format_size(DEFAULT_MAX_BYTES)
                } else {
                    format!("{DEFAULT_MAX_LINES} lines")
                };
                ToolOutput::ok(format!(
                    "{}\n\n[{} is {} ({} lines); showing the first {} of them ({limit} limit).]",
                    truncation.content,
                    shown,
                    format_size(truncation.total_bytes),
                    truncation.total_lines,
                    truncation.output_lines
                ))
            }
            Err(reason) => ToolOutput::err(format!("load_skill: {reason}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tau_core::SkillIndex;

    fn index() -> (tempfile::TempDir, SkillIndex) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("marker");
        let skill = dir.path().join(".agents/skills/foo");
        std::fs::create_dir_all(&skill).expect("mkdir");
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: foo\ndescription: does foo\n---\n\n# Foo body\n",
        )
        .expect("write");
        std::fs::write(skill.join("notes.md"), "supporting notes\n").expect("write");
        std::fs::write(dir.path().join("secret.txt"), "not yours\n").expect("write");
        let index = SkillIndex::discover(dir.path());
        (dir, index)
    }

    #[tokio::test]
    async fn loads_the_body_and_names_the_available_skills() {
        let (_dir, index) = index();
        let tool = LoadSkillTool::new(index, None);
        assert!(tool.def().description.contains("foo"), "{}", tool.def().description);
        let out = tool.execute(serde_json::json!({"name": "foo"})).await;
        assert!(!out.is_error);
        let text = serde_json::to_value(&out.content).expect("json");
        assert!(text.to_string().contains("# Foo body"), "{text}");
    }

    #[tokio::test]
    async fn loads_a_supporting_file_and_refuses_an_escape() {
        let (_dir, index) = index();
        let tool = LoadSkillTool::new(index, None);
        let out = tool
            .execute(serde_json::json!({"name": "foo", "path": "notes.md"}))
            .await;
        assert!(!out.is_error);
        let escape = tool
            .execute(serde_json::json!({"name": "foo", "path": "../secret.txt"}))
            .await;
        assert!(escape.is_error);
        let text = serde_json::to_value(&escape.content).expect("json").to_string();
        assert!(text.contains("climbs out of the skill directory"), "{text}");
    }

    #[tokio::test]
    async fn a_missing_name_is_a_visible_error_not_an_exception() {
        let (_dir, index) = index();
        let tool = LoadSkillTool::new(index, None);
        for arguments in [
            serde_json::json!({}),
            serde_json::json!({"name": "nope"}),
            serde_json::json!({"name": "foo", "path": 7}),
        ] {
            let out = tool.execute(arguments.clone()).await;
            assert!(out.is_error, "{arguments} should fail");
        }
    }

    #[tokio::test]
    async fn a_body_beyond_the_limit_is_noticed() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("marker");
        let skill = dir.path().join(".agents/skills/big");
        std::fs::create_dir_all(&skill).expect("mkdir");
        let body = (0..DEFAULT_MAX_LINES + 50)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: big\ndescription: a long one\n---\n{body}"),
        )
        .expect("write");
        let tool = LoadSkillTool::new(SkillIndex::discover(dir.path()), None);
        let out = tool.execute(serde_json::json!({"name": "big"})).await;
        assert!(!out.is_error);
        let text = serde_json::to_value(&out.content).expect("json").to_string();
        assert!(text.contains("big/SKILL.md is"), "the notice names the file: {text}");
        assert!(text.contains("showing the first"), "…and says how much it showed: {text}");
    }
}
