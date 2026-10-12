//! Repository-relative paths, and the names git refuses to store.
//!
//! Every recorded path is checked here before it reaches the journal, so a
//! commit can never contain a path that git's fsck (and therefore a remote)
//! would reject. See design §3.4 and §5.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why a path cannot be recorded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path is empty")]
    Empty,
    #[error("path {0:?} is not relative")]
    Absolute(String),
    #[error("path {0:?} has an empty, `.` or `..` component")]
    BadComponent(String),
    #[error("path {0:?} contains a NUL byte")]
    Nul(String),
    #[error("path {0:?} is not valid UTF-8")]
    NotUtf8(String),
    #[error("path {0:?} has a component git treats as `.git`")]
    DotGit(String),
}

/// A validated, `/`-separated path relative to the root of the mount.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoPath(String);

impl RepoPath {
    pub fn new(path: impl Into<String>) -> Result<Self, PathError> {
        let path = path.into();
        validate(&path)?;
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path's components, root first.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Final component.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for RepoPath {
    type Error = PathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<RepoPath> for String {
    fn from(value: RepoPath) -> Self {
        value.0
    }
}

fn validate(path: &str) -> Result<(), PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if path.starts_with('/') {
        return Err(PathError::Absolute(path.to_owned()));
    }
    if path.contains('\0') {
        return Err(PathError::Nul(path.to_owned()));
    }
    for component in path.split('/') {
        if matches!(component, "" | "." | "..") {
            return Err(PathError::BadComponent(path.to_owned()));
        }
        if is_dotgit(component) {
            return Err(PathError::DotGit(path.to_owned()));
        }
    }
    Ok(())
}

/// True if git's fsck would treat `component` as `.git`, on any platform.
///
/// git checks the NTFS and HFS+ spellings on every platform when it receives
/// objects, so they are rejected here even on Linux: `.GIT`, `.git.`,
/// `.git ` (trailing dots and spaces), the 8.3 short name `git~1`, names with
/// HFS+ ignorable code points (`.g\u{200c}it`), and backslash-separated parts
/// (`foo\.git`).
pub fn is_dotgit(component: &str) -> bool {
    component.split('\\').any(|part| {
        let stripped: String = part.chars().filter(|c| !is_hfs_ignorable(*c)).collect();
        let trimmed = stripped.trim_end_matches([' ', '.']).to_ascii_lowercase();
        trimmed == ".git" || trimmed == "git~1"
    })
}

/// Code points HFS+ ignores when comparing names.
fn is_hfs_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}'
    )
}

/// True for names git expects to be regular files, never symlinks. git's
/// fsck rejects a symlinked `.gitmodules`; the others are refused here too so
/// tracked rule files are always readable content.
pub fn must_not_be_symlink(path: &RepoPath) -> bool {
    matches!(
        path.file_name().to_ascii_lowercase().as_str(),
        ".gitmodules" | ".gitattributes" | ".gitignore" | ".mailmap"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_paths() {
        for p in [
            "a",
            "a/b.txt",
            ".gitignore",
            "dir/.gitattributes",
            "git",
            ".github/x",
        ] {
            assert!(RepoPath::new(p).is_ok(), "{p}");
        }
    }

    #[test]
    fn rejects_malformed_paths() {
        assert_eq!(RepoPath::new(""), Err(PathError::Empty));
        assert!(matches!(RepoPath::new("/a"), Err(PathError::Absolute(_))));
        for p in ["a//b", "a/", "./a", "a/../b", "a/."] {
            assert!(
                matches!(RepoPath::new(p), Err(PathError::BadComponent(_))),
                "{p}"
            );
        }
        assert!(matches!(RepoPath::new("a\0b"), Err(PathError::Nul(_))));
    }

    #[test]
    fn rejects_every_spelling_of_dotgit() {
        for p in [
            ".git",
            "a/.git/config",
            ".GIT",
            ".Git/x",
            ".git.",
            ".git ",
            ".git. .",
            "git~1",
            "GIT~1/x",
            ".g\u{200c}it",
            "\u{feff}.git",
            "foo\\.git",
        ] {
            assert!(
                matches!(RepoPath::new(p), Err(PathError::DotGit(_))),
                "{p:?}"
            );
        }
    }

    #[test]
    fn serde_round_trip_validates() {
        let p: RepoPath = serde_json::from_str("\"a/b\"").unwrap();
        assert_eq!(p.as_str(), "a/b");
        assert!(serde_json::from_str::<RepoPath>("\"a/.git\"").is_err());
    }

    #[test]
    fn symlink_rule_files() {
        assert!(must_not_be_symlink(
            &RepoPath::new("x/.gitmodules").unwrap()
        ));
        assert!(!must_not_be_symlink(&RepoPath::new("x/link").unwrap()));
    }
}
