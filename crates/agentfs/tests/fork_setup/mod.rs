//! The fork tests' shared setup (`cli_fork.rs`, `cli_fork_deliver.rs`):
//! a source volume `src` with `file://` bare remotes, driven through the
//! real `agentfs` binary, with git made hermetic.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use git2::{Oid, Repository};

pub fn have_git() -> bool {
    let found = Command::new("git").arg("--version").output().is_ok();
    if !found {
        eprintln!("skipping: git is not installed");
    }
    found
}

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A source volume `src` whose settings come from a profile fragment, a
/// bare `origin`, and, when `fork_remote`, a bare `forks` remote with
/// `agentfsForkRemote = true`.
pub struct Setup {
    pub dir: tempfile::TempDir,
}

impl Setup {
    pub fn new(fork_remote: bool) -> Self {
        Self::with_profile(|s| {
            let mut profile = format!("[remote \"origin\"]\n\turl = {}\n", s.url("origin.git"));
            if fork_remote {
                profile.push_str(&format!(
                    "[remote \"forks\"]\n\turl = {}\n\tagentfsForkRemote = true\n",
                    s.url("forks.git")
                ));
            }
            profile
        })
    }

    /// The source declared local-only by its profile (F8 S3): no remote.
    pub fn local_only() -> Self {
        Self::with_profile(|_| "[agentfs]\n\tremotes = none\n".into())
    }

    fn with_profile(profile: impl FnOnce(&Self) -> String) -> Self {
        let s = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(s.path("tree")).unwrap();
        fs::create_dir_all(s.path("home")).unwrap();
        Repository::init_bare(s.path("origin.git")).unwrap();
        Repository::init_bare(s.path("forks.git")).unwrap();
        let profile = profile(&s);
        fs::write(s.path("profile"), profile).unwrap();
        let profile = s.path("profile").display().to_string();
        s.ok(
            "state",
            "tree",
            &["init", "--volume", "src", "--include", &profile],
        );
        s
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub fn url(&self, name: &str) -> String {
        format!("file://{}", self.path(name).display())
    }

    pub fn hermetic(&self, cmd: &mut Command) {
        let home = self.path("home");
        cmd.env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Producer")
            .env("GIT_AUTHOR_EMAIL", "producer@example.com")
            .env("GIT_COMMITTER_NAME", "Producer")
            .env("GIT_COMMITTER_EMAIL", "producer@example.com")
            .env_remove("AGENTFS_STATE")
            .env_remove("AGENTFS_ROOT")
            .env_remove("AGENTFS_VOLUME")
            .env("AGENTFS_REPLICA", "test-0");
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("OTEL_") {
                cmd.env_remove(name);
            }
        }
    }

    /// `agentfs --state <state> --root <root> args`.
    pub fn agentfs(&self, state: &str, root: &str, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.arg("--state")
            .arg(self.path(state))
            .arg("--root")
            .arg(self.path(root))
            .args(args);
        self.hermetic(&mut cmd);
        cmd.output().unwrap()
    }

    pub fn ok(&self, state: &str, root: &str, args: &[&str]) -> String {
        let out = self.agentfs(state, root, args);
        assert!(
            out.status.success(),
            "agentfs {args:?}: {}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    pub fn record(&self, state: &str, root: &str, name: &str, content: &str) {
        fs::write(self.path(root).join(name), content).unwrap();
        self.ok(state, root, &["record", name]);
    }

    /// `fork create` of the source into `fork` (state) and `fork-tree`;
    /// returns the fork's name.
    pub fn create_fork(&self) -> String {
        let source = self.path("state").display().to_string();
        let out = self.ok(
            "fork",
            "fork-tree",
            &["fork", "create", "--source-state", &source],
        );
        let line = out.lines().next().unwrap().to_owned();
        let name = line
            .strip_prefix("fork ")
            .and_then(|l| l.split(' ').next())
            .unwrap_or_else(|| panic!("{line}"))
            .to_owned();
        let head = self.source_head();
        assert_eq!(line, format!("fork {name} of src at {head}"));
        name
    }

    pub fn source_head(&self) -> Oid {
        Repository::open_bare(self.path("state/git"))
            .unwrap()
            .refname_to_id("refs/heads/agentfs/src")
            .unwrap()
    }

    pub fn refs(&self, remote: &str) -> Vec<(String, Oid)> {
        let repo = Repository::open_bare(self.path(remote)).unwrap();
        let mut out: Vec<(String, Oid)> = repo
            .references()
            .unwrap()
            .map(|r| {
                let r = r.unwrap();
                (r.name().unwrap().to_owned(), r.target().unwrap())
            })
            .collect();
        out.sort();
        out
    }

    pub fn remote_ref(&self, remote: &str, name: &str) -> Option<Oid> {
        Repository::open_bare(self.path(remote))
            .unwrap()
            .refname_to_id(name)
            .ok()
    }

    pub fn git(&self, dir: &Path, args: &[&str]) -> Output {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(dir).args(args);
        self.hermetic(&mut cmd);
        cmd.output().unwrap()
    }
}
