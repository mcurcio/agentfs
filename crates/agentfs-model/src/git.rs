//! The one adapter: the `git` program, run in a directory with a fixed
//! identity and pinned dates so a seed's run is the same every time.
//! [`with_reversed_dates`] makes the dates run backwards instead, so a test
//! can show that no order is taken from them.

use std::cell::Cell;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const PINNED: &str = "2026-11-02T00:00:00+0000";

thread_local! {
    /// `Some(n)`: dates run backwards, `n` git runs so far.
    static REVERSED: Cell<Option<i64>> = const { Cell::new(None) };
}

/// Runs `f` with each git run on this thread dated one second before the
/// run before it: every commit is older than its parents, so any order
/// read from commit dates runs backwards.
pub fn with_reversed_dates<R>(f: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            REVERSED.with(|r| r.set(None));
        }
    }
    REVERSED.with(|r| r.set(Some(0)));
    let _reset = Reset;
    f()
}

fn date() -> String {
    REVERSED.with(|r| match r.get() {
        None => PINNED.to_string(),
        Some(n) => {
            r.set(Some(n + 1));
            format!("{} +0000", 2_000_000_000 - n)
        }
    })
}

/// What a git run returned.
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs git, with `input` on its stdin if given.
pub fn try_git(dir: &Path, args: &[&str], input: Option<&str>) -> Output {
    let date = date();
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "agentfs-model")
        .env("GIT_AUTHOR_EMAIL", "model@agentfs.invalid")
        .env("GIT_COMMITTER_NAME", "agentfs-model")
        .env("GIT_COMMITTER_EMAIL", "model@agentfs.invalid")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        // Every repository the model creates is SHA-1 with loose ref files:
        // the checks read its object and ref folders directly.
        .env_remove("GIT_DEFAULT_HASH")
        .env_remove("GIT_DEFAULT_REF_FORMAT")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("the git program runs");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(input.as_bytes())
            .expect("git reads its stdin");
    }
    let out = child.wait_with_output().expect("git finishes");
    Output {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Runs git and returns its trimmed stdout; a failure is a harness bug.
pub fn git(dir: &Path, args: &[&str]) -> String {
    checked(dir, args, None)
}

/// [`git`] with `input` on stdin.
pub fn git_in(dir: &Path, args: &[&str], input: &str) -> String {
    checked(dir, args, Some(input))
}

fn checked(dir: &Path, args: &[&str], input: Option<&str>) -> String {
    let o = try_git(dir, args, input);
    assert!(
        o.code == 0,
        "git {args:?} in {} failed ({}): {}",
        dir.display(),
        o.code,
        o.stderr
    );
    o.stdout.trim_end().to_string()
}
