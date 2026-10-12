//! Detects nested git repositories in the backing tree (design §5).
//!
//! A directory is a nested repository if it contains a `.git` directory, a
//! `.git` file or symlink pointing at a git directory inside the tree, or is
//! itself a git directory: a bare repository, or one still being created.
//! This module only probes one directory; [`crate::rules`] decides which
//! directories to probe (never the mount root) and what a hit means under
//! `agentfs.nested`.
//!
//! **Repositories under construction.** `git init --bare` and
//! `git clone --bare` write `description`, `hooks/`, `info/exclude`, `refs/`,
//! `HEAD` and `config` before `objects/` exists, and each of those files is a
//! change boundary. Requiring the complete layout would record them as new
//! paths, and they would then stay tracked. So any one of these marks a git
//! directory: a `HEAD` or `HEAD.lock` whose first line is a symbolic ref or
//! an object id, a `config` or `config.lock` with a `[core]` section setting
//! `repositoryformatversion`, or any file of git's default template:
//! `description`, `info/exclude`, or a sample hook (`hooks/<name>.sample`
//! starting with `#!`). git copies the template in directory order, which
//! differs between filesystems, so any of them can come first. Only a
//! custom template's files written before `HEAD` can still be recorded; the
//! committer drops them at commit time once `HEAD` exists
//! ([`crate::volume::Committer`], design §5).
//!
//! **Where `gitdir:` and `.git` symlinks point.** Their targets are paths in
//! the workload's view of the mount, never followed on the host. A relative
//! target is resolved lexically against the directory holding `.git` and
//! must stay inside the tree. An absolute target (what `git worktree add`
//! writes by default) has an unknown mount-point prefix, so each of its
//! suffixes is tried as a path from the tree root: `/workspace/main/.git/x`
//! is found as `main/.git/x` in a tree mounted at `/workspace`.
//!
//! Every file is read with [`probe::read_regular`]: regular files only, never
//! through a symlink, without blocking, and only up to a few KiB.

use std::fs;
use std::io;
use std::path::{Component, Path};

use crate::probe;

/// Why a directory counts as a nested repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NestedKind {
    /// It contains a `.git` directory (`git init`, `git clone`).
    GitDir,
    /// It contains a `.git` file whose `gitdir:` is a git directory in the
    /// tree (submodules, `git worktree add`).
    GitFile,
    /// It contains a `.git` symlink to a git directory in the tree (Android's
    /// `repo` tool).
    GitSymlink,
    /// It is itself a git directory, complete or being created
    /// (`git clone --bare`, `git init --bare`, tool-managed stores).
    Bare,
}

impl NestedKind {
    pub fn describe(self) -> &'static str {
        match self {
            Self::GitDir => ".git directory",
            Self::GitFile => ".git file",
            Self::GitSymlink => ".git symlink",
            Self::Bare => "bare repository layout",
        }
    }
}

/// git's default template `description`.
const DEFAULT_DESCRIPTION: &str =
    "Unnamed repository; edit this file 'description' to name the repository.";

/// The first line of git's default template `info/exclude`.
const DEFAULT_EXCLUDE: &str = "# git ls-files --others --exclude-from=.git/info/exclude";

/// The sample hooks of git's default template (`hooks/<name>.sample`).
const SAMPLE_HOOKS: [&str; 14] = [
    "applypatch-msg",
    "commit-msg",
    "fsmonitor-watchman",
    "post-update",
    "pre-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "pre-push",
    "pre-rebase",
    "pre-receive",
    "prepare-commit-msg",
    "push-to-checkout",
    "sendemail-validate",
    "update",
];

/// Probes `dir`, a directory relative to the backing tree at `tree` (`""`
/// is the root), for a nested repository.
///
/// A missing `dir`, or a `dir` that is not a directory, is not nested.
pub fn detect(tree: &Path, dir: &str) -> io::Result<Option<NestedKind>> {
    let abs = tree.join(dir);
    let dot_git = abs.join(".git");
    if let Some(meta) = metadata(&dot_git)? {
        if meta.is_dir() {
            return Ok(Some(NestedKind::GitDir));
        }
        if meta.is_file() && gitdir_from_file(tree, dir, &dot_git)? {
            return Ok(Some(NestedKind::GitFile));
        }
        if meta.is_symlink() && gitdir_from_symlink(tree, dir, &dot_git)? {
            return Ok(Some(NestedKind::GitSymlink));
        }
    }
    if looks_like_git_dir(&abs)? {
        return Ok(Some(NestedKind::Bare));
    }
    Ok(None)
}

