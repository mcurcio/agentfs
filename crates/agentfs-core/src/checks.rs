//! Checks before a path is recorded and committed (design §3.4).
//!
//! A commit no remote accepts can never be pushed, so every path is checked
//! against git's fsck rules and the remotes' limits before it is journaled
//! (at record time, the gate) and again before it is committed (a backstop
//! for records journaled before the limits changed). A path that fails is
//! not recorded; its earlier content, if any, stays in history.
//!
//! Rules, beyond those [`crate::paths::RepoPath`] already enforces:
//!
//! | Rule | Threshold | git fsck id |
//! |---|---|---|
//! | Path length | over [`MAX_PATH_BYTES`] | none (agentfs's own: `PATH_MAX` at checkout) |
//! | Symlinked `.gitmodules`, `.gitattributes`, `.gitignore`, `.mailmap`, any NTFS/HFS+ spelling | | `gitmodulesSymlink` and siblings |
//! | Symlink target length | over [`MAX_SYMLINK_TARGET`] | `symlinkTargetLength` |
//! | Symlink target with a `.git` component | | `symlinkPointsToGitDir` |
//! | `.gitattributes` size | over [`ATTR_MAX_FILE_SIZE`] | `gitattributesLarge` |
//! | `.gitattributes` line length | [`ATTR_MAX_LINE_LENGTH`] or more | `gitattributesLineLength` |
//! | `.gitmodules` that git's config parser cannot read | | `gitmodulesParse` |
//! | `.gitmodules` submodule name empty or with a `..` component | | `gitmodulesName` |
//! | `.gitmodules` url an option, with a decoded newline, escaping its root, or invalid for git-remote-curl | | `gitmodulesUrl` |
//! | `.gitmodules` path starting with `-` | | `gitmodulesPath` |
//! | `.gitmodules` update starting with `!` (a command) | | `gitmodulesUpdate` |
//! | `.gitmodules` size | over [`GITMODULES_MAX_SIZE`] | `gitmodulesLarge` |
//! | Blob size | over the smallest `remote.<name>.agentfsMaxBlobSize` | none (remote policy) |
//!
//! There is one history for every remote, so the blob limit that applies is
//! the smallest across remotes. Checks are pure: they read only the frozen
//! copy they are given, so one [`Checks`] is shared by the recorder's
//! threads and the committer.
//!
//! **Bounded synchronous step.** Size checks are a `stat` and always run.
//! Content scans (the `.gitattributes` line length, the `.gitmodules` rules
//! of [`crate::gitmodules`]) run in the synchronous
//! step ([`CheckStage::Record`]) only for files up to [`SYNC_SCAN_LIMIT`],
//! the bound tree rule files already have (design §4.1); a larger file is
//! scanned in full at commit time ([`CheckStage::Commit`]), where a refusal
//! drops the change and raises `PathRefused` with stage `commit`.

use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use crate::config::RemoteSettings;
use crate::events::CheckStage;
use crate::gitmodules;
use crate::paths::{PathError, RepoPath, SpecialFile, is_dotgit, special_file};

/// Longest path recorded, in bytes.
pub const MAX_PATH_BYTES: usize = 4096;
/// Longest symlink target git's fsck accepts (`PATH_MAX`).
pub const MAX_SYMLINK_TARGET: usize = 4096;
/// Largest `.gitattributes` git's fsck accepts.
pub const ATTR_MAX_FILE_SIZE: u64 = 100 << 20;
/// A `.gitattributes` line this long or longer is refused by git's fsck.
pub const ATTR_MAX_LINE_LENGTH: usize = 2048;
/// The largest file whose content is scanned in the synchronous step
/// ([`CheckStage::Record`]); larger ones are scanned at commit time. The
/// same bound as tree rule files (design §4.1).
pub const SYNC_SCAN_LIMIT: u64 = 1 << 20;
/// Largest `.gitmodules` git's fsck parses: above `core.bigFileThreshold`
/// (512 MiB by default) it reports `gitmodulesLarge`.
pub const GITMODULES_MAX_SIZE: u64 = 512 << 20;

