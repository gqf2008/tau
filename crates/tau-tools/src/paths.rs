//! Path resolution for the built-in tools — the Linux/Windows subset of
//! pi's `utils/paths.ts`.
//!
//! Models write paths the way humans do: `@src/main.rs`, `~/notes.txt`,
//! `/c/Users/me` (copy-pasted out of Git Bash). All of it lands on
//! [`resolve`], which yields an absolute path against the session cwd.
//!
//! Not ported: pi's macOS-only fallbacks (NFD normalization, curly quotes,
//! screenshot narrow-NBSP) — they exist for paths pasted out of macOS apps.

use std::path::{Component, Path, PathBuf};

/// Unicode spaces pi folds to a plain space before resolving: NBSP, the
/// en/em quad family (U+2000–U+200A), narrow NBSP, medium math space, and
/// ideographic space. A path copied out of a document or a chat message
/// often carries one of these.
const UNICODE_SPACES: [char; 15] = [
    '\u{a0}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}',
    '\u{2007}', '\u{2008}', '\u{2009}', '\u{200a}', '\u{202f}', '\u{205f}', '\u{3000}',
];

/// Normalize a model-supplied path the way pi's `normalizePath` does:
/// unicode spaces become plain spaces, one leading `@` is dropped, a leading
/// `~` expands to the home directory, and (on Windows) a Git-Bash/MSYS
/// (`/c/...`) or WSL (`/mnt/c/...`) drive path is rewritten to `C:\...`.
/// The result may still be relative.
pub fn normalize(raw: &str) -> PathBuf {
    let mut s: String = raw
        .chars()
        .map(|c| if UNICODE_SPACES.contains(&c) { ' ' } else { c })
        .collect();
    if let Some(rest) = s.strip_prefix('@') {
        s = rest.to_string();
    }
    if cfg!(windows) {
        s = normalize_windows_shell_path(&s);
    }
    if s == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    #[cfg(windows)]
    if let Some(rest) = s.strip_prefix("~\\")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(s)
}

/// `/c/Users/me` and `/mnt/c/Users/me` are how shells on Windows spell
/// `C:\Users\me`; pi converts both.
fn normalize_windows_shell_path(s: &str) -> String {
    let bytes = s.as_bytes();
    // `/c/rest` → `C:\rest`
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b'/' {
        let drive = (bytes[1] as char).to_ascii_uppercase();
        return format!("{drive}:\\{}", s[3..].replace('/', "\\"));
    }
    // `/mnt/c/rest` → `C:\rest`
    if let Some(rest) = s.strip_prefix("/mnt/")
        && rest.len() >= 3
    {
        let rb = rest.as_bytes();
        if rb[0].is_ascii_alphabetic() && rb[1] == b'/' {
            let drive = (rb[0] as char).to_ascii_uppercase();
            return format!("{drive}:\\{}", rest[2..].replace('/', "\\"));
        }
    }
    s.to_string()
}

