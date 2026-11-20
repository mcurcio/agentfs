//! Git's histogram diff, called one way (target git-model §7, rule 10; MODEL.md,
//! finding 41). Line identity is decided here once per save, so the model and the
//! library share this call: one program lookup, one environment, one parse.
//!
//! Which lines a diff pairs still depends on the `git` release (MODEL.md, finding
//! 42): 2.47.3 and 2.56.0 align some files of repeated lines differently. This
//! crate fixes how the program is called, and `ci/git-release` which release answers.

use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// The `PATH` git is looked up in and runs with.
pub const GIT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// A changed region of the histogram diff, with zero-based ranges. A count of 0
/// starts after the line the diff named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
}

/// Failure to run the diff or to read its answer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("histogram diff: {0}")]
    Io(#[from] io::Error),
    #[error("git diff failed: {0}")]
    Git(String),
    #[error("invalid histogram output")]
    Invalid,
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir() -> io::Result<TempDir> {
    loop {
        let path = std::env::temp_dir().join(format!(
            "agentfs-diff-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(TempDir(path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// The diff of the files `old` and `new`, run in `dir`. Neither the caller's
/// directory nor its git configuration may reach the alignment: a directory inside
/// a broken worktree stops `git diff`, and a diff setting would change which lines
/// keep their identity.
pub fn command(dir: &Path, old: &Path, new: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", GIT_PATH)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(dir)
        .args([
            "diff",
            "--no-index",
            "--no-color",
            "--no-ext-diff",
            "--histogram",
            "--text",
            "--unified=0",
            "--",
        ])
        .arg(old)
        .arg(new)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn parse_range(range: &str) -> Result<(usize, usize), Error> {
    let (start, count) = range.split_once(',').unwrap_or((range, "1"));
    let start: usize = start.parse().map_err(|_| Error::Invalid)?;
    let count: usize = count.parse().map_err(|_| Error::Invalid)?;
    let start = if count == 0 {
        start
    } else {
        start.checked_sub(1).ok_or(Error::Invalid)?
    };
    Ok((start, count))
}

/// The hunks of a `--unified=0` diff's output, including NUL-containing lines.
pub fn parse(output: &[u8]) -> Result<Vec<Hunk>, Error> {
    let mut hunks = Vec::new();
    for line in output.split(|b| *b == b'\n') {
        let Some(header) = line.strip_prefix(b"@@ -") else {
            continue;
        };
        let header = std::str::from_utf8(header).map_err(|_| Error::Invalid)?;
        let (old, rest) = header.split_once(" +").ok_or(Error::Invalid)?;
        let new = rest.split_once(" @@").ok_or(Error::Invalid)?.0;
        let (old_start, old_count) = parse_range(old)?;
        let (new_start, new_count) = parse_range(new)?;
        hunks.push(Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
        });
    }
    Ok(hunks)
}

/// Git's text-mode histogram diff of two byte strings, the program started by
/// `spawn` (a caller that counts its children passes its own).
pub fn histogram_with(
    old: &[u8],
    new: &[u8],
    spawn: impl FnOnce(&mut Command) -> io::Result<Child>,
) -> Result<Vec<Hunk>, Error> {
    if old == new {
        return Ok(Vec::new());
    }
    let dir = temp_dir()?;
    let (a, b) = (dir.0.join("old"), dir.0.join("new"));
    std::fs::write(&a, old)?;
    std::fs::write(&b, new)?;
    histogram_files(&a, &b, spawn)
}

/// Git's text-mode histogram diff of the files `old` and `new`, which differ,
/// run in `old`'s directory. Its output is read as it streams and only the hunk
/// headers are kept, so neither file's lines are held: the call for images too
/// large to hold. `git diff` itself reads both files whole.
pub fn histogram_files(
    old: &Path,
    new: &Path,
    spawn: impl FnOnce(&mut Command) -> io::Result<Child>,
) -> Result<Vec<Hunk>, Error> {
    let dir = old.parent().unwrap_or(Path::new("/"));
    let mut child = spawn(&mut command(dir, old, new))?;
    let stdout = child.stdout.take().ok_or(Error::Invalid)?;
    let headers = headers(stdout);
    let out = child.wait_with_output()?;
    let headers = headers?;
    if !matches!(out.status.code(), Some(0 | 1)) {
        return Err(Error::Git(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    let hunks = parse(&headers)?;
    if hunks.is_empty() {
        return Err(Error::Invalid);
    }
    Ok(hunks)
}

/// The `@@ ` lines of a diff's output, each with its newline, read in chunks:
/// every other line is skipped unread, however long.
fn headers(mut reader: impl io::Read) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0; 64 << 10];
    let mut out = Vec::new();
    // At a line's start: the bytes of it seen so far, while they could be a
    // header's; `None` past a line that is not one.
    let mut line: Option<Vec<u8>> = Some(Vec::new());
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        let mut rest = &buf[..n];
        while !rest.is_empty() {
            let end = rest.iter().position(|b| *b == b'\n');
            let (part, after) = match end {
                Some(i) => (&rest[..=i], &rest[i + 1..]),
                None => (rest, &[][..]),
            };
            if let Some(seen) = &mut line {
                seen.extend_from_slice(part);
                let prefix = seen.len().min(3);
                if seen[..prefix] != b"@@ "[..prefix] {
                    line = None;
                }
            }
            if end.is_some() {
                if let Some(seen) = line.take() {
                    out.extend_from_slice(&seen);
                }
                line = Some(Vec::new());
            }
            rest = after;
        }
    }
}

/// [`histogram_with`], the program started as it is.
pub fn histogram(old: &[u8], new: &[u8]) -> Result<Vec<Hunk>, Error> {
    histogram_with(old, new, Command::spawn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn identical_bytes_have_no_hunks_and_run_nothing() {
        let hunks = histogram_with(b"same\n", b"same\n", |_| {
            Err(io::Error::other("nothing should run"))
        });
        assert_eq!(hunks.unwrap(), Vec::new());
    }

    #[test]
    fn a_changed_line_and_an_insert_are_read_from_the_program() {
        assert_eq!(
            histogram(b"a\nb\nc\n", b"a\nB\nc\nd\n").unwrap(),
            vec![
                Hunk {
                    old_start: 1,
                    old_count: 1,
                    new_start: 1,
                    new_count: 1,
                },
                Hunk {
                    old_start: 3,
                    old_count: 0,
                    new_start: 3,
                    new_count: 1,
                },
            ]
        );
    }

    #[test]
    fn text_mode_reads_nul_bytes_as_lines() {
        let hunks = histogram(b"a\0\nb\n", b"a\0\nc\n").unwrap();
        assert_eq!(hunks[0].old_start, 1);
    }

    #[test]
    fn the_environment_is_only_what_the_call_sets() {
        let dir = Path::new("/");
        let command = command(dir, Path::new("a"), Path::new("b"));
        let envs: Vec<(&OsStr, Option<&OsStr>)> = command.get_envs().collect();
        assert_eq!(envs.len(), 4, "{envs:?}");
        assert!(envs.contains(&(OsStr::new("PATH"), Some(OsStr::new(GIT_PATH)))));
        assert!(format!("{command:?}").contains("--histogram"));
    }

    #[test]
    fn only_header_lines_are_kept_however_long_the_others() {
        let mut output = b"diff --git a/old b/new\n@@ -1 +1 @@\n-".to_vec();
        output.extend(std::iter::repeat_n(b'@', 200_000));
        output.extend_from_slice(b"\n+x\n@@ -3,0 +4,2 @@ ctx\n+y\n+z");
        let kept = headers(&output[..]).unwrap();
        assert_eq!(kept, b"@@ -1 +1 @@\n@@ -3,0 +4,2 @@ ctx\n");
        assert_eq!(parse(&kept).unwrap().len(), 2);
    }

    #[test]
    fn two_files_are_diffed_as_two_strings_are() {
        let dir = temp_dir().unwrap();
        let (a, b) = (dir.0.join("a"), dir.0.join("b"));
        std::fs::write(&a, b"a\nb\nc\n").unwrap();
        std::fs::write(&b, b"a\nB\nc\nd\n").unwrap();
        assert_eq!(
            histogram_files(&a, &b, Command::spawn).unwrap(),
            histogram(b"a\nb\nc\n", b"a\nB\nc\nd\n").unwrap()
        );
    }

    #[test]
    fn a_hunk_header_is_parsed_as_git_writes_it() {
        assert_eq!(
            parse(b"diff\n@@ -0,0 +1 @@\n+x\n@@ -2,2 +3 @@ ctx\n-y\n").unwrap(),
            vec![
                Hunk {
                    old_start: 0,
                    old_count: 0,
                    new_start: 0,
                    new_count: 1,
                },
                Hunk {
                    old_start: 1,
                    old_count: 2,
                    new_start: 2,
                    new_count: 1,
                },
            ]
        );
        assert!(parse(b"@@ -x +1 @@\n").is_err());
    }
}