/// The blob size limit in force, and the remote it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobLimit {
    pub bytes: u64,
    pub remote: String,
}

/// Limits from the configured remotes (design §11.3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Limits {
    pub max_blob_size: Option<BlobLimit>,
}

impl Limits {
    /// The strictest limits across `remotes`. Ties go to the first remote
    /// by name.
    pub fn from_remotes(remotes: &[RemoteSettings]) -> Self {
        let max_blob_size = remotes
            .iter()
            .filter_map(|r| r.max_blob_size.map(|bytes| (bytes, &r.name)))
            .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)))
            .map(|(bytes, remote)| BlobLimit {
                bytes,
                remote: remote.clone(),
            });
        Self { max_blob_size }
    }
}

/// Why a path is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The path itself is not one git can store (for example a `.git`
    /// component); reported by the caller that could not build a
    /// [`RepoPath`].
    InvalidPath(PathError),
    PathTooLong {
        bytes: usize,
    },
    SymlinkedSpecialFile {
        file: SpecialFile,
    },
    SymlinkTargetTooLong {
        bytes: usize,
    },
    SymlinkIntoGitDir {
        target: String,
    },
    GitattributesTooLarge {
        bytes: u64,
    },
    GitattributesLineTooLong {
        /// 1-based.
        line: usize,
        bytes: usize,
    },
    BlobTooLarge {
        bytes: u64,
        limit: u64,
        remote: String,
    },
    /// git's config parser cannot read the `.gitmodules`
    /// (`gitmodulesParse`, which git only reports; agentfs refuses it, as
    /// it does the other `.gitmodules` rules, design §3.4).
    GitmodulesParse {
        line: usize,
    },
    GitmodulesName {
        name: String,
    },
    GitmodulesUrl {
        url: String,
    },
    GitmodulesPath {
        path: String,
    },
    GitmodulesUpdate {
        value: String,
    },
    GitmodulesTooLarge {
        bytes: u64,
    },
}