/// True if `dir/.git` (`dir` relative to the tree at `tree`, `""` the root)
/// is git's own data: a directory that is, or is becoming, a git directory
/// (see the module docs), or a `.git` file or symlink pointing at a git
/// directory in the tree. A `.git` that is none of these (an ordinary
/// directory or file someone named `.git`) is not.
pub fn is_git_metadata(tree: &Path, dir: &str) -> io::Result<bool> {
    let dot_git = tree.join(dir).join(".git");
    match metadata(&dot_git)? {
        Some(m) if m.is_dir() => looks_like_git_dir(&dot_git),
        Some(m) if m.is_file() => gitdir_from_file(tree, dir, &dot_git),
        Some(m) if m.is_symlink() => gitdir_from_symlink(tree, dir, &dot_git),
        _ => Ok(false),
    }
}

/// True if the `.git` file `file` in `dir` points at a git directory in the
/// tree. Its first line must be `gitdir: <path>`.
fn gitdir_from_file(tree: &Path, dir: &str, file: &Path) -> io::Result<bool> {
    let Some(content) = probe::read_regular(file, probe::SMALL)? else {
        return Ok(false);
    };
    let first = content.split(|b| *b == b'\n').next().unwrap_or_default();
    let Some(rest) = first.strip_prefix(b"gitdir:") else {
        return Ok(false);
    };
    let Ok(target) = std::str::from_utf8(rest) else {
        return Ok(false);
    };
    any_git_dir(tree, dir, target.trim())
}

