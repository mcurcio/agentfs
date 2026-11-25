//! Running the `git` program on a state area's git directory.
//!
//! agentfs runs the `git` program where git does the job better than
//! libgit2 (AGENTS.md principle 4): `git repack` (agentfs-core's `repack`
//! and the tree history's repack task) and `git merge-tree` (agentfs-core's
//! `patch::integrate`). Both run it here, in an
//! environment of its own: a fixed `PATH`, `LC_ALL=C`, `HOME` set to the
//! git directory, and the system and global git config switched off, so the
//! program reads only the repository's own config, as libgit2 does, and nothing in the daemon's environment (`GIT_CONFIG_*`,
//! `GIT_EXEC_PATH`, `GIT_ATTR_SOURCE`) reaches it.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

pub use agentfs_align::GIT_PATH;

static CHILDREN: AtomicU64 = AtomicU64::new(0);

/// The memory bounds of every repack's `git`, before its subcommand: one
/// pack thread, at most 64 MiB of delta window and 16 MiB of delta cache,
/// blobs over 64 MiB stored whole, and 128 MiB of mapped packs. Both
/// engines' repacks run with them (docs/library/core.md explains the
/// recording engine's).
pub const PACK_CONFIG: [&str; 12] = [
    "-c",
    "repack.writeBitmaps=false",
    "-c",
    "pack.threads=1",
    "-c",
    "pack.windowMemory=64m",
    "-c",
    "pack.deltaCacheSize=16m",
    "-c",
    "core.bigFileThreshold=64m",
    "-c",
    "core.packedGitLimit=128m",
];

/// Starts a Git child on the tree actor's path, and counts it. The one spawn
/// site there: children clear their `PATH`, so no wrapper could count them, and
/// a source test (agentfs-repo's `the_tree_engine_starts_git_only_through_counted`)
/// fails when another site appears.
pub fn counted(cmd: &mut Command) -> io::Result<Child> {
    let child = cmd.spawn()?;
    CHILDREN.fetch_add(1, Ordering::Relaxed);
    Ok(child)
}

/// Git children started through [`counted`] in this process. A measurement.
pub fn children() -> u64 {
    CHILDREN.load(Ordering::Relaxed)
}

/// `git` in an empty environment: `PATH=GIT_PATH`, `LC_ALL=C`,
/// `HOME=GIT_DIR=git_dir`, `GIT_CONFIG_NOSYSTEM=1`,
/// `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_TERMINAL_PROMPT=0`, the working
/// directory `git_dir`, stdin null. Stdout and stderr are left to the
/// caller.
pub fn command(git: &Path, git_dir: &Path) -> Command {
    let mut cmd = Command::new(git);
    cmd.env_clear()
        .env("PATH", GIT_PATH)
        .env("LC_ALL", "C")
        .env("HOME", git_dir)
        .env("GIT_DIR", git_dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(git_dir)
        .stdin(Stdio::null());
    cmd
}

/// The `git` program as looked up in [`GIT_PATH`] (`None` when absent).
pub fn find_git() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    GIT_PATH
        .split(':')
        .map(|dir| Path::new(dir).join("git"))
        .find(|path| {
            std::fs::metadata(path)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_runs_in_an_environment_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let out = command(Path::new("/bin/sh"), dir.path())
            .args(["-c", "env"])
            .env("EXTRA", "1")
            .output()
            .unwrap();
        assert!(out.status.success());
        let env = String::from_utf8_lossy(&out.stdout);
        assert!(env.contains(&format!("PATH={GIT_PATH}\n")), "{env}");
        assert!(
            env.contains(&format!("GIT_DIR={}\n", dir.path().display())),
            "{env}"
        );
        assert!(env.contains("GIT_CONFIG_NOSYSTEM=1\n"), "{env}");
        assert!(env.contains("EXTRA=1\n"), "{env}");
        assert!(!env.contains("CARGO"), "{env}");
    }

    #[test]
    fn find_git_looks_in_git_path() {
        if let Some(git) = find_git() {
            assert!(
                GIT_PATH
                    .split(':')
                    .any(|d| git.parent() == Some(Path::new(d)))
            );
            assert!(git.ends_with("git"));
        }
    }
}
