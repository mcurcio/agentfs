//! Detects nested git repositories in the backing tree (design §5).
//!
//! A directory is a nested repository if it contains a `.git` directory, a
//! `.git` file pointing at a valid git directory, or has the layout of a bare
//! repository (`HEAD`, `objects/` and `refs/`). This module only probes one
//! directory; [`crate::rules`] decides which directories to probe (never the
//! mount root) and what a hit means under `agentfs.nested`.
//!
//! Probing never follows a symlinked `.git`: a symlink's target is a path in
//! the workload's view of the mount, not in the backing tree, so following it
//! from here could reach anything. A symlinked `.git` is not a nested
//! repository.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Why a directory counts as a nested repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NestedKind {
    /// It contains a `.git` directory (`git init`, `git clone`).
    GitDir,
    /// It contains a `.git` file whose `gitdir:` is a valid git directory
    /// (submodules, `git worktree add`).
    GitFile,
    /// It is itself laid out as a bare repository (`git clone --bare`,
    /// tool-managed stores).
    Bare,
}

impl NestedKind {
    pub fn describe(self) -> &'static str {
        match self {
            Self::GitDir => ".git directory",
            Self::GitFile => ".git file",
            Self::Bare => "bare repository layout",
        }
    }
}

/// Probes `dir` (a directory in the backing tree) for a nested repository.
///
/// A missing `dir`, or a `dir` that is not a directory, is not nested.
pub fn detect(dir: &Path) -> io::Result<Option<NestedKind>> {
    let dot_git = dir.join(".git");
    if let Some(meta) = metadata(&dot_git)? {
        if meta.is_dir() {
            return Ok(Some(NestedKind::GitDir));
        }
        if meta.is_file() && gitdir_from_file(dir, &dot_git)?.is_some() {
            return Ok(Some(NestedKind::GitFile));
        }
    }
    if is_file(&dir.join("HEAD"))? && is_dir(&dir.join("objects"))? && is_dir(&dir.join("refs"))? {
        return Ok(Some(NestedKind::Bare));
    }
    Ok(None)
}

/// The valid git directory a `.git` file in `dir` points to, if any.
///
/// The file's first line must be `gitdir: <path>`; a relative path is
/// resolved against `dir`, as git does.
fn gitdir_from_file(dir: &Path, file: &Path) -> io::Result<Option<PathBuf>> {
    let content = match fs::read(file) {
        Ok(c) => c,
        Err(e) if is_absent(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    let first = content.split(|b| *b == b'\n').next().unwrap_or_default();
    let Some(rest) = first.strip_prefix(b"gitdir:") else {
        return Ok(None);
    };
    let Ok(target) = std::str::from_utf8(rest) else {
        return Ok(None);
    };
    let target = target.trim();
    if target.is_empty() {
        return Ok(None);
    }
    let target = dir.join(target);
    Ok(is_git_dir(&target)?.then_some(target))
}

/// True if `p` looks like a git directory: `HEAD` plus `objects/` and
/// `refs/`, or `HEAD` plus `commondir` (a linked worktree's admin dir).
fn is_git_dir(p: &Path) -> io::Result<bool> {
    if !is_file(&p.join("HEAD"))? {
        return Ok(false);
    }
    Ok((is_dir(&p.join("objects"))? && is_dir(&p.join("refs"))?) || is_file(&p.join("commondir"))?)
}

/// `symlink_metadata`, treating a missing path (or a non-directory
/// ancestor) as absent.
fn metadata(p: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(p) {
        Ok(m) => Ok(Some(m)),
        Err(e) if is_absent(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

fn is_file(p: &Path) -> io::Result<bool> {
    Ok(metadata(p)?.is_some_and(|m| m.is_file()))
}

fn is_dir(p: &Path) -> io::Result<bool> {
    Ok(metadata(p)?.is_some_and(|m| m.is_dir()))
}

fn is_absent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn bare_layout(dir: &Path) {
        fs::create_dir_all(dir.join("objects")).unwrap();
        fs::create_dir_all(dir.join("refs")).unwrap();
        fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    #[test]
    fn plain_directory_is_not_nested() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(detect(t.path()).unwrap(), None);
        assert_eq!(detect(&t.path().join("missing")).unwrap(), None);
        fs::write(t.path().join("file"), "").unwrap();
        assert_eq!(detect(&t.path().join("file")).unwrap(), None);
    }

    #[test]
    fn dot_git_directory() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join(".git")).unwrap();
        assert_eq!(detect(t.path()).unwrap(), Some(NestedKind::GitDir));
    }

    #[test]
    fn dot_git_file_pointing_at_a_git_dir() {
        let t = tempfile::tempdir().unwrap();
        let work = t.path().join("work");
        fs::create_dir(&work).unwrap();
        bare_layout(&t.path().join("store/x"));
        fs::write(work.join(".git"), "gitdir: ../store/x\n").unwrap();
        assert_eq!(detect(&work).unwrap(), Some(NestedKind::GitFile));

        // Absolute paths work too.
        let abs = format!("gitdir: {}\n", t.path().join("store/x").display());
        fs::write(work.join(".git"), abs).unwrap();
        assert_eq!(detect(&work).unwrap(), Some(NestedKind::GitFile));
    }

    #[test]
    fn dot_git_file_pointing_at_a_worktree_admin_dir() {
        let t = tempfile::tempdir().unwrap();
        let admin = t.path().join("main/.git/worktrees/wt");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        let wt = t.path().join("wt");
        fs::create_dir(&wt).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        assert_eq!(detect(&wt).unwrap(), Some(NestedKind::GitFile));
    }

    #[test]
    fn invalid_dot_git_files_are_not_nested() {
        let t = tempfile::tempdir().unwrap();
        for content in ["garbage\n", "gitdir: \n", "gitdir: does/not/exist\n", ""] {
            fs::write(t.path().join(".git"), content).unwrap();
            assert_eq!(detect(t.path()).unwrap(), None, "{content:?}");
        }
        // A gitdir that exists but is not a git directory.
        fs::create_dir(t.path().join("plain")).unwrap();
        fs::write(t.path().join(".git"), "gitdir: plain\n").unwrap();
        assert_eq!(detect(t.path()).unwrap(), None);
    }

    #[test]
    fn symlinked_dot_git_is_not_followed() {
        let t = tempfile::tempdir().unwrap();
        bare_layout(&t.path().join("real"));
        let work = t.path().join("work");
        fs::create_dir(&work).unwrap();
        symlink(t.path().join("real"), work.join(".git")).unwrap();
        assert_eq!(detect(&work).unwrap(), None);
    }

    #[test]
    fn bare_layout_needs_all_three_parts() {
        let t = tempfile::tempdir().unwrap();
        let store = t.path().join("store");
        bare_layout(&store);
        assert_eq!(detect(&store).unwrap(), Some(NestedKind::Bare));

        for missing in ["HEAD", "objects", "refs"] {
            let d = t.path().join(format!("no-{missing}"));
            bare_layout(&d);
            let p = d.join(missing);
            if p.is_dir() {
                fs::remove_dir(&p).unwrap();
            } else {
                fs::remove_file(&p).unwrap();
            }
            assert_eq!(detect(&d).unwrap(), None, "without {missing}");
        }

        let d = t.path().join("head-dir");
        fs::create_dir_all(d.join("HEAD")).unwrap();
        fs::create_dir_all(d.join("objects")).unwrap();
        fs::create_dir_all(d.join("refs")).unwrap();
        assert_eq!(detect(&d).unwrap(), None);
    }
}
