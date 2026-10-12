//! What a path in the mount is, for recording (design §5).
//!
//! Every path the FUSE layer is about to record is classified first:
//!
//! - [`Class::Record`]: a valid repository path, handed to the recorder
//!   (which still applies the path rules and checks of design §4 and §3.4);
//! - [`Class::InGitDir`]: a path with a component that is exactly `.git`.
//!   It is never recorded (git cannot store it). Whether it raises an alarm
//!   depends on what that `.git` is, which only the backing tree can tell
//!   ([`crate::boundary`]): git's own data (a `git init` at the mount root,
//!   a nested clone's git directory, a gitfile) is skipped silently, since
//!   `git clone` on the mount would otherwise raise one alarm per object;
//!   anything else named `.git` raises `PathRefused`;
//! - [`Class::Invalid`]: anything else git cannot store: a `.git`
//!   look-alike (`.GIT`, `git~1`, `.git.`), a name that is not UTF-8, or an
//!   empty, `.` or `..` component. These are not recorded and raise
//!   `PathRefused` ([`agentfs_core::volume::Recorder::refuse_invalid`]).

use agentfs_core::paths::{PathError, RepoPath};

/// How a path is treated when recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    Record(RepoPath),
    /// Under (or at) `dir/.git`, the first `.git` component; `dir` is empty
    /// for the root.
    InGitDir {
        dir: Vec<u8>,
    },
    Invalid {
        raw: String,
        error: PathError,
    },
}

/// Classifies `rel`, a `/`-separated path relative to the mount root, as
/// raw bytes from the kernel.
pub fn classify(rel: &[u8]) -> Class {
    let components: Vec<&[u8]> = rel.split(|b| *b == b'/').collect();
    if let Some(i) = components.iter().position(|c| *c == b".git") {
        return Class::InGitDir {
            dir: components[..i].join(&b'/'),
        };
    }
    let raw = String::from_utf8_lossy(rel).into_owned();
    match std::str::from_utf8(rel) {
        Err(_) => Class::Invalid {
            error: PathError::NotUtf8(raw.clone()),
            raw,
        },
        Ok(text) => match RepoPath::new(text) {
            Ok(path) => Class::Record(path),
            Err(error) => Class::Invalid { raw, error },
        },
    }
}

/// True if `name` (one component) is exactly `.git`: a directory the
/// directory walk of a rename never descends into.
pub fn is_git_dir_name(name: &[u8]) -> bool {
    name == b".git"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(rel: &str) {
        assert_eq!(
            classify(rel.as_bytes()),
            Class::Record(RepoPath::new(rel).unwrap()),
            "{rel}"
        );
    }

    fn invalid(rel: &[u8]) -> PathError {
        match classify(rel) {
            Class::Invalid { error, .. } => error,
            other => panic!("{rel:?}: {other:?}"),
        }
    }

    fn in_git_dir(dir: &[u8]) -> Class {
        Class::InGitDir { dir: dir.to_vec() }
    }

    #[test]
    fn ordinary_paths_are_recorded() {
        record("a");
        record("a/b");
        record("dir/.gitignore");
        record("sub/.github/workflows/ci.yml");
        record(".gitx");
    }

    #[test]
    fn git_internals_name_their_git_dir() {
        for rel in [".git", ".git/HEAD", ".git/objects/ab/cd"] {
            assert_eq!(classify(rel.as_bytes()), in_git_dir(b""), "{rel}");
        }
        assert_eq!(classify(b"sub/.git/HEAD"), in_git_dir(b"sub"));
        assert_eq!(classify(b"a/b/.git"), in_git_dir(b"a/b"));
        // The first .git counts.
        assert_eq!(classify(b"a/.git/x/.git/y"), in_git_dir(b"a"));
        // Even when another component is not UTF-8.
        assert_eq!(classify(b"\xff/.git/x"), in_git_dir(b"\xff"));
    }

    #[test]
    fn look_alikes_and_bad_names_are_invalid() {
        assert!(matches!(invalid(b"sub/.GIT/x"), PathError::DotGit(_)));
        assert!(matches!(invalid(b"git~1/x"), PathError::DotGit(_)));
        assert!(matches!(invalid(b".git./x"), PathError::DotGit(_)));
        assert!(matches!(invalid(b"a/\xff\xfe"), PathError::NotUtf8(_)));
        assert!(matches!(invalid(b""), PathError::Empty));
        assert!(matches!(invalid(b".."), PathError::BadComponent(_)));
        assert!(matches!(invalid(b"a//b"), PathError::BadComponent(_)));
    }

    #[test]
    fn git_dir_names() {
        assert!(is_git_dir_name(b".git"));
        assert!(!is_git_dir_name(b".GIT"));
        assert!(!is_git_dir_name(b"git"));
    }
}
