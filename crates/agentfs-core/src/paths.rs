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

/// Files git gives meaning to by name, which its fsck refuses as symlinks
/// (`gitmodulesSymlink`, `gitattributesSymlink`, `gitignoreSymlink`,
/// `mailmapSymlink`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpecialFile {
    Gitmodules,
    Gitattributes,
    Gitignore,
    Mailmap,
}

impl SpecialFile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gitmodules => ".gitmodules",
            Self::Gitattributes => ".gitattributes",
            Self::Gitignore => ".gitignore",
            Self::Mailmap => ".mailmap",
        }
    }
}

impl fmt::Display for SpecialFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which special file `component` names, in any spelling git's fsck treats
/// as that file on NTFS or HFS+ (git's `is_ntfs_dot_generic` and
/// `is_hfs_dot_generic`): any case, HFS+ ignorable code points, trailing
/// spaces and dots, an NTFS stream suffix (`:x`), and the 8.3 short names
/// (`gitmod~1` to `~4`, or the hashed fallback `gi7eba~1` to `~9`).
pub fn special_file(component: &str) -> Option<SpecialFile> {
    let stripped: String = component
        .chars()
        .filter(|c| !is_hfs_ignorable(*c))
        .collect();
    let stream_cut = stripped.split(':').next().unwrap_or_default();
    let name = stream_cut.trim_end_matches([' ', '.']).to_ascii_lowercase();
    // (file, full name, 8.3 prefix taking ~1-4, hashed 8.3 prefix taking ~1-9)
    const NAMES: [(SpecialFile, &str, &str, &str); 4] = [
        (SpecialFile::Gitmodules, ".gitmodules", "gitmod", "gi7eba"),
        (
            SpecialFile::Gitattributes,
            ".gitattributes",
            "gitatt",
            "gi7d29",
        ),
        (SpecialFile::Gitignore, ".gitignore", "gitign", "gi250a"),
        (SpecialFile::Mailmap, ".mailmap", "mailma", "maba30"),
    ];
    let short_name = |prefix: &str, max: u8| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('~'))
            .is_some_and(|n| matches!(n.as_bytes(), [d] if (b'1'..=max).contains(d)))
    };
    NAMES.into_iter().find_map(|(file, full, short, hashed)| {
        (name == full || short_name(short, b'4') || short_name(hashed, b'9')).then_some(file)
    })
}

/// True for names git expects to be regular files, never symlinks, in any
/// spelling [`special_file`] recognises. git's fsck rejects these as
/// symlinks, and refusing them also keeps tracked rule files readable
/// content.
pub fn must_not_be_symlink(path: &RepoPath) -> bool {
    special_file(path.file_name()).is_some()
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
    fn special_file_spellings() {
        for (name, file) in [
            (".gitmodules", SpecialFile::Gitmodules),
            (".GITMODULES", SpecialFile::Gitmodules),
            (".gitmodules.", SpecialFile::Gitmodules),
            (".gitmodules . ", SpecialFile::Gitmodules),
            (".gitmodules:$DATA", SpecialFile::Gitmodules),
            ("gitmod~4", SpecialFile::Gitmodules),
            ("GI7EBA~9", SpecialFile::Gitmodules),
            (".g\u{200c}itmodules", SpecialFile::Gitmodules),
            ("\u{feff}.gitattributes", SpecialFile::Gitattributes),
            ("gitatt~1", SpecialFile::Gitattributes),
            ("gi7d29~2", SpecialFile::Gitattributes),
            (".GitIgnore", SpecialFile::Gitignore),
            ("gitign~3", SpecialFile::Gitignore),
            ("gi250a~1", SpecialFile::Gitignore),
            (".mailmap", SpecialFile::Mailmap),
            ("mailma~2", SpecialFile::Mailmap),
            ("maba30~5", SpecialFile::Mailmap),
        ] {
            assert_eq!(special_file(name), Some(file), "{name:?}");
        }
        for name in [
            "gitmodules",
            ".gitmodulesx",
            "x.gitmodules",
            "gitmod~5",
            "gitmod~0",
            "gitmod~10",
            "gi7eba~0",
            "gitmod1",
            ".mailmap2",
            "",
            ".",
        ] {
            assert_eq!(special_file(name), None, "{name:?}");
        }
    }

    #[test]
    fn symlink_rule_files() {
        for p in [
            "x/.gitmodules",
            ".GITATTRIBUTES",
            "d/gitign~1",
            ".mailmap.",
            ".gitmodules:x",
            ".g\u{200d}itignore",
        ] {
            assert!(must_not_be_symlink(&RepoPath::new(p).unwrap()), "{p:?}");
        }
        assert!(!must_not_be_symlink(&RepoPath::new("x/link").unwrap()));
        assert!(!must_not_be_symlink(
            &RepoPath::new(".gitmodules/inner").unwrap()
        ));
    }
}
