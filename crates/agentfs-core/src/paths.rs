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
/// (`gitmod~1` to `~4`, or the hashed fallback: a prefix of up to six
/// characters of `gi7eba`, `~`, a digit 1-9 and more digits, eight
/// characters in all, so `gi7eba~9`, `gi7eb~10`, `g~123456`, and even
/// `~1234567`, which names all four files).
pub fn special_file(component: &str) -> Option<SpecialFile> {
    let stripped: String = component
        .chars()
        .filter(|c| !is_hfs_ignorable(*c))
        .collect();
    // (file, name without its dot, hashed 8.3 fallback prefix)
    const NAMES: [(SpecialFile, &str, &str); 4] = [
        (SpecialFile::Gitmodules, "gitmodules", "gi7eba"),
        (SpecialFile::Gitattributes, "gitattributes", "gi7d29"),
        (SpecialFile::Gitignore, "gitignore", "gi250a"),
        (SpecialFile::Mailmap, "mailmap", "maba30"),
    ];
    NAMES.into_iter().find_map(|(file, name, hashed)| {
        is_ntfs_dot_generic(stripped.as_bytes(), name.as_bytes(), hashed.as_bytes()).then_some(file)
    })
}

/// A port of git's `is_ntfs_dot_generic` (path.c). `dotgit_name` is the
/// file name without its leading dot (at least six bytes), and
/// `shortname_prefix` the lower-case six-byte hashed 8.3 fallback prefix.
fn is_ntfs_dot_generic(name: &[u8], dotgit_name: &[u8], shortname_prefix: &[u8]) -> bool {
    // After the name proper only spaces and dots may follow, up to the end
    // or an NTFS stream separator.
    let only_spaces_and_periods = |rest: &[u8]| {
        for &c in rest {
            match c {
                b':' => return true,
                b' ' | b'.' => {}
                _ => return false,
            }
        }
        true
    };
    let len = dotgit_name.len();
    if name.first() == Some(&b'.')
        && name.len() > len
        && name[1..=len].eq_ignore_ascii_case(dotgit_name)
    {
        return only_spaces_and_periods(&name[len + 1..]);
    }
    // The regular short name: the first six characters, then `~1` to `~4`.
    if name.len() >= 8
        && name[..6].eq_ignore_ascii_case(&dotgit_name[..6])
        && name[6] == b'~'
        && (b'1'..=b'4').contains(&name[7])
    {
        return only_spaces_and_periods(&name[8..]);
    }
    // The fall-back short name. git reads a NUL at the end of the name.
    let at = |i: usize| name.get(i).copied().unwrap_or(0);
    let mut saw_tilde = false;
    let mut i = 0;
    while i < 8 {
        let c = at(i);
        if c == 0 {
            return false;
        } else if saw_tilde {
            if !c.is_ascii_digit() {
                return false;
            }
        } else if c == b'~' {
            i += 1;
            if !(b'1'..=b'9').contains(&at(i)) {
                return false;
            }
            saw_tilde = true;
        } else if i >= 6 || !c.is_ascii() || c.to_ascii_lowercase() != shortname_prefix[i] {
            return false;
        }
        i += 1;
    }
    only_spaces_and_periods(name.get(i..).unwrap_or_default())
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
            ("gi7eb~10", SpecialFile::Gitmodules),
            ("gi7e~123", SpecialFile::Gitmodules),
            ("GI7~1234", SpecialFile::Gitmodules),
            ("g~123456", SpecialFile::Gitmodules),
            ("gi250~99. ", SpecialFile::Gitignore),
            ("maba3~10:x", SpecialFile::Mailmap),
            // Names all four files; the first is reported.
            ("~1234567", SpecialFile::Gitmodules),
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
            "gi7eba~10",
            "gi7eba~",
            "gi7eb~0",
            "gi7eb~1x",
            "gi7eb~1",
            "g~12345",
            "~123456789",
            "~0123456",
            "gi7eb\u{e9}~1",
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
