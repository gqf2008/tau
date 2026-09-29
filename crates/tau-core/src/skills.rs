//! Skills and project instructions (issue #4).
//!
//! tau picks up the on-disk conventions a user already has, so moving a
//! workflow onto it does not mean re-authoring instructions and skills:
//! skill directories under `.agents/skills/`, `.claude/skills/` and
//! `.goose/skills/` (each one a directory whose `SKILL.md` carries `name`
//! and `description` frontmatter), and `AGENTS.md` project instructions
//! from the session's working directory up to the repository root.
//!
//! The model sees a **manifest** — one line per skill — and loads a body
//! on demand through the `load_skill` tool: bodies are large, and
//! inlining every one of them would crowd out the conversation it is
//! supposed to serve.
//!
//! Discovery never fails a session. A malformed `SKILL.md`, an
//! unreadable directory and a duplicate name are all reported on stderr
//! and skipped — the same posture as a torn session tail.

use std::path::{Path, PathBuf};

/// Where skills are looked for, relative to the working directory, in
/// discovery order (the first root to define a name wins it).
pub const SKILL_DIRS: [&str; 3] = [".agents/skills", ".claude/skills", ".goose/skills"];

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The name the model asks for (`load_skill`), from the frontmatter
    /// or — when the frontmatter does not name it — the directory.
    pub name: String,
    /// The one-line description the manifest shows.
    pub description: String,
    /// The skill's own directory: `SKILL.md` sits here and every
    /// supporting file resolves against it, never outside it.
    pub dir: PathBuf,
}

/// Everything the working directory offers in the way of skills and
/// project instructions.
#[derive(Debug, Clone, Default)]
pub struct SkillIndex {
    skills: Vec<Skill>,
    /// `(path, text)` root-first: the repository root's file comes first.
    hints: Vec<(PathBuf, String)>,
}

impl SkillIndex {
    /// Discover the skills under `cwd` and the `AGENTS.md` hints above it.
    pub fn discover(cwd: &Path) -> Self {
        let mut index = SkillIndex {
            skills: Vec::new(),
            hints: agents_hints(cwd),
        };
        for rel in SKILL_DIRS {
            let root = cwd.join(rel);
            let Ok(entries) = std::fs::read_dir(&root) else {
                continue; // no such root: the normal case
            };
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect();
            dirs.sort(); // read_dir order is the filesystem's, not ours
            for dir in dirs {
                match read_skill(&dir) {
                    Ok(skill) if index.get(&skill.name).is_some() => {
                        eprintln!(
                            "[tau] skill {} ignored: `{}` was discovered earlier",
                            dir.display(),
                            skill.name
                        );
                    }
                    Ok(skill) => index.skills.push(skill),
                    Err(reason) => {
                        eprintln!("[tau] skill {} ignored: {reason}", dir.display());
                    }
                }
            }
        }
        index
    }

    /// The discovered skills, in discovery order.
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// Look one up by the name the model uses.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    /// The project-instruction files that were found, root-first. The
    /// startup line names them, so a run says where its guidance came from
    /// rather than leaving the reader to guess why the model knows things
    /// nobody told it this session.
    pub fn hint_paths(&self) -> impl Iterator<Item = &Path> {
        self.hints.iter().map(|(path, _)| path.as_path())
    }

    /// Load a skill's body (`rel = None`) or one of its supporting files
    /// (`rel`, a path relative to the skill directory). A path that
    /// resolves outside that directory is refused — a skill may not read
    /// the host's filesystem through this door (issue #4's acceptance,
    /// and the read-root confinement #3 will generalize).
    pub fn load(&self, name: &str, rel: Option<&str>) -> Result<String, String> {
        let skill = self
            .get(name)
            .ok_or_else(|| format!("no skill named `{name}`"))?;
        let dir = std::fs::canonicalize(&skill.dir)
            .map_err(|e| format!("{}: {e}", skill.dir.display()))?;
        let target = match rel {
            None => dir.join("SKILL.md"),
            Some(rel) => {
                let rel = Path::new(rel);
                // `has_root`, not `is_absolute`: on Windows a rooted path
                // like `/etc/passwd` is not "absolute" (it has no drive),
                // yet `join` still resolves it against the drive root.
                if rel.has_root() {
                    return Err(format!(
                        "`{}` is not a relative path; supporting files are named relative to the skill directory",
                        rel.display()
                    ));
                }
                // Reject the climb lexically first: canonicalize cannot
                // answer for a path that does not exist, and "the file was
                // missing" is not the refusal this door owes.
                if rel
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
                {
                    return Err(format!(
                        "`{}` climbs out of the skill directory",
                        rel.display()
                    ));
                }
                dir.join(rel)
            }
        };
        let resolved = std::fs::canonicalize(&target)
            .map_err(|e| format!("{}: {e}", target.display()))?;
        if !resolved.starts_with(&dir) {
            return Err(format!(
                "`{}` resolves outside the skill directory",
                target.display()
            ));
        }
        std::fs::read_to_string(&resolved).map_err(|e| format!("{}: {e}", resolved.display()))
    }

