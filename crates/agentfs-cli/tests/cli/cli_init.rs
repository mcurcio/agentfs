//! `agentfs init` from the command line (docs/trees.md, "Volume lifecycle"):
//! it creates a tree volume or finds it (one id), refuses another name and
//! another format with nothing written, writes exactly the `--include`
//! lines of the operator settings, and records `--owner` on the new root
//! directory only. No FUSE.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agentfs_core::volume::StateDir;

struct Dir {
    dir: tempfile::TempDir,
}

impl Dir {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn state(&self) -> PathBuf {
        self.p("state")
    }

    fn run<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command().args(args).output().unwrap()
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        for (name, _) in std::env::vars_os() {
            let n = name.to_string_lossy();
            if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("--state").arg(self.state());
        cmd
    }

    fn ok<I, S>(&self, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self.run(args);
        assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
        stdout(&out)
    }

    fn fragment(&self, name: &str, content: &str) -> PathBuf {
        let path = self.p(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    /// Every file under the state area with its bytes.
    fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.insert(path.clone(), std::fs::read(&path).unwrap_or_default());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.state(), &mut out);
        out
    }

    /// `git config --file <state>/config --get-all include.path`.
    fn includes(&self) -> Vec<String> {
        let out = Command::new("git")
            .args(["config", "--file"])
            .arg(self.state().join("config"))
            .args(["--get-all", "include.path"])
            .output()
            .unwrap();
        stdout(&out).lines().map(str::to_owned).collect()
    }

    /// `git show <rev>` in the volume's history.
    fn show(&self, rev: &str) -> String {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(self.state().join("history.git"))
            .args(["show", rev])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        stdout(&out)
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// `<name> <id> created`, then `found` with the same id; the volume may come
/// from the environment (the downward API).
#[test]
fn created_then_found_give_one_id() {
    let d = Dir::new();
    let first = d.ok(["init", "--volume", "demo"]);
    let words: Vec<&str> = first.split_whitespace().collect();
    assert_eq!(words.len(), 3, "{first}");
    assert_eq!((words[0], words[2]), ("demo", "created"), "{first}");
    let again = d.ok(["init", "--volume", "demo"]);
    assert_eq!(again.trim(), format!("demo {} found", words[1]));
    let out = d
        .command()
        .arg("init")
        .env("AGENTFS_VOLUME", "demo")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), format!("demo {} found", words[1]));
}

/// Another name at the same state area exits 1 and changes no byte, even
/// with `--include` given.
#[test]
fn another_name_is_refused_with_nothing_written() {
    let d = Dir::new();
    d.ok(["init", "--volume", "demo"]);
    let before = d.files();
    let include = d.fragment("a.conf", "[agentfs]\n\tnested = exclude\n");
    let out = d.run(["init", "--volume", "other", "--include", s(&include)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("another volume is here"),
        "{}",
        stderr(&out)
    );
    assert_eq!(d.files(), before, "a file under the state area changed");
}

/// A recording state area is another format: refused, never converted,
/// pointing at the runbook that moves it aside.
#[test]
fn a_recording_state_area_is_refused_and_left_alone() {
    let d = Dir::new();
    agentfs_core::volume::init_with(&StateDir::new(d.state()), "demo", None, None).unwrap();
    let before = d.files();
    let out = d.run(["init", "--volume", "demo"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("holds another format"), "{err}");
    assert!(
        err.contains("docs/runbooks/upgrading-the-sidecar.md"),
        "{err}"
    );
    assert_eq!(d.files(), before, "the recording state area changed");
}

/// `--include` writes exactly the given lines, in order, replacing the
/// last list; without it a re-run keeps them.
#[test]
fn includes_are_exactly_the_given_lines() {
    let d = Dir::new();
    let a = d.fragment("a.conf", "[remote \"origin\"]\n\turl = /srv/history.git\n");
    let b = d.fragment("b.conf", "[agentfs]\n\tnested = exclude\n");
    d.ok(["init", "--volume", "demo", "--include", s(&a)]);
    assert_eq!(d.includes(), [s(&a)]);
    d.ok(["init", "--volume", "demo", "--include", s(&b)]);
    assert_eq!(d.includes(), [s(&b)]);
    d.ok([
        "init",
        "--volume",
        "demo",
        "--include",
        s(&a),
        "--include",
        s(&b),
    ]);
    d.ok(["init", "--volume", "demo"]);
    assert_eq!(d.includes(), [s(&a), s(&b)]);
}

/// A missing or relative include exits 1 before anything is written.
#[test]
fn a_missing_or_relative_include_writes_nothing() {
    let d = Dir::new();
    let out = d.run([
        "init",
        "--volume",
        "demo",
        "--include",
        s(&d.p("missing.conf")),
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("no such file"), "{}", stderr(&out));
    assert!(!d.state().exists(), "the state area was made");
    d.fragment("a.conf", "");
    let out = d
        .command()
        .current_dir(d.dir.path())
        .args(["init", "--volume", "demo", "--include", "a.conf"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("absolute path"), "{}", stderr(&out));
    assert!(!d.state().exists(), "the state area was made");
}

/// `--owner` is the new root directory's owner in its `dirs/` record; on a
/// found volume it changes nothing of the root and says so. (Every found
/// `init` recovers the volume, which appends to its journal: the root's
/// history is what must not change.)
#[test]
fn owner_is_recorded_at_create_only() {
    let d = Dir::new();
    d.ok(["init", "--volume", "demo", "--owner", "10000:10000"]);
    let record = d.show("root:dirs/root");
    assert!(record.contains("\"uid\":10000"), "{record}");
    assert!(record.contains("\"gid\":10000"), "{record}");
    let head = d.show("root");
    let out = d.run(["init", "--volume", "demo", "--owner", "20000:20000"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--owner applies only when the volume is created"),
        "{}",
        stderr(&out)
    );
    assert_eq!(d.show("root"), head, "the root's history changed");
    assert_eq!(d.show("root:dirs/root"), record, "the root's owner changed");
}

/// `init`, `mount` and `fork end` take `--state` only: `--root` or
/// `AGENTFS_ROOT` exits 1, so an old manifest fails loudly.
#[test]
fn root_is_refused() {
    let d = Dir::new();
    for args in [
        &["--root", "b", "init", "--volume", "demo"][..],
        &["--root", "b", "mount", "m"][..],
        &["--root", "b", "fork", "end", "--fork-key", "k"][..],
    ] {
        let out = d.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("take --state only"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    let out = d
        .command()
        .args(["init", "--volume", "demo"])
        .env("AGENTFS_ROOT", d.p("b"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(!d.state().exists(), "the state area was made");
}
