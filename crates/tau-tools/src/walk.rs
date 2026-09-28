//! The file walker `grep` and `find` share.
//!
//! It walks the way ripgrep and fd walk: `.gitignore` respected — even
//! outside a git repository, because pi passes `--no-require-git` in that
//! case — hidden files and directories included (`--hidden`), and the `.git`
//! directory itself skipped.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use ignore::{DirEntry, WalkBuilder};

/// Walk `root`, ignoring nothing but what the ignore files say (plus `.git`).
fn walk(root: &Path) -> impl Iterator<Item = DirEntry> {
    WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
        .filter_map(Result::ok)
}

/// Every file under `root`.
pub fn files(root: &Path) -> impl Iterator<Item = PathBuf> {
    walk(root)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .map(DirEntry::into_path)
}

/// Every entry under `root`, files and directories alike (fd's default), as
/// `(path, is_dir)`. The root itself is included first, so callers that only
/// want its contents skip it.
pub fn all_entries(root: &Path) -> impl Iterator<Item = (PathBuf, bool)> {
    walk(root).map(|entry| {
        let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
        (entry.into_path(), is_dir)
    })
}

/// `path` relative to `root`, with `/` separators, for display. The root
/// itself reads as `.`, like [`crate::paths::display`].
pub fn relative(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

/// A compiled `--glob` filter, with ripgrep's rule baked in: a pattern
/// containing a separator matches the path relative to the search root, one
/// without matches the file name. As in ripgrep, `*` does not cross a `/` —
/// `**/` is how a pattern reaches into subdirectories.
pub struct FileFilter {
    matcher: GlobMatcher,
    full_path: bool,
}

impl FileFilter {
    /// Compile `pattern`, or report why it is not a glob.
    pub fn new(pattern: &str) -> Result<Self, globset::Error> {
        Ok(Self {
            matcher: GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()?
                .compile_matcher(),
            full_path: pattern.contains('/'),
        })
    }

    /// True when `path` (found under `root`) passes the filter.
    pub fn matches(&self, root: &Path, path: &Path) -> bool {
        if self.full_path {
            self.matcher.is_match(relative(root, path))
        } else {
            path.file_name()
                .is_some_and(|name| self.matcher.is_match(name))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(root: &Path) {
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        std::fs::write(root.join("src/nested/lib.rs"), "").unwrap();
        std::fs::write(root.join("top.md"), "").unwrap();
        std::fs::write(root.join("skipped.rs"), "").unwrap();
        std::fs::write(root.join(".gitignore"), "skipped.rs\n").unwrap();
        std::fs::write(root.join(".hidden.rs"), "").unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::write(root.join(".git/objects/blob"), "").unwrap();
    }

    fn names(root: &Path, files: impl Iterator<Item = PathBuf>) -> Vec<String> {
        let mut names: Vec<String> = files.map(|p| relative(root, &p)).collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn files_are_walked_with_gitignore_respected_and_hidden_included() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        assert_eq!(
            names(tmp.path(), files(tmp.path())),
            vec![
                ".gitignore",
                ".hidden.rs",
                "src/main.rs",
                "src/nested/lib.rs",
                "top.md"
            ]
        );
    }

    #[test]
    fn gitignore_applies_without_a_git_repository() {
        let tmp = tempfile::tempdir().unwrap();
        // No `.git` anywhere above: pi passes --no-require-git there, and so
        // does this walker.
        std::fs::create_dir_all(tmp.path().join("nested")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "nested/\n").unwrap();
        std::fs::write(tmp.path().join("nested/x.rs"), "").unwrap();
        assert_eq!(names(tmp.path(), files(tmp.path())), vec![".gitignore"]);
    }

    #[test]
    fn entries_include_directories_and_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let entries: Vec<(String, bool)> = all_entries(tmp.path())
            .map(|(path, is_dir)| (relative(tmp.path(), &path), is_dir))
            .collect();
        assert_eq!(entries[0].0, ".");
        assert!(
            entries
                .iter()
                .any(|(name, is_dir)| name == "src" && *is_dir)
        );
        assert!(
            entries
                .iter()
                .any(|(name, is_dir)| name == "src/main.rs" && !*is_dir)
        );
        assert!(!entries.iter().any(|(name, _)| name.starts_with(".git/")));
    }

    #[test]
    fn a_bare_glob_matches_the_file_name() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let filter = FileFilter::new("*.rs").unwrap();
        assert!(filter.matches(tmp.path(), &tmp.path().join("src/main.rs")));
        assert!(filter.matches(tmp.path(), &tmp.path().join(".hidden.rs")));
        assert!(!filter.matches(tmp.path(), &tmp.path().join("top.md")));
    }

    #[test]
    fn a_path_glob_matches_the_relative_path() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path());
        let nested = FileFilter::new("src/nested/*.rs").unwrap();
        assert!(nested.matches(tmp.path(), &tmp.path().join("src/nested/lib.rs")));
        assert!(!nested.matches(tmp.path(), &tmp.path().join("src/main.rs")));

        // `**` covers zero directories, so this reaches the nested file too.
        let any = FileFilter::new("src/**/*.rs").unwrap();
        assert!(any.matches(tmp.path(), &tmp.path().join("src/nested/lib.rs")));
        assert!(any.matches(tmp.path(), &tmp.path().join("src/main.rs")));
    }

    #[test]
    fn an_invalid_glob_is_an_error() {
        assert!(FileFilter::new("[").is_err());
    }
}