    /// The project instructions from [`Self::hints_context`], plus the
    /// skills manifest the `load_skill` tool serves. `None` when the
    /// working directory offers neither (the common case in a bare
    /// directory), so a session that has nothing to add sends the
    /// requests it always did.
    pub fn system_context(&self) -> Option<String> {
        let mut out = self.hints_context().unwrap_or_default();
        if !self.skills.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(
                "# Skills\n\n\
                 The working directory offers the skills below. Each one's body is \
                 large, so it is not inlined here: call `load_skill` with the name \
                 when a task calls for one, and pass `path` to read a supporting \
                 file it mentions.\n\n",
            );
            for skill in &self.skills {
                out.push_str("- ");
                out.push_str(&skill.name);
                out.push_str(": ");
                out.push_str(&skill.description);
                out.push('\n');
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// Project instructions only — the `AGENTS.md` files, root-first.
    /// This is the context for a session whose built-ins do not include
    /// `load_skill`: advertising a manifest the model cannot load would
    /// invite calls that must fail.
    pub fn hints_context(&self) -> Option<String> {
        if self.hints.is_empty() {
            return None;
        }
        let mut out = String::new();
        for (path, text) in &self.hints {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str("# Project instructions (");
            out.push_str(&path.display().to_string());
            out.push_str(")\n\n");
            out.push_str(text.trim_end());
        }
        Some(out)
    }
}

/// Read one skill directory. The name comes from the frontmatter, or from
/// the directory when the frontmatter does not name it; a missing
/// description is a warning, not a refusal (many skills carry only a
/// name, and a manifest line without a description is still useful).
fn read_skill(dir: &Path) -> Result<Skill, String> {
    let file = dir.join("SKILL.md");
    let text = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
    let (name, description) = frontmatter(&text);
    let name = match name {
        Some(name) if !name.is_empty() => name,
        _ => dir
            .file_name()
            .map(|part| part.to_string_lossy().into_owned())
            .ok_or_else(|| "the skill directory has no name".to_string())?,
    };
    let description = description.unwrap_or_default();
    if description.is_empty() {
        eprintln!(
            "[tau] skill `{name}` has no description in its frontmatter; \
             the manifest line will be bare"
        );
    }
    Ok(Skill {
        name,
        description,
        dir: dir.to_path_buf(),
    })
}

/// The `name` and `description` of a `SKILL.md`'s frontmatter. Deliberately
/// a hand-rolled reader for that one shape — a YAML dependency for two
/// scalar keys would be the tail wagging the dog — so it accepts
/// `key: value` lines between the opening and closing `---`, with the
/// value optionally quoted.
fn frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return (None, None);
    }
    let (mut name, mut description) = (None, None);
    for line in lines {
        let line = line.trim_end();
        if line == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_string();
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            _ => {}
        }
    }
    (name, description)
}