impl Refusal {
    /// A stable label for logs and metrics.
    pub fn label(&self) -> &'static str {
        match self {
            Self::InvalidPath(_) => "invalid_path",
            Self::PathTooLong { .. } => "path_too_long",
            Self::SymlinkedSpecialFile { .. } => "symlinked_special_file",
            Self::SymlinkTargetTooLong { .. } => "symlink_target_too_long",
            Self::SymlinkIntoGitDir { .. } => "symlink_into_git_dir",
            Self::GitattributesTooLarge { .. } => "gitattributes_too_large",
            Self::GitattributesLineTooLong { .. } => "gitattributes_line_too_long",
            Self::BlobTooLarge { .. } => "blob_too_large",
            Self::GitmodulesParse { .. } => "gitmodules_parse",
            Self::GitmodulesName { .. } => "gitmodules_name",
            Self::GitmodulesUrl { .. } => "gitmodules_url",
            Self::GitmodulesPath { .. } => "gitmodules_path",
            Self::GitmodulesUpdate { .. } => "gitmodules_update",
            Self::GitmodulesTooLarge { .. } => "gitmodules_too_large",
        }
    }

    /// The git fsck message id this rule mirrors, if there is one.
    pub fn fsck_id(&self) -> Option<&'static str> {
        match self {
            Self::InvalidPath(PathError::DotGit(_)) => Some("hasDotgit"),
            Self::InvalidPath(_) | Self::PathTooLong { .. } | Self::BlobTooLarge { .. } => None,
            Self::SymlinkedSpecialFile { file } => Some(match file {
                SpecialFile::Gitmodules => "gitmodulesSymlink",
                SpecialFile::Gitattributes => "gitattributesSymlink",
                SpecialFile::Gitignore => "gitignoreSymlink",
                SpecialFile::Mailmap => "mailmapSymlink",
            }),
            Self::SymlinkTargetTooLong { .. } => Some("symlinkTargetLength"),
            Self::SymlinkIntoGitDir { .. } => Some("symlinkPointsToGitDir"),
            Self::GitattributesTooLarge { .. } => Some("gitattributesLarge"),
            Self::GitattributesLineTooLong { .. } => Some("gitattributesLineLength"),
            Self::GitmodulesParse { .. } => Some("gitmodulesParse"),
            Self::GitmodulesName { .. } => Some("gitmodulesName"),
            Self::GitmodulesUrl { .. } => Some("gitmodulesUrl"),
            Self::GitmodulesPath { .. } => Some("gitmodulesPath"),
            Self::GitmodulesUpdate { .. } => Some("gitmodulesUpdate"),
            Self::GitmodulesTooLarge { .. } => Some("gitmodulesLarge"),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(e) => write!(f, "{e}"),
            Self::PathTooLong { bytes } => {
                write!(f, "path is {bytes} bytes, over {MAX_PATH_BYTES}")
            }
            Self::SymlinkedSpecialFile { file } => {
                write!(f, "{file} must be a regular file, not a symlink")
            }
            Self::SymlinkTargetTooLong { bytes } => {
                write!(
                    f,
                    "symlink target is {bytes} bytes, over {MAX_SYMLINK_TARGET}"
                )
            }
            Self::SymlinkIntoGitDir { target } => {
                write!(f, "symlink target {target:?} points into a .git directory")
            }
            Self::GitattributesTooLarge { bytes } => {
                write!(
                    f,
                    ".gitattributes is {bytes} bytes, over {ATTR_MAX_FILE_SIZE}"
                )
            }
            Self::GitattributesLineTooLong { line, bytes } => write!(
                f,
                ".gitattributes line {line} is {bytes} bytes (limit {})",
                ATTR_MAX_LINE_LENGTH - 1
            ),
            Self::BlobTooLarge {
                bytes,
                limit,
                remote,
            } => write!(
                f,
                "{bytes} bytes is over the {limit}-byte limit of remote {remote}"
            ),
            Self::GitmodulesParse { line } => {
                write!(f, ".gitmodules cannot be parsed by git (line {line})")
            }
            Self::GitmodulesName { name } => {
                write!(f, ".gitmodules has a disallowed submodule name {name:?}")
            }
            Self::GitmodulesUrl { url } => {
                write!(f, ".gitmodules has a disallowed submodule url {url:?}")
            }
            Self::GitmodulesPath { path } => {
                write!(f, ".gitmodules has a disallowed submodule path {path:?}")
            }
            Self::GitmodulesUpdate { value } => write!(
                f,
                ".gitmodules has a disallowed submodule update setting {value:?}"
            ),
            Self::GitmodulesTooLarge { bytes } => {
                write!(
                    f,
                    ".gitmodules is {bytes} bytes, over {GITMODULES_MAX_SIZE}"
                )
            }
        }
    }
}

/// The pre-commit checks, for one set of limits.
#[derive(Debug, Clone, Default)]
pub struct Checks {
    limits: Limits,
}