/// Resolve a model-supplied path against the session cwd: [`normalize`],
/// then join onto `cwd` when the result is relative (pi's `resolveToCwd`).
pub fn resolve(cwd: &Path, raw: &str) -> PathBuf {
    let path = normalize(raw);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// How the tools print a path: relative to `cwd` with `/` separators when it
/// is inside the cwd, `.` for the cwd itself, and an absolute `/`-separated
/// path otherwise (pi's `formatPathRelativeToCwdOrAbsolute`).
///
/// Not used yet: grep and find print their hits this way, and they land
/// after ls/read in this series.
#[allow(dead_code)]
pub fn display(cwd: &Path, path: &Path) -> String {
    let (cwd, path) = (clean(cwd), clean(path));
    match path.strip_prefix(&cwd) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

/// Fold `.` and `..` lexically (no filesystem access), so `display` cannot be
/// fooled into calling `cwd/../../etc` "inside the cwd". A `..` that would
/// climb past the root is dropped, as `path.resolve` would.
#[allow(dead_code)] // reached only through `display`
fn clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !path.is_absolute() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        dirs::home_dir().expect("a home directory is required by these tests")
    }

    #[test]
    fn one_leading_at_is_stripped() {
        assert_eq!(normalize("@notes.txt"), PathBuf::from("notes.txt"));
        assert_eq!(normalize("@@notes.txt"), PathBuf::from("@notes.txt"));
        // Only leading, and only one.
        assert_eq!(normalize("a@b.txt"), PathBuf::from("a@b.txt"));
    }

    #[test]
    fn unicode_spaces_become_plain_spaces() {
        assert_eq!(normalize("my\u{a0}file.txt"), PathBuf::from("my file.txt"));
        assert_eq!(normalize("a\u{3000}b"), PathBuf::from("a b"));
        // Ordinary spaces and ordinary text are untouched.
        assert_eq!(normalize("a b"), PathBuf::from("a b"));
        assert_eq!(normalize("中文/路径"), PathBuf::from("中文/路径"));
    }

    #[test]
    fn tilde_expands_to_the_home_directory() {
        assert_eq!(normalize("~"), home());
        assert_eq!(normalize("~/notes.txt"), home().join("notes.txt"));
        // A tilde that is not the first segment is just a character.
        assert_eq!(normalize("a~b"), PathBuf::from("a~b"));
        assert_eq!(normalize("~tilde"), PathBuf::from("~tilde"));
    }

    #[test]
    fn relative_paths_join_the_cwd_and_absolute_ones_do_not() {
        let cwd = Path::new("/work/project");
        assert_eq!(resolve(cwd, "src/main.rs"), cwd.join("src/main.rs"));
        assert_eq!(resolve(cwd, "./src"), cwd.join("src"));
        assert_eq!(resolve(cwd, "@src/lib.rs"), cwd.join("src/lib.rs"));
        let absolute = if cfg!(windows) {
            "C:\\other\\x"
        } else {
            "/other/x"
        };
        assert_eq!(resolve(cwd, absolute), PathBuf::from(absolute));
    }

    #[test]
    fn display_is_relative_inside_the_cwd_with_forward_slashes() {
        let cwd = if cfg!(windows) {
            Path::new("C:\\work\\project")
        } else {
            Path::new("/work/project")
        };
        assert_eq!(
            display(cwd, &cwd.join("src").join("main.rs")),
            "src/main.rs"
        );
        assert_eq!(display(cwd, cwd), ".");
    }

    #[test]
    fn display_is_absolute_outside_the_cwd() {
        let cwd = if cfg!(windows) {
            Path::new("C:\\work\\project")
        } else {
            Path::new("/work/project")
        };
        let outside = if cfg!(windows) {
            Path::new("D:\\elsewhere\\x.txt")
        } else {
            Path::new("/elsewhere/x.txt")
        };
        assert_eq!(
            display(cwd, outside),
            outside.to_string_lossy().replace('\\', "/")
        );
    }

    #[test]
    fn display_does_not_call_a_traversal_path_inside_the_cwd() {
        let cwd = if cfg!(windows) {
            Path::new("C:\\work\\project")
        } else {
            Path::new("/work/project")
        };
        let escaping = cwd.join("..").join("..").join("etc").join("passwd");
        let shown = display(cwd, &escaping);
        assert!(shown.ends_with("etc/passwd"), "{shown}");
        assert_ne!(shown, "etc/passwd");
    }

    #[cfg(windows)]
    #[test]
    fn shell_drive_paths_become_windows_paths() {
        assert_eq!(
            normalize("/c/Users/me/x.txt"),
            PathBuf::from("C:\\Users\\me\\x.txt")
        );
        assert_eq!(normalize("/mnt/d/work"), PathBuf::from("D:\\work"));
        // Not drive-shaped: left alone.
        assert_eq!(normalize("/usr/local"), PathBuf::from("/usr/local"));
        assert_eq!(normalize("/mnt/notadrive"), PathBuf::from("/mnt/notadrive"));
    }
}