/// Every `AGENTS.md` from `cwd` up to the repository root, root-first.
/// The walk stops at the directory holding `.git` (that IS the repository
/// root) or at the filesystem root when there is no repository anywhere
/// above — a file outside the project is not this project's guidance.
fn agents_hints(cwd: &Path) -> Vec<(PathBuf, String)> {
    let mut dirs = Vec::new();
    let mut cursor = Some(cwd);
    while let Some(dir) = cursor {
        dirs.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            break;
        }
        cursor = dir.parent();
    }
    let mut hints = Vec::new();
    for dir in dirs.iter().rev() {
        let file = dir.join("AGENTS.md");
        match std::fs::read_to_string(&file) {
            Ok(text) if !text.trim().is_empty() => hints.push((file, text)),
            _ => {}
        }
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory tree with a `.git` marker, so the hint walk stops there
    /// instead of climbing into whatever surrounds the temp directory.
    fn workdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("marker");
        dir
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, body).expect("write");
    }

    const FOO: &str = "---\nname: foo\ndescription: foo does foo things\n---\n\n# Foo\n\nbody\n";

    #[test]
    fn discovers_skills_from_all_three_conventions() {
        let dir = workdir();
        write(dir.path(), ".agents/skills/foo/SKILL.md", FOO);
        write(
            dir.path(),
            ".claude/skills/bar/SKILL.md",
            "---\nname: bar\ndescription: \"quoted: description\"\n---\nbody\n",
        );
        write(
            dir.path(),
            ".goose/skills/zed/SKILL.md",
            "---\ndescription: no name here\n---\nbody\n",
        );
        let index = SkillIndex::discover(dir.path());
        let names: Vec<&str> = index.skills().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["foo", "bar", "zed"], "discovery order is the root order");
        assert_eq!(index.get("bar").expect("bar").description, "quoted: description");
        assert_eq!(
            index.get("zed").expect("zed").name,
            "zed",
            "a skill without a frontmatter name is named by its directory"
        );
    }

    #[test]
    fn a_missing_or_broken_skill_is_skipped_not_fatal() {
        let dir = workdir();
        write(dir.path(), ".agents/skills/foo/SKILL.md", FOO);
        // No SKILL.md at all.
        std::fs::create_dir_all(dir.path().join(".agents/skills/empty")).expect("mkdir");
        // A SKILL.md without frontmatter still yields a skill (directory
        // name, empty description) — only an unreadable file is skipped.
        write(dir.path(), ".agents/skills/plain/SKILL.md", "just a body\n");
        let index = SkillIndex::discover(dir.path());
        let names: Vec<&str> = index.skills().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["foo", "plain"]);
        assert_eq!(index.get("plain").expect("plain").description, "");
    }

    #[test]
    fn a_name_clash_keeps_the_first_discovered() {
        let dir = workdir();
        write(dir.path(), ".agents/skills/foo/SKILL.md", FOO);
        write(
            dir.path(),
            ".claude/skills/foo/SKILL.md",
            "---\nname: foo\ndescription: second one\n---\nbody\n",
        );
        let index = SkillIndex::discover(dir.path());
        assert_eq!(index.skills().len(), 1);
        assert_eq!(index.get("foo").expect("foo").description, "foo does foo things");
    }

    #[test]
    fn loads_a_body_and_a_supporting_file() {
        let dir = workdir();
        write(dir.path(), ".agents/skills/foo/SKILL.md", FOO);
        write(dir.path(), ".agents/skills/foo/scripts/run.sh", "echo hi\n");
        let index = SkillIndex::discover(dir.path());
        let body = index.load("foo", None).expect("body");
        assert!(body.contains("# Foo"), "{body}");
        assert_eq!(
            index.load("foo", Some("scripts/run.sh")).expect("supporting"),
            "echo hi\n"
        );
    }

    #[test]
    fn refuses_unknown_skills_and_escaping_paths() {
        let dir = workdir();
        write(dir.path(), ".agents/skills/foo/SKILL.md", FOO);
        write(dir.path(), "outside.txt", "secret\n");
        let index = SkillIndex::discover(dir.path());
        let unknown = index.load("nope", None).expect_err("unknown skill");
        assert!(unknown.contains("no skill named `nope`"), "{unknown}");
        // An existing file one level up: the canonicalized check catches it.
        write(dir.path(), ".agents/skills/outside.txt", "not yours\n");
        let err = index
            .load("foo", Some("../outside.txt"))
            .expect_err("an existing file outside the skill must be refused");
        assert!(err.contains("climbs out of the skill directory"), "{err}");
        // A climb with nothing at the end still has to be a refusal, not
        // "no such file".
        let err = index
            .load("foo", Some("../../nowhere.txt"))
            .expect_err("a climb must be refused before the filesystem is asked");
        assert!(err.contains("climbs out of the skill directory"), "{err}");
        // Rooted paths are not "absolute" on Windows, so the refusal is
        // written against `has_root` — assert the message either way.
        for rooted in ["/etc/passwd", "C:/Windows/win.ini"] {
            let err = index
                .load("foo", Some(rooted))
                .expect_err("a rooted path must be refused");
            assert!(err.contains("is not a relative path"), "{rooted}: {err}");
        }
    }

    #[test]
    fn hints_run_root_first_and_stop_at_the_repository_root() {
        let dir = workdir();
        let nested = dir.path().join("crates/inner");
        std::fs::create_dir_all(&nested).expect("mkdir");
        write(dir.path(), "AGENTS.md", "root instructions\n");
        write(dir.path(), "crates/AGENTS.md", "middle instructions\n");
        write(dir.path(), "crates/inner/AGENTS.md", "inner instructions\n");
        let index = SkillIndex::discover(&nested);
        let context = index.system_context().expect("context");
        let root = context.find("root instructions").expect("root present");
        let middle = context.find("middle instructions").expect("middle present");
        let inner = context.find("inner instructions").expect("inner present");
        assert!(root < middle && middle < inner, "root-first order: {context}");
    }

    #[test]
    fn a_directory_without_skills_or_hints_adds_nothing_to_the_prompt() {
        let dir = workdir();
        assert_eq!(SkillIndex::discover(dir.path()).system_context(), None);
    }

    #[test]
    fn the_manifest_lists_names_and_descriptions_without_bodies() {
        let dir = workdir();
        write(dir.path(), ".agents/skills/foo/SKILL.md", FOO);
        let context = SkillIndex::discover(dir.path())
            .system_context()
            .expect("context");
        assert!(context.contains("- foo: foo does foo things"), "{context}");
        assert!(
            !context.contains("# Foo"),
            "the body must not be inlined: {context}"
        );
        assert!(context.contains("load_skill"), "{context}");
    }

    #[test]
    fn hints_context_leaves_the_manifest_out() {
        let dir = workdir();
        write(dir.path(), "AGENTS.md", "root instructions\n");
        write(dir.path(), ".agents/skills/foo/SKILL.md", FOO);
        let index = SkillIndex::discover(dir.path());
        let hints = index.hints_context().expect("hints");
        assert!(hints.contains("root instructions"), "{hints}");
        assert!(!hints.contains("load_skill"), "{hints}");
        assert!(!hints.contains("- foo:"), "{hints}");
        assert!(
            index.system_context().expect("context").contains("- foo:"),
            "the manifest is still there when the tool is"
        );
    }
}