impl Checks {
    pub fn new(limits: Limits) -> Self {
        Self { limits }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Rules on the path alone.
    pub fn check_path(&self, path: &RepoPath) -> Option<Refusal> {
        let bytes = path.as_str().len();
        (bytes > MAX_PATH_BYTES).then_some(Refusal::PathTooLong { bytes })
    }

    /// Rules for a symlink at `path` pointing to `target`.
    pub fn check_symlink(&self, path: &RepoPath, target: &str) -> Option<Refusal> {
        if let Some(refusal) = self.check_path(path) {
            return Some(refusal);
        }
        if let Some(file) = special_file(path.file_name()) {
            return Some(Refusal::SymlinkedSpecialFile { file });
        }
        if target.len() > MAX_SYMLINK_TARGET {
            return Some(Refusal::SymlinkTargetTooLong {
                bytes: target.len(),
            });
        }
        // `is_dotgit` also splits on `\`, as git's fsck does for targets.
        if target.split('/').any(is_dotgit) {
            return Some(Refusal::SymlinkIntoGitDir {
                target: target.to_owned(),
            });
        }
        self.check_blob_size(target.len() as u64)
    }

    /// Rules for a regular file at `path` whose content is the frozen copy
    /// at `content`, at `stage`. Reads only special files, and in the
    /// synchronous step ([`CheckStage::Record`]) only those up to
    /// [`SYNC_SCAN_LIMIT`]; at [`CheckStage::Commit`] they are read in full
    /// (bounded by their size limits, checked first by length alone).
    pub fn check_file(
        &self,
        path: &RepoPath,
        content: &Path,
        stage: CheckStage,
    ) -> io::Result<Option<Refusal>> {
        if let Some(refusal) = self.check_path(path) {
            return Ok(Some(refusal));
        }
        let bytes = std::fs::metadata(content)?.len();
        if let Some(refusal) = self
            .check_blob_size(bytes)
            .or_else(|| special_size(path, bytes))
        {
            return Ok(Some(refusal));
        }
        if special_file(path.file_name()).is_none()
            || (stage == CheckStage::Record && bytes > SYNC_SCAN_LIMIT)
        {
            return Ok(None);
        }
        scan_content(path, BufReader::new(File::open(content)?))
    }

    /// Every rule for a regular file at `path` holding `bytes`, as at
    /// commit time (content scans run in full). A restore checks content
    /// with this before writing it back (design §15).
    pub fn check_blob(&self, path: &RepoPath, bytes: &[u8]) -> Option<Refusal> {
        let len = bytes.len() as u64;
        self.check_path(path)
            .or_else(|| self.check_blob_size(len))
            .or_else(|| special_size(path, len))
            .or_else(|| scan_content(path, bytes).unwrap_or(None))
    }

    /// The size limit on the live backing file, before it is frozen, so a
    /// file far over the limit is never copied (design §3.2). Advisory: the
    /// file can change before the freeze, so [`Checks::check_file`] on the
    /// frozen copy stays the authoritative check. A file that cannot be
    /// stat'ed passes here and fails in the freeze.
    pub fn check_backing_size(&self, backing: &Path) -> Option<Refusal> {
        self.limits.max_blob_size.as_ref()?;
        let bytes = std::fs::metadata(backing).ok()?.len();
        self.check_blob_size(bytes)
    }

    fn check_blob_size(&self, bytes: u64) -> Option<Refusal> {
        let limit = self.limits.max_blob_size.as_ref()?;
        (bytes > limit.bytes).then(|| Refusal::BlobTooLarge {
            bytes,
            limit: limit.bytes,
            remote: limit.remote.clone(),
        })
    }
}

/// The size rules of special files, which need only their length.
fn special_size(path: &RepoPath, bytes: u64) -> Option<Refusal> {
    match special_file(path.file_name()) {
        Some(SpecialFile::Gitattributes) if bytes > ATTR_MAX_FILE_SIZE => {
            Some(Refusal::GitattributesTooLarge { bytes })
        }
        Some(SpecialFile::Gitmodules) if bytes > GITMODULES_MAX_SIZE => {
            Some(Refusal::GitmodulesTooLarge { bytes })
        }
        _ => None,
    }
}

/// The content rules of special files, over `reader`.
fn scan_content(path: &RepoPath, reader: impl BufRead) -> io::Result<Option<Refusal>> {
    match special_file(path.file_name()) {
        Some(SpecialFile::Gitattributes) => long_attribute_line(reader),
        Some(SpecialFile::Gitmodules) => {
            let mut content = Vec::new();
            let mut reader = reader;
            reader.read_to_end(&mut content)?;
            Ok(gitmodules_refusal(&content))
        }
        _ => Ok(None),
    }
}

/// git fsck's `.gitmodules` content rules ([`crate::gitmodules`]).
fn gitmodules_refusal(content: &[u8]) -> Option<Refusal> {
    use crate::gitmodules::Problem;
    let problem = gitmodules::check(content).err()?;
    Some(match problem {
        Problem::Parse { line } => Refusal::GitmodulesParse { line },
        Problem::Name(name) => Refusal::GitmodulesName { name },
        Problem::Url(url) => Refusal::GitmodulesUrl { url },
        Problem::Path(path) => Refusal::GitmodulesPath { path },
        Problem::Update(value) => Refusal::GitmodulesUpdate { value },
    })
}

/// The first line of [`ATTR_MAX_LINE_LENGTH`] bytes or more (excluding its
/// `\n`), as git's fsck measures it.
fn long_attribute_line(mut reader: impl BufRead) -> io::Result<Option<Refusal>> {
    let mut line = 0;
    loop {
        line += 1;
        let mut len = 0;
        let mut ended = false;
        loop {
            let buf = reader.fill_buf()?;
            if buf.is_empty() {
                break;
            }
            let (used, found) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (buf.len(), false),
            };
            len += used - usize::from(found);
            reader.consume(used);
            if found {
                ended = true;
                break;
            }
        }
        if len >= ATTR_MAX_LINE_LENGTH {
            return Ok(Some(Refusal::GitattributesLineTooLong { line, bytes: len }));
        }
        if !ended {
            return Ok(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Instant;

    use super::*;
    use CheckStage::{Commit, Record};

    fn p(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn remote(name: &str, max: Option<u64>) -> RemoteSettings {
        RemoteSettings {
            name: name.into(),
            url: "/x".into(),
            push_url: None,
            max_blob_size: max,
            credentials: None,
            ssh_key: None,
            known_hosts: None,
            lfs_url: None,
            lfs_push_url: None,
            lfs_credentials: None,
        }
    }

    #[test]
    fn path_length_limit() {
        let c = Checks::default();
        let ok = format!("d/{}", "a".repeat(MAX_PATH_BYTES - 2));
        assert_eq!(c.check_path(&p(&ok)), None);
        let long = format!("{ok}b");
        assert_eq!(
            c.check_path(&p(&long)),
            Some(Refusal::PathTooLong { bytes: 4097 })
        );
        assert_eq!(
            c.check_symlink(&p(&long), "t").unwrap().label(),
            "path_too_long"
        );
    }

    #[test]
    fn symlinked_special_files_in_every_spelling() {
        let c = Checks::default();
        for name in [
            ".gitmodules",
            "x/.GITMODULES",
            ".gitmodules.",
            ".gitmodules ",
            ".gitmodules:x",
            "gitmod~1",
            "GI7EBA~3",
            "gi7eb~10",
            "gi7e~123",
            "g~123456",
            "d/~1234567",
            ".g\u{200c}itmodules",
            ".gitattributes",
            "gitatt~2",
            ".gitignore",
            ".mailmap",
        ] {
            let refusal = c.check_symlink(&p(name), "target");
            assert!(
                matches!(refusal, Some(Refusal::SymlinkedSpecialFile { .. })),
                "{name:?}: {refusal:?}"
            );
        }
        for name in [
            "gitmodules",
            ".gitmodulesx",
            "gitmod~5",
            "x/link",
            "gi7eba~10",
            "~123456789",
        ] {
            assert_eq!(c.check_symlink(&p(name), "target"), None, "{name:?}");
        }
        assert_eq!(
            c.check_symlink(&p("a/.gitmodules"), "t").unwrap().fsck_id(),
            Some("gitmodulesSymlink")
        );
        assert_eq!(
            c.check_symlink(&p(".mailmap"), "t").unwrap().fsck_id(),
            Some("mailmapSymlink")
        );
    }

    #[test]
    fn symlink_targets() {
        let c = Checks::default();
        let link = p("link");
        assert_eq!(c.check_symlink(&link, &"t".repeat(4096)), None);
        let long = c.check_symlink(&link, &"t".repeat(4097)).unwrap();
        assert_eq!(long, Refusal::SymlinkTargetTooLong { bytes: 4097 });
        assert_eq!(long.fsck_id(), Some("symlinkTargetLength"));
        for target in [
            "../.git/config",
            "sub/.GIT/HEAD",
            "a\\.git",
            "x/git~1/y",
            ".git",
        ] {
            let r = c.check_symlink(&link, target);
            assert!(
                matches!(r, Some(Refusal::SymlinkIntoGitDir { .. })),
                "{target}: {r:?}"
            );
            assert_eq!(r.unwrap().fsck_id(), Some("symlinkPointsToGitDir"));
        }
        for target in [".github/x", "git/x", "/abs/path", "../up", ".gitignore"] {
            assert_eq!(c.check_symlink(&link, target), None, "{target}");
        }
    }

    #[test]
    fn gitattributes_line_length() {
        let dir = tempfile::tempdir().unwrap();
        let c = Checks::default();
        let file = |content: &[u8]| {
            let path = dir.path().join("content");
            fs::write(&path, content).unwrap();
            path
        };

        let ok = file(format!("{}\nshort\n", "a".repeat(2047)).as_bytes());
        assert_eq!(
            c.check_file(&p(".gitattributes"), &ok, Commit).unwrap(),
            None
        );

        let long = file(format!("* text\n{}\nmore\n", "b".repeat(2048)).as_bytes());
        let refusal = Refusal::GitattributesLineTooLong {
            line: 2,
            bytes: 2048,
        };
        assert_eq!(refusal.fsck_id(), Some("gitattributesLineLength"));
        let expected = Some(refusal);
        for name in [
            ".gitattributes",
            "d/.gitattributes",
            "gitatt~1",
            ".GitAttributes.",
        ] {
            assert_eq!(
                c.check_file(&p(name), &long, Commit).unwrap(),
                expected,
                "{name}"
            );
        }
        for name in [".gitignore", "a.txt"] {
            assert_eq!(
                c.check_file(&p(name), &long, Commit).unwrap(),
                None,
                "{name}"
            );
        }

        // A last line without a newline is still checked.
        let last = file(format!("x\n{}", "c".repeat(3000)).as_bytes());
        assert_eq!(
            c.check_file(&p(".gitattributes"), &last, Commit).unwrap(),
            Some(Refusal::GitattributesLineTooLong {
                line: 2,
                bytes: 3000
            })
        );
        // CR is a byte of the line, as in git.
        let crlf = file(format!("{}\r\n", "d".repeat(2047)).as_bytes());
        assert_eq!(
            c.check_file(&p(".gitattributes"), &crlf, Commit).unwrap(),
            Some(Refusal::GitattributesLineTooLong {
                line: 1,
                bytes: 2048
            })
        );
        // Lines longer than the reader's buffer.
        let huge = file(format!("x\n\n{}\n", "e".repeat(20_000)).as_bytes());
        assert_eq!(
            c.check_file(&p(".gitattributes"), &huge, Commit).unwrap(),
            Some(Refusal::GitattributesLineTooLong {
                line: 3,
                bytes: 20_000
            })
        );
        let empty = file(b"");
        assert_eq!(
            c.check_file(&p(".gitattributes"), &empty, Commit).unwrap(),
            None
        );
    }

    #[test]
    fn gitattributes_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        // Sparse: one line of 100 MiB + 1 that is never read.
        File::create(&path)
            .unwrap()
            .set_len(ATTR_MAX_FILE_SIZE + 1)
            .unwrap();
        let start = Instant::now();
        let refusal = Checks::default()
            .check_file(&p(".gitattributes"), &path, Commit)
            .unwrap();
        assert_eq!(
            refusal,
            Some(Refusal::GitattributesTooLarge {
                bytes: ATTR_MAX_FILE_SIZE + 1
            })
        );
        assert_eq!(refusal.unwrap().fsck_id(), Some("gitattributesLarge"));
        assert!(start.elapsed().as_secs() < 2, "the file must not be read");
    }

    #[test]
    fn blob_limit_is_the_smallest_across_remotes() {
        let limits = Limits::from_remotes(&[
            remote("a", Some(10)),
            remote("b", Some(5)),
            remote("c", None),
            remote("d", Some(5)),
        ]);
        assert_eq!(
            limits.max_blob_size,
            Some(BlobLimit {
                bytes: 5,
                remote: "b".into()
            })
        );
        let c = Checks::new(limits);
        let dir = tempfile::tempdir().unwrap();
        let five = dir.path().join("five");
        fs::write(&five, b"12345").unwrap();
        let six = dir.path().join("six");
        fs::write(&six, b"123456").unwrap();
        assert_eq!(c.check_file(&p("f"), &five, Commit).unwrap(), None);
        let refused = c.check_file(&p("f"), &six, Commit).unwrap().unwrap();
        assert_eq!(
            refused,
            Refusal::BlobTooLarge {
                bytes: 6,
                limit: 5,
                remote: "b".into()
            }
        );
        assert_eq!(refused.fsck_id(), None);
        assert!(refused.to_string().contains("remote b"), "{refused}");
        assert_eq!(
            c.check_symlink(&p("l"), "123456"),
            Some(Refusal::BlobTooLarge {
                bytes: 6,
                limit: 5,
                remote: "b".into()
            })
        );
        assert_eq!(c.check_symlink(&p("l"), "12345"), None);

        assert_eq!(Limits::from_remotes(&[]), Limits::default());
        assert_eq!(
            Limits::from_remotes(&[remote("c", None)]).max_blob_size,
            None
        );
    }

    #[test]
    fn missing_content_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Checks::default()
                .check_file(&p("a"), &dir.path().join("missing"), Record)
                .is_err()
        );
    }

    #[test]
    fn record_stage_scans_only_up_to_the_sync_bound() {
        let dir = tempfile::tempdir().unwrap();
        let c = Checks::default();
        let attrs = p(".gitattributes");

        // 2 MiB of short lines, then one over the line limit at the end.
        let big = dir.path().join("big");
        let mut content = "* text\n".repeat((2 << 20) / 7).into_bytes();
        content.extend_from_slice(&[b'x'; 3000]);
        fs::write(&big, &content).unwrap();
        assert!(content.len() as u64 > SYNC_SCAN_LIMIT);
        assert_eq!(c.check_file(&attrs, &big, Record).unwrap(), None);
        let line = (2 << 20) / 7 + 1;
        let refusal = Some(Refusal::GitattributesLineTooLong { line, bytes: 3000 });
        assert_eq!(c.check_file(&attrs, &big, Commit).unwrap(), refusal);
        assert_eq!(c.check_blob(&attrs, &content), refusal);

        // A sparse file far over the bound but under the size limit is not
        // read at record time at all.
        let sparse = dir.path().join("sparse");
        File::create(&sparse)
            .unwrap()
            .set_len(ATTR_MAX_FILE_SIZE - 1)
            .unwrap();
        let start = Instant::now();
        assert_eq!(c.check_file(&attrs, &sparse, Record).unwrap(), None);
        assert!(
            start.elapsed().as_millis() < 500,
            "the file must not be read"
        );

        // Up to the bound, the record stage scans as the commit stage does.
        let small = dir.path().join("small");
        fs::write(&small, format!("x\n{}\n", "y".repeat(2048))).unwrap();
        for stage in [Record, Commit] {
            assert!(
                matches!(
                    c.check_file(&attrs, &small, stage).unwrap(),
                    Some(Refusal::GitattributesLineTooLong { line: 2, .. })
                ),
                "{stage}"
            );
        }
    }

    #[test]
    fn check_blob_agrees_with_check_file() {
        let dir = tempfile::tempdir().unwrap();
        let c = Checks::new(Limits {
            max_blob_size: Some(BlobLimit {
                bytes: 4000,
                remote: "r".into(),
            }),
        });
        let cases: [(&str, Vec<u8>); 6] = [
            ("a.txt", b"hello".to_vec()),
            ("a.txt", vec![b'z'; 4001]),
            (".gitattributes", b"* text\n".to_vec()),
            ("d/.gitattributes", [&b"x\n"[..], &[b'l'; 2500]].concat()),
            (".gitattributes", vec![b'l'; 3000]),
            ("gitatt~1", [&b"\n\n"[..], &[b'l'; 2048]].concat()),
        ];
        for (name, content) in cases {
            let file = dir.path().join("content");
            fs::write(&file, &content).unwrap();
            assert_eq!(
                c.check_blob(&p(name), &content),
                c.check_file(&p(name), &file, Commit).unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn gitmodules_content_rules() {
        let dir = tempfile::tempdir().unwrap();
        let c = Checks::default();
        let check = |name: &str, content: &str, stage| {
            let file = dir.path().join("content");
            fs::write(&file, content).unwrap();
            c.check_file(&p(name), &file, stage).unwrap()
        };
        let cases = [
            (
                "[submodule \"x\"]\n\tpath = x\n\turl = https://example.com/x.git\n",
                None,
            ),
            (
                "[submodule \"x\"\n",
                Some((
                    Refusal::GitmodulesParse { line: 1 },
                    "gitmodulesParse",
                    "gitmodules_parse",
                )),
            ),
            (
                "[submodule \"../../x\"]\nurl = ./x\n",
                Some((
                    Refusal::GitmodulesName {
                        name: "../../x".into(),
                    },
                    "gitmodulesName",
                    "gitmodules_name",
                )),
            ),
            (
                "[submodule \"x\"]\nurl = --upload-pack=evil\n",
                Some((
                    Refusal::GitmodulesUrl {
                        url: "--upload-pack=evil".into(),
                    },
                    "gitmodulesUrl",
                    "gitmodules_url",
                )),
            ),
            (
                "[submodule \"x\"]\npath = -x\n",
                Some((
                    Refusal::GitmodulesPath { path: "-x".into() },
                    "gitmodulesPath",
                    "gitmodules_path",
                )),
            ),
            (
                "[submodule \"x\"]\nupdate = !sh\n",
                Some((
                    Refusal::GitmodulesUpdate {
                        value: "!sh".into(),
                    },
                    "gitmodulesUpdate",
                    "gitmodules_update",
                )),
            ),
        ];
        for (content, expected) in cases {
            let got = check(".gitmodules", content, Record);
            match expected {
                None => assert_eq!(got, None, "{content:?}"),
                Some((refusal, id, label)) => {
                    assert_eq!(got.as_ref(), Some(&refusal), "{content:?}");
                    assert_eq!(refusal.fsck_id(), Some(id));
                    assert_eq!(refusal.label(), label);
                    assert!(refusal.to_string().contains(".gitmodules"), "{refusal}");
                }
            }
        }
        // Every spelling, in any directory; other names are not parsed.
        let bad = "[submodule \"x\"]\npath = -x\n";
        for name in [
            "sub/.gitmodules",
            ".GITMODULES",
            "gitmod~1",
            ".gitmodules.",
            ".gitmodules ",
        ] {
            assert!(
                matches!(
                    check(name, bad, Commit),
                    Some(Refusal::GitmodulesPath { .. })
                ),
                "{name}"
            );
        }
        for name in ["gitmodules", ".gitmodulesx", "a.txt", ".gitattributes"] {
            assert_eq!(check(name, bad, Commit), None, "{name}");
        }

        // Over the sync bound: parsed at commit time only.
        let large = format!("{}{bad}", "# padding\n".repeat((1 << 20) / 10 + 1));
        assert_eq!(check(".gitmodules", &large, Record), None);
        assert!(matches!(
            check(".gitmodules", &large, Commit),
            Some(Refusal::GitmodulesPath { .. })
        ));
        assert!(matches!(
            c.check_blob(&p(".gitmodules"), large.as_bytes()),
            Some(Refusal::GitmodulesPath { .. })
        ));
    }

    #[test]
    fn gitmodules_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        File::create(&path)
            .unwrap()
            .set_len(GITMODULES_MAX_SIZE + 1)
            .unwrap();
        let start = Instant::now();
        for stage in [Record, Commit] {
            let refusal = Checks::default()
                .check_file(&p(".gitmodules"), &path, stage)
                .unwrap();
            assert_eq!(
                refusal,
                Some(Refusal::GitmodulesTooLarge {
                    bytes: GITMODULES_MAX_SIZE + 1
                })
            );
            assert_eq!(refusal.unwrap().fsck_id(), Some("gitmodulesLarge"));
        }
        assert!(start.elapsed().as_secs() < 2, "the file must not be read");
    }
}