/// True if the `.git` symlink `link` in `dir` points at a git directory in
/// the tree. The link is read, never followed.
fn gitdir_from_symlink(tree: &Path, dir: &str, link: &Path) -> io::Result<bool> {
    let target = match fs::read_link(link) {
        Ok(t) => t,
        Err(e) if probe::is_absent(&e) || e.kind() == io::ErrorKind::InvalidInput => {
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    let Some(target) = target.to_str() else {
        return Ok(false);
    };
    any_git_dir(tree, dir, target)
}

/// True if `target`, as written in `dir`, names a git directory in the tree.
fn any_git_dir(tree: &Path, dir: &str, target: &str) -> io::Result<bool> {
    for candidate in candidates(dir, target) {
        if is_git_dir(&tree.join(candidate))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The tree-relative paths `target`, found in `dir`, may refer to (see the
/// module docs). Never the root, and never outside the tree.
fn candidates(dir: &str, target: &str) -> Vec<String> {
    if target.is_empty() {
        return Vec::new();
    }
    let target = Path::new(target);
    if target.is_absolute() {
        let Some(parts) = normalize(Vec::new(), target, false) else {
            return Vec::new();
        };
        (0..parts.len()).map(|i| parts[i..].join("/")).collect()
    } else {
        let base = dir.split('/').filter(|c| !c.is_empty()).collect();
        match normalize(base, target, true) {
            Some(parts) if !parts.is_empty() => vec![parts.join("/")],
            _ => Vec::new(),
        }
    }
}

/// Applies `path`'s components to `base` lexically. A `..` above the start
/// is `None` if `strict`, and ignored otherwise (as `/..` is `/`).
fn normalize<'a>(mut base: Vec<&'a str>, path: &'a Path, strict: bool) -> Option<Vec<&'a str>> {
    for component in path.components() {
        match component {
            Component::Normal(c) => base.push(c.to_str()?),
            Component::ParentDir => {
                if base.pop().is_none() && strict {
                    return None;
                }
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    Some(base)
}

/// True if `p` is a complete git directory: `HEAD` plus `objects/` and
/// `refs/`, or `HEAD` plus `commondir` (a linked worktree's admin dir).
fn is_git_dir(p: &Path) -> io::Result<bool> {
    if !is_file(&p.join("HEAD"))? {
        return Ok(false);
    }
    Ok((is_dir(&p.join("objects"))? && is_dir(&p.join("refs"))?) || is_file(&p.join("commondir"))?)
}

/// True if `p` is a git directory, complete or being created (see the module
/// docs).
fn looks_like_git_dir(p: &Path) -> io::Result<bool> {
    if is_git_dir(p)? {
        return Ok(true);
    }
    for name in ["HEAD", "HEAD.lock"] {
        if first_line(&p.join(name))?.is_some_and(is_head) {
            return Ok(true);
        }
    }
    for name in ["config", "config.lock"] {
        if probe::read_regular(&p.join(name), probe::SMALL)?.is_some_and(|c| is_git_config(&c)) {
            return Ok(true);
        }
    }
    if first_line(&p.join("description"))?.is_some_and(|l| l == DEFAULT_DESCRIPTION) {
        return Ok(true);
    }
    if first_line(&p.join("info/exclude"))?.is_some_and(|l| l == DEFAULT_EXCLUDE) {
        return Ok(true);
    }
    // The template's sample hooks: probed only when `hooks/` exists, so an
    // ordinary directory costs one extra stat.
    if is_dir(&p.join("hooks"))? {
        for hook in SAMPLE_HOOKS {
            let sample = p.join("hooks").join(format!("{hook}.sample"));
            if first_line(&sample)?.is_some_and(|l| l.starts_with("#!")) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The first line of a small regular file, trimmed, if it is UTF-8.
fn first_line(p: &Path) -> io::Result<Option<String>> {
    let Some(content) = probe::read_regular(p, probe::SMALL)? else {
        return Ok(None);
    };
    let first = content.split(|b| *b == b'\n').next().unwrap_or_default();
    Ok(std::str::from_utf8(first).ok().map(|s| s.trim().to_owned()))
}

/// What git accepts as `HEAD`: a symbolic ref, or a SHA-1 or SHA-256 id.
fn is_head(line: String) -> bool {
    line.starts_with("ref: refs/")
        || (matches!(line.len(), 40 | 64) && line.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// True for a git config with `[core]` setting `repositoryformatversion`,
/// which git writes into every repository it creates.
fn is_git_config(content: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(content) else {
        return false;
    };
    let mut in_core = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_core = line.eq_ignore_ascii_case("[core]");
        } else if in_core
            && line
                .split(['=', ' ', '\t'])
                .next()
                .is_some_and(|k| k.eq_ignore_ascii_case("repositoryformatversion"))
        {
            return true;
        }
    }
    false
}

/// `symlink_metadata`, treating a missing path (or a non-directory
/// ancestor) as absent.
fn metadata(p: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(p) {
        Ok(m) => Ok(Some(m)),
        Err(e) if probe::is_absent(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

fn is_file(p: &Path) -> io::Result<bool> {
    Ok(metadata(p)?.is_some_and(|m| m.is_file()))
}

fn is_dir(p: &Path) -> io::Result<bool> {
    Ok(metadata(p)?.is_some_and(|m| m.is_dir()))
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

    fn d(t: &tempfile::TempDir, dir: &str) -> Option<NestedKind> {
        detect(t.path(), dir).unwrap()
    }

    #[test]
    fn plain_directory_is_not_nested() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("plain")).unwrap();
        assert_eq!(d(&t, "plain"), None);
        assert_eq!(d(&t, "missing"), None);
        fs::write(t.path().join("file"), "").unwrap();
        assert_eq!(d(&t, "file"), None);
    }

    #[test]
    fn dot_git_directory() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("w/.git")).unwrap();
        assert_eq!(d(&t, "w"), Some(NestedKind::GitDir));
    }

    #[test]
    fn dot_git_file_pointing_at_a_git_dir() {
        let t = tempfile::tempdir().unwrap();
        let work = t.path().join("work");
        fs::create_dir(&work).unwrap();
        bare_layout(&t.path().join("store/x"));
        fs::write(work.join(".git"), "gitdir: ../store/x\n").unwrap();
        assert_eq!(d(&t, "work"), Some(NestedKind::GitFile));
        fs::write(work.join(".git"), "gitdir: ./../store/./x/\n").unwrap();
        assert_eq!(d(&t, "work"), Some(NestedKind::GitFile));
    }

    #[test]
    fn submodule_git_dir_inside_the_root_git_dir() {
        let t = tempfile::tempdir().unwrap();
        bare_layout(&t.path().join(".git/modules/lib/sub"));
        fs::create_dir_all(t.path().join("lib/sub")).unwrap();
        fs::write(
            t.path().join("lib/sub/.git"),
            "gitdir: ../../.git/modules/lib/sub\n",
        )
        .unwrap();
        assert_eq!(d(&t, "lib/sub"), Some(NestedKind::GitFile));
    }

    #[test]
    fn absolute_gitdir_is_a_workload_path() {
        // `git worktree add` inside a mount at /workspace.
        let t = tempfile::tempdir().unwrap();
        let admin = t.path().join("main/.git/worktrees/wt");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        fs::create_dir(t.path().join("wt")).unwrap();
        fs::write(
            t.path().join("wt/.git"),
            "gitdir: /workspace/main/.git/worktrees/wt\n",
        )
        .unwrap();
        assert_eq!(d(&t, "wt"), Some(NestedKind::GitFile));
        // Mounted at the namespace root.
        fs::write(
            t.path().join("wt/.git"),
            "gitdir: /main/.git/worktrees/wt\n",
        )
        .unwrap();
        assert_eq!(d(&t, "wt"), Some(NestedKind::GitFile));
    }

    #[test]
    fn gitdir_targets_never_leave_the_tree() {
        let outer = tempfile::tempdir().unwrap();
        bare_layout(&outer.path().join("outside"));
        let tree = outer.path().join("tree");
        fs::create_dir_all(tree.join("w")).unwrap();
        for target in ["../../outside", "/../outside/..", "/"] {
            fs::write(tree.join("w/.git"), format!("gitdir: {target}\n")).unwrap();
            assert_eq!(detect(&tree, "w").unwrap(), None, "{target}");
        }
        assert_eq!(candidates("w", "../.."), Vec::<String>::new());
        assert_eq!(
            candidates("w", ".."),
            Vec::<String>::new(),
            "never the root"
        );
        assert_eq!(candidates("a/b", "../c"), ["a/c"]);
        assert_eq!(candidates("", "/x/../y/z"), ["y/z", "z"]);
    }

    #[test]
    fn invalid_dot_git_files_are_not_nested() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("w")).unwrap();
        for content in ["garbage\n", "gitdir: \n", "gitdir: does/not/exist\n", ""] {
            fs::write(t.path().join("w/.git"), content).unwrap();
            assert_eq!(d(&t, "w"), None, "{content:?}");
        }
        // A gitdir that exists but is not a git directory.
        fs::create_dir(t.path().join("w/plain")).unwrap();
        fs::write(t.path().join("w/.git"), "gitdir: plain\n").unwrap();
        assert_eq!(d(&t, "w"), None);
    }

    #[test]
    fn oversized_dot_git_file_is_not_read() {
        let t = tempfile::tempdir().unwrap();
        bare_layout(&t.path().join("store"));
        fs::create_dir(t.path().join("w")).unwrap();
        let mut content = b"gitdir: ../store\n".to_vec();
        content.resize(probe::SMALL as usize + 1, b'x');
        fs::write(t.path().join("w/.git"), content).unwrap();
        assert_eq!(d(&t, "w"), None);
    }

    #[test]
    fn dot_git_symlinks_resolve_inside_the_tree_only() {
        let outer = tempfile::tempdir().unwrap();
        let tree = outer.path().join("tree");
        bare_layout(&tree.join("projects/p.git"));
        bare_layout(&outer.path().join("real"));
        fs::create_dir_all(tree.join("w")).unwrap();
        symlink("../projects/p.git", tree.join("w/.git")).unwrap();
        assert_eq!(detect(&tree, "w").unwrap(), Some(NestedKind::GitSymlink));

        // Absolute targets are workload paths; the host path is not followed.
        fs::remove_file(tree.join("w/.git")).unwrap();
        symlink(outer.path().join("real"), tree.join("w/.git")).unwrap();
        assert_eq!(detect(&tree, "w").unwrap(), None);
        fs::remove_file(tree.join("w/.git")).unwrap();
        symlink("/mnt/projects/p.git", tree.join("w/.git")).unwrap();
        assert_eq!(detect(&tree, "w").unwrap(), Some(NestedKind::GitSymlink));
    }

    #[test]
    fn complete_bare_layout() {
        let t = tempfile::tempdir().unwrap();
        bare_layout(&t.path().join("store"));
        assert_eq!(d(&t, "store"), Some(NestedKind::Bare));

        // HEAD alone is enough when it reads as a HEAD.
        let only = t.path().join("only-head");
        fs::create_dir(&only).unwrap();
        fs::write(only.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(d(&t, "only-head"), Some(NestedKind::Bare));

        // A file named HEAD that is not one, without objects/ and refs/.
        let other = t.path().join("other");
        fs::create_dir(&other).unwrap();
        fs::write(other.join("HEAD"), "not a ref\n").unwrap();
        fs::create_dir(other.join("refs")).unwrap();
        assert_eq!(d(&t, "other"), None);

        let dir_head = t.path().join("head-dir");
        fs::create_dir_all(dir_head.join("HEAD")).unwrap();
        fs::create_dir_all(dir_head.join("objects")).unwrap();
        fs::create_dir_all(dir_head.join("refs")).unwrap();
        assert_eq!(d(&t, "head-dir"), None);
    }

    #[test]
    fn repository_under_construction() {
        let t = tempfile::tempdir().unwrap();
        let cases: [(&str, &str); 8] = [
            ("description", &format!("{DEFAULT_DESCRIPTION}\n")),
            ("info/exclude", &format!("{DEFAULT_EXCLUDE}\n# more\n")),
            (
                "hooks/pre-commit.sample",
                "#!/bin/sh\n#\n# An example hook\n",
            ),
            ("HEAD", "ref: refs/heads/main\n"),
            ("HEAD.lock", "ref: refs/heads/main\n"),
            ("HEAD", &format!("{}\n", "a".repeat(40))),
            (
                "config",
                "[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
            ),
            ("config.lock", "[Core]\nrepositoryFormatVersion=1\n"),
        ];
        for (i, (name, content)) in cases.iter().enumerate() {
            let dir = format!("s{i}");
            fs::create_dir(t.path().join(&dir)).unwrap();
            let file = t.path().join(&dir).join(name);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, content).unwrap();
            assert_eq!(d(&t, &dir), Some(NestedKind::Bare), "{name}: {content:?}");
        }
        for (name, content) in [
            ("description", "My project\n"),
            ("config", "[user]\n\trepositoryformatversion = 0\n"),
            ("HEAD", &"g".repeat(40)),
            ("hooks/pre-commit", "#!/bin/sh\n"),
            ("hooks/my-hook.sample", "#!/bin/sh\n"),
            ("hooks/pre-commit.sample", "not a script\n"),
            ("info/exclude", "*.o\n"),
        ] {
            let dir = t.path().join("neg");
            fs::create_dir_all(dir.join(name).parent().unwrap()).unwrap();
            fs::write(dir.join(name), content).unwrap();
            assert_eq!(d(&t, "neg"), None, "{name}: {content:?}");
            fs::remove_file(dir.join(name)).unwrap();
        }
    }

    #[test]
    fn markers_are_never_read_through_symlinks() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("real-head"), "ref: refs/heads/main\n").unwrap();
        fs::create_dir(t.path().join("s")).unwrap();
        symlink("../real-head", t.path().join("s/HEAD")).unwrap();
        assert_eq!(d(&t, "s"), None);
    }

    #[test]
    fn git_metadata_is_only_a_real_git_dir_or_gitfile() {
        let t = tempfile::tempdir().unwrap();
        let m = |dir: &str| is_git_metadata(t.path(), dir).unwrap();
        // An ordinary directory named .git, and a file that is no gitfile.
        fs::create_dir_all(t.path().join("notes/.git")).unwrap();
        fs::write(t.path().join("notes/.git/report.txt"), "r").unwrap();
        assert!(!m("notes"));
        fs::create_dir_all(t.path().join("plain")).unwrap();
        fs::write(t.path().join("plain/.git"), "hello\n").unwrap();
        assert!(!m("plain"));
        assert!(!m("missing"));
        // A repository, also while git is creating it.
        fs::create_dir_all(t.path().join(".git")).unwrap();
        assert!(!m(""));
        fs::write(t.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(m(""));
        bare_layout(&t.path().join("w/.git"));
        assert!(m("w"));
        // A gitfile pointing at it.
        fs::create_dir_all(t.path().join("sub")).unwrap();
        fs::write(t.path().join("sub/.git"), "gitdir: ../w/.git\n").unwrap();
        assert!(m("sub"));
    }
}
